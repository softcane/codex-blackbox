use std::collections::BTreeMap;
use std::sync::LazyLock;

use regex::Regex;
use rusqlite::{Connection, OptionalExtension};
use serde_json::{json, Value};

use super::diagnosis;

const PROVIDER: &str = "codex_responses";
const DETAIL_UNAVAILABLE: &str = "detail unavailable";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PostmortemTarget {
    Last,
    Session(String),
}

#[derive(Debug)]
pub(crate) enum PostmortemBuildError {
    NotFound,
    Sqlite(rusqlite::Error),
}

impl From<rusqlite::Error> for PostmortemBuildError {
    fn from(err: rusqlite::Error) -> Self {
        Self::Sqlite(err)
    }
}

#[derive(Clone, Debug, Default)]
struct SessionFacts {
    session_id: String,
    started_at: Option<String>,
    ended_at: Option<String>,
    display_name: Option<String>,
}

#[derive(Clone, Debug, Default)]
struct TurnEvidence {
    turn_number: u32,
    timestamp: String,
    request_id: Option<String>,
    requested_model: Option<String>,
    served_model: Option<String>,
    status: String,
    input_tokens: u64,
    cached_input_tokens: u64,
    uncached_input_tokens: u64,
    output_tokens: u64,
    reasoning_output_tokens: u64,
    response_id: Option<String>,
    prompt_excerpt: Option<String>,
    failure_detail: Option<String>,
    incomplete_detail: Option<String>,
    tool_calls: Vec<ToolIntent>,
    accounting_anomalies: Vec<Value>,
    response_summary: Option<String>,
    context_utilization: f64,
    context_window_tokens: Option<u64>,
    duration_ms: Option<u64>,
}

#[derive(Clone, Debug, Default)]
struct ToolIntent {
    id: Option<String>,
    name: String,
    input: Option<String>,
}

#[derive(Clone, Debug)]
struct DiagnosisSnapshot {
    outcome: String,
    total_turns: u32,
    degraded: bool,
    degradation_turn: Option<i64>,
    causes: Value,
    advice: Value,
}

#[derive(Clone, Debug, Default)]
struct ImpactTotals {
    input_tokens: u64,
    cached_input_tokens: u64,
    uncached_input_tokens: u64,
    output_tokens: u64,
    reasoning_output_tokens: u64,
    local_total_tokens: u64,
}

pub(crate) fn build_postmortem_report(
    conn: &Connection,
    target: PostmortemTarget,
    redact: bool,
) -> Result<Value, PostmortemBuildError> {
    super::repair_persisted_session_artifacts(conn)?;

    let session_id = resolve_target_session(conn, target)?;
    if !super::session_has_codex_evidence(conn, &session_id)? {
        return Err(PostmortemBuildError::NotFound);
    }

    let turns = load_turn_evidence(conn, &session_id)?;
    if turns.is_empty() {
        return Err(PostmortemBuildError::NotFound);
    }

    let session = load_session_facts(conn, &session_id, &turns)?;
    let estimated =
        super::compute_estimated_costs_for_sessions(conn, std::slice::from_ref(&session_id))?
            .remove(&session_id)
            .unwrap_or_else(|| super::CostAccumulator::new().finish());
    let billing =
        super::load_latest_billing_reconciliations(conn, std::slice::from_ref(&session_id))?
            .remove(&session_id);
    let diagnosis = load_diagnosis_snapshot(conn, &session_id, &estimated, &turns)?;
    let redactor = Redactor::new(redact);
    let impact = impact_totals(&turns);
    let signals = build_signals(&turns);
    let evidence = build_evidence(&turns, &redactor);
    let timeline = build_timeline(&session, &turns, diagnosis.degraded, &redactor);
    let recommendations = build_recommendations(&diagnosis, &turns, &redactor);
    let summary = build_summary(&session, &turns, &diagnosis, redact, &redactor);
    let primary = primary_cause(&diagnosis.causes);
    let evidence_origin = evidence_origin(&session_id, &turns);
    let caveats = caveats(evidence_origin);
    let restart_prompt = restart_prompt(&session_id, &diagnosis, &turns, &redactor);

    Ok(json!({
        "schema_version": 1,
        "report_type": "codex_responses_postmortem",
        "evidence_origin": evidence_origin,
        "session_id": session_id,
        "redacted": redact,
        "partial": session_is_partial(&session),
        "summary": summary,
        "diagnosis": {
            "degraded": diagnosis.degraded,
            "degradation_turn": if diagnosis.degraded { diagnosis.degradation_turn } else { None },
            "primary_cause": primary.as_ref().map(|cause| cause.label.clone()).unwrap_or_else(|| "none".to_string()),
            "primary_cause_type": primary.as_ref().map(|cause| cause.cause_type.clone()).unwrap_or_else(|| "none".to_string()),
            "cause_classification": primary.as_ref().map(|cause| cause.classification.clone()).unwrap_or_else(|| "none".to_string()),
            "confidence": primary.as_ref().map(|cause| cause.confidence.clone()).unwrap_or_else(|| "low".to_string()),
            "detail": primary.as_ref().map(|cause| redactor.redact(&cause.detail)).unwrap_or_else(|| "No failed or incomplete model response was observed.".to_string()),
            "next_action": recommendations.first().cloned().unwrap_or_else(|| "No immediate action is required from the observed model traffic.".to_string()),
            "causes": redactor.redact_value(diagnosis.causes.clone()),
            "advice": redactor.redact_value(diagnosis.advice.clone()),
        },
        "impact": {
            "input_tokens": impact.input_tokens,
            "cached_input_tokens": impact.cached_input_tokens,
            "uncached_input_tokens": impact.uncached_input_tokens,
            "output_tokens": impact.output_tokens,
            "reasoning_output_tokens": impact.reasoning_output_tokens,
            "local_total_tokens": impact.local_total_tokens,
            "local_estimated_cost_dollars": round_cost(estimated.estimated_cost_dollars),
            "local_estimate_source": estimated.cost_source,
            "local_estimate_trusted_for_budget_enforcement": estimated.trusted_for_budget_enforcement,
            "billed_reconciliation": billing.map(|record| json!({
                "billed_cost_dollars": round_cost(record.billed_cost_dollars),
                "source": redactor.redact(&record.source),
                "imported_at": record.imported_at,
            })),
        },
        "signals": signals,
        "evidence": evidence,
        "timeline": timeline,
        "recommendations": recommendations,
        "caveats": caveats,
        "restart_prompt": restart_prompt,
    }))
}

fn resolve_target_session(
    conn: &Connection,
    target: PostmortemTarget,
) -> Result<String, PostmortemBuildError> {
    match target {
        PostmortemTarget::Session(session_id) => Ok(session_id),
        PostmortemTarget::Last => {
            latest_codex_session_id(conn)?.ok_or(PostmortemBuildError::NotFound)
        }
    }
}

fn latest_codex_session_id(conn: &Connection) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT session_id FROM ( \
            SELECT session_id, timestamp AS observed_at FROM requests WHERE provider = ?1 \
            UNION ALL \
            SELECT session_id, timestamp AS observed_at FROM turn_snapshots WHERE provider = ?1 \
         ) WHERE session_id IS NOT NULL AND trim(session_id) != '' \
         ORDER BY observed_at DESC LIMIT 1",
        rusqlite::params![PROVIDER],
        |row| row.get::<_, String>(0),
    )
    .optional()
}

fn load_session_facts(
    conn: &Connection,
    session_id: &str,
    turns: &[TurnEvidence],
) -> rusqlite::Result<SessionFacts> {
    let stored = conn
        .query_row(
            "SELECT session_id, started_at, ended_at, display_name, initial_prompt \
             FROM sessions WHERE session_id = ?1",
            rusqlite::params![session_id],
            |row| {
                Ok(SessionFacts {
                    session_id: row.get::<_, String>(0)?,
                    started_at: row.get::<_, Option<String>>(1)?,
                    ended_at: row.get::<_, Option<String>>(2)?,
                    display_name: row.get::<_, Option<String>>(3)?,
                })
            },
        )
        .optional()?;

    let first_ts = turns.first().map(|turn| turn.timestamp.clone());
    let last_ts = turns.last().map(|turn| turn.timestamp.clone());

    Ok(stored.unwrap_or_else(|| SessionFacts {
        session_id: session_id.to_string(),
        started_at: first_ts,
        ended_at: last_ts,
        display_name: None,
    }))
}

fn load_turn_evidence(conn: &Connection, session_id: &str) -> rusqlite::Result<Vec<TurnEvidence>> {
    let mut turns = load_turn_evidence_from_snapshots(conn, session_id)?;
    if turns.is_empty() {
        turns = load_turn_evidence_from_requests(conn, session_id)?;
    }
    Ok(turns)
}

fn load_turn_evidence_from_snapshots(
    conn: &Connection,
    session_id: &str,
) -> rusqlite::Result<Vec<TurnEvidence>> {
    let mut stmt = conn.prepare(
        "SELECT t.turn_number, t.timestamp, t.request_id, \
                COALESCE(NULLIF(trim(t.requested_model), ''), NULLIF(trim(r.requested_model), '')), \
                COALESCE(NULLIF(trim(t.actual_model), ''), NULLIF(trim(r.served_model), '')), \
                COALESCE(NULLIF(trim(t.codex_status), ''), NULLIF(trim(r.codex_status), ''), 'unknown'), \
                t.codex_input_tokens, t.codex_cached_input_tokens, \
                t.codex_uncached_input_tokens, t.codex_output_tokens, \
                t.codex_reasoning_output_tokens, t.codex_total_tokens, \
                COALESCE(NULLIF(trim(t.codex_response_id), ''), NULLIF(trim(r.codex_response_id), '')), \
                COALESCE(NULLIF(trim(t.codex_prompt_excerpt), ''), NULLIF(trim(r.codex_prompt_excerpt), '')), \
                COALESCE(t.codex_failure_detail, r.codex_failure_detail), \
                COALESCE(t.codex_incomplete_detail, r.codex_incomplete_detail), \
                COALESCE(t.codex_tool_calls, r.codex_tool_calls), \
                COALESCE(t.codex_accounting_anomalies, r.codex_accounting_anomalies), \
                t.response_summary, t.context_utilization, t.context_window_tokens, t.ttft_ms \
         FROM turn_snapshots t \
         LEFT JOIN requests r ON r.request_id = t.request_id AND r.provider = ?2 \
         WHERE t.session_id = ?1 AND t.provider = ?2 \
         ORDER BY t.turn_number ASC, t.timestamp ASC",
    )?;

    let rows = stmt.query_map(rusqlite::params![session_id, PROVIDER], row_to_turn)?;
    rows.collect()
}

fn load_turn_evidence_from_requests(
    conn: &Connection,
    session_id: &str,
) -> rusqlite::Result<Vec<TurnEvidence>> {
    let mut stmt = conn.prepare(
        "SELECT rowid, timestamp, request_id, requested_model, served_model, \
                COALESCE(NULLIF(trim(codex_status), ''), 'unknown'), \
                codex_input_tokens, codex_cached_input_tokens, codex_uncached_input_tokens, \
                codex_output_tokens, codex_reasoning_output_tokens, codex_total_tokens, \
                codex_response_id, codex_prompt_excerpt, codex_failure_detail, \
                codex_incomplete_detail, codex_tool_calls, codex_accounting_anomalies, \
                NULL, NULL, NULL, duration_ms \
         FROM requests \
         WHERE session_id = ?1 AND provider = ?2 \
         ORDER BY timestamp ASC, request_id ASC",
    )?;

    let mut turns = stmt
        .query_map(rusqlite::params![session_id, PROVIDER], row_to_turn)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (idx, turn) in turns.iter_mut().enumerate() {
        turn.turn_number = (idx + 1) as u32;
    }
    Ok(turns)
}

fn row_to_turn(row: &rusqlite::Row<'_>) -> rusqlite::Result<TurnEvidence> {
    let input_tokens = row.get::<_, Option<i64>>(6)?.unwrap_or(0).max(0) as u64;
    let cached_input_tokens = row.get::<_, Option<i64>>(7)?.unwrap_or(0).max(0) as u64;
    let uncached_input_tokens = input_tokens.saturating_sub(cached_input_tokens);
    let output_tokens = row.get::<_, Option<i64>>(9)?.unwrap_or(0).max(0) as u64;
    let context_window_tokens = row
        .get::<_, Option<i64>>(20)?
        .map(|value| value.max(0) as u64)
        .filter(|value| *value > 0);
    let context_utilization = row
        .get::<_, Option<f64>>(19)?
        .unwrap_or_else(|| {
            context_window_tokens
                .map(|window| input_tokens as f64 / window as f64)
                .unwrap_or(0.0)
        })
        .max(0.0);

    Ok(TurnEvidence {
        turn_number: row.get::<_, Option<i64>>(0)?.unwrap_or(0).max(0) as u32,
        timestamp: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
        request_id: row.get::<_, Option<String>>(2)?,
        requested_model: row.get::<_, Option<String>>(3)?,
        served_model: row.get::<_, Option<String>>(4)?,
        status: row
            .get::<_, Option<String>>(5)?
            .unwrap_or_else(|| "unknown".to_string()),
        input_tokens,
        cached_input_tokens,
        uncached_input_tokens,
        output_tokens,
        reasoning_output_tokens: row.get::<_, Option<i64>>(10)?.unwrap_or(0).max(0) as u64,
        response_id: row.get::<_, Option<String>>(12)?,
        prompt_excerpt: row.get::<_, Option<String>>(13)?,
        failure_detail: row.get::<_, Option<String>>(14)?,
        incomplete_detail: row.get::<_, Option<String>>(15)?,
        tool_calls: parse_tool_intents(row.get::<_, Option<String>>(16)?.as_deref()),
        accounting_anomalies: parse_json_array(row.get::<_, Option<String>>(17)?.as_deref()),
        response_summary: row.get::<_, Option<String>>(18)?,
        context_utilization,
        context_window_tokens,
        duration_ms: row
            .get::<_, Option<i64>>(21)?
            .map(|value| value.max(0) as u64),
    })
}

fn load_diagnosis_snapshot(
    conn: &Connection,
    session_id: &str,
    estimated: &super::EstimatedAggregate,
    turns: &[TurnEvidence],
) -> rusqlite::Result<DiagnosisSnapshot> {
    if let Some((_, report)) = super::build_fresh_diagnosis_report(conn, session_id, estimated)? {
        let (causes, advice) = super::filter_codex_envoy_diagnosis_payload(
            serde_json::to_value(&report.causes).unwrap_or(Value::Array(vec![])),
            serde_json::to_value(&report.advice).unwrap_or(Value::Array(vec![])),
        );
        let (degraded, degradation_turn) = super::codex_envoy_public_degradation(&causes);
        return Ok(DiagnosisSnapshot {
            outcome: report.outcome,
            total_turns: report.total_turns,
            degraded,
            degradation_turn,
            causes,
            advice,
        });
    }

    let stored = conn
        .query_row(
            "SELECT outcome, total_turns, degraded, degradation_turn, causes_json, advice_json \
             FROM session_diagnoses WHERE session_id = ?1",
            rusqlite::params![session_id],
            |row| {
                let causes_raw = row.get::<_, Option<String>>(4)?.unwrap_or_default();
                let advice_raw = row.get::<_, Option<String>>(5)?.unwrap_or_default();
                let (causes, advice) = super::filter_codex_envoy_diagnosis_payload(
                    serde_json::from_str(&causes_raw).unwrap_or(Value::Array(vec![])),
                    serde_json::from_str(&advice_raw).unwrap_or(Value::Array(vec![])),
                );
                let (degraded, degradation_turn) = super::codex_envoy_public_degradation(&causes);
                Ok(DiagnosisSnapshot {
                    outcome: row.get::<_, String>(0)?,
                    total_turns: row.get::<_, Option<i64>>(1)?.unwrap_or(0).max(0) as u32,
                    degraded,
                    degradation_turn,
                    causes,
                    advice,
                })
            },
        )
        .optional()?;

    if let Some(mut stored) = stored {
        if !stored.degraded {
            let (degraded, degradation_turn) = direct_status_degradation(turns);
            stored.degraded = degraded;
            stored.degradation_turn = degradation_turn;
        }
        return Ok(stored);
    }

    let (degraded, degradation_turn) = direct_status_degradation(turns);
    Ok(DiagnosisSnapshot {
        outcome: outcome_from_turns(turns),
        total_turns: turns.len() as u32,
        degraded,
        degradation_turn,
        causes: Value::Array(vec![]),
        advice: Value::Array(vec![]),
    })
}

fn direct_status_degradation(turns: &[TurnEvidence]) -> (bool, Option<i64>) {
    let degradation_turn = turns
        .iter()
        .find(|turn| matches!(turn.status.as_str(), "failed" | "incomplete"))
        .map(|turn| turn.turn_number as i64);
    (degradation_turn.is_some(), degradation_turn)
}

fn outcome_from_turns(turns: &[TurnEvidence]) -> String {
    match turns.last().map(|turn| turn.status.as_str()) {
        Some("completed") => "Likely Completed",
        Some("failed" | "incomplete") => "Likely Partially Completed",
        _ => "Unknown",
    }
    .to_string()
}

fn impact_totals(turns: &[TurnEvidence]) -> ImpactTotals {
    let mut totals = ImpactTotals::default();
    for turn in turns {
        totals.input_tokens = totals.input_tokens.saturating_add(turn.input_tokens);
        totals.cached_input_tokens = totals
            .cached_input_tokens
            .saturating_add(turn.cached_input_tokens);
        totals.uncached_input_tokens = totals
            .uncached_input_tokens
            .saturating_add(turn.uncached_input_tokens);
        totals.output_tokens = totals.output_tokens.saturating_add(turn.output_tokens);
        totals.reasoning_output_tokens = totals
            .reasoning_output_tokens
            .saturating_add(turn.reasoning_output_tokens);
        totals.local_total_tokens = totals
            .local_total_tokens
            .saturating_add(turn.input_tokens.saturating_add(turn.output_tokens));
    }
    totals
}

fn build_summary(
    session: &SessionFacts,
    turns: &[TurnEvidence],
    diagnosis: &DiagnosisSnapshot,
    redact: bool,
    redactor: &Redactor,
) -> Value {
    let first_turn = turns.first();
    let last_turn = turns.last();
    let requested_model = last_non_empty(
        turns
            .iter()
            .filter_map(|turn| turn.requested_model.as_deref()),
    );
    let served_model = last_non_empty(turns.iter().filter_map(|turn| turn.served_model.as_deref()));
    let initial_prompt = turns
        .iter()
        .find_map(|turn| turn.prompt_excerpt.clone())
        .and_then(|prompt| clean_prompt_excerpt(&prompt));
    let final_response_summary = turns
        .iter()
        .rev()
        .find_map(|turn| turn.response_summary.as_ref())
        .filter(|summary| !summary.trim().is_empty())
        .cloned();

    json!({
        "started_at": session.started_at.clone().or_else(|| first_turn.map(|turn| turn.timestamp.clone())),
        "last_observed_at": last_turn.map(|turn| turn.timestamp.clone()),
        "ended_at": session.ended_at.clone(),
        "duration_seconds": Value::Null,
        "display_name": session.display_name.as_ref().map(|value| redactor.redact(value)),
        "requested_model": requested_model,
        "served_model": served_model,
        "outcome": diagnosis.outcome,
        "turn_count": if diagnosis.total_turns > 0 { diagnosis.total_turns } else { turns.len() as u32 },
        "response_count": turns.iter().filter(|turn| turn.response_id.is_some()).count(),
        "initial_prompt_excerpt": if redact {
            initial_prompt.map(|_| "[redacted prompt excerpt]".to_string())
        } else {
            initial_prompt.map(|prompt| redactor.redact(&prompt))
        },
        "final_response_summary": final_response_summary.map(|summary| redactor.redact(&summary)),
    })
}

fn build_signals(turns: &[TurnEvidence]) -> Value {
    let mut response_statuses = BTreeMap::from([
        ("completed".to_string(), 0u64),
        ("failed".to_string(), 0u64),
        ("incomplete".to_string(), 0u64),
        ("unknown".to_string(), 0u64),
    ]);
    let mut tool_counts: BTreeMap<String, u64> = BTreeMap::new();
    let mut model_mismatches = Vec::new();
    let mut accounting_anomaly_count = 0u64;
    let mut max_context_fill_percent = 0.0f64;
    let mut max_context_window_tokens = None::<u64>;
    let mut max_reasoning_share = 0.0f64;
    let mut total_input = 0u64;
    let mut total_cached = 0u64;

    for turn in turns {
        *response_statuses.entry(turn.status.clone()).or_insert(0) += 1;
        for tool in &turn.tool_calls {
            *tool_counts.entry(tool.name.clone()).or_insert(0) += 1;
        }
        if let (Some(requested), Some(served)) =
            (turn.requested_model.as_ref(), turn.served_model.as_ref())
        {
            if requested != served {
                model_mismatches.push(json!({
                    "turn": turn.turn_number,
                    "requested_model": requested,
                    "served_model": served,
                }));
            }
        }
        accounting_anomaly_count += turn.accounting_anomalies.len() as u64;
        max_context_fill_percent = max_context_fill_percent.max(turn.context_utilization * 100.0);
        if let Some(window) = turn.context_window_tokens {
            max_context_window_tokens = Some(max_context_window_tokens.unwrap_or(0).max(window));
        }
        if turn.output_tokens > 0 {
            max_reasoning_share = max_reasoning_share
                .max(turn.reasoning_output_tokens as f64 / turn.output_tokens as f64);
        }
        total_input = total_input.saturating_add(turn.input_tokens);
        total_cached = total_cached.saturating_add(turn.cached_input_tokens);
    }

    let cached_ratio = if total_input > 0 {
        Some(total_cached as f64 / total_input as f64)
    } else {
        None
    };
    json!({
        "response_statuses": response_statuses,
        "model_mismatches": model_mismatches,
        "accounting_anomaly_count": accounting_anomaly_count,
        "context_fill": {
            "max_percent": round_percent(max_context_fill_percent),
            "context_window_tokens": max_context_window_tokens,
            "estimated": true,
        },
        "cached_input_reuse": {
            "input_tokens": total_input,
            "cached_input_tokens": total_cached,
            "ratio": cached_ratio.map(round_ratio),
            "low_reuse_heuristic": total_input >= 1000 && cached_ratio.unwrap_or(0.0) < 0.10,
        },
        "reasoning_output_share": {
            "max_ratio": round_ratio(max_reasoning_share),
            "high_share_heuristic": max_reasoning_share >= 0.50,
        },
        "tool_call_intent_counts": tool_counts,
    })
}

fn build_evidence(turns: &[TurnEvidence], redactor: &Redactor) -> Vec<Value> {
    let mut evidence = Vec::new();
    let totals = impact_totals(turns);

    for turn in turns {
        match turn.status.as_str() {
            "failed" => evidence.push(evidence_row(
                "direct",
                "codex_response_failed",
                turn,
                redactor.redact(
                    turn.failure_detail
                        .as_deref()
                        .filter(|detail| !detail.trim().is_empty())
                        .unwrap_or(DETAIL_UNAVAILABLE),
                ),
            )),
            "incomplete" => evidence.push(evidence_row(
                "direct",
                "codex_response_incomplete",
                turn,
                redactor.redact(
                    turn.incomplete_detail
                        .as_deref()
                        .filter(|detail| !detail.trim().is_empty())
                        .unwrap_or(DETAIL_UNAVAILABLE),
                ),
            )),
            "unknown" => evidence.push(evidence_row(
                "direct",
                "codex_response_unknown",
                turn,
                "Stream ended before Codex Blackbox could identify a final status.".to_string(),
            )),
            _ => {}
        }

        if let (Some(requested), Some(served)) =
            (turn.requested_model.as_ref(), turn.served_model.as_ref())
        {
            if requested != served {
                evidence.push(evidence_row(
                    "direct",
                    "codex_model_mismatch",
                    turn,
                    format!("requested {requested}, served {served}"),
                ));
            }
        }

        if !turn.accounting_anomalies.is_empty() {
            evidence.push(evidence_row(
                "direct",
                "codex_accounting_anomaly",
                turn,
                format!(
                    "{} accounting anomaly record(s) were persisted.",
                    turn.accounting_anomalies.len()
                ),
            ));
        }

        if turn.context_utilization >= 0.80 {
            evidence.push(evidence_row(
                "heuristic",
                "codex_high_context_fill",
                turn,
                format!(
                    "estimated context fill reached {:.0}%",
                    turn.context_utilization * 100.0
                ),
            ));
        }

        if turn.output_tokens > 0 && turn.reasoning_output_tokens >= 64 {
            let share = turn.reasoning_output_tokens as f64 / turn.output_tokens as f64;
            if share >= 0.50 {
                evidence.push(evidence_row(
                    "heuristic",
                    "codex_high_reasoning_share",
                    turn,
                    format!(
                        "reasoning output was {:.0}% of output tokens",
                        share * 100.0
                    ),
                ));
            }
        }

        if !turn.tool_calls.is_empty() {
            let names = turn
                .tool_calls
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            evidence.push(evidence_row(
                "direct",
                "codex_tool_call_intent",
                turn,
                format!("tool request observed: {names}"),
            ));
        }
    }

    if turns.len() >= 3
        && totals.input_tokens >= 1_000
        && totals.cached_input_tokens as f64 / totals.input_tokens as f64 <= 0.10
    {
        if let Some(turn) = turns.first() {
            evidence.push(evidence_row(
                "heuristic",
                "codex_low_cached_input_reuse",
                turn,
                format!(
                    "cached input reuse was {:.0}% across {} turns",
                    totals.cached_input_tokens as f64 / totals.input_tokens as f64 * 100.0,
                    turns.len()
                ),
            ));
        }
    }

    evidence
}

fn evidence_row(kind: &str, signal: &str, turn: &TurnEvidence, detail: String) -> Value {
    json!({
        "type": kind,
        "signal": signal,
        "turn": turn.turn_number,
        "timestamp": turn.timestamp,
        "response_id": turn.response_id,
        "request_id": turn.request_id,
        "detail": detail,
    })
}

fn build_timeline(
    session: &SessionFacts,
    turns: &[TurnEvidence],
    degraded: bool,
    redactor: &Redactor,
) -> Vec<Value> {
    let mut timeline = Vec::new();
    if let Some(started_at) = session
        .started_at
        .as_ref()
        .or_else(|| turns.first().map(|turn| &turn.timestamp))
    {
        timeline.push(json!({
            "timestamp": started_at,
            "event": "session_start",
            "detail": "First model response observed for the session.",
        }));
    }
    for turn in turns {
        let mut detail = format!("turn {} status {}", turn.turn_number, turn.status);
        if let Some(duration) = turn.duration_ms {
            detail.push_str(&format!(" in {duration} ms"));
        }
        timeline.push(json!({
            "timestamp": turn.timestamp,
            "event": "codex_turn",
            "turn": turn.turn_number,
            "detail": detail,
        }));
        for tool in &turn.tool_calls {
            timeline.push(json!({
                "timestamp": turn.timestamp,
                "event": "tool_call_intent",
                "turn": turn.turn_number,
                "detail": tool_timeline_detail(tool, redactor),
            }));
        }
    }
    if let Some(last) = turns.last() {
        timeline.push(json!({
            "timestamp": session.ended_at.as_ref().unwrap_or(&last.timestamp),
            "event": if degraded { "session_degraded" } else { "latest_observation" },
            "detail": if degraded {
                "A degraded-session signal was observed."
            } else {
                "Latest model response observed."
            },
        }));
    }
    timeline
}

fn build_recommendations(
    diagnosis: &DiagnosisSnapshot,
    turns: &[TurnEvidence],
    redactor: &Redactor,
) -> Vec<String> {
    let mut recommendations = diagnosis
        .advice
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|advice| redactor.redact(advice))
        .collect::<Vec<_>>();

    if recommendations.is_empty() {
        if turns.iter().any(|turn| turn.status == "failed") {
            recommendations.push("Read the failure detail above before retrying.".to_string());
        } else if turns.iter().any(|turn| turn.status == "incomplete") {
            recommendations.push(
                "Continue with a smaller prompt, or raise the output limit if that was intentional."
                    .to_string(),
            );
        } else if turns.iter().any(|turn| {
            turn.requested_model.is_some()
                && turn.served_model.is_some()
                && turn.requested_model != turn.served_model
        }) {
            recommendations.push(
                "Confirm which model answered before relying on model-specific conclusions."
                    .to_string(),
            );
        } else {
            recommendations.push(
                "Continue from the latest response summary if it matches the intended task."
                    .to_string(),
            );
        }
    }

    recommendations.truncate(3);
    recommendations
}

#[derive(Clone, Debug)]
struct PrimaryCause {
    cause_type: String,
    label: String,
    classification: String,
    confidence: String,
    detail: String,
}

fn primary_cause(causes: &Value) -> Option<PrimaryCause> {
    let causes = causes.as_array()?;
    let cause = causes
        .iter()
        .find(|cause| {
            cause
                .get("is_heuristic")
                .and_then(Value::as_bool)
                .map(|is_heuristic| !is_heuristic)
                .unwrap_or(false)
        })
        .or_else(|| causes.first())?;
    let cause_type = cause.get("cause_type")?.as_str()?.to_string();
    let is_heuristic = cause
        .get("is_heuristic")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Some(PrimaryCause {
        label: cause_label(&cause_type).to_string(),
        cause_type,
        classification: if is_heuristic { "heuristic" } else { "direct" }.to_string(),
        confidence: if is_heuristic { "medium" } else { "high" }.to_string(),
        detail: cause
            .get("detail")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    })
}

fn cause_label(cause_type: &str) -> &'static str {
    match cause_type {
        "codex_response_failed" => "Model response failed",
        "codex_response_incomplete" => "Model response stopped incomplete",
        "codex_model_mismatch" => "Requested and served models differed",
        "codex_accounting_anomaly" => "Token accounting anomaly",
        "codex_high_context_fill" => "High estimated context fill",
        "codex_high_reasoning_share" => "High internal reasoning token share",
        "codex_low_cached_input_reuse" => "Low prompt cache reuse",
        _ => "Observed model-response signal",
    }
}

fn restart_prompt(
    session_id: &str,
    diagnosis: &DiagnosisSnapshot,
    turns: &[TurnEvidence],
    redactor: &Redactor,
) -> Option<String> {
    let last = turns.last()?;
    let summary = turns
        .iter()
        .rev()
        .find_map(|turn| turn.response_summary.as_ref())
        .filter(|summary| !summary.trim().is_empty());
    let issue = match last.status.as_str() {
        "failed" => last.failure_detail.as_deref().unwrap_or(DETAIL_UNAVAILABLE),
        "incomplete" => last
            .incomplete_detail
            .as_deref()
            .unwrap_or(DETAIL_UNAVAILABLE),
        _ => "",
    };
    let mut prompt = format!(
        "Continue from Codex Blackbox session {session_id}. Outcome: {}. Last observed model status: {}.",
        diagnosis.outcome, last.status
    );
    if let Some(summary) = summary {
        prompt.push_str(&format!(" Final response summary: {summary}."));
    }
    if !issue.is_empty() {
        prompt.push_str(&format!(" Observed stop detail: {issue}."));
    }
    Some(redactor.redact(&prompt))
}

fn caveats(evidence_origin: &'static str) -> Vec<String> {
    vec![
        "Only local proxy traffic was used; confirm separately before making live-support claims."
            .to_string(),
        "Tool rows mean the model asked for a tool; they do not prove the tool ran or succeeded."
            .to_string(),
        "Cached input is token accounting only; cache timing is not inferred.".to_string(),
        "Account limits and permission decisions are not visible here.".to_string(),
        if evidence_origin == "local_fake_fixture_contract" {
            "Fake fixtures validate local contracts only and are not live support evidence."
                .to_string()
        } else {
            "Evidence origin is local observation unless separately backed by real smoke or dogfood notes."
                .to_string()
        },
    ]
}

fn evidence_origin(session_id: &str, turns: &[TurnEvidence]) -> &'static str {
    let fixture_like = session_id.contains("fixture")
        || turns.iter().any(|turn| {
            turn.response_id
                .as_deref()
                .is_some_and(|value| value.contains("fixture"))
                || turn
                    .requested_model
                    .as_deref()
                    .is_some_and(|value| value.contains("fixture"))
                || turn
                    .served_model
                    .as_deref()
                    .is_some_and(|value| value.contains("fixture"))
        });
    if fixture_like {
        "local_fake_fixture_contract"
    } else {
        "unknown_local_envoy"
    }
}

fn session_is_partial(session: &SessionFacts) -> bool {
    session.ended_at.is_none() && session_is_active(&session.session_id)
}

fn session_is_active(session_id: &str) -> bool {
    diagnosis::SESSIONS
        .iter()
        .any(|entry| entry.value().session_id == session_id)
}

fn parse_tool_intents(raw: Option<&str>) -> Vec<ToolIntent> {
    parse_json_array(raw)
        .into_iter()
        .filter_map(|item| {
            let id = item
                .get("id")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .map(str::to_string);
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .map(str::to_string)
                .or_else(|| id.as_ref().map(|value| format!("custom_tool_call:{value}")))?;
            Some(ToolIntent {
                id,
                name,
                input: item
                    .get("input")
                    .and_then(Value::as_str)
                    .filter(|value| !value.trim().is_empty())
                    .map(str::to_string),
            })
        })
        .collect()
}

fn parse_json_array(raw: Option<&str>) -> Vec<Value> {
    raw.and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default()
}

fn tool_summary(tool: &ToolIntent) -> String {
    let Some(input) = tool.input.as_deref() else {
        return tool.id.clone().unwrap_or_else(|| tool.name.clone());
    };
    if let Ok(value) = serde_json::from_str::<Value>(input) {
        if let Some(command) = value.get("command").and_then(Value::as_str) {
            return truncate_ascii(command, 100);
        }
        if let Some(path) = value
            .get("file_path")
            .or_else(|| value.get("path"))
            .and_then(Value::as_str)
        {
            return truncate_ascii(path, 100);
        }
        if let Some(query) = value.get("query").and_then(Value::as_str) {
            return truncate_ascii(query, 100);
        }
    }
    truncate_ascii(input, 100)
}

fn tool_timeline_detail(tool: &ToolIntent, redactor: &Redactor) -> String {
    let name = redactor.redact(&tool.name);
    if redactor.enabled {
        return format!("{name}: [redacted tool input]");
    }
    format!("{name}: {}", tool_summary(tool))
}

fn clean_prompt_excerpt(prompt: &str) -> Option<String> {
    let mut text = prompt.trim().to_string();
    if let Some((_, rest)) = text.split_once("</INSTRUCTIONS>") {
        text = rest.trim().to_string();
    }
    if let Some(start) = text.find("<environment_context>") {
        if let Some(end) = text.find("</environment_context>") {
            let mut cleaned = String::new();
            cleaned.push_str(text[..start].trim());
            cleaned.push(' ');
            cleaned.push_str(text[end + "</environment_context>".len()..].trim());
            text = cleaned;
        }
    }
    let text = text.trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(truncate_chars(&text, 320))
    }
}

fn last_non_empty<'a>(values: impl Iterator<Item = &'a str>) -> Option<String> {
    values
        .filter(|value| !value.trim().is_empty())
        .last()
        .map(str::to_string)
}

fn round_cost(value: f64) -> f64 {
    (value.max(0.0) * 100.0).round() / 100.0
}

fn round_ratio(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

fn round_percent(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

fn truncate_ascii(value: &str, max: usize) -> String {
    if value.len() <= max {
        value.to_string()
    } else {
        let mut end = 0usize;
        for (idx, ch) in value.char_indices() {
            let next = idx + ch.len_utf8();
            if next > max {
                break;
            }
            end = next;
        }
        format!("{}...", &value[..end])
    }
}

fn truncate_chars(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    let mut out = value.chars().take(max).collect::<String>();
    out.push_str("...");
    out
}

struct Redactor {
    enabled: bool,
}

impl Redactor {
    fn new(enabled: bool) -> Self {
        Self { enabled }
    }

    fn redact(&self, value: &str) -> String {
        if !self.enabled {
            return value.to_string();
        }
        static URL_QUERY_RE: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"([A-Za-z][A-Za-z0-9+.-]*://[^\s\?]+)\?[^\s\)]*")
                .expect("URL query redaction regex")
        });
        static UNIX_PATH_RE: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"(^|[\s(:=])(/(?:Users|home|tmp|var|private|data|opt)/[A-Za-z0-9._~+/@%=-]*(?:/[A-Za-z0-9._~+/@%=-]*)*)")
                .expect("Unix path redaction regex")
        });
        static WINDOWS_PATH_RE: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"(?i)\b[A-Z]:\\(?:[^\s\\/:*?<>|]+\\)*[^\s\\/:*?<>|]+")
                .expect("Windows path redaction regex")
        });
        static SECRET_RE: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(
                r"(?i)\b(api[_-]?key|token|secret|password|authorization|bearer)\b\s*[:=]\s*[A-Za-z0-9._~+/=-]{8,}",
            )
            .expect("secret redaction regex")
        });
        static OPAQUE_RE: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"\b[A-Za-z0-9_-]{32,}\b").expect("opaque token redaction regex")
        });

        let value = URL_QUERY_RE.replace_all(value, "$1?[redacted]");
        let value = UNIX_PATH_RE.replace_all(&value, "$1[path]");
        let value = WINDOWS_PATH_RE.replace_all(&value, "[path]");
        let value = SECRET_RE.replace_all(&value, "$1=[redacted]");
        OPAQUE_RE.replace_all(&value, "[opaque]").into_owned()
    }

    fn redact_value(&self, value: Value) -> Value {
        if !self.enabled {
            return value;
        }
        match value {
            Value::String(value) => Value::String(self.redact(&value)),
            Value::Array(items) => Value::Array(
                items
                    .into_iter()
                    .map(|item| self.redact_value(item))
                    .collect(),
            ),
            Value::Object(map) => Value::Object(
                map.into_iter()
                    .map(|(key, value)| (key, self.redact_value(value)))
                    .collect(),
            ),
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{tool_summary, Redactor, ToolIntent};

    #[test]
    fn redactor_masks_paths_queries_and_secrets() {
        let redactor = Redactor::new(true);
        let redacted = redactor.redact(
            "open /Users/alice/src/app and C:\\Users\\alice\\repo then fetch https://example.test/a?token=abc token=sk-abcdefghijklmnopqrstuvwxyz123456",
        );

        assert!(redacted.contains("[path]"));
        assert!(redacted.contains("https://example.test/a?[redacted]"));
        assert!(redacted.contains("token=[redacted]"));
        assert!(!redacted.contains("/Users/alice"));
        assert!(!redacted.contains("C:\\Users"));
        assert!(!redacted.contains("abcdefghijklmnopqrstuvwxyz123456"));
    }

    #[test]
    fn tool_summary_truncates_non_ascii_without_byte_boundary_panic() {
        let tool = ToolIntent {
            id: None,
            name: "shell".to_string(),
            input: Some("€".repeat(40)),
        };

        let summary = tool_summary(&tool);

        assert!(summary.ends_with("..."));
        assert!(summary.len() <= 103);
    }
}
