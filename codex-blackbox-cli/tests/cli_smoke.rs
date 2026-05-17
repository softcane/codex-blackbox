// CLI smoke tests for Codex Blackbox command surfaces.
// Tests use fake/local HTTP fixtures unless explicitly stated otherwise.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn codex_blackbox(args: &[&str]) -> Output {
    codex_blackbox_with_env(args, &[])
}

fn codex_blackbox_with_env(args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_codex-blackbox"));
    command
        .args(args)
        .env("NO_COLOR", "1")
        .env_remove("ANTHROPIC_BASE_URL");
    for (key, value) in envs {
        command.env(key, value);
    }
    command.output().expect("run codex-blackbox")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn serve_json_once(body: &str) -> (String, mpsc::Receiver<String>) {
    serve_response_once(200, body)
}

fn serve_response_once(status: u16, body: &str) -> (String, mpsc::Receiver<String>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind test server");
    let url = format!("http://{}", listener.local_addr().expect("local addr"));
    let body = body.to_string();
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept request");
        let mut request = Vec::new();
        let mut buffer = [0u8; 1024];
        loop {
            let n = stream.read(&mut buffer).expect("read request");
            if n == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..n]);
            if request_is_complete(&request) {
                break;
            }
        }

        tx.send(String::from_utf8_lossy(&request).into_owned())
            .expect("send captured request");

        write!(
            stream,
            "HTTP/1.1 {} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            status,
            if status < 400 { "OK" } else { "ERROR" },
            body.len(),
            body
        )
        .expect("write response");
    });

    (url, rx)
}

fn request_is_complete(request: &[u8]) -> bool {
    let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
        return false;
    };
    let headers = String::from_utf8_lossy(&request[..header_end]);
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);

    request.len() >= header_end + 4 + content_length
}

fn captured_request(rx: mpsc::Receiver<String>) -> String {
    rx.recv_timeout(Duration::from_secs(2))
        .expect("captured HTTP request")
}

fn unique_test_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time went backwards")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "codex-blackbox-cli-smoke-{label}-{}-{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("create test dir");
    path
}

#[test]
fn top_level_help_exposes_user_workflows() {
    let output = codex_blackbox(&["--help"]);

    assert!(output.status.success(), "stderr:\n{}", stderr(&output));
    let out = stdout(&output);
    assert!(out.contains(
        "Codex Blackbox observability proxy. Codex subscription wrapper is experimental."
    ));
    assert!(out.contains("doctor"));
    assert!(out.contains("up"));
    assert!(out.contains("run"));
    assert!(out.contains("watch"));
    assert!(out.contains("sessions"));
    assert!(out.contains("status"));
    assert!(out.contains("recall"));
    assert!(out.contains("postmortem"));
    assert!(out.contains("reconcile"));
    assert!(out.contains("config"));
}

#[test]
fn run_help_documents_watch_and_trailing_child_command() {
    let output = codex_blackbox(&["run", "--help"]);

    assert!(output.status.success(), "stderr:\n{}", stderr(&output));
    let out = stdout(&output);
    assert!(out.contains("Run a command through Codex Blackbox"));
    assert!(out.contains("experimental subscription proxy overrides"));
    assert!(out.contains("--watch"));
    assert!(out.contains("--dry-run"));
    assert!(!out.contains("--codex-mode"));
    assert!(out.contains("Command and arguments to run"));
}

#[test]
fn config_codex_prints_read_only_future_override() {
    let output = codex_blackbox(&["config", "codex"]);

    assert!(output.status.success(), "stderr:\n{}", stderr(&output));
    let out = stdout(&output);
    assert!(out.contains("Codex Blackbox Codex config preview (read-only)"));
    assert!(out.contains("experimental ChatGPT subscription wrapper"));
    assert!(out.contains("codex-blackbox up"));
    assert!(out.contains("~/.codex/config.toml is not modified"));
    assert!(out.contains(r#"-c 'chatgpt_base_url="http://127.0.0.1:10000/backend-api"'"#));
    assert!(out.contains(r#"-c 'model_provider="codex-blackbox-chatgpt"'"#));
    assert!(out.contains(
        r#"-c 'model_providers.codex-blackbox-chatgpt.base_url="http://127.0.0.1:10000/backend-api/codex"'"#
    ));
    assert!(out.contains("-c model_providers.codex-blackbox-chatgpt.supports_websockets=false"));
    assert!(out.contains("-c features.enable_request_compression=false"));
    assert!(out.contains("Codex Blackbox removes inherited parent-session variables"));
    assert!(out.contains("Codex Blackbox closes child stdin for Codex runs"));
    assert!(out.contains("does not pass codex exec --json"));
    assert!(out.contains("Envoy-observed Responses traffic is the telemetry source"));
    assert!(out.contains("CODEX_THREAD_ID"));
    assert!(out.contains("Codex CLI mode requires an existing Codex ChatGPT login"));
    assert!(!out.contains("model_providers.codex-blackbox-openai"));
}

#[test]
fn run_command_requires_child_command() {
    let output = codex_blackbox(&["run"]);

    assert!(!output.status.success());
    assert!(stderr(&output).contains("required"));
}

#[test]
fn codex_dry_run_prints_overrides_and_preserves_user_args() {
    let output = codex_blackbox(&["run", "--dry-run", "codex", "exec", "hello", "--json"]);

    assert!(output.status.success(), "stderr:\n{}", stderr(&output));
    let out = stdout(&output);
    assert!(out.contains("Mode: experimental Codex ChatGPT subscription proxy"));
    assert!(out.contains("Config files: not modified"));
    assert!(out.contains("Environment overrides:\n  (none)"));
    assert!(out.contains("Environment removals:\n  CODEX_CI"));
    assert!(out.contains("Child stdin: closed for Codex exec"));
    assert!(out.contains("CODEX_INTERNAL_ORIGINATOR_OVERRIDE"));
    assert!(out.contains("CODEX_SHELL"));
    assert!(out.contains("CODEX_THREAD_ID"));
    assert!(
        out.contains(r#"codex exec -c 'chatgpt_base_url="http://127.0.0.1:10000/backend-api"'"#)
    );
    assert!(out.contains(r#"-c 'model_provider="codex-blackbox-chatgpt"'"#));
    assert!(out.contains(
        r#"-c 'model_providers.codex-blackbox-chatgpt.base_url="http://127.0.0.1:10000/backend-api/codex"'"#
    ));
    assert!(out.contains("-c model_providers.codex-blackbox-chatgpt.supports_websockets=false"));
    assert!(out.contains("-c features.enable_request_compression=false"));
    assert!(out.contains("wrapper does not inject --ephemeral"));
    assert!(out.contains("Known Codex rollout-recording warning: suppressed"));
    assert!(out.contains("OPENAI_API_KEY is not used"));
    assert!(out.contains(
        "Post-run check: require Codex Blackbox to observe run-scoped Codex Responses evidence"
    ));
    assert!(!out.contains("forced_login_method"));
    assert!(!out.contains("openai_base_url"));
    assert!(!out.contains("model_providers.codex-blackbox-openai"));
    assert!(!out.contains("--json"));
    assert!(out.contains("hello"));
}

#[test]
fn non_codex_dry_run_has_no_proxy_overrides() {
    let output = codex_blackbox(&["run", "--dry-run", "/bin/sh", "-c", "echo ok"]);

    assert!(output.status.success(), "stderr:\n{}", stderr(&output));
    let out = stdout(&output);
    assert!(out.contains("Mode: plain child command (not proxied)"));
    assert!(out.contains("Environment overrides:\n  (none)"));
    assert!(out.contains("Environment removals:\n  (none)"));
    assert!(!out.contains("ANTHROPIC_BASE_URL"));
    assert!(!out.contains("codex-blackbox-openai-responses"));
    assert!(!out.contains("features.enable_request_compression=false"));
}

#[test]
fn codex_dry_run_does_not_mutate_codex_home_config() {
    let dir = unique_test_dir("codex-home");
    let config_path = dir.join("config.toml");
    let original = "model = \"gpt-5\"\n";
    fs::write(&config_path, original).expect("write codex config");

    let output = codex_blackbox_with_env(
        &["run", "--dry-run", "codex", "exec", "hello"],
        &[("CODEX_HOME", dir.to_str().expect("utf8 temp dir"))],
    );

    assert!(output.status.success(), "stderr:\n{}", stderr(&output));
    assert_eq!(
        fs::read_to_string(&config_path).expect("read codex config"),
        original
    );

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn non_codex_run_command_does_not_set_proxy_env() {
    let output = codex_blackbox(&[
        "run",
        "/bin/sh",
        "-c",
        "[ -z \"${ANTHROPIC_BASE_URL:-}\" ] && exit 7 || exit 9",
    ]);

    assert_eq!(
        output.status.code(),
        Some(7),
        "stderr:\n{}",
        stderr(&output)
    );
}

#[test]
fn watch_rejects_session_filter_with_tmux_mode() {
    let output = codex_blackbox(&["watch", "--session", "session_demo", "--tmux"]);

    assert!(!output.status.success());
    let err = stderr(&output);
    assert!(
        err.contains("cannot be used with"),
        "expected clap conflict error, got:\n{err}"
    );
}

#[test]
fn watch_help_documents_postmortem_and_color_options() {
    let output = codex_blackbox(&["watch", "--help"]);

    assert!(output.status.success(), "stderr:\n{}", stderr(&output));
    let out = stdout(&output);
    assert!(out.contains("--postmortem"));
    assert!(out.contains("--no-redact"));
    assert!(out.contains("--color"));
}

#[test]
fn status_help_documents_json_color_and_width_options() {
    let output = codex_blackbox(&["status", "--help"]);

    assert!(output.status.success(), "stderr:\n{}", stderr(&output));
    let out = stdout(&output);
    assert!(out.contains("--json"));
    assert!(out.contains("--color"));
    assert!(out.contains("--width"));
}

#[test]
fn guard_help_documents_policy_json_color_and_width_options() {
    let output = codex_blackbox(&["guard", "--help"]);

    assert!(output.status.success(), "stderr:\n{}", stderr(&output));
    let out = stdout(&output);
    assert!(out.contains("--policy"));
    assert!(out.contains("--json"));
    assert!(out.contains("--color"));
    assert!(out.contains("--width"));
}

#[test]
fn postmortem_help_documents_color_option() {
    let output = codex_blackbox(&["postmortem", "--help"]);

    assert!(output.status.success(), "stderr:\n{}", stderr(&output));
    let out = stdout(&output);
    assert!(out.contains("--color"));
    assert!(out.contains("--no-redact"));
    assert!(out.contains("--output"));
}

#[test]
fn status_command_renders_uncolored_decision_json_from_postmortem_api() {
    let (url, request_rx) = serve_json_once(
        r#"{
          "schema_version": 1,
          "report_type": "codex_responses_postmortem",
          "session_id": "session_status",
          "redacted": true,
          "partial": false,
          "summary": {"outcome": "Likely Completed", "turn_count": 2},
          "diagnosis": {"primary_cause": "none", "primary_cause_type": "none"},
          "impact": {
            "local_total_tokens": 1234,
            "local_estimate_trusted_for_budget_enforcement": true
          },
          "signals": {
            "response_statuses": {"completed": 2, "failed": 0, "incomplete": 0, "unknown": 0},
            "context_fill": {"max_percent": 31.0},
            "model_mismatches": [],
            "accounting_anomaly_count": 0
          }
        }"#,
    );

    let output = codex_blackbox(&["status", "--url", &url, "--json"]);

    assert!(output.status.success(), "stderr:\n{}", stderr(&output));
    let request = captured_request(request_rx);
    assert!(
        request.starts_with("GET /api/postmortem/last?redact=true "),
        "unexpected request:\n{request}"
    );
    let out = stdout(&output);
    assert!(!out.contains("\x1b["));
    let value: serde_json::Value = serde_json::from_str(&out).expect("status json");
    assert_eq!(value.get("state").and_then(|v| v.as_str()), Some("ended"));
    assert_eq!(
        value
            .pointer("/correlation/session_id")
            .and_then(|v| v.as_str()),
        Some("session_status")
    );
}

#[test]
fn status_json_handles_closed_stdout_pipe_without_panic() {
    let (url, request_rx) = serve_json_once(
        r#"{
          "schema_version": 1,
          "report_type": "codex_responses_postmortem",
          "session_id": "session_status",
          "redacted": true,
          "partial": false,
          "summary": {"outcome": "Likely Completed", "turn_count": 2},
          "diagnosis": {"primary_cause": "none", "primary_cause_type": "none"},
          "impact": {
            "local_total_tokens": 1234,
            "local_estimate_trusted_for_budget_enforcement": true
          },
          "signals": {
            "response_statuses": {"completed": 2, "failed": 0, "incomplete": 0, "unknown": 0},
            "context_fill": {"max_percent": 31.0},
            "model_mismatches": [],
            "accounting_anomaly_count": 0
          }
        }"#,
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_codex-blackbox"))
        .args(["status", "--url", &url, "--json"])
        .env("NO_COLOR", "1")
        .env_remove("ANTHROPIC_BASE_URL")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn codex-blackbox");

    let stdout = child.stdout.take().expect("stdout pipe");
    drop(stdout);

    let output = child.wait_with_output().expect("wait for codex-blackbox");
    let request = captured_request(request_rx);
    assert!(
        request.starts_with("GET /api/postmortem/last?redact=true "),
        "unexpected request:\n{request}"
    );
    assert!(output.status.success(), "stderr:\n{}", stderr(&output));
    assert!(
        !stderr(&output).contains("panicked at"),
        "stderr:\n{}",
        stderr(&output)
    );
}

#[test]
fn guard_command_renders_policy_block_json_from_postmortem_api() {
    let dir = unique_test_dir("guard-policy");
    let policy_path = dir.join("policy.toml");
    fs::write(&policy_path, "session_token_budget = 120000\n").expect("write policy");
    let (url, request_rx) = serve_json_once(
        r#"{
          "schema_version": 1,
          "report_type": "codex_responses_postmortem",
          "session_id": "session_guard",
          "redacted": true,
          "partial": false,
          "summary": {"outcome": "Likely Completed", "turn_count": 2},
          "diagnosis": {"primary_cause": "none", "primary_cause_type": "none"},
          "impact": {
            "local_total_tokens": 125000,
            "local_estimated_cost_dollars": 1.00,
            "local_estimate_trusted_for_budget_enforcement": true
          },
          "signals": {
            "response_statuses": {"completed": 2, "failed": 0, "incomplete": 0, "unknown": 0},
            "context_fill": {"max_percent": 31.0},
            "model_mismatches": [],
            "accounting_anomaly_count": 0
          }
        }"#,
    );

    let output = codex_blackbox(&[
        "guard",
        "--url",
        &url,
        "--policy",
        policy_path.to_str().expect("policy path"),
        "--json",
    ]);

    assert!(output.status.success(), "stderr:\n{}", stderr(&output));
    let request = captured_request(request_rx);
    assert!(
        request.starts_with("GET /api/postmortem/last?redact=true "),
        "unexpected request:\n{request}"
    );
    let out = stdout(&output);
    assert!(!out.contains("\x1b["));
    let value: serde_json::Value = serde_json::from_str(&out).expect("guard json");
    assert_eq!(value.get("state").and_then(|v| v.as_str()), Some("blocked"));
    assert_eq!(
        value.pointer("/policy_block/rule").and_then(|v| v.as_str()),
        Some("session_token_budget")
    );
    assert_eq!(
        value
            .pointer("/policy_block/session_id")
            .and_then(|v| v.as_str()),
        Some("session_guard")
    );
}

#[test]
fn status_command_renders_width_limited_footer_without_ansi_when_color_never() {
    let (url, request_rx) = serve_json_once(
        r#"{
          "schema_version": 1,
          "report_type": "codex_responses_postmortem",
          "session_id": "session_status",
          "redacted": true,
          "partial": true,
          "summary": {"outcome": "Active", "turn_count": 0},
          "diagnosis": {"primary_cause": "none", "primary_cause_type": "none"},
          "impact": {"local_total_tokens": 0},
          "signals": {
            "response_statuses": {"completed": 0, "failed": 0, "incomplete": 0, "unknown": 0},
            "context_fill": {"max_percent": 0.0},
            "model_mismatches": [],
            "accounting_anomaly_count": 0
          }
        }"#,
    );

    let output = codex_blackbox(&[
        "status",
        "--url",
        &url,
        "--color",
        "never",
        "--width",
        "24",
        "session_status",
    ]);

    assert!(output.status.success(), "stderr:\n{}", stderr(&output));
    let request = captured_request(request_rx);
    assert!(
        request.starts_with("GET /api/postmortem/session_status?redact=true "),
        "unexpected request:\n{request}"
    );
    let out = stdout(&output);
    assert!(!out.contains("\x1b["));
    assert!(out.trim().len() <= 24, "{out:?}");
    assert!(out.contains("Watching"), "{out:?}");
    assert!(out.contains("waiting"), "{out:?}");
}

#[test]
fn sessions_command_renders_sessions_from_api() {
    let (url, request_rx) = serve_json_once(
        r#"{
          "local_estimate_cost_source": "builtin_model_family_pricing",
          "local_estimate_trusted_for_budget_enforcement": false,
          "cost_source": "builtin_model_family_pricing",
          "trusted_for_budget_enforcement": false,
          "sessions": [
            {
	              "session_id": "session_abcdefghijklmnopqrstuvwxyz",
	              "display_name": "codex-blackbox",
	              "model": "gpt-5.5",
	              "requested_model": "gpt-5.5",
              "served_model": "gpt-5.4",
              "total_turns": 7,
              "outcome": "Likely Completed",
              "local_estimate_total_cost_dollars": 1.23,
              "estimated_total_cost_dollars": 1.23,
              "billed_cost_dollars": 1.11,
              "codex_input_tokens": 1000,
              "codex_cached_input_tokens": 500,
              "codex_cached_input_ratio": 0.5,
              "primary_cause": ""
            }
          ]
        }"#,
    );

    let output = codex_blackbox(&["sessions", "--url", &url, "--limit", "1", "--days", "2"]);

    assert!(output.status.success(), "stderr:\n{}", stderr(&output));
    let request = captured_request(request_rx);
    assert!(
        request.starts_with("GET /api/sessions?limit=1&days=2 "),
        "unexpected request:\n{request}"
    );
    let out = stdout(&output);
    assert!(out.contains("Local estimate source: built-in model-family pricing"));
    assert!(out.contains("codex-blackbox"));
    assert!(out.contains("gpt-5.5->gpt-5.4"));
    assert!(out.contains("$1.23"));
    assert!(out.contains("$1.11"));
    assert!(out.contains("50%"));
}

#[test]
fn sessions_command_reports_api_errors() {
    let (url, request_rx) = serve_response_once(503, r#"{"error":"db unavailable"}"#);

    let output = codex_blackbox(&["sessions", "--url", &url]);

    assert!(!output.status.success());
    let request = captured_request(request_rx);
    assert!(
        request.starts_with("GET /api/sessions?limit=20&days=7 "),
        "unexpected request:\n{request}"
    );
    assert!(stderr(&output).contains("Error: HTTP 503"));
}

#[test]
fn recall_command_renders_ranked_hits_from_api() {
    let (url, request_rx) = serve_json_once(
        r#"{
          "query": "auth cache",
          "hits": [
            {
              "score": 88,
              "session_id": "session_recall",
              "started_at": "2026-04-28T10:15:00Z",
              "completed_at": "2026-04-28T10:45:00Z",
              "model": "gpt-5.5",
              "outcome": "Likely Completed",
              "initial_prompt": "Investigate auth cache",
              "final_response_summary": "Fixed the auth cache warm path."
            }
          ]
        }"#,
    );

    let output = codex_blackbox(&[
        "recall", "--url", &url, "--limit", "2", "--days", "9", "auth", "cache",
    ]);

    assert!(output.status.success(), "stderr:\n{}", stderr(&output));
    let request = captured_request(request_rx);
    assert!(
        request.starts_with("GET /api/recall?"),
        "unexpected request:\n{request}"
    );
    assert!(request.contains("q=auth+cache"));
    assert!(request.contains("limit=2"));
    assert!(request.contains("days=9"));
    let out = stdout(&output);
    assert!(out.contains("Recall results for \"auth cache\":"));
    assert!(out.contains("session_recall"));
    assert!(out.contains("Outcome: Likely Completed"));
    assert!(out.contains("Prompt: Investigate auth cache"));
    assert!(out.contains("Landed: Fixed the auth cache warm path."));
}

#[test]
fn recall_command_reports_no_matches() {
    let (url, request_rx) = serve_json_once(r#"{"query":"missing","hits":[]}"#);

    let output = codex_blackbox(&["recall", "--url", &url, "missing"]);

    assert!(output.status.success(), "stderr:\n{}", stderr(&output));
    let request = captured_request(request_rx);
    assert!(
        request.starts_with("GET /api/recall?"),
        "unexpected request:\n{request}"
    );
    assert!(stdout(&output).contains("No matches for \"missing\"."));
}

#[test]
fn postmortem_command_renders_colored_human_terminal_report_from_api() {
    let (url, request_rx) = serve_json_once(
        r#"{
          "schema_version": 1,
          "report_type": "codex_responses_postmortem",
          "session_id": "session_postmortem",
          "redacted": true,
          "partial": false,
          "summary": {
            "outcome": "Likely Partially Completed",
            "turn_count": 2,
            "requested_model": "gpt-5.5",
            "served_model": "gpt-5.5",
            "initial_prompt_excerpt": "[redacted prompt excerpt]",
            "final_response_summary": "Partial fixture output before max tokens."
          },
          "diagnosis": {
            "primary_cause": "Codex Responses incomplete"
          },
          "impact": {
            "input_tokens": 900,
            "cached_input_tokens": 300,
            "uncached_input_tokens": 600,
            "output_tokens": 64,
            "reasoning_output_tokens": 16,
            "local_total_tokens": 964,
            "local_estimated_cost_dollars": 0.0,
            "local_estimate_source": "codex_unpriced:unknown_model:gpt-5.5",
            "local_estimate_trusted_for_budget_enforcement": false
          },
          "signals": {
            "response_statuses": {"completed": 0, "failed": 0, "incomplete": 1, "unknown": 0},
            "context_fill": {"max_percent": 12.5, "estimated": true},
            "cached_input_reuse": {"ratio": 0.333},
            "reasoning_output_share": {"max_ratio": 0.25},
            "tool_call_intent_counts": {"read_file": 1}
          },
          "evidence": [
            {"type": "direct", "signal": "codex_response_incomplete", "turn": 2, "timestamp": "2026-04-30T12:00:05Z", "detail": "max_output_tokens"}
          ],
          "timeline": [
            {"timestamp": "2026-04-30T12:00:05Z", "event": "codex_turn", "detail": "turn 2 status incomplete"}
          ],
          "recommendations": ["Continue with a narrower prompt."],
          "caveats": [
            "Evidence is limited to local Envoy-observed Codex Responses traffic.",
            "Tool-call rows are model-side intent only; local execution outcome is not observed."
          ],
          "restart_prompt": "Continue from Codex Blackbox session session_postmortem."
        }"#,
    );

    let output = codex_blackbox(&["postmortem", "--url", &url, "--color", "always", "last"]);

    assert!(output.status.success(), "stderr:\n{}", stderr(&output));
    let request = captured_request(request_rx);
    assert!(
        request.starts_with("GET /api/postmortem/last?redact=true "),
        "unexpected request:\n{request}"
    );
    let out = stdout(&output);
    assert!(out.contains("\x1b["));
    assert!(out.contains("\u{250c}\u{2500}[ Codex Session Report ]"));
    assert!(out.contains("[ At a Glance ]"));
    assert!(out.contains("[ Checks ]"));
    assert!(out.contains("[ What Triggered This ]"));
    assert!(out.contains("[ Timeline ]"));
    assert!(out.contains("[ Next Steps ]"));
    assert!(out.contains("[ Limits ]"));
    assert!(out.contains("[ Continue Prompt ]"));
    assert!(!out.contains("# Codex Session Report"));
    assert!(out.contains("[redacted prompt excerpt]"));
    assert!(out.contains("Pricing"));
    assert!(out.contains("no trusted price for gpt-5.5"));
    assert!(out.contains("Cost Confidence"));
    assert!(out.contains("untrusted - dollar budgets stay advisory"));
    assert!(out.contains("Tool requests"));
    assert!(out.contains("read_file: 1"));
    assert!(out.contains("response stopped incomplete"));
    assert!(out.contains("hit max_output_tokens"));
    assert!(!out.contains("Tool-call intent"));
    assert!(!out.contains("Responses statuses"));
    assert!(!out.contains("Max reasoning-output share"));
    assert!(!out.to_ascii_lowercase().contains("tool result"));
    assert!(!out.to_ascii_lowercase().contains("mcp lifecycle"));
}

#[test]
fn postmortem_command_supports_no_redact_and_output_file() {
    let (url, request_rx) = serve_json_once(
        r#"{
          "schema_version": 1,
          "session_id": "session_postmortem",
          "redacted": false,
          "partial": true,
          "summary": {
            "outcome": "Likely Completed",
            "turn_count": 1,
            "requested_model": "gpt-5.5",
            "initial_prompt_excerpt": "Inspect /Users/alice/private/repo"
          },
          "diagnosis": {"primary_cause": "none"},
          "impact": {
            "input_tokens": 10,
            "cached_input_tokens": 4,
            "uncached_input_tokens": 6,
            "output_tokens": 2,
            "reasoning_output_tokens": 0,
            "local_total_tokens": 12,
            "local_estimated_cost_dollars": 0.01
          },
          "signals": {"response_statuses": {"completed": 1}},
          "evidence": [],
          "timeline": [],
          "recommendations": ["Continue."],
          "caveats": ["Evidence is limited to local Envoy-observed Codex Responses traffic."]
        }"#,
    );
    let dir = unique_test_dir("postmortem-output");
    let output_path = dir.join("report.md");

    let output = codex_blackbox(&[
        "postmortem",
        "--url",
        &url,
        "--no-redact",
        "--output",
        output_path.to_str().expect("utf8 path"),
        "session_postmortem",
    ]);

    assert!(output.status.success(), "stderr:\n{}", stderr(&output));
    assert!(stdout(&output).is_empty());
    let request = captured_request(request_rx);
    assert!(
        request.starts_with("GET /api/postmortem/session_postmortem?redact=false "),
        "unexpected request:\n{request}"
    );
    let markdown = fs::read_to_string(&output_path).expect("read postmortem output");
    assert!(markdown.contains("Inspect /Users/alice/private/repo"));
    assert!(markdown.contains("# Codex Session Report"));
    assert!(markdown.contains("partial - session may still be running"));
    assert!(markdown.contains("## At a Glance"));
    assert!(markdown.contains("## Next Steps"));

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn postmortem_command_reports_api_errors() {
    let (url, request_rx) = serve_response_once(404, r#"{"error":"not found"}"#);

    let output = codex_blackbox(&["postmortem", "--url", &url, "session_missing"]);

    assert!(!output.status.success());
    let request = captured_request(request_rx);
    assert!(
        request.starts_with("GET /api/postmortem/session_missing?redact=true "),
        "unexpected request:\n{request}"
    );
    assert!(stderr(&output).contains("Error: HTTP 404"));
}

#[test]
fn reconcile_command_posts_billing_payload() {
    let (url, request_rx) = serve_json_once(r#"{"inserted":1}"#);

    let output = codex_blackbox(&[
        "reconcile",
        "--url",
        &url,
        "--session",
        "session_demo",
        "--billed-cost",
        "3.5",
        "--source",
        "invoice_test",
        "--imported-at",
        "2026-04-28T00:00:00Z",
    ]);

    assert!(output.status.success(), "stderr:\n{}", stderr(&output));
    let request = captured_request(request_rx);
    assert!(
        request.starts_with("POST /api/billing-reconciliations "),
        "unexpected request:\n{request}"
    );
    assert!(request.contains(r#""session_id":"session_demo""#));
    assert!(request.contains(r#""source":"invoice_test""#));
    assert!(request.contains(r#""billed_cost_dollars":3.5"#));
    assert!(request.contains(r#""imported_at":"2026-04-28T00:00:00Z""#));
    assert!(stdout(&output).contains("Imported 1 billed reconciliation."));
}

#[test]
fn reconcile_command_reports_api_errors() {
    let (url, request_rx) = serve_response_once(404, r#"{"error":"unknown session"}"#);

    let output = codex_blackbox(&[
        "reconcile",
        "--url",
        &url,
        "--session",
        "session_missing",
        "--billed-cost",
        "3.5",
        "--source",
        "invoice_test",
    ]);

    assert!(!output.status.success());
    let request = captured_request(request_rx);
    assert!(
        request.starts_with("POST /api/billing-reconciliations "),
        "unexpected request:\n{request}"
    );
    assert!(stderr(&output).contains("Error: HTTP 404"));
}
