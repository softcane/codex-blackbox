use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::mpsc as std_mpsc;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use rusqlite::{Connection, OptionalExtension};
use serde::Deserialize;
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
pub mod diagnosis;
pub mod metrics;
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
    estimated_cache_waste_dollars: f64,
}

static RUNTIME_STATE: LazyLock<Mutex<RuntimeState>> =
    LazyLock::new(|| Mutex::new(RuntimeState::new()));
static SESSION_BUDGETS: LazyLock<DashMap<String, SessionBudgetState>> = LazyLock::new(DashMap::new);

/// Check if request should be blocked by the process-wide circuit breaker.
fn check_circuit_breaker() -> Option<(&'static str, String)> {
    let runtime = RUNTIME_STATE.lock().unwrap();

    // Circuit breaker check.
    if let Some(until) = runtime.circuit_open_until {
        if Instant::now() < until {
            let remaining = until.duration_since(Instant::now()).as_secs();
            return Some((
                "circuit_breaker",
                format!(
                    "Coditor: circuit breaker open. {} consecutive errors detected. \
                 Pausing requests for {}s to prevent runaway estimated cost.",
                    runtime.consecutive_errors, remaining
                ),
            ));
        }
    }

    None
}

/// Check if the current session has exceeded its configured budget.
fn check_session_budget(session_id: Option<&str>) -> Option<(&'static str, String)> {
    let session_id = session_id?;
    let state = SESSION_BUDGETS.get(session_id)?;

    // Estimated-dollar budget check. These dollars are only enforced when the
    // active price catalog is explicitly marked trusted for enforcement.
    let budget = env_f64("CODITOR_SESSION_BUDGET_DOLLARS", 0.0);
    if budget > 0.0 && state.total_spend >= budget && pricing::trusted_for_budget_enforcement() {
        return Some(("budget_exceeded", format!(
            "Coditor: estimated session budget exceeded (${:.2}). Estimated spend: ${:.2} across {} requests. \
             Estimated cache rebuild waste: ${:.2}. Reset with CODITOR_SESSION_BUDGET_DOLLARS=0 or restart session.",
            budget,
            state.total_spend,
            state.request_count,
            state.estimated_cache_waste_dollars
        )));
    }

    // Token budget check.
    let token_budget = env_u64("CODITOR_SESSION_BUDGET_TOKENS", 0);
    if token_budget > 0 && state.total_tokens >= token_budget {
        return Some((
            "budget_exceeded",
            format!(
                "Coditor: token budget exceeded ({}). Used: {} tokens across {} requests. \
             Reset with CODITOR_SESSION_BUDGET_TOKENS=0 or restart session.",
                token_budget, state.total_tokens, state.request_count
            ),
        ));
    }

    None
}

fn make_block_response(error_type: &str, message: &str) -> ProcessingResponse {
    let body = serde_json::json!({
        "type": "error",
        "error": {
            "type": error_type,
            "message": message
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
    std::env::var("CODITOR_DB_PATH").unwrap_or_else(|_| "/data/coditor.db".to_string())
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
    cache_hit_ratio   REAL,
    degraded          INTEGER DEFAULT 0,
    degradation_turn  INTEGER,
    causes_json       TEXT,
    advice_json       TEXT,
    FOREIGN KEY (session_id) REFERENCES sessions(session_id)
);

CREATE TABLE IF NOT EXISTS tool_outcomes (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id  TEXT NOT NULL,
    turn_number INTEGER NOT NULL,
    timestamp   TEXT NOT NULL,
    tool_name   TEXT NOT NULL,
    input_summary TEXT,
    outcome     TEXT,
    duration_ms INTEGER
);

CREATE TABLE IF NOT EXISTS skill_events (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id  TEXT NOT NULL,
    timestamp   TEXT NOT NULL,
    skill_name  TEXT NOT NULL,
    event_type  TEXT NOT NULL,
    confidence  REAL DEFAULT 1.0,
    source      TEXT NOT NULL,
    detail      TEXT
);

CREATE TABLE IF NOT EXISTS mcp_events (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id  TEXT NOT NULL,
    timestamp   TEXT NOT NULL,
    server      TEXT NOT NULL,
    tool        TEXT NOT NULL,
    event_type  TEXT NOT NULL,
    source      TEXT NOT NULL,
    detail      TEXT
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
CREATE INDEX IF NOT EXISTS idx_tool_outcomes_session ON tool_outcomes(session_id);
CREATE INDEX IF NOT EXISTS idx_skill_events_session ON skill_events(session_id, timestamp);
CREATE INDEX IF NOT EXISTS idx_mcp_events_session ON mcp_events(session_id, timestamp);
CREATE INDEX IF NOT EXISTS idx_session_recall_session ON session_recall(session_id);
CREATE INDEX IF NOT EXISTS idx_billing_reconciliations_session_imported
    ON billing_reconciliations(session_id, imported_at DESC);
";

enum DbCommand {
    InsertSession {
        session_id: String,
        started_at: String,
        model: String,
        display_name: String,
        initial_prompt: Option<String>,
    },
    RecordRequest {
        request_id: String,
        session_id: String,
        timestamp: String,
        model: String,
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
        cache_creation_tokens: u64,
        cost_dollars: f64,
        cost_source: String,
        trusted_for_budget_enforcement: bool,
        duration_ms: u64,
        tool_calls_json: String,
        tool_calls_list: Vec<(String, String)>,
        cache_event: String,
    },
    RecordCodexTurn {
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
        tool_names_json: String,
        tool_calls_json: String,
        accounting_anomalies_json: String,
        cost_dollars: f64,
        cost_source: String,
        trusted_for_budget_enforcement: bool,
    },
    WriteTurnSnapshot {
        session_id: String,
        turn_number: u32,
        timestamp: String,
        input_tokens: u64,
        cache_read_tokens: u64,
        cache_creation_tokens: u64,
        output_tokens: u64,
        ttft_ms: u64,
        tool_calls_json: String,
        tool_failures: u32,
        gap_from_prev_secs: f64,
        context_utilization: f64,
        context_window_tokens: u64,
        frustration_signals: u32,
        requested_model: Option<String>,
        actual_model: Option<String>,
        response_summary: Option<String>,
    },
    WriteDiagnosis {
        session_id: String,
        completed_at: String,
        outcome: String,
        total_turns: u32,
        total_cost: f64,
        cache_hit_ratio: f64,
        degraded: bool,
        degradation_turn: Option<u32>,
        causes_json: String,
        advice_json: String,
    },
    WriteToolOutcome {
        session_id: String,
        turn_number: u32,
        timestamp: String,
        tool_name: String,
        input_summary: String,
        outcome: String,
        duration_ms: u64,
    },
    WriteSkillEvent {
        session_id: String,
        timestamp: String,
        skill_name: String,
        event_type: String,
        confidence: f64,
        source: String,
        detail: Option<String>,
    },
    WriteMcpEvent {
        session_id: String,
        timestamp: String,
        server: String,
        tool: String,
        event_type: String,
        source: String,
        detail: Option<String>,
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

fn db_writer_loop(path: &str, rx: std_mpsc::Receiver<DbCommand>) {
    let conn = match Connection::open(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to open SQLite at {path}: {e}");
            return;
        }
    };
    let _ = conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;");
    if let Err(e) = conn.execute_batch(SCHEMA) {
        eprintln!("Failed to create schema: {e}");
        return;
    }
    if let Err(e) = ensure_turn_snapshot_model_columns(&conn) {
        eprintln!("Failed to migrate turn_snapshots model columns: {e}");
        return;
    }
    if let Err(e) = ensure_session_columns(&conn) {
        eprintln!("Failed to migrate sessions columns: {e}");
        return;
    }
    if let Err(e) = ensure_request_cost_columns(&conn) {
        eprintln!("Failed to migrate requests cost columns: {e}");
        return;
    }
    if let Err(e) = ensure_codex_persistence_columns(&conn) {
        eprintln!("Failed to migrate Codex persistence columns: {e}");
        return;
    }
    if let Err(e) = repair_turn_snapshot_context_windows(&conn) {
        eprintln!("Failed to repair turn_snapshots context windows: {e}");
        return;
    }
    if let Err(e) = repair_session_diagnosis_degradation_turns(&conn) {
        eprintln!("Failed to repair session_diagnoses degradation turns: {e}");
        return;
    }
    if let Err(e) = seed_live_metric_labels_from_db(&conn) {
        eprintln!("Failed to seed live metric labels from SQLite: {e}");
        return;
    }

    while let Ok(cmd) = rx.recv() {
        match cmd {
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
            DbCommand::RecordRequest {
                request_id,
                session_id,
                timestamp,
                model,
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_creation_tokens,
                cost_dollars,
                cost_source,
                trusted_for_budget_enforcement,
                duration_ms,
                tool_calls_json,
                tool_calls_list,
                cache_event,
            } => {
                let inserted_rows = conn
                    .execute(
                    "INSERT OR IGNORE INTO requests (request_id, session_id, timestamp, model, \
                     input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens, \
                     cost_dollars, cost_source, trusted_for_budget_enforcement, duration_ms, tool_calls, cache_event) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
                    rusqlite::params![
                        &request_id,
                        &session_id,
                        timestamp,
                        model,
                        input_tokens,
                        output_tokens,
                        cache_read_tokens,
                        cache_creation_tokens,
                        cost_dollars,
                        cost_source,
                        trusted_for_budget_enforcement as i32,
                        duration_ms,
                        tool_calls_json,
                        cache_event,
                    ],
                )
                    .unwrap_or(0);

                if inserted_rows == 0 {
                    continue;
                }
                // Individual tool_calls rows.
                for (name, ts) in &tool_calls_list {
                    let _ = conn.execute(
                        "INSERT INTO tool_calls (request_id, timestamp, tool_name) VALUES (?1,?2,?3)",
                        rusqlite::params![&request_id, ts, name],
                    );
                }
                // Update session totals.
                let _ = conn.execute(
                    "UPDATE sessions SET \
                     total_input_tokens = total_input_tokens + ?2, \
                     total_output_tokens = total_output_tokens + ?3, \
                     total_cache_read_tokens = total_cache_read_tokens + ?4, \
                     total_cache_creation_tokens = total_cache_creation_tokens + ?5, \
                     total_cost_dollars = total_cost_dollars + ?6, \
                     request_count = request_count + 1, \
                     ended_at = ?7 \
                     WHERE session_id = ?1",
                    rusqlite::params![
                        session_id,
                        input_tokens,
                        output_tokens,
                        cache_read_tokens,
                        cache_creation_tokens,
                        cost_dollars,
                        timestamp,
                    ],
                );
            }
            DbCommand::RecordCodexTurn {
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
                tool_names_json,
                tool_calls_json,
                accounting_anomalies_json,
                cost_dollars,
                cost_source,
                trusted_for_budget_enforcement,
            } => {
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
                            codex_prompt_excerpt, codex_tool_calls,
                            codex_accounting_anomalies
                         ) VALUES (
                            ?1, ?2, ?3, ?4, ?5, ?6,
                            0, 0, ?7, ?8, ?9, ?10, ?11, NULL,
                            'codex_responses', ?12, ?13, ?14,
                            ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24
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
                        codex_prompt_excerpt, codex_tool_calls,
                        codex_accounting_anomalies
                     ) VALUES (
                        ?1, ?2, ?3, ?4, 0, 0, ?5, ?6, ?7,
                        0, 0.0, ?8, ?9, 0, ?10, ?11, ?12, ?13,
                        'codex_responses', ?14, ?15, ?16, ?17, ?18, ?19, ?20,
                        ?21, ?22, ?23, ?24
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
            DbCommand::WriteTurnSnapshot {
                session_id,
                turn_number,
                timestamp,
                input_tokens,
                cache_read_tokens,
                cache_creation_tokens,
                output_tokens,
                ttft_ms,
                tool_calls_json,
                tool_failures,
                gap_from_prev_secs,
                context_utilization,
                context_window_tokens,
                frustration_signals,
                requested_model,
                actual_model,
                response_summary,
            } => {
                let _ = conn.execute(
                    "INSERT INTO turn_snapshots (session_id, turn_number, timestamp, \
                     input_tokens, cache_read_tokens, cache_creation_tokens, output_tokens, \
                     ttft_ms, tool_calls, tool_failures, gap_from_prev_secs, \
                     context_utilization, context_window_tokens, frustration_signals, \
                     requested_model, actual_model, response_summary) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
                    rusqlite::params![
                        session_id,
                        turn_number,
                        timestamp,
                        input_tokens,
                        cache_read_tokens,
                        cache_creation_tokens,
                        output_tokens,
                        ttft_ms,
                        tool_calls_json,
                        tool_failures,
                        gap_from_prev_secs,
                        context_utilization,
                        context_window_tokens,
                        frustration_signals,
                        requested_model,
                        actual_model,
                        response_summary,
                    ],
                );
            }
            DbCommand::WriteDiagnosis {
                session_id,
                completed_at,
                outcome,
                total_turns,
                total_cost,
                cache_hit_ratio,
                degraded,
                degradation_turn,
                causes_json,
                advice_json,
            } => {
                let _ = conn.execute(
                    "INSERT OR REPLACE INTO session_diagnoses (session_id, completed_at, \
                     outcome, total_turns, total_cost, cache_hit_ratio, degraded, \
                     degradation_turn, causes_json, advice_json) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                    rusqlite::params![
                        session_id,
                        completed_at,
                        outcome,
                        total_turns,
                        total_cost,
                        cache_hit_ratio,
                        degraded as i32,
                        degradation_turn,
                        causes_json,
                        advice_json,
                    ],
                );
            }
            DbCommand::WriteToolOutcome {
                session_id,
                turn_number,
                timestamp,
                tool_name,
                input_summary,
                outcome,
                duration_ms,
            } => {
                let _ = conn.execute(
                    "INSERT INTO tool_outcomes (session_id, turn_number, timestamp, \
                     tool_name, input_summary, outcome, duration_ms) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7)",
                    rusqlite::params![
                        session_id,
                        turn_number,
                        timestamp,
                        tool_name,
                        input_summary,
                        outcome,
                        duration_ms,
                    ],
                );
            }
            DbCommand::WriteSkillEvent {
                session_id,
                timestamp,
                skill_name,
                event_type,
                confidence,
                source,
                detail,
            } => {
                let _ = conn.execute(
                    "INSERT INTO skill_events (session_id, timestamp, skill_name, event_type, \
                     confidence, source, detail) VALUES (?1,?2,?3,?4,?5,?6,?7)",
                    rusqlite::params![
                        session_id,
                        timestamp,
                        skill_name,
                        event_type,
                        confidence,
                        source,
                        detail,
                    ],
                );
            }
            DbCommand::WriteMcpEvent {
                session_id,
                timestamp,
                server,
                tool,
                event_type,
                source,
                detail,
            } => {
                let _ = conn.execute(
                    "INSERT INTO mcp_events (session_id, timestamp, server, tool, event_type, \
                     source, detail) VALUES (?1,?2,?3,?4,?5,?6,?7)",
                    rusqlite::params![
                        session_id, timestamp, server, tool, event_type, source, detail,
                    ],
                );
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
    cache_hit_ratio: f64,
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

fn compute_estimated_costs_for_sessions(
    conn: &Connection,
    session_ids: &[String],
) -> rusqlite::Result<HashMap<String, EstimatedAggregate>> {
    if session_ids.is_empty() {
        return Ok(HashMap::new());
    }

    let placeholders = vec!["?"; session_ids.len()].join(",");
    let sql = format!(
        "SELECT session_id, model, input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens, \
         cost_dollars, cost_source, trusted_for_budget_enforcement \
         FROM requests WHERE session_id IN ({placeholders})"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(session_ids.iter()), |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, Option<f64>>(6)?,
            row.get::<_, Option<String>>(7)?,
            row.get::<_, Option<i64>>(8)?,
        ))
    })?;

    let mut accumulators: HashMap<String, CostAccumulator> = HashMap::new();
    for row in rows {
        let (
            session_id,
            model,
            input,
            output,
            cache_read,
            cache_create,
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
            accumulator.record(pricing::estimate_cost_dollars(
                &model,
                input.max(0) as u64,
                output.max(0) as u64,
                cache_read.max(0) as u64,
                cache_create.max(0) as u64,
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
        "SELECT r.session_id, r.model, r.input_tokens, r.output_tokens, r.cache_read_tokens, \
                r.cache_creation_tokens, r.cost_dollars, r.cost_source, \
                r.trusted_for_budget_enforcement, CASE WHEN s.session_id IS NULL THEN 0 ELSE 1 END \
         FROM requests r \
         LEFT JOIN sessions s ON s.session_id = r.session_id \
         WHERE r.timestamp >= ?1",
    )?;
    let rows = stmt.query_map(rusqlite::params![since], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, Option<f64>>(6)?,
            row.get::<_, Option<String>>(7)?,
            row.get::<_, Option<i64>>(8)?,
            row.get::<_, i64>(9)? != 0,
        ))
    })?;

    let mut session_ids = HashSet::new();
    let mut cost_accumulator = CostAccumulator::new();
    let mut cache_read_tokens: i64 = 0;
    let mut cache_total_tokens: i64 = 0;

    for row in rows {
        let (
            session_id,
            model,
            input,
            output,
            cache_read,
            cache_create,
            stored_cost,
            stored_source,
            stored_trusted,
            is_real_session,
        ) = row?;
        if is_real_session {
            session_ids.insert(session_id);
        }
        cache_read_tokens += cache_read.max(0);
        cache_total_tokens += (cache_read + cache_create).max(0);
        if let Some(cost) = stored_cost {
            cost_accumulator.record_persisted(cost, stored_source, stored_trusted.map(|n| n != 0));
        } else {
            cost_accumulator.record(pricing::estimate_cost_dollars(
                &model,
                input.max(0) as u64,
                output.max(0) as u64,
                cache_read.max(0) as u64,
                cache_create.max(0) as u64,
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

    let cache_hit_ratio = if cache_total_tokens > 0 {
        cache_read_tokens as f64 / cache_total_tokens as f64
    } else {
        0.0
    };

    let estimate = cost_accumulator.finish();
    Ok(SummaryWindowData {
        sessions: session_ids.len() as i64,
        estimated_cost_dollars: estimate.estimated_cost_dollars,
        cost_source: estimate.cost_source,
        trusted_for_budget_enforcement: estimate.trusted_for_budget_enforcement,
        billed_cost_dollars,
        billed_sessions,
        cache_hit_ratio,
    })
}

fn summary_window_json(summary: &SummaryWindowData) -> Value {
    serde_json::json!({
        "sessions": summary.sessions,
        "estimated_cost_dollars": rounded_estimated_cost_dollars(summary.estimated_cost_dollars),
        "cost_source": summary.cost_source.clone(),
        "trusted_for_budget_enforcement": summary.trusted_for_budget_enforcement,
        "billed_cost_dollars": rounded_billed_cost_dollars(summary.billed_cost_dollars),
        "billed_sessions": summary.billed_sessions,
        "cache_hit_ratio": (summary.cache_hit_ratio * 100.0).round() / 100.0,
    })
}

fn build_summary_response_json(
    today: &SummaryWindowData,
    week: &SummaryWindowData,
    month: &SummaryWindowData,
) -> Value {
    serde_json::json!({
        "cost_source": pricing::active_catalog_source(),
        "trusted_for_budget_enforcement": pricing::trusted_for_budget_enforcement(),
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
    cache_hit_ratio: f64,
    degraded: bool,
    degradation_turn: Option<i64>,
    causes: Value,
    advice: Value,
) -> Value {
    serde_json::json!({
        "session_id": session_id,
        "completed_at": completed_at,
        "outcome": outcome,
        "total_turns": total_turns,
        "estimated_total_cost_dollars": estimated_total_cost_dollars,
        "cost_source": cost_source,
        "trusted_for_budget_enforcement": trusted_for_budget_enforcement,
        "billed_cost_dollars": rounded_billed_cost_dollars(billed_cost_dollars),
        "billing_source": billing_source,
        "billing_imported_at": billing_imported_at,
        "cache_hit_ratio": cache_hit_ratio,
        "degraded": degraded,
        "degradation_turn": if degraded { degradation_turn } else { None },
        "causes": causes,
        "advice": advice,
    })
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
    cache_hit_ratio: f64,
    model: Option<String>,
    requested_model: Option<String>,
    served_model: Option<String>,
) -> Value {
    serde_json::json!({
        "session_id": session_id,
        "display_name": display_name,
        "started_at": started_at,
        "outcome": outcome,
        "degraded": degraded,
        "total_turns": total_turns,
        "estimated_total_cost_dollars": estimated_total_cost_dollars,
        "cost_source": cost_source,
        "trusted_for_budget_enforcement": trusted_for_budget_enforcement,
        "billed_cost_dollars": rounded_billed_cost_dollars(billed_cost_dollars),
        "billing_source": billing_source,
        "billing_imported_at": billing_imported_at,
        "primary_cause": primary_cause,
        "cache_hit_ratio": cache_hit_ratio,
        "model": model,
        "requested_model": requested_model,
        "served_model": served_model,
    })
}

fn build_sessions_response_json(sessions: Vec<Value>) -> Value {
    serde_json::json!({
        "cost_source": pricing::active_catalog_source(),
        "trusted_for_budget_enforcement": pricing::trusted_for_budget_enforcement(),
        "sessions": sessions,
    })
}

#[derive(Deserialize)]
struct StoredDegradationCause {
    cause_type: String,
}

fn query_historical_window_from_db(
    conn: &Connection,
    since: &str,
    window: &'static str,
) -> rusqlite::Result<metrics::HistoricalWindowMetrics> {
    let mut stmt = conn.prepare(
        "SELECT model, input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens, \
         cost_dollars, cost_source, trusted_for_budget_enforcement, cache_event \
         FROM requests WHERE timestamp >= ?1",
    )?;
    let rows = stmt.query_map(rusqlite::params![since], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, Option<f64>>(5)?,
            row.get::<_, Option<String>>(6)?,
            row.get::<_, Option<i64>>(7)?,
            row.get::<_, Option<String>>(8)?,
        ))
    })?;

    let mut cost_accumulator = CostAccumulator::new();
    let mut estimated_spend_dollars_by_model = std::collections::BTreeMap::new();
    let mut estimated_cache_waste_dollars_by_model = std::collections::BTreeMap::new();
    let mut cache_read_tokens: i64 = 0;
    let mut cache_total_tokens: i64 = 0;
    let mut cache_events = std::collections::BTreeMap::new();
    for row in rows {
        let (
            model,
            input,
            output,
            cache_read,
            cache_create,
            stored_cost,
            stored_source,
            stored_trusted,
            cache_event,
        ) = row?;
        let row_cost = if let Some(cost) = stored_cost {
            cost_accumulator.record_persisted(cost, stored_source, stored_trusted.map(|n| n != 0));
            cost
        } else {
            let estimated = pricing::estimate_cost_dollars(
                &model,
                input.max(0) as u64,
                output.max(0) as u64,
                cache_read.max(0) as u64,
                cache_create.max(0) as u64,
            );
            let total_cost = estimated.total_cost_dollars;
            cost_accumulator.record(estimated);
            total_cost
        };
        let model_label = metrics::historical_model_label(&model);
        if let Some(cache_event_label) = cache_event
            .as_deref()
            .and_then(metrics::historical_cache_event_label)
        {
            *cache_events.entry(cache_event_label).or_insert(0) += 1;
            let waste =
                pricing::estimate_cache_rebuild_waste_dollars(&model, cache_create.max(0) as u64)
                    .total_cost_dollars;
            *estimated_cache_waste_dollars_by_model
                .entry(model_label)
                .or_insert(0.0) += waste.max(0.0);
        }
        *estimated_spend_dollars_by_model
            .entry(model_label)
            .or_insert(0.0) += row_cost.max(0.0);
        cache_read_tokens += cache_read.max(0);
        cache_total_tokens += (cache_read + cache_create).max(0);
    }

    let cache_hit_ratio = if cache_total_tokens > 0 {
        cache_read_tokens.max(0) as f64 / cache_total_tokens.max(0) as f64
    } else {
        0.0
    };

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
         FROM turn_snapshots WHERE timestamp >= ?1",
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

    let mut tool_failures_by_tool = std::collections::BTreeMap::new();
    let mut stmt = conn.prepare(
        "SELECT tool_name, COUNT(*) \
         FROM tool_outcomes \
         WHERE timestamp >= ?1 AND outcome = 'error' \
         GROUP BY tool_name",
    )?;
    let rows = stmt.query_map(rusqlite::params![since], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    for row in rows {
        let (tool_name, count) = row?;
        let tool_label = metrics::historical_tool_label(&tool_name);
        *tool_failures_by_tool.entry(tool_label).or_insert(0) += count.max(0) as u64;
    }

    let mut avg_estimated_session_cost_dollars_by_model = std::collections::BTreeMap::new();
    let mut model_totals = std::collections::BTreeMap::<&'static str, (f64, u64)>::new();
    let mut stmt = conn.prepare(
        "SELECT s.model, d.total_cost \
         FROM session_diagnoses d \
         LEFT JOIN sessions s ON s.session_id = d.session_id \
         WHERE d.completed_at >= ?1",
    )?;
    let rows = stmt.query_map(rusqlite::params![since], |row| {
        Ok((
            row.get::<_, Option<String>>(0)?,
            row.get::<_, Option<f64>>(1)?,
        ))
    })?;
    for row in rows {
        let (model, total_cost) = row?;
        let label = model
            .as_deref()
            .map(metrics::historical_model_label)
            .unwrap_or("other");
        let entry = model_totals.entry(label).or_insert((0.0, 0));
        entry.0 += total_cost.unwrap_or(0.0).max(0.0);
        entry.1 += 1;
    }
    for (label, (total_cost, count)) in model_totals {
        let avg_cost = if count > 0 {
            total_cost / count as f64
        } else {
            0.0
        };
        avg_estimated_session_cost_dollars_by_model.insert(label, avg_cost);
    }

    Ok(metrics::HistoricalWindowMetrics {
        window,
        sessions,
        estimated_spend_dollars: cost_accumulator.finish().estimated_cost_dollars,
        estimated_spend_dollars_by_model,
        estimated_cache_waste_dollars_by_model,
        avg_estimated_session_cost_dollars_by_model,
        cache_hit_ratio,
        cache_events,
        degraded_sessions,
        degraded_session_ratio,
        degraded_causes,
        model_fallbacks,
        tool_failures_by_tool,
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
    metrics::record_codex_turn(
        metric_model,
        outcome.accounting.input_tokens,
        outcome.accounting.cached_input_tokens,
        outcome.accounting.uncached_input_tokens,
        outcome.accounting.output_tokens,
        outcome.accounting.reasoning_output_tokens,
        outcome.accounting.total_tokens,
        outcome.accounting.pricing.cost_dollars.unwrap_or(0.0),
        duration.as_secs_f64(),
    );
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
        existing.cache_warning_sent = true;
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
            cache_warning_sent: true,
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
    DbCommand::RecordCodexTurn {
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
        tool_names_json: "[]".to_string(),
        tool_calls_json: codex_tool_calls_json(accounting),
        accounting_anomalies_json: codex_accounting_anomalies_json(accounting),
        cost_dollars: accounting.pricing.cost_dollars.unwrap_or(0.0),
        cost_source: codex_cost_source(accounting),
        trusted_for_budget_enforcement: accounting.pricing.trusted_for_budget_enforcement,
    }
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
    env_u64("CODITOR_SESSION_TIMEOUT_MINUTES", 5) * 60
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
         FROM turn_snapshots WHERE session_id = ?1 ORDER BY turn_number ASC",
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

fn stored_tool_recall_context(
    conn: &Connection,
    session_id: &str,
) -> rusqlite::Result<Option<String>> {
    let mut stmt = conn.prepare(
        "SELECT tool_name, input_summary, outcome \
         FROM tool_outcomes WHERE session_id = ?1 \
         ORDER BY turn_number DESC, id DESC LIMIT 12",
    )?;
    let rows = stmt.query_map(rusqlite::params![session_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
        ))
    })?;

    let mut seen = HashSet::new();
    let mut parts = Vec::new();
    for row in rows.flatten() {
        let (tool_name, input_summary, outcome) = row;
        let mut detail = tool_name;
        if let Some(summary) = input_summary {
            let summary = summary.trim();
            if !summary.is_empty() {
                detail.push(' ');
                detail.push_str(summary);
            }
        }
        if matches!(outcome.as_deref(), Some("error")) {
            detail.push_str(" (error)");
        }
        if seen.insert(detail.clone()) {
            parts.push(detail);
        }
        if parts.len() >= 3 {
            break;
        }
    }

    if parts.is_empty() {
        Ok(None)
    } else {
        Ok(Some(format!("Tools: {}", parts.join("; "))))
    }
}

#[derive(Debug)]
struct PersistedWatchSession {
    session_id: String,
    model: Option<String>,
    display_name: Option<String>,
    initial_prompt: Option<String>,
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
            "SELECT session_id, model, display_name, initial_prompt \
             FROM sessions WHERE session_id = ?1 LIMIT 1",
        )?;
        return stmt
            .query_map(rusqlite::params![session_id], |row| {
                Ok(PersistedWatchSession {
                    session_id: row.get::<_, String>(0)?,
                    model: row.get::<_, Option<String>>(1)?,
                    display_name: row.get::<_, Option<String>>(2)?,
                    initial_prompt: row.get::<_, Option<String>>(3)?,
                })
            })?
            .collect();
    }

    let mut stmt = conn.prepare(
        "SELECT session_id, model, display_name, initial_prompt \
         FROM sessions \
         WHERE request_count > 0 \
         ORDER BY COALESCE(ended_at, started_at) DESC LIMIT ?1",
    )?;
    let mut sessions = stmt
        .query_map(rusqlite::params![limit as i64], |row| {
            Ok(PersistedWatchSession {
                session_id: row.get::<_, String>(0)?,
                model: row.get::<_, Option<String>>(1)?,
                display_name: row.get::<_, Option<String>>(2)?,
                initial_prompt: row.get::<_, Option<String>>(3)?,
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

    let turns = stmt
        .query_map(rusqlite::params![session_id], |row| {
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
        })?
        .collect();
    turns
}

fn load_persisted_watch_replay_events(
    conn: &Connection,
    session_filter: Option<&str>,
    limit: usize,
) -> rusqlite::Result<Vec<watch::WatchEvent>> {
    let sessions = load_persisted_watch_sessions(conn, session_filter, limit)?;
    let mut events = Vec::new();

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

        for turn in load_persisted_watch_turns(conn, &session.session_id)? {
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
    }

    Ok(events)
}

fn watch_event_session_id(event: &watch::WatchEvent) -> Option<&str> {
    match event {
        watch::WatchEvent::ToolUse { session_id, .. }
        | watch::WatchEvent::ToolResult { session_id, .. }
        | watch::WatchEvent::SkillEvent { session_id, .. }
        | watch::WatchEvent::McpEvent { session_id, .. }
        | watch::WatchEvent::CacheEvent { session_id, .. }
        | watch::WatchEvent::SessionStart { session_id, .. }
        | watch::WatchEvent::SessionEnd { session_id, .. }
        | watch::WatchEvent::FrustrationSignal { session_id, .. }
        | watch::WatchEvent::CompactionLoop { session_id, .. }
        | watch::WatchEvent::Diagnosis { session_id, .. }
        | watch::WatchEvent::CacheWarning { session_id, .. }
        | watch::WatchEvent::ModelFallback { session_id, .. }
        | watch::WatchEvent::CodexTurnSummary { session_id, .. }
        | watch::WatchEvent::ContextStatus { session_id, .. } => Some(session_id.as_str()),
        watch::WatchEvent::RateLimitStatus { .. } => None,
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

fn latest_response_summary_from_db(
    conn: &Connection,
    session_id: &str,
) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT response_summary FROM turn_snapshots \
         WHERE session_id = ?1 AND response_summary IS NOT NULL AND trim(response_summary) != '' \
         ORDER BY turn_number DESC LIMIT 1",
        rusqlite::params![session_id],
        |row| row.get::<_, String>(0),
    )
    .optional()
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
    conn.execute(
        "INSERT OR REPLACE INTO session_diagnoses (session_id, completed_at, \
         outcome, total_turns, total_cost, cache_hit_ratio, degraded, degradation_turn, \
         causes_json, advice_json) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        rusqlite::params![
            session_id,
            completed_at,
            report.outcome,
            report.total_turns,
            report.estimated_total_cost_dollars,
            report.cache_hit_ratio,
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
    persist_session_diagnosis_report(conn, session_id, &completed_at, &report)?;

    Ok(Some((completed_at, report)))
}

fn repair_persisted_session_artifacts(conn: &Connection) -> rusqlite::Result<()> {
    let _ = ensure_turn_snapshot_model_columns(conn);
    let _ = ensure_session_columns(conn);
    let _ = ensure_request_cost_columns(conn);
    let _ = ensure_codex_persistence_columns(conn);
    let _ = repair_turn_snapshot_context_windows(conn);
    let _ = repair_session_diagnosis_degradation_turns(conn);
    let cutoff = epoch_to_iso8601(now_epoch_secs().saturating_sub(session_timeout_secs()));
    let mut stmt = conn.prepare(
        "SELECT s.session_id, s.ended_at, s.initial_prompt, d.outcome, \
                CASE WHEN r.session_id IS NULL THEN 0 ELSE 1 END \
         FROM sessions s \
         LEFT JOIN session_diagnoses d ON d.session_id = s.session_id \
         LEFT JOIN session_recall r ON r.session_id = s.session_id \
         WHERE s.ended_at IS NOT NULL AND s.ended_at <= ?1 \
           AND EXISTS (SELECT 1 FROM turn_snapshots t WHERE t.session_id = s.session_id) \
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
                              AND t2.response_summary IS NOT NULL
                              AND trim(t2.response_summary) != ''
                        )
                        OR EXISTS (
                            SELECT 1 FROM tool_outcomes o
                            WHERE o.session_id = s.session_id
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
            let summary = latest_response_summary_from_db(conn, &session_id)?
                .or_else(|| stored_tool_recall_context(conn, &session_id).ok().flatten())
                .unwrap_or_default();
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
        let report = diagnosis::analyze_session(session_id, &turns);

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
            cache_hit_ratio: report.cache_hit_ratio,
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
fn derive_display_name(working_dir: &str, model: &str, sys_prompt_hash: u64) -> String {
    let base = if !working_dir.is_empty() {
        // Extract the last path component: /Users/pradeep/code/idea/coditor → coditor
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
    let value = std::env::var("CODITOR_CONTEXT_WINDOW_TOKENS").ok()?;
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

/// Local quota-burn tracker. The Envoy-observed Responses stream does not
/// include subscription quota headers, so we track local token counters and
/// project burn from the delta over a sliding window. The user supplies a
/// weekly budget via `CODITOR_WEEKLY_TOKEN_BUDGET` (tokens); without it we
/// still broadcast burn rate but skip the "remaining" and "projected
/// exhaustion" fields.
struct BurnTracker {
    /// (timestamp, cumulative_tokens_seen) samples. Bounded ring.
    samples: VecDeque<(Instant, u64)>,
}

impl BurnTracker {
    fn new() -> Self {
        Self {
            samples: VecDeque::with_capacity(64),
        }
    }

    fn record(&mut self, cumulative_tokens: u64) {
        self.samples.push_back((Instant::now(), cumulative_tokens));
        if self.samples.len() > 64 {
            self.samples.pop_front();
        }
    }

    /// Tokens per second over the last window. None when span too short or
    /// not enough samples.
    fn tokens_per_sec(&self) -> Option<f64> {
        let (first_t, first_v) = self.samples.front()?;
        let (last_t, last_v) = self.samples.back()?;
        let span = last_t.duration_since(*first_t).as_secs_f64();
        if span < 60.0 {
            return None;
        }
        let delta = last_v.saturating_sub(*first_v) as f64;
        if delta <= 0.0 {
            return None;
        }
        Some(delta / span)
    }
}

static BURN_TRACKER: LazyLock<Mutex<BurnTracker>> =
    LazyLock::new(|| Mutex::new(BurnTracker::new()));

const QUOTA_SAMPLE_INTERVAL: Duration = Duration::from_secs(30);
const QUOTA_WATCH_BROADCAST_INTERVAL: Duration = Duration::from_secs(5 * 60);

fn should_broadcast_quota_snapshot(
    since_last_broadcast: Option<Duration>,
    previous_alarm: Option<bool>,
    alarm: bool,
) -> bool {
    let interval_elapsed = since_last_broadcast
        .map(|elapsed| elapsed >= QUOTA_WATCH_BROADCAST_INTERVAL)
        .unwrap_or(true);
    let alarm_changed = previous_alarm
        .map(|previous| previous != alarm)
        .unwrap_or(false);
    interval_elapsed || alarm_changed
}

#[derive(Clone)]
struct AutoWeeklyBudget {
    tokens_limit: u64,
}

struct AutoWeeklyBudgetCache {
    refreshed_at: Option<Instant>,
    week_start_epoch: Option<u64>,
    suggestion: Option<AutoWeeklyBudget>,
}

impl AutoWeeklyBudgetCache {
    fn new() -> Self {
        Self {
            refreshed_at: None,
            week_start_epoch: None,
            suggestion: None,
        }
    }
}

static AUTO_WEEKLY_BUDGET_CACHE: LazyLock<Mutex<AutoWeeklyBudgetCache>> =
    LazyLock::new(|| Mutex::new(AutoWeeklyBudgetCache::new()));
static CODEX_TOOL_EVENT_DEDUP: LazyLock<Mutex<HashMap<String, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
const CODEX_TOOL_EVENT_DEDUP_TTL: Duration = Duration::from_secs(300);

fn percentile_nearest_rank(mut values: Vec<u64>, percentile: f64) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    let rank = ((percentile.clamp(0.0, 1.0) * values.len() as f64).ceil() as usize)
        .max(1)
        .min(values.len());
    values.get(rank - 1).copied()
}

fn query_auto_weekly_budget_suggestion(current_week_start: u64) -> Option<AutoWeeklyBudget> {
    let conn = Connection::open(db_path()).ok()?;
    let mut stmt = conn.prepare(
        "SELECT COALESCE(SUM(input_tokens + output_tokens + cache_read_tokens + cache_creation_tokens), 0) \
         FROM requests WHERE timestamp >= ?1 AND timestamp < ?2",
    ).ok()?;

    let mut weekly_totals = Vec::with_capacity(4);
    for weeks_back in 1..=4u64 {
        let week_start = current_week_start.saturating_sub(weeks_back * 7 * 86_400);
        let week_end = week_start + 7 * 86_400;
        let total = stmt
            .query_row(
                rusqlite::params![epoch_to_iso8601(week_start), epoch_to_iso8601(week_end)],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or(0);
        weekly_totals.push(total.max(0) as u64);
    }

    if weekly_totals.iter().all(|&t| t == 0) {
        return None;
    }

    // With only four completed weeks, p95 is effectively the max weekly total.
    percentile_nearest_rank(weekly_totals, 0.95)
        .map(|tokens_limit| AutoWeeklyBudget { tokens_limit })
}

fn auto_weekly_budget_suggestion() -> Option<AutoWeeklyBudget> {
    let now = Instant::now();
    let current_week_start = start_of_week_epoch_at(now_epoch_secs());

    {
        let cache = AUTO_WEEKLY_BUDGET_CACHE.lock().unwrap();
        let is_fresh = cache
            .refreshed_at
            .map(|t| now.duration_since(t) < Duration::from_secs(3600))
            .unwrap_or(false);
        if is_fresh && cache.week_start_epoch == Some(current_week_start) {
            return cache.suggestion.clone();
        }
    }

    let suggestion =
        tokio::task::block_in_place(|| query_auto_weekly_budget_suggestion(current_week_start));

    let mut cache = AUTO_WEEKLY_BUDGET_CACHE.lock().unwrap();
    cache.refreshed_at = Some(now);
    cache.week_start_epoch = Some(current_week_start);
    cache.suggestion = suggestion.clone();
    suggestion
}

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
        watch::WatchEvent::ToolResult {
            session_id,
            tool_name,
            outcome,
            ..
        } => Some(format!(
            "{}|tool_result|{}|{}",
            session_id,
            tool_name.trim(),
            outcome.trim()
        )),
        watch::WatchEvent::McpEvent {
            session_id,
            server,
            tool,
            event_type,
            detail,
            ..
        } => Some(format!(
            "{}|mcp|{}|{}|{}|{}",
            session_id,
            server.trim(),
            tool.trim(),
            event_type.trim(),
            detail.as_deref().unwrap_or("").trim()
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
pub struct CoditorProcessor;

#[tonic::async_trait]
impl ExternalProcessor for CoditorProcessor {
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
                        if let Some((err_type, msg)) = check_circuit_breaker() {
                            warn!(request_id = %request_id, error_type = err_type, "request blocked");
                            let response = make_block_response(err_type, &msg);
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
                                    if let Some((err_type, msg)) = check_session_budget(
                                        diagnosis::SESSIONS
                                            .get(&request_metadata.session_hash)
                                            .as_deref()
                                            .map(|state| state.session_id.as_str()),
                                    ) {
                                        warn!(request_id = %request_id, error_type = err_type, "request blocked");
                                        let response = make_block_response(err_type, &msg);
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
                        match RUNTIME_STATE.lock() {
                            Ok(mut runtime) => {
                                if status >= 400 && !model.is_empty() {
                                    runtime.consecutive_errors += 1;
                                    let threshold = env_u64("CODITOR_CIRCUIT_BREAKER_THRESHOLD", 5);
                                    if runtime.consecutive_errors >= threshold
                                        && runtime.circuit_open_until.is_none()
                                    {
                                        runtime.circuit_open_until =
                                            Some(Instant::now() + Duration::from_secs(30));
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
/// Global events (RateLimitStatus, Lagged pseudo-events) are always passed
/// through so clients know about overflow or quota pressure.
fn event_matches_session(ev: &watch::WatchEvent, filter: Option<&str>) -> bool {
    let Some(want) = filter else {
        return true;
    };
    match ev {
        watch::WatchEvent::ToolUse { session_id, .. }
        | watch::WatchEvent::ToolResult { session_id, .. }
        | watch::WatchEvent::SkillEvent { session_id, .. }
        | watch::WatchEvent::McpEvent { session_id, .. }
        | watch::WatchEvent::CacheEvent { session_id, .. }
        | watch::WatchEvent::SessionStart { session_id, .. }
        | watch::WatchEvent::SessionEnd { session_id, .. }
        | watch::WatchEvent::FrustrationSignal { session_id, .. }
        | watch::WatchEvent::CompactionLoop { session_id, .. }
        | watch::WatchEvent::Diagnosis { session_id, .. }
        | watch::WatchEvent::CacheWarning { session_id, .. }
        | watch::WatchEvent::ModelFallback { session_id, .. }
        | watch::WatchEvent::CodexTurnSummary { session_id, .. }
        | watch::WatchEvent::ContextStatus { session_id, .. } => session_id == want,
        watch::WatchEvent::RateLimitStatus { .. } => true,
    }
}

async fn handle_diagnosis(
    axum::extract::Path(session_id): axum::extract::Path<String>,
) -> impl IntoResponse {
    let result = tokio::task::spawn_blocking(move || {
        let conn = Connection::open(db_path()).ok()?;
        let _ = repair_persisted_session_artifacts(&conn);
        let estimated =
            compute_estimated_costs_for_sessions(&conn, std::slice::from_ref(&session_id))
                .ok()?
                .remove(&session_id)
                .unwrap_or_else(|| CostAccumulator::new().finish());
        let billing = load_latest_billing_reconciliations(&conn, std::slice::from_ref(&session_id))
            .ok()?
            .remove(&session_id);
        if let Some((completed_at, report)) =
            build_fresh_diagnosis_report(&conn, &session_id, &estimated).ok()?
        {
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
                report.cache_hit_ratio,
                report.degraded,
                report.degradation_turn.map(i64::from),
                serde_json::to_value(&report.causes).unwrap_or(Value::Array(vec![])),
                serde_json::to_value(&report.advice).unwrap_or(Value::Array(vec![])),
            ));
        }

        let mut stmt = conn
            .prepare(
                "SELECT session_id, completed_at, outcome, total_turns, \
             cache_hit_ratio, degraded, degradation_turn, causes_json, advice_json \
             FROM session_diagnoses WHERE session_id = ?1",
            )
            .ok()?;
        stmt.query_row(rusqlite::params![&session_id], |row| {
            let causes_str: String = row.get(7)?;
            let advice_str: String = row.get(8)?;
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
                row.get::<_, f64>(4)?,
                row.get::<_, i32>(5)? != 0,
                row.get::<_, Option<i64>>(6)?,
                serde_json::from_str::<Value>(&causes_str).unwrap_or(Value::Array(vec![])),
                serde_json::from_str::<Value>(&advice_str).unwrap_or(Value::Array(vec![])),
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

        let mut stmt = conn
            .prepare(
                "SELECT s.session_id, s.started_at, s.model, s.display_name, s.initial_prompt, d.outcome, d.degraded, \
                        d.total_turns, d.causes_json, d.cache_hit_ratio, \
                        (SELECT r.requested_model FROM requests r \
                         WHERE r.session_id = s.session_id \
                           AND r.provider = 'codex_responses' \
                           AND r.requested_model IS NOT NULL \
                         ORDER BY r.timestamp DESC LIMIT 1), \
                        (SELECT r.served_model FROM requests r \
                         WHERE r.session_id = s.session_id \
                           AND r.provider = 'codex_responses' \
                           AND r.served_model IS NOT NULL \
                         ORDER BY r.timestamp DESC LIMIT 1) \
                 FROM sessions s \
                 LEFT JOIN session_diagnoses d ON d.session_id = s.session_id \
                 WHERE COALESCE(s.ended_at, s.started_at) >= ?1 \
                 ORDER BY COALESCE(s.ended_at, s.started_at) DESC LIMIT ?2",
            )
            .ok()?;

        type RecentSessionRow = (
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<bool>,
            Option<i64>,
            Option<String>,
            Option<f64>,
            Option<String>,
            Option<String>,
        );

        let session_rows: Vec<RecentSessionRow> = stmt
            .query_map(rusqlite::params![since, limit], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<i32>>(6)?.map(|value| value != 0),
                    row.get::<_, Option<i64>>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, Option<f64>>(9)?,
                    row.get::<_, Option<String>>(10)?,
                    row.get::<_, Option<String>>(11)?,
                ))
            })
            .ok()?
            .filter_map(|r| r.ok())
            .collect();

        let session_ids = session_rows
            .iter()
            .map(|row| row.0.clone())
            .collect::<Vec<_>>();
        let estimated_costs = compute_estimated_costs_for_sessions(&conn, &session_ids).ok()?;
        let billing = load_latest_billing_reconciliations(&conn, &session_ids).ok()?;

        let sessions: Vec<Value> = session_rows
            .into_iter()
            .map(
                |(
                    session_id,
                    started_at,
                    model,
                    stored_display_name,
                    initial_prompt,
                    stored_outcome,
                    stored_degraded,
                    stored_total_turns,
                    stored_causes_str,
                    stored_cache_hit_ratio,
                    requested_model,
                    served_model,
                )| {
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

                    let (outcome, degraded, total_turns, causes_json, cache_hit_ratio) =
                        if let Some(report) = refreshed {
                            (
                                report.outcome,
                                report.degraded,
                                report.total_turns as i64,
                                serde_json::to_value(&report.causes)
                                    .unwrap_or(Value::Array(vec![])),
                                report.cache_hit_ratio,
                            )
                        } else {
                            let causes_json = stored_causes_str
                                .as_deref()
                                .and_then(|causes| serde_json::from_str::<Value>(causes).ok())
                                .unwrap_or(Value::Array(vec![]));
                            (
                                stored_outcome.unwrap_or_else(|| "Unknown".to_string()),
                                stored_degraded.unwrap_or(false),
                                stored_total_turns.unwrap_or(0),
                                causes_json,
                                stored_cache_hit_ratio.unwrap_or(0.0),
                            )
                        };

                    let primary_cause = if degraded {
                        causes_json
                            .as_array()
                            .and_then(|causes| causes.first())
                            .and_then(|cause| cause.get("cause_type"))
                            .and_then(|cause_type| cause_type.as_str())
                            .unwrap_or("")
                            .to_string()
                    } else {
                        String::new()
                    };
                    let billed = billing.get(&session_id);
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
                        cache_hit_ratio,
                        model,
                        requested_model,
                        served_model,
                    )
                },
            )
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
            "SELECT r.session_id, s.started_at, s.model, d.completed_at, d.outcome, \
             r.initial_prompt, r.final_response_summary, \
             (SELECT req.requested_model FROM requests req \
              WHERE req.session_id = r.session_id \
                AND req.provider = 'codex_responses' \
                AND req.requested_model IS NOT NULL \
              ORDER BY req.timestamp DESC LIMIT 1), \
             (SELECT req.served_model FROM requests req \
              WHERE req.session_id = r.session_id \
                AND req.provider = 'codex_responses' \
                AND req.served_model IS NOT NULL \
              ORDER BY req.timestamp DESC LIMIT 1) \
             FROM session_recall r \
             LEFT JOIN sessions s ON r.session_id = s.session_id \
             LEFT JOIN session_diagnoses d ON r.session_id = d.session_id \
             WHERE d.completed_at >= ?1 \
                OR (d.completed_at IS NULL AND (s.started_at >= ?1 OR s.started_at IS NULL)) \
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

async fn handle_cache_rebuilds(
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let days: u64 = params.get("days").and_then(|d| d.parse().ok()).unwrap_or(7);

    let result = tokio::task::spawn_blocking(move || {
        let conn = Connection::open(db_path()).ok()?;
        let _ = repair_persisted_session_artifacts(&conn);
        let since_secs = now_epoch_secs() - (days * 86400);
        let since = epoch_to_iso8601(since_secs);

        // Find turns where cache was fully rebuilt (creation > 0, read == 0).
        let mut stmt = conn
            .prepare(
                "SELECT turn_number, cache_creation_tokens, cache_read_tokens, gap_from_prev_secs, \
             input_tokens \
             FROM turn_snapshots WHERE timestamp >= ?1",
            )
            .ok()?;

        let mut total_rebuilds: u64 = 0;
        let mut idle_gap_rebuilds: u64 = 0;
        let mut rebuilds_without_idle_gap: u64 = 0;
        let mut cold_start_builds: u64 = 0;
        let mut tokens_wasted: u64 = 0;
        let mut total_tokens: u64 = 0;
        let mut longest_gap: f64 = 0.0;

        let rows = stmt
            .query_map(rusqlite::params![since], |row| {
                Ok((
                    row.get::<_, i64>(0)?, // turn_number
                    row.get::<_, i64>(1)?, // cache_creation_tokens
                    row.get::<_, i64>(2)?, // cache_read_tokens
                    row.get::<_, f64>(3)?, // gap_from_prev_secs
                    row.get::<_, i64>(4)?, // input_tokens
                ))
            })
            .ok()?;

        for row in rows.flatten() {
            let (turn_number, cache_create, cache_read, gap, input) = row;
            total_tokens += (input + cache_read + cache_create) as u64;

            if cache_create > 0 && cache_read == 0 {
                if turn_number <= 1 {
                    cold_start_builds += 1;
                    continue;
                }
                total_rebuilds += 1;
                tokens_wasted += cache_create as u64;
                if gap > 300.0 {
                    idle_gap_rebuilds += 1;
                    if gap > longest_gap {
                        longest_gap = gap;
                    }
                } else {
                    rebuilds_without_idle_gap += 1;
                }
            }
        }

        let wasted_ratio = if total_tokens > 0 {
            tokens_wasted as f64 / total_tokens as f64
        } else {
            0.0
        };

        Some(serde_json::json!({
            "period_days": days,
            "total_rebuilds": total_rebuilds,
            "cold_start_builds": cold_start_builds,
            "rebuilds_from_idle_gaps": idle_gap_rebuilds,
            "rebuilds_without_idle_gap": rebuilds_without_idle_gap,
            "tokens_wasted_on_rebuilds": tokens_wasted,
            "tokens_wasted_ratio": (wasted_ratio * 1000.0).round() / 1000.0,
            "longest_gap_before_rebuild_secs": longest_gap.round() as u64,
        }))
    })
    .await
    .ok()
    .flatten()
    .unwrap_or(serde_json::json!({"error": "db unavailable"}));

    (
        StatusCode::OK,
        [("content-type", "application/json")],
        serde_json::to_string_pretty(&result).unwrap_or_default(),
    )
}

fn load_degradation_view_from_db(conn: &Connection, session_id: &str) -> Option<Value> {
    let _ = repair_persisted_session_artifacts(conn);

    let diag: Option<(bool, Option<i64>)> = conn
        .prepare("SELECT degraded, degradation_turn FROM session_diagnoses WHERE session_id = ?1")
        .ok()?
        .query_row(rusqlite::params![session_id], |row| {
            Ok((row.get::<_, i32>(0)? != 0, row.get::<_, Option<i64>>(1)?))
        })
        .ok();

    let (degraded, degradation_turn) = diag.unwrap_or((false, None));
    let degradation_turn = if degraded { degradation_turn } else { None };

    let mut stmt = conn
        .prepare(
            "SELECT turn_number, input_tokens, cache_read_tokens, cache_creation_tokens, \
             output_tokens, ttft_ms, gap_from_prev_secs, context_utilization, \
             context_window_tokens, tool_failures, requested_model, actual_model \
             FROM turn_snapshots WHERE session_id = ?1 ORDER BY turn_number",
        )
        .ok()?;

    struct TurnRow {
        turn: i64,
        input: i64,
        cache_read: i64,
        cache_create: i64,
        output: i64,
        ttft_ms: i64,
        gap: f64,
        ctx: f64,
        context_window_tokens: i64,
        failures: i64,
        requested_model: Option<String>,
        actual_model: Option<String>,
    }

    let turns: Vec<TurnRow> = stmt
        .query_map(rusqlite::params![session_id], |row| {
            let input = row.get::<_, i64>(1)?.max(0);
            let cache_read = row.get::<_, i64>(2)?.max(0);
            let cache_create = row.get::<_, i64>(3)?.max(0);
            let requested_model = row.get::<_, Option<String>>(10)?;
            let actual_model = row.get::<_, Option<String>>(11)?;
            let context_window_tokens = row
                .get::<_, Option<i64>>(8)?
                .map(|value| value.max(0))
                .filter(|value| *value > 0)
                .unwrap_or_else(|| {
                    infer_context_window_tokens(
                        requested_model.as_deref(),
                        actual_model.as_deref(),
                        input.max(0) as u64,
                        cache_read.max(0) as u64,
                        cache_create.max(0) as u64,
                    ) as i64
                });
            Ok(TurnRow {
                turn: row.get(0)?,
                input,
                cache_read,
                cache_create,
                output: row.get(4)?,
                ttft_ms: row.get(5)?,
                gap: row.get(6)?,
                ctx: context_fill_ratio(
                    input.max(0) as u64,
                    cache_read.max(0) as u64,
                    cache_create.max(0) as u64,
                    context_window_tokens.max(1) as u64,
                ),
                context_window_tokens,
                failures: row.get(9)?,
                requested_model,
                actual_model,
            })
        })
        .ok()?
        .filter_map(|r| r.ok())
        .collect();

    if turns.is_empty() {
        return Some(serde_json::json!({
            "session_id": session_id,
            "degraded": degraded,
            "degradation_turn": degradation_turn,
            "total_turns": 0,
            "turns": [],
        }));
    }

    let mut ttfts: Vec<i64> = turns.iter().map(|t| t.ttft_ms).collect();
    ttfts.sort();
    let median_ttft = ttfts[ttfts.len() / 2] as f64;

    let turn_data: Vec<Value> = turns
        .iter()
        .map(|t| {
            let mut flags: Vec<&str> = Vec::new();
            if t.cache_create > 0 && t.cache_read == 0 {
                if t.turn <= 1 {
                    flags.push("cold_start");
                } else if t.gap > 300.0 {
                    flags.push("cache_miss_ttl");
                } else {
                    flags.push("cache_miss_thrash");
                }
            }
            if t.ctx > 0.60 {
                flags.push("context_bloat");
            }
            if median_ttft > 0.0 && t.ttft_ms as f64 > median_ttft * 2.0 {
                flags.push("latency_spike");
            }
            if t.failures > 0 {
                flags.push("tool_failures");
            }
            if let (Some(requested), Some(actual)) =
                (t.requested_model.as_deref(), t.actual_model.as_deref())
            {
                if !model_matches(requested, actual) {
                    flags.push("model_fallback");
                }
            }
            serde_json::json!({
                "turn": t.turn,
                "input_tokens": t.input,
                "cache_read_tokens": t.cache_read,
                "cache_creation_tokens": t.cache_create,
                "output_tokens": t.output,
                "turn_duration_ms": t.ttft_ms,
                "gap_from_prev_secs": t.gap,
                "context_utilization": (t.ctx * 1000.0).round() / 1000.0,
                "context_window_tokens": t.context_window_tokens,
                "tool_failures": t.failures,
                "requested_model": t.requested_model.clone(),
                "actual_model": t.actual_model.clone(),
                "served_model": t.actual_model.clone(),
                "flags": flags,
            })
        })
        .collect();

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

async fn http_server() {
    let app = Router::new()
        .route("/health", get(handle_health))
        .route("/metrics", get(handle_metrics))
        .route("/api/summary", get(handle_summary))
        .route("/api/recall", get(handle_recall))
        .route(
            "/api/billing-reconciliations",
            post(handle_billing_reconciliations),
        )
        .route("/api/diagnosis/:session_id", get(handle_diagnosis))
        .route("/api/degradation/:session_id", get(handle_degradation))
        .route("/api/cache-rebuilds", get(handle_cache_rebuilds))
        .route("/api/sessions", get(handle_sessions))
        .route("/watch", get(handle_watch));

    let addr = std::env::var("CODITOR_HTTP_ADDR").unwrap_or_else(|_| "0.0.0.0:9090".to_string());
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|err| panic!("failed to bind HTTP server at {addr}: {err}"));
    let bound_addr = listener
        .local_addr()
        .map(|addr| addr.to_string())
        .unwrap_or_else(|_| addr.clone());
    info!("HTTP server listening on {bound_addr} (/health, /metrics, /api/summary, /api/recall, /api/billing-reconciliations, /api/sessions, /api/cache-rebuilds, /api/degradation, /watch)");
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

/// Sample quota burn every 30s so the burn-rate projection and Prometheus
/// gauges stay fresh, but only broadcast the user-facing watch snapshot every
/// five minutes unless the exhaustion alarm flips state. This is computed
/// entirely from Envoy-derived local counters. If the user sets
/// `CODITOR_WEEKLY_TOKEN_BUDGET` we can also surface remaining + projected
/// exhaustion; otherwise we auto-suggest a weekly cap from the last four
/// completed weeks of SQLite history.
async fn quota_burn_monitor() {
    let mut last_broadcast_at: Option<Instant> = None;
    let mut last_alarm: Option<bool> = None;

    loop {
        tokio::time::sleep(QUOTA_SAMPLE_INTERVAL).await;

        // Snapshot the cumulative token counter.
        let total_tokens = {
            let runtime = RUNTIME_STATE.lock().unwrap();
            runtime.total_tokens
        };

        let per_sec = {
            let mut t = BURN_TRACKER.lock().unwrap();
            t.record(total_tokens);
            t.tokens_per_sec()
        };
        let used_this_week = this_week_tokens_used();
        let weekly = env_u64("CODITOR_WEEKLY_TOKEN_BUDGET", 0);
        let (tokens_limit, budget_source) = if weekly > 0 {
            (Some(weekly), Some("env".to_string()))
        } else if let Some(auto) = auto_weekly_budget_suggestion() {
            (Some(auto.tokens_limit), Some("auto_p95_4w".to_string()))
        } else {
            (None, None)
        };
        let remaining = tokens_limit.map(|limit| limit.saturating_sub(used_this_week));
        let projected_exhaustion_secs = match (remaining, per_sec) {
            (Some(r), Some(rate)) if rate > 0.0 && r > 0 => Some((r as f64 / rate).round() as u64),
            _ => None,
        };

        // Skip watch broadcast until we've actually seen traffic — no point
        // telling the orchestrator "0 tokens, 0 burn" before the first turn.
        if total_tokens == 0 {
            continue;
        }

        let _ = per_sec; // currently displayed indirectly via projection
        let seconds_to_reset = Some(seconds_until_weekly_reset());
        let alarm = projected_exhaustion_secs
            .zip(seconds_to_reset)
            .map(|(exhaustion, reset)| exhaustion < reset)
            .unwrap_or(false);
        let now = Instant::now();
        let should_broadcast = should_broadcast_quota_snapshot(
            last_broadcast_at.map(|sent_at| now.duration_since(sent_at)),
            last_alarm,
            alarm,
        );
        last_alarm = Some(alarm);
        if !should_broadcast {
            continue;
        }
        last_broadcast_at = Some(now);

        watch::BROADCASTER.broadcast(watch::WatchEvent::RateLimitStatus {
            seconds_to_reset,
            requests_remaining: None,
            requests_limit: None,
            input_tokens_remaining: None,
            output_tokens_remaining: None,
            tokens_used_this_week: Some(used_this_week),
            tokens_limit,
            tokens_remaining: remaining,
            budget_source,
            projected_exhaustion_secs,
        });
    }
}

async fn historical_metrics_monitor() {
    loop {
        let refreshed_at_epoch = now_epoch_secs();
        let refresh = tokio::task::spawn_blocking(move || {
            let conn = Connection::open(db_path())?;
            let _ = repair_persisted_session_artifacts(&conn);
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

/// Sum of tokens spent this week. Reads from SQLite so we don't lose history
/// across core restarts. Weeks start Monday 00:00 UTC.
fn this_week_tokens_used() -> u64 {
    let week_start = start_of_week_iso();
    tokio::task::block_in_place(|| {
        let conn = match Connection::open(db_path()) {
            Ok(c) => c,
            Err(_) => return 0u64,
        };
        let mut stmt = match conn.prepare(
            "SELECT COALESCE(SUM(input_tokens + output_tokens + cache_read_tokens + cache_creation_tokens), 0) \
             FROM requests WHERE timestamp >= ?1",
        ) {
            Ok(s) => s,
            Err(_) => return 0u64,
        };
        stmt.query_row(rusqlite::params![week_start], |row| row.get::<_, i64>(0))
            .map(|n| n.max(0) as u64)
            .unwrap_or(0)
    })
}

/// Seconds until Monday 00:00 UTC.
fn seconds_until_weekly_reset() -> u64 {
    let now = now_epoch_secs();
    let next_reset = start_of_week_epoch_at(now) + 7 * 86_400;
    next_reset.saturating_sub(now)
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
                "DELETE FROM tool_outcomes WHERE timestamp < ?1",
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
// Per-session monitors: inactivity timeout and cache expiry warning.
// Both use periodic scanning of diagnosis::SESSIONS instead of a global Notify.
// ---------------------------------------------------------------------------
async fn cache_expiry_warning_monitor() {
    let warning_secs = env_u64("CODITOR_CACHE_WARNING_SECS", 240);
    let ttl_secs: u64 = 300;
    let check_interval = Duration::from_secs(30);

    loop {
        tokio::time::sleep(check_interval).await;
        let now = Instant::now();

        for mut entry in diagnosis::SESSIONS.iter_mut() {
            if !entry.session_inserted || entry.cache_warning_sent {
                continue;
            }
            let idle = now.duration_since(entry.last_activity).as_secs();
            if idle >= warning_secs {
                entry.cache_warning_sent = true;
                let remaining = ttl_secs.saturating_sub(idle);
                watch::BROADCASTER.broadcast(watch::WatchEvent::CacheWarning {
                    session_id: entry.session_id.clone(),
                    idle_secs: idle,
                    ttl_secs: remaining,
                });
                info!(session_id = %entry.session_id, idle_secs = idle, remaining_secs = remaining,
                    "cache expiry warning broadcast");
            }
        }
    }
}

async fn session_inactivity_monitor() {
    let timeout_mins = env_u64("CODITOR_SESSION_TIMEOUT_MINUTES", 5);
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

    info!("coditor-core v{}", env!("CARGO_PKG_VERSION"));
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
    tokio::spawn(cache_expiry_warning_monitor());
    tokio::spawn(data_retention_cleanup());
    tokio::spawn(quota_burn_monitor());
    tokio::spawn(historical_metrics_monitor());

    let addr = std::env::var("CODITOR_GRPC_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:50051".to_string())
        .parse()?;
    info!(%addr, "gRPC ext_proc server starting");

    Server::builder()
        .add_service(ExternalProcessorServer::new(CoditorProcessor))
        .serve(addr)
        .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::{LazyLock, Mutex};
    use std::time::Duration;
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    use rusqlite::Connection;

    use super::{
        build_codex_finalization_outcome, build_diagnosis_response_json,
        build_session_summary_json, build_sessions_response_json, build_summary_response_json,
        codex_request_headers_from_ext_proc, codex_response_headers_from_ext_proc,
        codex_watch_event_is_duplicate_or_remember, compact_response_summary, context_fill_percent,
        context_fill_ratio, db_writer_loop, derive_display_name, diagnosis,
        ensure_codex_persistence_columns, ensure_session_columns, epoch_to_iso8601, extract_header,
        extract_headers, infer_context_window_tokens, load_degradation_view_from_db,
        load_persisted_watch_replay_events, load_turn_snapshots_from_db,
        looks_like_machine_recall_line, metrics, normalize_search_text, now_epoch_secs,
        parse_request_body_metadata, persist_billing_reconciliation,
        persisted_session_display_name, pricing, query_historical_metrics, query_summary,
        record_codex_turn_command, repair_persisted_session_artifacts,
        repair_turn_snapshot_context_windows, repo_name_from_codex_initial_prompt,
        score_recall_doc, seed_live_metric_labels_from_db, session_timeout_secs,
        should_broadcast_quota_snapshot, should_skip_chatgpt_auxiliary_request_body, table_columns,
        tokenize_search_text, BillingReconciliationInput, BillingReconciliationWriteError,
        DbCommand, HttpHeaders, ProtoHeaderValue, RequestMetadataSource,
        SelectedFinalizationOutcome, SelectedResponseAccumulator, SummaryWindowData,
        ESTIMATED_COST_SOURCE, EXTENDED_CONTEXT_WINDOW_TOKENS, QUOTA_WATCH_BROADCAST_INTERVAL,
        SCHEMA, STANDARD_CONTEXT_WINDOW_TOKENS,
    };

    static METRICS_TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

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
                cache_event TEXT
            );
            CREATE TABLE session_diagnoses (
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
                response_summary TEXT
            );
            CREATE TABLE tool_outcomes (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                turn_number INTEGER NOT NULL,
                timestamp TEXT NOT NULL,
                tool_name TEXT NOT NULL,
                input_summary TEXT,
                outcome TEXT,
                duration_ms INTEGER
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
        assert_eq!(metadata.working_dir, "/Users/pradeepsingh/code/coditor");
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
          "metadata": { "cwd": "/tmp/coditor-hot-path" }
        }"#;
        let second = br#"{
          "model": "gpt-codex-fixture",
          "input": "second codex task",
          "metadata": { "cwd": "/tmp/coditor-hot-path" }
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
                    "Workspace packages: coditor-core and coditor-cli."
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
        assert!(!outcome
            .watch_events
            .iter()
            .any(|event| matches!(event, super::watch::WatchEvent::CacheEvent { .. })));
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
        assert!(super::SESSION_BUDGETS.get("phase-4b-session-005").is_none());

        let _ = diagnosis::SESSIONS.remove(&super::codex_request::fallback_session_hash(
            "",
            "phase-4b-session-005",
        ));
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
        assert!(turn_columns.contains("request_id"));
        assert!(turn_columns.contains("codex_status"));
        assert!(turn_columns.contains("codex_cached_input_tokens"));
        assert!(turn_columns.contains("codex_reasoning_output_tokens"));
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
        assert_eq!(session_display_name, "coditor");
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
            "Workspace packages: coditor-core and coditor-cli."
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
          "metadata": { "cwd": "/tmp/coditor-parallel" }
        }"#;
        let second = br#"{
          "model": "gpt-codex-fixture",
          "input": "summarize repository docs",
          "metadata": { "cwd": "/tmp/coditor-parallel" }
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
    fn codex_persistence_failed_and_incomplete_statuses_are_stored() {
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
        let statuses = conn
            .prepare("SELECT codex_status FROM requests ORDER BY request_id")
            .expect("prepare status query")
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query statuses")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect statuses");
        let turns: i64 = conn
            .query_row("SELECT COUNT(*) FROM turn_snapshots", [], |row| row.get(0))
            .expect("count turns");

        assert_eq!(
            statuses,
            vec!["failed".to_string(), "incomplete".to_string()]
        );
        assert_eq!(turns, 2);

        drop(conn);
        cleanup_test_db(&path);
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
            derive_display_name("/Users/pradeep/code/coditor", "gpt-5.5", 0xabc),
            "coditor"
        );

        let hash = 0xabc_u64;
        diagnosis::SESSIONS.insert(
            hash,
            diagnosis::SessionState {
                session_id: "session_existing".to_string(),
                display_name: "coditor".to_string(),
                model: "gpt-5.5".to_string(),
                initial_prompt: None,
                created_at: Instant::now(),
                last_activity: Instant::now(),
                session_inserted: true,
                cache_warning_sent: false,
            },
        );
        let display = derive_display_name("/tmp/coditor", "gpt-5.5", hash);
        let _ = diagnosis::SESSIONS.remove(&hash);
        assert_eq!(display, "coditor-abc");
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
        .expect("insert codex turn snapshot");
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
            "gpt-5.5",
            Some("# AGENTS.md instructions for /Users/pradeepsingh/code/coditor"),
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
            } if session_id == "codex-watch-db" && display_name == "coditor" && model == "gpt-5.5"
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
    }

    #[test]
    fn codex_diagnosis_ignores_non_envoy_tool_and_mcp_failures_from_db() {
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
        for idx in 1..=3 {
            conn.execute(
                "INSERT INTO tool_outcomes (
                    session_id, turn_number, timestamp, tool_name, input_summary, outcome, duration_ms
                ) VALUES (?1, 0, ?2, 'shell', '', 'failed', 1)",
                rusqlite::params!["codex-db-hooks", format!("2026-04-30T12:00:0{idx}Z")],
            )
            .expect("insert hook tool failure");
        }
        for idx in 1..=2 {
            conn.execute(
                "INSERT INTO mcp_events (
                    session_id, timestamp, server, tool, event_type, source, detail
                ) VALUES (?1, ?2, 'github', 'get_issue', 'failed', 'hook', 'fixture')",
                rusqlite::params!["codex-db-hooks", format!("2026-04-30T12:01:0{idx}Z")],
            )
            .expect("insert mcp failure");
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

    fn unique_test_db_path(label: &str) -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time went backwards")
            .as_nanos();
        let mut path = std::env::temp_dir();
        path.push(format!("coditor-{label}-{}-{nanos}.db", std::process::id()));
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

    fn insert_diagnosis(
        conn: &Connection,
        session_id: &str,
        completed_at: &str,
        degraded: bool,
        causes_json: &str,
    ) {
        conn.execute(
            "INSERT INTO session_diagnoses (
                session_id, completed_at, outcome, total_turns, total_cost, cache_hit_ratio,
                degraded, degradation_turn, causes_json, advice_json
            ) VALUES (?1, ?2, 'Completed', 3, 1.0, 0.5, ?3, NULL, ?4, '[]')",
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
            assert_eq!(window.estimated_spend_dollars, 0.0);
            assert!(window.estimated_spend_dollars_by_model.is_empty());
            assert!(window
                .avg_estimated_session_cost_dollars_by_model
                .is_empty());
            assert_eq!(window.cache_hit_ratio, 0.0);
            assert_eq!(window.degraded_sessions, 0);
            assert_eq!(window.degraded_session_ratio, 0.0);
            assert!(window.degraded_causes.is_empty());
        }
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
        assert_eq!(one_day.estimated_spend_dollars, 0.0);
        assert_eq!(one_day.degraded_sessions, 0);
        assert!(one_day.degraded_causes.is_empty());

        assert_eq!(seven_day.sessions, 1);
        assert_eq!(seven_day.degraded_sessions, 1);
        let expected =
            pricing::estimate_cost_dollars("gpt-5.4", 1_000_000, 0, 40, 10).total_cost_dollars;
        assert!((seven_day.estimated_spend_dollars - expected).abs() < 1e-9);
        assert_eq!(
            seven_day.estimated_spend_dollars_by_model.get("gpt-5.4"),
            Some(&expected)
        );
        assert_eq!(
            seven_day.degraded_causes.get("codex_high_context_fill"),
            Some(&1)
        );
    }

    #[test]
    fn historical_metrics_aggregate_estimated_spend_cache_and_causes() {
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
        let expected = pricing::estimate_cost_dollars("gpt-5.5", 500_000, 0, 20, 10)
            .total_cost_dollars
            + pricing::estimate_cost_dollars("gpt-5.4", 625_000, 0, 0, 30).total_cost_dollars
            + pricing::estimate_cost_dollars("gpt-5.5", 666_666, 0, 10, 10).total_cost_dollars;
        assert!((one_day.estimated_spend_dollars - expected).abs() < 1e-9);
        let gpt55_expected = pricing::estimate_cost_dollars("gpt-5.5", 500_000, 0, 20, 10)
            .total_cost_dollars
            + pricing::estimate_cost_dollars("gpt-5.5", 666_666, 0, 10, 10).total_cost_dollars;
        let gpt54_expected =
            pricing::estimate_cost_dollars("gpt-5.4", 625_000, 0, 0, 30).total_cost_dollars;
        assert_eq!(
            one_day.estimated_spend_dollars_by_model.get("gpt-5.5"),
            Some(&gpt55_expected)
        );
        assert_eq!(
            one_day.estimated_spend_dollars_by_model.get("gpt-5.4"),
            Some(&gpt54_expected)
        );
        assert_eq!(
            one_day
                .avg_estimated_session_cost_dollars_by_model
                .get("gpt-5.5"),
            Some(&1.0)
        );
        assert!((one_day.cache_hit_ratio - 0.375).abs() < 1e-9);
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
                estimated_spend_dollars: 12.5,
                estimated_spend_dollars_by_model: std::collections::BTreeMap::from([(
                    "gpt-5.5", 12.5,
                )]),
                avg_estimated_session_cost_dollars_by_model: std::collections::BTreeMap::from([(
                    "gpt-5.5", 2.5,
                )]),
                estimated_cache_waste_dollars_by_model: std::collections::BTreeMap::from([(
                    "gpt-5.5", 0.75,
                )]),
                cache_hit_ratio: 0.42,
                cache_events: std::collections::BTreeMap::from([("miss_thrash", 2)]),
                degraded_sessions: 2,
                degraded_session_ratio: 0.4,
                degraded_causes: causes,
                model_fallbacks: std::collections::BTreeMap::from([(("gpt-5.4", "gpt-5.5"), 1)]),
                tool_failures_by_tool: std::collections::BTreeMap::from([("bash".to_string(), 4)]),
            }],
            1_776_700_000,
        );

        let (_, body) = metrics::render().expect("render metrics");
        assert!(body
            .contains("coditor_model_fallback_total{actual=\"gpt-5.5\",requested=\"gpt-5.4\"} 0"));
        for dropped_metric in [
            "coditor_history_",
            "coditor_cache_events_total",
            "coditor_estimated_",
            "coditor_tool_failures_total",
            "coditor_mcp_",
            "coditor_skill_events_total",
            "coditor_active_sessions",
            "coditor_weekly_tokens",
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
            "INSERT INTO tool_outcomes (session_id, turn_number, timestamp, tool_name, outcome) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                "session_1",
                1,
                "2026-01-01T00:00:00Z",
                "Read File",
                "success"
            ],
        )
        .expect("insert non-Envoy tool outcome");
        conn.execute(
            "INSERT INTO tool_calls (request_id, timestamp, tool_name) VALUES (?1, ?2, ?3)",
            rusqlite::params!["req_1", "2026-01-01T00:00:01Z", "mcp__github__get_issue"],
        )
        .expect("insert mcp tool call");
        conn.execute(
            "INSERT INTO skill_events (session_id, timestamp, skill_name, event_type, confidence, source) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                "session_1",
                "2026-01-01T00:00:02Z",
                "openai-docs",
                "fired",
                1.0,
                "hook"
            ],
        )
        .expect("insert non-Envoy skill event");
        conn.execute(
            "INSERT INTO mcp_events (session_id, timestamp, server, tool, event_type, source) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                "session_1",
                "2026-01-01T00:00:03Z",
                "github",
                "get_issue",
                "called",
                "hook"
            ],
        )
        .expect("insert non-Envoy MCP event");

        seed_live_metric_labels_from_db(&conn).expect("seed tool labels");

        let (_, body) = metrics::render().expect("render metrics");
        assert!(body.contains("coditor_tool_calls_total{tool=\"bash\"} 0"));
        assert!(body.contains("coditor_tool_calls_total{tool=\"mcp__github__get_issue\"} 0"));
        assert!(!body.contains("coditor_tool_calls_total{tool=\"read_file\"}"));
        for dropped_metric in [
            "coditor_tool_failures_total",
            "coditor_mcp_",
            "coditor_skill_events_total",
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

        let tool_name = "mcp__metricstest_server__lookup_widget";
        metrics::record_tool_call(tool_name);

        let (_, body) = metrics::render().expect("render metrics");
        assert!(body.contains(
            "coditor_tool_calls_total{tool=\"mcp__metricstest_server__lookup_widget\"} 1"
        ));
        assert!(!body.contains("coditor_tool_failures_total"));
        assert!(!body.contains("coditor_mcp_"));
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
                estimated_spend_dollars: 1.0,
                estimated_spend_dollars_by_model: std::collections::BTreeMap::from([(
                    "gpt-5.5", 1.0,
                )]),
                avg_estimated_session_cost_dollars_by_model: std::collections::BTreeMap::from([(
                    "gpt-5.5", 0.5,
                )]),
                estimated_cache_waste_dollars_by_model: std::collections::BTreeMap::from([(
                    "gpt-5.5", 0.25,
                )]),
                cache_hit_ratio: 0.25,
                cache_events: std::collections::BTreeMap::from([("miss_thrash", 2)]),
                degraded_sessions: 1,
                degraded_session_ratio: 0.5,
                degraded_causes: causes,
                model_fallbacks: std::collections::BTreeMap::from([(("gpt-5.4", "gpt-5.5"), 1)]),
                tool_failures_by_tool: std::collections::BTreeMap::from([("bash".to_string(), 2)]),
            }],
            1_776_700_000,
        );

        metrics::update_historical_gauges(
            &[metrics::HistoricalWindowMetrics {
                window: "7d",
                sessions: 1,
                estimated_spend_dollars: 0.5,
                estimated_spend_dollars_by_model: std::collections::BTreeMap::new(),
                avg_estimated_session_cost_dollars_by_model: std::collections::BTreeMap::new(),
                estimated_cache_waste_dollars_by_model: std::collections::BTreeMap::new(),
                cache_hit_ratio: 0.0,
                cache_events: std::collections::BTreeMap::new(),
                degraded_sessions: 0,
                degraded_session_ratio: 0.0,
                degraded_causes: std::collections::BTreeMap::new(),
                model_fallbacks: std::collections::BTreeMap::new(),
                tool_failures_by_tool: std::collections::BTreeMap::new(),
            }],
            1_776_700_100,
        );

        let (_, body) = metrics::render().expect("render metrics");
        assert!(!body.contains("coditor_history_"));
        assert!(!body.contains("coditor_estimated_"));
        assert!(!body.contains("coditor_cache_events_total"));
        assert!(!body.contains("coditor_tool_failures_total"));
    }

    #[test]
    fn summary_response_uses_estimated_cost_fields_only() {
        let today = SummaryWindowData {
            sessions: 2,
            estimated_cost_dollars: 12.345,
            cost_source: "pricing_file:test-contract".to_string(),
            trusted_for_budget_enforcement: true,
            billed_cost_dollars: Some(10.25),
            billed_sessions: 1,
            cache_hit_ratio: 0.5,
        };
        let week = SummaryWindowData {
            sessions: 3,
            estimated_cost_dollars: 20.0,
            cost_source: pricing::MIXED_COST_SOURCE.to_string(),
            trusted_for_budget_enforcement: false,
            billed_cost_dollars: None,
            billed_sessions: 0,
            cache_hit_ratio: 0.25,
        };
        let month = SummaryWindowData {
            sessions: 4,
            estimated_cost_dollars: 30.0,
            cost_source: ESTIMATED_COST_SOURCE.to_string(),
            trusted_for_budget_enforcement: false,
            billed_cost_dollars: Some(18.0),
            billed_sessions: 2,
            cache_hit_ratio: 0.75,
        };
        let expected_source = pricing::active_catalog_source();
        let json = build_summary_response_json(&today, &week, &month);
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
        assert!(today.get("cost").is_none());
    }

    #[test]
    fn diagnosis_response_uses_estimated_cost_fields_only() {
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
            0.4,
            true,
            Some(2),
            serde_json::json!([]),
            serde_json::json!(["Retry less"]),
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
        assert!(json.get("total_cost_dollars").is_none());
    }

    #[test]
    fn session_summary_uses_estimated_cost_fields_only() {
        let json = build_session_summary_json(
            "session_1".to_string(),
            "coditor".to_string(),
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
            "cache_miss_ttl".to_string(),
            0.4,
            Some("gpt-5.5".to_string()),
            Some("gpt-5.5".to_string()),
            Some("gpt-5.5".to_string()),
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
            Some("coditor")
        );
        assert_eq!(
            json.get("requested_model").and_then(|v| v.as_str()),
            Some("gpt-5.5")
        );
        assert_eq!(
            json.get("served_model").and_then(|v| v.as_str()),
            Some("gpt-5.5")
        );
        assert!(json.get("total_cost_dollars").is_none());
    }

    #[test]
    fn sessions_response_exposes_cost_source_once_at_root() {
        let sessions = vec![build_session_summary_json(
            "session_1".to_string(),
            "coditor".to_string(),
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
            "cache_miss_ttl".to_string(),
            0.4,
            Some("gpt-5.5".to_string()),
            None,
            None,
        )];
        let json = build_sessions_response_json(sessions);

        let expected_source = pricing::active_catalog_source();
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
        insert_request(
            &conn,
            "req-a",
            "session-a",
            &one_hour_ago,
            "gpt-5.5",
            1_000_000,
            0,
            0,
            0,
        );
        insert_request(
            &conn,
            "req-b",
            "session-b",
            &one_hour_ago,
            "gpt-5.4",
            500_000,
            0,
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
        assert!((summary.estimated_cost_dollars - 6.25).abs() < 1e-9);
    }

    #[test]
    fn query_summary_excludes_internal_request_groups_from_session_count() {
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
        insert_request(
            &conn,
            "req-real",
            "session-real",
            &one_hour_ago,
            "gpt-5.5",
            1_000_000,
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
        assert!(summary.estimated_cost_dollars > 0.0);
    }

    #[test]
    fn repair_persisted_session_artifacts_reaches_older_missing_rows() {
        let conn = create_full_test_db();
        let cutoff_base = now_epoch_secs().saturating_sub(session_timeout_secs() + 3_600);

        for idx in 0..205u64 {
            let ts = epoch_to_iso8601(cutoff_base.saturating_sub(205 - idx));
            let session_id = format!("session-{idx:03}");
            insert_session(&conn, &session_id, &ts, Some(&ts), "gpt-5.5", None);
            insert_turn_snapshot(&conn, &session_id, 1, &ts, 0, 1000, 1000, 0.0, 0.10, None);
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
    fn degradation_view_repairs_missing_diagnosis_before_rendering() {
        let conn = create_full_test_db();
        let ended_at =
            epoch_to_iso8601(now_epoch_secs().saturating_sub(session_timeout_secs() + 3_600));
        insert_session(
            &conn,
            "session-ttl",
            &ended_at,
            Some(&ended_at),
            "gpt-5.5",
            Some("keep auth cache warm"),
        );
        insert_turn_snapshot(
            &conn,
            "session-ttl",
            1,
            &ended_at,
            0,
            1000,
            1000,
            0.0,
            0.10,
            None,
        );
        insert_turn_snapshot(
            &conn,
            "session-ttl",
            2,
            &ended_at,
            0,
            1000,
            3500,
            600.0,
            0.15,
            Some("Retried after cache expiry"),
        );
        insert_turn_snapshot(
            &conn,
            "session-ttl",
            3,
            &ended_at,
            900,
            0,
            1200,
            30.0,
            0.18,
            Some("Finished request"),
        );

        let json = load_degradation_view_from_db(&conn, "session-ttl").expect("degradation view");
        assert_eq!(json.get("degraded").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(
            json.get("degradation_turn").and_then(|v| v.as_i64()),
            Some(2)
        );
        assert_eq!(json.get("total_turns").and_then(|v| v.as_u64()), Some(3));
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
                session_id, completed_at, outcome, total_turns, total_cost, cache_hit_ratio,
                degraded, degradation_turn, causes_json, advice_json
            ) VALUES (?1, ?2, 'Likely Completed', 4, 1.5, 0.98, 0, 8, '[]', '[]')",
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

    #[test]
    fn cache_event_serializes_estimated_rebuild_cost_field_only() {
        let json = serde_json::to_value(crate::watch::WatchEvent::CacheEvent {
            session_id: "session_1".to_string(),
            event_type: "miss_ttl".to_string(),
            cache_expires_at_epoch: Some(1_776_700_000),
            estimated_rebuild_cost_dollars: Some(0.24),
        })
        .expect("serialize cache event");

        assert_eq!(
            json.get("estimated_rebuild_cost_dollars")
                .and_then(|v| v.as_f64()),
            Some(0.24)
        );
        assert!(json.get("rebuild_cost_dollars").is_none());
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

    #[test]
    fn quota_snapshot_broadcasts_immediately_before_any_prior_send() {
        assert!(should_broadcast_quota_snapshot(None, None, false));
    }

    #[test]
    fn quota_snapshot_stays_quiet_inside_broadcast_window_when_alarm_is_stable() {
        assert!(!should_broadcast_quota_snapshot(
            Some(Duration::from_secs(120)),
            Some(false),
            false,
        ));
    }

    #[test]
    fn quota_snapshot_broadcasts_when_window_elapses() {
        assert!(should_broadcast_quota_snapshot(
            Some(QUOTA_WATCH_BROADCAST_INTERVAL),
            Some(false),
            false,
        ));
    }

    #[test]
    fn quota_snapshot_broadcasts_when_alarm_flips_before_window_elapses() {
        assert!(should_broadcast_quota_snapshot(
            Some(Duration::from_secs(120)),
            Some(false),
            true,
        ));
    }
}
