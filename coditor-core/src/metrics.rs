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
const LIVE_TOKEN_KINDS: [&str; 8] = [
    "input",
    "cached_input",
    "uncached_input",
    "output",
    "reasoning_output",
    "total",
    "cache_read",
    "cache_create",
];
const LIVE_CONTEXT_PROVIDERS: [&str; 2] = ["codex_responses", "unknown"];
const LIVE_CODEX_RESPONSE_STATUSES: [&str; 4] = ["completed", "failed", "incomplete", "unknown"];
pub const HISTORY_CACHE_EVENT_TYPES: [&str; 2] = ["miss_ttl", "miss_thrash"];
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
    pub estimated_spend_dollars: f64,
    pub estimated_spend_dollars_by_model: BTreeMap<&'static str, f64>,
    pub avg_estimated_session_cost_dollars_by_model: BTreeMap<&'static str, f64>,
    pub estimated_cache_waste_dollars_by_model: BTreeMap<&'static str, f64>,
    pub cache_hit_ratio: f64,
    pub cache_events: BTreeMap<&'static str, u64>,
    pub degraded_sessions: u64,
    pub degraded_session_ratio: f64,
    pub degraded_causes: BTreeMap<&'static str, u64>,
    pub model_fallbacks: BTreeMap<(&'static str, &'static str), u64>,
    pub tool_failures_by_tool: BTreeMap<String, u64>,
}

pub struct CoditorMetrics {
    requests_total: IntCounterVec,
    tokens_total: IntCounterVec,
    sessions_degraded_total: IntCounterVec,
    codex_response_status_total: IntCounterVec,
    model_fallback_total: IntCounterVec,
    tool_calls_total: IntCounterVec,
    turn_duration_seconds: HistogramVec,
    context_fill_percent: HistogramVec,
}

impl CoditorMetrics {
    fn register() -> Self {
        let mut turn_duration_buckets = prometheus::DEFAULT_BUCKETS.to_vec();
        turn_duration_buckets.extend([30.0, 60.0, 120.0]);
        turn_duration_buckets.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        turn_duration_buckets.dedup();

        Self {
            requests_total: register_int_counter_vec!(
                opts!(
                    "coditor_requests_total",
                    "Completed turns observed by coditor-core."
                ),
                &["provider", "model"]
            )
            .expect("register coditor_requests_total"),
            tokens_total: register_int_counter_vec!(
                opts!(
                    "coditor_tokens_total",
                    "Tokens observed by turn and token kind."
                ),
                &["provider", "model", "kind"]
            )
            .expect("register coditor_tokens_total"),
            sessions_degraded_total: register_int_counter_vec!(
                opts!(
                    "coditor_sessions_degraded_total",
                    "Envoy-derived Codex degraded sessions by bounded cause type."
                ),
                &["cause_type"]
            )
            .expect("register coditor_sessions_degraded_total"),
            codex_response_status_total: register_int_counter_vec!(
                opts!(
                    "coditor_codex_response_status_total",
                    "Envoy-observed Codex Responses terminal statuses by normalized served model."
                ),
                &["status", "model"]
            )
            .expect("register coditor_codex_response_status_total"),
            model_fallback_total: register_int_counter_vec!(
                opts!(
                    "coditor_model_fallback_total",
                    "Envoy-observed requested model versus served model mismatches."
                ),
                &["requested", "actual"]
            )
            .expect("register coditor_model_fallback_total"),
            tool_calls_total: register_int_counter_vec!(
                opts!(
                    "coditor_tool_calls_total",
                    "Custom tool-call intent observed in Envoy-proxied assistant responses."
                ),
                &["tool"]
            )
            .expect("register coditor_tool_calls_total"),
            turn_duration_seconds: register_histogram_vec!(
                histogram_opts!(
                    "coditor_turn_duration_seconds",
                    "Envoy-observed Codex turn durations in seconds.",
                    turn_duration_buckets
                ),
                &["model"]
            )
            .expect("register coditor_turn_duration_seconds"),
            context_fill_percent: register_histogram_vec!(
                histogram_opts!(
                    "coditor_context_fill_percent",
                    "Observed context-window fill percentage by provider and normalized model.",
                    vec![10.0, 25.0, 50.0, 70.0, 80.0, 90.0, 95.0, 100.0]
                ),
                &["provider", "model"]
            )
            .expect("register coditor_context_fill_percent"),
        }
    }
}

static METRICS: LazyLock<CoditorMetrics> = LazyLock::new(CoditorMetrics::register);

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

pub fn record_codex_turn(
    model: &str,
    input_tokens: u64,
    cached_input_tokens: u64,
    uncached_input_tokens: u64,
    output_tokens: u64,
    reasoning_output_tokens: u64,
    total_tokens: u64,
    estimated_cost_dollars: f64,
    duration_seconds: f64,
) {
    let model = normalize_model(model);
    let provider = "codex_responses";
    METRICS
        .requests_total
        .with_label_values(&[provider, model])
        .inc();
    METRICS
        .tokens_total
        .with_label_values(&[provider, model, "input"])
        .inc_by(input_tokens);
    METRICS
        .tokens_total
        .with_label_values(&[provider, model, "cached_input"])
        .inc_by(cached_input_tokens);
    METRICS
        .tokens_total
        .with_label_values(&[provider, model, "uncached_input"])
        .inc_by(uncached_input_tokens);
    METRICS
        .tokens_total
        .with_label_values(&[provider, model, "output"])
        .inc_by(output_tokens);
    METRICS
        .tokens_total
        .with_label_values(&[provider, model, "reasoning_output"])
        .inc_by(reasoning_output_tokens);
    METRICS
        .tokens_total
        .with_label_values(&[provider, model, "total"])
        .inc_by(total_tokens);
    let _ = estimated_cost_dollars;
    METRICS
        .turn_duration_seconds
        .with_label_values(&[model])
        .observe(duration_seconds.max(0.0));
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

pub fn historical_cache_event_label(event_type: &str) -> Option<&'static str> {
    HISTORY_CACHE_EVENT_TYPES
        .iter()
        .copied()
        .find(|event| *event == event_type)
}

pub fn historical_tool_label(tool_name: &str) -> String {
    normalize_tool(tool_name)
}

fn normalize_tool(tool_name: &str) -> String {
    let trimmed = tool_name.trim();
    if trimmed.is_empty() {
        return "unknown".to_string();
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

    if normalized.is_empty() {
        "unknown".to_string()
    } else {
        normalized
    }
}

#[cfg(test)]
mod tests {
    use super::{
        init, record_codex_response_status, record_codex_turn, record_context_fill_percent,
        record_degraded_cause, render,
    };

    #[test]
    fn codex_observability_metrics_are_bounded_and_initialized() {
        init();
        record_context_fill_percent("codex_responses", "gpt-codex-fixture", 42.0);
        record_degraded_cause("session-id-like-cause-phase-8b");
        record_codex_turn("gpt-5.5", 1_280, 512, 768, 96, 32, 1_376, 0.006976, 1.5);
        record_codex_response_status("failed", "gpt-5.5");

        let (_, body) = render().expect("render metrics");
        assert!(body.contains(
            "coditor_context_fill_percent_count{model=\"gpt-codex\",provider=\"codex_responses\"}"
        ));
        assert!(
            body.contains("coditor_sessions_degraded_total{cause_type=\"codex_response_failed\"}")
        );
        assert!(body.contains(
            "coditor_codex_response_status_total{model=\"gpt-5.5\",status=\"failed\"} 1"
        ));
        assert!(body.contains("coditor_sessions_degraded_total{cause_type=\"unknown\"}"));
        assert!(!body.contains("session-id-like-cause-phase-8b"));
        assert!(!body.contains("session_id="));
        assert!(!body.contains("coditor_session_id="));
        for (prefix, suffix) in [
            ("coditor_", "estimated_cost_dollars_total"),
            ("coditor_", "tool_failures_total"),
            ("coditor_", "mcp_"),
            ("coditor_", "skill_events_total"),
            ("coditor_", "active_sessions"),
            ("coditor_", "weekly_tokens"),
            ("coditor_", "history_"),
        ] {
            let dropped_metric = format!("{prefix}{suffix}");
            assert!(
                !body.contains(&dropped_metric),
                "non-Envoy Codex metric family remained exposed: {dropped_metric}"
            );
        }
    }
}
