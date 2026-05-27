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

fn serve_response_sequence(responses: Vec<(u16, String)>) -> (String, mpsc::Receiver<String>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind test server");
    let url = format!("http://{}", listener.local_addr().expect("local addr"));
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        for (status, body) in responses {
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
        }
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

fn captured_requests(rx: mpsc::Receiver<String>, count: usize) -> Vec<String> {
    (0..count)
        .map(|_| {
            rx.recv_timeout(Duration::from_secs(2))
                .expect("captured HTTP request")
        })
        .collect()
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
    assert!(out.contains("coach"));
    assert!(out.contains("baseline"));
    assert!(out.contains("config"));
    assert!(out.contains("ui"));
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
fn ui_enable_dry_run_prints_exact_config_without_mutating_files() {
    let dir = unique_test_dir("ui-enable-dry-run");
    let config_path = dir.join("config.toml");
    let state_dir = dir.join("state");
    fs::write(&config_path, "model = \"gpt-5\"\n").expect("write config");

    let output = codex_blackbox_with_env(
        &["ui", "enable", "--dry-run"],
        &[
            (
                "CODEX_BLACKBOX_CODEX_CONFIG",
                config_path.to_str().expect("utf8 config"),
            ),
            (
                "CODEX_BLACKBOX_UI_STATE_DIR",
                state_dir.to_str().expect("utf8 state"),
            ),
        ],
    );

    assert!(output.status.success(), "stderr:\n{}", stderr(&output));
    let out = stdout(&output);
    assert!(out.contains("Codex Blackbox UI enable preview"));
    assert!(out.contains("Dry run: no files modified"));
    assert!(out.contains(config_path.to_str().expect("utf8 config")));
    assert!(out.contains(r#"openai_base_url = "http://127.0.0.1:10000/backend-api/codex""#));
    assert!(out.contains("[features]"));
    assert!(out.contains("enable_request_compression = false"));
    assert!(!out.contains(r#"chatgpt_base_url = "http://127.0.0.1:10000/backend-api""#));
    assert!(!out.contains(r#"model_provider = "codex-blackbox-chatgpt""#));
    assert!(!out.contains("[model_providers.codex-blackbox-chatgpt]"));
    assert_eq!(
        fs::read_to_string(&config_path).expect("read config"),
        "model = \"gpt-5\"\n"
    );
    assert!(!state_dir.exists());

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn ui_enable_and_disable_round_trip_through_temp_config() {
    let dir = unique_test_dir("ui-enable-disable");
    let config_path = dir.join("config.toml");
    let state_dir = dir.join("state");
    let original = "model = \"gpt-5\"\nmodel_provider = \"openai\"\n";
    fs::write(&config_path, original).expect("write config");
    let envs = [
        (
            "CODEX_BLACKBOX_CODEX_CONFIG",
            config_path.to_str().expect("utf8 config"),
        ),
        (
            "CODEX_BLACKBOX_UI_STATE_DIR",
            state_dir.to_str().expect("utf8 state"),
        ),
    ];

    let enable = codex_blackbox_with_env(&["ui", "enable"], &envs);
    assert!(enable.status.success(), "stderr:\n{}", stderr(&enable));
    assert!(stdout(&enable).contains("Codex Blackbox UI mode enabled"));
    let enabled = fs::read_to_string(&config_path).expect("read enabled");
    assert!(enabled.contains(r#"model_provider = "openai""#));
    assert!(enabled.contains(r#"openai_base_url = "http://127.0.0.1:10000/backend-api/codex""#));
    assert!(enabled.contains("enable_request_compression = false"));
    assert!(!enabled.contains("codex-blackbox-chatgpt"));
    assert!(state_dir.join("codex-ui-state.json").is_file());

    let disable = codex_blackbox_with_env(&["ui", "disable"], &envs);
    assert!(disable.status.success(), "stderr:\n{}", stderr(&disable));
    assert!(stdout(&disable).contains("Codex Blackbox UI mode disabled"));
    let disabled = fs::read_to_string(&config_path).expect("read disabled");
    assert!(disabled.contains("model = \"gpt-5\""));
    assert!(disabled.contains("model_provider = \"openai\""));
    assert!(!disabled.contains("codex-blackbox-chatgpt"));
    assert!(!state_dir.join("codex-ui-state.json").exists());

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn ui_status_json_reports_not_configured_without_touching_real_config() {
    let dir = unique_test_dir("ui-status-missing");
    let config_path = dir.join("missing-config.toml");
    let state_dir = dir.join("state");

    let output = codex_blackbox_with_env(
        &["ui", "status", "--json"],
        &[
            (
                "CODEX_BLACKBOX_CODEX_CONFIG",
                config_path.to_str().expect("utf8 config"),
            ),
            (
                "CODEX_BLACKBOX_UI_STATE_DIR",
                state_dir.to_str().expect("utf8 state"),
            ),
            ("CODEX_BLACKBOX_UI_SKIP_PROCESS_DETECTION", "1"),
        ],
    );

    assert!(output.status.success(), "stderr:\n{}", stderr(&output));
    let value: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("status json");
    assert_eq!(
        value.get("state").and_then(|v| v.as_str()),
        Some("not_configured")
    );
    assert_eq!(
        value
            .pointer("/config/config_exists")
            .and_then(|v| v.as_bool()),
        Some(false)
    );
    assert!(!config_path.exists());
    assert!(!state_dir.exists());

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn ui_doctor_json_reports_missing_config_stack_readiness_and_restart_warning() {
    let dir = unique_test_dir("ui-doctor");
    let config_path = dir.join("missing-config.toml");
    let state_dir = dir.join("state");
    let (url, request_rx) = serve_response_once(200, "ok");

    let output = codex_blackbox_with_env(
        &["ui", "doctor", "--json", "--url", &url],
        &[
            (
                "CODEX_BLACKBOX_CODEX_CONFIG",
                config_path.to_str().expect("utf8 config"),
            ),
            (
                "CODEX_BLACKBOX_UI_STATE_DIR",
                state_dir.to_str().expect("utf8 state"),
            ),
            (
                "CODEX_BLACKBOX_UI_PROCESS_FIXTURE",
                "102 /usr/local/bin/codex app-server --port 1234",
            ),
        ],
    );

    assert!(output.status.success(), "stderr:\n{}", stderr(&output));
    let request = captured_request(request_rx);
    assert!(
        request.starts_with("GET /health "),
        "unexpected request:\n{request}"
    );
    let value: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("doctor json");
    assert_eq!(
        value.pointer("/config/status").and_then(|v| v.as_str()),
        Some("not_configured")
    );
    assert_eq!(
        value.get("core_ready").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        value.get("restart_required").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        value
            .pointer("/active_app_server_processes/0/pid")
            .and_then(|v| v.as_u64()),
        Some(102)
    );

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn ui_doctor_json_reports_configured_state() {
    let dir = unique_test_dir("ui-doctor-configured");
    let config_path = dir.join("config.toml");
    let state_dir = dir.join("state");
    let envs = [
        (
            "CODEX_BLACKBOX_CODEX_CONFIG",
            config_path.to_str().expect("utf8 config"),
        ),
        (
            "CODEX_BLACKBOX_UI_STATE_DIR",
            state_dir.to_str().expect("utf8 state"),
        ),
        ("CODEX_BLACKBOX_UI_SKIP_PROCESS_DETECTION", "1"),
        ("CODEX_BLACKBOX_UI_SKIP_ENVOY_LOG_DETECTION", "1"),
    ];
    let enable = codex_blackbox_with_env(&["ui", "enable"], &envs);
    assert!(enable.status.success(), "stderr:\n{}", stderr(&enable));
    let (url, _request_rx) = serve_response_once(200, "ok");

    let output = codex_blackbox_with_env(&["ui", "doctor", "--json", "--url", &url], &envs);

    assert!(output.status.success(), "stderr:\n{}", stderr(&output));
    let value: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("doctor json");
    assert_eq!(
        value.pointer("/config/status").and_then(|v| v.as_str()),
        Some("configured")
    );
    assert_eq!(
        value
            .pointer("/config/state_exists")
            .and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        value.get("restart_required").and_then(|v| v.as_bool()),
        Some(false)
    );

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn ui_smoke_is_guided_and_does_not_start_live_model_traffic() {
    let output = codex_blackbox(&["ui", "smoke"]);

    assert!(output.status.success(), "stderr:\n{}", stderr(&output));
    let out = stdout(&output);
    assert!(out.contains("guided"));
    assert!(out.contains("No live model traffic"));
    assert!(out.contains("provider=\"codex_responses\""));
}

#[test]
fn ui_status_json_reports_configured_unobserved_from_core_observation_api() {
    let dir = unique_test_dir("ui-status-unobserved");
    let config_path = dir.join("config.toml");
    let state_dir = dir.join("state");
    let envs = [
        (
            "CODEX_BLACKBOX_CODEX_CONFIG",
            config_path.to_str().expect("utf8 config"),
        ),
        (
            "CODEX_BLACKBOX_UI_STATE_DIR",
            state_dir.to_str().expect("utf8 state"),
        ),
        ("CODEX_BLACKBOX_UI_SKIP_PROCESS_DETECTION", "1"),
        ("CODEX_BLACKBOX_UI_SKIP_ENVOY_LOG_DETECTION", "1"),
    ];
    let enable = codex_blackbox_with_env(&["ui", "enable"], &envs);
    assert!(enable.status.success(), "stderr:\n{}", stderr(&enable));
    let (url, request_rx) = serve_json_once(
        r#"{"provider":"codex_responses","request_count":0,"latest_request_rowid":0,"matched":false}"#,
    );

    let output = codex_blackbox_with_env(&["ui", "status", "--json", "--url", &url], &envs);

    assert!(output.status.success(), "stderr:\n{}", stderr(&output));
    let request = captured_request(request_rx);
    assert!(
        request.starts_with("POST /api/observations/codex "),
        "unexpected request:\n{request}"
    );
    assert!(request.contains(r#""after_request_rowid":0"#));
    let value: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("status json");
    assert_eq!(
        value.get("state").and_then(|v| v.as_str()),
        Some("configured_unobserved")
    );
    assert_eq!(
        value.get("core_ready").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        value
            .get("observed_codex_responses")
            .and_then(|v| v.as_bool()),
        Some(false)
    );

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn ui_status_json_reports_websocket_only_when_ui_reaches_proxy_without_http_fallback() {
    let dir = unique_test_dir("ui-status-websocket-only");
    let config_path = dir.join("config.toml");
    let state_dir = dir.join("state");
    let envoy_logs = r#"{"status":426,"method":"GET","path":"/backend-api/codex/responses","upgrade":"websocket"}"#;
    let envs = [
        (
            "CODEX_BLACKBOX_CODEX_CONFIG",
            config_path.to_str().expect("utf8 config"),
        ),
        (
            "CODEX_BLACKBOX_UI_STATE_DIR",
            state_dir.to_str().expect("utf8 state"),
        ),
        (
            "CODEX_BLACKBOX_UI_PROCESS_FIXTURE",
            "102 /usr/local/bin/codex app-server --port 1234",
        ),
        ("CODEX_BLACKBOX_UI_ENVOY_LOG_FIXTURE", envoy_logs),
    ];
    let enable = codex_blackbox_with_env(&["ui", "enable"], &envs);
    assert!(enable.status.success(), "stderr:\n{}", stderr(&enable));
    let (url, request_rx) = serve_json_once(
        r#"{"provider":"codex_responses","request_count":0,"latest_request_rowid":0,"matched":false}"#,
    );

    let output = codex_blackbox_with_env(&["ui", "status", "--json", "--url", &url], &envs);

    assert!(output.status.success(), "stderr:\n{}", stderr(&output));
    let request = captured_request(request_rx);
    assert!(
        request.starts_with("POST /api/observations/codex "),
        "unexpected request:\n{request}"
    );
    let value: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("status json");
    assert_eq!(
        value.get("state").and_then(|v| v.as_str()),
        Some("websocket_only_unobservable")
    );
    assert_eq!(
        value
            .get("recent_http_responses_post")
            .and_then(|v| v.as_bool()),
        Some(false)
    );
    assert_eq!(
        value
            .get("recent_websocket_upgrade_required")
            .and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        value
            .get("observed_codex_responses")
            .and_then(|v| v.as_bool()),
        Some(false)
    );

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn ui_status_json_reports_http_unparsed_when_proxy_sees_post_without_core_evidence() {
    let dir = unique_test_dir("ui-status-http-unparsed");
    let config_path = dir.join("config.toml");
    let state_dir = dir.join("state");
    let envoy_logs =
        r#"{"status":200,"method":"POST","path":"/backend-api/codex/responses","upgrade":""}"#;
    let envs = [
        (
            "CODEX_BLACKBOX_CODEX_CONFIG",
            config_path.to_str().expect("utf8 config"),
        ),
        (
            "CODEX_BLACKBOX_UI_STATE_DIR",
            state_dir.to_str().expect("utf8 state"),
        ),
        (
            "CODEX_BLACKBOX_UI_PROCESS_FIXTURE",
            "102 /usr/local/bin/codex app-server --port 1234",
        ),
        ("CODEX_BLACKBOX_UI_ENVOY_LOG_FIXTURE", envoy_logs),
    ];
    let enable = codex_blackbox_with_env(&["ui", "enable"], &envs);
    assert!(enable.status.success(), "stderr:\n{}", stderr(&enable));
    let (url, request_rx) = serve_json_once(
        r#"{"provider":"codex_responses","request_count":0,"latest_request_rowid":0,"matched":false}"#,
    );

    let output = codex_blackbox_with_env(&["ui", "status", "--json", "--url", &url], &envs);

    assert!(output.status.success(), "stderr:\n{}", stderr(&output));
    let request = captured_request(request_rx);
    assert!(
        request.starts_with("POST /api/observations/codex "),
        "unexpected request:\n{request}"
    );
    let value: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("status json");
    assert_eq!(
        value.get("state").and_then(|v| v.as_str()),
        Some("http_responses_unparsed")
    );
    assert_eq!(
        value
            .get("recent_http_responses_post")
            .and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        value
            .get("recent_websocket_upgrade_required")
            .and_then(|v| v.as_bool()),
        Some(false)
    );
    assert_eq!(
        value
            .get("observed_codex_responses")
            .and_then(|v| v.as_bool()),
        Some(false)
    );

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn ui_status_json_does_not_treat_fake_fixture_sessions_as_live_ui_evidence() {
    let dir = unique_test_dir("ui-status-fake-fixture");
    let config_path = dir.join("config.toml");
    let state_dir = dir.join("state");
    let envs = [
        (
            "CODEX_BLACKBOX_CODEX_CONFIG",
            config_path.to_str().expect("utf8 config"),
        ),
        (
            "CODEX_BLACKBOX_UI_STATE_DIR",
            state_dir.to_str().expect("utf8 state"),
        ),
        ("CODEX_BLACKBOX_UI_SKIP_PROCESS_DETECTION", "1"),
        ("CODEX_BLACKBOX_UI_SKIP_ENVOY_LOG_DETECTION", "1"),
    ];
    let enable = codex_blackbox_with_env(&["ui", "enable"], &envs);
    assert!(enable.status.success(), "stderr:\n{}", stderr(&enable));
    let (url, request_rx) = serve_response_sequence(vec![
        (
            200,
            r#"{"provider":"codex_responses","request_count":1,"latest_request_rowid":9,"matched":true}"#
                .to_string(),
        ),
        (
            200,
            r#"{"sessions":[{"session_id":"fake-full-e2e-session","started_at":"2099-01-01T00:00:00Z","model":"gpt-codex-fixture"}]}"#
                .to_string(),
        ),
    ]);

    let output = codex_blackbox_with_env(&["ui", "status", "--json", "--url", &url], &envs);

    assert!(output.status.success(), "stderr:\n{}", stderr(&output));
    let requests = captured_requests(request_rx, 2);
    assert!(requests[0].starts_with("POST /api/observations/codex "));
    assert!(requests[1].starts_with("GET /api/sessions?limit=20&days=30 "));
    let value: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("status json");
    assert_eq!(
        value.get("state").and_then(|v| v.as_str()),
        Some("configured_unobserved")
    );
    assert_eq!(
        value
            .get("observed_codex_responses")
            .and_then(|v| v.as_bool()),
        Some(false)
    );

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn ui_status_json_reports_recent_observed_traffic_from_core_fixtures() {
    let dir = unique_test_dir("ui-status-recent");
    let config_path = dir.join("config.toml");
    let state_dir = dir.join("state");
    let envs = [
        (
            "CODEX_BLACKBOX_CODEX_CONFIG",
            config_path.to_str().expect("utf8 config"),
        ),
        (
            "CODEX_BLACKBOX_UI_STATE_DIR",
            state_dir.to_str().expect("utf8 state"),
        ),
        ("CODEX_BLACKBOX_UI_SKIP_PROCESS_DETECTION", "1"),
    ];
    let enable = codex_blackbox_with_env(&["ui", "enable"], &envs);
    assert!(enable.status.success(), "stderr:\n{}", stderr(&enable));
    let (url, request_rx) = serve_response_sequence(vec![
        (
            200,
            r#"{"provider":"codex_responses","request_count":1,"latest_request_rowid":9,"matched":true}"#
                .to_string(),
        ),
        (
            200,
            r#"{"sessions":[{"session_id":"session_ui","started_at":"2099-01-01T00:00:00Z"}]}"#
                .to_string(),
        ),
    ]);

    let output = codex_blackbox_with_env(
        &[
            "ui",
            "status",
            "--json",
            "--url",
            &url,
            "--recent-seconds",
            "9999999999",
        ],
        &envs,
    );

    assert!(output.status.success(), "stderr:\n{}", stderr(&output));
    let requests = captured_requests(request_rx, 2);
    assert!(requests[0].starts_with("POST /api/observations/codex "));
    assert!(requests[1].starts_with("GET /api/sessions?limit=20&days=30 "));
    let value: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("status json");
    assert_eq!(
        value.get("state").and_then(|v| v.as_str()),
        Some("observing_recent_ui_traffic")
    );
    assert_eq!(
        value
            .get("observed_codex_responses")
            .and_then(|v| v.as_bool()),
        Some(true)
    );

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn ui_launch_dry_run_renders_safe_platform_action() {
    let output = codex_blackbox(&["ui", "launch", "--dry-run"]);

    assert!(output.status.success(), "stderr:\n{}", stderr(&output));
    let out = stdout(&output);
    assert!(out.contains("Codex Blackbox UI launch preview"));
    assert!(
        out.contains("No processes will be killed or restarted") || out.contains("unsupported")
    );
    assert!(!out.to_ascii_lowercase().contains("kill -"));
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
fn coach_install_status_and_uninstall_round_trip_through_temp_hooks_file() {
    let dir = unique_test_dir("coach-hooks");
    let hooks_file = dir.join(".codex").join("hooks.json");

    let preview = codex_blackbox(&[
        "coach",
        "preview",
        "--hooks-file",
        hooks_file.to_str().expect("utf8 hooks"),
    ]);
    assert!(preview.status.success(), "stderr:\n{}", stderr(&preview));
    assert!(!hooks_file.exists());
    assert!(stdout(&preview).contains("PreToolUse"));

    let install = codex_blackbox(&[
        "coach",
        "install",
        "--hooks-file",
        hooks_file.to_str().expect("utf8 hooks"),
    ]);
    assert!(install.status.success(), "stderr:\n{}", stderr(&install));
    let installed = fs::read_to_string(&hooks_file).expect("read hooks");
    assert!(installed.contains("codex-blackbox coach handle"));
    assert!(installed.contains("UserPromptSubmit"));

    let status = codex_blackbox(&[
        "coach",
        "status",
        "--json",
        "--hooks-file",
        hooks_file.to_str().expect("utf8 hooks"),
    ]);
    assert!(status.status.success(), "stderr:\n{}", stderr(&status));
    let value: serde_json::Value = serde_json::from_str(&stdout(&status)).expect("status json");
    assert_eq!(
        value.get("installed_handlers").and_then(|v| v.as_u64()),
        Some(4)
    );

    let uninstall = codex_blackbox(&[
        "coach",
        "uninstall",
        "--hooks-file",
        hooks_file.to_str().expect("utf8 hooks"),
    ]);
    assert!(
        uninstall.status.success(),
        "stderr:\n{}",
        stderr(&uninstall)
    );
    assert!(!fs::read_to_string(&hooks_file)
        .expect("read hooks")
        .contains("codex-blackbox coach handle"));
}

#[test]
fn coach_handle_posts_hook_payload_and_fails_open_with_json() {
    let (url, request_rx) = serve_json_once(
        r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"blocked"}}"#,
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_codex-blackbox"))
        .args(["coach", "handle", "--url", &url])
        .env("NO_COLOR", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn coach handle");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(br#"{"hook_event_name":"PreToolUse","tool_name":"Bash"}"#)
        .expect("write stdin");
    let output = child.wait_with_output().expect("coach output");
    assert!(output.status.success(), "stderr:\n{}", stderr(&output));
    let request = captured_request(request_rx);
    assert!(request.starts_with("POST /api/coach/hook "));
    assert!(stdout(&output).contains("permissionDecision"));
}

#[test]
fn baseline_disable_show_and_reset_are_derived_only() {
    let dir = unique_test_dir("baseline");
    let state = dir.join("baseline.json");

    let disable = codex_blackbox(&[
        "baseline",
        "disable",
        "--state",
        state.to_str().expect("utf8 baseline"),
    ]);
    assert!(disable.status.success(), "stderr:\n{}", stderr(&disable));
    let raw = fs::read_to_string(&state).expect("read baseline");
    assert!(raw.contains("derived_only"));
    for forbidden in [
        "raw_prompt",
        "raw_output",
        "raw_command",
        "raw_path",
        "secret_value",
    ] {
        assert!(
            !raw.contains(&format!("\"{forbidden}\"")),
            "baseline must not store field {forbidden}"
        );
    }

    let show = codex_blackbox(&[
        "baseline",
        "show",
        "--json",
        "--state",
        state.to_str().expect("utf8 baseline"),
    ]);
    assert!(show.status.success(), "stderr:\n{}", stderr(&show));
    let value: serde_json::Value = serde_json::from_str(&stdout(&show)).expect("baseline json");
    assert_eq!(value.get("enabled").and_then(|v| v.as_bool()), Some(false));

    let reset = codex_blackbox(&[
        "baseline",
        "reset",
        "--state",
        state.to_str().expect("utf8 baseline"),
    ]);
    assert!(reset.status.success(), "stderr:\n{}", stderr(&reset));
    assert!(!state.exists());
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
          "flight_recorder": [
            {
              "turn": 2,
              "timestamp": "2026-04-30T12:00:05Z",
              "status": "incomplete",
              "requested_model": "gpt-5.5",
              "served_model": "gpt-5.5",
              "model_mismatch": false,
              "input_tokens": 900,
              "cached_input_tokens": 300,
              "uncached_input_tokens": 600,
              "output_tokens": 64,
              "reasoning_output_tokens": 16,
              "local_total_tokens": 964,
              "context_fill_percent": 12.5,
              "context_window_tokens": 7200,
              "duration_ms": 42
            }
          ],
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
    assert!(out.contains("[ Flight Recorder ]"));
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
    assert!(out.contains("Turn 2"));
    assert!(out.contains("incomplete"));
    assert!(out.contains("12.5%"));
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
          "flight_recorder": [
            {
              "turn": 1,
              "timestamp": "2026-04-30T12:00:01Z",
              "status": "completed",
              "requested_model": "gpt-5.5",
              "served_model": "gpt-5.4",
              "model_mismatch": true,
              "input_tokens": 10,
              "cached_input_tokens": 4,
              "uncached_input_tokens": 6,
              "output_tokens": 2,
              "reasoning_output_tokens": 0,
              "local_total_tokens": 12,
              "context_fill_percent": null,
              "context_window_tokens": null,
              "duration_ms": 5
            }
          ],
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
    assert!(markdown.contains("## Flight Recorder"));
    assert!(markdown.contains("gpt-5.5->gpt-5.4"));
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
