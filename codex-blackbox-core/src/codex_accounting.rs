use crate::codex_request::{CodexSessionIdentitySource, ParsedCodexRequest};
use crate::codex_response::{CodexResponseStatus, CodexResponseSummary, CodexToolCallSummary};
use crate::pricing;

const PROMPT_EXCERPT_MAX_CHARS: usize = 320;

#[derive(Clone, Debug, PartialEq)]
pub struct CodexTurnAccounting {
    pub identity: CodexTurnIdentity,
    pub requested_model: String,
    pub served_model: Option<String>,
    pub status: CodexTurnStatus,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub uncached_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
    pub total_tokens: u64,
    pub first_user_prompt_excerpt: Option<String>,
    pub failure_detail: Option<String>,
    pub incomplete_detail: Option<String>,
    pub tool_calls: Vec<CodexToolCallSummary>,
    pub anomalies: Vec<CodexAccountingAnomaly>,
    pub pricing: CodexPricingEstimate,
}

impl CodexTurnAccounting {
    pub fn is_completed(&self) -> bool {
        self.status == CodexTurnStatus::Completed
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexTurnIdentity {
    pub session_id: String,
    pub session_source: CodexSessionIdentitySource,
    pub fallback_hash: Option<u64>,
    pub cwd: Option<String>,
    pub prompt_cache_key: Option<String>,
    pub response_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodexTurnStatus {
    Completed,
    Failed,
    Incomplete,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodexAccountingAnomaly {
    CachedInputExceedsInput {
        input_tokens: u64,
        cached_input_tokens: u64,
    },
    ReportedTotalTokensMismatch {
        reported_total_tokens: u64,
        local_total_tokens: u64,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct CodexPricingEstimate {
    pub status: CodexPricingStatus,
    pub cost_dollars: Option<f64>,
    pub trusted_for_budget_enforcement: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodexPricingStatus {
    EstimatedApiPricing { model: String, cost_source: String },
    UnknownModel { model: String },
}

pub fn summarize_codex_turn(
    request: &ParsedCodexRequest,
    response: &CodexResponseSummary,
) -> CodexTurnAccounting {
    let input_tokens = response.usage.input_tokens;
    let cached_input_tokens = response.usage.cached_input_tokens;
    let uncached_input_tokens = input_tokens.saturating_sub(cached_input_tokens);
    let output_tokens = response.usage.output_tokens;
    let reasoning_output_tokens = response.usage.reasoning_output_tokens;
    let total_tokens = input_tokens.saturating_add(output_tokens);
    let mut anomalies = Vec::new();

    if cached_input_tokens > input_tokens {
        anomalies.push(CodexAccountingAnomaly::CachedInputExceedsInput {
            input_tokens,
            cached_input_tokens,
        });
    }
    if response.usage.total_tokens != 0 && response.usage.total_tokens != total_tokens {
        anomalies.push(CodexAccountingAnomaly::ReportedTotalTokensMismatch {
            reported_total_tokens: response.usage.total_tokens,
            local_total_tokens: total_tokens,
        });
    }

    let served_model = response.served_model.clone();
    let pricing_model = served_model
        .as_deref()
        .unwrap_or(request.model.as_str())
        .to_string();

    CodexTurnAccounting {
        identity: CodexTurnIdentity {
            session_id: request.session.id.clone(),
            session_source: request.session.source.clone(),
            fallback_hash: request.session.fallback_hash,
            cwd: request.cwd.clone(),
            prompt_cache_key: request.prompt_cache_key.clone(),
            response_id: response.response_id.clone(),
        },
        requested_model: request.model.clone(),
        served_model,
        status: CodexTurnStatus::from(&response.status),
        input_tokens,
        cached_input_tokens,
        uncached_input_tokens,
        output_tokens,
        reasoning_output_tokens,
        total_tokens,
        first_user_prompt_excerpt: prompt_excerpt(request.first_user_input.as_deref()),
        failure_detail: response.error_message.clone(),
        incomplete_detail: response.incomplete_reason.clone(),
        tool_calls: response.tool_calls.clone(),
        anomalies,
        pricing: CodexPricingEstimate::estimate_api_pricing(
            &pricing_model,
            input_tokens,
            cached_input_tokens,
            output_tokens,
        ),
    }
}

pub fn estimate_codex_turn_cost(accounting: &CodexTurnAccounting) -> CodexPricingEstimate {
    let model = accounting
        .served_model
        .as_deref()
        .unwrap_or(accounting.requested_model.as_str());
    CodexPricingEstimate::estimate_api_pricing(
        model,
        accounting.input_tokens,
        accounting.cached_input_tokens,
        accounting.output_tokens,
    )
}

impl CodexPricingEstimate {
    fn estimate_api_pricing(model: &str, input: u64, cached_input: u64, output: u64) -> Self {
        let estimate = pricing::estimate_codex_api_cost_dollars(model, input, cached_input, output);
        if pricing::is_unpriced_cost_source(&estimate.cost_source) {
            return Self::unknown_model(model);
        }

        Self {
            status: CodexPricingStatus::EstimatedApiPricing {
                model: model.to_string(),
                cost_source: estimate.cost_source,
            },
            cost_dollars: Some(estimate.total_cost_dollars),
            trusted_for_budget_enforcement: estimate.trusted_for_budget_enforcement,
        }
    }

    fn unknown_model(model: &str) -> Self {
        Self {
            status: CodexPricingStatus::UnknownModel {
                model: model.to_string(),
            },
            cost_dollars: None,
            trusted_for_budget_enforcement: false,
        }
    }
}

impl From<&CodexResponseStatus> for CodexTurnStatus {
    fn from(status: &CodexResponseStatus) -> Self {
        match status {
            CodexResponseStatus::Completed => Self::Completed,
            CodexResponseStatus::Failed => Self::Failed,
            CodexResponseStatus::Incomplete => Self::Incomplete,
            CodexResponseStatus::Unknown => Self::Unknown,
        }
    }
}

fn prompt_excerpt(input: Option<&str>) -> Option<String> {
    let input = input?;
    let trimmed = input
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.chars().count() <= PROMPT_EXCERPT_MAX_CHARS {
        Some(trimmed)
    } else {
        let mut out: String = trimmed.chars().take(PROMPT_EXCERPT_MAX_CHARS).collect();
        out.push_str("...");
        Some(out)
    }
}
