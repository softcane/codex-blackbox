use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionState {
    Healthy,
    Watching,
    Careful,
    Stop,
    Blocked,
    Cooldown,
    Ended,
}

impl DecisionState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Healthy => "Healthy",
            Self::Watching => "Watching",
            Self::Careful => "Careful",
            Self::Stop => "Stop",
            Self::Blocked => "Blocked",
            Self::Cooldown => "Cooldown",
            Self::Ended => "Ended",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct PolicyBlockFacts {
    pub rule: String,
    pub reason: String,
    pub current: Option<String>,
    pub limit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub recovery_action: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct CooldownFacts {
    pub reason: String,
    pub retry_after_seconds: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ObservedSessionFacts {
    pub session_id: Option<String>,
    pub observed_codex_responses: bool,
    pub core_unavailable: bool,
    pub ended: bool,
    pub total_turns: u32,
    pub total_tokens: u64,
    pub max_context_fill_percent: Option<f64>,
    pub failed_responses: u32,
    pub incomplete_responses: u32,
    pub unknown_responses: u32,
    pub model_mismatch: bool,
    pub accounting_anomalies: u32,
    pub local_estimate_trusted_for_budget_enforcement: Option<bool>,
    pub policy_block: Option<PolicyBlockFacts>,
    pub policy_issues: Vec<String>,
    pub cooldown: Option<CooldownFacts>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DecisionCorrelation {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub observed_codex_responses: bool,
    pub ended: bool,
    pub total_turns: u32,
    pub total_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_context_fill_percent: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Decision {
    pub state: DecisionState,
    pub primary_reason: String,
    pub secondary_reasons: Vec<String>,
    pub next_action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drill_down_command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy_block: Option<PolicyBlockFacts>,
    pub correlation: DecisionCorrelation,
}

const CAREFUL_CONTEXT_PERCENT: f64 = 70.0;
const STOP_CONTEXT_PERCENT: f64 = 85.0;

pub fn decide(facts: &ObservedSessionFacts) -> Decision {
    let correlation = DecisionCorrelation {
        session_id: facts.session_id.clone(),
        observed_codex_responses: facts.observed_codex_responses,
        ended: facts.ended,
        total_turns: facts.total_turns,
        total_tokens: facts.total_tokens,
        max_context_fill_percent: facts.max_context_fill_percent,
    };

    if let Some(block) = facts.policy_block.as_ref() {
        let mut secondary = vec![format!("rule {}", block.rule)];
        if let Some(current) = block.current.as_ref() {
            secondary.push(format!("current {current}"));
        }
        if let Some(limit) = block.limit.as_ref() {
            secondary.push(format!("limit {limit}"));
        }
        if let Some(session_id) = block.session_id.as_ref() {
            secondary.push(format!("session {session_id}"));
        }
        return Decision {
            state: DecisionState::Blocked,
            primary_reason: nonempty_or(&block.reason, "local policy blocked request"),
            secondary_reasons: secondary,
            next_action: nonempty_or(&block.recovery_action, "restart narrower"),
            drill_down_command: Some(postmortem_command(facts.session_id.as_deref())),
            policy_block: Some(block.clone()),
            correlation,
        };
    }

    if let Some(cooldown) = facts.cooldown.as_ref() {
        let mut secondary = Vec::new();
        if let Some(seconds) = cooldown.retry_after_seconds {
            secondary.push(format!("retry after {seconds}s"));
        }
        return Decision {
            state: DecisionState::Cooldown,
            primary_reason: nonempty_or(&cooldown.reason, "upstream errors"),
            secondary_reasons: secondary,
            next_action: "wait before retry".to_string(),
            drill_down_command: Some(postmortem_command(facts.session_id.as_deref())),
            policy_block: None,
            correlation,
        };
    }

    if facts.core_unavailable {
        return Decision {
            state: DecisionState::Watching,
            primary_reason: "core unavailable".to_string(),
            secondary_reasons: Vec::new(),
            next_action: "start codex-blackbox up".to_string(),
            drill_down_command: None,
            policy_block: None,
            correlation,
        };
    }

    if !facts.policy_issues.is_empty() {
        return careful_decision(
            "guard policy issue",
            facts.policy_issues.clone(),
            facts,
            correlation,
        );
    }

    if !facts.observed_codex_responses {
        return Decision {
            state: DecisionState::Watching,
            primary_reason: "waiting for first observed Codex Responses request".to_string(),
            secondary_reasons: Vec::new(),
            next_action: "wait for observed traffic".to_string(),
            drill_down_command: None,
            policy_block: None,
            correlation,
        };
    }

    if facts.total_turns == 0 && !facts.ended {
        return Decision {
            state: DecisionState::Watching,
            primary_reason: "waiting for first observed Codex Responses turn".to_string(),
            secondary_reasons: Vec::new(),
            next_action: "wait for response evidence".to_string(),
            drill_down_command: None,
            policy_block: None,
            correlation,
        };
    }

    if facts.failed_responses > 0 {
        return stop_decision(
            "response failed",
            status_secondary("failed", facts.failed_responses),
            facts,
            correlation,
        );
    }

    if facts.incomplete_responses > 0 {
        return stop_decision(
            "response incomplete",
            status_secondary("incomplete", facts.incomplete_responses),
            facts,
            correlation,
        );
    }

    if facts.accounting_anomalies > 0 {
        return stop_decision(
            "accounting anomaly",
            vec![format!("{} anomalies", facts.accounting_anomalies)],
            facts,
            correlation,
        );
    }

    if facts
        .max_context_fill_percent
        .is_some_and(|percent| percent >= STOP_CONTEXT_PERCENT)
    {
        return stop_decision(
            &context_reason(facts.max_context_fill_percent),
            Vec::new(),
            facts,
            correlation,
        );
    }

    if facts
        .max_context_fill_percent
        .is_some_and(|percent| percent >= CAREFUL_CONTEXT_PERCENT)
    {
        return careful_decision(
            &context_reason(facts.max_context_fill_percent),
            Vec::new(),
            facts,
            correlation,
        );
    }

    if facts.model_mismatch {
        return careful_decision("served model changed", Vec::new(), facts, correlation);
    }

    if facts.unknown_responses > 0 {
        return careful_decision(
            "unknown response status",
            status_secondary("unknown", facts.unknown_responses),
            facts,
            correlation,
        );
    }

    if facts.ended {
        return Decision {
            state: DecisionState::Ended,
            primary_reason: format!(
                "{} turns, {} tokens",
                facts.total_turns,
                format_tokens(facts.total_tokens)
            ),
            secondary_reasons: Vec::new(),
            next_action: "read postmortem".to_string(),
            drill_down_command: Some(postmortem_command(facts.session_id.as_deref())),
            policy_block: None,
            correlation,
        };
    }

    if facts.local_estimate_trusted_for_budget_enforcement == Some(false) {
        return careful_decision("local estimate untrusted", Vec::new(), facts, correlation);
    }

    Decision {
        state: DecisionState::Healthy,
        primary_reason: facts
            .max_context_fill_percent
            .map(|_| context_reason(facts.max_context_fill_percent))
            .unwrap_or_else(|| "Codex Responses observed".to_string()),
        secondary_reasons: Vec::new(),
        next_action: "continue".to_string(),
        drill_down_command: None,
        policy_block: None,
        correlation,
    }
}

fn careful_decision(
    primary_reason: &str,
    secondary_reasons: Vec<String>,
    facts: &ObservedSessionFacts,
    correlation: DecisionCorrelation,
) -> Decision {
    Decision {
        state: DecisionState::Careful,
        primary_reason: primary_reason.to_string(),
        secondary_reasons,
        next_action: if primary_reason == "local estimate untrusted" {
            "keep budgets advisory".to_string()
        } else if primary_reason == "guard policy issue" {
            "fix policy or continue unguarded".to_string()
        } else {
            "narrow next prompt".to_string()
        },
        drill_down_command: Some(postmortem_command(facts.session_id.as_deref())),
        policy_block: None,
        correlation,
    }
}

fn stop_decision(
    primary_reason: &str,
    secondary_reasons: Vec<String>,
    facts: &ObservedSessionFacts,
    correlation: DecisionCorrelation,
) -> Decision {
    Decision {
        state: DecisionState::Stop,
        primary_reason: primary_reason.to_string(),
        secondary_reasons,
        next_action: "inspect postmortem".to_string(),
        drill_down_command: Some(postmortem_command(facts.session_id.as_deref())),
        policy_block: None,
        correlation,
    }
}

fn postmortem_command(session_id: Option<&str>) -> String {
    match session_id.filter(|value| !value.trim().is_empty()) {
        Some(session_id) => format!("codex-blackbox postmortem {session_id}"),
        None => "codex-blackbox postmortem last".to_string(),
    }
}

fn context_reason(percent: Option<f64>) -> String {
    format!("context {:.0}%", percent.unwrap_or(0.0).clamp(0.0, 100.0))
}

fn status_secondary(status: &str, count: u32) -> Vec<String> {
    if count <= 1 {
        Vec::new()
    } else {
        vec![format!("{count} {status} responses")]
    }
}

fn nonempty_or(value: &str, fallback: &str) -> String {
    if value.trim().is_empty() {
        fallback.to_string()
    } else {
        value.to_string()
    }
}

fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{}K", tokens / 1_000)
    } else {
        tokens.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::{decide, CooldownFacts, DecisionState, ObservedSessionFacts, PolicyBlockFacts};

    fn observed() -> ObservedSessionFacts {
        ObservedSessionFacts {
            session_id: Some("session_decision".to_string()),
            observed_codex_responses: true,
            total_turns: 2,
            total_tokens: 12_345,
            max_context_fill_percent: Some(31.0),
            local_estimate_trusted_for_budget_enforcement: Some(true),
            ..Default::default()
        }
    }

    #[test]
    fn decision_selects_healthy_from_low_risk_observed_facts() {
        let decision = decide(&observed());

        assert_eq!(decision.state, DecisionState::Healthy);
        assert_eq!(decision.primary_reason, "context 31%");
        assert_eq!(decision.next_action, "continue");
        assert_eq!(decision.drill_down_command, None);
    }

    #[test]
    fn decision_selects_watching_when_no_codex_responses_evidence_exists() {
        let decision = decide(&ObservedSessionFacts::default());

        assert_eq!(decision.state, DecisionState::Watching);
        assert_eq!(
            decision.primary_reason,
            "waiting for first observed Codex Responses request"
        );
        assert_eq!(decision.next_action, "wait for observed traffic");
    }

    #[test]
    fn decision_selects_watching_until_a_turn_summary_is_observed() {
        let decision = decide(&ObservedSessionFacts {
            observed_codex_responses: true,
            ..Default::default()
        });

        assert_eq!(decision.state, DecisionState::Watching);
        assert_eq!(
            decision.primary_reason,
            "waiting for first observed Codex Responses turn"
        );
        assert_eq!(decision.next_action, "wait for response evidence");
    }

    #[test]
    fn decision_selects_watching_for_core_unavailable() {
        let decision = decide(&ObservedSessionFacts {
            core_unavailable: true,
            ..Default::default()
        });

        assert_eq!(decision.state, DecisionState::Watching);
        assert_eq!(decision.primary_reason, "core unavailable");
        assert_eq!(decision.next_action, "start codex-blackbox up");
    }

    #[test]
    fn decision_selects_careful_for_context_pressure() {
        let mut facts = observed();
        facts.max_context_fill_percent = Some(72.0);

        let decision = decide(&facts);

        assert_eq!(decision.state, DecisionState::Careful);
        assert_eq!(decision.primary_reason, "context 72%");
        assert_eq!(decision.next_action, "narrow next prompt");
        assert_eq!(
            decision.drill_down_command.as_deref(),
            Some("codex-blackbox postmortem session_decision")
        );
    }

    #[test]
    fn decision_selects_stop_for_critical_context_pressure() {
        let mut facts = observed();
        facts.max_context_fill_percent = Some(85.0);

        let decision = decide(&facts);

        assert_eq!(decision.state, DecisionState::Stop);
        assert_eq!(decision.primary_reason, "context 85%");
        assert_eq!(decision.next_action, "inspect postmortem");
        assert_eq!(
            decision.drill_down_command.as_deref(),
            Some("codex-blackbox postmortem session_decision")
        );
    }

    #[test]
    fn decision_selects_stop_for_failed_incomplete_or_anomalous_responses() {
        let mut failed = observed();
        failed.failed_responses = 1;
        assert_eq!(decide(&failed).state, DecisionState::Stop);
        assert_eq!(decide(&failed).primary_reason, "response failed");

        let mut incomplete = observed();
        incomplete.incomplete_responses = 1;
        assert_eq!(decide(&incomplete).state, DecisionState::Stop);
        assert_eq!(decide(&incomplete).primary_reason, "response incomplete");

        let mut anomalous = observed();
        anomalous.accounting_anomalies = 1;
        assert_eq!(decide(&anomalous).state, DecisionState::Stop);
        assert_eq!(decide(&anomalous).primary_reason, "accounting anomaly");
    }

    #[test]
    fn decision_precedence_keeps_harder_stops_above_advisory_states() {
        let mut blocked = observed();
        blocked.failed_responses = 1;
        blocked.cooldown = Some(CooldownFacts {
            reason: "upstream errors".to_string(),
            retry_after_seconds: Some(60),
        });
        blocked.policy_block = Some(PolicyBlockFacts {
            rule: "session_token_budget".to_string(),
            reason: "token budget exceeded".to_string(),
            current: Some("125000 tokens".to_string()),
            limit: Some("120000 tokens".to_string()),
            session_id: Some("session_decision".to_string()),
            recovery_action: "restart narrower".to_string(),
        });
        assert_eq!(decide(&blocked).state, DecisionState::Blocked);

        let mut cooldown = observed();
        cooldown.failed_responses = 1;
        cooldown.cooldown = Some(CooldownFacts {
            reason: "upstream errors".to_string(),
            retry_after_seconds: Some(60),
        });
        assert_eq!(decide(&cooldown).state, DecisionState::Cooldown);

        let mut failed = observed();
        failed.failed_responses = 1;
        failed.ended = true;
        failed.max_context_fill_percent = Some(90.0);
        failed.local_estimate_trusted_for_budget_enforcement = Some(false);
        assert_eq!(decide(&failed).state, DecisionState::Stop);
    }

    #[test]
    fn decision_selects_blocked_with_policy_recovery_context() {
        let decision = decide(&ObservedSessionFacts {
            session_id: Some("session_blocked".to_string()),
            policy_block: Some(PolicyBlockFacts {
                rule: "session_token_budget".to_string(),
                reason: "token budget exceeded".to_string(),
                current: Some("125000 tokens".to_string()),
                limit: Some("120000 tokens".to_string()),
                session_id: Some("session_blocked".to_string()),
                recovery_action: "restart narrower".to_string(),
            }),
            ..Default::default()
        });

        assert_eq!(decision.state, DecisionState::Blocked);
        assert_eq!(decision.primary_reason, "token budget exceeded");
        assert_eq!(decision.next_action, "restart narrower");
        assert!(decision
            .secondary_reasons
            .contains(&"rule session_token_budget".to_string()));
        assert!(decision
            .secondary_reasons
            .contains(&"session session_blocked".to_string()));
        assert_eq!(
            decision
                .policy_block
                .as_ref()
                .and_then(|block| block.session_id.as_deref()),
            Some("session_blocked")
        );
        assert_eq!(
            decision.drill_down_command.as_deref(),
            Some("codex-blackbox postmortem session_blocked")
        );
    }

    #[test]
    fn decision_serializes_policy_block_facts_for_machine_output() {
        let decision = decide(&ObservedSessionFacts {
            session_id: Some("session_blocked".to_string()),
            policy_block: Some(PolicyBlockFacts {
                rule: "session_token_budget".to_string(),
                reason: "token budget exceeded".to_string(),
                current: Some("125000 tokens".to_string()),
                limit: Some("120000 tokens".to_string()),
                session_id: Some("session_blocked".to_string()),
                recovery_action: "restart narrower".to_string(),
            }),
            ..Default::default()
        });
        let json = serde_json::to_value(decision).expect("decision json");

        assert_eq!(
            json.pointer("/policy_block/rule").and_then(|v| v.as_str()),
            Some("session_token_budget")
        );
        assert_eq!(
            json.pointer("/policy_block/current")
                .and_then(|v| v.as_str()),
            Some("125000 tokens")
        );
        assert_eq!(
            json.pointer("/policy_block/limit").and_then(|v| v.as_str()),
            Some("120000 tokens")
        );
        assert_eq!(
            json.pointer("/policy_block/session_id")
                .and_then(|v| v.as_str()),
            Some("session_blocked")
        );
        assert_eq!(
            json.pointer("/policy_block/recovery_action")
                .and_then(|v| v.as_str()),
            Some("restart narrower")
        );
    }

    #[test]
    fn decision_reports_policy_issue_without_blocking() {
        let mut facts = observed();
        facts.policy_issues = vec!["policy_load_failed: invalid policy".to_string()];

        let decision = decide(&facts);

        assert_eq!(decision.state, DecisionState::Careful);
        assert_eq!(decision.primary_reason, "guard policy issue");
        assert_eq!(decision.next_action, "fix policy or continue unguarded");
        assert!(decision.policy_block.is_none());
    }

    #[test]
    fn decision_reports_policy_issue_even_before_observed_traffic() {
        let decision = decide(&ObservedSessionFacts {
            policy_issues: vec!["policy_load_failed: invalid policy".to_string()],
            ..Default::default()
        });

        assert_eq!(decision.state, DecisionState::Careful);
        assert_eq!(decision.primary_reason, "guard policy issue");
        assert!(decision.policy_block.is_none());
    }

    #[test]
    fn decision_selects_cooldown() {
        let decision = decide(&ObservedSessionFacts {
            cooldown: Some(CooldownFacts {
                reason: "upstream errors".to_string(),
                retry_after_seconds: Some(60),
            }),
            ..Default::default()
        });

        assert_eq!(decision.state, DecisionState::Cooldown);
        assert_eq!(decision.primary_reason, "upstream errors");
        assert_eq!(decision.next_action, "wait before retry");
        assert_eq!(
            decision.drill_down_command.as_deref(),
            Some("codex-blackbox postmortem last")
        );
    }

    #[test]
    fn decision_selects_ended_for_completed_idle_session() {
        let mut facts = observed();
        facts.ended = true;
        facts.total_turns = 5;
        facts.total_tokens = 84_000;
        facts.max_context_fill_percent = Some(12.0);

        let decision = decide(&facts);

        assert_eq!(decision.state, DecisionState::Ended);
        assert_eq!(decision.primary_reason, "5 turns, 84K tokens");
        assert_eq!(decision.next_action, "read postmortem");
    }

    #[test]
    fn decision_keeps_ended_when_pricing_is_untrusted() {
        let mut facts = observed();
        facts.ended = true;
        facts.total_turns = 5;
        facts.total_tokens = 84_000;
        facts.local_estimate_trusted_for_budget_enforcement = Some(false);

        let decision = decide(&facts);

        assert_eq!(decision.state, DecisionState::Ended);
        assert_eq!(decision.primary_reason, "5 turns, 84K tokens");
    }

    #[test]
    fn decision_output_omits_unsupported_surfaces() {
        let rendered = serde_json::to_string(&decide(&observed())).expect("decision json");

        assert!(
            !rendered.contains("\x1b["),
            "decision JSON must not contain ANSI escapes: {rendered}"
        );
        for forbidden in [
            "tool_result",
            "tool results",
            "mcp_lifecycle",
            "skill_lifecycle",
            "cache_lifecycle",
            "provider_quota",
            "quota",
        ] {
            assert!(
                !rendered.to_ascii_lowercase().contains(forbidden),
                "decision output must not expose unsupported surface {forbidden}: {rendered}"
            );
        }
    }

    #[test]
    fn decision_json_contract_is_stable_and_uncolored() {
        let mut facts = observed();
        facts.max_context_fill_percent = None;
        let rendered = serde_json::to_string(&decide(&facts)).expect("decision json");

        assert_eq!(
            rendered,
            r#"{"state":"healthy","primary_reason":"Codex Responses observed","secondary_reasons":[],"next_action":"continue","correlation":{"session_id":"session_decision","observed_codex_responses":true,"ended":false,"total_turns":2,"total_tokens":12345}}"#
        );
        assert!(!rendered.contains("\x1b["));
        for forbidden in [
            "tool_result",
            "mcp_lifecycle",
            "skill_lifecycle",
            "cache_lifecycle",
            "provider_quota",
        ] {
            assert!(!rendered.contains(forbidden));
        }
    }
}
