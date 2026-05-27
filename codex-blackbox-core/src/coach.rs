use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::decision::{
    decide, Decision, DecisionSignal, EvidenceSourceCount, ObservedSessionFacts, WarningFact,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceSource {
    Proxy,
    Hook,
    Transcript,
    AppServer,
    UserPolicy,
}

impl EvidenceSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Proxy => "proxy",
            Self::Hook => "hook",
            Self::Transcript => "transcript",
            Self::AppServer => "app_server",
            Self::UserPolicy => "user_policy",
        }
    }

    pub fn from_str(value: &str) -> Self {
        match value {
            "proxy" => Self::Proxy,
            "hook" => Self::Hook,
            "transcript" => Self::Transcript,
            "app_server" => Self::AppServer,
            "user_policy" => Self::UserPolicy,
            _ => Self::Hook,
        }
    }
}

impl std::fmt::Display for EvidenceSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfidenceLevel {
    High,
    Medium,
    Low,
}

impl ConfidenceLevel {
    pub fn from_str(value: &str) -> Self {
        match value {
            "medium" => Self::Medium,
            "low" => Self::Low,
            _ => Self::High,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrivacyClassification {
    PublicAggregate,
    DerivedPrivate,
    SensitiveRedacted,
}

impl PrivacyClassification {
    pub fn from_str(value: &str) -> Self {
        match value {
            "public_aggregate" => Self::PublicAggregate,
            "sensitive_redacted" => Self::SensitiveRedacted,
            _ => Self::DerivedPrivate,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventCategory {
    ModelTurnStarted,
    ModelTurnCompleted,
    ModelTurnFailed,
    ModelTurnIncomplete,
    ModelTurnUnknown,
    ToolIntentObserved,
    SupportedToolStarted,
    SupportedToolCompleted,
    SupportedToolFailed,
    ValidationStarted,
    ValidationSucceeded,
    ValidationFailed,
    FileEditObserved,
    PromptSubmitted,
    StopObserved,
    CompactionObserved,
    ContextPressureObserved,
    RateLimitPressureObserved,
    CoachWarningEmitted,
    CoachBlockEmitted,
    PricingTrustObserved,
    DurableEvidenceMissing,
    BaselineLearned,
}

impl EventCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ModelTurnStarted => "model_turn_started",
            Self::ModelTurnCompleted => "model_turn_completed",
            Self::ModelTurnFailed => "model_turn_failed",
            Self::ModelTurnIncomplete => "model_turn_incomplete",
            Self::ModelTurnUnknown => "model_turn_unknown",
            Self::ToolIntentObserved => "tool_intent_observed",
            Self::SupportedToolStarted => "supported_tool_started",
            Self::SupportedToolCompleted => "supported_tool_completed",
            Self::SupportedToolFailed => "supported_tool_failed",
            Self::ValidationStarted => "validation_started",
            Self::ValidationSucceeded => "validation_succeeded",
            Self::ValidationFailed => "validation_failed",
            Self::FileEditObserved => "file_edit_observed",
            Self::PromptSubmitted => "prompt_submitted",
            Self::StopObserved => "stop_observed",
            Self::CompactionObserved => "compaction_observed",
            Self::ContextPressureObserved => "context_pressure_observed",
            Self::RateLimitPressureObserved => "rate_limit_pressure_observed",
            Self::CoachWarningEmitted => "coach_warning_emitted",
            Self::CoachBlockEmitted => "coach_block_emitted",
            Self::PricingTrustObserved => "pricing_trust_observed",
            Self::DurableEvidenceMissing => "durable_evidence_missing",
            Self::BaselineLearned => "baseline_learned",
        }
    }

    pub fn from_str(value: &str) -> Self {
        match value {
            "model_turn_started" => Self::ModelTurnStarted,
            "model_turn_completed" => Self::ModelTurnCompleted,
            "model_turn_failed" => Self::ModelTurnFailed,
            "model_turn_incomplete" => Self::ModelTurnIncomplete,
            "model_turn_unknown" => Self::ModelTurnUnknown,
            "tool_intent_observed" => Self::ToolIntentObserved,
            "supported_tool_started" => Self::SupportedToolStarted,
            "supported_tool_completed" => Self::SupportedToolCompleted,
            "supported_tool_failed" => Self::SupportedToolFailed,
            "validation_started" => Self::ValidationStarted,
            "validation_succeeded" => Self::ValidationSucceeded,
            "validation_failed" => Self::ValidationFailed,
            "file_edit_observed" => Self::FileEditObserved,
            "prompt_submitted" => Self::PromptSubmitted,
            "stop_observed" => Self::StopObserved,
            "compaction_observed" => Self::CompactionObserved,
            "context_pressure_observed" => Self::ContextPressureObserved,
            "rate_limit_pressure_observed" => Self::RateLimitPressureObserved,
            "coach_warning_emitted" => Self::CoachWarningEmitted,
            "coach_block_emitted" => Self::CoachBlockEmitted,
            "pricing_trust_observed" => Self::PricingTrustObserved,
            "durable_evidence_missing" => Self::DurableEvidenceMissing,
            "baseline_learned" => Self::BaselineLearned,
            _ => Self::ModelTurnUnknown,
        }
    }
}

impl std::fmt::Display for EventCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NormalizedEvent {
    pub timestamp: String,
    pub evidence_source: EvidenceSource,
    pub category: EventCategory,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<String>,
    pub privacy: PrivacyClassification,
    pub confidence: ConfidenceLevel,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub payload_summary: BTreeMap<String, Value>,
}

impl NormalizedEvent {
    pub fn new(
        timestamp: impl Into<String>,
        evidence_source: EvidenceSource,
        category: EventCategory,
    ) -> Self {
        Self {
            timestamp: timestamp.into(),
            evidence_source,
            category,
            reason_code: None,
            privacy: PrivacyClassification::DerivedPrivate,
            confidence: ConfidenceLevel::High,
            session_id: None,
            turn_id: None,
            payload_summary: BTreeMap::new(),
        }
    }

    pub fn with_session(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    pub fn with_turn(mut self, turn_id: impl Into<String>) -> Self {
        self.turn_id = Some(turn_id.into());
        self
    }

    pub fn with_reason(mut self, reason_code: impl Into<String>) -> Self {
        self.reason_code = Some(reason_code.into());
        self
    }

    pub fn with_privacy(mut self, privacy: PrivacyClassification) -> Self {
        self.privacy = privacy;
        self
    }

    pub fn with_confidence(mut self, confidence: ConfidenceLevel) -> Self {
        self.confidence = confidence;
        self
    }

    pub fn with_payload_value(mut self, key: impl Into<String>, value: Value) -> Self {
        self.payload_summary.insert(key.into(), value);
        self
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponseStatusCounts {
    pub completed: u32,
    pub failed: u32,
    pub incomplete: u32,
    pub unknown: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenTotals {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
    pub local_total_tokens: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationResultSummary {
    pub category: String,
    pub result: String,
    pub evidence_source: EvidenceSource,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookActionSummary {
    pub action: String,
    pub reason_code: String,
    pub evidence_source: EvidenceSource,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TrustState {
    pub cost_trusted_for_budget_enforcement: bool,
    pub dollar_budget_configured: bool,
    pub rate_limit_pressure: Option<String>,
    pub context_trust: String,
}

impl Default for TrustState {
    fn default() -> Self {
        Self {
            cost_trusted_for_budget_enforcement: false,
            dollar_budget_configured: false,
            rate_limit_pressure: None,
            context_trust: "proxy_tokens_first".to_string(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionState {
    pub session_id: Option<String>,
    pub latest_known_state: String,
    pub turn_count: u32,
    pub response_status_counts: ResponseStatusCounts,
    pub token_totals: TokenTotals,
    pub max_context_fill_percent: Option<f64>,
    pub recent_validation_results: Vec<ValidationResultSummary>,
    pub repeated_failure_counters: BTreeMap<String, u32>,
    pub recent_edit_without_validation: bool,
    pub recent_risky_command: bool,
    pub recent_blind_retry: bool,
    pub hook_actions: Vec<HookActionSummary>,
    pub trust: TrustState,
    pub postmortem_available: bool,
    pub postmortem_link: Option<String>,
    pub evidence_source_summary: BTreeMap<EvidenceSource, u32>,
    pub missing_durable_evidence: bool,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            session_id: None,
            latest_known_state: "unknown".to_string(),
            turn_count: 0,
            response_status_counts: ResponseStatusCounts::default(),
            token_totals: TokenTotals::default(),
            max_context_fill_percent: None,
            recent_validation_results: Vec::new(),
            repeated_failure_counters: BTreeMap::new(),
            recent_edit_without_validation: false,
            recent_risky_command: false,
            recent_blind_retry: false,
            hook_actions: Vec::new(),
            trust: TrustState::default(),
            postmortem_available: false,
            postmortem_link: None,
            evidence_source_summary: BTreeMap::new(),
            missing_durable_evidence: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalSeverity {
    Watching,
    Careful,
    Stop,
    Blocked,
    Cooldown,
}

impl SignalSeverity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Watching => "watching",
            Self::Careful => "careful",
            Self::Stop => "stop",
            Self::Blocked => "blocked",
            Self::Cooldown => "cooldown",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoachSignal {
    pub signal_name: String,
    pub severity: SignalSeverity,
    pub reason_code: String,
    pub evidence_source: EvidenceSource,
    pub reason: String,
    pub next_action: String,
    pub advisory: bool,
}

impl CoachSignal {
    fn new(
        signal_name: &str,
        severity: SignalSeverity,
        reason_code: &str,
        evidence_source: EvidenceSource,
        reason: &str,
        next_action: &str,
    ) -> Self {
        Self {
            signal_name: signal_name.to_string(),
            severity,
            reason_code: reason_code.to_string(),
            evidence_source,
            reason: reason.to_string(),
            next_action: next_action.to_string(),
            advisory: evidence_source != EvidenceSource::Proxy,
        }
    }

    pub fn to_decision_signal(&self) -> DecisionSignal {
        DecisionSignal {
            signal_name: self.signal_name.clone(),
            severity: self.severity.as_str().to_string(),
            reason_code: self.reason_code.clone(),
            evidence_source: self.evidence_source.as_str().to_string(),
            reason: self.reason.clone(),
            next_action: self.next_action.clone(),
            advisory: self.advisory,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CompanionSnapshot {
    pub state: SessionState,
    pub signals: Vec<CoachSignal>,
    pub decision: Decision,
    pub timeline: Vec<NormalizedEvent>,
}

pub fn derive_session_state(events: &[NormalizedEvent]) -> SessionState {
    let mut state = SessionState::default();
    let mut last_failed_validation_category: Option<String> = None;

    for event in events {
        *state
            .evidence_source_summary
            .entry(event.evidence_source)
            .or_insert(0) += 1;
        if state.session_id.is_none() {
            state.session_id = event.session_id.clone();
        }

        match event.category {
            EventCategory::ModelTurnStarted => {
                state.latest_known_state = "running".to_string();
            }
            EventCategory::ModelTurnCompleted => {
                state.latest_known_state = "active".to_string();
                state.turn_count = state.turn_count.saturating_add(1);
                state.response_status_counts.completed =
                    state.response_status_counts.completed.saturating_add(1);
                add_token_payload(&mut state, event);
            }
            EventCategory::ModelTurnFailed => {
                state.latest_known_state = "active".to_string();
                state.turn_count = state.turn_count.saturating_add(1);
                state.response_status_counts.failed =
                    state.response_status_counts.failed.saturating_add(1);
                add_token_payload(&mut state, event);
            }
            EventCategory::ModelTurnIncomplete => {
                state.latest_known_state = "active".to_string();
                state.turn_count = state.turn_count.saturating_add(1);
                state.response_status_counts.incomplete =
                    state.response_status_counts.incomplete.saturating_add(1);
                add_token_payload(&mut state, event);
            }
            EventCategory::ModelTurnUnknown => {
                state.latest_known_state = "active".to_string();
                state.turn_count = state.turn_count.saturating_add(1);
                state.response_status_counts.unknown =
                    state.response_status_counts.unknown.saturating_add(1);
                add_token_payload(&mut state, event);
            }
            EventCategory::ContextPressureObserved => {
                if let Some(fill) = number_payload(event, "fill_percent") {
                    state.max_context_fill_percent = Some(
                        state
                            .max_context_fill_percent
                            .map(|current| current.max(fill))
                            .unwrap_or(fill),
                    );
                }
            }
            EventCategory::ToolIntentObserved => {}
            EventCategory::SupportedToolStarted => {
                if event.reason_code.as_deref() == Some("risky_supported_tool_call") {
                    state.recent_risky_command = true;
                }
            }
            EventCategory::SupportedToolCompleted => {
                if validation_category(event).is_some() {
                    push_validation(&mut state, event, "succeeded");
                    state.recent_edit_without_validation = false;
                    last_failed_validation_category = None;
                }
            }
            EventCategory::SupportedToolFailed => {
                let category = bounded_category(event);
                let count = state
                    .repeated_failure_counters
                    .entry(category.clone())
                    .or_insert(0);
                *count = count.saturating_add(1);
                if *count >= 2 {
                    state.recent_blind_retry = true;
                }
                if validation_category(event).is_some() {
                    push_validation(&mut state, event, "failed");
                    if last_failed_validation_category.as_deref() == Some(category.as_str()) {
                        let count = state
                            .repeated_failure_counters
                            .entry(format!("validation:{category}"))
                            .or_insert(1);
                        *count = count.saturating_add(1);
                    } else {
                        state
                            .repeated_failure_counters
                            .entry(format!("validation:{category}"))
                            .or_insert(1);
                    }
                    last_failed_validation_category = Some(category);
                }
            }
            EventCategory::ValidationStarted => {
                push_validation(&mut state, event, "started");
                state.recent_edit_without_validation = false;
            }
            EventCategory::ValidationSucceeded => {
                push_validation(&mut state, event, "succeeded");
                state.recent_edit_without_validation = false;
                last_failed_validation_category = None;
            }
            EventCategory::ValidationFailed => {
                let category = validation_category(event)
                    .map(str::to_string)
                    .unwrap_or_else(|| bounded_category(event));
                push_validation(&mut state, event, "failed");
                state.recent_edit_without_validation = false;
                let key = format!("validation:{category}");
                let count = state.repeated_failure_counters.entry(key).or_insert(0);
                *count = count.saturating_add(1);
                if last_failed_validation_category.as_deref() == Some(category.as_str()) {
                    state.recent_blind_retry = true;
                }
                last_failed_validation_category = Some(category);
            }
            EventCategory::FileEditObserved => {
                state.recent_edit_without_validation = true;
            }
            EventCategory::PromptSubmitted => {}
            EventCategory::StopObserved => {
                state.latest_known_state = "ended".to_string();
            }
            EventCategory::CompactionObserved => {}
            EventCategory::RateLimitPressureObserved => {
                state.trust.rate_limit_pressure = event
                    .reason_code
                    .clone()
                    .or_else(|| string_payload(event, "pressure").map(str::to_string))
                    .or_else(|| Some("rate_limit_pressure".to_string()));
            }
            EventCategory::CoachWarningEmitted => {
                state.hook_actions.push(HookActionSummary {
                    action: "warn".to_string(),
                    reason_code: event
                        .reason_code
                        .clone()
                        .unwrap_or_else(|| "coach_warning".to_string()),
                    evidence_source: event.evidence_source,
                });
            }
            EventCategory::CoachBlockEmitted => {
                state.hook_actions.push(HookActionSummary {
                    action: "block".to_string(),
                    reason_code: event
                        .reason_code
                        .clone()
                        .unwrap_or_else(|| "coach_block".to_string()),
                    evidence_source: event.evidence_source,
                });
            }
            EventCategory::PricingTrustObserved => {
                state.trust.cost_trusted_for_budget_enforcement =
                    bool_payload(event, "trusted_for_budget_enforcement").unwrap_or(false);
                state.trust.dollar_budget_configured =
                    bool_payload(event, "dollar_budget_configured").unwrap_or(false);
            }
            EventCategory::DurableEvidenceMissing => {
                state.missing_durable_evidence = true;
            }
            EventCategory::BaselineLearned => {}
        }
    }

    state.missing_durable_evidence = !state
        .evidence_source_summary
        .contains_key(&EvidenceSource::Proxy)
        && !events.is_empty();

    state
}

pub fn detect_signals(state: &SessionState) -> Vec<CoachSignal> {
    let mut signals = Vec::new();

    if state.response_status_counts.failed > 0 {
        signals.push(CoachSignal::new(
            "failed_response",
            SignalSeverity::Stop,
            "failed_response",
            EvidenceSource::Proxy,
            "model response failed",
            "inspect the failed response before continuing",
        ));
    }
    if state.response_status_counts.incomplete > 0 {
        signals.push(CoachSignal::new(
            "incomplete_response",
            SignalSeverity::Stop,
            "incomplete_response",
            EvidenceSource::Proxy,
            "model response incomplete",
            "continue with a narrower prompt or raise the output limit",
        ));
    }
    if state.response_status_counts.unknown > 0 {
        signals.push(CoachSignal::new(
            "unknown_response",
            SignalSeverity::Careful,
            "unknown_response",
            EvidenceSource::Proxy,
            "model response status unknown",
            "inspect the postmortem before trusting the result",
        ));
    }
    if let Some(fill) = state.max_context_fill_percent {
        if fill >= 85.0 {
            signals.push(CoachSignal::new(
                "high_context",
                SignalSeverity::Stop,
                "high_context_stop",
                EvidenceSource::Proxy,
                "context pressure is critical",
                "summarize and restart narrower",
            ));
        } else if fill >= 70.0 {
            signals.push(CoachSignal::new(
                "high_context",
                SignalSeverity::Careful,
                "high_context_careful",
                EvidenceSource::Proxy,
                "context pressure is high",
                "avoid broad edits and narrow the next prompt",
            ));
        }
    }

    let repeated_validation_failure = state
        .repeated_failure_counters
        .iter()
        .filter(|(key, _)| key.starts_with("validation:"))
        .map(|(_, count)| *count)
        .max()
        .unwrap_or(0);
    if repeated_validation_failure >= 3 {
        signals.push(CoachSignal::new(
            "repeated_validation_failure",
            SignalSeverity::Stop,
            "repeated_validation_failure",
            EvidenceSource::Hook,
            "validation failed repeatedly",
            "inspect the first failure before editing again",
        ));
    } else if repeated_validation_failure >= 2 {
        signals.push(CoachSignal::new(
            "repeated_validation_failure",
            SignalSeverity::Careful,
            "repeated_validation_failure",
            EvidenceSource::Hook,
            "validation failed twice",
            "inspect the first failure before retrying",
        ));
    }

    if state.recent_edit_without_validation {
        signals.push(CoachSignal::new(
            "unvalidated_edit",
            SignalSeverity::Careful,
            "unvalidated_edit",
            EvidenceSource::Hook,
            "files changed without validation evidence",
            "run the relevant validation before stopping",
        ));
    }

    if state
        .repeated_failure_counters
        .iter()
        .any(|(key, count)| !key.starts_with("validation:") && *count >= 2)
    {
        signals.push(CoachSignal::new(
            "repeated_tool_failure",
            SignalSeverity::Careful,
            "repeated_tool_failure",
            EvidenceSource::Hook,
            "the same tool category failed repeatedly",
            "inspect the error before retrying",
        ));
    }

    if state.recent_blind_retry {
        signals.push(CoachSignal::new(
            "blind_retry",
            SignalSeverity::Careful,
            "blind_retry",
            EvidenceSource::Hook,
            "a retry followed a failure without recovery evidence",
            "change strategy before retrying",
        ));
    }

    if state.recent_risky_command {
        let severity = if state
            .hook_actions
            .iter()
            .any(|action| action.action == "block")
        {
            SignalSeverity::Blocked
        } else {
            SignalSeverity::Careful
        };
        signals.push(CoachSignal::new(
            "risky_supported_tool_call",
            severity,
            "risky_supported_tool_call",
            EvidenceSource::Hook,
            "risky supported tool call observed",
            "confirm scope or use a safer command",
        ));
    }

    if state.trust.dollar_budget_configured && !state.trust.cost_trusted_for_budget_enforcement {
        signals.push(CoachSignal::new(
            "untrusted_pricing",
            SignalSeverity::Careful,
            "untrusted_pricing",
            EvidenceSource::UserPolicy,
            "dollar budget uses untrusted pricing",
            "use token/context budgets or configure trusted pricing",
        ));
    }

    if state.trust.rate_limit_pressure.is_some() {
        signals.push(CoachSignal::new(
            "rate_limit_pressure",
            SignalSeverity::Cooldown,
            "rate_limit_pressure",
            EvidenceSource::Proxy,
            "rate-limit pressure is active",
            "wait before retrying",
        ));
    }

    if state.missing_durable_evidence {
        signals.push(CoachSignal::new(
            "missing_durable_evidence",
            SignalSeverity::Watching,
            "missing_durable_evidence",
            EvidenceSource::Hook,
            "no durable proxy evidence has been observed",
            "wait for Codex Responses traffic before making durable claims",
        ));
    }

    signals
}

pub fn facts_from_state_and_signals(
    state: &SessionState,
    signals: &[CoachSignal],
) -> ObservedSessionFacts {
    ObservedSessionFacts {
        session_id: state.session_id.clone(),
        observed_codex_responses: state
            .evidence_source_summary
            .contains_key(&EvidenceSource::Proxy),
        ended: state.latest_known_state == "ended",
        total_turns: state.turn_count,
        total_tokens: state.token_totals.local_total_tokens,
        max_context_fill_percent: state.max_context_fill_percent,
        failed_responses: state.response_status_counts.failed,
        incomplete_responses: state.response_status_counts.incomplete,
        unknown_responses: state.response_status_counts.unknown,
        local_estimate_trusted_for_budget_enforcement: Some(
            state.trust.cost_trusted_for_budget_enforcement,
        ),
        active_signals: signals
            .iter()
            .map(CoachSignal::to_decision_signal)
            .collect(),
        evidence_sources: state
            .evidence_source_summary
            .iter()
            .map(|(source, count)| EvidenceSourceCount {
                evidence_source: source.as_str().to_string(),
                count: *count,
            })
            .collect(),
        postmortem_available: state.postmortem_available,
        postmortem_link: state.postmortem_link.clone(),
        warning_facts: signals
            .iter()
            .filter(|signal| signal.severity == SignalSeverity::Careful)
            .map(|signal| WarningFact {
                reason_code: signal.reason_code.clone(),
                evidence_source: signal.evidence_source.as_str().to_string(),
                message: signal.reason.clone(),
            })
            .collect(),
        ..Default::default()
    }
}

pub fn companion_snapshot(events: Vec<NormalizedEvent>) -> CompanionSnapshot {
    let state = derive_session_state(&events);
    let signals = detect_signals(&state);
    let decision = decide(&facts_from_state_and_signals(&state, &signals));
    CompanionSnapshot {
        state,
        signals,
        decision,
        timeline: events,
    }
}

pub fn validation_category_from_command(command: &str) -> Option<&'static str> {
    let lower = command.to_ascii_lowercase();
    if lower.contains("cargo test")
        || lower.contains("npm test")
        || lower.contains("pnpm test")
        || lower.contains("pytest")
        || lower.contains("go test")
    {
        Some("test")
    } else if lower.contains("cargo clippy")
        || lower.contains("eslint")
        || lower.contains("ruff")
        || lower.contains("lint")
    {
        Some("lint")
    } else if lower.contains("cargo check") || lower.contains("tsc") || lower.contains("typecheck")
    {
        Some("typecheck")
    } else if lower.contains("cargo build")
        || lower.contains("npm run build")
        || lower.contains("pnpm build")
    {
        Some("build")
    } else {
        None
    }
}

pub fn tool_category(tool_name: &str) -> &'static str {
    let lower = tool_name.to_ascii_lowercase();
    if lower == "bash" {
        "bash"
    } else if lower == "apply_patch" || lower == "edit" || lower == "write" {
        "apply_patch"
    } else if lower.starts_with("mcp__") {
        "mcp"
    } else {
        "other"
    }
}

pub fn command_is_risky(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    [
        "rm -rf",
        "sudo rm",
        "git reset --hard",
        "git clean -fd",
        "chmod -r",
        "chown -r",
        "curl ",
        "wget ",
        "mkfs",
        "dd if=",
        "shutdown",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
        && (lower.contains(" | sh")
            || lower.contains(" | bash")
            || lower.contains("rm ")
            || lower.contains("reset --hard")
            || lower.contains("clean -fd")
            || lower.contains("chmod")
            || lower.contains("chown")
            || lower.contains("mkfs")
            || lower.contains("dd if=")
            || lower.contains("shutdown"))
}

fn add_token_payload(state: &mut SessionState, event: &NormalizedEvent) {
    state.token_totals.input_tokens = state
        .token_totals
        .input_tokens
        .saturating_add(u64_payload(event, "input_tokens"));
    state.token_totals.cached_input_tokens = state
        .token_totals
        .cached_input_tokens
        .saturating_add(u64_payload(event, "cached_input_tokens"));
    state.token_totals.output_tokens = state
        .token_totals
        .output_tokens
        .saturating_add(u64_payload(event, "output_tokens"));
    state.token_totals.reasoning_output_tokens = state
        .token_totals
        .reasoning_output_tokens
        .saturating_add(u64_payload(event, "reasoning_output_tokens"));
    state.token_totals.local_total_tokens = state
        .token_totals
        .local_total_tokens
        .saturating_add(u64_payload(event, "total_tokens"));
    if let Some(fill) = number_payload(event, "context_fill_percent") {
        state.max_context_fill_percent = Some(
            state
                .max_context_fill_percent
                .map(|current| current.max(fill))
                .unwrap_or(fill),
        );
    }
}

fn push_validation(state: &mut SessionState, event: &NormalizedEvent, result: &str) {
    let category = validation_category(event).unwrap_or("unknown").to_string();
    state
        .recent_validation_results
        .push(ValidationResultSummary {
            category,
            result: result.to_string(),
            evidence_source: event.evidence_source,
        });
    if state.recent_validation_results.len() > 10 {
        state.recent_validation_results.remove(0);
    }
}

fn bounded_category(event: &NormalizedEvent) -> String {
    event.reason_code.clone().unwrap_or_else(|| {
        string_payload(event, "tool_category")
            .or_else(|| validation_category(event))
            .unwrap_or("unknown")
            .to_string()
    })
}

fn validation_category(event: &NormalizedEvent) -> Option<&str> {
    string_payload(event, "validation_category")
}

fn string_payload<'a>(event: &'a NormalizedEvent, key: &str) -> Option<&'a str> {
    event.payload_summary.get(key).and_then(Value::as_str)
}

fn u64_payload(event: &NormalizedEvent, key: &str) -> u64 {
    event
        .payload_summary
        .get(key)
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

fn number_payload(event: &NormalizedEvent, key: &str) -> Option<f64> {
    event.payload_summary.get(key).and_then(Value::as_f64)
}

fn bool_payload(event: &NormalizedEvent, key: &str) -> Option<bool> {
    event.payload_summary.get(key).and_then(Value::as_bool)
}

pub fn proxy_turn_event(
    timestamp: impl Into<String>,
    session_id: impl Into<String>,
    turn_id: impl Into<String>,
    status: &str,
    tokens: TokenTotals,
    context_fill_percent: Option<f64>,
) -> NormalizedEvent {
    let category = match status {
        "completed" => EventCategory::ModelTurnCompleted,
        "failed" => EventCategory::ModelTurnFailed,
        "incomplete" => EventCategory::ModelTurnIncomplete,
        _ => EventCategory::ModelTurnUnknown,
    };
    let mut event = NormalizedEvent::new(timestamp, EvidenceSource::Proxy, category)
        .with_session(session_id)
        .with_turn(turn_id)
        .with_reason(status)
        .with_payload_value("input_tokens", json!(tokens.input_tokens))
        .with_payload_value("cached_input_tokens", json!(tokens.cached_input_tokens))
        .with_payload_value("output_tokens", json!(tokens.output_tokens))
        .with_payload_value(
            "reasoning_output_tokens",
            json!(tokens.reasoning_output_tokens),
        )
        .with_payload_value("total_tokens", json!(tokens.local_total_tokens));
    if let Some(fill) = context_fill_percent {
        event = event.with_payload_value("context_fill_percent", json!(fill));
    }
    event
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(category: EventCategory) -> NormalizedEvent {
        NormalizedEvent::new("2026-05-27T00:00:00Z", EvidenceSource::Hook, category)
            .with_session("session_coach")
    }

    #[test]
    fn normalized_events_carry_evidence_privacy_category_and_reason() {
        let event = NormalizedEvent::new(
            "2026-05-27T00:00:00Z",
            EvidenceSource::Proxy,
            EventCategory::ModelTurnFailed,
        )
        .with_session("session_a")
        .with_turn("turn_a")
        .with_reason("failed_response")
        .with_privacy(PrivacyClassification::DerivedPrivate);

        assert_eq!(event.evidence_source, EvidenceSource::Proxy);
        assert_eq!(event.category, EventCategory::ModelTurnFailed);
        assert_eq!(event.reason_code.as_deref(), Some("failed_response"));
        assert_eq!(event.privacy, PrivacyClassification::DerivedPrivate);
    }

    #[test]
    fn session_state_tracks_response_tokens_context_and_evidence_sources() {
        let events = vec![
            proxy_turn_event(
                "2026-05-27T00:00:00Z",
                "session_a",
                "turn_1",
                "completed",
                TokenTotals {
                    input_tokens: 100,
                    cached_input_tokens: 40,
                    output_tokens: 25,
                    reasoning_output_tokens: 5,
                    local_total_tokens: 125,
                },
                Some(31.0),
            ),
            proxy_turn_event(
                "2026-05-27T00:00:01Z",
                "session_a",
                "turn_2",
                "failed",
                TokenTotals {
                    input_tokens: 200,
                    cached_input_tokens: 50,
                    output_tokens: 20,
                    reasoning_output_tokens: 2,
                    local_total_tokens: 220,
                },
                Some(72.0),
            ),
        ];

        let state = derive_session_state(&events);

        assert_eq!(state.turn_count, 2);
        assert_eq!(state.response_status_counts.completed, 1);
        assert_eq!(state.response_status_counts.failed, 1);
        assert_eq!(state.token_totals.local_total_tokens, 345);
        assert_eq!(state.max_context_fill_percent, Some(72.0));
        assert_eq!(
            state.evidence_source_summary.get(&EvidenceSource::Proxy),
            Some(&2)
        );
        assert!(!state.missing_durable_evidence);
    }

    #[test]
    fn signal_engine_detects_repeated_validation_unvalidated_edit_and_blind_retry() {
        let validation_failed = event(EventCategory::ValidationFailed)
            .with_reason("test")
            .with_payload_value("validation_category", json!("test"));
        let events = vec![
            event(EventCategory::FileEditObserved),
            validation_failed.clone(),
            validation_failed,
            event(EventCategory::SupportedToolFailed)
                .with_reason("bash")
                .with_payload_value("tool_category", json!("bash")),
            event(EventCategory::SupportedToolFailed)
                .with_reason("bash")
                .with_payload_value("tool_category", json!("bash")),
        ];
        let state = derive_session_state(&events);
        let signals = detect_signals(&state);

        assert!(signals
            .iter()
            .any(|signal| signal.signal_name == "repeated_validation_failure"));
        assert!(signals
            .iter()
            .any(|signal| signal.signal_name == "repeated_tool_failure"));
        assert!(signals
            .iter()
            .any(|signal| signal.signal_name == "blind_retry"));
        assert!(!signals
            .iter()
            .any(|signal| signal.signal_name == "unvalidated_edit"));
    }

    #[test]
    fn signal_engine_labels_hook_result_evidence_as_advisory() {
        let events = vec![event(EventCategory::FileEditObserved)];
        let state = derive_session_state(&events);
        let signals = detect_signals(&state);
        let unvalidated = signals
            .iter()
            .find(|signal| signal.signal_name == "unvalidated_edit")
            .expect("unvalidated edit signal");

        assert_eq!(unvalidated.evidence_source, EvidenceSource::Hook);
        assert!(unvalidated.advisory);
        assert!(signals
            .iter()
            .any(|signal| signal.signal_name == "missing_durable_evidence"));
    }

    #[test]
    fn companion_snapshot_uses_shared_decision_object() {
        let events = vec![proxy_turn_event(
            "2026-05-27T00:00:00Z",
            "session_a",
            "turn_1",
            "incomplete",
            TokenTotals {
                input_tokens: 10,
                cached_input_tokens: 0,
                output_tokens: 1,
                reasoning_output_tokens: 0,
                local_total_tokens: 11,
            },
            Some(10.0),
        )];
        let snapshot = companion_snapshot(events);

        assert_eq!(snapshot.decision.state.label(), "Stop");
        assert!(snapshot
            .decision
            .active_signals
            .iter()
            .any(|signal| signal.signal_name == "incomplete_response"));
        assert!(snapshot
            .decision
            .evidence_sources
            .iter()
            .any(|source| source.evidence_source == "proxy"));
    }

    #[test]
    fn command_classifiers_are_bounded_and_privacy_safe() {
        assert_eq!(
            validation_category_from_command("cargo test --workspace"),
            Some("test")
        );
        assert_eq!(tool_category("mcp__fs__read_file"), "mcp");
        assert!(command_is_risky("git reset --hard HEAD"));
        assert!(!command_is_risky("cargo test"));
    }
}
