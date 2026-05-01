use std::fmt;

use serde_json::Value;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CodexRequestHeaders {
    pub session_id: Option<String>,
    pub client_request_id: Option<String>,
}

impl CodexRequestHeaders {
    pub fn from_pairs<I, K, V>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let mut headers = Self::default();
        for (name, value) in pairs {
            let Some(value) = non_empty_string(value.as_ref()) else {
                continue;
            };
            match name.as_ref().to_ascii_lowercase().as_str() {
                "session_id" | "session-id" => headers.session_id = Some(value),
                "x-client-request-id" => headers.client_request_id = Some(value),
                _ => {}
            }
        }
        headers
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedCodexRequest {
    pub model: String,
    pub instructions_length: usize,
    pub input_count: usize,
    pub first_user_input: Option<String>,
    pub tools_count: usize,
    pub has_reasoning: bool,
    pub reasoning_effort: Option<String>,
    pub prompt_cache_key: Option<String>,
    pub metadata: Option<Value>,
    pub client_metadata: Option<Value>,
    pub cwd: Option<String>,
    pub session: CodexSessionIdentity,
}

impl ParsedCodexRequest {
    pub fn has_tools(&self) -> bool {
        self.tools_count > 0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexSessionIdentity {
    pub id: String,
    pub source: CodexSessionIdentitySource,
    pub fallback_hash: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodexSessionIdentitySource {
    RequestSessionHeader,
    ClientRequestIdHeader,
    ClientMetadata,
    FallbackHash,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodexRequestParseError {
    InvalidJson,
    MissingModel,
    MissingInput,
}

impl fmt::Display for CodexRequestParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidJson => write!(f, "invalid JSON request body"),
            Self::MissingModel => write!(f, "missing model field"),
            Self::MissingInput => write!(f, "missing input field"),
        }
    }
}

impl std::error::Error for CodexRequestParseError {}

pub fn parse_codex_responses_request(
    body: &[u8],
    headers: CodexRequestHeaders,
) -> Result<ParsedCodexRequest, CodexRequestParseError> {
    let value: Value =
        serde_json::from_slice(body).map_err(|_| CodexRequestParseError::InvalidJson)?;
    let model = value
        .get("model")
        .and_then(Value::as_str)
        .and_then(non_empty_string)
        .ok_or(CodexRequestParseError::MissingModel)?;

    let instructions_length = value
        .get("instructions")
        .map(text_length_from_value)
        .unwrap_or(0);
    let input = value
        .get("input")
        .ok_or(CodexRequestParseError::MissingInput)?;
    let input_count = count_input_items(input);
    let first_user_input = first_user_visible_input(input);
    let tools_count = value
        .get("tools")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    let reasoning = value.get("reasoning");
    let has_reasoning = reasoning.is_some();
    let reasoning_effort = reasoning
        .and_then(|reasoning| reasoning.get("effort"))
        .and_then(Value::as_str)
        .and_then(non_empty_string);
    let prompt_cache_key = value
        .get("prompt_cache_key")
        .and_then(Value::as_str)
        .and_then(non_empty_string);
    let metadata = value.get("metadata").cloned();
    let client_metadata = value.get("client_metadata").cloned();
    let cwd = cwd_from_metadata(metadata.as_ref())
        .or_else(|| cwd_from_metadata(client_metadata.as_ref()))
        .or_else(|| cwd_from_input_preamble(input));
    let session = resolve_session_identity(
        &headers,
        client_metadata.as_ref(),
        cwd.as_deref(),
        first_user_input.as_deref(),
    );

    Ok(ParsedCodexRequest {
        model,
        instructions_length,
        input_count,
        first_user_input,
        tools_count,
        has_reasoning,
        reasoning_effort,
        prompt_cache_key,
        metadata,
        client_metadata,
        cwd,
        session,
    })
}

pub fn resolve_session_identity(
    headers: &CodexRequestHeaders,
    client_metadata: Option<&Value>,
    cwd: Option<&str>,
    first_user_input: Option<&str>,
) -> CodexSessionIdentity {
    if let Some(id) = headers.session_id.as_deref().and_then(non_empty_string) {
        return CodexSessionIdentity {
            id,
            source: CodexSessionIdentitySource::RequestSessionHeader,
            fallback_hash: None,
        };
    }

    if let Some(id) = headers
        .client_request_id
        .as_deref()
        .and_then(non_empty_string)
    {
        return CodexSessionIdentity {
            id,
            source: CodexSessionIdentitySource::ClientRequestIdHeader,
            fallback_hash: None,
        };
    }

    if let Some(id) = sessionish_id_from_client_metadata(client_metadata) {
        return CodexSessionIdentity {
            id,
            source: CodexSessionIdentitySource::ClientMetadata,
            fallback_hash: None,
        };
    }

    let hash = fallback_session_hash(cwd.unwrap_or(""), first_user_input.unwrap_or(""));
    CodexSessionIdentity {
        id: format!("codex_fallback_{hash:016x}"),
        source: CodexSessionIdentitySource::FallbackHash,
        fallback_hash: Some(hash),
    }
}

pub fn fallback_session_hash(cwd: &str, first_user_input: &str) -> u64 {
    // Stable FNV-1a so fallback identities do not depend on Rust's hasher.
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;

    let mut hash = FNV_OFFSET;
    for byte in cwd
        .as_bytes()
        .iter()
        .copied()
        .chain([0xff])
        .chain(first_user_input.as_bytes().iter().copied())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

fn count_input_items(input: &Value) -> usize {
    match input {
        Value::Array(items) => items.len(),
        Value::Null => 0,
        _ => 1,
    }
}

fn first_user_visible_input(input: &Value) -> Option<String> {
    let mut user_texts = Vec::new();
    collect_visible_input_texts(input, true, &mut user_texts);
    select_user_prompt_candidate(&user_texts).or_else(|| {
        let mut fallback_texts = Vec::new();
        collect_visible_input_texts(input, false, &mut fallback_texts);
        select_user_prompt_candidate(&fallback_texts)
    })
}

fn collect_visible_input_texts(input: &Value, require_user_role: bool, out: &mut Vec<String>) {
    match input {
        Value::String(text) => {
            if let Some(text) = non_empty_string(text) {
                out.push(text);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_visible_input_texts(item, require_user_role, out);
            }
        }
        Value::Object(object) => {
            let role = object.get("role").and_then(Value::as_str);
            let is_message_like = object.contains_key("role") || object.contains_key("content");
            let is_text_part = object.get("type").and_then(Value::as_str) == Some("input_text");
            if require_user_role && is_message_like && role != Some("user") {
                return;
            }
            if require_user_role && !is_message_like && !is_text_part {
                return;
            }
            collect_text_fragments(input, out);
        }
        _ => {}
    }
}

fn collect_text_fragments(input: &Value, out: &mut Vec<String>) {
    match input {
        Value::String(text) => {
            if let Some(text) = non_empty_string(text) {
                out.push(text);
            }
        }
        Value::Array(parts) => {
            for part in parts {
                collect_text_fragments(part, out);
            }
        }
        Value::Object(object) => {
            if object.get("type").and_then(Value::as_str) == Some("input_text") {
                if let Some(text) = object
                    .get("text")
                    .and_then(Value::as_str)
                    .and_then(non_empty_string)
                {
                    out.push(text);
                    return;
                }
            }
            if let Some(content) = object.get("content") {
                collect_text_fragments(content, out);
            }
            if let Some(text) = object
                .get("text")
                .and_then(Value::as_str)
                .and_then(non_empty_string)
            {
                out.push(text);
            }
        }
        _ => {}
    }
}

fn select_user_prompt_candidate(texts: &[String]) -> Option<String> {
    let mut fallback = None;
    for text in texts.iter().rev() {
        let Some(candidate) = codex_user_prompt_candidate(text) else {
            continue;
        };
        if is_codex_instruction_preamble(&candidate) {
            fallback.get_or_insert(candidate);
            continue;
        }
        return Some(candidate);
    }
    fallback
}

fn codex_user_prompt_candidate(text: &str) -> Option<String> {
    let mut candidate = text.trim();
    for marker in [
        "## My request for Codex:",
        "</environment_context>",
        "</INSTRUCTIONS>",
    ] {
        if let Some(index) = candidate.rfind(marker) {
            let tail = candidate[index + marker.len()..].trim();
            if !tail.is_empty() {
                candidate = tail;
                break;
            }
        }
    }
    non_empty_string(candidate)
}

fn is_codex_instruction_preamble(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with("# AGENTS.md instructions for ")
        || trimmed.starts_with("<environment_context>")
        || trimmed.starts_with("<INSTRUCTIONS>")
        || trimmed.starts_with("Files called AGENTS.md commonly appear")
}

fn text_length_from_value(value: &Value) -> usize {
    match value {
        Value::String(text) => text.len(),
        Value::Array(items) => items.iter().map(text_length_from_value).sum(),
        Value::Object(object) => object
            .get("content")
            .map(text_length_from_value)
            .or_else(|| object.get("text").map(text_length_from_value))
            .unwrap_or(0),
        _ => 0,
    }
}

fn cwd_from_metadata(metadata: Option<&Value>) -> Option<String> {
    let metadata = metadata?;
    for key in ["cwd", "working_dir", "working_directory", "workspace_root"] {
        if let Some(value) = metadata
            .get(key)
            .and_then(Value::as_str)
            .and_then(non_empty_string)
        {
            return Some(value);
        }
    }
    None
}

fn cwd_from_input_preamble(input: &Value) -> Option<String> {
    let mut texts = Vec::new();
    collect_visible_input_texts(input, false, &mut texts);
    texts
        .iter()
        .find_map(|text| cwd_from_agents_preamble(text.as_str()))
}

fn cwd_from_agents_preamble(text: &str) -> Option<String> {
    let marker = "AGENTS.md instructions for ";
    let start = text.find(marker)? + marker.len();
    let path = text[start..]
        .split(|ch: char| ch.is_whitespace() || ch == '<')
        .next()
        .unwrap_or("")
        .trim_matches(|ch| matches!(ch, '"' | '\'' | '`'))
        .to_string();
    non_empty_string(&path)
}

fn sessionish_id_from_client_metadata(client_metadata: Option<&Value>) -> Option<String> {
    let client_metadata = client_metadata?;
    for key in [
        "session_id",
        "conversation_id",
        "codex_session_id",
        "codex_conversation_id",
    ] {
        if let Some(value) = client_metadata
            .get(key)
            .and_then(Value::as_str)
            .and_then(non_empty_string)
        {
            return Some(value);
        }
    }
    None
}

fn non_empty_string(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::{parse_codex_responses_request, CodexRequestHeaders};

    #[test]
    fn request_prompt_prefers_actual_task_after_codex_preamble() {
        let body = br##"{
          "model": "gpt-5.5",
          "input": [
            {
              "type": "message",
              "role": "user",
              "content": [
                {
                  "type": "input_text",
                  "text": "# AGENTS.md instructions for /repo\n<INSTRUCTIONS>Follow local rules.</INSTRUCTIONS><environment_context><cwd>/repo</cwd></environment_context>\nCODITOR marker actual prompt. Do not edit files."
                }
              ]
            }
          ],
          "client_metadata": {"session_id": "session-preamble"}
        }"##;

        let parsed = parse_codex_responses_request(body, CodexRequestHeaders::default())
            .expect("parse request");

        assert_eq!(
            parsed.first_user_input.as_deref(),
            Some("CODITOR marker actual prompt. Do not edit files.")
        );
        assert_eq!(parsed.cwd.as_deref(), Some("/repo"));
    }

    #[test]
    fn request_prompt_prefers_later_user_text_over_agents_text_part() {
        let body = br##"{
          "model": "gpt-5.5",
          "input": [
            {
              "type": "message",
              "role": "user",
              "content": [
                {
                  "type": "input_text",
                  "text": "# AGENTS.md instructions for /repo\n<INSTRUCTIONS>Follow local rules.</INSTRUCTIONS>"
                },
                {
                  "type": "input_text",
                  "text": "CODITOR marker second text part. Do not edit files."
                }
              ]
            }
          ],
          "client_metadata": {"session_id": "session-text-part"}
        }"##;

        let parsed = parse_codex_responses_request(body, CodexRequestHeaders::default())
            .expect("parse request");

        assert_eq!(
            parsed.first_user_input.as_deref(),
            Some("CODITOR marker second text part. Do not edit files.")
        );
        assert_eq!(parsed.cwd.as_deref(), Some("/repo"));
    }
}
