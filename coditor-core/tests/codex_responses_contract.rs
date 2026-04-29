use coditor_core::codex_accounting::{
    summarize_codex_turn, CodexAccountingAnomaly, CodexPricingStatus, CodexTurnStatus,
};
use coditor_core::codex_request::{
    parse_codex_responses_request, CodexRequestHeaders, CodexRequestParseError,
    CodexSessionIdentitySource,
};
use coditor_core::codex_response::{
    CodexResponseHeaders, CodexResponseStatus, CodexResponseSummary, CodexResponsesAccumulator,
    CodexUsage,
};
use serde_json::Value;

const MINIMAL_TEXT_REQUEST: &str =
    include_str!("../../test/fixtures/openai_responses_minimal_text_request.json");
const TOOL_REQUEST: &str = include_str!("../../test/fixtures/openai_responses_tool_request.json");
const TEXT_STREAM: &str = include_str!("../../test/fixtures/openai_responses_text_stream.sse");
const TOOL_STREAM: &str = include_str!("../../test/fixtures/openai_responses_tool_stream.sse");
const FAILED_STREAM: &str = include_str!("../../test/fixtures/openai_responses_failed_stream.sse");
const INCOMPLETE_STREAM: &str =
    include_str!("../../test/fixtures/openai_responses_incomplete_stream.sse");

#[test]
fn fake_responses_request_fixtures_are_valid_json() {
    let minimal: Value = serde_json::from_str(MINIMAL_TEXT_REQUEST).expect("minimal request JSON");
    assert_eq!(minimal["model"], "gpt-codex-fixture");
    assert_eq!(minimal["stream"], true);
    assert!(minimal.get("input").is_some());
    assert!(minimal.get("prompt_cache_key").is_some());

    let tool: Value = serde_json::from_str(TOOL_REQUEST).expect("tool request JSON");
    assert_eq!(tool["model"], "gpt-codex-fixture");
    assert_eq!(tool["stream"], true);
    assert!(tool["tools"]
        .as_array()
        .is_some_and(|tools| !tools.is_empty()));
}

#[test]
fn fake_responses_sse_fixtures_contain_valid_json_events() {
    for fixture in [TEXT_STREAM, TOOL_STREAM, FAILED_STREAM, INCOMPLETE_STREAM] {
        let events = parse_sse_data_events(fixture);
        assert!(!events.is_empty(), "fixture should contain SSE data events");
        for event in events {
            let json: Value = serde_json::from_str(event).expect("SSE data JSON");
            assert!(
                json.get("type").and_then(Value::as_str).is_some(),
                "SSE data must include a type: {json}"
            );
        }
    }
}

#[test]
fn request_parser_should_extract_codex_request_metadata_from_fixture() {
    let parsed = parse_codex_responses_request(
        MINIMAL_TEXT_REQUEST.as_bytes(),
        CodexRequestHeaders::default(),
    )
    .expect("parse fixture");

    assert_eq!(parsed.model, "gpt-codex-fixture");
    assert!(parsed.instructions_length > 0);
    assert_eq!(parsed.input_count, 1);
    assert_eq!(
        parsed.first_user_input.as_deref(),
        Some("Summarize the current repository status.")
    );
    assert_eq!(parsed.tools_count, 0);
    assert!(!parsed.has_tools());
    assert!(parsed.has_reasoning);
    assert_eq!(parsed.reasoning_effort.as_deref(), Some("medium"));
    assert_eq!(
        parsed.prompt_cache_key.as_deref(),
        Some("coditor-fixture:/Users/pradeepsingh/code/coditor")
    );
    assert!(parsed.metadata.is_some());
    assert!(parsed.client_metadata.is_some());
    assert_eq!(
        parsed.cwd.as_deref(),
        Some("/Users/pradeepsingh/code/coditor")
    );
    assert_eq!(parsed.session.id, "codex-session-fixture-001");
    assert_eq!(
        parsed.session.source,
        CodexSessionIdentitySource::ClientMetadata
    );
}

#[test]
fn request_parser_should_extract_tool_request_shape_from_fixture() {
    let parsed =
        parse_codex_responses_request(TOOL_REQUEST.as_bytes(), CodexRequestHeaders::default())
            .expect("parse fixture");

    assert_eq!(parsed.model, "gpt-codex-fixture");
    assert!(parsed.instructions_length > 0);
    assert_eq!(parsed.input_count, 1);
    assert_eq!(
        parsed.first_user_input.as_deref(),
        Some("Find the package names and report them.")
    );
    assert_eq!(parsed.tools_count, 1);
    assert!(parsed.has_tools());
    assert!(parsed.has_reasoning);
    assert_eq!(parsed.reasoning_effort.as_deref(), Some("low"));
    assert_eq!(
        parsed.prompt_cache_key.as_deref(),
        Some("coditor-fixture:/Users/pradeepsingh/code/coditor:tools")
    );
    assert_eq!(
        parsed.cwd.as_deref(),
        Some("/Users/pradeepsingh/code/coditor")
    );
    assert_eq!(parsed.session.id, "codex-session-fixture-tools-001");
}

#[test]
fn session_identity_prefers_request_session_header_then_client_request_id() {
    let parsed = parse_codex_responses_request(
        MINIMAL_TEXT_REQUEST.as_bytes(),
        CodexRequestHeaders {
            session_id: Some("header-session-001".to_string()),
            client_request_id: Some("client-request-001".to_string()),
        },
    )
    .expect("parse fixture");

    assert_eq!(parsed.session.id, "header-session-001");
    assert_eq!(
        parsed.session.source,
        CodexSessionIdentitySource::RequestSessionHeader
    );

    let parsed = parse_codex_responses_request(
        MINIMAL_TEXT_REQUEST.as_bytes(),
        CodexRequestHeaders {
            session_id: None,
            client_request_id: Some("client-request-001".to_string()),
        },
    )
    .expect("parse fixture");

    assert_eq!(parsed.session.id, "client-request-001");
    assert_eq!(
        parsed.session.source,
        CodexSessionIdentitySource::ClientRequestIdHeader
    );
}

#[test]
fn request_headers_can_be_extracted_from_case_insensitive_pairs() {
    let headers = CodexRequestHeaders::from_pairs([
        ("Session-ID", "header-session-002"),
        ("X-Client-Request-Id", "client-request-002"),
    ]);

    assert_eq!(headers.session_id.as_deref(), Some("header-session-002"));
    assert_eq!(
        headers.client_request_id.as_deref(),
        Some("client-request-002")
    );
}

#[test]
fn parser_uses_client_metadata_conversation_fields_before_fallback() {
    let body = br#"{
      "model": "gpt-codex-fixture",
      "input": "hello from a fixture",
      "client_metadata": {
        "conversation_id": "conversation-fixture-001"
      }
    }"#;

    let parsed =
        parse_codex_responses_request(body, CodexRequestHeaders::default()).expect("parse request");

    assert_eq!(parsed.session.id, "conversation-fixture-001");
    assert_eq!(
        parsed.session.source,
        CodexSessionIdentitySource::ClientMetadata
    );
}

#[test]
fn fallback_session_identity_is_deterministic_and_splits_distinct_inputs() {
    let first = br#"{
      "model": "gpt-codex-fixture",
      "input": "first task",
      "metadata": {
        "cwd": "/tmp/coditor-phase-2a"
      }
    }"#;
    let first_again = br#"{
      "model": "gpt-codex-fixture",
      "input": "first task",
      "metadata": {
        "cwd": "/tmp/coditor-phase-2a"
      }
    }"#;
    let second = br#"{
      "model": "gpt-codex-fixture",
      "input": "second task",
      "metadata": {
        "cwd": "/tmp/coditor-phase-2a"
      }
    }"#;

    let first = parse_codex_responses_request(first, CodexRequestHeaders::default())
        .expect("parse first request");
    let first_again = parse_codex_responses_request(first_again, CodexRequestHeaders::default())
        .expect("parse repeated request");
    let second = parse_codex_responses_request(second, CodexRequestHeaders::default())
        .expect("parse second request");

    assert_eq!(
        first.session.source,
        CodexSessionIdentitySource::FallbackHash
    );
    assert_eq!(first.session.id, first_again.session.id);
    assert_ne!(first.session.id, second.session.id);
}

#[test]
fn parser_accepts_missing_optional_fields() {
    let body = br#"{
      "model": "gpt-codex-fixture",
      "input": "minimal request"
    }"#;

    let parsed =
        parse_codex_responses_request(body, CodexRequestHeaders::default()).expect("parse request");

    assert_eq!(parsed.model, "gpt-codex-fixture");
    assert_eq!(parsed.instructions_length, 0);
    assert_eq!(parsed.input_count, 1);
    assert_eq!(parsed.first_user_input.as_deref(), Some("minimal request"));
    assert_eq!(parsed.tools_count, 0);
    assert!(!parsed.has_reasoning);
    assert!(parsed.reasoning_effort.is_none());
    assert!(parsed.prompt_cache_key.is_none());
    assert!(parsed.cwd.is_none());
    assert_eq!(
        parsed.session.source,
        CodexSessionIdentitySource::FallbackHash
    );
}

#[test]
fn parser_reports_malformed_json_and_missing_model() {
    let malformed = parse_codex_responses_request(
        br#"{"model":"gpt-codex-fixture","#,
        CodexRequestHeaders::default(),
    );
    assert_eq!(malformed, Err(CodexRequestParseError::InvalidJson));

    let missing_model =
        parse_codex_responses_request(br#"{"input":"hello"}"#, CodexRequestHeaders::default());
    assert_eq!(missing_model, Err(CodexRequestParseError::MissingModel));
}

#[test]
fn sse_accumulator_should_produce_text_only_turn_summary_from_fixture() {
    let summary = accumulate_fixture(TEXT_STREAM);

    assert_eq!(
        summary.response_id.as_deref(),
        Some("resp_fixture_text_001")
    );
    assert_eq!(summary.status, CodexResponseStatus::Completed);
    assert_eq!(summary.served_model.as_deref(), Some("gpt-codex-fixture"));
    assert_eq!(
        summary.output_text,
        "Workspace packages: coditor-core and coditor-cli."
    );
    assert!(summary.tool_calls.is_empty());
}

#[test]
fn usage_mapping_should_treat_cached_input_tokens_as_subset_not_additive() {
    let summary = accumulate_fixture(TEXT_STREAM);
    let usage = summary.usage;

    assert_eq!(usage.input_tokens, 1280);
    assert_eq!(usage.cached_input_tokens, 512);
    assert_eq!(usage.uncached_input_tokens, 768);
    assert_eq!(usage.output_tokens, 96);
    assert_eq!(usage.reasoning_output_tokens, 32);
    assert_eq!(usage.total_tokens, usage.input_tokens + usage.output_tokens);
}

#[test]
fn response_header_model_should_feed_model_fallback_detection() {
    let requested_model = "gpt-codex-fixture";
    let response_headers = [
        (":status", "200"),
        ("session_id", "codex-session-fixture-001"),
        ("x-client-request-id", "codex-request-fixture-001"),
        ("openai-model", "gpt-codex-fixture-served"),
        ("x-openai-model", "gpt-codex-fixture-served"),
    ];
    let parsed_headers = CodexResponseHeaders::from_pairs(response_headers);

    assert_eq!(
        parsed_headers.served_model.as_deref(),
        Some("gpt-codex-fixture-served")
    );
    assert_eq!(parsed_headers.http_status, Some(200));
    assert_ne!(
        requested_model,
        parsed_headers.served_model.as_deref().unwrap()
    );
}

#[test]
fn response_header_model_falls_back_to_x_openai_model() {
    let parsed_headers = CodexResponseHeaders::from_pairs([
        ("content-type", "text/event-stream"),
        ("status", "200"),
        ("x-openai-model", "gpt-codex-fixture-x-served"),
    ]);

    assert_eq!(
        parsed_headers.served_model.as_deref(),
        Some("gpt-codex-fixture-x-served")
    );
    assert_eq!(parsed_headers.http_status, Some(200));
}

#[test]
fn response_headers_override_payload_model_in_accumulator_summary() {
    let mut accumulator = CodexResponsesAccumulator::new();
    let headers = CodexResponseHeaders::from_pairs([
        (":status", "200"),
        ("openai-model", "gpt-codex-fixture-header-served"),
    ]);

    accumulator.apply_headers(&headers);
    accumulator
        .process_chunk(TEXT_STREAM.as_bytes())
        .expect("process fixture");
    accumulator.finish().expect("finish stream");
    let summary = accumulator.summary();

    assert_eq!(summary.http_status, Some(200));
    assert_eq!(
        summary.served_model.as_deref(),
        Some("gpt-codex-fixture-header-served")
    );
    assert_eq!(summary.status, CodexResponseStatus::Completed);
}

#[test]
fn sse_accumulator_should_capture_tool_call_summary_from_fixture() {
    let summary = accumulate_fixture(TOOL_STREAM);

    assert_eq!(summary.status, CodexResponseStatus::Completed);
    assert_eq!(
        summary.output_text,
        "The workspace defines coditor-core and coditor-cli."
    );
    assert_eq!(summary.tool_calls.len(), 1);
    assert_eq!(summary.tool_calls[0].id, "ctc_fixture_read_file_001");
    assert_eq!(summary.tool_calls[0].name.as_deref(), Some("read_file"));
    assert_eq!(summary.tool_calls[0].input, r#"{"path":"Cargo.toml"}"#);
    assert_eq!(summary.usage.input_tokens, 2048);
    assert_eq!(summary.usage.cached_input_tokens, 1024);
    assert_eq!(summary.usage.uncached_input_tokens, 1024);
    assert_eq!(summary.usage.reasoning_output_tokens, 48);
}

#[test]
fn sse_accumulator_handles_split_chunks_and_done_marker() {
    let stream = format!("{TEXT_STREAM}\ndata: [DONE]\n");
    let mut accumulator = CodexResponsesAccumulator::new();

    for chunk in stream.as_bytes().chunks(7) {
        accumulator
            .process_chunk(chunk)
            .expect("process split chunk");
    }
    accumulator.finish().expect("finish stream");
    let summary = accumulator.summary();

    assert_eq!(summary.status, CodexResponseStatus::Completed);
    assert_eq!(
        summary.output_text,
        "Workspace packages: coditor-core and coditor-cli."
    );
}

#[test]
fn sse_accumulator_should_capture_failed_stream_fixture() {
    let summary = accumulate_fixture(FAILED_STREAM);

    assert_eq!(
        summary.response_id.as_deref(),
        Some("resp_fixture_failed_001")
    );
    assert_eq!(summary.status, CodexResponseStatus::Failed);
    assert_eq!(summary.served_model.as_deref(), Some("gpt-codex-fixture"));
    assert_eq!(
        summary.error_message.as_deref(),
        Some("Fixture failure for Coditor contract tests.")
    );
    assert_eq!(summary.output_text, "");
    assert_eq!(summary.usage.total_tokens, 0);
}

#[test]
fn sse_accumulator_should_capture_incomplete_stream_fixture() {
    let summary = accumulate_fixture(INCOMPLETE_STREAM);

    assert_eq!(
        summary.response_id.as_deref(),
        Some("resp_fixture_incomplete_001")
    );
    assert_eq!(summary.status, CodexResponseStatus::Incomplete);
    assert_eq!(summary.served_model.as_deref(), Some("gpt-codex-fixture"));
    assert_eq!(
        summary.output_text,
        "Partial fixture output before max tokens."
    );
    assert_eq!(
        summary.incomplete_reason.as_deref(),
        Some("max_output_tokens")
    );
    assert_eq!(summary.usage.input_tokens, 900);
    assert_eq!(summary.usage.cached_input_tokens, 300);
    assert_eq!(summary.usage.uncached_input_tokens, 600);
    assert_eq!(summary.usage.output_tokens, 64);
    assert_eq!(summary.usage.reasoning_output_tokens, 16);
    assert_eq!(summary.usage.total_tokens, 964);
}

#[test]
fn codex_turn_accounting_summarizes_completed_text_fixture() {
    let request = parse_minimal_request();
    let response = accumulate_fixture(TEXT_STREAM);

    let accounting = summarize_codex_turn(&request, &response);

    assert_eq!(accounting.identity.session_id, "codex-session-fixture-001");
    assert_eq!(
        accounting.identity.session_source,
        CodexSessionIdentitySource::ClientMetadata
    );
    assert_eq!(
        accounting.identity.response_id.as_deref(),
        Some("resp_fixture_text_001")
    );
    assert_eq!(accounting.requested_model, "gpt-codex-fixture");
    assert_eq!(
        accounting.served_model.as_deref(),
        Some("gpt-codex-fixture")
    );
    assert_eq!(accounting.status, CodexTurnStatus::Completed);
    assert!(accounting.is_completed());
    assert_eq!(
        accounting.first_user_prompt_excerpt.as_deref(),
        Some("Summarize the current repository status.")
    );
    assert!(accounting.tool_calls.is_empty());
}

#[test]
fn codex_turn_accounting_treats_cached_input_as_subset() {
    let accounting =
        summarize_codex_turn(&parse_minimal_request(), &accumulate_fixture(TEXT_STREAM));

    assert_eq!(accounting.input_tokens, 1280);
    assert_eq!(accounting.cached_input_tokens, 512);
    assert_eq!(accounting.uncached_input_tokens, 768);
    assert_eq!(accounting.output_tokens, 96);
    assert_eq!(accounting.total_tokens, 1376);
    assert_eq!(
        accounting.total_tokens,
        accounting.input_tokens + accounting.output_tokens
    );
    assert_eq!(
        accounting.total_tokens,
        accounting.uncached_input_tokens
            + accounting.cached_input_tokens
            + accounting.output_tokens
    );
    assert!(accounting.anomalies.is_empty());
}

#[test]
fn codex_turn_accounting_saturates_cached_input_anomaly() {
    let response = CodexResponseSummary {
        status: CodexResponseStatus::Completed,
        served_model: Some("gpt-codex-fixture".to_string()),
        usage: CodexUsage {
            input_tokens: 10,
            cached_input_tokens: 20,
            uncached_input_tokens: 0,
            output_tokens: 5,
            reasoning_output_tokens: 0,
            total_tokens: 15,
        },
        ..Default::default()
    };

    let accounting = summarize_codex_turn(&parse_minimal_request(), &response);

    assert_eq!(accounting.uncached_input_tokens, 0);
    assert_eq!(accounting.total_tokens, 15);
    assert_eq!(
        accounting.anomalies,
        vec![CodexAccountingAnomaly::CachedInputExceedsInput {
            input_tokens: 10,
            cached_input_tokens: 20,
        }]
    );
}

#[test]
fn codex_turn_accounting_tracks_reasoning_tokens_separately() {
    let accounting =
        summarize_codex_turn(&parse_minimal_request(), &accumulate_fixture(TEXT_STREAM));

    assert_eq!(accounting.output_tokens, 96);
    assert_eq!(accounting.reasoning_output_tokens, 32);
    assert_eq!(accounting.total_tokens, 1280 + 96);
    assert_ne!(accounting.total_tokens, 1280 + 96 + 32);
}

#[test]
fn codex_turn_accounting_uses_header_served_model_precedence() {
    let mut accumulator = CodexResponsesAccumulator::new();
    let headers = CodexResponseHeaders::from_pairs([
        (":status", "200"),
        ("openai-model", "gpt-codex-fixture-header-served"),
    ]);
    accumulator.apply_headers(&headers);
    accumulator
        .process_chunk(TEXT_STREAM.as_bytes())
        .expect("process fixture");
    accumulator.finish().expect("finish stream");

    let accounting = summarize_codex_turn(&parse_minimal_request(), &accumulator.summary());

    assert_eq!(
        accounting.served_model.as_deref(),
        Some("gpt-codex-fixture-header-served")
    );
}

#[test]
fn codex_turn_accounting_maps_failed_and_incomplete_statuses() {
    let failed = summarize_codex_turn(&parse_minimal_request(), &accumulate_fixture(FAILED_STREAM));
    let incomplete = summarize_codex_turn(
        &parse_minimal_request(),
        &accumulate_fixture(INCOMPLETE_STREAM),
    );

    assert_eq!(failed.status, CodexTurnStatus::Failed);
    assert!(!failed.is_completed());
    assert_eq!(incomplete.status, CodexTurnStatus::Incomplete);
    assert!(!incomplete.is_completed());
}

#[test]
fn codex_turn_accounting_keeps_unknown_model_pricing_explicit() {
    let accounting =
        summarize_codex_turn(&parse_minimal_request(), &accumulate_fixture(TEXT_STREAM));

    assert_eq!(
        accounting.pricing.status,
        CodexPricingStatus::UnknownModel {
            model: "gpt-codex-fixture".to_string()
        }
    );
    assert_eq!(accounting.pricing.cost_dollars, None);
    assert!(!accounting.pricing.trusted_for_budget_enforcement);
}

fn parse_sse_data_events(stream: &str) -> Vec<&str> {
    stream
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .collect()
}

fn parse_minimal_request() -> coditor_core::codex_request::ParsedCodexRequest {
    parse_codex_responses_request(
        MINIMAL_TEXT_REQUEST.as_bytes(),
        CodexRequestHeaders::default(),
    )
    .expect("parse minimal request fixture")
}

fn accumulate_fixture(stream: &str) -> CodexResponseSummary {
    let mut accumulator = CodexResponsesAccumulator::new();
    accumulator
        .process_chunk(stream.as_bytes())
        .expect("process fixture");
    accumulator.finish().expect("finish fixture");
    accumulator.summary()
}
