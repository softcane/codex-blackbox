// UNPORTED/DEFERRED: copied baseline from Phase 0A for workspace shape only.
// This still validates Anthropic-shaped behavior and is not Coditor validation.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn coditor(args: &[&str]) -> Output {
    coditor_with_env(args, &[])
}

fn coditor_with_env(args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_coditor"));
    command.args(args).env("NO_COLOR", "1");
    for (key, value) in envs {
        command.env(key, value);
    }
    command.output().expect("run coditor")
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
        "coditor-cli-smoke-{label}-{}-{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("create test dir");
    path
}

#[test]
fn top_level_help_exposes_user_workflows() {
    let output = coditor(&["--help"]);

    assert!(output.status.success(), "stderr:\n{}", stderr(&output));
    let out = stdout(&output);
    assert!(out.contains("Coditor observability proxy. Codex API-key wrapper is experimental."));
    assert!(out.contains("doctor"));
    assert!(out.contains("up"));
    assert!(out.contains("run"));
    assert!(out.contains("watch"));
    assert!(out.contains("sessions"));
    assert!(out.contains("recall"));
    assert!(out.contains("reconcile"));
    assert!(out.contains("config"));
}

#[test]
fn run_help_documents_watch_and_trailing_child_command() {
    let output = coditor(&["run", "--help"]);

    assert!(output.status.success(), "stderr:\n{}", stderr(&output));
    let out = stdout(&output);
    assert!(out.contains("Run a command through Coditor"));
    assert!(out.contains("experimental API-key proxy overrides"));
    assert!(out.contains("--watch"));
    assert!(out.contains("--dry-run"));
    assert!(out.contains("Command and arguments to run"));
}

#[test]
fn config_codex_prints_read_only_future_override() {
    let output = coditor(&["config", "codex"]);

    assert!(output.status.success(), "stderr:\n{}", stderr(&output));
    let out = stdout(&output);
    assert!(out.contains("Coditor Codex config preview (read-only)"));
    assert!(out.contains("experimental manual OpenAI API-key wrapper"));
    assert!(out.contains("docker compose -f docker-compose.yml -f docker-compose.openai.yml up -d"));
    assert!(out.contains("~/.codex/config.toml is not modified"));
    assert!(out.contains(r#"-c 'model_provider="coditor-openai-responses"'"#));
    assert!(out.contains(
        r#"-c 'model_providers.coditor-openai-responses.base_url="http://127.0.0.1:10000/v1"'"#
    ));
    assert!(
        out.contains(r#"-c 'model_providers.coditor-openai-responses.env_key="OPENAI_API_KEY"'"#)
    );
    assert!(out.contains("-c features.enable_request_compression=false"));
    assert!(out.contains("ChatGPT-auth Codex backend routing is not supported or verified"));
}

#[test]
fn run_command_requires_child_command() {
    let output = coditor(&["run"]);

    assert!(!output.status.success());
    assert!(stderr(&output).contains("required"));
}

#[test]
fn codex_dry_run_prints_overrides_and_preserves_user_args() {
    let output = coditor(&["run", "--dry-run", "codex", "exec", "hello", "--json"]);

    assert!(output.status.success(), "stderr:\n{}", stderr(&output));
    let out = stdout(&output);
    assert!(out.contains("Mode: experimental Codex API-key proxy"));
    assert!(out.contains("Config files: not modified"));
    assert!(out.contains("Environment overrides:\n  (none)"));
    assert!(out.contains(r#"codex -c 'model_provider="coditor-openai-responses"'"#));
    assert!(out.contains(
        r#"-c 'model_providers.coditor-openai-responses.base_url="http://127.0.0.1:10000/v1"'"#
    ));
    assert!(out.contains(r#"-c 'model_providers.coditor-openai-responses.wire_api="responses"'"#));
    assert!(out.contains("-c features.enable_request_compression=false"));
    assert!(out.contains("exec hello --json"));
}

#[test]
fn non_codex_dry_run_keeps_unported_anthropic_fallback() {
    let output = coditor(&["run", "--dry-run", "/bin/sh", "-c", "echo ok"]);

    assert!(output.status.success(), "stderr:\n{}", stderr(&output));
    let out = stdout(&output);
    assert!(out.contains("Mode: UNPORTED copied Anthropic fallback"));
    assert!(out.contains("ANTHROPIC_BASE_URL=http://127.0.0.1:10000"));
    assert!(!out.contains("coditor-openai-responses"));
    assert!(!out.contains("features.enable_request_compression=false"));
}

#[test]
fn codex_dry_run_does_not_mutate_codex_home_config() {
    let dir = unique_test_dir("codex-home");
    let config_path = dir.join("config.toml");
    let original = "model = \"gpt-5\"\n";
    fs::write(&config_path, original).expect("write codex config");

    let output = coditor_with_env(
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
fn run_command_uses_proxy_env_after_core_health_check() {
    let (url, request_rx) = serve_json_once(r#"{"ok":true}"#);
    let health_url = format!("{url}/health");

    let output = coditor_with_env(
        &[
            "run",
            "/bin/sh",
            "-c",
            "printf '%s' \"$ANTHROPIC_BASE_URL\"; exit 7",
        ],
        &[("CODITOR_CORE_HEALTH_URL", &health_url)],
    );

    assert_eq!(
        output.status.code(),
        Some(7),
        "stderr:\n{}",
        stderr(&output)
    );
    let request = captured_request(request_rx);
    assert!(
        request.starts_with("GET /health "),
        "unexpected request:\n{request}"
    );
    assert_eq!(stdout(&output), "http://127.0.0.1:10000");
}

#[test]
fn watch_rejects_session_filter_with_tmux_mode() {
    let output = coditor(&["watch", "--session", "session_demo", "--tmux"]);

    assert!(!output.status.success());
    let err = stderr(&output);
    assert!(
        err.contains("cannot be used with"),
        "expected clap conflict error, got:\n{err}"
    );
}

#[test]
fn sessions_command_renders_sessions_from_api() {
    let (url, request_rx) = serve_json_once(
        r#"{
          "cost_source": "builtin_model_family_pricing",
          "trusted_for_budget_enforcement": false,
          "sessions": [
            {
              "session_id": "session_abcdefghijklmnopqrstuvwxyz",
              "model": "claude-sonnet-4-5-20250929",
              "total_turns": 7,
              "outcome": "Likely Completed",
              "estimated_total_cost_dollars": 1.23,
              "billed_cost_dollars": 1.11,
              "cache_hit_ratio": 0.5,
              "primary_cause": ""
            }
          ]
        }"#,
    );

    let output = coditor(&["sessions", "--url", &url, "--limit", "1", "--days", "2"]);

    assert!(output.status.success(), "stderr:\n{}", stderr(&output));
    let request = captured_request(request_rx);
    assert!(
        request.starts_with("GET /api/sessions?limit=1&days=2 "),
        "unexpected request:\n{request}"
    );
    let out = stdout(&output);
    assert!(out.contains("Estimated cost source: built-in model-family pricing"));
    assert!(out.contains("session_abcdefghijkl"));
    assert!(out.contains("sonnet-4-5-20250929"));
    assert!(out.contains("$1.23"));
    assert!(out.contains("$1.11"));
}

#[test]
fn sessions_command_reports_api_errors() {
    let (url, request_rx) = serve_response_once(503, r#"{"error":"db unavailable"}"#);

    let output = coditor(&["sessions", "--url", &url]);

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
              "model": "claude-sonnet-4-5-20250929",
              "outcome": "Likely Completed",
              "initial_prompt": "Investigate auth cache",
              "final_response_summary": "Fixed the auth cache warm path."
            }
          ]
        }"#,
    );

    let output = coditor(&[
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

    let output = coditor(&["recall", "--url", &url, "missing"]);

    assert!(output.status.success(), "stderr:\n{}", stderr(&output));
    let request = captured_request(request_rx);
    assert!(
        request.starts_with("GET /api/recall?"),
        "unexpected request:\n{request}"
    );
    assert!(stdout(&output).contains("No matches for \"missing\"."));
}

#[test]
fn reconcile_command_posts_billing_payload() {
    let (url, request_rx) = serve_json_once(r#"{"inserted":1}"#);

    let output = coditor(&[
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

    let output = coditor(&[
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
