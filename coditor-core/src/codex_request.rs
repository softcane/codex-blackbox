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
        .or_else(|| cwd_from_metadata(client_metadata.as_ref()));
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
    match input {
        Value::String(text) => non_empty_string(text),
        Value::Array(items) => {
            first_text_from_items(items, true).or_else(|| first_text_from_items(items, false))
        }
        Value::Object(_) => text_from_input_item(input),
        _ => None,
    }
}

fn first_text_from_items(items: &[Value], require_user_role: bool) -> Option<String> {
    items.iter().find_map(|item| {
        let role = item.get("role").and_then(Value::as_str);
        if require_user_role && role != Some("user") {
            return None;
        }
        text_from_input_item(item)
    })
}

fn text_from_input_item(item: &Value) -> Option<String> {
    match item {
        Value::String(text) => non_empty_string(text),
        Value::Object(object) => {
            if object.get("type").and_then(Value::as_str) == Some("input_text") {
                if let Some(text) = object.get("text").and_then(Value::as_str) {
                    return non_empty_string(text);
                }
            }
            object
                .get("content")
                .and_then(text_from_content)
                .or_else(|| {
                    object
                        .get("text")
                        .and_then(Value::as_str)
                        .and_then(non_empty_string)
                })
        }
        _ => None,
    }
}

fn text_from_content(content: &Value) -> Option<String> {
    match content {
        Value::String(text) => non_empty_string(text),
        Value::Array(parts) => {
            let joined = parts
                .iter()
                .filter_map(text_from_input_item)
                .collect::<Vec<_>>()
                .join("\n");
            non_empty_string(&joined)
        }
        Value::Object(_) => text_from_input_item(content),
        _ => None,
    }
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
