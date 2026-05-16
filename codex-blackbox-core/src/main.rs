use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::mpsc as std_mpsc;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Server;
use tonic::{Request, Response, Status, Streaming};
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// Proto modules
// ---------------------------------------------------------------------------
pub mod envoy {
    pub mod service {
        pub mod ext_proc {
            pub mod v3 {
                tonic::include_proto!("envoy.service.ext_proc.v3");
            }
        }
    }
    pub mod config {
        pub mod core {
            pub mod v3 {
                tonic::include_proto!("envoy.config.core.v3");
            }
        }
    }
    pub mod r#type {
        pub mod v3 {
            tonic::include_proto!("envoy.r#type.v3");
        }
    }
    pub mod extensions {
        pub mod filters {
            pub mod http {
                pub mod ext_proc {
                    pub mod v3 {
                        tonic::include_proto!("envoy.extensions.filters.http.ext_proc.v3");
                    }
                }
            }
        }
    }
}

pub mod codex_accounting;
pub mod codex_request;
pub mod codex_response;
pub mod decision;
pub mod diagnosis;
pub mod guard_policy;
pub mod metrics;
pub mod postmortem;
pub mod pricing;
pub mod watch;

use envoy::config::core::v3::{
    HeaderValue as ProtoHeaderValue, HeaderValueOption as ProtoHeaderValueOption,
};
use envoy::service::ext_proc::v3::{
    common_response::ResponseStatus,
    external_processor_server::{ExternalProcessor, ExternalProcessorServer},
    processing_request::Request as ExtProcRequest,
    processing_response::Response as ExtProcResponse,
    BodyResponse, CommonResponse, HeaderMutation, HeadersResponse, HttpHeaders, ImmediateResponse,
    ProcessingRequest, ProcessingResponse,
};

// ---------------------------------------------------------------------------
// Date formatting (minimal UTC ISO 8601, no chrono dependency)
// ---------------------------------------------------------------------------
pub fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn epoch_to_iso8601(secs: u64) -> String {
    let days_total = (secs / 86400) as i32;
    let tod = secs % 86400;
    let (y, m, d) = days_to_ymd(days_total);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

fn days_to_ymd(mut days: i32) -> (i32, u32, u32) {
    let mut y = 1970;
    loop {
        let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
        let diy = if leap { 366 } else { 365 };
        if days < diy {
            break;
        }
        days -= diy;
        y += 1;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let md = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut m = 0usize;
    while m < 12 && days >= md[m] {
        days -= md[m];
        m += 1;
    }
    (y, (m + 1) as u32, (days + 1) as u32)
}

fn now_iso8601() -> String {
    epoch_to_iso8601(now_epoch_secs())
}

/// Return ISO 8601 for start of today (UTC).
fn start_of_today_iso() -> String {
    let secs = now_epoch_secs();
    let day_start = secs - (secs % 86400);
    epoch_to_iso8601(day_start)
}

fn start_of_week_epoch_at(secs: u64) -> u64 {
    let days = (secs / 86400) as i32;
    // day-of-week: 0=Thu for epoch. Monday = (days + 3) % 7 offset.
    let dow = ((days + 3) % 7) as u64; // 0=Mon .. 6=Sun
    (secs - (secs % 86400)).saturating_sub(dow * 86400)
}

fn start_of_week_iso_at(secs: u64) -> String {
    epoch_to_iso8601(start_of_week_epoch_at(secs))
}

fn start_of_week_iso() -> String {
    start_of_week_iso_at(now_epoch_secs())
}

fn start_of_month_iso() -> String {
    let secs = now_epoch_secs();
    let days_total = (secs / 86400) as i32;
    let (y, m, _) = days_to_ymd(days_total);
    format!("{y:04}-{m:02}-01T00:00:00Z")
}

// ---------------------------------------------------------------------------
// Per-request metadata
// ---------------------------------------------------------------------------
pub struct RequestMeta {
    pub request_id: String,
    pub session_id: String,
    pub model: String,
    pub message_count: usize,
    pub has_tools: bool,
    pub system_prompt_length: usize,
    pub estimated_input_tokens: usize,
    pub started_at: Instant,
}

static REQUEST_STATE: LazyLock<DashMap<String, RequestMeta>> = LazyLock::new(DashMap::new);

// ---------------------------------------------------------------------------
// Budget & circuit breaker state (Phase 7)
// ---------------------------------------------------------------------------
pub fn env_f64(key: &str, default: f64) -> f64 {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}
pub fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

pub const ESTIMATED_COST_SOURCE: &str = pricing::BUILTIN_COST_SOURCE;

struct RuntimeState {
    total_spend: f64,
    total_tokens: u64,
    request_count: u64,
    consecutive_errors: u64,
    circuit_open_until: Option<Instant>,
}

impl RuntimeState {
    fn new() -> Self {
        Self {
            total_spend: 0.0,
            total_tokens: 0,
            request_count: 0,
            consecutive_errors: 0,
            circuit_open_until: None,
        }
    }
}

#[derive(Clone, Debug, Default)]
struct SessionBudgetState {
    total_spend: f64,
    total_tokens: u64,
    request_count: u64,
}

static RUNTIME_STATE: LazyLock<Mutex<RuntimeState>> =
    LazyLock::new(|| Mutex::new(RuntimeState::new()));
static SESSION_BUDGETS: LazyLock<DashMap<String, SessionBudgetState>> = LazyLock::new(DashMap::new);
static POSTMORTEM_READY_SESSIONS: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

#[derive(Clone, Debug)]
struct GuardBlockResponse {
    error_type: &'static str,
    message: String,
    policy_block: Option<decision::PolicyBlockFacts>,
    cooldown: Option<decision::CooldownFacts>,
}

/// Check if the next request should be blocked by the process-wide cooldown.
fn check_circuit_breaker() -> Option<GuardBlockResponse> {
    let runtime = RUNTIME_STATE.lock().unwrap();

    if let Some(until) = runtime.circuit_open_until {
        if Instant::now() < until {
            let remaining = until.duration_since(Instant::now()).as_secs();
            let evaluation = guard_policy::evaluate_guard_policy(
                &guard_policy::GuardPolicy::default(),
                &guard_policy::GuardEvidence {
                    applies_to_next_request: true,
                    cooldown: Some(guard_policy::GuardCooldownEvidence {
                        reason: "upstream errors".to_string(),
                        retry_after_seconds: Some(remaining),
                    }),
                    ..Default::default()
                },
            );
            if let Some(cooldown) = evaluation.cooldown {
                return Some(GuardBlockResponse {
                    error_type: "api_cooldown",
                    message: format!(
                        "Codex Blackbox: API cooldown active after {} consecutive errors. \
                         Wait {}s before retrying. This applies only before the next request is sent; \
                         it cannot interrupt an already-streaming model response.",
                        runtime.consecutive_errors, remaining
                    ),
                    policy_block: None,
                    cooldown: Some(cooldown),
                });
            }
        }
    }

    None
}

fn current_cooldown_facts() -> Option<decision::CooldownFacts> {
    let runtime = RUNTIME_STATE.lock().ok()?;
    let until = runtime.circuit_open_until?;
    let now = Instant::now();
    if now >= until {
        return None;
    }
    Some(decision::CooldownFacts {
        reason: "upstream errors".to_string(),
        retry_after_seconds: Some(until.duration_since(now).as_secs()),
    })
}

fn cooldown_watch_event(cooldown: &decision::CooldownFacts) -> watch::WatchEvent {
    watch::WatchEvent::Cooldown {
        reason: cooldown.reason.clone(),
        retry_after_seconds: cooldown.retry_after_seconds,
    }
}

fn load_process_guard_policy() -> guard_policy::GuardPolicyLoad {
    guard_policy::load_guard_policy_from_env(|key| std::env::var(key).ok())
}

fn warn_guard_policy_issues(issues: &[guard_policy::GuardPolicyIssue]) {
    for issue in issues {
        warn!(
            issue_type = %issue.issue_type,
            recovery_action = %issue.recovery_action,
            message = %issue.message,
            "guard policy issue; failing open for local policy"
        );
    }
}

/// Check if the current session has exceeded an explicit local policy before
/// allowing the next request.
fn check_session_budget(session_id: Option<&str>) -> Option<GuardBlockResponse> {
    let session_id = session_id?;
    let state = SESSION_BUDGETS.get(session_id)?;
    let loaded = load_process_guard_policy();
    let mut evaluation = guard_policy::evaluate_guard_policy(
        &loaded.policy,
        &guard_policy::GuardEvidence {
            session_id: Some(session_id.to_string()),
            observed_codex_responses: state.request_count > 0,
            applies_to_next_request: true,
            session_total_tokens: Some(state.total_tokens),
            session_estimated_cost_dollars: Some(state.total_spend),
            local_estimate_trusted_for_budget_enforcement: pricing::trusted_for_budget_enforcement(
            ),
            cooldown: None,
        },
    );
    evaluation.policy_issues.extend(loaded.issues);
    warn_guard_policy_issues(&evaluation.policy_issues);

    evaluation.block.map(|block| {
        let message = policy_block_message(&block);
        GuardBlockResponse {
            error_type: "policy_block",
            message,
            policy_block: Some(block),
            cooldown: None,
        }
    })
}

fn policy_block_message(block: &decision::PolicyBlockFacts) -> String {
    let current = block.current.as_deref().unwrap_or("unknown");
    let limit = block.limit.as_deref().unwrap_or("unknown");
    let session = block.session_id.as_deref().unwrap_or("unknown");
    format!(
        "Codex Blackbox: {}. Rule: {}. Current: {}. Limit: {}. Session: {}. \
         Recovery: {}. This applies only before the next request is sent; \
         it cannot interrupt an already-streaming model response.",
        block.reason, block.rule, current, limit, session, block.recovery_action
    )
}

fn make_block_response(block: &GuardBlockResponse) -> ProcessingResponse {
    let body = serde_json::json!({
        "type": "error",
        "error": {
            "type": block.error_type,
            "message": block.message,
            "policy_block": block.policy_block,
            "cooldown": block.cooldown,
            "scope": "next_request_only",
            "stream_interruption": false
        }
    })
    .to_string();

    ProcessingResponse {
        response: Some(ExtProcResponse::ImmediateResponse(ImmediateResponse {
            status: Some(envoy::r#type::v3::HttpStatus {
                code: envoy::r#type::v3::StatusCode::TooManyRequests.into(),
            }),
            headers: Some(HeaderMutation {
                set_headers: vec![ProtoHeaderValueOption {
                    header: Some(ProtoHeaderValue {
                        key: "content-type".into(),
                        value: "application/json".into(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            body,
            ..Default::default()
        })),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// SQLite persistence (Phase 6)
// ---------------------------------------------------------------------------
fn db_path() -> String {
    std::env::var("CODEX_BLACKBOX_DB_PATH")
        .unwrap_or_else(|_| "/data/codex-blackbox.db".to_string())
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sessions (
    session_id TEXT PRIMARY KEY,
    started_at TEXT NOT NULL,
    ended_at TEXT,
    total_input_tokens INTEGER DEFAULT 0,
    total_output_tokens INTEGER DEFAULT 0,
    total_cache_read_tokens INTEGER DEFAULT 0,
    total_cache_creation_tokens INTEGER DEFAULT 0,
    total_codex_input_tokens INTEGER DEFAULT 0,
    total_codex_cached_input_tokens INTEGER DEFAULT 0,
    total_codex_uncached_input_tokens INTEGER DEFAULT 0,
    total_codex_output_tokens INTEGER DEFAULT 0,
    total_codex_reasoning_output_tokens INTEGER DEFAULT 0,
    total_codex_tokens INTEGER DEFAULT 0,
    total_cost_dollars REAL DEFAULT 0.0,
    cache_waste_dollars REAL DEFAULT 0.0,
    request_count INTEGER DEFAULT 0,
    model TEXT,
    display_name TEXT,
    initial_prompt TEXT
);

CREATE TABLE IF NOT EXISTS requests (
    request_id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    timestamp TEXT NOT NULL,
    model TEXT NOT NULL,
    input_tokens INTEGER,
    output_tokens INTEGER,
    cache_read_tokens INTEGER DEFAULT 0,
    cache_creation_tokens INTEGER DEFAULT 0,
    cost_dollars REAL,
    cost_source TEXT,
    trusted_for_budget_enforcement INTEGER DEFAULT 0,
    duration_ms INTEGER,
    tool_calls TEXT,
    cache_event TEXT,
    provider TEXT,
    requested_model TEXT,
    served_model TEXT,
    codex_status TEXT,
    codex_input_tokens INTEGER DEFAULT 0,
    codex_cached_input_tokens INTEGER DEFAULT 0,
    codex_uncached_input_tokens INTEGER DEFAULT 0,
    codex_output_tokens INTEGER DEFAULT 0,
    codex_reasoning_output_tokens INTEGER DEFAULT 0,
    codex_total_tokens INTEGER DEFAULT 0,
    codex_response_id TEXT,
    codex_prompt_excerpt TEXT,
    codex_failure_detail TEXT,
    codex_incomplete_detail TEXT,
    codex_tool_calls TEXT,
    codex_accounting_anomalies TEXT,
    FOREIGN KEY (session_id) REFERENCES sessions(session_id)
);

CREATE TABLE IF NOT EXISTS tool_calls (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    request_id TEXT NOT NULL,
    timestamp TEXT NOT NULL,
    tool_name TEXT NOT NULL,
    FOREIGN KEY (request_id) REFERENCES requests(request_id)
);

CREATE INDEX IF NOT EXISTS idx_sessions_started ON sessions(started_at);
CREATE INDEX IF NOT EXISTS idx_requests_session ON requests(session_id);
CREATE INDEX IF NOT EXISTS idx_requests_timestamp ON requests(timestamp);

CREATE TABLE IF NOT EXISTS turn_snapshots (
    id                    INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id            TEXT NOT NULL,
    turn_number           INTEGER NOT NULL,
    timestamp             TEXT NOT NULL,
    input_tokens          INTEGER,
    cache_read_tokens     INTEGER DEFAULT 0,
    cache_creation_tokens INTEGER DEFAULT 0,
    output_tokens         INTEGER,
    ttft_ms               INTEGER,
    tool_calls            TEXT,
    tool_failures         INTEGER DEFAULT 0,
    gap_from_prev_secs    REAL,
    context_utilization   REAL,
    context_window_tokens INTEGER,
    frustration_signals   INTEGER DEFAULT 0,
    requested_model       TEXT,
    actual_model          TEXT,
    response_summary      TEXT,
    request_id            TEXT,
    provider              TEXT,
    codex_status          TEXT,
    codex_input_tokens    INTEGER DEFAULT 0,
    codex_cached_input_tokens INTEGER DEFAULT 0,
    codex_uncached_input_tokens INTEGER DEFAULT 0,
    codex_output_tokens   INTEGER DEFAULT 0,
    codex_reasoning_output_tokens INTEGER DEFAULT 0,
    codex_total_tokens    INTEGER DEFAULT 0,
    codex_response_id     TEXT,
    codex_prompt_excerpt  TEXT,
    codex_failure_detail  TEXT,
    codex_incomplete_detail TEXT,
    codex_tool_calls      TEXT,
    codex_accounting_anomalies TEXT,
    FOREIGN KEY (session_id) REFERENCES sessions(session_id)
);

CREATE TABLE IF NOT EXISTS session_diagnoses (
    session_id        TEXT PRIMARY KEY,
    completed_at      TEXT NOT NULL,
    outcome           TEXT NOT NULL,
    total_turns       INTEGER,
    total_cost        REAL,
    degraded          INTEGER DEFAULT 0,
    degradation_turn  INTEGER,
    causes_json       TEXT,
    advice_json       TEXT,
    FOREIGN KEY (session_id) REFERENCES sessions(session_id)
);

CREATE TABLE IF NOT EXISTS session_recall (
    session_id              TEXT PRIMARY KEY,
    initial_prompt          TEXT,
    final_response_summary  TEXT,
    FOREIGN KEY (session_id) REFERENCES sessions(session_id)
);

CREATE TABLE IF NOT EXISTS billing_reconciliations (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id          TEXT NOT NULL,
    imported_at         TEXT NOT NULL,
    source              TEXT NOT NULL,
    billed_cost_dollars REAL NOT NULL,
    FOREIGN KEY (session_id) REFERENCES sessions(session_id)
);

CREATE INDEX IF NOT EXISTS idx_turns_session ON turn_snapshots(session_id, turn_number);
CREATE INDEX IF NOT EXISTS idx_diagnoses_completed ON session_diagnoses(completed_at);
CREATE INDEX IF NOT EXISTS idx_session_recall_session ON session_recall(session_id);
CREATE INDEX IF NOT EXISTS idx_billing_reconciliations_session_imported
    ON billing_reconciliations(session_id, imported_at DESC);
";

struct RecordCodexTurnCommand {
    request_id: String,
    session_id: String,
    timestamp: String,
    requested_model: String,
    served_model: Option<String>,
    status: String,
    input_tokens: u64,
    cached_input_tokens: u64,
    uncached_input_tokens: u64,
    output_tokens: u64,
    reasoning_output_tokens: u64,
    total_tokens: u64,
    duration_ms: u64,
    context_utilization: f64,
    context_window_tokens: u64,
    display_name: String,
    initial_prompt: Option<String>,
    response_summary: Option<String>,
    response_id: Option<String>,
    failure_detail: Option<String>,
    incomplete_detail: Option<String>,
    tool_names_json: String,
    tool_calls_json: String,
    accounting_anomalies_json: String,
    cost_dollars: f64,
    cost_source: String,
    trusted_for_budget_enforcement: bool,
}

enum DbCommand {
    #[cfg(test)]
    InsertSession {
        session_id: String,
        started_at: String,
        model: String,
        display_name: String,
        initial_prompt: Option<String>,
    },
    RecordCodexTurn(Box<RecordCodexTurnCommand>),
    WriteDiagnosis {
        session_id: String,
        completed_at: String,
        outcome: String,
        total_turns: u32,
        total_cost: f64,
        degraded: bool,
        degradation_turn: Option<u32>,
        causes_json: String,
        advice_json: String,
    },
    WriteRecall {
        session_id: String,
        initial_prompt: String,
        final_response_summary: String,
    },
    WriteBillingReconciliation {
        session_id: String,
        imported_at: String,
        source: String,
        billed_cost_dollars: f64,
        response_tx: oneshot::Sender<Result<(), BillingReconciliationWriteError>>,
    },
}

#[derive(Debug, PartialEq, Eq)]
enum BillingReconciliationWriteError {
    DbUnavailable,
    UnknownSession(String),
    Sqlite(String),
}

static DB_TX: LazyLock<std_mpsc::Sender<DbCommand>> = LazyLock::new(|| {
    let (tx, rx) = std_mpsc::channel();
    let path = db_path();
    std::thread::spawn(move || db_writer_loop(&path, rx));
    tx
});

fn table_columns(conn: &Connection, table: &str) -> rusqlite::Result<HashSet<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .filter_map(|row| row.ok())
        .collect();
    Ok(columns)
}

fn ensure_columns(
    conn: &Connection,
    table: &str,
    columns: &[(&str, &str)],
) -> rusqlite::Result<()> {
    let existing = table_columns(conn, table)?;
    for (name, definition) in columns {
        if !existing.contains(*name) {
            conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {definition}"), [])?;
        }
    }
    Ok(())
}

fn ensure_turn_snapshot_model_columns(conn: &Connection) -> rusqlite::Result<()> {
    let columns = table_columns(conn, "turn_snapshots")?;

    if !columns.contains("requested_model") {
        conn.execute(
            "ALTER TABLE turn_snapshots ADD COLUMN requested_model TEXT",
            [],
        )?;
    }
    if !columns.contains("actual_model") {
        conn.execute(
            "ALTER TABLE turn_snapshots ADD COLUMN actual_model TEXT",
            [],
        )?;
    }
    if !columns.contains("response_summary") {
        conn.execute(
            "ALTER TABLE turn_snapshots ADD COLUMN response_summary TEXT",
            [],
        )?;
    }
    if !columns.contains("context_window_tokens") {
        conn.execute(
            "ALTER TABLE turn_snapshots ADD COLUMN context_window_tokens INTEGER",
            [],
        )?;
    }

    Ok(())
}

fn ensure_session_columns(conn: &Connection) -> rusqlite::Result<()> {
    let columns = table_columns(conn, "sessions")?;

    if !columns.contains("initial_prompt") {
        conn.execute("ALTER TABLE sessions ADD COLUMN initial_prompt TEXT", [])?;
    }
    if !columns.contains("display_name") {
        conn.execute("ALTER TABLE sessions ADD COLUMN display_name TEXT", [])?;
    }
    if columns.contains("cache_hit_ratio") {
        conn.execute("ALTER TABLE sessions DROP COLUMN cache_hit_ratio", [])?;
    }

    Ok(())
}

fn ensure_session_diagnosis_columns(conn: &Connection) -> rusqlite::Result<()> {
    let columns = table_columns(conn, "session_diagnoses")?;

    if columns.contains("cache_hit_ratio") {
        conn.execute(
            "ALTER TABLE session_diagnoses DROP COLUMN cache_hit_ratio",
            [],
        )?;
    }

    Ok(())
}

fn drop_legacy_lifecycle_tables(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "DROP INDEX IF EXISTS idx_tool_outcomes_session;
         DROP INDEX IF EXISTS idx_skill_events_session;
         DROP INDEX IF EXISTS idx_mcp_events_session;
         DROP TABLE IF EXISTS tool_outcomes;
         DROP TABLE IF EXISTS skill_events;
         DROP TABLE IF EXISTS mcp_events;",
    )
}

fn ensure_request_cost_columns(conn: &Connection) -> rusqlite::Result<()> {
    let columns = table_columns(conn, "requests")?;

    if !columns.contains("cost_source") {
        conn.execute("ALTER TABLE requests ADD COLUMN cost_source TEXT", [])?;
    }
    if !columns.contains("trusted_for_budget_enforcement") {
        conn.execute(
            "ALTER TABLE requests ADD COLUMN trusted_for_budget_enforcement INTEGER DEFAULT 0",
            [],
        )?;
    }

    Ok(())
}

fn ensure_codex_persistence_columns(conn: &Connection) -> rusqlite::Result<()> {
    ensure_columns(
        conn,
        "sessions",
        &[
            (
                "total_codex_input_tokens",
                "total_codex_input_tokens INTEGER DEFAULT 0",
            ),
            (
                "total_codex_cached_input_tokens",
                "total_codex_cached_input_tokens INTEGER DEFAULT 0",
            ),
            (
                "total_codex_uncached_input_tokens",
                "total_codex_uncached_input_tokens INTEGER DEFAULT 0",
            ),
            (
                "total_codex_output_tokens",
                "total_codex_output_tokens INTEGER DEFAULT 0",
            ),
            (
                "total_codex_reasoning_output_tokens",
                "total_codex_reasoning_output_tokens INTEGER DEFAULT 0",
            ),
            ("total_codex_tokens", "total_codex_tokens INTEGER DEFAULT 0"),
        ],
    )?;
    ensure_columns(
        conn,
        "requests",
        &[
            ("provider", "provider TEXT"),
            ("requested_model", "requested_model TEXT"),
            ("served_model", "served_model TEXT"),
            ("codex_status", "codex_status TEXT"),
            ("codex_input_tokens", "codex_input_tokens INTEGER DEFAULT 0"),
            (
                "codex_cached_input_tokens",
                "codex_cached_input_tokens INTEGER DEFAULT 0",
            ),
            (
                "codex_uncached_input_tokens",
                "codex_uncached_input_tokens INTEGER DEFAULT 0",
            ),
            (
                "codex_output_tokens",
                "codex_output_tokens INTEGER DEFAULT 0",
            ),
            (
                "codex_reasoning_output_tokens",
                "codex_reasoning_output_tokens INTEGER DEFAULT 0",
            ),
            ("codex_total_tokens", "codex_total_tokens INTEGER DEFAULT 0"),
            ("codex_response_id", "codex_response_id TEXT"),
            ("codex_prompt_excerpt", "codex_prompt_excerpt TEXT"),
            ("codex_failure_detail", "codex_failure_detail TEXT"),
            ("codex_incomplete_detail", "codex_incomplete_detail TEXT"),
            ("codex_tool_calls", "codex_tool_calls TEXT"),
            (
                "codex_accounting_anomalies",
                "codex_accounting_anomalies TEXT",
            ),
        ],
    )?;
    ensure_columns(
        conn,
        "turn_snapshots",
        &[
            ("request_id", "request_id TEXT"),
            ("provider", "provider TEXT"),
            ("codex_status", "codex_status TEXT"),
            ("codex_input_tokens", "codex_input_tokens INTEGER DEFAULT 0"),
            (
                "codex_cached_input_tokens",
                "codex_cached_input_tokens INTEGER DEFAULT 0",
            ),
            (
                "codex_uncached_input_tokens",
                "codex_uncached_input_tokens INTEGER DEFAULT 0",
            ),
            (
                "codex_output_tokens",
                "codex_output_tokens INTEGER DEFAULT 0",
            ),
            (
                "codex_reasoning_output_tokens",
                "codex_reasoning_output_tokens INTEGER DEFAULT 0",
            ),
            ("codex_total_tokens", "codex_total_tokens INTEGER DEFAULT 0"),
            ("codex_response_id", "codex_response_id TEXT"),
            ("codex_prompt_excerpt", "codex_prompt_excerpt TEXT"),
            ("codex_failure_detail", "codex_failure_detail TEXT"),
            ("codex_incomplete_detail", "codex_incomplete_detail TEXT"),
            ("codex_tool_calls", "codex_tool_calls TEXT"),
            (
                "codex_accounting_anomalies",
                "codex_accounting_anomalies TEXT",
            ),
        ],
    )
}

fn repair_turn_snapshot_context_windows(conn: &Connection) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare(
        "SELECT id, input_tokens, cache_read_tokens, cache_creation_tokens, \
         requested_model, actual_model \
         FROM turn_snapshots \
         WHERE context_window_tokens IS NULL OR context_window_tokens <= 0",
    )?;

    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?.max(0) as u64,
                row.get::<_, i64>(2)?.max(0) as u64,
                row.get::<_, i64>(3)?.max(0) as u64,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        })?
        .filter_map(|row| row.ok())
        .collect::<Vec<_>>();

    for (
        id,
        input_tokens,
        cache_read_tokens,
        cache_creation_tokens,
        requested_model,
        actual_model,
    ) in rows
    {
        let context_window_tokens = infer_context_window_tokens(
            requested_model.as_deref(),
            actual_model.as_deref(),
            input_tokens,
            cache_read_tokens,
            cache_creation_tokens,
        );
        let context_utilization = context_fill_ratio(
            input_tokens,
            cache_read_tokens,
            cache_creation_tokens,
            context_window_tokens,
        );
        conn.execute(
            "UPDATE turn_snapshots \
             SET context_window_tokens = ?2, context_utilization = ?3 \
             WHERE id = ?1",
            rusqlite::params![id, context_window_tokens as i64, context_utilization],
        )?;
    }

    Ok(())
}

fn repair_session_diagnosis_degradation_turns(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE session_diagnoses SET degradation_turn = NULL WHERE degraded = 0",
        [],
    )?;
    Ok(())
}

fn repair_session_diagnosis_envoy_causes(conn: &Connection) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare(
        "SELECT session_id, causes_json, advice_json \
         FROM session_diagnoses",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);

    for (session_id, causes_json, advice_json) in rows {
        let causes_value = causes_json
            .as_deref()
            .and_then(|json| serde_json::from_str::<Value>(json).ok())
            .unwrap_or(Value::Array(vec![]));
        let advice_value = advice_json
            .as_deref()
            .and_then(|json| serde_json::from_str::<Value>(json).ok())
            .unwrap_or(Value::Array(vec![]));
        let (causes, advice) = filter_codex_envoy_diagnosis_payload(causes_value, advice_value);
        let (degraded, degradation_turn) = codex_envoy_public_degradation(&causes);
        conn.execute(
            "UPDATE session_diagnoses \
             SET degraded = ?2, degradation_turn = ?3, causes_json = ?4, advice_json = ?5 \
             WHERE session_id = ?1",
            rusqlite::params![
                session_id,
                degraded as i32,
                degradation_turn,
                serde_json::to_string(&causes).unwrap_or_default(),
                serde_json::to_string(&advice).unwrap_or_default(),
            ],
        )?;
    }

    Ok(())
}

fn seed_live_metric_labels_from_db(conn: &Connection) -> rusqlite::Result<()> {
    metrics::ensure_tool_metric_labels("unknown");

    let mut stmt = conn.prepare(
        "SELECT DISTINCT tc.tool_name \
         FROM tool_calls tc \
         INNER JOIN requests r ON r.request_id = tc.request_id \
         WHERE r.provider = 'codex_responses'",
    )?;
    let tool_names = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .filter_map(|row| row.ok())
        .collect::<Vec<_>>();

    for tool_name in tool_names {
        metrics::ensure_tool_metric_labels(&tool_name);
    }

    Ok(())
}

fn initialize_persistence_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(SCHEMA)?;
    ensure_turn_snapshot_model_columns(conn)?;
    ensure_session_columns(conn)?;
    ensure_session_diagnosis_columns(conn)?;
    drop_legacy_lifecycle_tables(conn)?;
    ensure_request_cost_columns(conn)?;
    ensure_codex_persistence_columns(conn)?;
    repair_turn_snapshot_context_windows(conn)?;
    repair_session_diagnosis_degradation_turns(conn)?;
    repair_session_diagnosis_envoy_causes(conn)?;
    Ok(())
}

fn db_writer_loop(path: &str, rx: std_mpsc::Receiver<DbCommand>) {
    let conn = match Connection::open(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to open SQLite at {path}: {e}");
            return;
        }
    };
    let _ = conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;");
    if let Err(e) = initialize_persistence_schema(&conn) {
        eprintln!("Failed to initialize SQLite schema: {e}");
        return;
    }
    if let Err(e) = seed_live_metric_labels_from_db(&conn) {
        eprintln!("Failed to seed live metric labels from SQLite: {e}");
        return;
    }

    while let Ok(cmd) = rx.recv() {
        match cmd {
            #[cfg(test)]
            DbCommand::InsertSession {
                session_id,
                started_at,
                model,
                display_name,
                initial_prompt,
            } => {
                let _ = conn.execute(
                    "INSERT OR IGNORE INTO sessions (session_id, started_at, model, display_name, initial_prompt) VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![session_id, started_at, model, display_name, initial_prompt],
                );
            }
            DbCommand::RecordCodexTurn(command) => {
                let RecordCodexTurnCommand {
                    request_id,
                    session_id,
                    timestamp,
                    requested_model,
                    served_model,
                    status,
                    input_tokens,
                    cached_input_tokens,
                    uncached_input_tokens,
                    output_tokens,
                    reasoning_output_tokens,
                    total_tokens,
                    duration_ms,
                    context_utilization,
                    context_window_tokens,
                    display_name,
                    initial_prompt,
                    response_summary,
                    response_id,
                    failure_detail,
                    incomplete_detail,
                    tool_names_json,
                    tool_calls_json,
                    accounting_anomalies_json,
                    cost_dollars,
                    cost_source,
                    trusted_for_budget_enforcement,
                } = *command;
                let model_for_row = served_model
                    .as_deref()
                    .unwrap_or(requested_model.as_str())
                    .to_string();
                let _ = conn.execute(
                    "INSERT OR IGNORE INTO sessions (session_id, started_at, model, display_name, initial_prompt) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![
                        &session_id,
                        &timestamp,
                        &requested_model,
                        &display_name,
                        &initial_prompt
                    ],
                );

                let inserted_rows = conn
                    .execute(
                        "INSERT OR IGNORE INTO requests (
                            request_id, session_id, timestamp, model, input_tokens, output_tokens,
                            cache_read_tokens, cache_creation_tokens, cost_dollars, cost_source,
                            trusted_for_budget_enforcement, duration_ms, tool_calls, cache_event,
                            provider, requested_model, served_model, codex_status,
                            codex_input_tokens, codex_cached_input_tokens,
                            codex_uncached_input_tokens, codex_output_tokens,
                            codex_reasoning_output_tokens, codex_total_tokens, codex_response_id,
                            codex_prompt_excerpt, codex_failure_detail, codex_incomplete_detail,
                            codex_tool_calls,
                            codex_accounting_anomalies
                         ) VALUES (
                            ?1, ?2, ?3, ?4, ?5, ?6,
                            0, 0, ?7, ?8, ?9, ?10, ?11, NULL,
                            'codex_responses', ?12, ?13, ?14,
                            ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24,
                            ?25, ?26
                         )",
                        rusqlite::params![
                            &request_id,
                            &session_id,
                            &timestamp,
                            &model_for_row,
                            input_tokens,
                            output_tokens,
                            cost_dollars,
                            &cost_source,
                            trusted_for_budget_enforcement as i32,
                            duration_ms,
                            &tool_names_json,
                            &requested_model,
                            &served_model,
                            &status,
                            input_tokens,
                            cached_input_tokens,
                            uncached_input_tokens,
                            output_tokens,
                            reasoning_output_tokens,
                            total_tokens,
                            &response_id,
                            &initial_prompt,
                            &failure_detail,
                            &incomplete_detail,
                            &tool_calls_json,
                            &accounting_anomalies_json,
                        ],
                    )
                    .unwrap_or(0);

                if inserted_rows == 0 {
                    continue;
                }

                let turn_number = conn
                    .query_row(
                        "SELECT COALESCE(MAX(turn_number), 0) + 1 \
                         FROM turn_snapshots WHERE session_id = ?1",
                        rusqlite::params![&session_id],
                        |row| row.get::<_, i64>(0),
                    )
                    .unwrap_or(1)
                    .max(1) as u32;

                let _ = conn.execute(
                    "INSERT INTO turn_snapshots (
                        session_id, turn_number, timestamp, input_tokens, cache_read_tokens,
                        cache_creation_tokens, output_tokens, ttft_ms, tool_calls,
                        tool_failures, gap_from_prev_secs, context_utilization,
                        context_window_tokens, frustration_signals, requested_model,
                        actual_model, response_summary, request_id, provider, codex_status,
                        codex_input_tokens, codex_cached_input_tokens,
                        codex_uncached_input_tokens, codex_output_tokens,
                        codex_reasoning_output_tokens, codex_total_tokens, codex_response_id,
                        codex_prompt_excerpt, codex_failure_detail, codex_incomplete_detail,
                        codex_tool_calls,
                        codex_accounting_anomalies
                     ) VALUES (
                        ?1, ?2, ?3, ?4, 0, 0, ?5, ?6, ?7,
                        0, 0.0, ?8, ?9, 0, ?10, ?11, ?12, ?13,
                        'codex_responses', ?14, ?15, ?16, ?17, ?18, ?19, ?20,
                        ?21, ?22, ?23, ?24, ?25, ?26
                     )",
                    rusqlite::params![
                        &session_id,
                        turn_number,
                        &timestamp,
                        input_tokens,
                        output_tokens,
                        duration_ms,
                        &tool_names_json,
                        context_utilization,
                        context_window_tokens,
                        &requested_model,
                        &served_model,
                        &response_summary,
                        &request_id,
                        &status,
                        input_tokens,
                        cached_input_tokens,
                        uncached_input_tokens,
                        output_tokens,
                        reasoning_output_tokens,
                        total_tokens,
                        &response_id,
                        &initial_prompt,
                        &failure_detail,
                        &incomplete_detail,
                        &tool_calls_json,
                        &accounting_anomalies_json,
                    ],
                );

                let _ = conn.execute(
                    "UPDATE sessions SET \
                     total_input_tokens = total_input_tokens + ?2, \
                     total_output_tokens = total_output_tokens + ?3, \
                     total_codex_input_tokens = total_codex_input_tokens + ?2, \
                     total_codex_cached_input_tokens = total_codex_cached_input_tokens + ?4, \
                     total_codex_uncached_input_tokens = total_codex_uncached_input_tokens + ?5, \
                     total_codex_output_tokens = total_codex_output_tokens + ?3, \
                     total_codex_reasoning_output_tokens = total_codex_reasoning_output_tokens + ?6, \
                     total_codex_tokens = total_codex_tokens + ?7, \
                     total_cost_dollars = total_cost_dollars + ?8, \
                     request_count = request_count + 1, \
                     ended_at = ?9, \
                     display_name = COALESCE(NULLIF(display_name, ''), ?10) \
                     WHERE session_id = ?1",
                    rusqlite::params![
                        session_id,
                        input_tokens,
                        output_tokens,
                        cached_input_tokens,
                        uncached_input_tokens,
                        reasoning_output_tokens,
                        total_tokens,
                        cost_dollars,
                        timestamp,
                        display_name,
                    ],
                );
            }
            DbCommand::WriteDiagnosis {
                session_id,
                completed_at,
                outcome,
                total_turns,
                total_cost,
                degraded,
                degradation_turn,
                causes_json,
                advice_json,
            } => {
                let _ = conn.execute(
                    "INSERT OR REPLACE INTO session_diagnoses (session_id, completed_at, \
                     outcome, total_turns, total_cost, degraded, \
                     degradation_turn, causes_json, advice_json) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                    rusqlite::params![
                        &session_id,
                        &completed_at,
                        &outcome,
                        total_turns,
                        total_cost,
                        degraded as i32,
                        degradation_turn,
                        &causes_json,
                        &advice_json,
                    ],
                );
                maybe_broadcast_postmortem_ready(&conn, &session_id);
            }
            DbCommand::WriteRecall {
                session_id,
                initial_prompt,
                final_response_summary,
            } => {
                match conn.execute(
                    "INSERT OR IGNORE INTO session_recall (session_id, initial_prompt, final_response_summary) \
                     VALUES (?1,?2,?3)",
                    rusqlite::params![&session_id, &initial_prompt, &final_response_summary],
                ) {
                    Ok(1) => {}
                    Ok(_) => {
                        warn!(
                            session_id,
                            "session recall row already existed; ignoring duplicate insert"
                        );
                    }
                    Err(err) => {
                        warn!(
                            session_id,
                            error = %err,
                            "failed to persist session recall"
                        );
                    }
                }
            }
            DbCommand::WriteBillingReconciliation {
                session_id,
                imported_at,
                source,
                billed_cost_dollars,
                response_tx,
            } => {
                let result = match conn
                    .query_row(
                        "SELECT 1 FROM sessions WHERE session_id = ?1 LIMIT 1",
                        rusqlite::params![&session_id],
                        |_| Ok(()),
                    )
                    .optional()
                {
                    Ok(Some(())) => match conn.execute(
                        "INSERT INTO billing_reconciliations (session_id, imported_at, source, billed_cost_dollars) \
                         VALUES (?1, ?2, ?3, ?4)",
                        rusqlite::params![
                            &session_id,
                            &imported_at,
                            &source,
                            billed_cost_dollars
                        ],
                    ) {
                        Ok(1) => Ok(()),
                        Ok(rows) => Err(BillingReconciliationWriteError::Sqlite(format!(
                            "expected 1 inserted row, got {rows}"
                        ))),
                        Err(err) => Err(BillingReconciliationWriteError::Sqlite(err.to_string())),
                    },
                    Ok(None) => Err(BillingReconciliationWriteError::UnknownSession(
                        session_id.clone(),
                    )),
                    Err(err) => Err(BillingReconciliationWriteError::Sqlite(err.to_string())),
                };

                if let Err(err) = &result {
                    warn!(
                        session_id = %session_id,
                        error = ?err,
                        "failed to persist billing reconciliation"
                    );
                }

                let _ = response_tx.send(result);
            }
        }
    }
}

#[derive(Clone, Debug)]
struct EstimatedAggregate {
    estimated_cost_dollars: f64,
    cost_source: String,
    trusted_for_budget_enforcement: bool,
}

#[derive(Clone, Debug)]
struct LatestBillingReconciliation {
    billed_cost_dollars: f64,
    source: String,
    imported_at: String,
}

#[derive(Clone, Debug)]
struct SummaryWindowData {
    sessions: i64,
    estimated_cost_dollars: f64,
    cost_source: String,
    trusted_for_budget_enforcement: bool,
    billed_cost_dollars: Option<f64>,
    billed_sessions: u64,
    codex_cached_input: CodexCachedInputSummary,
}

#[derive(Clone, Copy, Debug, Default)]
struct CodexCachedInputSummary {
    input_tokens: u64,
    cached_input_tokens: u64,
}

impl CodexCachedInputSummary {
    fn record(&mut self, input_tokens: i64, cached_input_tokens: i64) {
        self.input_tokens = self.input_tokens.saturating_add(input_tokens.max(0) as u64);
        self.cached_input_tokens = self
            .cached_input_tokens
            .saturating_add(cached_input_tokens.max(0) as u64);
    }

    fn cached_input_ratio(self) -> Option<f64> {
        if self.input_tokens == 0 {
            return None;
        }
        Some(self.cached_input_tokens as f64 / self.input_tokens as f64)
    }
}

struct CostAccumulator {
    total_cost_dollars: f64,
    sources: HashSet<String>,
    trusted_for_budget_enforcement: bool,
    saw_rows: bool,
}

impl CostAccumulator {
    fn new() -> Self {
        Self {
            total_cost_dollars: 0.0,
            sources: HashSet::new(),
            trusted_for_budget_enforcement: true,
            saw_rows: false,
        }
    }

    fn record(&mut self, breakdown: pricing::EstimatedCostBreakdown) {
        self.total_cost_dollars += breakdown.total_cost_dollars;
        self.sources.insert(breakdown.cost_source);
        self.trusted_for_budget_enforcement &= breakdown.trusted_for_budget_enforcement;
        self.saw_rows = true;
    }

    fn record_persisted(
        &mut self,
        total_cost_dollars: f64,
        cost_source: Option<String>,
        trusted_for_budget_enforcement: Option<bool>,
    ) {
        self.total_cost_dollars += total_cost_dollars.max(0.0);
        self.sources
            .insert(cost_source.unwrap_or_else(pricing::active_catalog_source));
        self.trusted_for_budget_enforcement &=
            trusted_for_budget_enforcement.unwrap_or_else(pricing::trusted_for_budget_enforcement);
        self.saw_rows = true;
    }

    fn finish(self) -> EstimatedAggregate {
        EstimatedAggregate {
            estimated_cost_dollars: self.total_cost_dollars.max(0.0),
            cost_source: pricing::summarize_cost_sources(&self.sources),
            trusted_for_budget_enforcement: if self.saw_rows {
                self.trusted_for_budget_enforcement
            } else {
                pricing::trusted_for_budget_enforcement()
            },
        }
    }
}

fn rounded_estimated_cost_dollars(amount: f64) -> f64 {
    (amount * 100.0).round() / 100.0
}

fn rounded_billed_cost_dollars(amount: Option<f64>) -> Option<f64> {
    amount.map(rounded_estimated_cost_dollars)
}

fn rounded_ratio(value: Option<f64>) -> Option<f64> {
    value.map(|ratio| (ratio * 100.0).round() / 100.0)
}

fn codex_cached_input_json(summary: CodexCachedInputSummary) -> Value {
    serde_json::json!({
        "codex_input_tokens": summary.input_tokens,
        "codex_cached_input_tokens": summary.cached_input_tokens,
        "codex_cached_input_ratio": rounded_ratio(summary.cached_input_ratio()),
    })
}

fn load_latest_billing_reconciliations(
    conn: &Connection,
    session_ids: &[String],
) -> rusqlite::Result<HashMap<String, LatestBillingReconciliation>> {
    if session_ids.is_empty() {
        return Ok(HashMap::new());
    }

    let placeholders = vec!["?"; session_ids.len()].join(",");
    let sql = format!(
        "SELECT session_id, imported_at, source, billed_cost_dollars \
         FROM billing_reconciliations \
         WHERE session_id IN ({placeholders}) \
         ORDER BY imported_at DESC, id DESC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(session_ids.iter()), |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, f64>(3)?,
        ))
    })?;

    let mut latest = HashMap::new();
    for row in rows {
        let (session_id, imported_at, source, billed_cost_dollars) = row?;
        latest
            .entry(session_id)
            .or_insert(LatestBillingReconciliation {
                billed_cost_dollars,
                source,
                imported_at,
            });
    }

    Ok(latest)
}

fn load_codex_cached_input_summaries(
    conn: &Connection,
    session_ids: &[String],
) -> rusqlite::Result<HashMap<String, CodexCachedInputSummary>> {
    if session_ids.is_empty() {
        return Ok(HashMap::new());
    }

    let placeholders = vec!["?"; session_ids.len()].join(",");
    let sql = format!(
        "SELECT session_id, COALESCE(SUM(codex_input_tokens), 0), \
                COALESCE(SUM(codex_cached_input_tokens), 0) \
         FROM requests \
         WHERE session_id IN ({placeholders}) \
           AND provider = 'codex_responses' \
         GROUP BY session_id"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(session_ids.iter()), |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;

    let mut summaries = HashMap::new();
    for row in rows {
        let (session_id, input_tokens, cached_input_tokens) = row?;
        let mut summary = CodexCachedInputSummary::default();
        summary.record(input_tokens, cached_input_tokens);
        summaries.insert(session_id, summary);
    }

    Ok(summaries)
}

fn session_has_codex_evidence(conn: &Connection, session_id: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT \
            EXISTS(SELECT 1 FROM requests r \
                   WHERE r.session_id = ?1 AND r.provider = 'codex_responses') \
            OR EXISTS(SELECT 1 FROM turn_snapshots t \
                      WHERE t.session_id = ?1 AND t.provider = 'codex_responses')",
        rusqlite::params![session_id],
        |row| row.get::<_, i64>(0),
    )
    .map(|value| value != 0)
}

#[derive(Debug, Serialize)]
struct CodexObservationSnapshot {
    provider: &'static str,
    after_request_rowid: i64,
    latest_request_rowid: i64,
    request_count: u64,
    matching_request_count: u64,
    matched: bool,
}

fn load_codex_observation_snapshot(
    conn: &Connection,
    after_request_rowid: i64,
    session_id: Option<&str>,
    prompt_excerpt: Option<&str>,
) -> rusqlite::Result<CodexObservationSnapshot> {
    let after_request_rowid = after_request_rowid.max(0);
    let latest_request_rowid = conn.query_row(
        "SELECT COALESCE(MAX(rowid), 0) FROM requests WHERE provider = 'codex_responses'",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    let request_count = conn.query_row(
        "SELECT COUNT(*) FROM requests \
         WHERE provider = 'codex_responses' AND rowid > ?1",
        rusqlite::params![after_request_rowid],
        |row| row.get::<_, i64>(0),
    )?;
    let session_id = session_id.map(str::trim).filter(|value| !value.is_empty());
    let prompt_excerpt = prompt_excerpt
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let matching_request_count = if let Some(session_id) = session_id {
        conn.query_row(
            "SELECT COUNT(*) FROM requests \
             WHERE provider = 'codex_responses' \
               AND rowid > ?1 \
               AND session_id = ?2",
            rusqlite::params![after_request_rowid, session_id],
            |row| row.get::<_, i64>(0),
        )?
    } else if let Some(prompt_excerpt) = prompt_excerpt {
        conn.query_row(
            "SELECT COUNT(*) FROM requests \
             WHERE provider = 'codex_responses' \
               AND rowid > ?1 \
               AND codex_prompt_excerpt = ?2",
            rusqlite::params![after_request_rowid, prompt_excerpt],
            |row| row.get::<_, i64>(0),
        )?
    } else {
        request_count
    };
    let request_count = request_count.max(0) as u64;
    let matching_request_count = matching_request_count.max(0) as u64;

    Ok(CodexObservationSnapshot {
        provider: "codex_responses",
        after_request_rowid,
        latest_request_rowid,
        request_count,
        matching_request_count,
        matched: matching_request_count > 0,
    })
}

fn postmortem_command_for_session(session_id: &str) -> String {
    if session_id.trim().is_empty() {
        "codex-blackbox postmortem last".to_string()
    } else {
        format!("codex-blackbox postmortem {session_id}")
    }
}

fn postmortem_ready_already_sent_or_remember(session_id: &str) -> bool {
    let mut seen = POSTMORTEM_READY_SESSIONS.lock().unwrap();
    if seen.contains(session_id) {
        return true;
    }
    seen.insert(session_id.to_string());
    false
}

fn postmortem_ready_totals_from_db(
    conn: &Connection,
    session_id: &str,
) -> rusqlite::Result<Option<(u32, u64)>> {
    let (turn_count, turn_tokens): (i64, i64) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(codex_total_tokens), 0) \
         FROM turn_snapshots \
         WHERE session_id = ?1 AND provider = 'codex_responses'",
        rusqlite::params![session_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if turn_count > 0 {
        return Ok(Some((turn_count.max(0) as u32, turn_tokens.max(0) as u64)));
    }

    let (request_count, request_tokens): (i64, i64) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(codex_total_tokens), 0) \
         FROM requests \
         WHERE session_id = ?1 AND provider = 'codex_responses'",
        rusqlite::params![session_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if request_count > 0 {
        Ok(Some((
            request_count.max(0) as u32,
            request_tokens.max(0) as u64,
        )))
    } else {
        Ok(None)
    }
}

fn maybe_broadcast_postmortem_ready(conn: &Connection, session_id: &str) {
    let Ok(true) = session_has_codex_evidence(conn, session_id) else {
        return;
    };
    let Ok(Some((total_turns, total_tokens))) = postmortem_ready_totals_from_db(conn, session_id)
    else {
        return;
    };
    if total_turns == 0 || postmortem_ready_already_sent_or_remember(session_id) {
        return;
    }

    watch::BROADCASTER.broadcast(watch::WatchEvent::PostmortemReady {
        session_id: session_id.to_string(),
        total_turns,
        total_tokens,
        reason: "session idle enough to review".to_string(),
        postmortem_command: postmortem_command_for_session(session_id),
    });
}

fn compute_estimated_costs_for_sessions(
    conn: &Connection,
    session_ids: &[String],
) -> rusqlite::Result<HashMap<String, EstimatedAggregate>> {
    if session_ids.is_empty() {
        return Ok(HashMap::new());
    }

    let placeholders = vec!["?"; session_ids.len()].join(",");
    let sql = format!(
        "SELECT session_id, model, codex_input_tokens, codex_cached_input_tokens, codex_output_tokens, \
         cost_dollars, cost_source, trusted_for_budget_enforcement \
         FROM requests WHERE session_id IN ({placeholders}) AND provider = 'codex_responses'"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(session_ids.iter()), |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, Option<f64>>(5)?,
            row.get::<_, Option<String>>(6)?,
            row.get::<_, Option<i64>>(7)?,
        ))
    })?;

    let mut accumulators: HashMap<String, CostAccumulator> = HashMap::new();
    for row in rows {
        let (
            session_id,
            model,
            input,
            cached_input,
            output,
            stored_cost,
            stored_source,
            stored_trusted,
        ) = row?;
        let accumulator = accumulators
            .entry(session_id)
            .or_insert_with(CostAccumulator::new);
        if let Some(cost) = stored_cost {
            accumulator.record_persisted(cost, stored_source, stored_trusted.map(|n| n != 0));
        } else {
            accumulator.record(pricing::estimate_codex_api_cost_dollars(
                &model,
                input.max(0) as u64,
                cached_input.max(0) as u64,
                output.max(0) as u64,
            ));
        }
    }

    let mut estimates = HashMap::new();
    for session_id in session_ids {
        let aggregate = accumulators
            .remove(session_id)
            .map(CostAccumulator::finish)
            .unwrap_or_else(|| CostAccumulator::new().finish());
        estimates.insert(session_id.clone(), aggregate);
    }

    Ok(estimates)
}

fn query_summary(conn: &Connection, since: &str) -> rusqlite::Result<SummaryWindowData> {
    let mut stmt = conn.prepare(
        "SELECT r.session_id, r.model, r.codex_input_tokens, r.codex_cached_input_tokens, \
                r.codex_output_tokens, r.cost_dollars, r.cost_source, \
                r.trusted_for_budget_enforcement, CASE WHEN s.session_id IS NULL THEN 0 ELSE 1 END \
         FROM requests r \
         LEFT JOIN sessions s ON s.session_id = r.session_id \
         WHERE r.timestamp >= ?1 AND r.provider = 'codex_responses'",
    )?;
    let rows = stmt.query_map(rusqlite::params![since], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, Option<f64>>(5)?,
            row.get::<_, Option<String>>(6)?,
            row.get::<_, Option<i64>>(7)?,
            row.get::<_, i64>(8)? != 0,
        ))
    })?;

    let mut session_ids = HashSet::new();
    let mut cost_accumulator = CostAccumulator::new();
    let mut codex_cached_input = CodexCachedInputSummary::default();

    for row in rows {
        let (
            session_id,
            model,
            input,
            cached_input,
            output,
            stored_cost,
            stored_source,
            stored_trusted,
            is_real_session,
        ) = row?;
        if is_real_session {
            session_ids.insert(session_id);
        }
        codex_cached_input.record(input, cached_input);
        if let Some(cost) = stored_cost {
            cost_accumulator.record_persisted(cost, stored_source, stored_trusted.map(|n| n != 0));
        } else {
            cost_accumulator.record(pricing::estimate_codex_api_cost_dollars(
                &model,
                input.max(0) as u64,
                cached_input.max(0) as u64,
                output.max(0) as u64,
            ));
        }
    }

    let billing_records = load_latest_billing_reconciliations(
        conn,
        &session_ids.iter().cloned().collect::<Vec<_>>(),
    )?;
    let billed_sessions = billing_records.len() as u64;
    let billed_cost_dollars = if billed_sessions > 0 {
        Some(
            billing_records
                .values()
                .map(|record| record.billed_cost_dollars)
                .sum::<f64>(),
        )
    } else {
        None
    };

    let estimate = cost_accumulator.finish();
    Ok(SummaryWindowData {
        sessions: session_ids.len() as i64,
        estimated_cost_dollars: estimate.estimated_cost_dollars,
        cost_source: estimate.cost_source,
        trusted_for_budget_enforcement: estimate.trusted_for_budget_enforcement,
        billed_cost_dollars,
        billed_sessions,
        codex_cached_input,
    })
}

fn summary_window_json(summary: &SummaryWindowData) -> Value {
    let local_estimate_cost_dollars =
        rounded_estimated_cost_dollars(summary.estimated_cost_dollars);
    let local_estimate_cost_source = summary.cost_source.clone();
    let local_estimate_trusted_for_budget_enforcement = summary.trusted_for_budget_enforcement;
    let mut value = serde_json::json!({
        "sessions": summary.sessions,
        "local_estimate_cost_dollars": local_estimate_cost_dollars,
        "local_estimate_cost_source": local_estimate_cost_source,
        "local_estimate_trusted_for_budget_enforcement": local_estimate_trusted_for_budget_enforcement,
        "estimated_cost_dollars": local_estimate_cost_dollars,
        "cost_source": summary.cost_source.clone(),
        "trusted_for_budget_enforcement": local_estimate_trusted_for_budget_enforcement,
        "billed_cost_dollars": rounded_billed_cost_dollars(summary.billed_cost_dollars),
        "billed_sessions": summary.billed_sessions,
    });
    if let (Some(target), Value::Object(source)) = (
        value.as_object_mut(),
        codex_cached_input_json(summary.codex_cached_input),
    ) {
        target.extend(source);
    }
    value
}

fn build_summary_response_json(
    today: &SummaryWindowData,
    week: &SummaryWindowData,
    month: &SummaryWindowData,
) -> Value {
    let local_estimate_cost_source = pricing::active_catalog_source();
    let local_estimate_trusted_for_budget_enforcement = pricing::trusted_for_budget_enforcement();
    serde_json::json!({
        "local_estimate_cost_source": local_estimate_cost_source.clone(),
        "local_estimate_trusted_for_budget_enforcement": local_estimate_trusted_for_budget_enforcement,
        "cost_source": local_estimate_cost_source,
        "trusted_for_budget_enforcement": local_estimate_trusted_for_budget_enforcement,
        "today": summary_window_json(today),
        "this_week": summary_window_json(week),
        "this_month": summary_window_json(month),
    })
}

#[allow(clippy::too_many_arguments)]
fn build_diagnosis_response_json(
    session_id: String,
    completed_at: String,
    outcome: String,
    total_turns: i64,
    estimated_total_cost_dollars: f64,
    cost_source: String,
    trusted_for_budget_enforcement: bool,
    billed_cost_dollars: Option<f64>,
    billing_source: Option<String>,
    billing_imported_at: Option<String>,
    codex_cached_input: CodexCachedInputSummary,
    degraded: bool,
    degradation_turn: Option<i64>,
    causes: Value,
    advice: Value,
) -> Value {
    let local_estimate_cost_source = cost_source.clone();
    let mut value = serde_json::json!({
        "session_id": session_id,
        "completed_at": completed_at,
        "outcome": outcome,
        "total_turns": total_turns,
        "local_estimate_total_cost_dollars": estimated_total_cost_dollars,
        "local_estimate_cost_source": local_estimate_cost_source,
        "local_estimate_trusted_for_budget_enforcement": trusted_for_budget_enforcement,
        "estimated_total_cost_dollars": estimated_total_cost_dollars,
        "cost_source": cost_source,
        "trusted_for_budget_enforcement": trusted_for_budget_enforcement,
        "billed_cost_dollars": rounded_billed_cost_dollars(billed_cost_dollars),
        "billing_source": billing_source,
        "billing_imported_at": billing_imported_at,
        "degraded": degraded,
        "degradation_turn": if degraded { degradation_turn } else { None },
        "causes": causes,
        "advice": advice,
    });
    if let (Some(target), Value::Object(source)) = (
        value.as_object_mut(),
        codex_cached_input_json(codex_cached_input),
    ) {
        target.extend(source);
    }
    value
}

#[allow(clippy::too_many_arguments)]
fn build_session_summary_json(
    session_id: String,
    display_name: String,
    started_at: Option<String>,
    outcome: String,
    degraded: bool,
    total_turns: i64,
    estimated_total_cost_dollars: f64,
    cost_source: String,
    trusted_for_budget_enforcement: bool,
    billed_cost_dollars: Option<f64>,
    billing_source: Option<String>,
    billing_imported_at: Option<String>,
    primary_cause: String,
    codex_cached_input: CodexCachedInputSummary,
    model: Option<String>,
    requested_model: Option<String>,
    served_model: Option<String>,
) -> Value {
    let local_estimate_cost_source = cost_source.clone();
    let mut value = serde_json::json!({
        "session_id": session_id,
        "display_name": display_name,
        "started_at": started_at,
        "outcome": outcome,
        "degraded": degraded,
        "total_turns": total_turns,
        "local_estimate_total_cost_dollars": estimated_total_cost_dollars,
        "local_estimate_cost_source": local_estimate_cost_source,
        "local_estimate_trusted_for_budget_enforcement": trusted_for_budget_enforcement,
        "estimated_total_cost_dollars": estimated_total_cost_dollars,
        "cost_source": cost_source,
        "trusted_for_budget_enforcement": trusted_for_budget_enforcement,
        "billed_cost_dollars": rounded_billed_cost_dollars(billed_cost_dollars),
        "billing_source": billing_source,
        "billing_imported_at": billing_imported_at,
        "primary_cause": primary_cause,
        "model": model,
        "requested_model": requested_model,
        "served_model": served_model,
    });
    if let (Some(target), Value::Object(source)) = (
        value.as_object_mut(),
        codex_cached_input_json(codex_cached_input),
    ) {
        target.extend(source);
    }
    value
}

fn build_sessions_response_json(sessions: Vec<Value>) -> Value {
    let local_estimate_cost_source = pricing::active_catalog_source();
    let local_estimate_trusted_for_budget_enforcement = pricing::trusted_for_budget_enforcement();
    serde_json::json!({
        "local_estimate_cost_source": local_estimate_cost_source.clone(),
        "local_estimate_trusted_for_budget_enforcement": local_estimate_trusted_for_budget_enforcement,
        "cost_source": local_estimate_cost_source,
        "trusted_for_budget_enforcement": local_estimate_trusted_for_budget_enforcement,
        "sessions": sessions,
    })
}

#[derive(Deserialize)]
struct StoredDegradationCause {
    cause_type: String,
}

fn is_codex_envoy_diagnosis_cause(cause_type: &str) -> bool {
    matches!(
        cause_type,
        "codex_response_failed"
            | "codex_response_incomplete"
            | "codex_model_mismatch"
            | "codex_high_context_fill"
            | "codex_high_reasoning_share"
            | "codex_accounting_anomaly"
            | "codex_low_cached_input_reuse"
    )
}

fn is_codex_envoy_degrading_cause(cause_type: &str) -> bool {
    matches!(
        cause_type,
        "codex_response_failed"
            | "codex_response_incomplete"
            | "codex_model_mismatch"
            | "codex_accounting_anomaly"
    )
}

fn filter_codex_envoy_diagnosis_payload(causes: Value, advice: Value) -> (Value, Value) {
    let Some(items) = causes.as_array() else {
        return (Value::Array(vec![]), Value::Array(vec![]));
    };
    let filtered = items
        .iter()
        .filter_map(|cause| {
            if !cause
                .get("cause_type")
                .and_then(Value::as_str)
                .map(is_codex_envoy_diagnosis_cause)
                .unwrap_or(false)
            {
                return None;
            }
            let mut cause = cause.clone();
            if let Value::Object(fields) = &mut cause {
                if !fields.contains_key("served_model") {
                    if let Some(actual_model) = fields.remove("actual_model") {
                        fields.insert("served_model".to_string(), actual_model);
                    }
                } else {
                    fields.remove("actual_model");
                }
            }
            Some(cause)
        })
        .collect::<Vec<_>>();
    let advice = if filtered.len() == items.len() {
        advice
    } else {
        Value::Array(vec![])
    };
    (Value::Array(filtered), advice)
}

fn codex_envoy_public_degradation(causes: &Value) -> (bool, Option<i64>) {
    let mut degraded = false;
    let mut degradation_turn = None;

    for cause in causes.as_array().into_iter().flatten().filter(|cause| {
        cause
            .get("cause_type")
            .and_then(Value::as_str)
            .map(is_codex_envoy_degrading_cause)
            .unwrap_or(false)
    }) {
        degraded = true;
        if let Some(turn) = cause.get("turn_first_noticed").and_then(Value::as_i64) {
            if degradation_turn
                .map(|current| turn < current)
                .unwrap_or(true)
            {
                degradation_turn = Some(turn);
            }
        }
    }

    (degraded, degradation_turn)
}

fn codex_envoy_primary_degrading_cause(causes: &Value) -> Option<String> {
    causes
        .as_array()
        .into_iter()
        .flatten()
        .find(|cause| {
            cause
                .get("cause_type")
                .and_then(Value::as_str)
                .map(is_codex_envoy_degrading_cause)
                .unwrap_or(false)
        })
        .and_then(|cause| cause.get("cause_type"))
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

fn codex_envoy_diagnosis_report(report: &diagnosis::DiagnosisReport) -> diagnosis::DiagnosisReport {
    let mut report = report.clone();
    let original_cause_count = report.causes.len();
    report
        .causes
        .retain(|cause| is_codex_envoy_diagnosis_cause(&cause.cause_type));
    if report.causes.len() != original_cause_count {
        report.advice.clear();
    }
    report.degradation_turn = report
        .causes
        .iter()
        .filter(|cause| is_codex_envoy_degrading_cause(&cause.cause_type))
        .map(|cause| cause.turn_first_noticed)
        .min();
    report.degraded = report.degradation_turn.is_some();
    if !report.degraded {
        report.advice.clear();
    }
    report
}

fn query_historical_window_from_db(
    conn: &Connection,
    since: &str,
    window: &'static str,
) -> rusqlite::Result<metrics::HistoricalWindowMetrics> {
    let mut sessions = 0u64;
    let mut degraded_sessions = 0u64;
    let mut degraded_causes = std::collections::BTreeMap::new();

    let mut stmt = conn.prepare(
        "SELECT degraded, causes_json \
         FROM session_diagnoses WHERE completed_at >= ?1",
    )?;

    let rows = stmt.query_map(rusqlite::params![since], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;

    for row in rows {
        let (degraded, causes_json) = row?;
        sessions += 1;
        if degraded == 0 {
            continue;
        }

        degraded_sessions += 1;
        let causes: Vec<StoredDegradationCause> =
            serde_json::from_str(&causes_json).unwrap_or_default();
        for cause in causes {
            if let Some(label) = metrics::historical_cause_label(&cause.cause_type) {
                *degraded_causes.entry(label).or_insert(0) += 1;
            }
        }
    }

    let degraded_session_ratio = if sessions > 0 {
        degraded_sessions as f64 / sessions as f64
    } else {
        0.0
    };

    let mut model_fallbacks = std::collections::BTreeMap::new();
    let mut stmt = conn.prepare(
        "SELECT requested_model, actual_model \
         FROM turn_snapshots \
         WHERE timestamp >= ?1 \
           AND (provider = 'codex_responses' OR codex_status IS NOT NULL \
                OR codex_cached_input_tokens > 0 OR codex_reasoning_output_tokens > 0 \
                OR codex_accounting_anomalies IS NOT NULL)",
    )?;
    let rows = stmt.query_map(rusqlite::params![since], |row| {
        Ok((
            row.get::<_, Option<String>>(0)?,
            row.get::<_, Option<String>>(1)?,
        ))
    })?;
    for row in rows {
        let (requested_model, actual_model) = row?;
        let Some(requested_model) = requested_model else {
            continue;
        };
        let Some(actual_model) = actual_model else {
            continue;
        };
        let requested_label = metrics::historical_model_label(&requested_model);
        let actual_label = metrics::historical_model_label(&actual_model);
        if requested_label != actual_label {
            *model_fallbacks
                .entry((requested_label, actual_label))
                .or_insert(0) += 1;
        }
    }

    Ok(metrics::HistoricalWindowMetrics {
        window,
        sessions,
        degraded_sessions,
        degraded_session_ratio,
        degraded_causes,
        model_fallbacks,
    })
}

fn query_historical_metrics(
    conn: &Connection,
    now_epoch: u64,
) -> rusqlite::Result<Vec<metrics::HistoricalWindowMetrics>> {
    let mut windows = Vec::with_capacity(metrics::HISTORY_WINDOWS.len());
    for (window, days) in metrics::HISTORY_WINDOWS {
        let since = epoch_to_iso8601(now_epoch.saturating_sub(days * 86_400));
        windows.push(query_historical_window_from_db(conn, &since, window)?);
    }
    Ok(windows)
}

// ---------------------------------------------------------------------------
// Estimated pricing
// ---------------------------------------------------------------------------
pub fn token_cost(tokens: u64, price_per_mtok: f64) -> f64 {
    pricing::token_cost(tokens, price_per_mtok)
}

enum SelectedResponseAccumulator {
    CodexResponses {
        accumulator: codex_response::CodexResponsesAccumulator,
        is_sse: bool,
    },
}

impl SelectedResponseAccumulator {
    fn for_request_source(_source: RequestMetadataSource) -> Self {
        Self::CodexResponses {
            accumulator: codex_response::CodexResponsesAccumulator::new(),
            is_sse: false,
        }
    }

    fn apply_response_headers(&mut self, headers: &codex_response::CodexResponseHeaders) {
        match self {
            Self::CodexResponses {
                accumulator,
                is_sse,
            } => {
                *is_sse = headers.http_status == Some(200);
                accumulator.apply_headers(headers);
            }
        }
    }

    fn is_sse(&self) -> bool {
        match self {
            Self::CodexResponses { is_sse, .. } => *is_sse,
        }
    }

    fn process_chunk(
        &mut self,
        chunk: &[u8],
    ) -> Result<(), codex_response::CodexResponseParseError> {
        match self {
            Self::CodexResponses { accumulator, .. } => accumulator.process_chunk(chunk),
        }
    }
}

#[derive(Clone, Debug)]
enum SelectedFinalizationOutcome {
    Codex(CodexFinalizationOutcome),
}

#[derive(Clone, Debug)]
struct CodexFinalizationOutcome {
    request_id: String,
    accounting: codex_accounting::CodexTurnAccounting,
    duration_ms: u64,
    context_window_tokens: u64,
    context_fill_percent: f64,
    response_summary: Option<String>,
    watch_events: Vec<watch::WatchEvent>,
}

// ---------------------------------------------------------------------------
// Finalize: metrics + DB persistence
// ---------------------------------------------------------------------------
pub(crate) fn model_matches(requested: &str, actual: &str) -> bool {
    requested == actual
}

/// Linear extrapolation: estimate how many turns remain before the configured
/// context-pressure threshold.
pub(crate) fn project_turns_until_compaction(
    prev_fill_percent: f64,
    current_fill_percent: f64,
) -> Option<u32> {
    const COMPACT_THRESHOLD: f64 = 85.0;
    if current_fill_percent >= COMPACT_THRESHOLD {
        return Some(0);
    }
    let delta = current_fill_percent - prev_fill_percent;
    if delta <= 0.0 {
        return None;
    }
    let remaining = COMPACT_THRESHOLD - current_fill_percent;
    Some((remaining / delta).ceil().max(1.0) as u32)
}

fn build_codex_finalization_outcome(
    request_id: &str,
    request: &codex_request::ParsedCodexRequest,
    response: &codex_response::CodexResponseSummary,
    duration: Duration,
    context_window_tokens: u64,
) -> CodexFinalizationOutcome {
    let accounting = codex_accounting::summarize_codex_turn(request, response);
    let response_summary = compact_response_summary(&response.output_text);
    let context_fill_percent =
        context_fill_percent(accounting.input_tokens, 0, 0, context_window_tokens);
    let mut watch_events = Vec::new();

    if !accounting.identity.session_id.is_empty() {
        if let Some(initial_prompt) = accounting.first_user_prompt_excerpt.clone() {
            watch_events.push(watch::WatchEvent::SessionStart {
                session_id: accounting.identity.session_id.clone(),
                display_name: codex_display_name(&accounting),
                model: accounting.requested_model.clone(),
                initial_prompt: Some(initial_prompt),
            });
        }

        if let Some(served_model) = accounting.served_model.as_ref() {
            if served_model != &accounting.requested_model {
                watch_events.push(watch::WatchEvent::ModelFallback {
                    session_id: accounting.identity.session_id.clone(),
                    requested: accounting.requested_model.clone(),
                    actual: served_model.clone(),
                });
            }
        }

        for tool in &accounting.tool_calls {
            watch_events.push(watch::WatchEvent::ToolUse {
                session_id: accounting.identity.session_id.clone(),
                timestamp: now_iso8601(),
                tool_name: codex_tool_name(tool),
                summary: summarize_codex_tool_input(tool),
            });
        }

        watch_events.push(watch::WatchEvent::CodexTurnSummary {
            session_id: accounting.identity.session_id.clone(),
            status: codex_status_label(&accounting.status).to_string(),
            requested_model: accounting.requested_model.clone(),
            served_model: accounting.served_model.clone(),
            input_tokens: accounting.input_tokens,
            cached_input_tokens: accounting.cached_input_tokens,
            uncached_input_tokens: accounting.uncached_input_tokens,
            output_tokens: accounting.output_tokens,
            reasoning_output_tokens: accounting.reasoning_output_tokens,
            total_tokens: accounting.total_tokens,
        });

        watch_events.push(watch::WatchEvent::ContextStatus {
            session_id: accounting.identity.session_id.clone(),
            fill_percent: context_fill_percent,
            context_window_tokens: Some(context_window_tokens),
            turns_to_compact: None,
        });
    }

    CodexFinalizationOutcome {
        request_id: request_id.to_string(),
        accounting,
        duration_ms: duration.as_millis() as u64,
        context_window_tokens,
        context_fill_percent,
        response_summary,
        watch_events,
    }
}

fn finalize_codex_response(
    request_id: &str,
    request: &codex_request::ParsedCodexRequest,
    response: &codex_response::CodexResponseSummary,
    started_at: &Instant,
    context_window_tokens: u64,
) -> CodexFinalizationOutcome {
    let duration = started_at.elapsed();
    let outcome = build_codex_finalization_outcome(
        request_id,
        request,
        response,
        duration,
        context_window_tokens,
    );
    apply_codex_finalization_outcome(&outcome, duration);
    log_codex_finalization_outcome(&outcome);
    outcome
}

fn apply_codex_finalization_outcome(outcome: &CodexFinalizationOutcome, duration: Duration) {
    let newly_started = upsert_codex_session(&outcome.accounting);
    record_codex_runtime_counters(&outcome.accounting);
    persist_codex_finalization_outcome(outcome);
    remember_codex_turn_and_emit_diagnosis(outcome);

    let metric_model = outcome
        .accounting
        .served_model
        .as_deref()
        .unwrap_or(outcome.accounting.requested_model.as_str());
    metrics::record_codex_turn(metrics::CodexTurnMetric {
        model: metric_model,
        input_tokens: outcome.accounting.input_tokens,
        cached_input_tokens: outcome.accounting.cached_input_tokens,
        uncached_input_tokens: outcome.accounting.uncached_input_tokens,
        output_tokens: outcome.accounting.output_tokens,
        reasoning_output_tokens: outcome.accounting.reasoning_output_tokens,
        total_tokens: outcome.accounting.total_tokens,
        estimated_cost_dollars: outcome.accounting.pricing.cost_dollars.unwrap_or(0.0),
        duration_seconds: duration.as_secs_f64(),
    });
    metrics::record_codex_response_status(
        codex_status_label(&outcome.accounting.status),
        metric_model,
    );
    metrics::record_context_fill_percent(
        "codex_responses",
        metric_model,
        outcome.context_fill_percent,
    );
    for tool in &outcome.accounting.tool_calls {
        metrics::record_tool_call(&codex_tool_name(tool));
    }

    for event in &outcome.watch_events {
        match event {
            watch::WatchEvent::SessionStart { .. } if !newly_started => {}
            watch::WatchEvent::ToolUse { .. } => {
                if codex_watch_event_is_duplicate_or_remember(event) {
                    continue;
                }
                watch::BROADCASTER.broadcast(event.clone());
            }
            watch::WatchEvent::ModelFallback {
                requested, actual, ..
            } => {
                metrics::record_model_fallback(requested, actual);
                watch::BROADCASTER.broadcast(event.clone());
            }
            watch::WatchEvent::ContextStatus {
                turns_to_compact, ..
            } => {
                let _ = turns_to_compact;
                watch::BROADCASTER.broadcast(event.clone());
            }
            _ => watch::BROADCASTER.broadcast(event.clone()),
        }
    }
}

fn record_codex_runtime_counters(accounting: &codex_accounting::CodexTurnAccounting) {
    let trusted_cost = accounting
        .pricing
        .trusted_for_budget_enforcement
        .then_some(accounting.pricing.cost_dollars)
        .flatten()
        .unwrap_or(0.0);
    let session_id = accounting.identity.session_id.trim();

    if !session_id.is_empty() {
        let mut session = SESSION_BUDGETS.entry(session_id.to_string()).or_default();
        session.total_spend += trusted_cost;
        session.total_tokens = session.total_tokens.saturating_add(accounting.total_tokens);
        session.request_count = session.request_count.saturating_add(1);
    }

    match RUNTIME_STATE.lock() {
        Ok(mut runtime) => {
            runtime.total_spend += trusted_cost;
            runtime.total_tokens = runtime.total_tokens.saturating_add(accounting.total_tokens);
            runtime.request_count = runtime.request_count.saturating_add(1);
        }
        Err(err) => {
            warn!(
                session_id = %accounting.identity.session_id,
                error = %err,
                "failed to update Codex runtime counters"
            );
        }
    }
}

fn upsert_codex_session(accounting: &codex_accounting::CodexTurnAccounting) -> bool {
    let session_key = accounting.identity.fallback_hash.unwrap_or_else(|| {
        codex_request::fallback_session_hash("", &accounting.identity.session_id)
    });
    if let Some(mut existing) = diagnosis::SESSIONS.get_mut(&session_key) {
        existing.last_activity = Instant::now();
        return false;
    }

    diagnosis::SESSIONS.insert(
        session_key,
        diagnosis::SessionState {
            session_id: accounting.identity.session_id.clone(),
            display_name: codex_display_name(accounting),
            model: accounting.requested_model.clone(),
            initial_prompt: accounting.first_user_prompt_excerpt.clone(),
            created_at: Instant::now(),
            last_activity: Instant::now(),
            session_inserted: false,
        },
    );
    true
}

fn log_codex_finalization_outcome(outcome: &CodexFinalizationOutcome) {
    info!(
        phase = "codex_finalization",
        request_id = %outcome.request_id,
        session_id = %outcome.accounting.identity.session_id,
        response_id = outcome.accounting.identity.response_id.as_deref().unwrap_or(""),
        requested_model = %outcome.accounting.requested_model,
        served_model = outcome.accounting.served_model.as_deref().unwrap_or(""),
        status = ?outcome.accounting.status,
        input_tokens = outcome.accounting.input_tokens,
        cached_input_tokens = outcome.accounting.cached_input_tokens,
        uncached_input_tokens = outcome.accounting.uncached_input_tokens,
        output_tokens = outcome.accounting.output_tokens,
        reasoning_output_tokens = outcome.accounting.reasoning_output_tokens,
        total_tokens = outcome.accounting.total_tokens,
        context_window_tokens = outcome.context_window_tokens,
        fill_percent = format!("{:.1}", outcome.context_fill_percent),
        pricing = ?outcome.accounting.pricing.status,
        trusted_for_budget_enforcement = outcome.accounting.pricing.trusted_for_budget_enforcement,
        duration_ms = outcome.duration_ms,
        anomalies = ?outcome.accounting.anomalies,
        "Codex response finalized"
    );
}

fn codex_status_label(status: &codex_accounting::CodexTurnStatus) -> &'static str {
    match status {
        codex_accounting::CodexTurnStatus::Completed => "completed",
        codex_accounting::CodexTurnStatus::Failed => "failed",
        codex_accounting::CodexTurnStatus::Incomplete => "incomplete",
        codex_accounting::CodexTurnStatus::Unknown => "unknown",
    }
}

fn codex_cost_source(accounting: &codex_accounting::CodexTurnAccounting) -> String {
    match &accounting.pricing.status {
        codex_accounting::CodexPricingStatus::EstimatedApiPricing { cost_source, .. } => {
            cost_source.clone()
        }
        codex_accounting::CodexPricingStatus::UnknownModel { model } => {
            pricing::unpriced_unknown_model_cost_source(model)
        }
    }
}

fn codex_tool_name(tool: &codex_response::CodexToolCallSummary) -> String {
    tool.name
        .clone()
        .unwrap_or_else(|| format!("custom_tool_call:{}", tool.id))
}

fn summarize_codex_tool_input(tool: &codex_response::CodexToolCallSummary) -> String {
    let input = tool.input.trim();
    if input.is_empty() {
        return tool.id.clone();
    }
    if let Ok(value) = serde_json::from_str::<Value>(input) {
        return summarize_structured_tool_input(&value)
            .unwrap_or_else(|| truncate_detail(input, 100));
    }
    truncate_detail(input, 100)
}

fn codex_tool_calls_json(accounting: &codex_accounting::CodexTurnAccounting) -> String {
    let calls = accounting
        .tool_calls
        .iter()
        .map(|tool| {
            serde_json::json!({
                "id": tool.id.as_str(),
                "name": tool.name.as_deref(),
                "input": tool.input.as_str(),
            })
        })
        .collect::<Vec<_>>();
    serde_json::to_string(&calls).unwrap_or_else(|_| "[]".to_string())
}

fn codex_accounting_anomalies_json(accounting: &codex_accounting::CodexTurnAccounting) -> String {
    let anomalies = accounting
        .anomalies
        .iter()
        .map(|anomaly| match anomaly {
            codex_accounting::CodexAccountingAnomaly::CachedInputExceedsInput {
                input_tokens,
                cached_input_tokens,
            } => serde_json::json!({
                "type": "cached_input_exceeds_input",
                "input_tokens": input_tokens,
                "cached_input_tokens": cached_input_tokens,
            }),
            codex_accounting::CodexAccountingAnomaly::ReportedTotalTokensMismatch {
                reported_total_tokens,
                local_total_tokens,
            } => serde_json::json!({
                "type": "reported_total_tokens_mismatch",
                "reported_total_tokens": reported_total_tokens,
                "local_total_tokens": local_total_tokens,
            }),
        })
        .collect::<Vec<_>>();
    serde_json::to_string(&anomalies).unwrap_or_else(|_| "[]".to_string())
}

fn saturating_u64_to_u32(value: u64) -> u32 {
    value.min(u64::from(u32::MAX)) as u32
}

fn count_codex_accounting_anomalies_json(raw: Option<&str>) -> u32 {
    let Some(raw) = raw else {
        return 0;
    };
    serde_json::from_str::<Value>(raw)
        .ok()
        .and_then(|value| value.as_array().map(|items| items.len() as u32))
        .unwrap_or(0)
}

fn codex_turn_snapshot_from_outcome(
    outcome: &CodexFinalizationOutcome,
    turn_number: u32,
) -> diagnosis::TurnSnapshot {
    let accounting = &outcome.accounting;
    diagnosis::TurnSnapshot {
        turn_number,
        timestamp: Instant::now(),
        provider: Some("codex_responses".to_string()),
        input_tokens: saturating_u64_to_u32(accounting.input_tokens),
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
        output_tokens: saturating_u64_to_u32(accounting.output_tokens),
        codex_status: Some(codex_status_label(&accounting.status).to_string()),
        codex_cached_input_tokens: saturating_u64_to_u32(accounting.cached_input_tokens),
        codex_uncached_input_tokens: saturating_u64_to_u32(accounting.uncached_input_tokens),
        codex_reasoning_output_tokens: saturating_u64_to_u32(accounting.reasoning_output_tokens),
        codex_accounting_anomaly_count: accounting.anomalies.len() as u32,
        ttft_ms: outcome.duration_ms,
        tool_calls: accounting.tool_calls.iter().map(codex_tool_name).collect(),
        tool_results_failed: 0,
        mcp_tool_failures: 0,
        gap_from_prev_secs: 0.0,
        context_utilization: outcome.context_fill_percent / 100.0,
        context_window_tokens: outcome.context_window_tokens,
        frustration_signals: 0,
        requested_model: Some(accounting.requested_model.clone()),
        actual_model: accounting.served_model.clone(),
        response_summary: outcome.response_summary.clone(),
    }
}

fn remember_codex_turn_and_emit_diagnosis(outcome: &CodexFinalizationOutcome) {
    let session_id = &outcome.accounting.identity.session_id;
    if session_id.is_empty() {
        return;
    }

    let (report, latest_turn) = {
        let mut turns = diagnosis::SESSION_TURNS
            .entry(session_id.clone())
            .or_default();
        let turn_number = turns.len() as u32 + 1;
        let snapshot = codex_turn_snapshot_from_outcome(outcome, turn_number);
        turns.push(snapshot);
        (
            diagnosis::analyze_session(session_id, turns.as_slice()),
            turn_number,
        )
    };

    let new_non_heuristic_cause = report
        .causes
        .iter()
        .any(|cause| !cause.is_heuristic && cause.turn_first_noticed == latest_turn);
    if new_non_heuristic_cause {
        watch::BROADCASTER.broadcast(watch::WatchEvent::Diagnosis {
            session_id: session_id.clone(),
            report,
        });
    }
}

fn record_codex_turn_command(outcome: &CodexFinalizationOutcome, timestamp: String) -> DbCommand {
    let accounting = &outcome.accounting;
    DbCommand::RecordCodexTurn(Box::new(RecordCodexTurnCommand {
        request_id: outcome.request_id.clone(),
        session_id: accounting.identity.session_id.clone(),
        timestamp: timestamp.clone(),
        requested_model: accounting.requested_model.clone(),
        served_model: accounting.served_model.clone(),
        status: codex_status_label(&accounting.status).to_string(),
        input_tokens: accounting.input_tokens,
        cached_input_tokens: accounting.cached_input_tokens,
        uncached_input_tokens: accounting.uncached_input_tokens,
        output_tokens: accounting.output_tokens,
        reasoning_output_tokens: accounting.reasoning_output_tokens,
        total_tokens: accounting.total_tokens,
        duration_ms: outcome.duration_ms,
        context_utilization: outcome.context_fill_percent / 100.0,
        context_window_tokens: outcome.context_window_tokens,
        display_name: codex_display_name(accounting),
        initial_prompt: accounting.first_user_prompt_excerpt.clone(),
        response_summary: outcome.response_summary.clone(),
        response_id: accounting.identity.response_id.clone(),
        failure_detail: accounting.failure_detail.clone(),
        incomplete_detail: accounting.incomplete_detail.clone(),
        tool_names_json: "[]".to_string(),
        tool_calls_json: codex_tool_calls_json(accounting),
        accounting_anomalies_json: codex_accounting_anomalies_json(accounting),
        cost_dollars: accounting.pricing.cost_dollars.unwrap_or(0.0),
        cost_source: codex_cost_source(accounting),
        trusted_for_budget_enforcement: accounting.pricing.trusted_for_budget_enforcement,
    }))
}

fn persist_codex_finalization_outcome(outcome: &CodexFinalizationOutcome) {
    if outcome.accounting.identity.session_id.is_empty() {
        return;
    }
    if let Err(err) = DB_TX.send(record_codex_turn_command(outcome, now_iso8601())) {
        warn!(
            request_id = %outcome.request_id,
            error = %err,
            "failed to queue Codex turn persistence"
        );
    }
}

fn repo_name_from_codex_initial_prompt(prompt: &str) -> Option<String> {
    let marker = "AGENTS.md instructions for ";
    let start = prompt.find(marker)? + marker.len();
    let path = prompt[start..]
        .split(|ch: char| ch.is_whitespace() || ch == '<')
        .next()
        .unwrap_or("")
        .trim_matches(|ch| matches!(ch, '"' | '\'' | '`'));
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
        .filter(|name| !name.is_empty())
}

fn codex_display_name(accounting: &codex_accounting::CodexTurnAccounting) -> String {
    accounting
        .identity
        .cwd
        .as_deref()
        .and_then(|cwd| cwd.rsplit('/').find(|part| !part.is_empty()))
        .map(str::to_string)
        .filter(|name| !name.is_empty())
        .or_else(|| {
            accounting
                .first_user_prompt_excerpt
                .as_deref()
                .and_then(repo_name_from_codex_initial_prompt)
        })
        .unwrap_or_else(|| accounting.requested_model.clone())
}

fn persisted_session_display_name(
    session_id: &str,
    model: Option<&str>,
    initial_prompt: Option<&str>,
) -> String {
    initial_prompt
        .and_then(repo_name_from_codex_initial_prompt)
        .or_else(|| model.map(str::to_string))
        .unwrap_or_else(|| {
            if session_id.len() > 20 {
                session_id[..20].to_string()
            } else {
                session_id.to_string()
            }
        })
}

#[allow(clippy::too_many_arguments)]
fn finalize_selected_response(
    acc: &mut SelectedResponseAccumulator,
    codex_request: Option<&codex_request::ParsedCodexRequest>,
    request_id: &str,
    model: &str,
    started_at: &Instant,
    context_window_tokens: u64,
) -> Option<SelectedFinalizationOutcome> {
    match acc {
        SelectedResponseAccumulator::CodexResponses { accumulator, .. } => {
            if let Err(err) = accumulator.finish() {
                warn!(request_id, error = %err, "failed to finish Codex Responses accumulator");
                return None;
            }
            let summary = accumulator.summary();
            let Some(codex_request) = codex_request else {
                warn!(
                    request_id,
                    requested_model = model,
                    "Codex response completed without parsed Codex request metadata"
                );
                return None;
            };
            Some(SelectedFinalizationOutcome::Codex(finalize_codex_response(
                request_id,
                codex_request,
                &summary,
                started_at,
                context_window_tokens,
            )))
        }
    }
}

fn observe_selected_finalization_outcome(outcome: &Option<SelectedFinalizationOutcome>) {
    if let Some(SelectedFinalizationOutcome::Codex(outcome)) = outcome {
        debug!(
            request_id = %outcome.request_id,
            status = ?outcome.accounting.status,
            "selected Codex finalization outcome recorded"
        );
    }
}

fn last_session_response_summary(turns: &[diagnosis::TurnSnapshot]) -> String {
    turns
        .iter()
        .rev()
        .filter_map(|t| t.response_summary.as_ref())
        .find(|summary| !summary.is_empty())
        .cloned()
        .unwrap_or_default()
}

fn session_timeout_secs() -> u64 {
    env_u64("CODEX_BLACKBOX_SESSION_TIMEOUT_MINUTES", 5) * 60
}

fn parse_tool_calls_json(raw: &str) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(raw).unwrap_or_default()
}

fn parse_codex_tool_calls_json(raw: &str) -> Vec<String> {
    serde_json::from_str::<Value>(raw)
        .ok()
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|item| {
            item.get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.trim().is_empty())
                .map(str::to_string)
                .or_else(|| {
                    item.get("id")
                        .and_then(Value::as_str)
                        .filter(|id| !id.trim().is_empty())
                        .map(|id| format!("custom_tool_call:{id}"))
                })
        })
        .collect()
}

fn load_turn_snapshots_from_db(
    conn: &Connection,
    session_id: &str,
) -> rusqlite::Result<Vec<diagnosis::TurnSnapshot>> {
    let mut stmt = conn.prepare(
        "SELECT turn_number, input_tokens, cache_read_tokens, cache_creation_tokens, \
         output_tokens, ttft_ms, tool_calls, tool_failures, gap_from_prev_secs, \
         context_utilization, context_window_tokens, frustration_signals, requested_model, \
         actual_model, response_summary, provider, codex_status, codex_cached_input_tokens, \
         codex_uncached_input_tokens, codex_reasoning_output_tokens, codex_accounting_anomalies \
         FROM turn_snapshots \
         WHERE session_id = ?1 AND provider = 'codex_responses' \
         ORDER BY turn_number ASC",
    )?;

    let turns = stmt
        .query_map(rusqlite::params![session_id], |row| {
            let tool_calls_raw = row.get::<_, String>(6)?;
            let input_tokens = row.get::<_, i64>(1)?.max(0) as u32;
            let cache_read_tokens = row.get::<_, i64>(2)?.max(0) as u32;
            let cache_creation_tokens = row.get::<_, i64>(3)?.max(0) as u32;
            let requested_model = row.get::<_, Option<String>>(12)?;
            let actual_model = row.get::<_, Option<String>>(13)?;
            let context_window_tokens = row
                .get::<_, Option<i64>>(10)?
                .map(|value| value.max(0) as u64)
                .filter(|value| *value > 0)
                .unwrap_or_else(|| {
                    infer_context_window_tokens(
                        requested_model.as_deref(),
                        actual_model.as_deref(),
                        input_tokens as u64,
                        cache_read_tokens as u64,
                        cache_creation_tokens as u64,
                    )
                });
            let response_summary = row.get::<_, Option<String>>(14)?;
            Ok(diagnosis::TurnSnapshot {
                turn_number: row.get::<_, i64>(0)?.max(0) as u32,
                timestamp: Instant::now(),
                provider: row.get::<_, Option<String>>(15)?,
                input_tokens,
                cache_read_tokens,
                cache_creation_tokens,
                output_tokens: row.get::<_, i64>(4)?.max(0) as u32,
                codex_status: row.get::<_, Option<String>>(16)?,
                codex_cached_input_tokens: row.get::<_, i64>(17)?.max(0) as u32,
                codex_uncached_input_tokens: row.get::<_, i64>(18)?.max(0) as u32,
                codex_reasoning_output_tokens: row.get::<_, i64>(19)?.max(0) as u32,
                codex_accounting_anomaly_count: count_codex_accounting_anomalies_json(
                    row.get::<_, Option<String>>(20)?.as_deref(),
                ),
                ttft_ms: row.get::<_, i64>(5)?.max(0) as u64,
                tool_calls: parse_tool_calls_json(&tool_calls_raw),
                tool_results_failed: row.get::<_, i64>(7)?.max(0) as u32,
                mcp_tool_failures: 0,
                gap_from_prev_secs: row.get::<_, f64>(8)?.max(0.0),
                context_utilization: context_fill_ratio(
                    input_tokens as u64,
                    cache_read_tokens as u64,
                    cache_creation_tokens as u64,
                    context_window_tokens,
                ),
                context_window_tokens,
                frustration_signals: row.get::<_, i64>(11)?.max(0) as u32,
                requested_model,
                actual_model,
                response_summary: response_summary.filter(|s| !s.trim().is_empty()),
            })
        })?
        .filter_map(|row| row.ok())
        .collect::<Vec<_>>();

    Ok(turns)
}

#[derive(Debug)]
struct PersistedWatchSession {
    session_id: String,
    model: Option<String>,
    display_name: Option<String>,
    initial_prompt: Option<String>,
    ended_at: Option<String>,
}

#[derive(Debug)]
struct PersistedWatchTurn {
    timestamp: String,
    status: String,
    requested_model: String,
    served_model: Option<String>,
    input_tokens: u64,
    cached_input_tokens: u64,
    uncached_input_tokens: u64,
    output_tokens: u64,
    reasoning_output_tokens: u64,
    total_tokens: u64,
    context_utilization: f64,
    context_window_tokens: Option<u64>,
    tool_calls: Vec<String>,
}

fn load_persisted_watch_sessions(
    conn: &Connection,
    session_filter: Option<&str>,
    limit: usize,
) -> rusqlite::Result<Vec<PersistedWatchSession>> {
    if let Some(session_id) = session_filter {
        let mut stmt = conn.prepare(
            "SELECT s.session_id, \
                    (SELECT COALESCE(NULLIF(trim(t.requested_model), ''), NULLIF(trim(t.actual_model), '')) \
                     FROM turn_snapshots t \
                     WHERE t.session_id = s.session_id \
                       AND t.provider = 'codex_responses' \
                       AND COALESCE(NULLIF(trim(t.requested_model), ''), NULLIF(trim(t.actual_model), '')) IS NOT NULL \
                     ORDER BY t.turn_number DESC LIMIT 1), \
                    s.display_name, s.initial_prompt, s.ended_at \
             FROM sessions s \
             WHERE s.session_id = ?1 \
               AND EXISTS (
                    SELECT 1 FROM turn_snapshots t \
                    WHERE t.session_id = s.session_id \
                      AND t.provider = 'codex_responses'
               ) \
             LIMIT 1",
        )?;
        return stmt
            .query_map(rusqlite::params![session_id], |row| {
                Ok(PersistedWatchSession {
                    session_id: row.get::<_, String>(0)?,
                    model: row.get::<_, Option<String>>(1)?,
                    display_name: row.get::<_, Option<String>>(2)?,
                    initial_prompt: row.get::<_, Option<String>>(3)?,
                    ended_at: row.get::<_, Option<String>>(4)?,
                })
            })?
            .collect();
    }

    let mut stmt = conn.prepare(
        "SELECT s.session_id, \
                (SELECT COALESCE(NULLIF(trim(t.requested_model), ''), NULLIF(trim(t.actual_model), '')) \
                 FROM turn_snapshots t \
                 WHERE t.session_id = s.session_id \
                   AND t.provider = 'codex_responses' \
                   AND COALESCE(NULLIF(trim(t.requested_model), ''), NULLIF(trim(t.actual_model), '')) IS NOT NULL \
                 ORDER BY t.turn_number DESC LIMIT 1), \
                s.display_name, s.initial_prompt, s.ended_at \
         FROM sessions s \
         WHERE EXISTS (
            SELECT 1 FROM turn_snapshots t \
            WHERE t.session_id = s.session_id \
              AND t.provider = 'codex_responses'
         ) \
         ORDER BY COALESCE(ended_at, started_at) DESC LIMIT ?1",
    )?;
    let mut sessions = stmt
        .query_map(rusqlite::params![limit as i64], |row| {
            Ok(PersistedWatchSession {
                session_id: row.get::<_, String>(0)?,
                model: row.get::<_, Option<String>>(1)?,
                display_name: row.get::<_, Option<String>>(2)?,
                initial_prompt: row.get::<_, Option<String>>(3)?,
                ended_at: row.get::<_, Option<String>>(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    sessions.reverse();
    Ok(sessions)
}

fn load_persisted_watch_turns(
    conn: &Connection,
    session_id: &str,
) -> rusqlite::Result<Vec<PersistedWatchTurn>> {
    let mut stmt = conn.prepare(
        "SELECT timestamp, codex_status, requested_model, actual_model, \
                codex_input_tokens, codex_cached_input_tokens, \
                codex_uncached_input_tokens, codex_output_tokens, \
                codex_reasoning_output_tokens, codex_total_tokens, \
                context_utilization, context_window_tokens, codex_tool_calls \
         FROM turn_snapshots \
         WHERE session_id = ?1 AND provider = 'codex_responses' \
         ORDER BY turn_number ASC",
    )?;

    let rows = stmt.query_map(rusqlite::params![session_id], |row| {
        let context_window_tokens = row
            .get::<_, Option<i64>>(11)?
            .map(|value| value.max(0) as u64)
            .filter(|value| *value > 0);
        let tool_calls_raw = row.get::<_, Option<String>>(12)?.unwrap_or_default();
        Ok(PersistedWatchTurn {
            timestamp: row.get::<_, String>(0)?,
            status: row
                .get::<_, Option<String>>(1)?
                .unwrap_or_else(|| "completed".to_string()),
            requested_model: row
                .get::<_, Option<String>>(2)?
                .unwrap_or_else(|| "unknown".to_string()),
            served_model: row.get::<_, Option<String>>(3)?,
            input_tokens: row.get::<_, i64>(4)?.max(0) as u64,
            cached_input_tokens: row.get::<_, i64>(5)?.max(0) as u64,
            uncached_input_tokens: row.get::<_, i64>(6)?.max(0) as u64,
            output_tokens: row.get::<_, i64>(7)?.max(0) as u64,
            reasoning_output_tokens: row.get::<_, i64>(8)?.max(0) as u64,
            total_tokens: row.get::<_, i64>(9)?.max(0) as u64,
            context_utilization: row.get::<_, Option<f64>>(10)?.unwrap_or(0.0),
            context_window_tokens,
            tool_calls: parse_codex_tool_calls_json(&tool_calls_raw),
        })
    })?;
    rows.collect()
}

fn load_persisted_watch_replay_events(
    conn: &Connection,
    session_filter: Option<&str>,
    limit: usize,
) -> rusqlite::Result<Vec<watch::WatchEvent>> {
    let sessions = load_persisted_watch_sessions(conn, session_filter, limit)?;
    let mut events = Vec::new();
    let postmortem_ready_cutoff =
        epoch_to_iso8601(now_epoch_secs().saturating_sub(session_timeout_secs()));

    for session in sessions {
        let display_name = session.display_name.clone().unwrap_or_else(|| {
            persisted_session_display_name(
                &session.session_id,
                session.model.as_deref(),
                session.initial_prompt.as_deref(),
            )
        });
        let model = session
            .model
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        events.push(watch::WatchEvent::SessionStart {
            session_id: session.session_id.clone(),
            display_name,
            model,
            initial_prompt: session.initial_prompt.clone(),
        });

        let turns = load_persisted_watch_turns(conn, &session.session_id)?;
        let total_turns = turns.len() as u32;
        let total_tokens = turns.iter().map(|turn| turn.total_tokens).sum::<u64>();

        for turn in turns {
            for tool_name in &turn.tool_calls {
                events.push(watch::WatchEvent::ToolUse {
                    session_id: session.session_id.clone(),
                    timestamp: turn.timestamp.clone(),
                    tool_name: tool_name.clone(),
                    summary: String::new(),
                });
            }

            if let Some(served_model) = turn.served_model.as_ref() {
                if served_model != &turn.requested_model {
                    events.push(watch::WatchEvent::ModelFallback {
                        session_id: session.session_id.clone(),
                        requested: turn.requested_model.clone(),
                        actual: served_model.clone(),
                    });
                }
            }

            events.push(watch::WatchEvent::CodexTurnSummary {
                session_id: session.session_id.clone(),
                status: turn.status,
                requested_model: turn.requested_model,
                served_model: turn.served_model,
                input_tokens: turn.input_tokens,
                cached_input_tokens: turn.cached_input_tokens,
                uncached_input_tokens: turn.uncached_input_tokens,
                output_tokens: turn.output_tokens,
                reasoning_output_tokens: turn.reasoning_output_tokens,
                total_tokens: turn.total_tokens,
            });
            events.push(watch::WatchEvent::ContextStatus {
                session_id: session.session_id.clone(),
                fill_percent: (turn.context_utilization * 100.0).clamp(0.0, 100.0),
                context_window_tokens: turn.context_window_tokens,
                turns_to_compact: None,
            });
        }

        if session
            .ended_at
            .as_deref()
            .is_some_and(|ended_at| ended_at <= postmortem_ready_cutoff.as_str())
            && total_turns > 0
        {
            events.push(watch::WatchEvent::PostmortemReady {
                session_id: session.session_id.clone(),
                total_turns,
                total_tokens,
                reason: "session idle enough to review".to_string(),
                postmortem_command: postmortem_command_for_session(&session.session_id),
            });
        }
    }

    Ok(events)
}

fn watch_event_session_id(event: &watch::WatchEvent) -> Option<&str> {
    match event {
        watch::WatchEvent::ToolUse { session_id, .. }
        | watch::WatchEvent::SessionStart { session_id, .. }
        | watch::WatchEvent::SessionEnd { session_id, .. }
        | watch::WatchEvent::FrustrationSignal { session_id, .. }
        | watch::WatchEvent::CompactionLoop { session_id, .. }
        | watch::WatchEvent::Diagnosis { session_id, .. }
        | watch::WatchEvent::PostmortemReady { session_id, .. }
        | watch::WatchEvent::ModelFallback { session_id, .. }
        | watch::WatchEvent::CodexTurnSummary { session_id, .. }
        | watch::WatchEvent::ContextStatus { session_id, .. } => Some(session_id.as_str()),
        watch::WatchEvent::Cooldown { .. } => None,
    }
}

fn should_load_persisted_watch_replay(
    params: &std::collections::HashMap<String, String>,
    session_filter: Option<&str>,
    history: &[watch::WatchEvent],
) -> bool {
    let requested = session_filter.is_some()
        || params
            .get("replay")
            .is_some_and(|value| matches!(value.as_str(), "1" | "true" | "recent"));
    if !requested {
        return false;
    }

    !history.iter().any(|event| {
        watch_event_session_id(event).is_some() && event_matches_session(event, session_filter)
    })
}

fn history_contains_session_start(history: &[watch::WatchEvent], session_id: &str) -> bool {
    history.iter().any(|event| {
        matches!(
            event,
            watch::WatchEvent::SessionStart {
                session_id: event_session_id,
                ..
            } if event_session_id == session_id
        )
    })
}

fn latest_response_summary_from_db(
    conn: &Connection,
    session_id: &str,
) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT response_summary FROM turn_snapshots \
         WHERE session_id = ?1 AND provider = 'codex_responses' \
           AND response_summary IS NOT NULL AND trim(response_summary) != '' \
         ORDER BY turn_number DESC LIMIT 1",
        rusqlite::params![session_id],
        |row| row.get::<_, String>(0),
    )
    .optional()
}

fn repair_session_recall_codex_summaries(conn: &Connection) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare(
        "SELECT r.session_id, r.final_response_summary \
         FROM session_recall r \
         WHERE EXISTS (
            SELECT 1 FROM turn_snapshots t \
            WHERE t.session_id = r.session_id \
              AND t.provider = 'codex_responses'
         )",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?.unwrap_or_default(),
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);

    for (session_id, stored_summary) in rows {
        let codex_summary = latest_response_summary_from_db(conn, &session_id)?.unwrap_or_default();
        if stored_summary != codex_summary {
            conn.execute(
                "UPDATE session_recall \
                 SET final_response_summary = ?2 \
                 WHERE session_id = ?1",
                rusqlite::params![session_id, codex_summary],
            )?;
        }
    }

    Ok(())
}

fn diagnosis_outcome_needs_refresh(outcome: Option<&str>) -> bool {
    matches!(
        outcome,
        Some("Completed" | "PartiallyCompleted" | "Abandoned")
    ) || outcome
        .map(|value| {
            matches!(
                value,
                "Likely Completed" | "Likely Partially Completed" | "Likely Abandoned"
            )
        })
        .unwrap_or(false)
}

fn session_completed_at_from_db(conn: &Connection, session_id: &str) -> String {
    conn.query_row(
        "SELECT COALESCE(ended_at, started_at) FROM sessions WHERE session_id = ?1",
        rusqlite::params![session_id],
        |row| row.get::<_, Option<String>>(0),
    )
    .optional()
    .ok()
    .flatten()
    .flatten()
    .unwrap_or_else(now_iso8601)
}

fn apply_persisted_costs_to_report(
    report: &mut diagnosis::DiagnosisReport,
    estimated: &EstimatedAggregate,
) {
    report.estimated_total_cost_dollars = estimated.estimated_cost_dollars;
    report.cost_source = estimated.cost_source.clone();
    report.trusted_for_budget_enforcement = estimated.trusted_for_budget_enforcement;
}

fn persist_session_diagnosis_report(
    conn: &Connection,
    session_id: &str,
    completed_at: &str,
    report: &diagnosis::DiagnosisReport,
) -> rusqlite::Result<()> {
    let report = codex_envoy_diagnosis_report(report);
    conn.execute(
        "INSERT OR REPLACE INTO session_diagnoses (session_id, completed_at, \
         outcome, total_turns, total_cost, degraded, degradation_turn, \
         causes_json, advice_json) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        rusqlite::params![
            session_id,
            completed_at,
            report.outcome,
            report.total_turns,
            report.estimated_total_cost_dollars,
            report.degraded as i32,
            report.degradation_turn,
            serde_json::to_string(&report.causes).unwrap_or_default(),
            serde_json::to_string(&report.advice).unwrap_or_default(),
        ],
    )?;
    Ok(())
}

fn build_fresh_diagnosis_report(
    conn: &Connection,
    session_id: &str,
    estimated: &EstimatedAggregate,
) -> rusqlite::Result<Option<(String, diagnosis::DiagnosisReport)>> {
    let turns = load_turn_snapshots_from_db(conn, session_id)?;
    if turns.is_empty() {
        return Ok(None);
    }

    let completed_at = session_completed_at_from_db(conn, session_id);
    let mut report = diagnosis::analyze_session(session_id, &turns);
    apply_persisted_costs_to_report(&mut report, estimated);
    let report = codex_envoy_diagnosis_report(&report);
    persist_session_diagnosis_report(conn, session_id, &completed_at, &report)?;

    Ok(Some((completed_at, report)))
}

fn repair_persisted_session_artifacts(conn: &Connection) -> rusqlite::Result<()> {
    initialize_persistence_schema(conn)?;
    let _ = repair_session_recall_codex_summaries(conn);
    let cutoff = epoch_to_iso8601(now_epoch_secs().saturating_sub(session_timeout_secs()));
    let mut stmt = conn.prepare(
        "SELECT s.session_id, s.ended_at, s.initial_prompt, d.outcome, \
                CASE WHEN r.session_id IS NULL THEN 0 ELSE 1 END \
         FROM sessions s \
         LEFT JOIN session_diagnoses d ON d.session_id = s.session_id \
         LEFT JOIN session_recall r ON r.session_id = s.session_id \
         WHERE s.ended_at IS NOT NULL AND s.ended_at <= ?1 \
           AND EXISTS (
                SELECT 1 FROM turn_snapshots t \
                WHERE t.session_id = s.session_id \
                  AND t.provider = 'codex_responses'
           ) \
           AND (
                d.session_id IS NULL
                OR d.outcome IN ('Completed', 'PartiallyCompleted', 'Abandoned')
                OR (
                    r.session_id IS NULL
                    AND (
                        (s.initial_prompt IS NOT NULL AND trim(s.initial_prompt) != '')
                        OR EXISTS (
                            SELECT 1 FROM turn_snapshots t2
                            WHERE t2.session_id = s.session_id
                              AND t2.provider = 'codex_responses'
                              AND t2.response_summary IS NOT NULL
                              AND trim(t2.response_summary) != ''
                        )
                    )
                )
           ) \
         ORDER BY s.ended_at ASC LIMIT 200",
    )?;

    let candidates = stmt
        .query_map(rusqlite::params![cutoff], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, i64>(4)? != 0,
            ))
        })?
        .filter_map(|row| row.ok())
        .collect::<Vec<_>>();

    for (session_id, ended_at, initial_prompt, stored_outcome, recall_exists) in candidates {
        let turns = load_turn_snapshots_from_db(conn, &session_id)?;
        if turns.is_empty() {
            continue;
        }

        if stored_outcome.is_none() || diagnosis_outcome_needs_refresh(stored_outcome.as_deref()) {
            let mut report = diagnosis::analyze_session(&session_id, &turns);
            let estimated =
                compute_estimated_costs_for_sessions(conn, std::slice::from_ref(&session_id))?
                    .remove(&session_id)
                    .unwrap_or_else(|| CostAccumulator::new().finish());
            apply_persisted_costs_to_report(&mut report, &estimated);
            let completed_at = ended_at.clone().unwrap_or_else(now_iso8601);
            let _ = persist_session_diagnosis_report(conn, &session_id, &completed_at, &report);
        }

        if !recall_exists {
            let summary = latest_response_summary_from_db(conn, &session_id)?.unwrap_or_default();
            let prompt = initial_prompt.unwrap_or_default();
            if !prompt.is_empty() || !summary.is_empty() {
                let _ = conn.execute(
                    "INSERT OR IGNORE INTO session_recall (session_id, initial_prompt, final_response_summary) \
                     VALUES (?1,?2,?3)",
                    rusqlite::params![&session_id, prompt, summary],
                );
            }
        }
    }

    Ok(())
}

/// End a session: run diagnosis, broadcast SessionEnd, persist to DB, clean up.
fn end_session(session_id: &str, _session_model: Option<String>, initial_prompt: Option<String>) {
    SESSION_BUDGETS.remove(session_id);
    if let Some((_, turns)) = diagnosis::SESSION_TURNS.remove(session_id) {
        if turns.is_empty() {
            // No turns collected — still broadcast session end.
            watch::BROADCASTER.broadcast(watch::WatchEvent::SessionEnd {
                session_id: session_id.to_string(),
                outcome: "timeout".to_string(),
                total_tokens: 0,
                total_turns: 0,
            });
            return;
        }
        let report = codex_envoy_diagnosis_report(&diagnosis::analyze_session(session_id, &turns));

        watch::BROADCASTER.broadcast(watch::WatchEvent::SessionEnd {
            session_id: session_id.to_string(),
            outcome: report.outcome.clone(),
            total_tokens: report.total_tokens,
            total_turns: report.total_turns,
        });
        if report.degraded {
            for cause in &report.causes {
                metrics::record_degraded_cause(&cause.cause_type);
            }
            watch::BROADCASTER.broadcast(watch::WatchEvent::Diagnosis {
                session_id: session_id.to_string(),
                report: report.clone(),
            });
        }

        let recall_initial_prompt = initial_prompt.unwrap_or_default();
        let recall_summary = last_session_response_summary(&turns);
        if !recall_initial_prompt.is_empty() || !recall_summary.is_empty() {
            if let Err(err) = DB_TX.send(DbCommand::WriteRecall {
                session_id: session_id.to_string(),
                initial_prompt: recall_initial_prompt,
                final_response_summary: recall_summary,
            }) {
                warn!(
                    session_id,
                    error = %err,
                    "failed to queue session recall for persistence"
                );
            }
        }

        let _ = DB_TX.send(DbCommand::WriteDiagnosis {
            session_id: session_id.to_string(),
            completed_at: now_iso8601(),
            outcome: report.outcome,
            total_turns: report.total_turns,
            total_cost: report.estimated_total_cost_dollars,
            degraded: report.degraded,
            degradation_turn: report.degradation_turn,
            causes_json: serde_json::to_string(&report.causes).unwrap_or_default(),
            advice_json: serde_json::to_string(&report.advice).unwrap_or_default(),
        });
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Derive a short display name from the working directory path.
/// Falls back to model+time if working_dir is empty.
/// Appends a 3-char hash suffix if the name collides with an existing session.
#[cfg(test)]
fn derive_display_name(working_dir: &str, model: &str, sys_prompt_hash: u64) -> String {
    let base = if !working_dir.is_empty() {
        // Extract the last path component: /Users/pradeep/code/idea/codex-blackbox → codex-blackbox
        working_dir
            .rsplit('/')
            .next()
            .unwrap_or(working_dir)
            .to_string()
    } else {
        let short_model = model.to_string();
        let tod = now_epoch_secs() % 86400;
        format!(
            "{}\u{00b7}{:02}:{:02}",
            short_model,
            tod / 3600,
            (tod % 3600) / 60
        )
    };

    // Check if any existing session has the same base name — if so, add a 3-char hash suffix.
    let has_collision = diagnosis::SESSIONS
        .iter()
        .any(|entry| entry.display_name == base);
    if has_collision {
        let suffix = &format!("{:x}", sys_prompt_hash)[..3];
        format!("{}-{}", base, suffix)
    } else {
        base
    }
}

fn extract_header(h: &HttpHeaders, name: &str) -> Option<String> {
    h.headers
        .as_ref()?
        .headers
        .iter()
        .find(|hv| hv.key.eq_ignore_ascii_case(name))
        .map(|hv| {
            if hv.value.is_empty() {
                String::from_utf8_lossy(&hv.raw_value).into_owned()
            } else {
                hv.value.clone()
            }
        })
}

#[cfg(test)]
fn extract_headers(h: &HttpHeaders, name: &str) -> Vec<String> {
    h.headers
        .as_ref()
        .map(|headers| {
            headers
                .headers
                .iter()
                .filter(|hv| hv.key.eq_ignore_ascii_case(name))
                .map(|hv| {
                    if hv.value.is_empty() {
                        String::from_utf8_lossy(&hv.raw_value).into_owned()
                    } else {
                        hv.value.clone()
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

const STANDARD_CONTEXT_WINDOW_TOKENS: u64 = 200_000;
const EXTENDED_CONTEXT_WINDOW_TOKENS: u64 = 1_000_000;

fn configured_context_window_tokens() -> Option<u64> {
    let value = std::env::var("CODEX_BLACKBOX_CONTEXT_WINDOW_TOKENS").ok()?;
    let parsed = value.parse::<u64>().ok()?;
    if parsed == 0 {
        None
    } else {
        Some(parsed)
    }
}

fn resolve_context_window_tokens() -> u64 {
    configured_context_window_tokens().unwrap_or(STANDARD_CONTEXT_WINDOW_TOKENS)
}

fn infer_context_window_tokens(
    _requested_model: Option<&str>,
    _actual_model: Option<&str>,
    input_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
) -> u64 {
    if let Some(configured) = configured_context_window_tokens() {
        return configured;
    }

    let total_input_tokens = input_tokens + cache_read_tokens + cache_creation_tokens;
    if total_input_tokens > STANDARD_CONTEXT_WINDOW_TOKENS {
        return EXTENDED_CONTEXT_WINDOW_TOKENS;
    }

    STANDARD_CONTEXT_WINDOW_TOKENS
}

fn context_fill_ratio(
    input_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
    context_window_tokens: u64,
) -> f64 {
    let total = input_tokens + cache_read_tokens + cache_creation_tokens;
    total as f64 / context_window_tokens.max(1) as f64
}

fn context_fill_percent(
    input_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
    context_window_tokens: u64,
) -> f64 {
    context_fill_ratio(
        input_tokens,
        cache_read_tokens,
        cache_creation_tokens,
        context_window_tokens,
    ) * 100.0
}

static CODEX_TOOL_EVENT_DEDUP: LazyLock<Mutex<HashMap<String, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
const CODEX_TOOL_EVENT_DEDUP_TTL: Duration = Duration::from_secs(300);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RequestMetadataSource {
    CodexResponses,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RequestBodyMetadata {
    source: RequestMetadataSource,
    codex_request: Option<codex_request::ParsedCodexRequest>,
    model: String,
    message_count: usize,
    has_tools: bool,
    system_prompt_length: usize,
    estimated_input_tokens: usize,
    session_hash: u64,
    session_id: String,
    working_dir: String,
    user_prompt_excerpt: String,
}

fn parse_request_body_metadata(
    body: &[u8],
    headers: &codex_request::CodexRequestHeaders,
) -> Option<RequestBodyMetadata> {
    codex_request::parse_codex_responses_request(body, headers.clone())
        .map(|parsed| request_metadata_from_codex(body.len(), parsed))
        .map_err(|err| {
            debug!(error = %err, "skipping non-Responses request body");
            err
        })
        .ok()
}

fn should_skip_chatgpt_auxiliary_request_body(path: &str) -> bool {
    let path = path.split('?').next().unwrap_or(path);
    path.starts_with("/backend-api/") && !path.starts_with("/backend-api/codex/responses")
}

fn request_metadata_from_codex(
    body_len: usize,
    parsed: codex_request::ParsedCodexRequest,
) -> RequestBodyMetadata {
    let session_hash = parsed
        .session
        .fallback_hash
        .unwrap_or_else(|| codex_request::fallback_session_hash("", &parsed.session.id));
    let has_tools = parsed.has_tools();
    RequestBodyMetadata {
        source: RequestMetadataSource::CodexResponses,
        codex_request: Some(parsed.clone()),
        model: parsed.model,
        message_count: parsed.input_count,
        has_tools,
        system_prompt_length: parsed.instructions_length,
        estimated_input_tokens: body_len / 4,
        session_hash,
        session_id: parsed.session.id,
        working_dir: parsed.cwd.unwrap_or_default(),
        user_prompt_excerpt: prompt_excerpt_from_codex_input(parsed.first_user_input.as_deref()),
    }
}

fn prompt_excerpt_from_codex_input(input: Option<&str>) -> String {
    let Some(input) = input else {
        return String::new();
    };
    let trimmed = input
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    const MAX_LEN: usize = 320;
    if trimmed.chars().count() <= MAX_LEN {
        trimmed
    } else {
        let mut out: String = trimmed.chars().take(MAX_LEN).collect();
        out.push('…');
        out
    }
}

fn codex_request_headers_from_ext_proc(h: &HttpHeaders) -> codex_request::CodexRequestHeaders {
    codex_request::CodexRequestHeaders {
        session_id: extract_header(h, "session_id").or_else(|| extract_header(h, "session-id")),
        client_request_id: extract_header(h, "x-client-request-id"),
    }
}

fn codex_response_headers_from_ext_proc(h: &HttpHeaders) -> codex_response::CodexResponseHeaders {
    let pairs = h
        .headers
        .as_ref()
        .map(|headers| {
            headers
                .headers
                .iter()
                .map(|header| {
                    let value = if header.value.is_empty() {
                        String::from_utf8_lossy(&header.raw_value).into_owned()
                    } else {
                        header.value.clone()
                    };
                    (header.key.clone(), value)
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    codex_response::CodexResponseHeaders::from_pairs(pairs)
}

fn looks_like_machine_recall_line(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.starts_with("```") {
        return true;
    }
    if matches!(trimmed, "{" | "}" | "[" | "]" | "}," | "],") {
        return true;
    }
    if (trimmed.starts_with('{') && trimmed.ends_with('}'))
        || (trimmed.starts_with('[') && trimmed.ends_with(']'))
    {
        return true;
    }
    if trimmed.starts_with('"') && trimmed.contains("\":") {
        return true;
    }
    trimmed.starts_with('<') && trimmed.ends_with('>') && !trimmed.contains(' ')
}

fn compact_response_summary(raw: &str) -> Option<String> {
    let trimmed = raw
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .filter(|l| !looks_like_machine_recall_line(l))
        .collect::<Vec<_>>()
        .join(" ");
    if trimmed.is_empty() {
        return None;
    }
    const MAX_LEN: usize = 360;
    if trimmed.chars().count() <= MAX_LEN {
        Some(trimmed)
    } else {
        let mut out: String = trimmed.chars().take(MAX_LEN).collect();
        out.push('…');
        Some(out)
    }
}

fn truncate_detail(raw: &str, max: usize) -> String {
    if raw.len() <= max {
        raw.to_string()
    } else {
        format!("{}...", &raw[..max.min(raw.len())])
    }
}

fn summarize_structured_tool_input(value: &Value) -> Option<String> {
    if let Some(command) = value.get("command").and_then(Value::as_str) {
        return Some(truncate_detail(command, 100));
    }
    if let Some(path) = value
        .get("file_path")
        .or_else(|| value.get("path"))
        .and_then(Value::as_str)
    {
        return Some(truncate_detail(path, 100));
    }
    if let Some(query) = value.get("query").and_then(Value::as_str) {
        return Some(truncate_detail(query, 100));
    }
    serde_json::to_string(value)
        .ok()
        .map(|json| truncate_detail(&json, 100))
}

fn codex_watch_event_dedupe_key(event: &watch::WatchEvent) -> Option<String> {
    match event {
        watch::WatchEvent::ToolUse {
            session_id,
            tool_name,
            summary,
            ..
        } => Some(format!(
            "{}|tool_use|{}|{}",
            session_id,
            tool_name.trim(),
            summary.trim()
        )),
        _ => None,
    }
}

fn codex_watch_event_is_duplicate_or_remember(event: &watch::WatchEvent) -> bool {
    let Some(key) = codex_watch_event_dedupe_key(event) else {
        return false;
    };
    let now = Instant::now();
    let mut seen = CODEX_TOOL_EVENT_DEDUP.lock().unwrap();
    seen.retain(|_, first_seen| now.duration_since(*first_seen) < CODEX_TOOL_EVENT_DEDUP_TTL);
    if seen.contains_key(&key) {
        return true;
    }
    seen.insert(key, now);
    false
}

fn headers_continue() -> HeadersResponse {
    HeadersResponse {
        response: Some(CommonResponse {
            status: ResponseStatus::Continue.into(),
            ..Default::default()
        }),
    }
}
fn body_continue() -> BodyResponse {
    BodyResponse {
        response: Some(CommonResponse {
            status: ResponseStatus::Continue.into(),
            ..Default::default()
        }),
    }
}

// ---------------------------------------------------------------------------
// ext_proc gRPC service
// ---------------------------------------------------------------------------
pub struct CodexBlackboxProcessor;

#[tonic::async_trait]
impl ExternalProcessor for CodexBlackboxProcessor {
    type ProcessStream = ReceiverStream<Result<ProcessingResponse, Status>>;

    async fn process(
        &self,
        request: Request<Streaming<ProcessingRequest>>,
    ) -> Result<Response<Self::ProcessStream>, Status> {
        let mut stream = request.into_inner();
        let (tx, rx) = mpsc::channel(32);

        tokio::spawn(async move {
            let mut request_id = String::new();
            let mut model = String::new();
            let mut started_at = Instant::now();
            let mut response_accumulator = SelectedResponseAccumulator::for_request_source(
                RequestMetadataSource::CodexResponses,
            );
            let mut codex_request_headers = codex_request::CodexRequestHeaders::default();
            let mut codex_observed_model_header: Option<String> = None;
            let mut current_codex_request: Option<codex_request::ParsedCodexRequest> = None;
            let mut request_path = String::new();
            let mut context_window_tokens = STANDARD_CONTEXT_WINDOW_TOKENS;
            let mut finalized = false;

            loop {
                let msg = match stream.message().await {
                    Ok(Some(r)) => r,
                    Ok(None) => {
                        // Stream closed — finalize if not already done via end_of_stream.
                        if !finalized && !model.is_empty() {
                            let outcome = finalize_selected_response(
                                &mut response_accumulator,
                                current_codex_request.as_ref(),
                                &request_id,
                                &model,
                                &started_at,
                                context_window_tokens,
                            );
                            observe_selected_finalization_outcome(&outcome);
                            REQUEST_STATE.remove(&request_id);
                        }
                        break;
                    }
                    Err(e) => {
                        warn!(error = %e, "ext_proc stream error");
                        if !finalized && !model.is_empty() {
                            let outcome = finalize_selected_response(
                                &mut response_accumulator,
                                current_codex_request.as_ref(),
                                &request_id,
                                &model,
                                &started_at,
                                context_window_tokens,
                            );
                            observe_selected_finalization_outcome(&outcome);
                            REQUEST_STATE.remove(&request_id);
                        }
                        break;
                    }
                };

                match msg.request {
                    Some(ExtProcRequest::RequestHeaders(ref h)) => {
                        started_at = Instant::now();
                        request_id = extract_header(h, "x-request-id")
                            .unwrap_or_else(|| format!("req_{}", started_at.elapsed().as_nanos()));
                        codex_request_headers = codex_request_headers_from_ext_proc(h);
                        request_path = extract_header(h, ":path").unwrap_or_default();
                        codex_observed_model_header = extract_header(h, "openai-model")
                            .or_else(|| extract_header(h, "x-openai-model"));
                        if let Some(header_model) = codex_observed_model_header.as_deref() {
                            debug!(request_id = %request_id, header_model, "observed Codex/OpenAI model header on request");
                        }
                        context_window_tokens = configured_context_window_tokens()
                            .unwrap_or(STANDARD_CONTEXT_WINDOW_TOKENS);

                        // Phase 7: process-wide circuit breaker.
                        if let Some(block) = check_circuit_breaker() {
                            warn!(request_id = %request_id, error_type = block.error_type, "request blocked");
                            let response = make_block_response(&block);
                            if tx.send(Ok(response)).await.is_err() {
                                break;
                            }
                            continue; // stream will close after immediate response
                        }

                        let response = ProcessingResponse {
                            response: Some(ExtProcResponse::RequestHeaders(HeadersResponse {
                                response: Some(CommonResponse {
                                    status: ResponseStatus::Continue.into(),
                                    header_mutation: Some(HeaderMutation {
                                        remove_headers: vec!["accept-encoding".into()],
                                        ..Default::default()
                                    }),
                                    ..Default::default()
                                }),
                            })),
                            ..Default::default()
                        };
                        if tx.send(Ok(response)).await.is_err() {
                            break;
                        }
                    }
                    Some(ExtProcRequest::RequestBody(ref b)) => {
                        let parse_start = Instant::now();

                        let mut blocked = false;
                        if should_skip_chatgpt_auxiliary_request_body(&request_path) {
                            debug!(
                                request_id = %request_id,
                                path = %request_path,
                                bytes = b.body.len(),
                                "skipping non-model ChatGPT backend request body parse"
                            );
                        } else {
                            match parse_request_body_metadata(&b.body, &codex_request_headers) {
                                Some(request_metadata) => {
                                    response_accumulator =
                                        SelectedResponseAccumulator::for_request_source(
                                            request_metadata.source,
                                        );
                                    current_codex_request = request_metadata.codex_request.clone();
                                    info!(
                                        phase = "request_body",
                                        request_id = %request_id,
                                        request_source = "codex_responses",
                                        model = %request_metadata.model,
                                        message_count = request_metadata.message_count,
                                        has_tools = request_metadata.has_tools,
                                        system_prompt_length = request_metadata.system_prompt_length,
                                        estimated_input_tokens = request_metadata.estimated_input_tokens,
                                        sys_prompt_hash = request_metadata.session_hash,
                                        session_id = %request_metadata.session_id,
                                        request_model_header = codex_observed_model_header.as_deref().unwrap_or(""),
                                        "ext_proc"
                                    );
                                    if let Some(block) = check_session_budget(
                                        diagnosis::SESSIONS
                                            .get(&request_metadata.session_hash)
                                            .as_deref()
                                            .map(|state| state.session_id.as_str()),
                                    ) {
                                        warn!(request_id = %request_id, error_type = block.error_type, "request blocked");
                                        let response = make_block_response(&block);
                                        if tx.send(Ok(response)).await.is_err() {
                                            break;
                                        }
                                        blocked = true;
                                    }
                                    if !blocked {
                                        context_window_tokens = resolve_context_window_tokens();
                                        REQUEST_STATE.insert(
                                            request_id.clone(),
                                            RequestMeta {
                                                request_id: request_id.clone(),
                                                session_id: request_metadata.session_id.clone(),
                                                model: request_metadata.model.clone(),
                                                message_count: request_metadata.message_count,
                                                has_tools: request_metadata.has_tools,
                                                system_prompt_length: request_metadata
                                                    .system_prompt_length,
                                                estimated_input_tokens: request_metadata
                                                    .estimated_input_tokens,
                                                started_at,
                                            },
                                        );
                                        model = request_metadata.model;
                                    }
                                }
                                None => {
                                    warn!(request_id=%request_id, bytes=b.body.len(), path=%request_path, "failed to parse request JSON");
                                }
                            }
                        }

                        if blocked {
                            continue;
                        }

                        let response = ProcessingResponse {
                            response: Some(ExtProcResponse::RequestBody(body_continue())),
                            ..Default::default()
                        };
                        if tx.send(Ok(response)).await.is_err() {
                            break;
                        }

                        let parse_ms = parse_start.elapsed().as_millis();
                        debug!(request_id=%request_id, parse_ms, "request_body parse time");
                        if parse_ms > 10 {
                            warn!(request_id=%request_id, parse_ms, "request_body parse exceeded 10ms");
                        }
                    }
                    Some(ExtProcRequest::ResponseHeaders(ref h)) => {
                        let response = ProcessingResponse {
                            response: Some(ExtProcResponse::ResponseHeaders(headers_continue())),
                            ..Default::default()
                        };
                        if tx.send(Ok(response)).await.is_err() {
                            break;
                        }

                        let response_headers = codex_response_headers_from_ext_proc(h);
                        let status = response_headers.http_status.unwrap_or(0);
                        response_accumulator.apply_response_headers(&response_headers);

                        if let Some(served_model) = response_headers.served_model.as_deref() {
                            debug!(
                                request_id = %request_id,
                                served_model,
                                "observed Codex/OpenAI model header on response"
                            );
                        }

                        // Phase 7: circuit breaker — only track errors from real API requests
                        // (ones where we parsed a model). Ignores envoy DPE/protocol errors.
                        let mut cooldown_event = None;
                        match RUNTIME_STATE.lock() {
                            Ok(mut runtime) => {
                                if status >= 400 && !model.is_empty() {
                                    runtime.consecutive_errors += 1;
                                    let threshold =
                                        env_u64("CODEX_BLACKBOX_CIRCUIT_BREAKER_THRESHOLD", 5);
                                    if runtime.consecutive_errors >= threshold
                                        && runtime.circuit_open_until.is_none()
                                    {
                                        let retry_after_seconds = 30;
                                        runtime.circuit_open_until = Some(
                                            Instant::now()
                                                + Duration::from_secs(retry_after_seconds),
                                        );
                                        cooldown_event =
                                            Some(cooldown_watch_event(&decision::CooldownFacts {
                                                reason: "upstream errors".to_string(),
                                                retry_after_seconds: Some(retry_after_seconds),
                                            }));
                                        warn!(
                                            consecutive_errors = runtime.consecutive_errors,
                                            http_status = status,
                                            "circuit breaker tripped — blocking requests for 30s"
                                        );
                                    }
                                } else {
                                    runtime.consecutive_errors = 0;
                                    // Auto-reset circuit breaker on success.
                                    if runtime.circuit_open_until.is_some() {
                                        runtime.circuit_open_until = None;
                                        info!("circuit breaker reset after successful response");
                                    }
                                }
                            }
                            Err(err) => {
                                warn!(
                                    request_id = %request_id,
                                    error = %err,
                                    "failed to update circuit breaker after response headers"
                                );
                            }
                        }
                        if let Some(event) = cooldown_event {
                            watch::BROADCASTER.broadcast(event);
                        }
                    }
                    Some(ExtProcRequest::ResponseBody(ref b)) => {
                        let response = ProcessingResponse {
                            response: Some(ExtProcResponse::ResponseBody(body_continue())),
                            ..Default::default()
                        };
                        let send_ok = tx.send(Ok(response)).await.is_ok();
                        // On end_of_stream, envoy closes the channel — send may fail.
                        // Continue processing to finalize the response regardless.
                        if !send_ok && !b.end_of_stream {
                            break;
                        }
                        if response_accumulator.is_sse() && !b.body.is_empty() {
                            let chunk_start = Instant::now();
                            if let Err(err) = response_accumulator.process_chunk(&b.body) {
                                warn!(
                                    request_id = %request_id,
                                    error = %err,
                                    "failed to parse response_body chunk"
                                );
                            }
                            let chunk_ms = chunk_start.elapsed().as_millis();
                            debug!(request_id=%request_id, chunk_ms, bytes=b.body.len(), "response_body chunk parse time");
                            if chunk_ms > 10 {
                                warn!(request_id=%request_id, chunk_ms, bytes=b.body.len(), "response_body chunk parse exceeded 10ms");
                            }
                        }
                        if b.end_of_stream {
                            if !model.is_empty() {
                                let outcome = finalize_selected_response(
                                    &mut response_accumulator,
                                    current_codex_request.as_ref(),
                                    &request_id,
                                    &model,
                                    &started_at,
                                    context_window_tokens,
                                );
                                observe_selected_finalization_outcome(&outcome);
                            }
                            REQUEST_STATE.remove(&request_id);
                            finalized = true;
                        }
                    }
                    Some(ExtProcRequest::RequestTrailers(_))
                    | Some(ExtProcRequest::ResponseTrailers(_)) => {
                        continue;
                    }
                    None => {
                        continue;
                    }
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

// ---------------------------------------------------------------------------
// HTTP server (axum): /health, /api/summary, /watch
// ---------------------------------------------------------------------------
use axum::extract::Json;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Router;

async fn handle_health() -> &'static str {
    "ok"
}

async fn handle_metrics() -> impl IntoResponse {
    match metrics::render() {
        Ok((content_type, body)) => {
            let mut headers = axum::http::HeaderMap::new();
            let value = HeaderValue::from_str(&content_type)
                .unwrap_or_else(|_| HeaderValue::from_static("text/plain; version=0.0.4"));
            headers.insert(header::CONTENT_TYPE, value);
            (StatusCode::OK, headers, body).into_response()
        }
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err).into_response(),
    }
}

async fn handle_summary() -> impl IntoResponse {
    let summary = tokio::task::spawn_blocking(|| {
        let conn = Connection::open(db_path()).ok()?;
        let _ = repair_persisted_session_artifacts(&conn);
        let today = query_summary(&conn, &start_of_today_iso()).ok()?;
        let week = query_summary(&conn, &start_of_week_iso()).ok()?;
        let month = query_summary(&conn, &start_of_month_iso()).ok()?;
        Some(build_summary_response_json(&today, &week, &month))
    })
    .await
    .ok()
    .flatten()
    .unwrap_or(serde_json::json!({"error": "db unavailable"}));

    (
        StatusCode::OK,
        [("content-type", "application/json")],
        serde_json::to_string_pretty(&summary).unwrap_or_default(),
    )
}

async fn handle_guard_state() -> impl IntoResponse {
    let state = serde_json::json!({
        "cooldown": current_cooldown_facts(),
    });

    (
        StatusCode::OK,
        [("content-type", "application/json")],
        serde_json::to_string_pretty(&state).unwrap_or_default(),
    )
}

#[derive(Debug, Deserialize)]
struct CodexObservationRequest {
    #[serde(default)]
    after_request_rowid: i64,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    prompt_excerpt: Option<String>,
}

async fn handle_codex_observations(
    Json(request): Json<CodexObservationRequest>,
) -> impl IntoResponse {
    let result = tokio::task::spawn_blocking(move || {
        let conn = Connection::open(db_path())?;
        load_codex_observation_snapshot(
            &conn,
            request.after_request_rowid,
            request.session_id.as_deref(),
            request.prompt_excerpt.as_deref(),
        )
    })
    .await;

    match result {
        Ok(Ok(snapshot)) => (
            StatusCode::OK,
            [("content-type", "application/json")],
            serde_json::to_string_pretty(&snapshot).unwrap_or_default(),
        )
            .into_response(),
        Ok(Err(err)) => {
            warn!(error = %err, "Codex observation query failed");
            let body = serde_json::json!({"error": "db unavailable"});
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                [("content-type", "application/json")],
                serde_json::to_string_pretty(&body).unwrap_or_default(),
            )
                .into_response()
        }
        Err(err) => {
            warn!(error = %err, "Codex observation query task failed");
            let body = serde_json::json!({"error": "internal error"});
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                [("content-type", "application/json")],
                serde_json::to_string_pretty(&body).unwrap_or_default(),
            )
                .into_response()
        }
    }
}

async fn handle_watch(
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> axum::response::sse::Sse<
    impl futures_core::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>,
> {
    use axum::response::sse::{Event, KeepAlive, Sse};

    // `?session=X` filters to events for that session AND injects a synthetic
    // SessionStart at stream head so subscribers joining mid-session (e.g. a
    // lazy-discovered tmux pane, or a reattach after the 30s replay window)
    // still see the session header box + initial prompt.
    let session_filter = params.get("session").cloned();

    let (history, mut rx) = watch::BROADCASTER.subscribe_with_history();
    let should_load_persisted_replay =
        should_load_persisted_watch_replay(&params, session_filter.as_deref(), &history);
    let mut persisted_replay = if should_load_persisted_replay {
        let session_filter_for_db = session_filter.clone();
        tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path()).ok()?;
            let _ = repair_persisted_session_artifacts(&conn);
            load_persisted_watch_replay_events(&conn, session_filter_for_db.as_deref(), 20).ok()
        })
        .await
        .ok()
        .flatten()
        .unwrap_or_default()
    } else {
        Vec::new()
    };

    // Look up stored session info synchronously before the stream starts.
    let synthetic_start = session_filter.as_ref().and_then(|sid| {
        if history_contains_session_start(&history, sid) {
            return None;
        }
        diagnosis::SESSIONS.iter().find_map(|entry| {
            let s = entry.value();
            if s.session_id == *sid {
                Some(watch::WatchEvent::SessionStart {
                    session_id: s.session_id.clone(),
                    display_name: s.display_name.clone(),
                    model: s.model.clone(),
                    initial_prompt: s.initial_prompt.clone(),
                })
            } else {
                None
            }
        })
    });
    if let Some(watch::WatchEvent::SessionStart { session_id, .. }) = synthetic_start.as_ref() {
        persisted_replay.retain(|event| {
            !matches!(event, watch::WatchEvent::SessionStart { session_id: persisted_id, .. } if persisted_id == session_id)
        });
    }

    let stream = async_stream::stream! {
        // Synthetic SessionStart first if we're filtered to a session.
        if let Some(ev) = synthetic_start {
            if let Ok(json) = serde_json::to_string(&ev) {
                yield Ok(Event::default().data(json));
            }
        }

        for event in persisted_replay {
            if !event_matches_session(&event, session_filter.as_deref()) {
                continue;
            }
            if let Ok(json) = serde_json::to_string(&event) {
                yield Ok(Event::default().data(json));
            }
        }

        // Replay recent history, filtered if a session is specified.
        for event in history {
            if !event_matches_session(&event, session_filter.as_deref()) {
                continue;
            }
            if let Ok(json) = serde_json::to_string(&event) {
                yield Ok(Event::default().data(json));
            }
        }
        loop {
            match rx.recv().await {
                Ok(event) => {
                    if !event_matches_session(&event, session_filter.as_deref()) {
                        continue;
                    }
                    if let Ok(json) = serde_json::to_string(&event) {
                        yield Ok(Event::default().data(json));
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    let msg = serde_json::json!({"type": "lagged", "missed": n});
                    yield Ok(Event::default().data(msg.to_string()));
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    break;
                }
            }
        }
    };

    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

/// True when the event matches the session filter (or no filter is set).
fn event_matches_session(ev: &watch::WatchEvent, filter: Option<&str>) -> bool {
    let Some(want) = filter else {
        return true;
    };
    match ev {
        watch::WatchEvent::ToolUse { session_id, .. }
        | watch::WatchEvent::SessionStart { session_id, .. }
        | watch::WatchEvent::SessionEnd { session_id, .. }
        | watch::WatchEvent::FrustrationSignal { session_id, .. }
        | watch::WatchEvent::CompactionLoop { session_id, .. }
        | watch::WatchEvent::Diagnosis { session_id, .. }
        | watch::WatchEvent::PostmortemReady { session_id, .. }
        | watch::WatchEvent::ModelFallback { session_id, .. }
        | watch::WatchEvent::CodexTurnSummary { session_id, .. }
        | watch::WatchEvent::ContextStatus { session_id, .. } => session_id == want,
        watch::WatchEvent::Cooldown { .. } => true,
    }
}

async fn handle_diagnosis(
    axum::extract::Path(session_id): axum::extract::Path<String>,
) -> impl IntoResponse {
    let result = tokio::task::spawn_blocking(move || {
        let conn = Connection::open(db_path()).ok()?;
        let _ = repair_persisted_session_artifacts(&conn);
        if !session_has_codex_evidence(&conn, &session_id).ok()? {
            return None;
        }
        let estimated =
            compute_estimated_costs_for_sessions(&conn, std::slice::from_ref(&session_id))
                .ok()?
                .remove(&session_id)
                .unwrap_or_else(|| CostAccumulator::new().finish());
        let billing = load_latest_billing_reconciliations(&conn, std::slice::from_ref(&session_id))
            .ok()?
            .remove(&session_id);
        let codex_cached_input =
            load_codex_cached_input_summaries(&conn, std::slice::from_ref(&session_id))
                .ok()?
                .remove(&session_id)
                .unwrap_or_default();
        if let Some((completed_at, report)) =
            build_fresh_diagnosis_report(&conn, &session_id, &estimated).ok()?
        {
            let (causes, advice) = filter_codex_envoy_diagnosis_payload(
                serde_json::to_value(&report.causes).unwrap_or(Value::Array(vec![])),
                serde_json::to_value(&report.advice).unwrap_or(Value::Array(vec![])),
            );
            let (degraded, degradation_turn) = codex_envoy_public_degradation(&causes);
            return Some(build_diagnosis_response_json(
                session_id,
                completed_at,
                report.outcome,
                report.total_turns as i64,
                estimated.estimated_cost_dollars,
                estimated.cost_source,
                estimated.trusted_for_budget_enforcement,
                billing.as_ref().map(|record| record.billed_cost_dollars),
                billing.as_ref().map(|record| record.source.clone()),
                billing.as_ref().map(|record| record.imported_at.clone()),
                codex_cached_input,
                degraded,
                degradation_turn,
                causes,
                advice,
            ));
        }

        let mut stmt = conn
            .prepare(
                "SELECT session_id, completed_at, outcome, total_turns, \
             degraded, degradation_turn, causes_json, advice_json \
             FROM session_diagnoses WHERE session_id = ?1",
            )
            .ok()?;
        stmt.query_row(rusqlite::params![&session_id], |row| {
            let causes_str: String = row.get(6)?;
            let advice_str: String = row.get(7)?;
            let (causes, advice) = filter_codex_envoy_diagnosis_payload(
                serde_json::from_str::<Value>(&causes_str).unwrap_or(Value::Array(vec![])),
                serde_json::from_str::<Value>(&advice_str).unwrap_or(Value::Array(vec![])),
            );
            let (degraded, degradation_turn) = codex_envoy_public_degradation(&causes);
            Ok(build_diagnosis_response_json(
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                estimated.estimated_cost_dollars,
                estimated.cost_source.clone(),
                estimated.trusted_for_budget_enforcement,
                billing.as_ref().map(|record| record.billed_cost_dollars),
                billing.as_ref().map(|record| record.source.clone()),
                billing.as_ref().map(|record| record.imported_at.clone()),
                codex_cached_input,
                degraded,
                degradation_turn,
                causes,
                advice,
            ))
        })
        .optional()
        .ok()?
    })
    .await
    .ok()
    .flatten();

    match result {
        Some(report) => (
            StatusCode::OK,
            [("content-type", "application/json")],
            serde_json::to_string_pretty(&report).unwrap_or_default(),
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

#[derive(Debug)]
struct RecentSessionRow {
    session_id: String,
    started_at: Option<String>,
    model: Option<String>,
    stored_display_name: Option<String>,
    initial_prompt: Option<String>,
    stored_outcome: Option<String>,
    stored_degraded: Option<bool>,
    stored_total_turns: Option<i64>,
    stored_causes_str: Option<String>,
    requested_model: Option<String>,
    served_model: Option<String>,
}

fn load_recent_codex_session_rows(
    conn: &Connection,
    since: &str,
    limit: i64,
) -> rusqlite::Result<Vec<RecentSessionRow>> {
    let mut stmt = conn.prepare(
        "SELECT s.session_id, s.started_at, \
                COALESCE( \
                    (SELECT COALESCE(NULLIF(trim(r_model.served_model), ''), NULLIF(trim(r_model.requested_model), '')) \
                     FROM requests r_model \
                     WHERE r_model.session_id = s.session_id \
                       AND r_model.provider = 'codex_responses' \
                       AND COALESCE(NULLIF(trim(r_model.served_model), ''), NULLIF(trim(r_model.requested_model), '')) IS NOT NULL \
                     ORDER BY r_model.timestamp DESC LIMIT 1), \
                    (SELECT COALESCE(NULLIF(trim(t_model.actual_model), ''), NULLIF(trim(t_model.requested_model), '')) \
                     FROM turn_snapshots t_model \
                     WHERE t_model.session_id = s.session_id \
                       AND t_model.provider = 'codex_responses' \
                       AND COALESCE(NULLIF(trim(t_model.actual_model), ''), NULLIF(trim(t_model.requested_model), '')) IS NOT NULL \
                     ORDER BY t_model.turn_number DESC LIMIT 1) \
                ), \
                s.display_name, s.initial_prompt, d.outcome, d.degraded, \
                d.total_turns, d.causes_json, \
                COALESCE( \
                    (SELECT NULLIF(trim(r.requested_model), '') FROM requests r \
                     WHERE r.session_id = s.session_id \
                       AND r.provider = 'codex_responses' \
                       AND NULLIF(trim(r.requested_model), '') IS NOT NULL \
                     ORDER BY r.timestamp DESC LIMIT 1), \
                    (SELECT NULLIF(trim(t_requested.requested_model), '') FROM turn_snapshots t_requested \
                     WHERE t_requested.session_id = s.session_id \
                       AND t_requested.provider = 'codex_responses' \
                       AND NULLIF(trim(t_requested.requested_model), '') IS NOT NULL \
                     ORDER BY t_requested.turn_number DESC LIMIT 1) \
                ), \
                COALESCE( \
                    (SELECT NULLIF(trim(r.served_model), '') FROM requests r \
                 WHERE r.session_id = s.session_id \
                   AND r.provider = 'codex_responses' \
                   AND NULLIF(trim(r.served_model), '') IS NOT NULL \
                 ORDER BY r.timestamp DESC LIMIT 1), \
                    (SELECT NULLIF(trim(t_served.actual_model), '') FROM turn_snapshots t_served \
                     WHERE t_served.session_id = s.session_id \
                       AND t_served.provider = 'codex_responses' \
                       AND NULLIF(trim(t_served.actual_model), '') IS NOT NULL \
                     ORDER BY t_served.turn_number DESC LIMIT 1) \
                ) \
         FROM sessions s \
         LEFT JOIN session_diagnoses d ON d.session_id = s.session_id \
         WHERE COALESCE(s.ended_at, s.started_at) >= ?1 \
           AND (
                EXISTS (
                    SELECT 1 FROM requests r_codex \
                    WHERE r_codex.session_id = s.session_id \
                      AND r_codex.provider = 'codex_responses'
                ) \
                OR EXISTS (
                    SELECT 1 FROM turn_snapshots t_codex \
                    WHERE t_codex.session_id = s.session_id \
                      AND t_codex.provider = 'codex_responses'
                )
           ) \
         ORDER BY COALESCE(s.ended_at, s.started_at) DESC LIMIT ?2",
    )?;

    let rows = stmt.query_map(rusqlite::params![since, limit], |row| {
        Ok(RecentSessionRow {
            session_id: row.get::<_, String>(0)?,
            started_at: row.get::<_, Option<String>>(1)?,
            model: row.get::<_, Option<String>>(2)?,
            stored_display_name: row.get::<_, Option<String>>(3)?,
            initial_prompt: row.get::<_, Option<String>>(4)?,
            stored_outcome: row.get::<_, Option<String>>(5)?,
            stored_degraded: row.get::<_, Option<i32>>(6)?.map(|value| value != 0),
            stored_total_turns: row.get::<_, Option<i64>>(7)?,
            stored_causes_str: row.get::<_, Option<String>>(8)?,
            requested_model: row.get::<_, Option<String>>(9)?,
            served_model: row.get::<_, Option<String>>(10)?,
        })
    })?;

    rows.collect()
}

async fn handle_sessions(
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let limit: i64 = params
        .get("limit")
        .and_then(|l| l.parse().ok())
        .unwrap_or(20);
    let days: i64 = params.get("days").and_then(|d| d.parse().ok()).unwrap_or(7);

    let result = tokio::task::spawn_blocking(move || {
        let conn = Connection::open(db_path()).ok()?;
        let _ = repair_persisted_session_artifacts(&conn);
        let since_secs = now_epoch_secs() - (days as u64 * 86400);
        let since = epoch_to_iso8601(since_secs);

        let session_rows = load_recent_codex_session_rows(&conn, &since, limit).ok()?;

        let session_ids = session_rows
            .iter()
            .map(|row| row.session_id.clone())
            .collect::<Vec<_>>();
        let estimated_costs = compute_estimated_costs_for_sessions(&conn, &session_ids).ok()?;
        let billing = load_latest_billing_reconciliations(&conn, &session_ids).ok()?;
        let codex_cached_input_summaries =
            load_codex_cached_input_summaries(&conn, &session_ids).ok()?;

        let sessions: Vec<Value> = session_rows
            .into_iter()
            .map(|row| {
                let RecentSessionRow {
                    session_id,
                    started_at,
                    model,
                    stored_display_name,
                    initial_prompt,
                    stored_outcome,
                    stored_degraded,
                    stored_total_turns,
                    stored_causes_str,
                    requested_model,
                    served_model,
                } = row;
                let estimated = estimated_costs
                    .get(&session_id)
                    .cloned()
                    .unwrap_or_else(|| CostAccumulator::new().finish());
                let refreshed = if stored_outcome.is_none()
                    || diagnosis_outcome_needs_refresh(stored_outcome.as_deref())
                {
                    build_fresh_diagnosis_report(&conn, &session_id, &estimated)
                        .ok()
                        .flatten()
                        .map(|(_, report)| report)
                } else {
                    None
                };

                let (outcome, _stored_degraded, total_turns, causes_json) =
                    if let Some(report) = refreshed {
                        let (causes_json, _) = filter_codex_envoy_diagnosis_payload(
                            serde_json::to_value(&report.causes).unwrap_or(Value::Array(vec![])),
                            serde_json::to_value(&report.advice).unwrap_or(Value::Array(vec![])),
                        );
                        let (degraded, _) = codex_envoy_public_degradation(&causes_json);
                        (
                            report.outcome,
                            degraded,
                            report.total_turns as i64,
                            causes_json,
                        )
                    } else {
                        let causes_json = stored_causes_str
                            .as_deref()
                            .and_then(|causes| serde_json::from_str::<Value>(causes).ok())
                            .unwrap_or(Value::Array(vec![]));
                        let (causes_json, _) =
                            filter_codex_envoy_diagnosis_payload(causes_json, Value::Array(vec![]));
                        (
                            stored_outcome.unwrap_or_else(|| "Unknown".to_string()),
                            stored_degraded.unwrap_or(false),
                            stored_total_turns.unwrap_or(0),
                            causes_json,
                        )
                    };

                let (degraded, _) = codex_envoy_public_degradation(&causes_json);
                let primary_cause = if degraded {
                    codex_envoy_primary_degrading_cause(&causes_json).unwrap_or_default()
                } else {
                    String::new()
                };
                let billed = billing.get(&session_id);
                let codex_cached_input = codex_cached_input_summaries
                    .get(&session_id)
                    .copied()
                    .unwrap_or_default();
                let display_name = stored_display_name.unwrap_or_else(|| {
                    persisted_session_display_name(
                        &session_id,
                        model.as_deref(),
                        initial_prompt.as_deref(),
                    )
                });
                build_session_summary_json(
                    session_id,
                    display_name,
                    started_at,
                    outcome,
                    degraded,
                    total_turns,
                    estimated.estimated_cost_dollars,
                    estimated.cost_source,
                    estimated.trusted_for_budget_enforcement,
                    billed.map(|record| record.billed_cost_dollars),
                    billed.map(|record| record.source.clone()),
                    billed.map(|record| record.imported_at.clone()),
                    primary_cause,
                    codex_cached_input,
                    model,
                    requested_model,
                    served_model,
                )
            })
            .collect();

        Some(build_sessions_response_json(sessions))
    })
    .await
    .ok()
    .flatten()
    .unwrap_or(serde_json::json!({"sessions": []}));

    (
        StatusCode::OK,
        [("content-type", "application/json")],
        serde_json::to_string_pretty(&result).unwrap_or_default(),
    )
}

#[derive(Debug, Deserialize)]
struct BillingReconciliationInput {
    session_id: String,
    source: String,
    billed_cost_dollars: f64,
    #[serde(default)]
    imported_at: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum BillingReconciliationRequest {
    Single(BillingReconciliationInput),
    Batch {
        reconciliations: Vec<BillingReconciliationInput>,
    },
}

async fn persist_billing_reconciliation(
    tx: &std_mpsc::Sender<DbCommand>,
    record: BillingReconciliationInput,
) -> Result<(), BillingReconciliationWriteError> {
    let (response_tx, response_rx) = oneshot::channel();
    tx.send(DbCommand::WriteBillingReconciliation {
        session_id: record.session_id,
        imported_at: record.imported_at.unwrap_or_else(now_iso8601),
        source: record.source,
        billed_cost_dollars: record.billed_cost_dollars,
        response_tx,
    })
    .map_err(|_| BillingReconciliationWriteError::DbUnavailable)?;

    response_rx
        .await
        .map_err(|_| BillingReconciliationWriteError::DbUnavailable)?
}

async fn handle_billing_reconciliations(
    Json(payload): Json<BillingReconciliationRequest>,
) -> impl IntoResponse {
    let records = match payload {
        BillingReconciliationRequest::Single(record) => vec![record],
        BillingReconciliationRequest::Batch { reconciliations } => reconciliations,
    };

    if records.is_empty() {
        let body = serde_json::json!({
            "inserted": 0,
            "error": "missing reconciliations"
        });
        return (
            StatusCode::BAD_REQUEST,
            [("content-type", "application/json")],
            serde_json::to_string_pretty(&body).unwrap_or_default(),
        )
            .into_response();
    }

    let mut inserted = 0usize;
    for record in records {
        if record.session_id.trim().is_empty()
            || record.source.trim().is_empty()
            || !record.billed_cost_dollars.is_finite()
            || record.billed_cost_dollars < 0.0
        {
            let body = serde_json::json!({
                "inserted": inserted,
                "error": "invalid reconciliation payload"
            });
            return (
                StatusCode::BAD_REQUEST,
                [("content-type", "application/json")],
                serde_json::to_string_pretty(&body).unwrap_or_default(),
            )
                .into_response();
        }

        match persist_billing_reconciliation(&DB_TX, record).await {
            Ok(()) => inserted += 1,
            Err(BillingReconciliationWriteError::DbUnavailable) => {
                let body = serde_json::json!({
                    "inserted": inserted,
                    "error": "db unavailable"
                });
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    [("content-type", "application/json")],
                    serde_json::to_string_pretty(&body).unwrap_or_default(),
                )
                    .into_response();
            }
            Err(BillingReconciliationWriteError::UnknownSession(session_id)) => {
                let body = serde_json::json!({
                    "inserted": inserted,
                    "error": format!("unknown session_id: {session_id}")
                });
                return (
                    StatusCode::NOT_FOUND,
                    [("content-type", "application/json")],
                    serde_json::to_string_pretty(&body).unwrap_or_default(),
                )
                    .into_response();
            }
            Err(BillingReconciliationWriteError::Sqlite(err)) => {
                let body = serde_json::json!({
                    "inserted": inserted,
                    "error": format!("failed to persist reconciliation: {err}")
                });
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    [("content-type", "application/json")],
                    serde_json::to_string_pretty(&body).unwrap_or_default(),
                )
                    .into_response();
            }
        }
    }

    let body = serde_json::json!({
        "inserted": inserted,
    });
    (
        StatusCode::OK,
        [("content-type", "application/json")],
        serde_json::to_string_pretty(&body).unwrap_or_default(),
    )
        .into_response()
}

fn normalize_search_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last_was_space = true;
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_was_space = false;
        } else if !last_was_space {
            out.push(' ');
            last_was_space = true;
        }
    }
    out.trim().to_string()
}

fn tokenize_search_text(text: &str) -> Vec<String> {
    normalize_search_text(text)
        .split_whitespace()
        .filter(|term| term.len() >= 2)
        .map(|term| term.to_string())
        .collect()
}

fn search_term_set(text: &str) -> HashSet<String> {
    tokenize_search_text(text).into_iter().collect()
}

fn score_recall_doc(
    query: &str,
    query_terms: &[String],
    initial_prompt: &str,
    final_response_summary: &str,
    model: &str,
) -> Option<i64> {
    let query_norm = normalize_search_text(query);
    let prompt_norm = normalize_search_text(initial_prompt);
    let summary_norm = normalize_search_text(final_response_summary);
    let model_terms = search_term_set(model);
    let prompt_terms = search_term_set(initial_prompt);
    let summary_terms = search_term_set(final_response_summary);

    let mut score = 0i64;
    let mut matched_terms = 0usize;

    if !query_norm.is_empty() {
        if prompt_norm.contains(&query_norm) {
            score += 48;
        }
        if summary_norm.contains(&query_norm) {
            score += 42;
        }
    }

    for term in query_terms {
        let mut hit = false;
        if prompt_terms.contains(term) {
            score += 18;
            hit = true;
        }
        if summary_terms.contains(term) {
            score += 14;
            hit = true;
        }
        if model_terms.contains(term) {
            score += 4;
            hit = true;
        }
        if hit {
            matched_terms += 1;
        }
    }

    if matched_terms == 0 && score == 0 {
        return None;
    }

    if !query_terms.is_empty()
        && query_terms.iter().all(|term| {
            prompt_terms.contains(term)
                || summary_terms.contains(term)
                || model_terms.contains(term)
        })
    {
        score += 20;
    }

    Some(score)
}

async fn handle_recall(
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let query = params.get("q").cloned().unwrap_or_default();
    let query_terms = tokenize_search_text(&query);
    let limit: usize = params
        .get("limit")
        .and_then(|l| l.parse().ok())
        .unwrap_or(5usize);
    let days: i64 = params
        .get("days")
        .and_then(|d| d.parse().ok())
        .unwrap_or(30i64);

    if query.trim().is_empty() {
        let body = serde_json::json!({
            "query": query,
            "hits": [],
            "error": "missing query"
        });
        return (
            StatusCode::BAD_REQUEST,
            [("content-type", "application/json")],
            serde_json::to_string_pretty(&body).unwrap_or_default(),
        );
    }

    let query_for_search = query.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<Value, String> {
        let conn = Connection::open(db_path()).map_err(|e| format!("open sqlite: {e}"))?;
        let _ = repair_persisted_session_artifacts(&conn);
        let since_secs = now_epoch_secs().saturating_sub(days.max(0) as u64 * 86400);
        let since = epoch_to_iso8601(since_secs);
        let mut stmt = conn
            .prepare(
            "SELECT r.session_id, s.started_at, \
             COALESCE( \
                (SELECT COALESCE(NULLIF(trim(req_model.served_model), ''), NULLIF(trim(req_model.requested_model), '')) \
                 FROM requests req_model \
                 WHERE req_model.session_id = r.session_id \
                   AND req_model.provider = 'codex_responses' \
                   AND COALESCE(NULLIF(trim(req_model.served_model), ''), NULLIF(trim(req_model.requested_model), '')) IS NOT NULL \
                 ORDER BY req_model.timestamp DESC LIMIT 1), \
                (SELECT COALESCE(NULLIF(trim(turn_model.actual_model), ''), NULLIF(trim(turn_model.requested_model), '')) \
                 FROM turn_snapshots turn_model \
                 WHERE turn_model.session_id = r.session_id \
                   AND turn_model.provider = 'codex_responses' \
                   AND COALESCE(NULLIF(trim(turn_model.actual_model), ''), NULLIF(trim(turn_model.requested_model), '')) IS NOT NULL \
                 ORDER BY turn_model.turn_number DESC LIMIT 1) \
             ), \
             d.completed_at, d.outcome, \
             r.initial_prompt, r.final_response_summary, \
             COALESCE( \
                (SELECT NULLIF(trim(req.requested_model), '') FROM requests req \
                 WHERE req.session_id = r.session_id \
                   AND req.provider = 'codex_responses' \
                   AND NULLIF(trim(req.requested_model), '') IS NOT NULL \
                 ORDER BY req.timestamp DESC LIMIT 1), \
                (SELECT NULLIF(trim(turn_requested.requested_model), '') FROM turn_snapshots turn_requested \
                 WHERE turn_requested.session_id = r.session_id \
                   AND turn_requested.provider = 'codex_responses' \
                   AND NULLIF(trim(turn_requested.requested_model), '') IS NOT NULL \
                 ORDER BY turn_requested.turn_number DESC LIMIT 1) \
             ), \
             COALESCE( \
                (SELECT NULLIF(trim(req.served_model), '') FROM requests req \
                 WHERE req.session_id = r.session_id \
                   AND req.provider = 'codex_responses' \
                   AND NULLIF(trim(req.served_model), '') IS NOT NULL \
                 ORDER BY req.timestamp DESC LIMIT 1), \
                (SELECT NULLIF(trim(turn_served.actual_model), '') FROM turn_snapshots turn_served \
                 WHERE turn_served.session_id = r.session_id \
                   AND turn_served.provider = 'codex_responses' \
                   AND NULLIF(trim(turn_served.actual_model), '') IS NOT NULL \
                 ORDER BY turn_served.turn_number DESC LIMIT 1) \
             ) \
             FROM session_recall r \
             LEFT JOIN sessions s ON r.session_id = s.session_id \
             LEFT JOIN session_diagnoses d ON r.session_id = d.session_id \
	             WHERE (
	                d.completed_at >= ?1 \
	                OR (d.completed_at IS NULL AND (s.started_at >= ?1 OR s.started_at IS NULL))
	             ) \
	               AND (
	                EXISTS (
	                    SELECT 1 FROM requests req_evidence \
	                    WHERE req_evidence.session_id = r.session_id \
	                      AND req_evidence.provider = 'codex_responses'
	                ) \
	                OR EXISTS (
	                    SELECT 1 FROM turn_snapshots turn_evidence \
	                    WHERE turn_evidence.session_id = r.session_id \
	                      AND turn_evidence.provider = 'codex_responses'
	                )
	             ) \
	             ORDER BY COALESCE(d.completed_at, s.started_at, '') DESC"
            )
            .map_err(|e| format!("prepare recall query: {e}"))?;

        let mut hits: Vec<(i64, String, Value)> = stmt
            .query_map(rusqlite::params![since], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, Option<String>>(8)?,
                ))
            })
            .map_err(|e| format!("query recall rows: {e}"))?
            .filter_map(|row| match row {
                Ok(row) => Some(row),
                Err(err) => {
                    warn!(error = %err, "skipping malformed recall row");
                    None
                }
            })
            .filter_map(|(session_id, started_at, model, completed_at, outcome, initial_prompt, final_response_summary, requested_model, served_model)| {
                let initial_prompt = initial_prompt.unwrap_or_default();
                let final_response_summary = final_response_summary.unwrap_or_default();
                let model = model.unwrap_or_default();
                let score = score_recall_doc(
                    &query_for_search,
                    &query_terms,
                    &initial_prompt,
                    &final_response_summary,
                    &model,
                )?;
                Some((
                    score,
                    completed_at.clone().unwrap_or_default(),
                    serde_json::json!({
                        "session_id": session_id,
                        "started_at": started_at,
                        "completed_at": completed_at,
                        "model": if model.is_empty() { None::<String> } else { Some(model.clone()) },
                        "outcome": outcome,
                        "initial_prompt": initial_prompt,
                        "final_response_summary": final_response_summary,
                        "requested_model": requested_model,
                        "served_model": served_model,
                        "score": score,
                    }),
                ))
            })
            .collect();

        hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
        let hits = hits
            .into_iter()
            .take(limit)
            .map(|(_, _, hit)| hit)
            .collect::<Vec<_>>();

        Ok(serde_json::json!({
            "query": query_for_search,
            "hits": hits,
        }))
    })
    .await;

    match result {
        Ok(Ok(body)) => (
            StatusCode::OK,
            [("content-type", "application/json")],
            serde_json::to_string_pretty(&body).unwrap_or_default(),
        ),
        Ok(Err(err)) => {
            warn!(error = %err, "recall search failed");
            let body = serde_json::json!({
                "query": query,
                "hits": [],
                "error": "db unavailable"
            });
            (
                StatusCode::SERVICE_UNAVAILABLE,
                [("content-type", "application/json")],
                serde_json::to_string_pretty(&body).unwrap_or_default(),
            )
        }
        Err(err) => {
            warn!(error = %err, "recall search task failed");
            let body = serde_json::json!({
                "query": query,
                "hits": [],
                "error": "internal error"
            });
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                [("content-type", "application/json")],
                serde_json::to_string_pretty(&body).unwrap_or_default(),
            )
        }
    }
}

fn load_degradation_view_from_db(conn: &Connection, session_id: &str) -> Option<Value> {
    let _ = repair_persisted_session_artifacts(conn);

    let mut stmt = conn
        .prepare(
            "SELECT turn_number, input_tokens, output_tokens, ttft_ms, context_utilization, \
             context_window_tokens, requested_model, actual_model, codex_status, \
             codex_cached_input_tokens, codex_uncached_input_tokens, \
             codex_reasoning_output_tokens, codex_accounting_anomalies \
             FROM turn_snapshots \
             WHERE session_id = ?1 \
               AND provider = 'codex_responses' \
             ORDER BY turn_number",
        )
        .ok()?;

    struct TurnRow {
        turn: i64,
        input: i64,
        output: i64,
        ttft_ms: i64,
        ctx: f64,
        context_window_tokens: Option<i64>,
        requested_model: Option<String>,
        actual_model: Option<String>,
        status: Option<String>,
        cached_input_tokens: i64,
        uncached_input_tokens: i64,
        reasoning_output_tokens: i64,
        accounting_anomaly_count: u32,
    }

    let turns: Vec<TurnRow> = stmt
        .query_map(rusqlite::params![session_id], |row| {
            let input = row.get::<_, i64>(1)?.max(0);
            let requested_model = row.get::<_, Option<String>>(6)?;
            let actual_model = row.get::<_, Option<String>>(7)?;
            let context_window_tokens = row
                .get::<_, Option<i64>>(5)?
                .map(|value| value.max(0))
                .filter(|value| *value > 0);
            let inferred_context_window = || {
                infer_context_window_tokens(
                    requested_model.as_deref(),
                    actual_model.as_deref(),
                    input.max(0) as u64,
                    0,
                    0,
                ) as i64
            };
            let ctx = row
                .get::<_, Option<f64>>(4)?
                .unwrap_or_else(|| {
                    context_fill_ratio(
                        input.max(0) as u64,
                        0,
                        0,
                        context_window_tokens.unwrap_or_else(inferred_context_window) as u64,
                    )
                })
                .max(0.0);
            let anomalies = row.get::<_, Option<String>>(12)?;
            Ok(TurnRow {
                turn: row.get(0)?,
                input,
                output: row.get::<_, i64>(2)?.max(0),
                ttft_ms: row.get::<_, i64>(3)?.max(0),
                ctx,
                context_window_tokens,
                requested_model,
                actual_model,
                status: row.get::<_, Option<String>>(8)?,
                cached_input_tokens: row.get::<_, i64>(9)?.max(0),
                uncached_input_tokens: row.get::<_, i64>(10)?.max(0),
                reasoning_output_tokens: row.get::<_, i64>(11)?.max(0),
                accounting_anomaly_count: count_codex_accounting_anomalies_json(
                    anomalies.as_deref(),
                ),
            })
        })
        .ok()?
        .filter_map(|r| r.ok())
        .collect();

    if turns.is_empty() {
        return Some(serde_json::json!({
            "session_id": session_id,
            "degraded": false,
            "degradation_turn": None::<i64>,
            "total_turns": 0,
            "turns": [],
        }));
    }

    let turn_data: Vec<Value> = turns
        .iter()
        .map(|t| {
            let mut flags: Vec<&str> = Vec::new();
            let mut heuristic_signals: Vec<&str> = Vec::new();
            match t.status.as_deref() {
                Some("failed") => flags.push("codex_response_failed"),
                Some("incomplete") => flags.push("codex_response_incomplete"),
                _ => {}
            }
            if t.ctx >= 0.80 {
                heuristic_signals.push("codex_high_context_fill");
            }
            if t.output > 0 && t.reasoning_output_tokens >= 64 {
                let reasoning_share = t.reasoning_output_tokens as f64 / t.output as f64;
                if reasoning_share >= 0.50 {
                    heuristic_signals.push("codex_high_reasoning_share");
                }
            }
            if t.accounting_anomaly_count > 0 {
                flags.push("codex_accounting_anomaly");
            }
            if let (Some(requested), Some(actual)) =
                (t.requested_model.as_deref(), t.actual_model.as_deref())
            {
                if !model_matches(requested, actual) {
                    flags.push("codex_model_mismatch");
                }
            }
            serde_json::json!({
                "turn": t.turn,
                "input_tokens": t.input,
                "codex_cached_input_tokens": t.cached_input_tokens,
                "codex_uncached_input_tokens": t.uncached_input_tokens,
                "output_tokens": t.output,
                "codex_reasoning_output_tokens": t.reasoning_output_tokens,
                "codex_accounting_anomaly_count": t.accounting_anomaly_count,
                "codex_status": t.status.clone(),
                "turn_duration_ms": t.ttft_ms,
                "context_utilization": (t.ctx * 1000.0).round() / 1000.0,
                "context_window_tokens": t.context_window_tokens,
                "requested_model": t.requested_model.clone(),
                "served_model": t.actual_model.clone(),
                "flags": flags,
                "heuristic_signals": heuristic_signals,
            })
        })
        .collect();

    let degradation_turn = turn_data
        .iter()
        .filter(|turn| {
            turn.get("flags")
                .and_then(Value::as_array)
                .map(|flags| !flags.is_empty())
                .unwrap_or(false)
        })
        .filter_map(|turn| turn.get("turn").and_then(Value::as_i64))
        .min();
    let degraded = degradation_turn.is_some();

    Some(serde_json::json!({
        "session_id": session_id,
        "degraded": degraded,
        "degradation_turn": degradation_turn,
        "total_turns": turns.len(),
        "turns": turn_data,
    }))
}

async fn handle_degradation(
    axum::extract::Path(session_id): axum::extract::Path<String>,
) -> impl IntoResponse {
    let result = tokio::task::spawn_blocking(move || {
        let conn = Connection::open(db_path()).ok()?;
        load_degradation_view_from_db(&conn, &session_id)
    })
    .await
    .ok()
    .flatten();

    match result {
        Some(data) => (
            StatusCode::OK,
            [("content-type", "application/json")],
            serde_json::to_string_pretty(&data).unwrap_or_default(),
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

fn postmortem_redact_param(params: &std::collections::HashMap<String, String>) -> bool {
    !params
        .get("redact")
        .map(|value| matches!(value.to_ascii_lowercase().as_str(), "0" | "false" | "no"))
        .unwrap_or(false)
}

async fn handle_postmortem_last(
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    handle_postmortem_target(
        postmortem::PostmortemTarget::Last,
        postmortem_redact_param(&params),
    )
    .await
}

async fn handle_postmortem_session(
    axum::extract::Path(session_id): axum::extract::Path<String>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    handle_postmortem_target(
        postmortem::PostmortemTarget::Session(session_id),
        postmortem_redact_param(&params),
    )
    .await
}

async fn handle_postmortem_target(
    target: postmortem::PostmortemTarget,
    redact: bool,
) -> impl IntoResponse {
    let result = tokio::task::spawn_blocking(move || {
        let conn = Connection::open(db_path())?;
        postmortem::build_postmortem_report(&conn, target, redact)
    })
    .await;

    match result {
        Ok(Ok(report)) => (
            StatusCode::OK,
            [("content-type", "application/json")],
            serde_json::to_string_pretty(&report).unwrap_or_default(),
        )
            .into_response(),
        Ok(Err(postmortem::PostmortemBuildError::NotFound)) => {
            let body = serde_json::json!({"error": "not found"});
            (
                StatusCode::NOT_FOUND,
                [("content-type", "application/json")],
                serde_json::to_string_pretty(&body).unwrap_or_default(),
            )
                .into_response()
        }
        Ok(Err(postmortem::PostmortemBuildError::Sqlite(err))) => {
            warn!(error = %err, "postmortem report failed");
            let body = serde_json::json!({"error": "db unavailable"});
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                [("content-type", "application/json")],
                serde_json::to_string_pretty(&body).unwrap_or_default(),
            )
                .into_response()
        }
        Err(err) => {
            warn!(error = %err, "postmortem task failed");
            let body = serde_json::json!({"error": "internal error"});
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                [("content-type", "application/json")],
                serde_json::to_string_pretty(&body).unwrap_or_default(),
            )
                .into_response()
        }
    }
}

async fn http_server() {
    let app = Router::new()
        .route("/health", get(handle_health))
        .route("/metrics", get(handle_metrics))
        .route("/api/summary", get(handle_summary))
        .route("/api/guard-state", get(handle_guard_state))
        .route("/api/observations/codex", post(handle_codex_observations))
        .route("/api/recall", get(handle_recall))
        .route(
            "/api/billing-reconciliations",
            post(handle_billing_reconciliations),
        )
        .route("/api/diagnosis/:session_id", get(handle_diagnosis))
        .route("/api/degradation/:session_id", get(handle_degradation))
        .route("/api/postmortem/last", get(handle_postmortem_last))
        .route(
            "/api/postmortem/:session_id",
            get(handle_postmortem_session),
        )
        .route("/api/sessions", get(handle_sessions))
        .route("/watch", get(handle_watch));

    let addr =
        std::env::var("CODEX_BLACKBOX_HTTP_ADDR").unwrap_or_else(|_| "0.0.0.0:9090".to_string());
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|err| panic!("failed to bind HTTP server at {addr}: {err}"));
    let bound_addr = listener
        .local_addr()
        .map(|addr| addr.to_string())
        .unwrap_or_else(|_| addr.clone());
    info!("HTTP server listening on {bound_addr} (/health, /metrics, /api/summary, /api/guard-state, /api/recall, /api/billing-reconciliations, /api/sessions, /api/degradation, /api/postmortem/last, /api/postmortem/:session_id, /watch)");
    axum::serve(listener, app).await.expect("HTTP server error");
}

// ---------------------------------------------------------------------------
// Background tasks
// ---------------------------------------------------------------------------
async fn cleanup_stale_requests() {
    let ttl = Duration::from_secs(300);
    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;
        let before = REQUEST_STATE.len();
        REQUEST_STATE.retain(|_, v| v.started_at.elapsed() < ttl);
        let removed = before - REQUEST_STATE.len();
        if removed > 0 {
            info!(removed, "cleaned up stale request metadata");
        }
    }
}

async fn historical_metrics_monitor() {
    loop {
        let refreshed_at_epoch = now_epoch_secs();
        let refresh = tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path())?;
            repair_persisted_session_artifacts(&conn)?;
            query_historical_metrics(&conn, refreshed_at_epoch)
        })
        .await;

        match refresh {
            Ok(Ok(windows)) => {
                metrics::update_historical_gauges(&windows, refreshed_at_epoch);
            }
            Ok(Err(err)) => {
                warn!(error = %err, "failed to refresh historical metrics");
            }
            Err(err) => {
                warn!(error = %err, "historical metrics refresh task failed");
            }
        }

        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}

async fn data_retention_cleanup() {
    loop {
        tokio::time::sleep(Duration::from_secs(3600)).await; // hourly
        let _ = tokio::task::spawn_blocking(|| {
            let conn = match Connection::open(db_path()) {
                Ok(c) => c,
                Err(_) => return,
            };
            let now = now_epoch_secs();
            let thirty_days_ago = epoch_to_iso8601(now - 30 * 86400);
            let ninety_days_ago = epoch_to_iso8601(now - 90 * 86400);

            let _ = conn.execute(
                "DELETE FROM turn_snapshots WHERE timestamp < ?1",
                rusqlite::params![thirty_days_ago],
            );
            let _ = conn.execute(
                "DELETE FROM session_recall WHERE session_id IN \
                 (SELECT session_id FROM session_diagnoses WHERE completed_at < ?1)",
                rusqlite::params![ninety_days_ago],
            );
            let _ = conn.execute(
                "DELETE FROM session_diagnoses WHERE completed_at < ?1",
                rusqlite::params![ninety_days_ago],
            );
            info!("data retention cleanup complete");
        })
        .await;
    }
}

// ---------------------------------------------------------------------------
// Per-session monitors use periodic scanning of diagnosis::SESSIONS instead
// of a global Notify.
// ---------------------------------------------------------------------------
async fn session_inactivity_monitor() {
    let timeout_mins = env_u64("CODEX_BLACKBOX_SESSION_TIMEOUT_MINUTES", 5);
    let timeout_secs = timeout_mins * 60;
    let check_interval = Duration::from_secs(30);

    loop {
        tokio::time::sleep(check_interval).await;
        let now = Instant::now();

        // Collect expired session hashes (can't remove while iterating).
        let mut expired: Vec<(u64, String)> = Vec::new();
        for entry in diagnosis::SESSIONS.iter() {
            let idle = now.duration_since(entry.last_activity).as_secs();
            if idle > timeout_secs {
                expired.push((*entry.key(), entry.session_id.clone()));
            }
        }

        for (hash, sid) in expired {
            if let Some((_, state)) = diagnosis::SESSIONS.remove(&hash) {
                info!(session_id = %sid, timeout_mins, "session ended (inactivity timeout)");
                end_session(
                    &sid,
                    if state.session_inserted {
                        Some(state.model)
                    } else {
                        None
                    },
                    state.initial_prompt,
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_target(false)
        .json()
        .with_env_filter(filter)
        .init();

    info!("codex-blackbox-core v{}", env!("CARGO_PKG_VERSION"));
    info!("per-terminal session tracking enabled");

    // Ensure /data directory exists for SQLite.
    let _ = std::fs::create_dir_all(
        std::path::Path::new(&db_path())
            .parent()
            .unwrap_or(std::path::Path::new("/data")),
    );

    // Initialize DB writer thread.
    let _ = &*DB_TX;

    // Load pricing catalog once at startup so all pricing decisions share the
    // same resolver state until the next process restart.
    let _ = &*pricing::PRICING_CATALOG;

    // Initialize the event broadcaster.
    let _ = &*watch::BROADCASTER;
    metrics::init();

    tokio::spawn(http_server());
    tokio::spawn(cleanup_stale_requests());
    tokio::spawn(session_inactivity_monitor());
    tokio::spawn(data_retention_cleanup());
    tokio::spawn(historical_metrics_monitor());

    let addr = std::env::var("CODEX_BLACKBOX_GRPC_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:50051".to_string())
        .parse()?;
    info!(%addr, "gRPC ext_proc server starting");

    Server::builder()
        .add_service(ExternalProcessorServer::new(CodexBlackboxProcessor))
        .serve(addr)
        .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::{decision, ExtProcResponse};

    use std::fs;
    use std::sync::{LazyLock, Mutex};
    use std::time::Duration;
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    use rusqlite::Connection;
    use serde_json::Value;

    use super::{
        build_codex_finalization_outcome, build_diagnosis_response_json,
        build_fresh_diagnosis_report, build_session_summary_json, build_sessions_response_json,
        build_summary_response_json, codex_request_headers_from_ext_proc,
        codex_response_headers_from_ext_proc, codex_watch_event_is_duplicate_or_remember,
        compact_response_summary, compute_estimated_costs_for_sessions, context_fill_percent,
        context_fill_ratio, db_writer_loop, derive_display_name, diagnosis,
        drop_legacy_lifecycle_tables, ensure_codex_persistence_columns, ensure_session_columns,
        ensure_session_diagnosis_columns, epoch_to_iso8601, event_matches_session, extract_header,
        extract_headers, filter_codex_envoy_diagnosis_payload, history_contains_session_start,
        infer_context_window_tokens, load_codex_observation_snapshot,
        load_degradation_view_from_db, load_persisted_watch_replay_events,
        load_recent_codex_session_rows, load_turn_snapshots_from_db,
        looks_like_machine_recall_line, make_block_response, maybe_broadcast_postmortem_ready,
        metrics, normalize_search_text, now_epoch_secs, parse_request_body_metadata,
        persist_billing_reconciliation, persist_session_diagnosis_report,
        persisted_session_display_name, policy_block_message, postmortem, pricing,
        query_historical_metrics, query_summary, record_codex_turn_command,
        repair_persisted_session_artifacts, repair_session_diagnosis_envoy_causes,
        repair_turn_snapshot_context_windows, repo_name_from_codex_initial_prompt,
        score_recall_doc, seed_live_metric_labels_from_db, session_has_codex_evidence,
        session_timeout_secs, should_skip_chatgpt_auxiliary_request_body, table_columns,
        tokenize_search_text, BillingReconciliationInput, BillingReconciliationWriteError,
        CodexCachedInputSummary, CostAccumulator, DbCommand, GuardBlockResponse, HttpHeaders,
        ProtoHeaderValue, RequestMetadataSource, SelectedFinalizationOutcome,
        SelectedResponseAccumulator, SummaryWindowData, ESTIMATED_COST_SOURCE,
        EXTENDED_CONTEXT_WINDOW_TOKENS, SCHEMA, STANDARD_CONTEXT_WINDOW_TOKENS,
    };

    #[test]
    fn guard_block_response_includes_structured_facts_and_next_request_scope() {
        let block = decision::PolicyBlockFacts {
            rule: "session_token_budget".to_string(),
            reason: "token budget exceeded".to_string(),
            current: Some("125000 tokens".to_string()),
            limit: Some("120000 tokens".to_string()),
            session_id: Some("session_guard".to_string()),
            recovery_action: "restart narrower".to_string(),
        };
        let message = policy_block_message(&block);
        assert!(message.contains("This applies only before the next request is sent"));
        assert!(message.contains("cannot interrupt an already-streaming model response"));
        assert!(!message.to_ascii_lowercase().contains("cache rebuild"));
        assert!(!message.to_ascii_lowercase().contains("cache waste"));

        let response = make_block_response(&GuardBlockResponse {
            error_type: "policy_block",
            message,
            policy_block: Some(block),
            cooldown: None,
        });
        let body = match response.response.expect("response") {
            ExtProcResponse::ImmediateResponse(immediate) => immediate.body,
            other => panic!("expected immediate response, got {other:?}"),
        };
        let json: Value = serde_json::from_str(&body).expect("block response json");

        assert_eq!(
            json.pointer("/error/policy_block/rule")
                .and_then(|value| value.as_str()),
            Some("session_token_budget")
        );
        assert_eq!(
            json.pointer("/error/policy_block/current")
                .and_then(|value| value.as_str()),
            Some("125000 tokens")
        );
        assert_eq!(
            json.pointer("/error/policy_block/limit")
                .and_then(|value| value.as_str()),
            Some("120000 tokens")
        );
        assert_eq!(
            json.pointer("/error/policy_block/session_id")
                .and_then(|value| value.as_str()),
            Some("session_guard")
        );
        assert_eq!(
            json.pointer("/error/scope")
                .and_then(|value| value.as_str()),
            Some("next_request_only")
        );
        assert_eq!(
            json.pointer("/error/stream_interruption")
                .and_then(|value| value.as_bool()),
            Some(false)
        );
    }

    static METRICS_TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
    static ENV_TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    fn create_history_test_db() -> Connection {
        let conn = Connection::open_in_memory().expect("open sqlite");
        conn.execute_batch(
            "CREATE TABLE sessions (
                session_id TEXT PRIMARY KEY,
                started_at TEXT NOT NULL,
                ended_at TEXT,
                model TEXT,
                initial_prompt TEXT
            );
            CREATE TABLE requests (
                request_id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                timestamp TEXT NOT NULL,
                model TEXT NOT NULL,
                input_tokens INTEGER,
                output_tokens INTEGER,
                cache_read_tokens INTEGER DEFAULT 0,
                cache_creation_tokens INTEGER DEFAULT 0,
                cost_dollars REAL,
                cost_source TEXT,
                trusted_for_budget_enforcement INTEGER DEFAULT 0,
                duration_ms INTEGER,
                tool_calls TEXT,
                cache_event TEXT,
                provider TEXT,
                requested_model TEXT,
                served_model TEXT,
                codex_input_tokens INTEGER DEFAULT 0,
                codex_cached_input_tokens INTEGER DEFAULT 0,
                codex_output_tokens INTEGER DEFAULT 0
            );
            CREATE TABLE session_diagnoses (
                session_id TEXT PRIMARY KEY,
                completed_at TEXT NOT NULL,
                outcome TEXT NOT NULL,
                total_turns INTEGER,
                total_cost REAL,
                degraded INTEGER DEFAULT 0,
                degradation_turn INTEGER,
                causes_json TEXT,
                advice_json TEXT
            );
            CREATE TABLE turn_snapshots (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                turn_number INTEGER NOT NULL,
                timestamp TEXT NOT NULL,
                input_tokens INTEGER,
                cache_read_tokens INTEGER DEFAULT 0,
                cache_creation_tokens INTEGER DEFAULT 0,
                output_tokens INTEGER,
                ttft_ms INTEGER,
                tool_calls TEXT,
                tool_failures INTEGER DEFAULT 0,
                gap_from_prev_secs REAL,
                context_utilization REAL,
                context_window_tokens INTEGER,
                frustration_signals INTEGER DEFAULT 0,
                requested_model TEXT,
                actual_model TEXT,
                response_summary TEXT,
                provider TEXT,
                codex_status TEXT,
                codex_cached_input_tokens INTEGER DEFAULT 0,
                codex_reasoning_output_tokens INTEGER DEFAULT 0,
                codex_accounting_anomalies TEXT
            );
            CREATE TABLE billing_reconciliations (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                imported_at TEXT NOT NULL,
                source TEXT NOT NULL,
                billed_cost_dollars REAL NOT NULL
            );",
        )
        .expect("create test schema");
        conn
    }

    fn create_full_test_db() -> Connection {
        let conn = Connection::open_in_memory().expect("open sqlite");
        conn.execute_batch(SCHEMA).expect("create full test schema");
        conn
    }

    #[test]
    fn epoch_and_week_formatting_are_utc_stable() {
        assert_eq!(epoch_to_iso8601(0), "1970-01-01T00:00:00Z");
        assert_eq!(epoch_to_iso8601(1_582_934_400), "2020-02-29T00:00:00Z");
        assert_eq!(super::start_of_week_iso_at(0), "1970-01-01T00:00:00Z");
        assert_eq!(
            super::start_of_week_iso_at(1_619_827_200),
            "2021-04-26T00:00:00Z"
        );
    }

    #[test]
    fn codex_request_metadata_maps_fixture_for_hot_path() {
        let body = include_bytes!("../../test/fixtures/openai_responses_minimal_text_request.json");
        let metadata = parse_request_body_metadata(
            body,
            &super::codex_request::CodexRequestHeaders::default(),
        )
        .expect("parse codex fixture");

        assert_eq!(metadata.source, RequestMetadataSource::CodexResponses);
        assert_eq!(metadata.model, "gpt-codex-fixture");
        assert_eq!(metadata.message_count, 1);
        assert!(!metadata.has_tools);
        assert!(metadata.system_prompt_length > 0);
        assert_eq!(metadata.estimated_input_tokens, body.len() / 4);
        assert_eq!(metadata.session_id, "codex-session-fixture-001");
        assert_eq!(
            metadata.working_dir,
            "/Users/pradeepsingh/code/codex-blackbox"
        );
        assert_eq!(
            metadata.user_prompt_excerpt,
            "Summarize the current repository status."
        );
        assert_ne!(metadata.session_hash, 0);
    }

    #[test]
    fn codex_request_headers_are_captured_for_hot_path_precedence() {
        let headers = make_http_headers(&[
            ("session-id", "header-session-003"),
            ("x-client-request-id", "client-request-003"),
        ]);

        let parsed = codex_request_headers_from_ext_proc(&headers);

        assert_eq!(parsed.session_id.as_deref(), Some("header-session-003"));
        assert_eq!(
            parsed.client_request_id.as_deref(),
            Some("client-request-003")
        );
    }

    #[test]
    fn codex_session_id_header_wins_in_hot_path_metadata() {
        let body = include_bytes!("../../test/fixtures/openai_responses_minimal_text_request.json");
        let headers = super::codex_request::CodexRequestHeaders {
            session_id: Some("header-session-004".to_string()),
            client_request_id: Some("client-request-004".to_string()),
        };

        let metadata = parse_request_body_metadata(body, &headers).expect("parse codex fixture");

        assert_eq!(metadata.source, RequestMetadataSource::CodexResponses);
        assert_eq!(metadata.session_id, "header-session-004");
        assert_eq!(
            metadata.session_hash,
            super::codex_request::fallback_session_hash("", "header-session-004")
        );
    }

    #[test]
    fn codex_client_request_id_fallback_maps_to_hot_path_session() {
        let body = include_bytes!("../../test/fixtures/openai_responses_minimal_text_request.json");
        let headers = super::codex_request::CodexRequestHeaders {
            session_id: None,
            client_request_id: Some("client-request-005".to_string()),
        };

        let metadata = parse_request_body_metadata(body, &headers).expect("parse codex fixture");

        assert_eq!(metadata.source, RequestMetadataSource::CodexResponses);
        assert_eq!(metadata.session_id, "client-request-005");
        assert_eq!(
            metadata.session_hash,
            super::codex_request::fallback_session_hash("", "client-request-005")
        );
    }

    #[test]
    fn codex_fallback_id_splits_distinct_first_inputs_in_hot_path_metadata() {
        let first = br#"{
          "model": "gpt-codex-fixture",
          "input": "first codex task",
          "metadata": { "cwd": "/tmp/codex-blackbox-hot-path" }
        }"#;
        let second = br#"{
          "model": "gpt-codex-fixture",
          "input": "second codex task",
          "metadata": { "cwd": "/tmp/codex-blackbox-hot-path" }
        }"#;
        let headers = super::codex_request::CodexRequestHeaders::default();

        let first = parse_request_body_metadata(first, &headers).expect("parse first request");
        let second = parse_request_body_metadata(second, &headers).expect("parse second request");

        assert_eq!(first.source, RequestMetadataSource::CodexResponses);
        assert_eq!(second.source, RequestMetadataSource::CodexResponses);
        assert_ne!(first.session_id, second.session_id);
        assert_ne!(first.session_hash, second.session_hash);
    }

    #[test]
    fn response_parser_selection_uses_codex_for_codex_request_metadata() {
        let body = include_bytes!("../../test/fixtures/openai_responses_minimal_text_request.json");
        let metadata = parse_request_body_metadata(
            body,
            &super::codex_request::CodexRequestHeaders::default(),
        )
        .expect("parse codex fixture");

        let accumulator = SelectedResponseAccumulator::for_request_source(metadata.source);

        assert!(matches!(
            accumulator,
            SelectedResponseAccumulator::CodexResponses { .. }
        ));
    }

    #[test]
    fn response_headers_feed_codex_accumulator_summary_state() {
        let headers = make_http_headers(&[
            (":status", "200"),
            ("openai-model", "gpt-codex-fixture-served-from-header"),
            ("x-openai-model", "gpt-codex-fixture-served-from-x-header"),
        ]);
        let parsed_headers = codex_response_headers_from_ext_proc(&headers);
        let mut accumulator =
            SelectedResponseAccumulator::for_request_source(RequestMetadataSource::CodexResponses);

        accumulator.apply_response_headers(&parsed_headers);

        match accumulator {
            SelectedResponseAccumulator::CodexResponses {
                accumulator,
                is_sse,
            } => {
                let summary = accumulator.summary();
                assert!(is_sse);
                assert_eq!(summary.http_status, Some(200));
                assert_eq!(
                    summary.served_model.as_deref(),
                    Some("gpt-codex-fixture-served-from-header")
                );
            }
        }
    }

    #[test]
    fn selected_codex_accumulator_handles_split_response_chunks() {
        let stream = include_str!("../../test/fixtures/openai_responses_text_stream.sse");
        let headers = make_http_headers(&[
            (":status", "200"),
            ("openai-model", "gpt-codex-fixture-served-from-header"),
        ]);
        let parsed_headers = codex_response_headers_from_ext_proc(&headers);
        let mut accumulator =
            SelectedResponseAccumulator::for_request_source(RequestMetadataSource::CodexResponses);

        accumulator.apply_response_headers(&parsed_headers);
        for chunk in stream.as_bytes().chunks(7) {
            accumulator
                .process_chunk(chunk)
                .expect("process split chunk");
        }

        match &mut accumulator {
            SelectedResponseAccumulator::CodexResponses { accumulator, .. } => {
                accumulator.finish().expect("finish selected accumulator");
                let summary = accumulator.summary();
                assert_eq!(
                    summary.status,
                    super::codex_response::CodexResponseStatus::Completed
                );
                assert_eq!(
                    summary.output_text,
                    "Workspace packages: codex-blackbox-core and codex-blackbox-cli."
                );
                assert_eq!(
                    summary.served_model.as_deref(),
                    Some("gpt-codex-fixture-served-from-header")
                );
            }
        }
    }

    #[test]
    fn codex_finalization_builds_accounting_and_watch_without_cache_event() {
        let request = parse_codex_fixture_request("phase-4b-session-001");
        let response = accumulate_codex_fixture_response(
            include_str!("../../test/fixtures/openai_responses_text_stream.sse"),
            Some("gpt-codex-fixture"),
        );

        let outcome = build_codex_finalization_outcome(
            "req-phase-4b-001",
            &request,
            &response,
            Duration::from_millis(42),
            STANDARD_CONTEXT_WINDOW_TOKENS,
        );

        assert_eq!(
            outcome.accounting.identity.session_id,
            "phase-4b-session-001"
        );
        assert_eq!(outcome.accounting.requested_model, "gpt-codex-fixture");
        assert_eq!(outcome.accounting.total_tokens, 1376);
        assert!(outcome
            .watch_events
            .iter()
            .any(|event| matches!(event, super::watch::WatchEvent::SessionStart { session_id, initial_prompt: Some(prompt), .. }
                if session_id == "phase-4b-session-001"
                    && prompt == "Summarize the current repository status.")));
        assert!(outcome.watch_events.iter().any(
            |event| matches!(event, super::watch::WatchEvent::ContextStatus { session_id, .. }
                if session_id == "phase-4b-session-001")
        ));
        assert!(outcome.watch_events.iter().any(|event| {
            matches!(
                event,
                super::watch::WatchEvent::CodexTurnSummary {
                    session_id,
                    status,
                    requested_model,
                    served_model: Some(served_model),
                    cached_input_tokens,
                    reasoning_output_tokens,
                    ..
                } if session_id == "phase-4b-session-001"
                    && status == "completed"
                    && requested_model == "gpt-codex-fixture"
                    && served_model == "gpt-codex-fixture"
                    && *cached_input_tokens == 512
                    && *reasoning_output_tokens == 32
            )
        }));
        let serialized = serde_json::to_string(&outcome.watch_events).expect("watch json");
        assert!(!serialized.contains("cache_event"));
        assert!(!serialized.contains("cache_warning"));
    }

    #[test]
    fn codex_finalization_uses_input_only_for_context_and_totals() {
        let request = parse_codex_fixture_request("phase-4b-session-002");
        let response = super::codex_response::CodexResponseSummary {
            status: super::codex_response::CodexResponseStatus::Completed,
            served_model: Some("gpt-codex-fixture".to_string()),
            usage: super::codex_response::CodexUsage {
                input_tokens: 100,
                cached_input_tokens: 80,
                uncached_input_tokens: 20,
                output_tokens: 10,
                reasoning_output_tokens: 4,
                total_tokens: 110,
            },
            ..Default::default()
        };

        let outcome = build_codex_finalization_outcome(
            "req-phase-4b-002",
            &request,
            &response,
            Duration::from_millis(10),
            200,
        );

        assert_eq!(outcome.accounting.total_tokens, 110);
        assert_eq!(outcome.context_fill_percent, 50.0);
        assert_ne!(outcome.context_fill_percent, 90.0);
    }

    #[test]
    fn codex_finalization_derives_model_fallback_event() {
        let request = parse_codex_fixture_request("phase-4b-session-003");
        let response = accumulate_codex_fixture_response(
            include_str!("../../test/fixtures/openai_responses_text_stream.sse"),
            Some("gpt-codex-fixture-served"),
        );

        let outcome = build_codex_finalization_outcome(
            "req-phase-4b-003",
            &request,
            &response,
            Duration::from_millis(10),
            STANDARD_CONTEXT_WINDOW_TOKENS,
        );

        assert!(outcome.watch_events.iter().any(|event| {
            matches!(event, super::watch::WatchEvent::ModelFallback { session_id, requested, actual }
                if session_id == "phase-4b-session-003"
                    && requested == "gpt-codex-fixture"
                    && actual == "gpt-codex-fixture-served")
        }));
    }

    #[test]
    fn codex_finalization_represents_failed_and_incomplete_statuses() {
        let request = parse_codex_fixture_request("phase-4b-session-004");
        let failed = accumulate_codex_fixture_response(
            include_str!("../../test/fixtures/openai_responses_failed_stream.sse"),
            None,
        );
        let incomplete = accumulate_codex_fixture_response(
            include_str!("../../test/fixtures/openai_responses_incomplete_stream.sse"),
            None,
        );

        let failed_outcome = build_codex_finalization_outcome(
            "req-phase-4b-004a",
            &request,
            &failed,
            Duration::from_millis(10),
            STANDARD_CONTEXT_WINDOW_TOKENS,
        );
        let incomplete_outcome = build_codex_finalization_outcome(
            "req-phase-4b-004b",
            &request,
            &incomplete,
            Duration::from_millis(10),
            STANDARD_CONTEXT_WINDOW_TOKENS,
        );

        assert_eq!(
            failed_outcome.accounting.status,
            super::codex_accounting::CodexTurnStatus::Failed
        );
        assert_eq!(
            incomplete_outcome.accounting.status,
            super::codex_accounting::CodexTurnStatus::Incomplete
        );
        assert!(failed_outcome.watch_events.iter().any(|event| {
            matches!(event, super::watch::WatchEvent::CodexTurnSummary { status, .. }
                if status == "failed")
        }));
        assert!(incomplete_outcome.watch_events.iter().any(|event| {
            matches!(event, super::watch::WatchEvent::CodexTurnSummary { status, .. }
                if status == "incomplete")
        }));
    }

    #[test]
    fn selected_codex_finalization_returns_codex_outcome() {
        let request = parse_codex_fixture_request("phase-4b-session-005");
        let mut accumulator =
            SelectedResponseAccumulator::for_request_source(RequestMetadataSource::CodexResponses);
        accumulator.apply_response_headers(&super::codex_response::CodexResponseHeaders {
            http_status: Some(200),
            served_model: Some("gpt-codex-fixture".to_string()),
        });
        accumulator
            .process_chunk(include_bytes!(
                "../../test/fixtures/openai_responses_text_stream.sse"
            ))
            .expect("process codex fixture");

        let outcome = super::finalize_selected_response(
            &mut accumulator,
            Some(&request),
            "req-phase-4b-005",
            "gpt-codex-fixture",
            &Instant::now(),
            STANDARD_CONTEXT_WINDOW_TOKENS,
        );

        let SelectedFinalizationOutcome::Codex(outcome) =
            outcome.expect("codex finalization outcome");
        assert_eq!(
            outcome.accounting.identity.session_id,
            "phase-4b-session-005"
        );
        assert_eq!(
            outcome.accounting.status,
            super::codex_accounting::CodexTurnStatus::Completed
        );
        {
            let budget_state = super::SESSION_BUDGETS
                .get("phase-4b-session-005")
                .expect("session budget state");
            assert_eq!(budget_state.total_tokens, outcome.accounting.total_tokens);
            assert_eq!(budget_state.request_count, 1);
        }

        let _ = super::SESSION_BUDGETS.remove("phase-4b-session-005");
        let _ = diagnosis::SESSIONS.remove(&super::codex_request::fallback_session_hash(
            "",
            "phase-4b-session-005",
        ));
    }

    #[test]
    fn session_budget_guard_blocks_next_request_from_runtime_state() {
        let _guard = ENV_TEST_LOCK.lock().unwrap();
        let session_id = "phase-4b-session-budget";
        std::env::set_var("CODEX_BLACKBOX_SESSION_BUDGET_TOKENS", "1000");
        std::env::remove_var("CODEX_BLACKBOX_SESSION_BUDGET_DOLLARS");
        std::env::remove_var("CODEX_BLACKBOX_GUARD_POLICY_FILE");
        let _ = super::SESSION_BUDGETS.remove(session_id);

        let request = parse_codex_fixture_request(session_id);
        let response = accumulate_codex_fixture_response(
            include_str!("../../test/fixtures/openai_responses_text_stream.sse"),
            Some("gpt-codex-fixture"),
        );
        let outcome = build_codex_finalization_outcome(
            "req-phase-4b-budget",
            &request,
            &response,
            Duration::from_millis(10),
            STANDARD_CONTEXT_WINDOW_TOKENS,
        );

        super::record_codex_runtime_counters(&outcome.accounting);
        let block = super::check_session_budget(Some(session_id)).expect("budget block");

        assert_eq!(block.error_type, "policy_block");
        let policy_block = block.policy_block.expect("policy facts");
        assert_eq!(policy_block.rule, "session_token_budget");
        assert_eq!(
            policy_block.session_id.as_deref(),
            Some("phase-4b-session-budget")
        );
        assert_eq!(policy_block.current.as_deref(), Some("1376 tokens"));
        assert_eq!(policy_block.limit.as_deref(), Some("1000 tokens"));
        assert!(block.cooldown.is_none());

        let _ = super::SESSION_BUDGETS.remove(session_id);
        std::env::remove_var("CODEX_BLACKBOX_SESSION_BUDGET_TOKENS");
    }

    #[test]
    fn current_guard_state_reports_active_cooldown_without_session_surface() {
        let _guard = ENV_TEST_LOCK.lock().unwrap();
        {
            let mut runtime = super::RUNTIME_STATE.lock().unwrap();
            runtime.circuit_open_until = Some(Instant::now() + Duration::from_secs(30));
        }

        let cooldown = super::current_cooldown_facts().expect("cooldown");
        assert_eq!(cooldown.reason, "upstream errors");
        assert!(cooldown.retry_after_seconds.unwrap_or_default() <= 30);
        let event = super::cooldown_watch_event(&cooldown);
        let json = serde_json::to_value(event).expect("cooldown event");
        assert_eq!(
            json.get("type").and_then(|value| value.as_str()),
            Some("cooldown")
        );
        assert!(json.get("session_id").is_none());

        {
            let mut runtime = super::RUNTIME_STATE.lock().unwrap();
            runtime.circuit_open_until = None;
        }
    }

    #[test]
    fn codex_persistence_migration_adds_codex_native_columns_to_legacy_tables() {
        let conn = create_history_test_db();

        ensure_codex_persistence_columns(&conn).expect("migrate codex columns");

        let session_columns = table_columns(&conn, "sessions").expect("session columns");
        let request_columns = table_columns(&conn, "requests").expect("request columns");
        let turn_columns = table_columns(&conn, "turn_snapshots").expect("turn columns");

        assert!(session_columns.contains("total_codex_cached_input_tokens"));
        assert!(session_columns.contains("total_codex_reasoning_output_tokens"));
        assert!(request_columns.contains("codex_cached_input_tokens"));
        assert!(request_columns.contains("codex_uncached_input_tokens"));
        assert!(request_columns.contains("codex_reasoning_output_tokens"));
        assert!(request_columns.contains("codex_total_tokens"));
        assert!(request_columns.contains("codex_failure_detail"));
        assert!(request_columns.contains("codex_incomplete_detail"));
        assert!(turn_columns.contains("request_id"));
        assert!(turn_columns.contains("codex_status"));
        assert!(turn_columns.contains("codex_cached_input_tokens"));
        assert!(turn_columns.contains("codex_reasoning_output_tokens"));
        assert!(turn_columns.contains("codex_failure_detail"));
        assert!(turn_columns.contains("codex_incomplete_detail"));
    }

    #[test]
    fn codex_persistence_completed_fake_turn_writes_immutable_request_and_turn() {
        let path = unique_test_db_path("codex-persist-completed");
        let request = parse_codex_fixture_request("phase-4c-session-001");
        let response = accumulate_codex_fixture_response(
            include_str!("../../test/fixtures/openai_responses_text_stream.sse"),
            Some("gpt-codex-fixture-served"),
        );
        let outcome = build_codex_finalization_outcome(
            "req-phase-4c-001",
            &request,
            &response,
            Duration::from_millis(42),
            STANDARD_CONTEXT_WINDOW_TOKENS,
        );
        let timestamp = "2026-04-30T12:00:00Z".to_string();

        run_db_writer_commands(
            &path,
            vec![
                record_codex_turn_command(&outcome, timestamp.clone()),
                record_codex_turn_command(&outcome, timestamp),
            ],
        );

        let conn = Connection::open(&path).expect("open persisted db");
        let request_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM requests", [], |row| row.get(0))
            .expect("request count");
        let turn_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM turn_snapshots", [], |row| row.get(0))
            .expect("turn count");
        assert_eq!(request_count, 1);
        assert_eq!(turn_count, 1);
        let (session_display_name, session_initial_prompt): (String, String) = conn
            .query_row(
                "SELECT display_name, initial_prompt FROM sessions WHERE session_id = 'phase-4c-session-001'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("load codex session");
        assert_eq!(session_display_name, "codex-blackbox");
        assert_eq!(
            session_initial_prompt,
            "Summarize the current repository status."
        );

        let (
            provider,
            model,
            requested_model,
            served_model,
            status,
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_creation_tokens,
            codex_cached_input_tokens,
            codex_uncached_input_tokens,
            codex_reasoning_output_tokens,
            codex_total_tokens,
            cost_dollars,
            cost_source,
            trusted_for_budget_enforcement,
            response_id,
            prompt_excerpt,
            tool_calls_json,
            anomalies_json,
        ): (
            String,
            String,
            String,
            String,
            String,
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
            f64,
            String,
            i64,
            String,
            String,
            String,
            String,
        ) = conn
            .query_row(
                "SELECT provider, model, requested_model, served_model, codex_status,
                        input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens,
                        codex_cached_input_tokens, codex_uncached_input_tokens,
                        codex_reasoning_output_tokens, codex_total_tokens, cost_dollars,
                        cost_source, trusted_for_budget_enforcement, codex_response_id,
                        codex_prompt_excerpt, codex_tool_calls, codex_accounting_anomalies
                 FROM requests WHERE request_id = 'req-phase-4c-001'",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                        row.get(8)?,
                        row.get(9)?,
                        row.get(10)?,
                        row.get(11)?,
                        row.get(12)?,
                        row.get(13)?,
                        row.get(14)?,
                        row.get(15)?,
                        row.get(16)?,
                        row.get(17)?,
                        row.get(18)?,
                        row.get(19)?,
                    ))
                },
            )
            .expect("load codex request");

        assert_eq!(provider, "codex_responses");
        assert_eq!(model, "gpt-codex-fixture-served");
        assert_eq!(requested_model, "gpt-codex-fixture");
        assert_eq!(served_model, "gpt-codex-fixture-served");
        assert_eq!(status, "completed");
        assert_eq!(input_tokens, 1280);
        assert_eq!(output_tokens, 96);
        assert_eq!(cache_read_tokens, 0);
        assert_eq!(cache_creation_tokens, 0);
        assert_eq!(codex_cached_input_tokens, 512);
        assert_eq!(codex_uncached_input_tokens, 768);
        assert_eq!(codex_reasoning_output_tokens, 32);
        assert_eq!(codex_total_tokens, 1376);
        assert_eq!(cost_dollars, 0.0);
        assert!(cost_source.starts_with("codex_unpriced:unknown_model:"));
        assert_eq!(trusted_for_budget_enforcement, 0);
        assert_eq!(response_id, "resp_fixture_text_001");
        assert_eq!(prompt_excerpt, "Summarize the current repository status.");
        assert_eq!(tool_calls_json, "[]");
        assert_eq!(anomalies_json, "[]");

        let (
            turn_request_id,
            turn_provider,
            turn_status,
            turn_cache_read_tokens,
            turn_cached_input_tokens,
            turn_reasoning_tokens,
            turn_total_tokens,
            turn_context_utilization,
            turn_context_window_tokens,
            turn_response_summary,
        ): (String, String, String, i64, i64, i64, i64, f64, i64, String) = conn
            .query_row(
                "SELECT request_id, provider, codex_status, cache_read_tokens,
                        codex_cached_input_tokens, codex_reasoning_output_tokens,
                        codex_total_tokens, context_utilization, context_window_tokens,
                        response_summary
                 FROM turn_snapshots WHERE session_id = 'phase-4c-session-001'",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                        row.get(8)?,
                        row.get(9)?,
                    ))
                },
            )
            .expect("load codex turn");

        assert_eq!(turn_request_id, "req-phase-4c-001");
        assert_eq!(turn_provider, "codex_responses");
        assert_eq!(turn_status, "completed");
        assert_eq!(turn_cache_read_tokens, 0);
        assert_eq!(turn_cached_input_tokens, 512);
        assert_eq!(turn_reasoning_tokens, 32);
        assert_eq!(turn_total_tokens, 1376);
        assert_eq!(
            turn_context_window_tokens,
            STANDARD_CONTEXT_WINDOW_TOKENS as i64
        );
        assert_eq!(
            turn_response_summary,
            "Workspace packages: codex-blackbox-core and codex-blackbox-cli."
        );
        assert!(turn_context_utilization < 0.01);

        let (
            request_count,
            total_input_tokens,
            total_output_tokens,
            total_cache_read_tokens,
            total_codex_cached_input_tokens,
            total_codex_uncached_input_tokens,
            total_codex_reasoning_output_tokens,
            total_codex_tokens,
        ): (i64, i64, i64, i64, i64, i64, i64, i64) = conn
            .query_row(
                "SELECT request_count, total_input_tokens, total_output_tokens,
                        total_cache_read_tokens, total_codex_cached_input_tokens,
                        total_codex_uncached_input_tokens,
                        total_codex_reasoning_output_tokens, total_codex_tokens
                 FROM sessions WHERE session_id = 'phase-4c-session-001'",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                    ))
                },
            )
            .expect("load codex session totals");

        assert_eq!(request_count, 1);
        assert_eq!(total_input_tokens, 1280);
        assert_eq!(total_output_tokens, 96);
        assert_eq!(total_cache_read_tokens, 0);
        assert_eq!(total_codex_cached_input_tokens, 512);
        assert_eq!(total_codex_uncached_input_tokens, 768);
        assert_eq!(total_codex_reasoning_output_tokens, 32);
        assert_eq!(total_codex_tokens, 1376);

        drop(conn);
        cleanup_test_db(&path);
    }

    #[test]
    fn codex_persistence_parallel_same_repo_prompts_create_distinct_sessions() {
        let path = unique_test_db_path("codex-persist-parallel");
        let first = br#"{
          "model": "gpt-codex-fixture",
          "input": "inspect package metadata",
          "metadata": { "cwd": "/tmp/codex-blackbox-parallel" }
        }"#;
        let second = br#"{
          "model": "gpt-codex-fixture",
          "input": "summarize repository docs",
          "metadata": { "cwd": "/tmp/codex-blackbox-parallel" }
        }"#;
        let first_request = super::codex_request::parse_codex_responses_request(
            first,
            super::codex_request::CodexRequestHeaders::default(),
        )
        .expect("parse first codex request");
        let second_request = super::codex_request::parse_codex_responses_request(
            second,
            super::codex_request::CodexRequestHeaders::default(),
        )
        .expect("parse second codex request");
        assert_ne!(first_request.session.id, second_request.session.id);

        let response = super::codex_response::CodexResponseSummary {
            status: super::codex_response::CodexResponseStatus::Completed,
            served_model: Some("gpt-codex-fixture".to_string()),
            usage: super::codex_response::CodexUsage {
                input_tokens: 20,
                cached_input_tokens: 8,
                uncached_input_tokens: 12,
                output_tokens: 4,
                reasoning_output_tokens: 1,
                total_tokens: 24,
            },
            ..Default::default()
        };
        let first_outcome = build_codex_finalization_outcome(
            "req-phase-4c-parallel-001",
            &first_request,
            &response,
            Duration::from_millis(10),
            STANDARD_CONTEXT_WINDOW_TOKENS,
        );
        let second_outcome = build_codex_finalization_outcome(
            "req-phase-4c-parallel-002",
            &second_request,
            &response,
            Duration::from_millis(12),
            STANDARD_CONTEXT_WINDOW_TOKENS,
        );

        run_db_writer_commands(
            &path,
            vec![
                record_codex_turn_command(&first_outcome, "2026-04-30T12:00:01Z".to_string()),
                record_codex_turn_command(&second_outcome, "2026-04-30T12:00:02Z".to_string()),
            ],
        );

        let conn = Connection::open(&path).expect("open persisted db");
        let sessions: i64 = conn
            .query_row(
                "SELECT COUNT(DISTINCT session_id) FROM sessions",
                [],
                |row| row.get(0),
            )
            .expect("count sessions");
        let turns: i64 = conn
            .query_row("SELECT COUNT(*) FROM turn_snapshots", [], |row| row.get(0))
            .expect("count turns");
        let prompts = conn
            .prepare("SELECT initial_prompt FROM sessions ORDER BY initial_prompt")
            .expect("prepare prompt query")
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query prompts")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect prompts");

        assert_eq!(sessions, 2);
        assert_eq!(turns, 2);
        assert_eq!(
            prompts,
            vec![
                "inspect package metadata".to_string(),
                "summarize repository docs".to_string()
            ]
        );

        drop(conn);
        cleanup_test_db(&path);
    }

    #[test]
    fn codex_persistence_saturates_anomalous_cached_input_and_records_anomaly() {
        let path = unique_test_db_path("codex-persist-anomaly");
        let request = parse_codex_fixture_request("phase-4c-session-anomaly");
        let response = super::codex_response::CodexResponseSummary {
            status: super::codex_response::CodexResponseStatus::Completed,
            served_model: Some("gpt-codex-fixture".to_string()),
            usage: super::codex_response::CodexUsage {
                input_tokens: 10,
                cached_input_tokens: 30,
                uncached_input_tokens: 0,
                output_tokens: 5,
                reasoning_output_tokens: 2,
                total_tokens: 15,
            },
            ..Default::default()
        };
        let outcome = build_codex_finalization_outcome(
            "req-phase-4c-anomaly",
            &request,
            &response,
            Duration::from_millis(5),
            STANDARD_CONTEXT_WINDOW_TOKENS,
        );

        run_db_writer_commands(
            &path,
            vec![record_codex_turn_command(
                &outcome,
                "2026-04-30T12:00:03Z".to_string(),
            )],
        );

        let conn = Connection::open(&path).expect("open persisted db");
        let (uncached_input_tokens, total_tokens, anomalies_json): (i64, i64, String) = conn
            .query_row(
                "SELECT codex_uncached_input_tokens, codex_total_tokens,
                        codex_accounting_anomalies
                 FROM requests WHERE request_id = 'req-phase-4c-anomaly'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("load anomaly request");
        let anomalies: serde_json::Value =
            serde_json::from_str(&anomalies_json).expect("parse anomalies json");

        assert_eq!(uncached_input_tokens, 0);
        assert_eq!(total_tokens, 15);
        assert!(anomalies.to_string().contains("cached_input_exceeds_input"));

        drop(conn);
        cleanup_test_db(&path);
    }

    #[test]
    fn codex_persistence_failed_and_incomplete_status_details_are_stored() {
        let path = unique_test_db_path("codex-persist-status");
        let failed_request = parse_codex_fixture_request("phase-4c-session-failed");
        let incomplete_request = parse_codex_fixture_request("phase-4c-session-incomplete");
        let failed = accumulate_codex_fixture_response(
            include_str!("../../test/fixtures/openai_responses_failed_stream.sse"),
            None,
        );
        let incomplete = accumulate_codex_fixture_response(
            include_str!("../../test/fixtures/openai_responses_incomplete_stream.sse"),
            None,
        );
        let failed_outcome = build_codex_finalization_outcome(
            "req-phase-4c-failed",
            &failed_request,
            &failed,
            Duration::from_millis(10),
            STANDARD_CONTEXT_WINDOW_TOKENS,
        );
        let incomplete_outcome = build_codex_finalization_outcome(
            "req-phase-4c-incomplete",
            &incomplete_request,
            &incomplete,
            Duration::from_millis(10),
            STANDARD_CONTEXT_WINDOW_TOKENS,
        );

        run_db_writer_commands(
            &path,
            vec![
                record_codex_turn_command(&failed_outcome, "2026-04-30T12:00:04Z".to_string()),
                record_codex_turn_command(&incomplete_outcome, "2026-04-30T12:00:05Z".to_string()),
            ],
        );

        let conn = Connection::open(&path).expect("open persisted db");
        let details = conn
            .prepare(
                "SELECT codex_status, codex_failure_detail, codex_incomplete_detail \
                 FROM requests ORDER BY request_id",
            )
            .expect("prepare status query")
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })
            .expect("query statuses")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect statuses");
        let turn_details = conn
            .prepare(
                "SELECT codex_status, codex_failure_detail, codex_incomplete_detail \
                 FROM turn_snapshots ORDER BY request_id",
            )
            .expect("prepare turn status query")
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })
            .expect("query turn statuses")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect turn statuses");
        let turns: i64 = conn
            .query_row("SELECT COUNT(*) FROM turn_snapshots", [], |row| row.get(0))
            .expect("count turns");

        assert_eq!(
            details,
            vec![
                (
                    "failed".to_string(),
                    Some("Fixture failure for Codex Blackbox contract tests.".to_string()),
                    None
                ),
                (
                    "incomplete".to_string(),
                    None,
                    Some("max_output_tokens".to_string())
                )
            ]
        );
        assert_eq!(turn_details, details);
        assert_eq!(turns, 2);

        drop(conn);
        cleanup_test_db(&path);
    }

    #[test]
    fn postmortem_report_exposes_failed_and_incomplete_details_with_redaction() {
        let path = unique_test_db_path("codex-postmortem-details");
        let request = parse_codex_fixture_request("codex-postmortem-session");
        let failed = accumulate_codex_fixture_response(
            include_str!("../../test/fixtures/openai_responses_failed_stream.sse"),
            None,
        );
        let incomplete = accumulate_codex_fixture_response(
            include_str!("../../test/fixtures/openai_responses_incomplete_stream.sse"),
            None,
        );
        let failed_outcome = build_codex_finalization_outcome(
            "req-postmortem-failed",
            &request,
            &failed,
            Duration::from_millis(10),
            STANDARD_CONTEXT_WINDOW_TOKENS,
        );
        let incomplete_outcome = build_codex_finalization_outcome(
            "req-postmortem-incomplete",
            &request,
            &incomplete,
            Duration::from_millis(12),
            STANDARD_CONTEXT_WINDOW_TOKENS,
        );

        run_db_writer_commands(
            &path,
            vec![
                record_codex_turn_command(&failed_outcome, "2026-04-30T12:00:04Z".to_string()),
                record_codex_turn_command(&incomplete_outcome, "2026-04-30T12:00:05Z".to_string()),
            ],
        );

        let conn = Connection::open(&path).expect("open persisted db");
        let report = postmortem::build_postmortem_report(
            &conn,
            postmortem::PostmortemTarget::Session("codex-postmortem-session".to_string()),
            true,
        )
        .expect("postmortem report");
        let body = report.to_string();

        assert_eq!(
            report
                .pointer("/signals/response_statuses/failed")
                .and_then(Value::as_u64),
            Some(1)
        );
        assert_eq!(
            report
                .pointer("/signals/response_statuses/incomplete")
                .and_then(Value::as_u64),
            Some(1)
        );
        assert!(body.contains("Fixture failure for Codex Blackbox contract tests."));
        assert!(body.contains("max_output_tokens"));
        assert_eq!(
            report
                .pointer("/summary/initial_prompt_excerpt")
                .and_then(Value::as_str),
            Some("[redacted prompt excerpt]")
        );
        assert_eq!(
            report.get("evidence_origin").and_then(Value::as_str),
            Some("local_fake_fixture_contract")
        );

        drop(conn);
        cleanup_test_db(&path);
    }

    #[test]
    fn postmortem_last_is_provider_scoped_to_codex_responses() {
        let conn = create_full_test_db();
        insert_session(
            &conn,
            "legacy-session",
            "2026-04-30T12:10:00Z",
            Some("2026-04-30T12:10:00Z"),
            "gpt-5.5",
            Some("legacy"),
        );
        insert_request(
            &conn,
            "legacy-request",
            "legacy-session",
            "2026-04-30T12:10:00Z",
            "gpt-5.5",
            100,
            10,
            0,
            0,
        );
        insert_session(
            &conn,
            "codex-session",
            "2026-04-30T12:00:00Z",
            Some("2026-04-30T12:00:00Z"),
            "gpt-codex-fixture",
            Some("codex"),
        );
        insert_codex_degradation_turn_snapshot(
            &conn,
            "codex-session",
            1,
            "2026-04-30T12:00:00Z",
            "completed",
            1000,
            500,
            100,
            20,
            0.10,
            "[]",
        );

        let report =
            postmortem::build_postmortem_report(&conn, postmortem::PostmortemTarget::Last, true)
                .expect("postmortem report");

        assert_eq!(
            report.get("session_id").and_then(Value::as_str),
            Some("codex-session")
        );
    }

    #[test]
    fn postmortem_report_omits_unsupported_surfaces() {
        let path = unique_test_db_path("codex-postmortem-boundary");
        let request = parse_codex_fixture_request("codex-postmortem-boundary");
        let response = accumulate_codex_fixture_response(
            include_str!("../../test/fixtures/openai_responses_tool_stream.sse"),
            Some("gpt-codex-fixture"),
        );
        let outcome = build_codex_finalization_outcome(
            "req-postmortem-boundary",
            &request,
            &response,
            Duration::from_millis(25),
            STANDARD_CONTEXT_WINDOW_TOKENS,
        );

        run_db_writer_commands(
            &path,
            vec![record_codex_turn_command(
                &outcome,
                "2026-04-30T12:00:06Z".to_string(),
            )],
        );

        let conn = Connection::open(&path).expect("open persisted db");
        let report = postmortem::build_postmortem_report(
            &conn,
            postmortem::PostmortemTarget::Session("codex-postmortem-boundary".to_string()),
            true,
        )
        .expect("postmortem report");
        let body = report.to_string().to_ascii_lowercase();

        for forbidden in [
            "tool_result",
            "tool result",
            "mcpevent",
            "mcp lifecycle",
            "skillevent",
            "skill lifecycle",
            "cache ttl",
            "cache rebuild",
            "quota",
            "provider cap",
        ] {
            assert!(
                !body.contains(forbidden),
                "postmortem report must not expose unsupported surface {forbidden}"
            );
        }
        assert!(body.contains("tool-call intent"));

        drop(conn);
        cleanup_test_db(&path);
    }

    #[test]
    fn postmortem_redacts_tool_inputs_by_default() {
        let conn = create_full_test_db();
        insert_session(
            &conn,
            "postmortem-redacted-tool",
            "2026-04-30T12:00:00Z",
            Some("2026-04-30T12:00:02Z"),
            "gpt-5.5",
            Some("fixture prompt"),
        );
        let tool_calls = serde_json::json!([{
            "id": "tool-private",
            "name": "shell",
            "input": "{\"command\":\"deploy aurora-private-payload\"}"
        }])
        .to_string();
        conn.execute(
            "INSERT INTO requests (
                request_id, session_id, timestamp, model, input_tokens, output_tokens,
                cache_read_tokens, cache_creation_tokens, cost_dollars, cost_source,
                trusted_for_budget_enforcement, duration_ms, tool_calls, cache_event,
                provider, requested_model, served_model, codex_status,
                codex_input_tokens, codex_cached_input_tokens, codex_uncached_input_tokens,
                codex_output_tokens, codex_reasoning_output_tokens, codex_total_tokens,
                codex_response_id, codex_prompt_excerpt, codex_tool_calls,
                codex_accounting_anomalies
            ) VALUES (
                'req-postmortem-redacted-tool', 'postmortem-redacted-tool',
                '2026-04-30T12:00:01Z', 'gpt-5.5', 100, 20,
                0, 0, 0.0, 'codex_unpriced:unknown_model:gpt-5.5',
                0, 10, '[]', NULL, 'codex_responses', 'gpt-5.5', 'gpt-5.5',
                'completed', 100, 40, 60, 20, 0, 120, 'resp_redacted_tool',
                'codex prompt', ?1, '[]'
            )",
            rusqlite::params![tool_calls],
        )
        .expect("insert codex request");

        let report = postmortem::build_postmortem_report(
            &conn,
            postmortem::PostmortemTarget::Session("postmortem-redacted-tool".to_string()),
            true,
        )
        .expect("postmortem report");
        let body = report.to_string();

        assert!(body.contains("shell"));
        assert!(body.contains("[redacted tool input]"));
        assert!(!body.contains("aurora-private-payload"));
        assert!(!body.contains("deploy aurora"));
    }

    #[test]
    fn postmortem_uses_codex_prompt_and_recomputes_local_token_math() {
        let conn = create_full_test_db();
        insert_session(
            &conn,
            "postmortem-codex-scoped",
            "2026-04-30T12:00:00Z",
            Some("2026-04-30T12:00:02Z"),
            "gpt-5.5",
            Some("legacy session prompt should not appear"),
        );
        conn.execute(
            "INSERT INTO requests (
                request_id, session_id, timestamp, model, input_tokens, output_tokens,
                cache_read_tokens, cache_creation_tokens, cost_dollars, cost_source,
                trusted_for_budget_enforcement, duration_ms, tool_calls, cache_event,
                provider, requested_model, served_model, codex_status,
                codex_input_tokens, codex_cached_input_tokens, codex_uncached_input_tokens,
                codex_output_tokens, codex_reasoning_output_tokens, codex_total_tokens,
                codex_response_id, codex_prompt_excerpt, codex_tool_calls,
                codex_accounting_anomalies
            ) VALUES (
                'req-postmortem-codex-scoped', 'postmortem-codex-scoped',
                '2026-04-30T12:00:01Z', 'gpt-5.5', 100, 20,
                0, 0, 0.0, 'codex_unpriced:unknown_model:gpt-5.5',
                0, 10, '[]', NULL, 'codex_responses', 'gpt-5.5', 'gpt-5.5',
                'completed', 100, 40, 0, 20, 0, 120, 'resp_codex_scoped',
                'codex scoped prompt', '[]', '[]'
            )",
            [],
        )
        .expect("insert codex request");

        let report = postmortem::build_postmortem_report(
            &conn,
            postmortem::PostmortemTarget::Session("postmortem-codex-scoped".to_string()),
            false,
        )
        .expect("postmortem report");
        let body = report.to_string();

        assert_eq!(
            report
                .pointer("/summary/initial_prompt_excerpt")
                .and_then(Value::as_str),
            Some("codex scoped prompt")
        );
        assert_eq!(
            report
                .pointer("/impact/uncached_input_tokens")
                .and_then(Value::as_u64),
            Some(60)
        );
        assert!(report
            .pointer("/signals/provider_reported_total_tokens")
            .is_none());
        assert!(!body.contains("legacy session prompt should not appear"));
    }

    #[test]
    fn postmortem_ignores_stored_degraded_flag_after_filtering_unsupported_causes() {
        let conn = create_full_test_db();
        insert_session(
            &conn,
            "postmortem-stored-legacy",
            "2026-04-30T12:00:00Z",
            Some("2026-04-30T12:00:02Z"),
            "gpt-5.5",
            Some("fixture prompt"),
        );
        conn.execute(
            "INSERT INTO requests (
                request_id, session_id, timestamp, model, input_tokens, output_tokens,
                cache_read_tokens, cache_creation_tokens, cost_dollars, cost_source,
                trusted_for_budget_enforcement, duration_ms, tool_calls, cache_event,
                provider, requested_model, served_model, codex_status,
                codex_input_tokens, codex_cached_input_tokens, codex_uncached_input_tokens,
                codex_output_tokens, codex_reasoning_output_tokens, codex_total_tokens,
                codex_response_id, codex_prompt_excerpt, codex_tool_calls,
                codex_accounting_anomalies
            ) VALUES (
                'req-postmortem-stored-legacy', 'postmortem-stored-legacy',
                '2026-04-30T12:00:01Z', 'gpt-5.5', 100, 20,
                0, 0, 0.0, 'codex_unpriced:unknown_model:gpt-5.5',
                0, 10, '[]', NULL, 'codex_responses', 'gpt-5.5', 'gpt-5.5',
                'completed', 100, 40, 60, 20, 0, 120, 'resp_stored_legacy',
                'fixture prompt', '[]', '[]'
            )",
            [],
        )
        .expect("insert codex request");
        conn.execute(
            "INSERT INTO session_diagnoses (
                session_id, completed_at, outcome, total_turns, total_cost,
                degraded, degradation_turn, causes_json, advice_json
            ) VALUES (?1, ?2, 'Likely Partially Completed', 1, 0.0, 1, 1, ?3, ?4)",
            rusqlite::params![
                "postmortem-stored-legacy",
                "2026-04-30T12:00:02Z",
                serde_json::json!([
                    {
                        "turn_first_noticed": 1,
                        "cause_type": "cache_miss_ttl",
                        "detail": "legacy cache cause"
                    }
                ])
                .to_string(),
                serde_json::json!(["legacy advice"]).to_string(),
            ],
        )
        .expect("insert legacy diagnosis row");

        let report = postmortem::build_postmortem_report(
            &conn,
            postmortem::PostmortemTarget::Session("postmortem-stored-legacy".to_string()),
            true,
        )
        .expect("postmortem report");
        let body = report.to_string();

        assert_eq!(
            report
                .pointer("/diagnosis/degraded")
                .and_then(Value::as_bool),
            Some(false)
        );
        assert_eq!(
            report
                .pointer("/diagnosis/degradation_turn")
                .unwrap_or(&Value::Null),
            &Value::Null
        );
        assert_eq!(
            report
                .pointer("/diagnosis/primary_cause")
                .and_then(Value::as_str),
            Some("none")
        );
        assert!(!body.contains("cache_miss_ttl"));
        assert!(!body.contains("legacy cache cause"));
        assert!(!body.contains("legacy advice"));
    }

    #[test]
    fn codex_persistence_tool_intent_is_source_scoped_without_cache_event() {
        let path = unique_test_db_path("codex-persist-tools");
        let request = parse_codex_fixture_request("phase-4c-session-tools");
        let response = accumulate_codex_fixture_response(
            include_str!("../../test/fixtures/openai_responses_tool_stream.sse"),
            Some("gpt-codex-fixture"),
        );
        let outcome = build_codex_finalization_outcome(
            "req-phase-4c-tools",
            &request,
            &response,
            Duration::from_millis(25),
            STANDARD_CONTEXT_WINDOW_TOKENS,
        );

        run_db_writer_commands(
            &path,
            vec![record_codex_turn_command(
                &outcome,
                "2026-04-30T12:00:06Z".to_string(),
            )],
        );

        let conn = Connection::open(&path).expect("open persisted db");
        let (tool_names_json, tool_calls_json, cache_event): (String, String, Option<String>) =
            conn.query_row(
                "SELECT tool_calls, codex_tool_calls, cache_event
                 FROM requests WHERE request_id = 'req-phase-4c-tools'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("load tool summaries");
        let tool_call_rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM tool_calls", [], |row| row.get(0))
            .expect("count tool call rows");

        assert_eq!(tool_names_json, "[]");
        assert!(tool_calls_json.contains("ctc_fixture_read_file_001"));
        assert!(tool_calls_json.contains(r#""name":"read_file""#));
        assert_eq!(cache_event, None);
        assert_eq!(tool_call_rows, 0);

        drop(conn);
        cleanup_test_db(&path);
    }

    #[test]
    fn headers_and_context_helpers_handle_common_variants() {
        let mut headers = make_http_headers(&[
            ("openai-model", "gpt-5.5"),
            ("X-Context-Window-Tokens", "123456"),
        ]);
        headers
            .headers
            .as_mut()
            .expect("headers")
            .headers
            .push(ProtoHeaderValue {
                key: "x-raw".to_string(),
                value: String::new(),
                raw_value: b"raw-value".to_vec(),
            });

        assert_eq!(
            extract_header(&headers, "OPENAI-MODEL").as_deref(),
            Some("gpt-5.5")
        );
        assert_eq!(
            extract_header(&headers, "X-RAW").as_deref(),
            Some("raw-value")
        );
        assert_eq!(extract_headers(&headers, "missing"), Vec::<String>::new());
        assert_eq!(
            infer_context_window_tokens(Some("gpt-5.5"), None, 200_001, 0, 0),
            EXTENDED_CONTEXT_WINDOW_TOKENS
        );
        assert_eq!(
            infer_context_window_tokens(Some("gpt-5.5"), None, 100, 50, 25),
            STANDARD_CONTEXT_WINDOW_TOKENS
        );
        assert_eq!(context_fill_ratio(100, 50, 50, 200), 1.0);
        assert_eq!(context_fill_percent(100, 50, 50, 200), 100.0);
    }

    #[test]
    fn display_names_use_workdir_and_collision_suffix() {
        assert_eq!(
            derive_display_name("/Users/pradeep/code/codex-blackbox", "gpt-5.5", 0xabc),
            "codex-blackbox"
        );

        let hash = 0xabc_u64;
        diagnosis::SESSIONS.insert(
            hash,
            diagnosis::SessionState {
                session_id: "session_existing".to_string(),
                display_name: "codex-blackbox".to_string(),
                model: "gpt-5.5".to_string(),
                initial_prompt: None,
                created_at: Instant::now(),
                last_activity: Instant::now(),
                session_inserted: true,
            },
        );
        let display = derive_display_name("/tmp/codex-blackbox", "gpt-5.5", hash);
        let _ = diagnosis::SESSIONS.remove(&hash);
        assert_eq!(display, "codex-blackbox-abc");
    }

    #[test]
    fn codex_display_name_falls_back_to_agents_preamble_repo() {
        let prompt = "# AGENTS.md instructions for /Users/pradeepsingh/code/nordic_hedge_fund\n\
                      <INSTRUCTIONS>\n...";

        assert_eq!(
            repo_name_from_codex_initial_prompt(prompt).as_deref(),
            Some("nordic_hedge_fund")
        );
        assert_eq!(
            persisted_session_display_name("019ddf2c-4387", Some("gpt-5.5"), Some(prompt)),
            "nordic_hedge_fund"
        );
    }

    #[test]
    fn chatgpt_auxiliary_requests_are_not_parsed_as_model_json() {
        assert!(should_skip_chatgpt_auxiliary_request_body(
            "/backend-api/wham/apps"
        ));
        assert!(should_skip_chatgpt_auxiliary_request_body(
            "/backend-api/wham/apps?foo=bar"
        ));
        assert!(!should_skip_chatgpt_auxiliary_request_body(
            "/backend-api/codex/responses"
        ));
        assert!(!should_skip_chatgpt_auxiliary_request_body("/v1/messages"));
        assert!(!should_skip_chatgpt_auxiliary_request_body(""));
    }

    #[test]
    fn recall_text_helpers_score_human_content_not_machine_noise() {
        assert!(looks_like_machine_recall_line("```json"));
        assert!(looks_like_machine_recall_line(r#""tool_use": "Bash","#));
        assert!(!looks_like_machine_recall_line(
            "Fixed the auth cache warm path."
        ));

        assert_eq!(normalize_search_text("Auth-cache!!"), "auth cache");
        let terms = tokenize_search_text("auth cache x");
        assert_eq!(terms, vec!["auth", "cache"]);
        let score = score_recall_doc(
            "auth cache",
            &terms,
            "Investigate auth cache",
            "Fixed cache warm path",
            "gpt-5.5",
        )
        .expect("score");
        assert!(score > 80);
        assert!(score_recall_doc("billing", &["billing".to_string()], "", "", "gpt-5").is_none());
    }

    #[test]
    fn codex_envoy_tool_events_are_suppressed_by_key() {
        let event = super::watch::WatchEvent::ToolUse {
            session_id: "codex-dedupe-session-001".to_string(),
            timestamp: "2026-04-30T12:00:08Z".to_string(),
            tool_name: "read_file".to_string(),
            summary: "Cargo.toml".to_string(),
        };

        assert!(!codex_watch_event_is_duplicate_or_remember(&event));
        assert!(codex_watch_event_is_duplicate_or_remember(&event));
    }

    #[test]
    fn codex_finalization_emits_tool_use_for_responses_tool_calls() {
        let request = parse_codex_fixture_request("phase-7-tool-session-001");
        let response = accumulate_codex_fixture_response(
            include_str!("../../test/fixtures/openai_responses_tool_stream.sse"),
            Some("gpt-codex-fixture"),
        );
        let outcome = build_codex_finalization_outcome(
            "phase-7-tool-request-001",
            &request,
            &response,
            Duration::from_millis(20),
            128_000,
        );

        assert!(outcome.watch_events.iter().any(|event| {
            matches!(
                event,
                super::watch::WatchEvent::ToolUse {
                    session_id,
                    tool_name,
                    summary,
                    ..
                } if session_id == "phase-7-tool-session-001"
                    && tool_name == "read_file"
                    && summary == "Cargo.toml"
            )
        }));
    }

    fn insert_codex_turn_snapshot(
        conn: &Connection,
        session_id: &str,
        status: &str,
        turn_number: i64,
    ) {
        conn.execute(
            "INSERT INTO turn_snapshots (
                session_id, turn_number, timestamp, input_tokens, cache_read_tokens,
                cache_creation_tokens, output_tokens, ttft_ms, tool_calls, tool_failures,
                gap_from_prev_secs, context_utilization, context_window_tokens,
                frustration_signals, requested_model, actual_model, response_summary,
                request_id, provider, codex_status, codex_input_tokens,
                codex_cached_input_tokens, codex_uncached_input_tokens,
                codex_output_tokens, codex_reasoning_output_tokens, codex_total_tokens,
                codex_accounting_anomalies
            ) VALUES (
                ?1, ?2, '2026-04-30T12:00:00Z', 1000, 0, 0, 100, 10,
                '[]', 0, 0.0, 0.10, 128000, 0, 'gpt-codex-fixture',
                'gpt-codex-fixture', NULL, ?3, 'codex_responses', ?4, 1000,
                500, 500, 100, 20, 1100, '[]'
            )",
            rusqlite::params![
                session_id,
                turn_number,
                format!("req-{turn_number}"),
                status
            ],
        )
        .expect("insert codex degradation turn snapshot");
    }

    #[test]
    fn codex_diagnosis_persisted_failed_turn_reports_on_demand() {
        let conn = Connection::open_in_memory().expect("open sqlite");
        conn.execute_batch(SCHEMA).expect("create schema");
        ensure_codex_persistence_columns(&conn).expect("ensure codex columns");
        insert_session(
            &conn,
            "codex-db-failed",
            "2026-04-30T12:00:00Z",
            None,
            "gpt-codex-fixture",
            Some("fixture prompt"),
        );
        insert_codex_turn_snapshot(&conn, "codex-db-failed", "failed", 1);

        let turns =
            load_turn_snapshots_from_db(&conn, "codex-db-failed").expect("load persisted turns");
        let report = diagnosis::analyze_session("codex-db-failed", &turns);

        assert!(report.degraded);
        assert!(report
            .causes
            .iter()
            .any(|cause| cause.cause_type == "codex_response_failed"));
    }

    #[test]
    fn persisted_watch_replay_rebuilds_completed_codex_session_events() {
        let conn = Connection::open_in_memory().expect("open sqlite");
        conn.execute_batch(SCHEMA).expect("create schema");
        ensure_codex_persistence_columns(&conn).expect("ensure codex columns");
        insert_session(
            &conn,
            "codex-watch-db",
            "2026-04-30T12:00:00Z",
            Some("2026-04-30T12:00:10Z"),
            "stale-session-model",
            Some("# AGENTS.md instructions for /Users/pradeepsingh/code/codex-blackbox"),
        );
        insert_codex_turn_snapshot(&conn, "codex-watch-db", "completed", 1);

        let events =
            load_persisted_watch_replay_events(&conn, Some("codex-watch-db"), 8).expect("events");

        assert!(events.iter().any(|event| matches!(
            event,
            super::watch::WatchEvent::SessionStart {
                session_id,
                display_name,
                model,
                ..
            } if session_id == "codex-watch-db" && display_name == "codex-blackbox" && model == "gpt-codex-fixture"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            super::watch::WatchEvent::CodexTurnSummary {
                session_id,
                status,
                requested_model,
                served_model: Some(served_model),
                cached_input_tokens: 500,
                reasoning_output_tokens: 20,
                total_tokens: 1100,
                ..
            } if session_id == "codex-watch-db"
                && status == "completed"
                && requested_model == "gpt-codex-fixture"
                && served_model == "gpt-codex-fixture"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            super::watch::WatchEvent::ContextStatus {
                session_id,
                fill_percent,
                context_window_tokens: Some(128000),
                ..
            } if session_id == "codex-watch-db" && (fill_percent - 10.0).abs() < f64::EPSILON
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            super::watch::WatchEvent::PostmortemReady {
                session_id,
                total_turns: 1,
                total_tokens: 1100,
                postmortem_command,
                ..
            } if session_id == "codex-watch-db"
                && postmortem_command == "codex-blackbox postmortem codex-watch-db"
        )));
    }

    #[test]
    fn session_filtered_watch_detects_existing_session_start_in_recent_history() {
        let history = vec![
            super::watch::WatchEvent::CodexTurnSummary {
                session_id: "codex-watch-db".to_string(),
                status: "completed".to_string(),
                requested_model: "gpt-5.5".to_string(),
                served_model: Some("gpt-5.5".to_string()),
                input_tokens: 10,
                cached_input_tokens: 4,
                uncached_input_tokens: 6,
                output_tokens: 2,
                reasoning_output_tokens: 1,
                total_tokens: 12,
            },
            super::watch::WatchEvent::SessionStart {
                session_id: "codex-watch-db".to_string(),
                display_name: "codex-blackbox".to_string(),
                model: "gpt-5.5".to_string(),
                initial_prompt: Some("fixture prompt".to_string()),
            },
        ];

        assert!(history_contains_session_start(&history, "codex-watch-db"));
        assert!(!history_contains_session_start(&history, "other-session"));
    }

    #[test]
    fn postmortem_ready_broadcasts_once_per_eligible_codex_session() {
        let conn = Connection::open_in_memory().expect("open sqlite");
        conn.execute_batch(SCHEMA).expect("create schema");
        ensure_codex_persistence_columns(&conn).expect("ensure codex columns");
        let session_id = format!("codex-postmortem-ready-{}", now_epoch_secs());
        insert_session(
            &conn,
            &session_id,
            "2026-04-30T12:00:00Z",
            Some("2026-04-30T12:00:10Z"),
            "gpt-codex-fixture",
            Some("fixture prompt"),
        );
        insert_codex_turn_snapshot(&conn, &session_id, "completed", 1);

        let (_history, mut rx) = super::watch::BROADCASTER.subscribe_with_history();
        maybe_broadcast_postmortem_ready(&conn, &session_id);
        maybe_broadcast_postmortem_ready(&conn, &session_id);

        let mut matching = 0;
        while let Ok(event) = rx.try_recv() {
            if matches!(
                event,
                super::watch::WatchEvent::PostmortemReady {
                    session_id: ref event_session,
                    ..
                } if event_session == &session_id
            ) {
                matching += 1;
            }
        }
        assert_eq!(matching, 1);
    }

    #[test]
    fn persisted_watch_replay_requires_codex_turn_evidence() {
        let conn = create_full_test_db();
        insert_session(
            &conn,
            "legacy-watch-db",
            "2026-04-30T12:00:00Z",
            Some("2026-04-30T12:00:10Z"),
            "gpt-5.5",
            Some("legacy prompt"),
        );
        conn.execute(
            "UPDATE sessions SET request_count = 1 WHERE session_id = 'legacy-watch-db'",
            [],
        )
        .expect("mark legacy session active");
        insert_turn_snapshot(
            &conn,
            "legacy-watch-db",
            1,
            "2026-04-30T12:00:00Z",
            0,
            1000,
            1000,
            0.0,
            0.10,
            None,
        );
        insert_session(
            &conn,
            "codex-watch-db",
            "2026-04-30T12:01:00Z",
            Some("2026-04-30T12:01:10Z"),
            "gpt-5.5",
            Some("codex prompt"),
        );
        insert_codex_turn_snapshot(&conn, "codex-watch-db", "completed", 1);

        let events = load_persisted_watch_replay_events(&conn, None, 8).expect("events");
        let legacy_events =
            load_persisted_watch_replay_events(&conn, Some("legacy-watch-db"), 8).expect("events");

        assert!(events
            .iter()
            .any(|event| event_matches_session(event, Some("codex-watch-db"))));
        assert!(!events
            .iter()
            .any(|event| event_matches_session(event, Some("legacy-watch-db"))));
        assert!(legacy_events.is_empty());
    }

    #[test]
    fn codex_diagnosis_uses_only_envoy_turn_snapshots_from_db() {
        let conn = Connection::open_in_memory().expect("open sqlite");
        conn.execute_batch(SCHEMA).expect("create schema");
        ensure_codex_persistence_columns(&conn).expect("ensure codex columns");
        insert_session(
            &conn,
            "codex-db-hooks",
            "2026-04-30T12:00:00Z",
            None,
            "gpt-codex-fixture",
            Some("fixture prompt"),
        );
        for turn in 1..=3 {
            insert_codex_turn_snapshot(&conn, "codex-db-hooks", "completed", turn);
        }

        let turns =
            load_turn_snapshots_from_db(&conn, "codex-db-hooks").expect("load persisted turns");
        let report = diagnosis::analyze_session("codex-db-hooks", &turns);
        let causes = report
            .causes
            .iter()
            .map(|cause| cause.cause_type.as_str())
            .collect::<Vec<_>>();

        assert!(!report.degraded);
        assert!(!causes.iter().any(|cause| cause.contains("tool")));
        assert!(!causes.iter().any(|cause| cause.contains("mcp")));
    }

    #[test]
    fn load_turn_snapshots_from_db_requires_codex_provider_evidence() {
        let conn = create_full_test_db();
        insert_session(
            &conn,
            "mixed-db-session",
            "2026-04-30T12:00:00Z",
            Some("2026-04-30T12:00:00Z"),
            "gpt-5.5",
            Some("fixture prompt"),
        );
        insert_turn_snapshot(
            &conn,
            "mixed-db-session",
            1,
            "2026-04-30T12:00:00Z",
            0,
            1000,
            1000,
            0.0,
            0.10,
            None,
        );
        insert_codex_degradation_turn_snapshot(
            &conn,
            "mixed-db-session",
            2,
            "2026-04-30T12:01:00Z",
            "failed",
            10_000,
            4_000,
            800,
            500,
            0.85,
            r#"[{"type":"reported_total_tokens_mismatch"}]"#,
        );

        let turns =
            load_turn_snapshots_from_db(&conn, "mixed-db-session").expect("load persisted turns");

        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].turn_number, 2);
        assert_eq!(turns[0].provider.as_deref(), Some("codex_responses"));
    }

    #[test]
    fn persisted_diagnosis_report_keeps_only_codex_envoy_causes() {
        let conn = create_full_test_db();
        insert_session(
            &conn,
            "diagnosis-codex-only",
            "2026-04-30T12:00:00Z",
            Some("2026-04-30T12:00:00Z"),
            "gpt-5.5",
            Some("fixture prompt"),
        );
        let report = diagnosis::DiagnosisReport {
            session_id: "diagnosis-codex-only".to_string(),
            outcome: "Likely Partially Completed".to_string(),
            total_turns: 2,
            total_tokens: 1200,
            estimated_total_cost_dollars: 0.0,
            cost_source: "codex_unpriced:unknown_model:gpt-5.5".to_string(),
            trusted_for_budget_enforcement: false,
            cache_hit_ratio: 0.0,
            degraded: true,
            degradation_turn: Some(1),
            causes: vec![
                diagnosis::DegradationCause {
                    turn_first_noticed: 1,
                    cause_type: "cache_miss_ttl".to_string(),
                    detail: "legacy cache miss".to_string(),
                    estimated_cost: 0.0,
                    is_heuristic: false,
                    requested_model: None,
                    actual_model: None,
                },
                diagnosis::DegradationCause {
                    turn_first_noticed: 2,
                    cause_type: "codex_response_failed".to_string(),
                    detail: "Responses stream failed".to_string(),
                    estimated_cost: 0.0,
                    is_heuristic: false,
                    requested_model: None,
                    actual_model: None,
                },
            ],
            advice: vec!["legacy advice".to_string()],
        };

        persist_session_diagnosis_report(
            &conn,
            "diagnosis-codex-only",
            "2026-04-30T12:02:00Z",
            &report,
        )
        .expect("persist diagnosis");

        let (degraded, degradation_turn, causes_json, advice_json): (
            i64,
            Option<i64>,
            String,
            String,
        ) = conn
            .query_row(
                "SELECT degraded, degradation_turn, causes_json, advice_json \
                 FROM session_diagnoses WHERE session_id = 'diagnosis-codex-only'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("load persisted diagnosis");
        let causes: Value = serde_json::from_str(&causes_json).expect("parse causes");
        let advice: Value = serde_json::from_str(&advice_json).expect("parse advice");

        assert_eq!(degraded, 1);
        assert_eq!(degradation_turn, Some(2));
        assert!(causes.to_string().contains("codex_response_failed"));
        assert!(!causes.to_string().contains("cache_miss_ttl"));
        assert_eq!(advice, Value::Array(vec![]));
    }

    #[test]
    fn repair_session_diagnosis_envoy_causes_filters_existing_rows() {
        let conn = create_full_test_db();
        insert_session(
            &conn,
            "diagnosis-legacy-row",
            "2026-04-30T12:00:00Z",
            Some("2026-04-30T12:00:00Z"),
            "gpt-5.5",
            Some("fixture prompt"),
        );
        conn.execute(
            "INSERT INTO session_diagnoses (
                session_id, completed_at, outcome, total_turns, total_cost,
                degraded, degradation_turn, causes_json, advice_json
            ) VALUES (?1, ?2, 'Likely Partially Completed', 2, 0.0, 1, 1, ?3, ?4)",
            rusqlite::params![
                "diagnosis-legacy-row",
                "2026-04-30T12:02:00Z",
                serde_json::json!([
                    {"turn_first_noticed": 1, "cause_type": "cache_miss_ttl"},
                    {"turn_first_noticed": 2, "cause_type": "codex_response_failed"}
                ])
                .to_string(),
                serde_json::json!(["legacy advice"]).to_string(),
            ],
        )
        .expect("insert legacy diagnosis row");

        repair_session_diagnosis_envoy_causes(&conn).expect("repair diagnosis causes");

        let (degraded, degradation_turn, causes_json, advice_json): (
            i64,
            Option<i64>,
            String,
            String,
        ) = conn
            .query_row(
                "SELECT degraded, degradation_turn, causes_json, advice_json \
                 FROM session_diagnoses WHERE session_id = 'diagnosis-legacy-row'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("load repaired diagnosis");

        assert_eq!(degraded, 1);
        assert_eq!(degradation_turn, Some(2));
        assert!(causes_json.contains("codex_response_failed"));
        assert!(!causes_json.contains("cache_miss_ttl"));
        assert_eq!(
            serde_json::from_str::<Value>(&advice_json).expect("parse advice"),
            Value::Array(vec![])
        );
    }

    #[test]
    fn codex_diagnosis_metrics_do_not_expose_session_id_labels() {
        let _guard = METRICS_TEST_LOCK.lock().unwrap();
        metrics::init();
        metrics::record_degraded_cause("codex_response_failed");
        let (_, body) = metrics::render().expect("render metrics");

        assert!(!body.contains("session_id="));
        assert!(!body.contains("session=\""));
        assert!(body.contains("cause_type=\"codex_response_failed\""));
    }

    #[test]
    fn ensure_session_columns_drops_legacy_cache_hit_ratio() {
        let conn = Connection::open_in_memory().expect("open sqlite");
        conn.execute_batch(
            "CREATE TABLE sessions (
                session_id TEXT PRIMARY KEY,
                started_at TEXT NOT NULL,
                ended_at TEXT,
                total_input_tokens INTEGER DEFAULT 0,
                total_output_tokens INTEGER DEFAULT 0,
                total_cache_read_tokens INTEGER DEFAULT 0,
                total_cache_creation_tokens INTEGER DEFAULT 0,
                total_cost_dollars REAL DEFAULT 0.0,
                cache_hit_ratio REAL,
                cache_waste_dollars REAL DEFAULT 0.0,
                request_count INTEGER DEFAULT 0,
                model TEXT
            );",
        )
        .expect("create legacy sessions table");

        ensure_session_columns(&conn).expect("migrate sessions columns");

        let columns = conn
            .prepare("PRAGMA table_info(sessions)")
            .expect("prepare pragma")
            .query_map([], |row| row.get::<_, String>(1))
            .expect("query pragma")
            .filter_map(|row| row.ok())
            .collect::<Vec<_>>();

        assert!(columns.contains(&"initial_prompt".to_string()));
        assert!(columns.contains(&"display_name".to_string()));
        assert!(!columns.contains(&"cache_hit_ratio".to_string()));
    }

    #[test]
    fn ensure_session_diagnosis_columns_drops_legacy_cache_hit_ratio() {
        let conn = Connection::open_in_memory().expect("open sqlite");
        conn.execute_batch(
            "CREATE TABLE session_diagnoses (
                session_id TEXT PRIMARY KEY,
                completed_at TEXT NOT NULL,
                outcome TEXT NOT NULL,
                total_turns INTEGER,
                total_cost REAL,
                cache_hit_ratio REAL,
                degraded INTEGER DEFAULT 0,
                degradation_turn INTEGER,
                causes_json TEXT,
                advice_json TEXT
            );",
        )
        .expect("create legacy diagnoses table");

        ensure_session_diagnosis_columns(&conn).expect("migrate session_diagnoses columns");

        let columns = table_columns(&conn, "session_diagnoses").expect("load diagnosis columns");
        assert!(!columns.contains("cache_hit_ratio"));
        assert!(columns.contains("causes_json"));
    }

    #[test]
    fn drop_legacy_lifecycle_tables_removes_old_db_evidence_tables() {
        let conn = Connection::open_in_memory().expect("open sqlite");
        conn.execute_batch(
            "CREATE TABLE tool_outcomes (id INTEGER PRIMARY KEY, session_id TEXT);
             CREATE INDEX idx_tool_outcomes_session ON tool_outcomes(session_id);
             CREATE TABLE skill_events (id INTEGER PRIMARY KEY, session_id TEXT);
             CREATE INDEX idx_skill_events_session ON skill_events(session_id);
             CREATE TABLE mcp_events (id INTEGER PRIMARY KEY, session_id TEXT);
             CREATE INDEX idx_mcp_events_session ON mcp_events(session_id);",
        )
        .expect("create legacy lifecycle tables");

        drop_legacy_lifecycle_tables(&conn).expect("drop legacy lifecycle tables");

        let table_count = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'table' AND name IN ('tool_outcomes', 'skill_events', 'mcp_events')",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("count legacy lifecycle tables");
        let index_count = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'index' AND name IN (
                    'idx_tool_outcomes_session',
                    'idx_skill_events_session',
                    'idx_mcp_events_session'
                 )",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("count legacy lifecycle indexes");

        assert_eq!(table_count, 0);
        assert_eq!(index_count, 0);
    }

    fn unique_test_db_path(label: &str) -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time went backwards")
            .as_nanos();
        let mut path = std::env::temp_dir();
        path.push(format!(
            "codex-blackbox-{label}-{}-{nanos}.db",
            std::process::id()
        ));
        path.to_string_lossy().into_owned()
    }

    fn cleanup_test_db(path: &str) {
        let _ = fs::remove_file(path);
        let _ = fs::remove_file(format!("{path}-wal"));
        let _ = fs::remove_file(format!("{path}-shm"));
    }

    fn run_db_writer_commands(path: &str, commands: Vec<DbCommand>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let writer_path = path.to_string();
        let handle = std::thread::spawn(move || db_writer_loop(&writer_path, rx));
        for command in commands {
            tx.send(command).expect("queue db command");
        }
        drop(tx);
        handle.join().expect("join db writer");
    }

    fn insert_session(
        conn: &Connection,
        session_id: &str,
        started_at: &str,
        ended_at: Option<&str>,
        model: &str,
        initial_prompt: Option<&str>,
    ) {
        conn.execute(
            "INSERT INTO sessions (session_id, started_at, ended_at, model, initial_prompt) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![session_id, started_at, ended_at, model, initial_prompt],
        )
        .expect("insert session");
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_request(
        conn: &Connection,
        request_id: &str,
        session_id: &str,
        timestamp: &str,
        model: &str,
        input_tokens: i64,
        output_tokens: i64,
        cache_read_tokens: i64,
        cache_creation_tokens: i64,
    ) {
        conn.execute(
            "INSERT INTO requests (
                request_id, session_id, timestamp, model, input_tokens, output_tokens,
                cache_read_tokens, cache_creation_tokens, cost_dollars, cost_source,
                trusted_for_budget_enforcement, duration_ms, tool_calls, cache_event
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, NULL, NULL, 0, '[]', NULL)",
            rusqlite::params![
                request_id,
                session_id,
                timestamp,
                model,
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_creation_tokens,
            ],
        )
        .expect("insert request");
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_codex_request(
        conn: &Connection,
        request_id: &str,
        session_id: &str,
        timestamp: &str,
        model: &str,
        input_tokens: i64,
        cached_input_tokens: i64,
        output_tokens: i64,
    ) {
        conn.execute(
            "INSERT INTO requests (
                request_id, session_id, timestamp, model, input_tokens, output_tokens,
                cache_read_tokens, cache_creation_tokens, cost_dollars, cost_source,
                trusted_for_budget_enforcement, duration_ms, tool_calls, cache_event,
                provider, requested_model, served_model, codex_input_tokens,
                codex_cached_input_tokens, codex_output_tokens
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, 0, NULL, NULL, NULL, 0, '[]', NULL,
                      'codex_responses', ?4, ?4, ?5, ?7, ?6)",
            rusqlite::params![
                request_id,
                session_id,
                timestamp,
                model,
                input_tokens,
                output_tokens,
                cached_input_tokens,
            ],
        )
        .expect("insert codex request");
    }

    fn insert_diagnosis(
        conn: &Connection,
        session_id: &str,
        completed_at: &str,
        degraded: bool,
        causes_json: &str,
    ) {
        conn.execute(
            "INSERT INTO session_diagnoses (
                session_id, completed_at, outcome, total_turns, total_cost,
                degraded, degradation_turn, causes_json, advice_json
            ) VALUES (?1, ?2, 'Completed', 3, 1.0, ?3, NULL, ?4, '[]')",
            rusqlite::params![session_id, completed_at, degraded as i64, causes_json],
        )
        .expect("insert diagnosis");
    }

    fn insert_billing_reconciliation(
        conn: &Connection,
        session_id: &str,
        imported_at: &str,
        source: &str,
        billed_cost_dollars: f64,
    ) {
        conn.execute(
            "INSERT INTO billing_reconciliations (
                session_id, imported_at, source, billed_cost_dollars
            ) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![session_id, imported_at, source, billed_cost_dollars],
        )
        .expect("insert billing reconciliation");
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_turn_snapshot(
        conn: &Connection,
        session_id: &str,
        turn_number: i64,
        timestamp: &str,
        cache_read_tokens: i64,
        cache_creation_tokens: i64,
        ttft_ms: i64,
        gap_from_prev_secs: f64,
        context_utilization: f64,
        response_summary: Option<&str>,
    ) {
        conn.execute(
            "INSERT INTO turn_snapshots (
                session_id, turn_number, timestamp, input_tokens, cache_read_tokens,
                cache_creation_tokens, output_tokens, ttft_ms, tool_calls, tool_failures,
                gap_from_prev_secs, context_utilization, frustration_signals,
                requested_model, actual_model, response_summary
            ) VALUES (?1, ?2, ?3, 1000, ?4, ?5, 100, ?6, '[]', 0, ?7, ?8, 0, ?9, ?10, ?11)",
            rusqlite::params![
                session_id,
                turn_number,
                timestamp,
                cache_read_tokens,
                cache_creation_tokens,
                ttft_ms,
                gap_from_prev_secs,
                context_utilization,
                "gpt-5.5",
                "gpt-5.5",
                response_summary,
            ],
        )
        .expect("insert turn snapshot");
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_codex_degradation_turn_snapshot(
        conn: &Connection,
        session_id: &str,
        turn_number: i64,
        timestamp: &str,
        status: &str,
        input_tokens: i64,
        cached_input_tokens: i64,
        output_tokens: i64,
        reasoning_output_tokens: i64,
        context_utilization: f64,
        accounting_anomalies: &str,
    ) {
        conn.execute(
            "INSERT INTO turn_snapshots (
                session_id, turn_number, timestamp, input_tokens, cache_read_tokens,
                cache_creation_tokens, output_tokens, ttft_ms, tool_calls, tool_failures,
                gap_from_prev_secs, context_utilization, context_window_tokens,
                frustration_signals, requested_model, actual_model, response_summary,
                provider, codex_status, codex_input_tokens, codex_cached_input_tokens,
                codex_uncached_input_tokens, codex_output_tokens,
                codex_reasoning_output_tokens, codex_total_tokens,
                codex_accounting_anomalies
            ) VALUES (?1, ?2, ?3, ?4, 0, 0, ?5, 120, '[]', 0, 0.0, ?6,
                      1000000, 0, 'gpt-5.5', 'gpt-5.5', NULL,
                      'codex_responses', ?7, ?4, ?8, ?9, ?5, ?10, ?11, ?12)",
            rusqlite::params![
                session_id,
                turn_number,
                timestamp,
                input_tokens,
                output_tokens,
                context_utilization,
                status,
                cached_input_tokens,
                input_tokens.saturating_sub(cached_input_tokens),
                reasoning_output_tokens,
                input_tokens + output_tokens,
                accounting_anomalies,
            ],
        )
        .expect("insert codex turn snapshot");
    }

    fn parse_codex_fixture_request(session_id: &str) -> super::codex_request::ParsedCodexRequest {
        super::codex_request::parse_codex_responses_request(
            include_bytes!("../../test/fixtures/openai_responses_minimal_text_request.json"),
            super::codex_request::CodexRequestHeaders {
                session_id: Some(session_id.to_string()),
                client_request_id: None,
            },
        )
        .expect("parse codex fixture request")
    }

    fn accumulate_codex_fixture_response(
        stream: &str,
        served_model_header: Option<&str>,
    ) -> super::codex_response::CodexResponseSummary {
        let mut accumulator = super::codex_response::CodexResponsesAccumulator::new();
        if let Some(served_model) = served_model_header {
            accumulator.apply_headers(&super::codex_response::CodexResponseHeaders {
                http_status: Some(200),
                served_model: Some(served_model.to_string()),
            });
        }
        accumulator
            .process_chunk(stream.as_bytes())
            .expect("process codex fixture stream");
        accumulator.finish().expect("finish codex fixture stream");
        accumulator.summary()
    }

    fn make_http_headers(entries: &[(&str, &str)]) -> HttpHeaders {
        HttpHeaders {
            headers: Some(super::envoy::config::core::v3::HeaderMap {
                headers: entries
                    .iter()
                    .map(|(key, value)| ProtoHeaderValue {
                        key: (*key).to_string(),
                        value: (*value).to_string(),
                        raw_value: Vec::new(),
                    })
                    .collect(),
            }),
            ..Default::default()
        }
    }

    fn history_window<'a>(
        windows: &'a [metrics::HistoricalWindowMetrics],
        label: &str,
    ) -> &'a metrics::HistoricalWindowMetrics {
        windows
            .iter()
            .find(|window| window.window == label)
            .expect("window present")
    }

    #[test]
    fn compact_response_summary_strips_machine_noise() {
        let raw = r#"
            ```json
            { "tool_use": "Bash", "command": "cargo test auth" }
            ```
            Fixed the auth middleware ordering and reran the targeted tests.
        "#;

        let summary = compact_response_summary(raw).expect("summary");
        assert_eq!(
            summary,
            "Fixed the auth middleware ordering and reran the targeted tests."
        );
    }

    #[test]
    fn historical_metrics_empty_db_returns_zeroes() {
        let conn = create_history_test_db();
        let windows = query_historical_metrics(&conn, 1_776_700_000).expect("query history");

        assert_eq!(windows.len(), metrics::HISTORY_WINDOWS.len());
        for window in &windows {
            assert_eq!(window.sessions, 0);
            assert_eq!(window.degraded_sessions, 0);
            assert_eq!(window.degraded_session_ratio, 0.0);
            assert!(window.degraded_causes.is_empty());
            assert!(window.model_fallbacks.is_empty());
        }
    }

    #[test]
    fn historical_metrics_refresh_initializes_clean_db_schema() {
        let conn = Connection::open_in_memory().expect("open sqlite");

        repair_persisted_session_artifacts(&conn).expect("initialize clean schema");
        let windows = query_historical_metrics(&conn, 1_776_700_000).expect("query history");

        assert_eq!(windows.len(), metrics::HISTORY_WINDOWS.len());
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'session_diagnoses'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("count diagnosis table"),
            1
        );
        assert!(windows.iter().all(|window| window.sessions == 0));
    }

    #[test]
    fn historical_metrics_excludes_rows_outside_window() {
        let conn = create_history_test_db();
        let now = 1_776_700_000;
        let two_days_ago = epoch_to_iso8601(now - 2 * 86_400);

        insert_request(
            &conn,
            "req-old",
            "session-old",
            &two_days_ago,
            "gpt-5.4",
            1_000_000,
            0,
            40,
            10,
        );
        insert_session(
            &conn,
            "session-old",
            &two_days_ago,
            Some(&two_days_ago),
            "gpt-5.4",
            None,
        );
        insert_diagnosis(
            &conn,
            "session-old",
            &two_days_ago,
            true,
            r#"[{"cause_type":"codex_high_context_fill"}]"#,
        );

        let windows = query_historical_metrics(&conn, now).expect("query history");
        let one_day = history_window(&windows, "1d");
        let seven_day = history_window(&windows, "7d");

        assert_eq!(one_day.sessions, 0);
        assert_eq!(one_day.degraded_sessions, 0);
        assert!(one_day.degraded_causes.is_empty());

        assert_eq!(seven_day.sessions, 1);
        assert_eq!(seven_day.degraded_sessions, 1);
        assert_eq!(
            seven_day.degraded_causes.get("codex_high_context_fill"),
            Some(&1)
        );
    }

    #[test]
    fn historical_metrics_aggregate_codex_causes() {
        let conn = create_history_test_db();
        let now = 1_776_700_000;
        let one_hour_ago = epoch_to_iso8601(now - 3_600);
        let two_hours_ago = epoch_to_iso8601(now - 7_200);
        let three_hours_ago = epoch_to_iso8601(now - 10_800);

        insert_request(
            &conn,
            "req-a",
            "session-a",
            &one_hour_ago,
            "gpt-5.5",
            500_000,
            0,
            20,
            10,
        );
        insert_session(
            &conn,
            "session-a",
            &one_hour_ago,
            Some(&one_hour_ago),
            "gpt-5.5",
            None,
        );
        insert_request(
            &conn,
            "req-b",
            "session-b",
            &two_hours_ago,
            "gpt-5.4",
            625_000,
            0,
            0,
            30,
        );
        insert_session(
            &conn,
            "session-b",
            &two_hours_ago,
            Some(&two_hours_ago),
            "gpt-5.4",
            None,
        );
        insert_request(
            &conn,
            "req-c",
            "session-c",
            &three_hours_ago,
            "gpt-5.5",
            666_666,
            0,
            10,
            10,
        );
        insert_session(
            &conn,
            "session-c",
            &three_hours_ago,
            Some(&three_hours_ago),
            "gpt-5.5",
            None,
        );

        insert_diagnosis(
            &conn,
            "session-a",
            &one_hour_ago,
            true,
            r#"[{"cause_type":"codex_response_failed"},{"cause_type":"codex_high_context_fill"}]"#,
        );
        insert_diagnosis(
            &conn,
            "session-b",
            &two_hours_ago,
            true,
            r#"[{"cause_type":"codex_response_failed"}]"#,
        );
        insert_diagnosis(&conn, "session-c", &three_hours_ago, false, "[]");

        let windows = query_historical_metrics(&conn, now).expect("query history");
        let one_day = history_window(&windows, "1d");

        assert_eq!(one_day.sessions, 3);
        assert_eq!(one_day.degraded_sessions, 2);
        assert!((one_day.degraded_session_ratio - (2.0 / 3.0)).abs() < 1e-9);
        assert_eq!(
            one_day.degraded_causes.get("codex_response_failed"),
            Some(&2)
        );
        assert_eq!(
            one_day.degraded_causes.get("codex_high_context_fill"),
            Some(&1)
        );
    }

    #[test]
    fn historical_metrics_render_keeps_non_envoy_surfaces_out_of_prometheus() {
        let _guard = METRICS_TEST_LOCK.lock().unwrap();
        metrics::init();

        let mut causes = std::collections::BTreeMap::new();
        causes.insert("codex_high_context_fill", 3);
        metrics::update_historical_gauges(
            &[metrics::HistoricalWindowMetrics {
                window: "7d",
                sessions: 5,
                degraded_sessions: 2,
                degraded_session_ratio: 0.4,
                degraded_causes: causes,
                model_fallbacks: std::collections::BTreeMap::from([(("gpt-5.4", "gpt-5.5"), 1)]),
            }],
            1_776_700_000,
        );

        let (_, body) = metrics::render().expect("render metrics");
        assert!(body.contains(
            "codex_blackbox_model_fallback_total{actual=\"gpt-5.5\",requested=\"gpt-5.4\"} 0"
        ));
        for dropped_metric in [
            "codex_blackbox_history_",
            "codex_blackbox_cache_events_total",
            "codex_blackbox_estimated_",
            "codex_blackbox_tool_failures_total",
            "codex_blackbox_mcp_",
            "codex_blackbox_skill_events_total",
            "codex_blackbox_active_sessions",
            "codex_blackbox_weekly_tokens",
        ] {
            assert!(
                !body.contains(dropped_metric),
                "non-Envoy Codex metric family remained exposed: {dropped_metric}"
            );
        }
    }

    #[test]
    fn seed_live_metric_labels_from_db_precreates_envoy_tool_intent_series_only() {
        let _guard = METRICS_TEST_LOCK.lock().unwrap();
        metrics::init();

        let conn = Connection::open_in_memory().expect("open sqlite");
        conn.execute_batch(SCHEMA).expect("create schema");
        conn.execute(
            "INSERT INTO sessions (session_id, started_at, model) VALUES (?1, ?2, ?3)",
            rusqlite::params!["session_1", "2026-01-01T00:00:00Z", "gpt-5.5"],
        )
        .expect("insert session");
        conn.execute(
            "INSERT INTO requests (request_id, session_id, timestamp, model, provider) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                "req_1",
                "session_1",
                "2026-01-01T00:00:00Z",
                "gpt-5.5",
                "codex_responses"
            ],
        )
        .expect("insert request");
        conn.execute(
            "INSERT INTO tool_calls (request_id, timestamp, tool_name) VALUES (?1, ?2, ?3)",
            rusqlite::params!["req_1", "2026-01-01T00:00:00Z", "Bash"],
        )
        .expect("insert tool call");
        conn.execute(
            "INSERT INTO tool_calls (request_id, timestamp, tool_name) VALUES (?1, ?2, ?3)",
            rusqlite::params!["req_1", "2026-01-01T00:00:01Z", "mcp__github__get_issue"],
        )
        .expect("insert mcp tool call");

        seed_live_metric_labels_from_db(&conn).expect("seed tool labels");

        let (_, body) = metrics::render().expect("render metrics");
        assert!(body.contains("codex_blackbox_tool_calls_total{tool=\"bash\"} 0"));
        assert!(body.contains("codex_blackbox_tool_calls_total{tool=\"named_tool\"} 0"));
        assert!(!body.contains("mcp__github__get_issue"));
        for dropped_metric in [
            "codex_blackbox_tool_failures_total",
            "codex_blackbox_mcp_",
            "codex_blackbox_skill_events_total",
        ] {
            assert!(
                !body.contains(dropped_metric),
                "non-Envoy lifecycle metric remained exposed: {dropped_metric}"
            );
        }
    }

    #[test]
    fn tool_metric_path_records_only_custom_tool_intent() {
        let _guard = METRICS_TEST_LOCK.lock().unwrap();
        metrics::init();

        metrics::record_tool_call("mcp__metricstest_server__lookup_widget");
        metrics::record_tool_call("custom_tool_call:provider_generated_item_123");
        metrics::record_tool_call("ProviderGeneratedToolName");

        let (_, body) = metrics::render().expect("render metrics");
        assert!(body.contains("codex_blackbox_tool_calls_total{tool=\"custom_tool_call\"}"));
        assert!(body.contains("codex_blackbox_tool_calls_total{tool=\"named_tool\"}"));
        assert!(!body.contains("metricstest_server"));
        assert!(!body.contains("provider_generated_item_123"));
        assert!(!body.contains("ProviderGeneratedToolName"));
        assert!(!body.contains("codex_blackbox_tool_failures_total"));
        assert!(!body.contains("codex_blackbox_mcp_"));
    }

    #[test]
    fn historical_gauge_refresh_does_not_reintroduce_dropped_metric_families() {
        let _guard = METRICS_TEST_LOCK.lock().unwrap();
        metrics::init();

        let mut causes = std::collections::BTreeMap::new();
        causes.insert("codex_response_failed", 2);
        metrics::update_historical_gauges(
            &[metrics::HistoricalWindowMetrics {
                window: "7d",
                sessions: 2,
                degraded_sessions: 1,
                degraded_session_ratio: 0.5,
                degraded_causes: causes,
                model_fallbacks: std::collections::BTreeMap::from([(("gpt-5.4", "gpt-5.5"), 1)]),
            }],
            1_776_700_000,
        );

        metrics::update_historical_gauges(
            &[metrics::HistoricalWindowMetrics {
                window: "7d",
                sessions: 1,
                degraded_sessions: 0,
                degraded_session_ratio: 0.0,
                degraded_causes: std::collections::BTreeMap::new(),
                model_fallbacks: std::collections::BTreeMap::new(),
            }],
            1_776_700_100,
        );

        let (_, body) = metrics::render().expect("render metrics");
        assert!(!body.contains("codex_blackbox_history_"));
        assert!(!body.contains("codex_blackbox_estimated_"));
        assert!(!body.contains("codex_blackbox_cache_events_total"));
        assert!(!body.contains("codex_blackbox_tool_failures_total"));
    }

    #[test]
    fn summary_response_exposes_local_estimate_cost_fields_with_compatibility_aliases() {
        let today = SummaryWindowData {
            sessions: 2,
            estimated_cost_dollars: 12.345,
            cost_source: "pricing_file:test-contract".to_string(),
            trusted_for_budget_enforcement: true,
            billed_cost_dollars: Some(10.25),
            billed_sessions: 1,
            codex_cached_input: CodexCachedInputSummary {
                input_tokens: 100,
                cached_input_tokens: 50,
            },
        };
        let week = SummaryWindowData {
            sessions: 3,
            estimated_cost_dollars: 20.0,
            cost_source: pricing::MIXED_COST_SOURCE.to_string(),
            trusted_for_budget_enforcement: false,
            billed_cost_dollars: None,
            billed_sessions: 0,
            codex_cached_input: CodexCachedInputSummary::default(),
        };
        let month = SummaryWindowData {
            sessions: 4,
            estimated_cost_dollars: 30.0,
            cost_source: ESTIMATED_COST_SOURCE.to_string(),
            trusted_for_budget_enforcement: false,
            billed_cost_dollars: Some(18.0),
            billed_sessions: 2,
            codex_cached_input: CodexCachedInputSummary {
                input_tokens: 200,
                cached_input_tokens: 150,
            },
        };
        let expected_source = pricing::active_catalog_source();
        let json = build_summary_response_json(&today, &week, &month);
        assert_eq!(
            json.get("local_estimate_cost_source")
                .and_then(|v| v.as_str()),
            Some(expected_source.as_str())
        );
        assert_eq!(
            json.get("local_estimate_trusted_for_budget_enforcement")
                .and_then(|v| v.as_bool()),
            Some(pricing::trusted_for_budget_enforcement())
        );
        assert_eq!(
            json.get("cost_source").and_then(|v| v.as_str()),
            Some(expected_source.as_str())
        );
        assert_eq!(
            json.get("trusted_for_budget_enforcement")
                .and_then(|v| v.as_bool()),
            Some(pricing::trusted_for_budget_enforcement())
        );
        let today = json.get("today").expect("today");
        assert_eq!(
            today
                .get("local_estimate_cost_dollars")
                .and_then(|v| v.as_f64()),
            Some(12.35)
        );
        assert_eq!(
            today
                .get("local_estimate_cost_source")
                .and_then(|v| v.as_str()),
            Some("pricing_file:test-contract")
        );
        assert_eq!(
            today
                .get("local_estimate_trusted_for_budget_enforcement")
                .and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            today.get("estimated_cost_dollars").and_then(|v| v.as_f64()),
            Some(12.35)
        );
        assert_eq!(
            today.get("billed_cost_dollars").and_then(|v| v.as_f64()),
            Some(10.25)
        );
        assert_eq!(
            today.get("billed_sessions").and_then(|v| v.as_u64()),
            Some(1)
        );
        assert_eq!(
            today.get("cost_source").and_then(|v| v.as_str()),
            Some("pricing_file:test-contract")
        );
        assert_eq!(
            today.get("codex_input_tokens").and_then(|v| v.as_u64()),
            Some(100)
        );
        assert_eq!(
            today
                .get("codex_cached_input_tokens")
                .and_then(|v| v.as_u64()),
            Some(50)
        );
        assert_eq!(
            today
                .get("codex_cached_input_ratio")
                .and_then(|v| v.as_f64()),
            Some(0.5)
        );
        assert!(today.get("cache_hit_ratio").is_none());
        assert!(today.get("cost").is_none());
    }

    #[test]
    fn diagnosis_response_exposes_local_estimate_cost_fields_with_compatibility_aliases() {
        let json = build_diagnosis_response_json(
            "session_1".to_string(),
            "2026-01-01T00:00:00Z".to_string(),
            "Completed".to_string(),
            4,
            1.25,
            "pricing_file:test-contract".to_string(),
            true,
            Some(0.98),
            Some("invoice_2026q2".to_string()),
            Some("2026-01-02T00:00:00Z".to_string()),
            CodexCachedInputSummary {
                input_tokens: 100,
                cached_input_tokens: 40,
            },
            true,
            Some(2),
            serde_json::json!([]),
            serde_json::json!(["Retry less"]),
        );
        assert_eq!(
            json.get("local_estimate_total_cost_dollars")
                .and_then(|v| v.as_f64()),
            Some(1.25)
        );
        assert_eq!(
            json.get("local_estimate_cost_source")
                .and_then(|v| v.as_str()),
            Some("pricing_file:test-contract")
        );
        assert_eq!(
            json.get("local_estimate_trusted_for_budget_enforcement")
                .and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            json.get("estimated_total_cost_dollars")
                .and_then(|v| v.as_f64()),
            Some(1.25)
        );
        assert_eq!(
            json.get("cost_source").and_then(|v| v.as_str()),
            Some("pricing_file:test-contract")
        );
        assert_eq!(
            json.get("trusted_for_budget_enforcement")
                .and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            json.get("billed_cost_dollars").and_then(|v| v.as_f64()),
            Some(0.98)
        );
        assert_eq!(
            json.get("billing_source").and_then(|v| v.as_str()),
            Some("invoice_2026q2")
        );
        assert_eq!(
            json.get("codex_cached_input_ratio")
                .and_then(|v| v.as_f64()),
            Some(0.4)
        );
        assert!(json.get("cache_hit_ratio").is_none());
        assert!(json.get("total_cost_dollars").is_none());
    }

    #[test]
    fn diagnosis_payload_filter_keeps_codex_causes_and_public_model_names() {
        let causes = serde_json::json!([
            {
                "cause_type": "codex_model_mismatch",
                "turn_first_noticed": 2,
                "requested_model": "gpt-5.5",
                "actual_model": "gpt-5.4"
            },
            {
                "cause_type": "cache_miss_ttl",
                "turn_first_noticed": 3,
                "actual_model": "legacy-cache-model"
            }
        ]);

        let (causes, advice) =
            filter_codex_envoy_diagnosis_payload(causes, serde_json::json!(["legacy advice"]));

        assert_eq!(advice, Value::Array(vec![]));
        let causes = causes.as_array().expect("filtered causes");
        assert_eq!(causes.len(), 1);
        assert_eq!(
            causes[0].get("cause_type").and_then(Value::as_str),
            Some("codex_model_mismatch")
        );
        assert_eq!(
            causes[0].get("served_model").and_then(Value::as_str),
            Some("gpt-5.4")
        );
        assert!(causes[0].get("actual_model").is_none());
    }

    #[test]
    fn session_summary_exposes_local_estimate_cost_fields_with_compatibility_aliases() {
        let json = build_session_summary_json(
            "session_1".to_string(),
            "codex-blackbox".to_string(),
            Some("2026-01-01T00:00:00Z".to_string()),
            "Completed".to_string(),
            false,
            4,
            1.25,
            "pricing_file:test-contract".to_string(),
            true,
            Some(0.98),
            Some("invoice_2026q2".to_string()),
            Some("2026-01-02T00:00:00Z".to_string()),
            "codex_response_failed".to_string(),
            CodexCachedInputSummary {
                input_tokens: 100,
                cached_input_tokens: 40,
            },
            Some("gpt-5.5".to_string()),
            Some("gpt-5.5".to_string()),
            Some("gpt-5.5".to_string()),
        );
        assert_eq!(
            json.get("local_estimate_total_cost_dollars")
                .and_then(|v| v.as_f64()),
            Some(1.25)
        );
        assert_eq!(
            json.get("local_estimate_cost_source")
                .and_then(|v| v.as_str()),
            Some("pricing_file:test-contract")
        );
        assert_eq!(
            json.get("local_estimate_trusted_for_budget_enforcement")
                .and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            json.get("estimated_total_cost_dollars")
                .and_then(|v| v.as_f64()),
            Some(1.25)
        );
        assert_eq!(
            json.get("cost_source").and_then(|v| v.as_str()),
            Some("pricing_file:test-contract")
        );
        assert_eq!(
            json.get("billed_cost_dollars").and_then(|v| v.as_f64()),
            Some(0.98)
        );
        assert_eq!(
            json.get("display_name").and_then(|v| v.as_str()),
            Some("codex-blackbox")
        );
        assert_eq!(
            json.get("requested_model").and_then(|v| v.as_str()),
            Some("gpt-5.5")
        );
        assert_eq!(
            json.get("served_model").and_then(|v| v.as_str()),
            Some("gpt-5.5")
        );
        assert_eq!(
            json.get("codex_cached_input_ratio")
                .and_then(|v| v.as_f64()),
            Some(0.4)
        );
        assert!(json.get("cache_hit_ratio").is_none());
        assert!(json.get("total_cost_dollars").is_none());
    }

    #[test]
    fn sessions_response_exposes_local_estimate_source_at_root() {
        let sessions = vec![build_session_summary_json(
            "session_1".to_string(),
            "codex-blackbox".to_string(),
            Some("2026-01-01T00:00:00Z".to_string()),
            "Completed".to_string(),
            false,
            4,
            1.25,
            ESTIMATED_COST_SOURCE.to_string(),
            false,
            Some(1.10),
            Some("invoice_2026q2".to_string()),
            Some("2026-01-02T00:00:00Z".to_string()),
            "codex_response_failed".to_string(),
            CodexCachedInputSummary {
                input_tokens: 100,
                cached_input_tokens: 40,
            },
            Some("gpt-5.5".to_string()),
            None,
            None,
        )];
        let json = build_sessions_response_json(sessions);

        let expected_source = pricing::active_catalog_source();
        assert_eq!(
            json.get("local_estimate_cost_source")
                .and_then(|v| v.as_str()),
            Some(expected_source.as_str())
        );
        assert_eq!(
            json.get("local_estimate_trusted_for_budget_enforcement")
                .and_then(|v| v.as_bool()),
            Some(pricing::trusted_for_budget_enforcement())
        );
        assert_eq!(
            json.get("cost_source").and_then(|v| v.as_str()),
            Some(expected_source.as_str())
        );
        assert_eq!(
            json.get("trusted_for_budget_enforcement")
                .and_then(|v| v.as_bool()),
            Some(pricing::trusted_for_budget_enforcement())
        );
        assert_eq!(
            json.get("sessions")
                .and_then(|v| v.as_array())
                .map(|v| v.len()),
            Some(1)
        );
        assert!(json.get("cost").is_none());
    }

    #[test]
    fn recent_sessions_query_requires_codex_backed_evidence() {
        let conn = create_full_test_db();
        let since = "2026-01-01T00:00:00Z";
        insert_session(
            &conn,
            "legacy-session",
            "2026-01-01T00:00:00Z",
            Some("2026-01-01T00:00:01Z"),
            "gpt-5.5",
            None,
        );
        insert_request(
            &conn,
            "legacy-req",
            "legacy-session",
            "2026-01-01T00:00:00Z",
            "gpt-5.5",
            1000,
            100,
            0,
            1000,
        );
        insert_session(
            &conn,
            "codex-session",
            "2026-01-01T00:01:00Z",
            Some("2026-01-01T00:01:01Z"),
            "gpt-5.5",
            None,
        );
        insert_codex_request(
            &conn,
            "codex-req",
            "codex-session",
            "2026-01-01T00:01:00Z",
            "gpt-5.5",
            10_000,
            4_000,
            800,
        );

        let rows = load_recent_codex_session_rows(&conn, since, 20).expect("load recent sessions");
        let session_ids = rows
            .iter()
            .map(|row| row.session_id.as_str())
            .collect::<Vec<_>>();

        assert_eq!(session_ids, vec!["codex-session"]);
        assert!(!session_has_codex_evidence(&conn, "legacy-session").expect("legacy evidence"));
        assert!(session_has_codex_evidence(&conn, "codex-session").expect("codex evidence"));
    }

    #[test]
    fn codex_observation_snapshot_is_rowid_session_and_prompt_scoped() {
        let conn = create_full_test_db();
        insert_session(
            &conn,
            "session-observed-a",
            "2026-01-01T00:00:00Z",
            Some("2026-01-01T00:00:01Z"),
            "gpt-5.5",
            None,
        );
        insert_codex_request(
            &conn,
            "req-observed-a",
            "session-observed-a",
            "2026-01-01T00:00:00Z",
            "gpt-5.5",
            100,
            20,
            10,
        );
        conn.execute(
            "UPDATE requests SET codex_prompt_excerpt = 'first prompt' WHERE request_id = 'req-observed-a'",
            [],
        )
        .expect("set first prompt");
        let before = load_codex_observation_snapshot(&conn, 0, None, None)
            .expect("initial observation snapshot");

        insert_session(
            &conn,
            "session-observed-b",
            "2026-01-01T00:00:02Z",
            Some("2026-01-01T00:00:03Z"),
            "gpt-5.5",
            None,
        );
        insert_codex_request(
            &conn,
            "req-observed-b",
            "session-observed-b",
            "2026-01-01T00:00:02Z",
            "gpt-5.5",
            200,
            40,
            20,
        );
        conn.execute(
            "UPDATE requests SET codex_prompt_excerpt = 'second prompt' WHERE request_id = 'req-observed-b'",
            [],
        )
        .expect("set second prompt");

        let unrelated = load_codex_observation_snapshot(
            &conn,
            before.latest_request_rowid,
            None,
            Some("first prompt"),
        )
        .expect("unrelated prompt snapshot");
        assert_eq!(unrelated.request_count, 1);
        assert_eq!(unrelated.matching_request_count, 0);
        assert!(!unrelated.matched);

        let matched = load_codex_observation_snapshot(
            &conn,
            before.latest_request_rowid,
            None,
            Some("second prompt"),
        )
        .expect("matched prompt snapshot");
        assert_eq!(matched.request_count, 1);
        assert_eq!(matched.matching_request_count, 1);
        assert!(matched.matched);

        let wrong_session = load_codex_observation_snapshot(
            &conn,
            before.latest_request_rowid,
            Some("session-observed-a"),
            Some("second prompt"),
        )
        .expect("wrong session snapshot");
        assert_eq!(wrong_session.request_count, 1);
        assert_eq!(wrong_session.matching_request_count, 0);
        assert!(!wrong_session.matched);

        let matched_session = load_codex_observation_snapshot(
            &conn,
            before.latest_request_rowid,
            Some("session-observed-b"),
            Some("first prompt"),
        )
        .expect("matched session snapshot");
        assert_eq!(matched_session.request_count, 1);
        assert_eq!(matched_session.matching_request_count, 1);
        assert!(matched_session.matched);
    }

    #[test]
    fn recent_sessions_model_comes_from_codex_evidence_not_session_row() {
        let conn = create_full_test_db();
        let since = "2026-01-01T00:00:00Z";
        insert_session(
            &conn,
            "codex-session-stale-model",
            "2026-01-01T00:01:00Z",
            Some("2026-01-01T00:01:01Z"),
            "stale-session-model",
            None,
        );
        insert_codex_request(
            &conn,
            "codex-req-model",
            "codex-session-stale-model",
            "2026-01-01T00:01:00Z",
            "gpt-codex-requested",
            10_000,
            4_000,
            800,
        );
        conn.execute(
            "UPDATE requests SET served_model = 'gpt-codex-served' WHERE request_id = 'codex-req-model'",
            [],
        )
        .expect("mark served model");

        let rows = load_recent_codex_session_rows(&conn, since, 20).expect("load recent sessions");
        let row = rows
            .iter()
            .find(|row| row.session_id == "codex-session-stale-model")
            .expect("codex session row");

        assert_eq!(row.model.as_deref(), Some("gpt-codex-served"));
        assert_eq!(row.requested_model.as_deref(), Some("gpt-codex-requested"));
        assert_eq!(row.served_model.as_deref(), Some("gpt-codex-served"));
    }

    #[test]
    fn query_summary_uses_latest_billing_reconciliation_per_session() {
        let conn = create_history_test_db();
        let now = 1_776_700_000;
        let one_hour_ago = epoch_to_iso8601(now - 3_600);
        insert_session(
            &conn,
            "session-a",
            &one_hour_ago,
            Some(&one_hour_ago),
            "gpt-5.5",
            None,
        );
        insert_session(
            &conn,
            "session-b",
            &one_hour_ago,
            Some(&one_hour_ago),
            "gpt-5.4",
            None,
        );
        insert_codex_request(
            &conn,
            "req-a",
            "session-a",
            &one_hour_ago,
            "gpt-5.5",
            100_000,
            0,
            0,
        );
        insert_codex_request(
            &conn,
            "req-b",
            "session-b",
            &one_hour_ago,
            "gpt-5.4",
            100_000,
            0,
            0,
        );
        insert_billing_reconciliation(
            &conn,
            "session-a",
            "2026-01-01T00:00:00Z",
            "invoice_old",
            2.0,
        );
        insert_billing_reconciliation(
            &conn,
            "session-a",
            "2026-01-03T00:00:00Z",
            "invoice_new",
            2.5,
        );
        insert_billing_reconciliation(
            &conn,
            "session-b",
            "2026-01-02T00:00:00Z",
            "invoice_b",
            1.25,
        );

        let summary = query_summary(&conn, &epoch_to_iso8601(now - 86_400)).expect("summary");
        assert_eq!(summary.sessions, 2);
        assert_eq!(summary.billed_sessions, 2);
        assert_eq!(summary.billed_cost_dollars, Some(3.75));
        assert!((summary.estimated_cost_dollars - 0.75).abs() < 1e-9);
    }

    #[test]
    fn query_summary_excludes_non_codex_and_internal_request_rows() {
        let conn = create_history_test_db();
        let now = 1_776_700_000;
        let one_hour_ago = epoch_to_iso8601(now - 3_600);
        insert_session(
            &conn,
            "session-real",
            &one_hour_ago,
            Some(&one_hour_ago),
            "gpt-5.5",
            None,
        );
        insert_codex_request(
            &conn,
            "req-real",
            "session-real",
            &one_hour_ago,
            "gpt-5.5",
            100_000,
            0,
            0,
        );
        insert_session(
            &conn,
            "session-legacy",
            &one_hour_ago,
            Some(&one_hour_ago),
            "gpt-5.4",
            None,
        );
        insert_request(
            &conn,
            "req-legacy",
            "session-legacy",
            &one_hour_ago,
            "gpt-5.4",
            500_000,
            0,
            0,
            0,
        );
        insert_request(
            &conn,
            "req-title",
            "session-internal",
            &one_hour_ago,
            "gpt-5.4",
            500_000,
            0,
            0,
            0,
        );

        let summary = query_summary(&conn, &epoch_to_iso8601(now - 86_400)).expect("summary");
        assert_eq!(summary.sessions, 1);
        assert!((summary.estimated_cost_dollars - 0.5).abs() < 1e-9);
    }

    #[test]
    fn session_cost_estimates_ignore_non_codex_request_rows() {
        let conn = create_history_test_db();
        let now = 1_776_700_000;
        let one_hour_ago = epoch_to_iso8601(now - 3_600);
        insert_session(
            &conn,
            "session-mixed",
            &one_hour_ago,
            Some(&one_hour_ago),
            "gpt-5.5",
            None,
        );
        insert_request(
            &conn,
            "req-legacy-cost",
            "session-mixed",
            &one_hour_ago,
            "gpt-5.4",
            10_000_000,
            10_000_000,
            0,
            0,
        );
        conn.execute(
            "UPDATE requests SET cost_dollars = 99.0, cost_source = 'legacy-cost-source' \
             WHERE request_id = 'req-legacy-cost'",
            [],
        )
        .expect("update legacy cost");
        insert_codex_request(
            &conn,
            "req-codex-cost",
            "session-mixed",
            &one_hour_ago,
            "gpt-5.5",
            100_000,
            0,
            0,
        );

        let estimates = compute_estimated_costs_for_sessions(&conn, &["session-mixed".to_string()])
            .expect("compute cost estimates");
        let estimate = estimates
            .get("session-mixed")
            .expect("mixed session estimate");

        assert!((estimate.estimated_cost_dollars - 0.5).abs() < 1e-9);
        assert_ne!(estimate.cost_source, "legacy-cost-source");
    }

    #[test]
    fn query_summary_reports_codex_cached_input_not_legacy_cache_ratio() {
        let conn = create_history_test_db();
        let now = 1_776_700_000;
        let one_hour_ago = epoch_to_iso8601(now - 3_600);
        insert_session(
            &conn,
            "session-codex",
            &one_hour_ago,
            Some(&one_hour_ago),
            "gpt-5.5",
            None,
        );
        insert_request(
            &conn,
            "req-legacy-cache",
            "session-codex",
            &one_hour_ago,
            "gpt-5.5",
            1_000,
            10,
            9_000,
            1_000,
        );
        insert_codex_request(
            &conn,
            "req-codex",
            "session-codex",
            &one_hour_ago,
            "gpt-5.5",
            10_000,
            4_000,
            800,
        );

        let summary = query_summary(&conn, &epoch_to_iso8601(now - 86_400)).expect("summary");

        assert_eq!(summary.codex_cached_input.input_tokens, 10_000);
        assert_eq!(summary.codex_cached_input.cached_input_tokens, 4_000);
        assert_eq!(summary.codex_cached_input.cached_input_ratio(), Some(0.4));
    }

    #[test]
    fn repair_persisted_session_artifacts_reaches_older_missing_rows() {
        let conn = create_full_test_db();
        let cutoff_base = now_epoch_secs().saturating_sub(session_timeout_secs() + 3_600);

        for idx in 0..205u64 {
            let ts = epoch_to_iso8601(cutoff_base.saturating_sub(205 - idx));
            let session_id = format!("session-{idx:03}");
            insert_session(&conn, &session_id, &ts, Some(&ts), "gpt-5.5", None);
            insert_codex_turn_snapshot(&conn, &session_id, "completed", 1);
        }

        repair_persisted_session_artifacts(&conn).expect("first repair");
        let repaired_after_first: i64 = conn
            .query_row("SELECT COUNT(*) FROM session_diagnoses", [], |row| {
                row.get(0)
            })
            .expect("count diagnoses after first repair");
        assert_eq!(repaired_after_first, 200);

        repair_persisted_session_artifacts(&conn).expect("second repair");
        let repaired_after_second: i64 = conn
            .query_row("SELECT COUNT(*) FROM session_diagnoses", [], |row| {
                row.get(0)
            })
            .expect("count diagnoses after second repair");
        assert_eq!(repaired_after_second, 205);
    }

    #[test]
    fn repair_session_recall_ignores_non_codex_response_summaries() {
        let conn = create_full_test_db();
        let ended_at =
            epoch_to_iso8601(now_epoch_secs().saturating_sub(session_timeout_secs() + 3_600));
        insert_session(
            &conn,
            "session-recall-mixed",
            &ended_at,
            Some(&ended_at),
            "gpt-5.5",
            None,
        );
        insert_codex_turn_snapshot(&conn, "session-recall-mixed", "completed", 1);
        insert_turn_snapshot(
            &conn,
            "session-recall-mixed",
            2,
            &ended_at,
            0,
            0,
            100,
            0.0,
            0.10,
            Some("non-codex summary must not become Codex recall"),
        );

        repair_persisted_session_artifacts(&conn).expect("repair persisted artifacts");

        let recall_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM session_recall WHERE session_id = 'session-recall-mixed'",
                [],
                |row| row.get(0),
            )
            .expect("count recall rows");
        assert_eq!(recall_rows, 0);
    }

    #[test]
    fn repair_session_recall_rewrites_existing_summary_from_codex_turns() {
        let conn = create_full_test_db();
        let ended_at =
            epoch_to_iso8601(now_epoch_secs().saturating_sub(session_timeout_secs() + 3_600));
        insert_session(
            &conn,
            "session-recall-polluted",
            &ended_at,
            Some(&ended_at),
            "gpt-5.5",
            None,
        );
        insert_codex_turn_snapshot(&conn, "session-recall-polluted", "completed", 1);
        conn.execute(
            "UPDATE turn_snapshots \
             SET response_summary = 'codex summary' \
             WHERE session_id = 'session-recall-polluted' AND turn_number = 1",
            [],
        )
        .expect("set codex summary");
        insert_turn_snapshot(
            &conn,
            "session-recall-polluted",
            2,
            &ended_at,
            0,
            0,
            100,
            0.0,
            0.10,
            Some("newer non-codex summary"),
        );
        conn.execute(
            "INSERT INTO session_recall (session_id, initial_prompt, final_response_summary) \
             VALUES ('session-recall-polluted', '', 'newer non-codex summary')",
            [],
        )
        .expect("insert polluted recall row");

        repair_persisted_session_artifacts(&conn).expect("repair persisted artifacts");

        let summary: String = conn
            .query_row(
                "SELECT final_response_summary FROM session_recall \
                 WHERE session_id = 'session-recall-polluted'",
                [],
                |row| row.get(0),
            )
            .expect("load repaired recall summary");
        assert_eq!(summary, "codex summary");
    }

    #[test]
    fn repair_turn_snapshot_context_windows_backfills_oversized_turns_as_1m() {
        let conn = create_full_test_db();
        insert_session(
            &conn,
            "session-long",
            "2026-04-23T06:00:00Z",
            Some("2026-04-23T06:00:00Z"),
            "gpt-5.5",
            None,
        );
        insert_turn_snapshot(
            &conn,
            "session-long",
            1,
            "2026-04-23T06:00:00Z",
            250_000,
            0,
            1000,
            0.0,
            1.255,
            None,
        );

        repair_turn_snapshot_context_windows(&conn).expect("repair turn snapshots");

        let (context_window_tokens, context_utilization): (i64, f64) = conn
            .query_row(
                "SELECT context_window_tokens, context_utilization \
                 FROM turn_snapshots WHERE session_id = 'session-long'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("load repaired row");

        assert_eq!(context_window_tokens, EXTENDED_CONTEXT_WINDOW_TOKENS as i64);
        assert!((context_utilization - 0.251).abs() < 0.0001);
    }

    #[test]
    fn diagnosis_and_degradation_agree_for_codex_heuristic_only_session() {
        let conn = create_full_test_db();
        let session_id = "019df69d-3306-71e0-b71b-fbc3a81b3f70";
        let ended_at =
            epoch_to_iso8601(now_epoch_secs().saturating_sub(session_timeout_secs() + 3_600));
        insert_session(
            &conn,
            session_id,
            &ended_at,
            Some(&ended_at),
            "gpt-5.5",
            Some("dummy repo validation"),
        );
        insert_codex_degradation_turn_snapshot(
            &conn,
            session_id,
            1,
            &ended_at,
            "completed",
            10_000,
            4_000,
            120,
            80,
            0.20,
            "[]",
        );

        let estimated = CostAccumulator::new().finish();
        let (_, report) = build_fresh_diagnosis_report(&conn, session_id, &estimated)
            .expect("fresh diagnosis")
            .expect("diagnosis report");
        assert!(!report.degraded);
        assert_eq!(report.degradation_turn, None);
        assert!(report
            .causes
            .iter()
            .any(|cause| cause.cause_type == "codex_high_reasoning_share"));

        let diagnosis_row = conn
            .query_row(
                "SELECT degraded, degradation_turn FROM session_diagnoses WHERE session_id = ?1",
                rusqlite::params![session_id],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<i64>>(1)?)),
            )
            .expect("load persisted diagnosis");
        assert_eq!(diagnosis_row, (0, None));

        let degradation =
            load_degradation_view_from_db(&conn, session_id).expect("degradation view");
        assert_eq!(
            degradation.get("degraded").and_then(Value::as_bool),
            Some(false)
        );
        assert!(degradation
            .get("degradation_turn")
            .expect("degradation turn")
            .is_null());
        let turn = degradation
            .get("turns")
            .and_then(Value::as_array)
            .and_then(|turns| turns.first())
            .expect("first turn");
        assert!(turn
            .get("flags")
            .and_then(Value::as_array)
            .expect("degradation flags")
            .is_empty());
        assert!(turn
            .get("heuristic_signals")
            .and_then(Value::as_array)
            .expect("heuristic signals")
            .contains(&Value::String("codex_high_reasoning_share".to_string())));
    }

    #[test]
    fn degradation_view_renders_only_codex_envoy_fields() {
        let conn = create_full_test_db();
        let ended_at =
            epoch_to_iso8601(now_epoch_secs().saturating_sub(session_timeout_secs() + 3_600));
        insert_session(
            &conn,
            "session-codex",
            &ended_at,
            Some(&ended_at),
            "gpt-5.5",
            Some("ship the fix"),
        );
        insert_turn_snapshot(
            &conn,
            "session-codex",
            1,
            &ended_at,
            0,
            1000,
            1000,
            0.0,
            0.10,
            None,
        );
        conn.execute(
            "UPDATE turn_snapshots \
             SET codex_status = 'failed', codex_cached_input_tokens = 900, \
                 codex_uncached_input_tokens = 100, codex_reasoning_output_tokens = 100, \
                 codex_accounting_anomalies = '[{\"type\":\"unscoped_codex_looking_row\"}]' \
             WHERE session_id = 'session-codex' AND turn_number = 1",
            [],
        )
        .expect("mark non-Codex row as Codex-looking");
        insert_codex_degradation_turn_snapshot(
            &conn,
            "session-codex",
            2,
            &ended_at,
            "failed",
            10_000,
            4_000,
            800,
            500,
            0.85,
            r#"[{"type":"reported_total_tokens_mismatch"}]"#,
        );
        insert_codex_degradation_turn_snapshot(
            &conn,
            "session-codex",
            3,
            &ended_at,
            "completed",
            9_000,
            3_000,
            700,
            0,
            0.50,
            "[]",
        );

        let json = load_degradation_view_from_db(&conn, "session-codex").expect("degradation view");
        assert_eq!(json.get("degraded").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(
            json.get("degradation_turn").and_then(|v| v.as_i64()),
            Some(2)
        );
        assert_eq!(json.get("total_turns").and_then(|v| v.as_u64()), Some(2));
        let turns = json
            .get("turns")
            .and_then(Value::as_array)
            .expect("turn array");
        assert!(turns.iter().all(|turn| {
            turn.get("cache_read_tokens").is_none()
                && turn.get("cache_creation_tokens").is_none()
                && turn.get("tool_failures").is_none()
                && turn.get("gap_from_prev_secs").is_none()
                && turn.get("actual_model").is_none()
                && turn.get("served_model").is_some()
        }));
        let flags = turns[0]
            .get("flags")
            .and_then(Value::as_array)
            .expect("flags");
        assert!(flags.contains(&Value::String("codex_response_failed".to_string())));
        assert!(flags.contains(&Value::String("codex_accounting_anomaly".to_string())));
        let heuristic_signals = turns[0]
            .get("heuristic_signals")
            .and_then(Value::as_array)
            .expect("heuristic signals");
        assert!(heuristic_signals.contains(&Value::String("codex_high_context_fill".to_string())));
        assert!(
            heuristic_signals.contains(&Value::String("codex_high_reasoning_share".to_string()))
        );
    }

    #[test]
    fn repair_clears_degradation_turn_for_non_degraded_sessions() {
        let conn = create_full_test_db();
        let ended_at =
            epoch_to_iso8601(now_epoch_secs().saturating_sub(session_timeout_secs() + 3_600));
        insert_session(
            &conn,
            "session-ok",
            &ended_at,
            Some(&ended_at),
            "gpt-5.5",
            Some("ship the fix"),
        );
        conn.execute(
            "INSERT INTO session_diagnoses (
                session_id, completed_at, outcome, total_turns, total_cost,
                degraded, degradation_turn, causes_json, advice_json
            ) VALUES (?1, ?2, 'Likely Completed', 4, 1.5, 0, 8, '[]', '[]')",
            rusqlite::params!["session-ok", ended_at],
        )
        .expect("insert inconsistent diagnosis");

        repair_persisted_session_artifacts(&conn).expect("repair persisted artifacts");

        let stored_turn = conn
            .query_row(
                "SELECT degradation_turn FROM session_diagnoses WHERE session_id = 'session-ok'",
                [],
                |row| row.get::<_, Option<i64>>(0),
            )
            .expect("load repaired diagnosis turn");
        assert_eq!(stored_turn, None);

        let json = load_degradation_view_from_db(&conn, "session-ok")
            .expect("degradation view after repair");
        assert_eq!(json.get("degraded").and_then(|v| v.as_bool()), Some(false));
        assert!(json.get("degradation_turn").is_some());
        assert!(json.get("degradation_turn").unwrap().is_null());
    }

    #[tokio::test]
    async fn persist_billing_reconciliation_waits_for_db_ack() {
        let path = unique_test_db_path("billing-ack");
        let (tx, rx) = std::sync::mpsc::channel();
        let writer_path = path.clone();
        let handle = std::thread::spawn(move || db_writer_loop(&writer_path, rx));

        tx.send(DbCommand::InsertSession {
            session_id: "session-test".to_string(),
            started_at: "2026-01-01T00:00:00Z".to_string(),
            model: "gpt-5.5".to_string(),
            display_name: "session-test".to_string(),
            initial_prompt: None,
        })
        .expect("queue session insert");

        persist_billing_reconciliation(
            &tx,
            BillingReconciliationInput {
                session_id: "session-test".to_string(),
                source: "invoice_q1".to_string(),
                billed_cost_dollars: 1.23,
                imported_at: Some("2026-01-02T00:00:00Z".to_string()),
            },
        )
        .await
        .expect("persist billing reconciliation");

        drop(tx);
        handle.join().expect("join db writer");

        let conn = Connection::open(&path).expect("open test db");
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM billing_reconciliations WHERE session_id = ?1",
                rusqlite::params!["session-test"],
                |row| row.get(0),
            )
            .expect("count billing rows");
        assert_eq!(count, 1);

        cleanup_test_db(&path);
    }

    #[tokio::test]
    async fn persist_billing_reconciliation_rejects_unknown_sessions() {
        let path = unique_test_db_path("billing-missing-session");
        let (tx, rx) = std::sync::mpsc::channel();
        let writer_path = path.clone();
        let handle = std::thread::spawn(move || db_writer_loop(&writer_path, rx));

        let err = persist_billing_reconciliation(
            &tx,
            BillingReconciliationInput {
                session_id: "session-missing".to_string(),
                source: "invoice_q1".to_string(),
                billed_cost_dollars: 4.56,
                imported_at: Some("2026-01-02T00:00:00Z".to_string()),
            },
        )
        .await
        .expect_err("missing session should fail");

        assert_eq!(
            err,
            BillingReconciliationWriteError::UnknownSession("session-missing".to_string())
        );

        drop(tx);
        handle.join().expect("join db writer");

        let conn = Connection::open(&path).expect("open test db");
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM billing_reconciliations", [], |row| {
                row.get(0)
            })
            .expect("count billing rows");
        assert_eq!(count, 0);

        cleanup_test_db(&path);
    }
}
