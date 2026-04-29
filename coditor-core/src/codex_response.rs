use std::collections::BTreeMap;
use std::fmt;

use serde_json::Value;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CodexResponsesAccumulator {
    line_buffer: Vec<u8>,
    summary: CodexResponseSummary,
    tool_calls: BTreeMap<String, CodexToolCallSummary>,
    tool_order: Vec<String>,
}

impl CodexResponsesAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply_headers(&mut self, headers: &CodexResponseHeaders) {
        if let Some(status) = headers.http_status {
            self.summary.http_status = Some(status);
        }
        if let Some(served_model) = headers.served_model.as_ref() {
            self.summary.served_model = Some(served_model.clone());
        }
    }

    pub fn process_chunk(&mut self, chunk: &[u8]) -> Result<(), CodexResponseParseError> {
        self.line_buffer.extend_from_slice(chunk);
        while let Some(pos) = self.line_buffer.iter().position(|&byte| byte == b'\n') {
            let line = String::from_utf8_lossy(&self.line_buffer[..pos]).into_owned();
            self.line_buffer.drain(..=pos);
            self.process_line(line.trim_end_matches('\r'))?;
        }
        Ok(())
    }

    pub fn finish(&mut self) -> Result<(), CodexResponseParseError> {
        if self.line_buffer.is_empty() {
            return Ok(());
        }
        let line = String::from_utf8_lossy(&self.line_buffer).into_owned();
        self.line_buffer.clear();
        self.process_line(line.trim_end_matches('\r'))
    }

    pub fn summary(&self) -> CodexResponseSummary {
        let mut summary = self.summary.clone();
        summary.tool_calls = self
            .tool_order
            .iter()
            .filter_map(|id| self.tool_calls.get(id).cloned())
            .collect();
        summary
    }

    fn process_line(&mut self, line: &str) -> Result<(), CodexResponseParseError> {
        let Some(data) = line.strip_prefix("data:") else {
            return Ok(());
        };
        let data = data.trim_start();
        if data.is_empty() || data == "[DONE]" {
            return Ok(());
        }

        let event: Value = serde_json::from_str(data)
            .map_err(|err| CodexResponseParseError::InvalidJson(err.to_string()))?;
        self.process_event(&event);
        Ok(())
    }

    fn process_event(&mut self, event: &Value) {
        match event.get("type").and_then(Value::as_str) {
            Some("response.created") => {
                if let Some(response) = event.get("response") {
                    self.apply_response_metadata(response);
                }
            }
            Some("response.output_item.added") => {
                if let Some(item) = event.get("item") {
                    self.apply_output_item(item);
                }
            }
            Some("response.output_text.delta") => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    self.summary.output_text.push_str(delta);
                }
            }
            Some("response.custom_tool_call_input.delta") => {
                if let Some(item_id) = event.get("item_id").and_then(Value::as_str) {
                    let delta = event.get("delta").and_then(Value::as_str).unwrap_or("");
                    self.tool_entry(item_id).input.push_str(delta);
                }
            }
            Some("response.output_item.done") => {
                if let Some(item) = event.get("item") {
                    self.apply_output_item(item);
                }
            }
            Some("response.completed") => {
                self.summary.status = CodexResponseStatus::Completed;
                if let Some(response) = event.get("response") {
                    self.apply_response_metadata(response);
                    self.apply_usage(response.get("usage"));
                    self.fill_text_from_response_if_needed(response);
                }
            }
            Some("response.failed") => {
                self.summary.status = CodexResponseStatus::Failed;
                if let Some(response) = event.get("response") {
                    self.apply_response_metadata(response);
                    self.summary.error_message = response
                        .pointer("/error/message")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                }
            }
            Some("response.incomplete") => {
                self.summary.status = CodexResponseStatus::Incomplete;
                if let Some(response) = event.get("response") {
                    self.apply_response_metadata(response);
                    self.apply_usage(response.get("usage"));
                    self.fill_text_from_response_if_needed(response);
                    self.summary.incomplete_reason = response
                        .pointer("/incomplete_details/reason")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                }
            }
            _ => {}
        }
    }

    fn apply_response_metadata(&mut self, response: &Value) {
        if self.summary.response_id.is_none() {
            self.summary.response_id = response
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        if self.summary.served_model.is_none() {
            self.summary.served_model = response
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
    }

    fn apply_output_item(&mut self, item: &Value) {
        let item_type = item.get("type").and_then(Value::as_str);
        if item_type != Some("custom_tool_call") {
            return;
        }
        let Some(id) = item.get("id").and_then(Value::as_str) else {
            return;
        };
        let entry = self.tool_entry(id);
        if let Some(name) = item.get("name").and_then(Value::as_str) {
            entry.name = Some(name.to_string());
        }
        if let Some(input) = item.get("input").and_then(Value::as_str) {
            entry.input = input.to_string();
        }
    }

    fn apply_usage(&mut self, usage: Option<&Value>) {
        let Some(usage) = usage else {
            return;
        };
        let input_tokens = usage
            .get("input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let cached_input_tokens = usage
            .pointer("/input_tokens_details/cached_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let output_tokens = usage
            .get("output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let reasoning_output_tokens = usage
            .pointer("/output_tokens_details/reasoning_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let total_tokens = usage
            .get("total_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);

        self.summary.usage = CodexUsage {
            input_tokens,
            cached_input_tokens,
            uncached_input_tokens: input_tokens.saturating_sub(cached_input_tokens),
            output_tokens,
            reasoning_output_tokens,
            total_tokens,
        };
    }

    fn fill_text_from_response_if_needed(&mut self, response: &Value) {
        if !self.summary.output_text.is_empty() {
            return;
        }
        let text = response
            .get("output")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .flat_map(output_text_parts)
            .collect::<Vec<_>>()
            .join("\n");
        if !text.is_empty() {
            self.summary.output_text = text;
        }
    }

    fn tool_entry(&mut self, id: &str) -> &mut CodexToolCallSummary {
        if !self.tool_calls.contains_key(id) {
            self.tool_order.push(id.to_string());
            self.tool_calls.insert(
                id.to_string(),
                CodexToolCallSummary {
                    id: id.to_string(),
                    name: None,
                    input: String::new(),
                },
            );
        }
        self.tool_calls.get_mut(id).expect("tool entry exists")
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CodexResponseSummary {
    pub response_id: Option<String>,
    pub status: CodexResponseStatus,
    pub http_status: Option<u32>,
    pub output_text: String,
    pub tool_calls: Vec<CodexToolCallSummary>,
    pub usage: CodexUsage,
    pub served_model: Option<String>,
    pub error_message: Option<String>,
    pub incomplete_reason: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum CodexResponseStatus {
    #[default]
    Unknown,
    Completed,
    Failed,
    Incomplete,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CodexToolCallSummary {
    pub id: String,
    pub name: Option<String>,
    pub input: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CodexUsage {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub uncached_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodexResponseParseError {
    InvalidJson(String),
}

impl fmt::Display for CodexResponseParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidJson(err) => write!(f, "invalid Responses SSE JSON: {err}"),
        }
    }
}

impl std::error::Error for CodexResponseParseError {}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CodexResponseHeaders {
    pub http_status: Option<u32>,
    pub served_model: Option<String>,
}

impl CodexResponseHeaders {
    pub fn from_pairs<I, K, V>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let mut openai_model = None;
        let mut x_openai_model = None;
        let mut http_status = None;
        for (name, value) in pairs {
            let value = value.as_ref().trim();
            if value.is_empty() {
                continue;
            }
            match name.as_ref().to_ascii_lowercase().as_str() {
                ":status" | "status" => http_status = value.parse::<u32>().ok(),
                "openai-model" => openai_model = Some(value.to_string()),
                "x-openai-model" => x_openai_model = Some(value.to_string()),
                _ => {}
            }
        }
        Self {
            http_status,
            served_model: openai_model.or(x_openai_model),
        }
    }
}

fn output_text_parts(item: &Value) -> Vec<String> {
    item.get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|part| {
            (part.get("type").and_then(Value::as_str) == Some("output_text"))
                .then(|| part.get("text").and_then(Value::as_str).map(str::to_string))
                .flatten()
        })
        .collect()
}
