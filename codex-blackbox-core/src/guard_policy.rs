use std::path::Path;

use serde::Deserialize;
use serde::Serialize;

use crate::decision::{CooldownFacts, PolicyBlockFacts};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct GuardPolicy {
    pub session_token_budget: Option<u64>,
    pub session_cost_budget_dollars: Option<f64>,
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
}

impl From<GuardPolicyFile> for GuardPolicy {
    fn from(value: GuardPolicyFile) -> Self {
        Self {
            session_token_budget: value.session_token_budget.or(value.session_budget_tokens),
            session_cost_budget_dollars: value
                .session_cost_budget_dollars
                .or(value.session_budget_dollars),
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

    if let Some(limit) = policy.session_token_budget.filter(|limit| *limit > 0) {
        if trusted_session_evidence(evidence) {
            if let Some(current) = evidence.session_total_tokens {
                if current >= limit {
                    evaluation.block = Some(PolicyBlockFacts {
                        rule: "session_token_budget".to_string(),
                        reason: "token budget exceeded".to_string(),
                        current: Some(format!("{current} tokens")),
                        limit: Some(format!("{limit} tokens")),
                        session_id: evidence.session_id.clone(),
                        recovery_action: "restart narrower".to_string(),
                    });
                    return evaluation;
                }
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
            return evaluation;
        }

        if trusted_session_evidence(evidence) {
            if let Some(current) = evidence.session_estimated_cost_dollars {
                if current >= limit {
                    evaluation.block = Some(PolicyBlockFacts {
                        rule: "session_cost_budget".to_string(),
                        reason: "cost budget exceeded".to_string(),
                        current: Some(format!("${current:.2}")),
                        limit: Some(format!("${limit:.2}")),
                        session_id: evidence.session_id.clone(),
                        recovery_action: "restart narrower".to_string(),
                    });
                }
            }
        }
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

fn parse_f64_env<F>(get_env: &F, key: &str) -> Option<f64>
where
    F: Fn(&str) -> Option<String>,
{
    get_env(key)
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| *value > 0.0)
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
        evaluate_guard_policy, load_guard_policy_from_path, GuardCooldownEvidence, GuardEvidence,
        GuardPolicy,
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
    fn token_policy_blocks_only_trusted_explicit_next_request_evidence() {
        let policy = GuardPolicy {
            session_token_budget: Some(120_000),
            session_cost_budget_dollars: None,
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
