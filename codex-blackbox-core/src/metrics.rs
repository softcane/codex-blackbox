use std::collections::BTreeMap;
use std::sync::LazyLock;

use prometheus::{
    gather, histogram_opts, opts, register_histogram_vec, register_int_counter_vec, Encoder,
    HistogramVec, IntCounterVec, TextEncoder,
};

pub const HISTORY_WINDOWS: [(&str, u64); 3] = [("1d", 1), ("7d", 7), ("30d", 30)];
pub const HISTORY_MODELS: [&str; 8] = [
    "gpt-5.5",
    "gpt-5.4",
    "gpt-5.3",
    "gpt-5",
    "gpt-codex",
    "o4-mini",
    "o3",
    "other",
];
const LIVE_MODELS: [&str; 8] = HISTORY_MODELS;
const LIVE_PROVIDERS: [&str; 2] = ["codex_responses", "unknown"];
const LIVE_TOKEN_KINDS: [&str; 6] = [
    "input",
    "cached_input",
    "uncached_input",
    "output",
    "reasoning_output",
    "total",
];
const LIVE_CONTEXT_PROVIDERS: [&str; 2] = ["codex_responses", "unknown"];
const LIVE_CODEX_RESPONSE_STATUSES: [&str; 4] = ["completed", "failed", "incomplete", "unknown"];
const HOOK_EVENTS: [&str; 7] = [
    "pre_tool_use",
    "post_tool_use",
    "user_prompt_submit",
    "stop",
    "pre_compact",
    "post_compact",
    "other",
];
const HOOK_TOOL_CATEGORIES: [&str; 4] = ["bash", "apply_patch", "mcp", "other"];
const HOOK_RESULTS: [&str; 4] = ["success", "failure", "blocked", "unknown"];
const VALIDATION_CATEGORIES: [&str; 5] = ["test", "lint", "typecheck", "build", "unknown"];
const VALIDATION_RESULTS: [&str; 3] = ["success", "failure", "unknown"];
const LOOP_SIGNALS: [&str; 13] = [
    "repeated_validation_failure",
    "repeated_tool_failure",
    "blind_retry",
    "unvalidated_edit",
    "high_context",
    "incomplete_response",
    "failed_response",
    "unknown_response",
    "risky_supported_tool_call",
    "untrusted_pricing",
    "rate_limit_pressure",
    "missing_durable_evidence",
    "other",
];
const SIGNAL_SEVERITIES: [&str; 5] = ["watching", "careful", "stop", "blocked", "cooldown"];
const EVIDENCE_SOURCES: [&str; 5] = ["proxy", "hook", "transcript", "user_policy", "app_server"];
const COACH_ACTIONS: [&str; 3] = ["warn", "block", "continue"];
const REASON_CODES: [&str; 12] = [
    "failed_response",
    "incomplete_response",
    "unknown_response",
    "high_context_careful",
    "high_context_stop",
    "repeated_validation_failure",
    "unvalidated_edit",
    "blind_retry",
    "risky_supported_tool_call",
    "untrusted_pricing",
    "missing_durable_evidence",
    "other",
];
const BASELINE_SCOPES: [&str; 2] = ["project", "user"];
const BASELINE_RESULTS: [&str; 4] = ["preview", "learned", "reset", "disabled"];
pub const HISTORY_CAUSE_TYPES: [&str; 7] = [
    "codex_response_failed",
    "codex_response_incomplete",
    "codex_model_mismatch",
    "codex_high_context_fill",
    "codex_high_reasoning_share",
    "codex_accounting_anomaly",
    "codex_low_cached_input_reuse",
];

#[derive(Clone, Debug, Default)]
pub struct HistoricalWindowMetrics {
    pub window: &'static str,
    pub sessions: u64,
    pub degraded_sessions: u64,
    pub degraded_session_ratio: f64,
    pub degraded_causes: BTreeMap<&'static str, u64>,
    pub model_fallbacks: BTreeMap<(&'static str, &'static str), u64>,
}

pub struct CodexBlackboxMetrics {
    requests_total: IntCounterVec,
    tokens_total: IntCounterVec,
    sessions_degraded_total: IntCounterVec,
    codex_response_status_total: IntCounterVec,
    model_fallback_total: IntCounterVec,
    tool_calls_total: IntCounterVec,
    hook_events_total: IntCounterVec,
    validation_runs_total: IntCounterVec,
    loop_signals_total: IntCounterVec,
    coach_actions_total: IntCounterVec,
    guard_blocks_total: IntCounterVec,
    baseline_builds_total: IntCounterVec,
    unvalidated_edit_signals_total: IntCounterVec,
    turn_duration_seconds: HistogramVec,
    context_fill_percent: HistogramVec,
}

impl CodexBlackboxMetrics {
    fn register() -> Self {
        let mut turn_duration_buckets = prometheus::DEFAULT_BUCKETS.to_vec();
        turn_duration_buckets.extend([30.0, 60.0, 120.0]);
        turn_duration_buckets.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        turn_duration_buckets.dedup();

        Self {
            requests_total: register_int_counter_vec!(
                opts!(
                    "codex_blackbox_requests_total",
                    "Completed turns observed by codex-blackbox-core."
                ),
                &["provider", "model"]
            )
            .expect("register codex_blackbox_requests_total"),
            tokens_total: register_int_counter_vec!(
                opts!(
                    "codex_blackbox_tokens_total",
                    "Tokens observed by turn and token kind."
                ),
                &["provider", "model", "kind"]
            )
            .expect("register codex_blackbox_tokens_total"),
            sessions_degraded_total: register_int_counter_vec!(
                opts!(
                    "codex_blackbox_sessions_degraded_total",
                    "Envoy-derived Codex degraded sessions by bounded cause type."
                ),
                &["cause_type"]
            )
            .expect("register codex_blackbox_sessions_degraded_total"),
            codex_response_status_total: register_int_counter_vec!(
                opts!(
                    "codex_blackbox_codex_response_status_total",
                    "Envoy-observed Codex Responses terminal statuses by normalized served model."
                ),
                &["status", "model"]
            )
            .expect("register codex_blackbox_codex_response_status_total"),
            model_fallback_total: register_int_counter_vec!(
                opts!(
                    "codex_blackbox_model_fallback_total",
                    "Envoy-observed requested model versus served model mismatches."
                ),
                &["requested", "actual"]
            )
            .expect("register codex_blackbox_model_fallback_total"),
            tool_calls_total: register_int_counter_vec!(
                opts!(
                    "codex_blackbox_tool_calls_total",
                    "Custom tool-call intent observed in Envoy-proxied assistant responses."
                ),
                &["tool"]
            )
            .expect("register codex_blackbox_tool_calls_total"),
            hook_events_total: register_int_counter_vec!(
                opts!(
                    "codex_blackbox_hook_events_total",
                    "Supported Codex hook events observed by bounded event, tool category, and result."
                ),
                &["event", "tool_category", "result"]
            )
            .expect("register codex_blackbox_hook_events_total"),
            validation_runs_total: register_int_counter_vec!(
                opts!(
                    "codex_blackbox_validation_runs_total",
                    "Supported validation runs observed by bounded category and result."
                ),
                &["category", "result"]
            )
            .expect("register codex_blackbox_validation_runs_total"),
            loop_signals_total: register_int_counter_vec!(
                opts!(
                    "codex_blackbox_loop_signals_total",
                    "Coach loop signals by bounded signal, severity, and evidence source."
                ),
                &["signal", "severity", "evidence_source"]
            )
            .expect("register codex_blackbox_loop_signals_total"),
            coach_actions_total: register_int_counter_vec!(
                opts!(
                    "codex_blackbox_coach_actions_total",
                    "Coach actions by bounded action and reason code."
                ),
                &["action", "reason_code"]
            )
            .expect("register codex_blackbox_coach_actions_total"),
            guard_blocks_total: register_int_counter_vec!(
                opts!(
                    "codex_blackbox_guard_blocks_total",
                    "Guard blocks by bounded reason code."
                ),
                &["reason_code"]
            )
            .expect("register codex_blackbox_guard_blocks_total"),
            baseline_builds_total: register_int_counter_vec!(
                opts!(
                    "codex_blackbox_baseline_builds_total",
                    "Baseline preview, learn, reset, and disable actions by bounded scope and result."
                ),
                &["scope", "result"]
            )
            .expect("register codex_blackbox_baseline_builds_total"),
            unvalidated_edit_signals_total: register_int_counter_vec!(
                opts!(
                    "codex_blackbox_unvalidated_edit_signals_total",
                    "Unvalidated edit coach signals by severity."
                ),
                &["severity"]
            )
            .expect("register codex_blackbox_unvalidated_edit_signals_total"),
            turn_duration_seconds: register_histogram_vec!(
                histogram_opts!(
                    "codex_blackbox_turn_duration_seconds",
                    "Envoy-observed Codex turn durations in seconds.",
                    turn_duration_buckets
                ),
                &["model"]
            )
            .expect("register codex_blackbox_turn_duration_seconds"),
            context_fill_percent: register_histogram_vec!(
                histogram_opts!(
                    "codex_blackbox_context_fill_percent",
                    "Observed context-window fill percentage by provider and normalized model.",
                    vec![10.0, 25.0, 50.0, 70.0, 80.0, 90.0, 95.0, 100.0]
                ),
                &["provider", "model"]
            )
            .expect("register codex_blackbox_context_fill_percent"),
        }
    }
}

static METRICS: LazyLock<CodexBlackboxMetrics> = LazyLock::new(CodexBlackboxMetrics::register);

pub fn init() {
    let metrics = LazyLock::force(&METRICS);
    for provider in LIVE_PROVIDERS {
        for model in LIVE_MODELS {
            metrics.requests_total.with_label_values(&[provider, model]);
            for kind in LIVE_TOKEN_KINDS {
                metrics
                    .tokens_total
                    .with_label_values(&[provider, model, kind]);
            }
        }
    }
    for model in LIVE_MODELS {
        metrics.turn_duration_seconds.with_label_values(&[model]);
        for provider in LIVE_CONTEXT_PROVIDERS {
            metrics
                .context_fill_percent
                .with_label_values(&[provider, model]);
        }
        for actual in LIVE_MODELS {
            metrics
                .model_fallback_total
                .with_label_values(&[model, actual]);
        }
    }

    ensure_tool_metric_labels("unknown");
    for event in HOOK_EVENTS {
        for tool_category in HOOK_TOOL_CATEGORIES {
            for result in HOOK_RESULTS {
                metrics
                    .hook_events_total
                    .with_label_values(&[event, tool_category, result]);
            }
        }
    }
    for category in VALIDATION_CATEGORIES {
        for result in VALIDATION_RESULTS {
            metrics
                .validation_runs_total
                .with_label_values(&[category, result]);
        }
    }
    for signal in LOOP_SIGNALS {
        for severity in SIGNAL_SEVERITIES {
            for source in EVIDENCE_SOURCES {
                metrics
                    .loop_signals_total
                    .with_label_values(&[signal, severity, source]);
            }
        }
    }
    for action in COACH_ACTIONS {
        for reason_code in REASON_CODES {
            metrics
                .coach_actions_total
                .with_label_values(&[action, reason_code]);
        }
    }
    for reason_code in REASON_CODES {
        metrics.guard_blocks_total.with_label_values(&[reason_code]);
    }
    for scope in BASELINE_SCOPES {
        for result in BASELINE_RESULTS {
            metrics
                .baseline_builds_total
                .with_label_values(&[scope, result]);
        }
    }
    for severity in SIGNAL_SEVERITIES {
        metrics
            .unvalidated_edit_signals_total
            .with_label_values(&[severity]);
    }
    for cause_type in HISTORY_CAUSE_TYPES {
        metrics
            .sessions_degraded_total
            .with_label_values(&[cause_type]);
    }
    metrics
        .sessions_degraded_total
        .with_label_values(&["unknown"]);
    for status in LIVE_CODEX_RESPONSE_STATUSES {
        for model in LIVE_MODELS {
            metrics
                .codex_response_status_total
                .with_label_values(&[status, model]);
        }
    }
}

pub struct CodexTurnMetric<'a> {
    pub model: &'a str,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub uncached_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
    pub total_tokens: u64,
    pub estimated_cost_dollars: f64,
    pub duration_seconds: f64,
}

pub fn record_codex_turn(turn: CodexTurnMetric<'_>) {
    let model = normalize_model(turn.model);
    let provider = "codex_responses";
    METRICS
        .requests_total
        .with_label_values(&[provider, model])
        .inc();
    METRICS
        .tokens_total
        .with_label_values(&[provider, model, "input"])
        .inc_by(turn.input_tokens);
    METRICS
        .tokens_total
        .with_label_values(&[provider, model, "cached_input"])
        .inc_by(turn.cached_input_tokens);
    METRICS
        .tokens_total
        .with_label_values(&[provider, model, "uncached_input"])
        .inc_by(turn.uncached_input_tokens);
    METRICS
        .tokens_total
        .with_label_values(&[provider, model, "output"])
        .inc_by(turn.output_tokens);
    METRICS
        .tokens_total
        .with_label_values(&[provider, model, "reasoning_output"])
        .inc_by(turn.reasoning_output_tokens);
    METRICS
        .tokens_total
        .with_label_values(&[provider, model, "total"])
        .inc_by(turn.total_tokens);
    let _ = turn.estimated_cost_dollars;
    METRICS
        .turn_duration_seconds
        .with_label_values(&[model])
        .observe(turn.duration_seconds.max(0.0));
}

pub fn record_codex_response_status(status: &str, model: &str) {
    METRICS
        .codex_response_status_total
        .with_label_values(&[
            normalize_codex_response_status(status),
            normalize_model(model),
        ])
        .inc();
}

pub fn record_degraded_cause(cause_type: &str) {
    METRICS
        .sessions_degraded_total
        .with_label_values(&[normalize_cause(cause_type)])
        .inc();
}

pub fn record_model_fallback(requested: &str, actual: &str) {
    METRICS
        .model_fallback_total
        .with_label_values(&[normalize_model(requested), normalize_model(actual)])
        .inc();
}

pub fn record_context_fill_percent(provider: &str, model: &str, fill_percent: f64) {
    METRICS
        .context_fill_percent
        .with_label_values(&[normalize_context_provider(provider), normalize_model(model)])
        .observe(fill_percent.clamp(0.0, 100.0));
}

pub fn record_tool_call(tool_name: &str) {
    let tool = normalize_tool(tool_name);
    METRICS
        .tool_calls_total
        .with_label_values(&[tool.as_str()])
        .inc();
}

pub fn ensure_tool_metric_labels(tool_name: &str) {
    let tool = normalize_tool(tool_name);
    METRICS.tool_calls_total.with_label_values(&[tool.as_str()]);
}

pub fn record_hook_event(event: &str, tool_category: &str, result: &str) {
    METRICS
        .hook_events_total
        .with_label_values(&[
            normalize_hook_event(event),
            normalize_hook_tool_category(tool_category),
            normalize_hook_result(result),
        ])
        .inc();
}

pub fn record_validation_run(category: &str, result: &str) {
    METRICS
        .validation_runs_total
        .with_label_values(&[
            normalize_validation_category(category),
            normalize_validation_result(result),
        ])
        .inc();
}

pub fn record_loop_signal(signal: &str, severity: &str, evidence_source: &str) {
    let signal = normalize_loop_signal(signal);
    let severity = normalize_signal_severity(severity);
    METRICS
        .loop_signals_total
        .with_label_values(&[signal, severity, normalize_evidence_source(evidence_source)])
        .inc();
    if signal == "unvalidated_edit" {
        METRICS
            .unvalidated_edit_signals_total
            .with_label_values(&[severity])
            .inc();
    }
}

pub fn record_coach_action(action: &str, reason_code: &str) {
    METRICS
        .coach_actions_total
        .with_label_values(&[
            normalize_coach_action(action),
            normalize_reason_code(reason_code),
        ])
        .inc();
}

pub fn record_guard_block(reason_code: &str) {
    METRICS
        .guard_blocks_total
        .with_label_values(&[normalize_reason_code(reason_code)])
        .inc();
}

pub fn record_baseline_build(scope: &str, result: &str) {
    METRICS
        .baseline_builds_total
        .with_label_values(&[
            normalize_baseline_scope(scope),
            normalize_baseline_result(result),
        ])
        .inc();
}

pub fn update_historical_gauges(windows: &[HistoricalWindowMetrics], refreshed_at_epoch: u64) {
    let _ = (windows, refreshed_at_epoch);
}

pub fn render() -> Result<(String, String), String> {
    let metric_families = gather();
    let encoder = TextEncoder::new();
    let mut buf = Vec::new();
    encoder
        .encode(&metric_families, &mut buf)
        .map_err(|e| e.to_string())?;
    let body = String::from_utf8(buf).map_err(|e| e.to_string())?;
    Ok((encoder.format_type().to_string(), body))
}

fn normalize_model(model: &str) -> &'static str {
    let lower = model.to_ascii_lowercase();
    if lower.starts_with("gpt-5.5") {
        "gpt-5.5"
    } else if lower.starts_with("gpt-5.4") {
        "gpt-5.4"
    } else if lower.starts_with("gpt-5.3") {
        "gpt-5.3"
    } else if lower.starts_with("gpt-5") {
        "gpt-5"
    } else if lower.starts_with("gpt-codex") || lower.contains("codex") {
        "gpt-codex"
    } else if lower.starts_with("o4-mini") {
        "o4-mini"
    } else if lower.starts_with("o3") {
        "o3"
    } else {
        "other"
    }
}

fn normalize_context_provider(provider: &str) -> &'static str {
    match provider {
        "codex_responses" => "codex_responses",
        _ => "unknown",
    }
}

fn normalize_codex_response_status(status: &str) -> &'static str {
    match status {
        "completed" => "completed",
        "failed" => "failed",
        "incomplete" => "incomplete",
        _ => "unknown",
    }
}

fn normalize_cause(cause_type: &str) -> &str {
    if cause_type.is_empty() {
        "unknown"
    } else if HISTORY_CAUSE_TYPES.contains(&cause_type) {
        cause_type
    } else {
        "unknown"
    }
}

pub fn historical_cause_label(cause_type: &str) -> Option<&'static str> {
    HISTORY_CAUSE_TYPES
        .iter()
        .copied()
        .find(|c| *c == cause_type)
}

pub fn historical_model_label(model: &str) -> &'static str {
    normalize_model(model)
}

fn normalize_tool(tool_name: &str) -> String {
    let trimmed = tool_name.trim();
    if trimmed.is_empty() {
        return "unknown".to_string();
    }
    if trimmed.starts_with("custom_tool_call:") {
        return "custom_tool_call".to_string();
    }

    let mut normalized = String::with_capacity(trimmed.len());
    for ch in trimmed.chars() {
        if ch.is_ascii_alphanumeric() {
            normalized.push(ch.to_ascii_lowercase());
        } else if matches!(ch, '_' | '-' | '.') {
            normalized.push(ch);
        } else {
            normalized.push('_');
        }
    }

    match normalized.as_str() {
        "" => "unknown".to_string(),
        "bash" | "read" | "read_file" | "grep" | "glob" | "edit" | "write" => normalized,
        "custom_tool_call" => normalized,
        _ => "named_tool".to_string(),
    }
}

fn normalize_hook_event(event: &str) -> &'static str {
    match event {
        "PreToolUse" | "pre_tool_use" => "pre_tool_use",
        "PostToolUse" | "post_tool_use" => "post_tool_use",
        "UserPromptSubmit" | "user_prompt_submit" => "user_prompt_submit",
        "Stop" | "stop" => "stop",
        "PreCompact" | "pre_compact" => "pre_compact",
        "PostCompact" | "post_compact" => "post_compact",
        _ => "other",
    }
}

fn normalize_hook_tool_category(category: &str) -> &'static str {
    match category {
        "bash" => "bash",
        "apply_patch" => "apply_patch",
        "mcp" => "mcp",
        _ => "other",
    }
}

fn normalize_hook_result(result: &str) -> &'static str {
    match result {
        "success" => "success",
        "failure" => "failure",
        "blocked" => "blocked",
        _ => "unknown",
    }
}

fn normalize_validation_category(category: &str) -> &'static str {
    match category {
        "test" => "test",
        "lint" => "lint",
        "typecheck" => "typecheck",
        "build" => "build",
        _ => "unknown",
    }
}

fn normalize_validation_result(result: &str) -> &'static str {
    match result {
        "success" => "success",
        "failure" => "failure",
        _ => "unknown",
    }
}

fn normalize_loop_signal(signal: &str) -> &'static str {
    match signal {
        "repeated_validation_failure" => "repeated_validation_failure",
        "repeated_tool_failure" => "repeated_tool_failure",
        "blind_retry" => "blind_retry",
        "unvalidated_edit" => "unvalidated_edit",
        "high_context" => "high_context",
        "incomplete_response" => "incomplete_response",
        "failed_response" => "failed_response",
        "unknown_response" => "unknown_response",
        "risky_supported_tool_call" => "risky_supported_tool_call",
        "untrusted_pricing" => "untrusted_pricing",
        "rate_limit_pressure" => "rate_limit_pressure",
        "missing_durable_evidence" => "missing_durable_evidence",
        _ => "other",
    }
}

fn normalize_signal_severity(severity: &str) -> &'static str {
    match severity {
        "watching" => "watching",
        "careful" => "careful",
        "stop" => "stop",
        "blocked" => "blocked",
        "cooldown" => "cooldown",
        _ => "watching",
    }
}

fn normalize_evidence_source(source: &str) -> &'static str {
    match source {
        "proxy" => "proxy",
        "hook" => "hook",
        "transcript" => "transcript",
        "user_policy" => "user_policy",
        "app_server" => "app_server",
        _ => "hook",
    }
}

fn normalize_coach_action(action: &str) -> &'static str {
    match action {
        "warn" => "warn",
        "block" => "block",
        "continue" => "continue",
        _ => "warn",
    }
}

fn normalize_reason_code(reason_code: &str) -> &'static str {
    match reason_code {
        "failed_response" => "failed_response",
        "incomplete_response" => "incomplete_response",
        "unknown_response" => "unknown_response",
        "high_context_careful" => "high_context_careful",
        "high_context_stop" => "high_context_stop",
        "repeated_validation_failure" => "repeated_validation_failure",
        "unvalidated_edit" => "unvalidated_edit",
        "blind_retry" => "blind_retry",
        "risky_supported_tool_call" => "risky_supported_tool_call",
        "untrusted_pricing" => "untrusted_pricing",
        "missing_durable_evidence" => "missing_durable_evidence",
        _ => "other",
    }
}

fn normalize_baseline_scope(scope: &str) -> &'static str {
    match scope {
        "user" => "user",
        _ => "project",
    }
}

fn normalize_baseline_result(result: &str) -> &'static str {
    match result {
        "preview" => "preview",
        "learned" => "learned",
        "reset" => "reset",
        "disabled" => "disabled",
        _ => "preview",
    }
}

#[cfg(test)]
mod tests {
    use super::{
        init, record_baseline_build, record_coach_action, record_codex_response_status,
        record_codex_turn, record_context_fill_percent, record_degraded_cause, record_guard_block,
        record_hook_event, record_loop_signal, record_validation_run, render, CodexTurnMetric,
    };

    #[test]
    fn codex_observability_metrics_are_bounded_and_initialized() {
        init();
        record_context_fill_percent("codex_responses", "gpt-codex-fixture", 42.0);
        record_degraded_cause("session-id-like-cause-phase-8b");
        record_codex_turn(CodexTurnMetric {
            model: "gpt-5.5",
            input_tokens: 1_280,
            cached_input_tokens: 512,
            uncached_input_tokens: 768,
            output_tokens: 96,
            reasoning_output_tokens: 32,
            total_tokens: 1_376,
            estimated_cost_dollars: 0.006976,
            duration_seconds: 1.5,
        });
        record_codex_response_status("failed", "gpt-5.5");
        record_hook_event("PreToolUse", "bash", "blocked");
        record_hook_event("PreCompact", "other", "success");
        record_validation_run("test", "failure");
        record_loop_signal(
            "session-id-like-cause-phase-8b",
            "stop",
            "session-id-like-source",
        );
        record_loop_signal("unvalidated_edit", "careful", "hook");
        record_loop_signal("repeated_tool_failure", "careful", "hook");
        record_coach_action("block", "risky_supported_tool_call");
        record_guard_block("session-id-like-cause-phase-8b");
        record_baseline_build("project", "learned");

        let (_, body) = render().expect("render metrics");
        assert!(body.contains(
            "codex_blackbox_context_fill_percent_count{model=\"gpt-codex\",provider=\"codex_responses\"}"
        ));
        assert!(body.contains(
            "codex_blackbox_sessions_degraded_total{cause_type=\"codex_response_failed\"}"
        ));
        assert!(body.contains(
            "codex_blackbox_codex_response_status_total{model=\"gpt-5.5\",status=\"failed\"} 1"
        ));
        assert!(body.contains("codex_blackbox_sessions_degraded_total{cause_type=\"unknown\"}"));
        assert!(body.contains(
            "codex_blackbox_hook_events_total{event=\"pre_tool_use\",result=\"blocked\",tool_category=\"bash\"} 1"
        ));
        assert!(body.contains(
            "codex_blackbox_hook_events_total{event=\"pre_compact\",result=\"success\",tool_category=\"other\"} 1"
        ));
        assert!(body.contains(
            "codex_blackbox_validation_runs_total{category=\"test\",result=\"failure\"} 1"
        ));
        assert!(body.contains(
            "codex_blackbox_loop_signals_total{evidence_source=\"hook\",severity=\"careful\",signal=\"unvalidated_edit\"} 1"
        ));
        assert!(body.contains(
            "codex_blackbox_loop_signals_total{evidence_source=\"hook\",severity=\"careful\",signal=\"repeated_tool_failure\"} 1"
        ));
        assert!(body.contains(
            "codex_blackbox_coach_actions_total{action=\"block\",reason_code=\"risky_supported_tool_call\"} 1"
        ));
        assert!(body.contains("codex_blackbox_guard_blocks_total{reason_code=\"other\"} 1"));
        assert!(body.contains(
            "codex_blackbox_baseline_builds_total{result=\"learned\",scope=\"project\"} 1"
        ));
        assert!(!body.contains("session-id-like-cause-phase-8b"));
        assert!(!body.contains("session_id="));
        assert!(!body.contains("codex_blackbox_session_id="));
        assert!(!body.contains("kind=\"cache_read\""));
        assert!(!body.contains("kind=\"cache_create\""));
        for (prefix, suffix) in [
            ("codex_blackbox_", "estimated_cost_dollars_total"),
            ("codex_blackbox_", "tool_failures_total"),
            ("codex_blackbox_", "mcp_"),
            ("codex_blackbox_", "skill_events_total"),
            ("codex_blackbox_", "active_sessions"),
            ("codex_blackbox_", "weekly_tokens"),
            ("codex_blackbox_", "history_"),
        ] {
            let dropped_metric = format!("{prefix}{suffix}");
            assert!(
                !body.contains(&dropped_metric),
                "non-Envoy Codex metric family remained exposed: {dropped_metric}"
            );
        }
    }
}
