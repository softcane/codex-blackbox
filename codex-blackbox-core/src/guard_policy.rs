use std::path::Path;

use serde::Deserialize;
use serde::Serialize;

use crate::decision::{CooldownFacts, PolicyBlockFacts};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct GuardPolicy {
    pub session_token_budget: Option<u64>,
    pub session_cost_budget_dollars: Option<f64>,
    pub context_warn_percent: Option<f64>,
    pub context_block_percent: Option<f64>,
    pub failed_response_warn_count: Option<u32>,
    pub failed_response_block_count: Option<u32>,
    pub incomplete_response_warn_count: Option<u32>,
    pub incomplete_response_block_count: Option<u32>,
    pub unknown_response_warn_count: Option<u32>,
    pub unknown_response_block_count: Option<u32>,
    pub accounting_anomaly_warn_count: Option<u32>,
    pub accounting_anomaly_block_count: Option<u32>,
    pub model_mismatch_warn: bool,
    pub model_mismatch_block: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GuardPolicyIssue {
    pub issue_type: String,
    pub message: String,
    pub recovery_action: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct GuardPolicyLoad {
    pub policy: GuardPolicy,
    pub issues: Vec<GuardPolicyIssue>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct GuardCooldownEvidence {
    pub reason: String,
    pub retry_after_seconds: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct GuardEvidence {
    pub session_id: Option<String>,
    pub observed_codex_responses: bool,
    pub applies_to_next_request: bool,
    pub session_total_tokens: Option<u64>,
    pub session_estimated_cost_dollars: Option<f64>,
    pub local_estimate_trusted_for_budget_enforcement: bool,
    pub max_context_fill_percent: Option<f64>,
    pub failed_responses: u32,
    pub incomplete_responses: u32,
    pub unknown_responses: u32,
    pub accounting_anomalies: u32,
    pub model_mismatch: bool,
    pub cooldown: Option<GuardCooldownEvidence>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct GuardEvaluation {
    pub block: Option<PolicyBlockFacts>,
    pub cooldown: Option<CooldownFacts>,
    pub policy_issues: Vec<GuardPolicyIssue>,
}

#[derive(Debug, Deserialize)]
struct GuardPolicyFile {
    session_token_budget: Option<u64>,
    session_budget_tokens: Option<u64>,
    session_cost_budget_dollars: Option<f64>,
    session_budget_dollars: Option<f64>,
    context_warn_percent: Option<f64>,
    context_block_percent: Option<f64>,
    failed_response_warn_count: Option<u32>,
    failed_response_block_count: Option<u32>,
    incomplete_response_warn_count: Option<u32>,
    incomplete_response_block_count: Option<u32>,
    unknown_response_warn_count: Option<u32>,
    unknown_response_block_count: Option<u32>,
    accounting_anomaly_warn_count: Option<u32>,
    accounting_anomaly_block_count: Option<u32>,
    model_mismatch_warn: Option<bool>,
    model_mismatch_block: Option<bool>,
}

impl From<GuardPolicyFile> for GuardPolicy {
    fn from(value: GuardPolicyFile) -> Self {
        Self {
            session_token_budget: value.session_token_budget.or(value.session_budget_tokens),
            session_cost_budget_dollars: value
                .session_cost_budget_dollars
                .or(value.session_budget_dollars),
            context_warn_percent: positive_percent(value.context_warn_percent),
            context_block_percent: positive_percent(value.context_block_percent),
            failed_response_warn_count: positive_count(value.failed_response_warn_count),
            failed_response_block_count: positive_count(value.failed_response_block_count),
            incomplete_response_warn_count: positive_count(value.incomplete_response_warn_count),
            incomplete_response_block_count: positive_count(value.incomplete_response_block_count),
            unknown_response_warn_count: positive_count(value.unknown_response_warn_count),
            unknown_response_block_count: positive_count(value.unknown_response_block_count),
            accounting_anomaly_warn_count: positive_count(value.accounting_anomaly_warn_count),
            accounting_anomaly_block_count: positive_count(value.accounting_anomaly_block_count),
            model_mismatch_warn: value.model_mismatch_warn.unwrap_or(false),
            model_mismatch_block: value.model_mismatch_block.unwrap_or(false),
        }
    }
}

pub fn load_guard_policy_from_path(path: &Path) -> GuardPolicyLoad {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) => {
            return load_failure(path, err);
        }
    };
    match toml::from_str::<GuardPolicyFile>(&raw) {
        Ok(policy) => GuardPolicyLoad {
            policy: policy.into(),
            issues: Vec::new(),
        },
        Err(err) => load_failure(path, err),
    }
}

pub fn load_guard_policy_from_env<F>(get_env: F) -> GuardPolicyLoad
where
    F: Fn(&str) -> Option<String>,
{
    if let Some(path) =
        get_env("CODEX_BLACKBOX_GUARD_POLICY_FILE").filter(|value| !value.trim().is_empty())
    {
        return load_guard_policy_from_path(Path::new(&path));
    }

    GuardPolicyLoad {
        policy: GuardPolicy {
            session_token_budget: parse_u64_env(&get_env, "CODEX_BLACKBOX_SESSION_BUDGET_TOKENS"),
            session_cost_budget_dollars: parse_f64_env(
                &get_env,
                "CODEX_BLACKBOX_SESSION_BUDGET_DOLLARS",
            ),
            context_warn_percent: parse_percent_env(
                &get_env,
                "CODEX_BLACKBOX_CONTEXT_WARN_PERCENT",
            ),
            context_block_percent: parse_percent_env(
                &get_env,
                "CODEX_BLACKBOX_CONTEXT_BLOCK_PERCENT",
            ),
            failed_response_warn_count: parse_u32_env(
                &get_env,
                "CODEX_BLACKBOX_FAILED_RESPONSE_WARN_COUNT",
            ),
            failed_response_block_count: parse_u32_env(
                &get_env,
                "CODEX_BLACKBOX_FAILED_RESPONSE_BLOCK_COUNT",
            ),
            incomplete_response_warn_count: parse_u32_env(
                &get_env,
                "CODEX_BLACKBOX_INCOMPLETE_RESPONSE_WARN_COUNT",
            ),
            incomplete_response_block_count: parse_u32_env(
                &get_env,
                "CODEX_BLACKBOX_INCOMPLETE_RESPONSE_BLOCK_COUNT",
            ),
            unknown_response_warn_count: parse_u32_env(
                &get_env,
                "CODEX_BLACKBOX_UNKNOWN_RESPONSE_WARN_COUNT",
            ),
            unknown_response_block_count: parse_u32_env(
                &get_env,
                "CODEX_BLACKBOX_UNKNOWN_RESPONSE_BLOCK_COUNT",
            ),
            accounting_anomaly_warn_count: parse_u32_env(
                &get_env,
                "CODEX_BLACKBOX_ACCOUNTING_ANOMALY_WARN_COUNT",
            ),
            accounting_anomaly_block_count: parse_u32_env(
                &get_env,
                "CODEX_BLACKBOX_ACCOUNTING_ANOMALY_BLOCK_COUNT",
            ),
            model_mismatch_warn: parse_bool_env(&get_env, "CODEX_BLACKBOX_MODEL_MISMATCH_WARN"),
            model_mismatch_block: parse_bool_env(&get_env, "CODEX_BLACKBOX_MODEL_MISMATCH_BLOCK"),
        },
        issues: Vec::new(),
    }
}

pub fn evaluate_guard_policy(policy: &GuardPolicy, evidence: &GuardEvidence) -> GuardEvaluation {
    let mut evaluation = GuardEvaluation::default();

    if evidence.applies_to_next_request {
        if let Some(cooldown) = evidence.cooldown.as_ref() {
            evaluation.cooldown = Some(CooldownFacts {
                reason: nonempty_or(&cooldown.reason, "upstream errors"),
                retry_after_seconds: cooldown.retry_after_seconds,
            });
            return evaluation;
        }
    }

    let trusted = trusted_session_evidence(evidence);

    if let Some(limit) = policy.session_token_budget.filter(|limit| *limit > 0) {
        if trusted {
            if let Some(current) = evidence
                .session_total_tokens
                .filter(|current| *current >= limit)
            {
                set_block_if_none(
                    &mut evaluation,
                    PolicyBlockFacts {
                        rule: "session_token_budget".to_string(),
                        reason: "token budget exceeded".to_string(),
                        current: Some(format!("{current} tokens")),
                        limit: Some(format!("{limit} tokens")),
                        session_id: evidence.session_id.clone(),
                        recovery_action: "restart narrower".to_string(),
                    },
                );
            }
        }
    }

    if let Some(limit) = policy
        .session_cost_budget_dollars
        .filter(|limit| *limit > 0.0)
    {
        if !evidence.local_estimate_trusted_for_budget_enforcement {
            evaluation.policy_issues.push(GuardPolicyIssue {
                issue_type: "untrusted_pricing".to_string(),
                message:
                    "local cost estimate is untrusted for budget enforcement; dollar budget is advisory"
                        .to_string(),
                recovery_action: "use token budget or configure trusted pricing".to_string(),
            });
        } else if trusted {
            if let Some(current) = evidence
                .session_estimated_cost_dollars
                .filter(|current| *current >= limit)
            {
                set_block_if_none(
                    &mut evaluation,
                    PolicyBlockFacts {
                        rule: "session_cost_budget".to_string(),
                        reason: "cost budget exceeded".to_string(),
                        current: Some(format!("${current:.2}")),
                        limit: Some(format!("${limit:.2}")),
                        session_id: evidence.session_id.clone(),
                        recovery_action: "restart narrower".to_string(),
                    },
                );
            }
        }
    }

    if trusted {
        evaluate_percent_rule(
            &mut evaluation,
            "context_warn_percent",
            "context_block_percent",
            "context fill",
            evidence.max_context_fill_percent,
            policy.context_warn_percent,
            policy.context_block_percent,
            evidence.session_id.clone(),
            "narrow next prompt",
            "context threshold exceeded",
        );
        evaluate_count_rule(
            &mut evaluation,
            "failed_response_warn_count",
            "failed_response_block_count",
            "failed responses",
            evidence.failed_responses,
            policy.failed_response_warn_count,
            policy.failed_response_block_count,
            evidence.session_id.clone(),
            "inspect postmortem before continuing",
            "failed response threshold exceeded",
        );
        evaluate_count_rule(
            &mut evaluation,
            "incomplete_response_warn_count",
            "incomplete_response_block_count",
            "incomplete responses",
            evidence.incomplete_responses,
            policy.incomplete_response_warn_count,
            policy.incomplete_response_block_count,
            evidence.session_id.clone(),
            "continue with a narrower prompt or raise the output limit",
            "incomplete response threshold exceeded",
        );
        evaluate_count_rule(
            &mut evaluation,
            "unknown_response_warn_count",
            "unknown_response_block_count",
            "unknown responses",
            evidence.unknown_responses,
            policy.unknown_response_warn_count,
            policy.unknown_response_block_count,
            evidence.session_id.clone(),
            "inspect postmortem before continuing",
            "unknown response threshold exceeded",
        );
        evaluate_count_rule(
            &mut evaluation,
            "accounting_anomaly_warn_count",
            "accounting_anomaly_block_count",
            "accounting anomalies",
            evidence.accounting_anomalies,
            policy.accounting_anomaly_warn_count,
            policy.accounting_anomaly_block_count,
            evidence.session_id.clone(),
            "inspect accounting anomalies before continuing",
            "accounting anomaly threshold exceeded",
        );
        evaluate_bool_rule(
            &mut evaluation,
            "model_mismatch_warn",
            "model_mismatch_block",
            evidence.model_mismatch,
            policy.model_mismatch_warn,
            policy.model_mismatch_block,
            evidence.session_id.clone(),
            "confirm which model answered before continuing",
            "model mismatch observed",
        );
    }

    evaluation
}

fn trusted_session_evidence(evidence: &GuardEvidence) -> bool {
    evidence.applies_to_next_request
        && evidence.observed_codex_responses
        && evidence
            .session_id
            .as_ref()
            .is_some_and(|id| !id.trim().is_empty())
}

fn parse_u64_env<F>(get_env: &F, key: &str) -> Option<u64>
where
    F: Fn(&str) -> Option<String>,
{
    get_env(key)
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
}

fn parse_u32_env<F>(get_env: &F, key: &str) -> Option<u32>
where
    F: Fn(&str) -> Option<String>,
{
    get_env(key)
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|value| *value > 0)
}

fn parse_f64_env<F>(get_env: &F, key: &str) -> Option<f64>
where
    F: Fn(&str) -> Option<String>,
{
    get_env(key)
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| *value > 0.0)
}

fn parse_percent_env<F>(get_env: &F, key: &str) -> Option<f64>
where
    F: Fn(&str) -> Option<String>,
{
    positive_percent(get_env(key).and_then(|value| value.parse::<f64>().ok()))
}

fn parse_bool_env<F>(get_env: &F, key: &str) -> bool
where
    F: Fn(&str) -> Option<String>,
{
    get_env(key)
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn positive_count(value: Option<u32>) -> Option<u32> {
    value.filter(|value| *value > 0)
}

fn positive_percent(value: Option<f64>) -> Option<f64> {
    value.filter(|value| value.is_finite() && *value > 0.0)
}

fn set_block_if_none(evaluation: &mut GuardEvaluation, block: PolicyBlockFacts) {
    if evaluation.block.is_none() {
        evaluation.block = Some(block);
    }
}

#[allow(clippy::too_many_arguments)]
fn push_policy_issue(
    evaluation: &mut GuardEvaluation,
    issue_type: &str,
    metric: &str,
    current: String,
    limit: String,
    recovery_action: &str,
) {
    evaluation.policy_issues.push(GuardPolicyIssue {
        issue_type: issue_type.to_string(),
        message: format!("{metric} {current} reached {limit}"),
        recovery_action: recovery_action.to_string(),
    });
}

#[allow(clippy::too_many_arguments)]
fn evaluate_percent_rule(
    evaluation: &mut GuardEvaluation,
    warn_rule: &str,
    block_rule: &str,
    metric: &str,
    current: Option<f64>,
    warn_limit: Option<f64>,
    block_limit: Option<f64>,
    session_id: Option<String>,
    recovery_action: &str,
    block_reason: &str,
) {
    let Some(current) = current else {
        return;
    };
    if let Some(limit) = warn_limit.filter(|limit| current >= *limit) {
        push_policy_issue(
            evaluation,
            warn_rule,
            metric,
            format!("{current:.1}%"),
            format!("{limit:.1}%"),
            recovery_action,
        );
    }
    if let Some(limit) = block_limit.filter(|limit| current >= *limit) {
        set_block_if_none(
            evaluation,
            PolicyBlockFacts {
                rule: block_rule.to_string(),
                reason: block_reason.to_string(),
                current: Some(format!("{current:.1}%")),
                limit: Some(format!("{limit:.1}%")),
                session_id,
                recovery_action: recovery_action.to_string(),
            },
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn evaluate_count_rule(
    evaluation: &mut GuardEvaluation,
    warn_rule: &str,
    block_rule: &str,
    metric: &str,
    current: u32,
    warn_limit: Option<u32>,
    block_limit: Option<u32>,
    session_id: Option<String>,
    recovery_action: &str,
    block_reason: &str,
) {
    if let Some(limit) = warn_limit.filter(|limit| current >= *limit) {
        push_policy_issue(
            evaluation,
            warn_rule,
            metric,
            current.to_string(),
            limit.to_string(),
            recovery_action,
        );
    }
    if let Some(limit) = block_limit.filter(|limit| current >= *limit) {
        set_block_if_none(
            evaluation,
            PolicyBlockFacts {
                rule: block_rule.to_string(),
                reason: block_reason.to_string(),
                current: Some(current.to_string()),
                limit: Some(limit.to_string()),
                session_id,
                recovery_action: recovery_action.to_string(),
            },
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn evaluate_bool_rule(
    evaluation: &mut GuardEvaluation,
    warn_rule: &str,
    block_rule: &str,
    current: bool,
    warn_enabled: bool,
    block_enabled: bool,
    session_id: Option<String>,
    recovery_action: &str,
    block_reason: &str,
) {
    if !current {
        return;
    }
    if warn_enabled {
        push_policy_issue(
            evaluation,
            warn_rule,
            "model mismatch",
            "observed".to_string(),
            "false".to_string(),
            recovery_action,
        );
    }
    if block_enabled {
        set_block_if_none(
            evaluation,
            PolicyBlockFacts {
                rule: block_rule.to_string(),
                reason: block_reason.to_string(),
                current: Some("true".to_string()),
                limit: Some("false".to_string()),
                session_id,
                recovery_action: recovery_action.to_string(),
            },
        );
    }
}

fn load_failure(path: &Path, err: impl std::fmt::Display) -> GuardPolicyLoad {
    GuardPolicyLoad {
        policy: GuardPolicy::default(),
        issues: vec![GuardPolicyIssue {
            issue_type: "policy_load_failed".to_string(),
            message: format!("failed to load guard policy {}: {err}", path.display()),
            recovery_action: "fix policy or continue unguarded".to_string(),
        }],
    }
}

fn nonempty_or(value: &str, fallback: &str) -> String {
    if value.trim().is_empty() {
        fallback.to_string()
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        evaluate_guard_policy, load_guard_policy_from_env, load_guard_policy_from_path,
        GuardCooldownEvidence, GuardEvidence, GuardPolicy,
    };
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_test_dir(label: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time went backwards")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "codex-blackbox-guard-{label}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create test dir");
        path
    }

    fn trusted_next_request_evidence() -> GuardEvidence {
        GuardEvidence {
            session_id: Some("session_guard".to_string()),
            observed_codex_responses: true,
            applies_to_next_request: true,
            session_total_tokens: Some(125_000),
            session_estimated_cost_dollars: Some(5.25),
            local_estimate_trusted_for_budget_enforcement: true,
            cooldown: None,
            ..Default::default()
        }
    }

    #[test]
    fn policy_load_failure_fails_open_and_reports_issue() {
        let dir = unique_test_dir("invalid-policy");
        let path = dir.join("policy.toml");
        fs::write(&path, "session_token_budget = not-a-number").expect("write invalid policy");

        let loaded = load_guard_policy_from_path(&path);
        let evaluation = evaluate_guard_policy(&loaded.policy, &trusted_next_request_evidence());

        assert!(loaded.policy.session_token_budget.is_none());
        assert!(evaluation.block.is_none(), "load failure must fail open");
        assert_eq!(loaded.issues.len(), 1);
        assert_eq!(loaded.issues[0].issue_type, "policy_load_failed");
        assert!(loaded.issues[0].message.contains("policy.toml"));
    }

    #[test]
    fn policy_file_and_env_parse_codex_native_rules() {
        let dir = unique_test_dir("native-policy");
        let path = dir.join("policy.toml");
        fs::write(
            &path,
            "\
session_token_budget = 200000
session_cost_budget_dollars = 10.0
context_warn_percent = 70
context_block_percent = 85
failed_response_warn_count = 1
failed_response_block_count = 2
incomplete_response_warn_count = 1
incomplete_response_block_count = 2
unknown_response_warn_count = 1
unknown_response_block_count = 2
accounting_anomaly_warn_count = 1
accounting_anomaly_block_count = 2
model_mismatch_warn = true
model_mismatch_block = true
",
        )
        .expect("write policy");

        let loaded = load_guard_policy_from_path(&path);
        assert!(loaded.issues.is_empty());
        assert_eq!(loaded.policy.context_warn_percent, Some(70.0));
        assert_eq!(loaded.policy.context_block_percent, Some(85.0));
        assert_eq!(loaded.policy.failed_response_warn_count, Some(1));
        assert_eq!(loaded.policy.failed_response_block_count, Some(2));
        assert_eq!(loaded.policy.incomplete_response_warn_count, Some(1));
        assert_eq!(loaded.policy.incomplete_response_block_count, Some(2));
        assert_eq!(loaded.policy.unknown_response_warn_count, Some(1));
        assert_eq!(loaded.policy.unknown_response_block_count, Some(2));
        assert_eq!(loaded.policy.accounting_anomaly_warn_count, Some(1));
        assert_eq!(loaded.policy.accounting_anomaly_block_count, Some(2));
        assert!(loaded.policy.model_mismatch_warn);
        assert!(loaded.policy.model_mismatch_block);

        let env = |key: &str| match key {
            "CODEX_BLACKBOX_CONTEXT_WARN_PERCENT" => Some("71".to_string()),
            "CODEX_BLACKBOX_CONTEXT_BLOCK_PERCENT" => Some("86".to_string()),
            "CODEX_BLACKBOX_FAILED_RESPONSE_WARN_COUNT" => Some("1".to_string()),
            "CODEX_BLACKBOX_FAILED_RESPONSE_BLOCK_COUNT" => Some("3".to_string()),
            "CODEX_BLACKBOX_INCOMPLETE_RESPONSE_WARN_COUNT" => Some("1".to_string()),
            "CODEX_BLACKBOX_INCOMPLETE_RESPONSE_BLOCK_COUNT" => Some("3".to_string()),
            "CODEX_BLACKBOX_UNKNOWN_RESPONSE_WARN_COUNT" => Some("1".to_string()),
            "CODEX_BLACKBOX_UNKNOWN_RESPONSE_BLOCK_COUNT" => Some("3".to_string()),
            "CODEX_BLACKBOX_ACCOUNTING_ANOMALY_WARN_COUNT" => Some("1".to_string()),
            "CODEX_BLACKBOX_ACCOUNTING_ANOMALY_BLOCK_COUNT" => Some("3".to_string()),
            "CODEX_BLACKBOX_MODEL_MISMATCH_WARN" => Some("true".to_string()),
            "CODEX_BLACKBOX_MODEL_MISMATCH_BLOCK" => Some("1".to_string()),
            _ => None,
        };
        let loaded = load_guard_policy_from_env(env);
        assert_eq!(loaded.policy.context_warn_percent, Some(71.0));
        assert_eq!(loaded.policy.context_block_percent, Some(86.0));
        assert_eq!(loaded.policy.failed_response_block_count, Some(3));
        assert!(loaded.policy.model_mismatch_warn);
        assert!(loaded.policy.model_mismatch_block);
    }

    #[test]
    fn token_policy_blocks_only_trusted_explicit_next_request_evidence() {
        let policy = GuardPolicy {
            session_token_budget: Some(120_000),
            session_cost_budget_dollars: None,
            ..Default::default()
        };

        let blocked = evaluate_guard_policy(&policy, &trusted_next_request_evidence())
            .block
            .expect("trusted token evidence should block");
        assert_eq!(blocked.rule, "session_token_budget");
        assert_eq!(blocked.reason, "token budget exceeded");
        assert_eq!(blocked.current.as_deref(), Some("125000 tokens"));
        assert_eq!(blocked.limit.as_deref(), Some("120000 tokens"));
        assert_eq!(blocked.session_id.as_deref(), Some("session_guard"));
        assert_eq!(blocked.recovery_action, "restart narrower");

        let mut streaming = trusted_next_request_evidence();
        streaming.applies_to_next_request = false;
        assert!(
            evaluate_guard_policy(&policy, &streaming).block.is_none(),
            "policy must not claim it can interrupt an already-streaming response"
        );

        let mut unobserved = trusted_next_request_evidence();
        unobserved.observed_codex_responses = false;
        assert!(
            evaluate_guard_policy(&policy, &unobserved).block.is_none(),
            "policy must not block without trusted Envoy-observed Codex Responses evidence"
        );
    }

    #[test]
    fn cost_policy_does_not_block_when_pricing_is_untrusted() {
        let policy = GuardPolicy {
            session_token_budget: None,
            session_cost_budget_dollars: Some(5.00),
            ..Default::default()
        };
        let mut evidence = trusted_next_request_evidence();
        evidence.local_estimate_trusted_for_budget_enforcement = false;

        let evaluation = evaluate_guard_policy(&policy, &evidence);

        assert!(
            evaluation.block.is_none(),
            "unknown or untrusted pricing cannot enforce dollar budgets"
        );
        assert!(
            evaluation
                .policy_issues
                .iter()
                .any(|issue| issue.issue_type == "untrusted_pricing"),
            "untrusted pricing should remain explicit"
        );
    }

    #[test]
    fn untrusted_dollar_pricing_does_not_mask_later_context_block() {
        let policy = GuardPolicy {
            session_cost_budget_dollars: Some(1.00),
            context_block_percent: Some(85.0),
            ..Default::default()
        };
        let mut evidence = trusted_next_request_evidence();
        evidence.local_estimate_trusted_for_budget_enforcement = false;
        evidence.max_context_fill_percent = Some(90.0);

        let evaluation = evaluate_guard_policy(&policy, &evidence);
        let block = evaluation.block.expect("context block should still apply");

        assert_eq!(block.rule, "context_block_percent");
        assert_eq!(block.reason, "context threshold exceeded");
        assert_eq!(block.current.as_deref(), Some("90.0%"));
        assert_eq!(block.limit.as_deref(), Some("85.0%"));
        assert!(evaluation
            .policy_issues
            .iter()
            .any(|issue| issue.issue_type == "untrusted_pricing"));
    }

    #[test]
    fn context_warn_and_block_rules_use_structured_facts() {
        let warn_policy = GuardPolicy {
            context_warn_percent: Some(70.0),
            ..Default::default()
        };
        let mut evidence = trusted_next_request_evidence();
        evidence.max_context_fill_percent = Some(72.0);

        let warning = evaluate_guard_policy(&warn_policy, &evidence);
        assert!(warning.block.is_none());
        assert!(warning
            .policy_issues
            .iter()
            .any(|issue| issue.issue_type == "context_warn_percent"));

        let block_policy = GuardPolicy {
            context_block_percent: Some(85.0),
            ..Default::default()
        };
        evidence.max_context_fill_percent = Some(90.0);
        let block = evaluate_guard_policy(&block_policy, &evidence)
            .block
            .expect("context should block");
        assert_eq!(block.rule, "context_block_percent");
        assert_eq!(block.current.as_deref(), Some("90.0%"));
        assert_eq!(block.limit.as_deref(), Some("85.0%"));
        assert_eq!(block.session_id.as_deref(), Some("session_guard"));
        assert_eq!(block.recovery_action, "narrow next prompt");
    }

    #[test]
    fn response_status_and_accounting_rules_block_with_structured_facts() {
        let cases: [(GuardPolicy, fn(&mut GuardEvidence), &str, &str); 4] = [
            (
                GuardPolicy {
                    failed_response_block_count: Some(1),
                    ..Default::default()
                },
                |evidence: &mut GuardEvidence| evidence.failed_responses = 1,
                "failed_response_block_count",
                "failed response threshold exceeded",
            ),
            (
                GuardPolicy {
                    incomplete_response_block_count: Some(1),
                    ..Default::default()
                },
                |evidence: &mut GuardEvidence| evidence.incomplete_responses = 1,
                "incomplete_response_block_count",
                "incomplete response threshold exceeded",
            ),
            (
                GuardPolicy {
                    unknown_response_block_count: Some(1),
                    ..Default::default()
                },
                |evidence: &mut GuardEvidence| evidence.unknown_responses = 1,
                "unknown_response_block_count",
                "unknown response threshold exceeded",
            ),
            (
                GuardPolicy {
                    accounting_anomaly_block_count: Some(1),
                    ..Default::default()
                },
                |evidence: &mut GuardEvidence| evidence.accounting_anomalies = 1,
                "accounting_anomaly_block_count",
                "accounting anomaly threshold exceeded",
            ),
        ];

        for (policy, mutate, rule, reason) in cases {
            let mut evidence = trusted_next_request_evidence();
            mutate(&mut evidence);
            let block = evaluate_guard_policy(&policy, &evidence)
                .block
                .expect("native count rule should block");
            assert_eq!(block.rule, rule);
            assert_eq!(block.reason, reason);
            assert_eq!(block.current.as_deref(), Some("1"));
            assert_eq!(block.limit.as_deref(), Some("1"));
            assert_eq!(block.session_id.as_deref(), Some("session_guard"));
        }
    }

    #[test]
    fn model_mismatch_warns_or_blocks_when_configured() {
        let mut evidence = trusted_next_request_evidence();
        evidence.model_mismatch = true;

        let warning = evaluate_guard_policy(
            &GuardPolicy {
                model_mismatch_warn: true,
                ..Default::default()
            },
            &evidence,
        );
        assert!(warning.block.is_none());
        assert!(warning
            .policy_issues
            .iter()
            .any(|issue| issue.issue_type == "model_mismatch_warn"));

        let block = evaluate_guard_policy(
            &GuardPolicy {
                model_mismatch_block: true,
                ..Default::default()
            },
            &evidence,
        )
        .block
        .expect("model mismatch should block");
        assert_eq!(block.rule, "model_mismatch_block");
        assert_eq!(block.current.as_deref(), Some("true"));
        assert_eq!(block.limit.as_deref(), Some("false"));
    }

    #[test]
    fn native_policy_rules_do_not_block_without_trusted_codex_evidence() {
        let policy = GuardPolicy {
            context_block_percent: Some(85.0),
            failed_response_block_count: Some(1),
            model_mismatch_block: true,
            ..Default::default()
        };
        let mut evidence = trusted_next_request_evidence();
        evidence.observed_codex_responses = false;
        evidence.max_context_fill_percent = Some(90.0);
        evidence.failed_responses = 1;
        evidence.model_mismatch = true;

        assert!(evaluate_guard_policy(&policy, &evidence).block.is_none());
    }

    #[test]
    fn cooldown_blocks_next_request_without_session_budget_evidence() {
        let evaluation = evaluate_guard_policy(
            &GuardPolicy::default(),
            &GuardEvidence {
                applies_to_next_request: true,
                cooldown: Some(GuardCooldownEvidence {
                    reason: "upstream errors".to_string(),
                    retry_after_seconds: Some(30),
                }),
                ..Default::default()
            },
        );

        let cooldown = evaluation
            .cooldown
            .expect("cooldown should block next request");
        assert_eq!(cooldown.reason, "upstream errors");
        assert_eq!(cooldown.retry_after_seconds, Some(30));
        assert!(evaluation.block.is_none());
    }
}
