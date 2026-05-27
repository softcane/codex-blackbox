use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

use crate::{ui_config, ui_launch, ui_process, ui_status};

pub(crate) fn render_enable_preview(paths: &ui_config::UiConfigPaths) -> String {
    format!(
        "Codex Blackbox UI enable preview\nStatus: experimental local Codex Desktop/IDE app-server mode.\nConfig file: {}\nState directory: {}\nDry run: no files modified\n\n{}\nEvidence rule: fake fixtures prove local contracts only; live UI support requires observed provider=\"codex_responses\" traffic from a real Desktop/IDE smoke.\n",
        paths.config_path.display(),
        paths.state_dir.display(),
        ui_config::target_config_toml()
    )
}

pub(crate) fn run_enable(
    config: Option<PathBuf>,
    state_dir: Option<PathBuf>,
    dry_run: bool,
    force: bool,
) -> i32 {
    let paths = match ui_config::UiConfigPaths::resolve(config, state_dir) {
        Ok(paths) => paths,
        Err(err) => {
            eprintln!("Error: {err}");
            return 1;
        }
    };
    if dry_run {
        print!("{}", render_enable_preview(&paths));
        return 0;
    }

    match ui_config::enable(&paths, ui_config::EnableOptions { force }) {
        Ok(outcome) => {
            if outcome.changed {
                println!("Codex Blackbox UI mode enabled.");
            } else {
                println!("Codex Blackbox UI mode was already enabled.");
            }
            println!("Config file: {}", paths.config_path.display());
            println!("Backup: {}", outcome.backup_path.display());
            println!("State: {}", outcome.state_path.display());
            println!("Restart local Codex Desktop/IDE app-server processes to pick up config.");
            0
        }
        Err(err) => {
            eprintln!("Error: {err}");
            1
        }
    }
}

pub(crate) fn run_disable(config: Option<PathBuf>, state_dir: Option<PathBuf>) -> i32 {
    let paths = match ui_config::UiConfigPaths::resolve(config, state_dir) {
        Ok(paths) => paths,
        Err(err) => {
            eprintln!("Error: {err}");
            return 1;
        }
    };
    match ui_config::disable(&paths) {
        Ok(outcome) => {
            if outcome.changed {
                println!("Codex Blackbox UI mode disabled.");
            } else {
                println!("Codex Blackbox UI mode is not enabled by Blackbox state.");
            }
            println!("Config file: {}", paths.config_path.display());
            println!("State: {}", outcome.state_path.display());
            0
        }
        Err(err) => {
            eprintln!("Error: {err}");
            1
        }
    }
}

#[derive(Debug, Deserialize)]
struct UiObservationSnapshot {
    #[serde(default)]
    request_count: u64,
}

#[derive(Debug, Deserialize)]
struct UiSessionsResponse {
    #[serde(default)]
    sessions: Vec<UiSessionSummary>,
}

#[derive(Debug, Deserialize)]
struct UiSessionSummary {
    session_id: Option<String>,
    started_at: Option<String>,
    model: Option<String>,
}

#[derive(Debug, Serialize)]
struct UiStatusReport {
    state: ui_status::UiStatusState,
    config: ui_config::UiConfigInspection,
    core_ready: bool,
    observed_codex_responses: bool,
    latest_observed_age_seconds: Option<u64>,
    recent_http_responses_post: bool,
    recent_websocket_upgrade_required: bool,
    active_app_server_processes: Vec<ui_process::CodexUiProcess>,
    evidence: String,
    caveat: String,
}

#[derive(Debug, Serialize)]
struct UiDoctorReport {
    config: ui_config::UiConfigInspection,
    core_ready: bool,
    active_app_server_processes: Vec<ui_process::CodexUiProcess>,
    restart_required: bool,
    experimental: bool,
    scope: String,
    evidence_rule: String,
}

pub(crate) async fn run_doctor(
    config: Option<PathBuf>,
    state_dir: Option<PathBuf>,
    url: String,
    json: bool,
) -> i32 {
    match build_doctor_report(config, state_dir, &url).await {
        Ok(report) => {
            if json {
                match serde_json::to_string_pretty(&report) {
                    Ok(text) => println!("{text}"),
                    Err(err) => {
                        eprintln!("Error: failed to encode UI doctor JSON: {err}");
                        return 1;
                    }
                }
            } else {
                print_doctor_report(&report);
            }
            0
        }
        Err(err) => {
            eprintln!("Error: {err}");
            1
        }
    }
}

async fn build_doctor_report(
    config: Option<PathBuf>,
    state_dir: Option<PathBuf>,
    url: &str,
) -> Result<UiDoctorReport, String> {
    let paths = ui_config::UiConfigPaths::resolve(config, state_dir)?;
    let config = ui_config::inspect(&paths)?;
    let core_ready = health_check(&format!("{}/health", url.trim_end_matches('/'))).await;
    let active_app_server_processes = detect_processes_for_cli();
    let restart_required = !active_app_server_processes.is_empty();
    Ok(UiDoctorReport {
        config,
        core_ready,
        active_app_server_processes,
        restart_required,
        experimental: true,
        scope: "local Codex Desktop and local IDE extension app-server traffic only".to_string(),
        evidence_rule:
            "live UI support requires real provider=\"codex_responses\" traffic observed by core"
                .to_string(),
    })
}

fn print_doctor_report(report: &UiDoctorReport) {
    println!("Codex Blackbox UI doctor");
    println!("Status: experimental local Desktop/IDE app-server mode");
    println!("Config: {:?}", report.config.status);
    println!(
        "Core: {}",
        if report.core_ready {
            "ready"
        } else {
            "not ready"
        }
    );
    if report.restart_required {
        println!("Restart required: active local Codex UI/app-server process detected.");
    }
    println!("Scope: {}", report.scope);
    println!("Evidence: {}", report.evidence_rule);
}

pub(crate) async fn run_status(
    config: Option<PathBuf>,
    state_dir: Option<PathBuf>,
    url: String,
    json: bool,
    recent_seconds: u64,
) -> i32 {
    match build_status_report(config, state_dir, &url, recent_seconds).await {
        Ok(report) => {
            if json {
                match serde_json::to_string_pretty(&report) {
                    Ok(text) => println!("{text}"),
                    Err(err) => {
                        eprintln!("Error: failed to encode UI status JSON: {err}");
                        return 1;
                    }
                }
            } else {
                print_status_report(&report);
            }
            0
        }
        Err(err) => {
            eprintln!("Error: {err}");
            1
        }
    }
}

async fn build_status_report(
    config: Option<PathBuf>,
    state_dir: Option<PathBuf>,
    url: &str,
    recent_seconds: u64,
) -> Result<UiStatusReport, String> {
    let paths = ui_config::UiConfigPaths::resolve(config, state_dir)?;
    let config = ui_config::inspect(&paths)?;
    let config_state = status_config_state(&config.status);
    let should_query_core = config.status != ui_config::UiConfigStatus::NotConfigured;
    let (core_ready, observed_codex_responses, latest_observed_age_seconds) = if should_query_core {
        let observation = fetch_observation_snapshot(url).await.ok();
        let any_observed = observation
            .as_ref()
            .is_some_and(|snapshot| snapshot.request_count > 0);
        let latest_age = if any_observed {
            fetch_latest_non_fixture_session_age_seconds(url)
                .await
                .ok()
                .flatten()
        } else {
            None
        };
        let observed_after_enable =
            latest_age.is_some_and(|age| latest_observation_is_after_enable(age, &config));
        (
            observation.is_some(),
            any_observed && observed_after_enable,
            latest_age.filter(|age| latest_observation_is_after_enable(*age, &config)),
        )
    } else {
        (false, false, None)
    };
    let active_app_server_processes = detect_processes_for_cli();
    let recent_envoy_ui_traffic = if observed_codex_responses {
        RecentEnvoyUiTraffic::default()
    } else {
        detect_recent_envoy_ui_traffic(recent_seconds)
    };
    let state = ui_status::classify(&ui_status::UiStatusInput {
        config_state,
        observed_codex_responses,
        latest_observed_age_seconds,
        recent_http_responses_post: recent_envoy_ui_traffic.http_responses_post,
        recent_websocket_upgrade_required: recent_envoy_ui_traffic.websocket_upgrade_required,
        active_app_server_processes: !active_app_server_processes.is_empty(),
        recent_threshold_seconds: recent_seconds,
    });
    Ok(UiStatusReport {
        state,
        config,
        core_ready,
        observed_codex_responses,
        latest_observed_age_seconds,
        recent_http_responses_post: recent_envoy_ui_traffic.http_responses_post,
        recent_websocket_upgrade_required: recent_envoy_ui_traffic.websocket_upgrade_required,
        active_app_server_processes,
        evidence: "Envoy-observed provider=\"codex_responses\" traffic only".to_string(),
        caveat: "Experimental local Desktop/IDE app-server mode; fake fixtures are not live UI support proof.".to_string(),
    })
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RecentEnvoyUiTraffic {
    http_responses_post: bool,
    websocket_upgrade_required: bool,
}

fn detect_recent_envoy_ui_traffic(recent_seconds: u64) -> RecentEnvoyUiTraffic {
    if let Some(fixture) = std::env::var_os("CODEX_BLACKBOX_UI_ENVOY_LOG_FIXTURE") {
        return recent_envoy_ui_traffic_from_logs(&fixture.to_string_lossy());
    }
    if std::env::var_os("CODEX_BLACKBOX_UI_SKIP_ENVOY_LOG_DETECTION").is_some() {
        return RecentEnvoyUiTraffic::default();
    }

    let Some(container_id) = find_envoy_container_id() else {
        return RecentEnvoyUiTraffic::default();
    };
    let since = format!("{}s", recent_seconds.max(1));
    let Ok(output) = Command::new("docker")
        .args(["logs", "--since", since.as_str(), container_id.as_str()])
        .stdin(Stdio::null())
        .output()
    else {
        return RecentEnvoyUiTraffic::default();
    };
    if !output.status.success() {
        return RecentEnvoyUiTraffic::default();
    }

    let mut logs = String::from_utf8_lossy(&output.stdout).into_owned();
    logs.push_str(&String::from_utf8_lossy(&output.stderr));
    recent_envoy_ui_traffic_from_logs(&logs)
}

fn find_envoy_container_id() -> Option<String> {
    let output = Command::new("docker")
        .args([
            "ps",
            "--filter",
            "label=com.docker.compose.project=codex-blackbox",
            "--filter",
            "label=com.docker.compose.service=envoy",
            "--format",
            "{{.ID}}",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

fn recent_envoy_ui_traffic_from_logs(logs: &str) -> RecentEnvoyUiTraffic {
    logs.lines()
        .fold(RecentEnvoyUiTraffic::default(), |mut traffic, line| {
            let line_traffic = envoy_ui_traffic_from_log_line(line);
            traffic.http_responses_post |= line_traffic.http_responses_post;
            traffic.websocket_upgrade_required |= line_traffic.websocket_upgrade_required;
            traffic
        })
}

fn envoy_ui_traffic_from_log_line(line: &str) -> RecentEnvoyUiTraffic {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) {
        let method = value
            .get("method")
            .and_then(|method| method.as_str())
            .unwrap_or_default();
        let path = value
            .get("path")
            .and_then(|path| path.as_str())
            .unwrap_or_default();
        let upgrade = value
            .get("upgrade")
            .and_then(|upgrade| upgrade.as_str())
            .unwrap_or_default();
        let status_426 = value
            .get("status")
            .is_some_and(|status| status.as_u64() == Some(426) || status.as_str() == Some("426"));
        let websocket_upgrade = upgrade.is_empty() || upgrade.eq_ignore_ascii_case("websocket");
        return RecentEnvoyUiTraffic {
            http_responses_post: method == "POST" && path == "/backend-api/codex/responses",
            websocket_upgrade_required: method == "GET"
                && status_426
                && websocket_upgrade
                && path.starts_with("/backend-api/codex/responses"),
        };
    }

    RecentEnvoyUiTraffic {
        http_responses_post: line.contains("\"method\":\"POST\"")
            && line.contains("\"path\":\"/backend-api/codex/responses\""),
        websocket_upgrade_required: line.contains("\"method\":\"GET\"")
            && line.contains("\"status\":426")
            && line.contains("\"path\":\"/backend-api/codex/responses"),
    }
}

fn detect_processes_for_cli() -> Vec<ui_process::CodexUiProcess> {
    if std::env::var_os("CODEX_BLACKBOX_UI_SKIP_PROCESS_DETECTION").is_some() {
        return Vec::new();
    }
    if let Some(fixture) = std::env::var_os("CODEX_BLACKBOX_UI_PROCESS_FIXTURE") {
        return ui_process::parse_ps_output(&fixture.to_string_lossy());
    }
    ui_process::detect_codex_ui_processes()
}

fn status_config_state(status: &ui_config::UiConfigStatus) -> ui_status::UiConfigState {
    match status {
        ui_config::UiConfigStatus::NotConfigured => ui_status::UiConfigState::NotConfigured,
        ui_config::UiConfigStatus::Configured => ui_status::UiConfigState::Configured,
        ui_config::UiConfigStatus::Misconfigured => ui_status::UiConfigState::Misconfigured,
    }
}

async fn fetch_observation_snapshot(base_url: &str) -> Result<UiObservationSnapshot, String> {
    let url = format!("{}/api/observations/codex", base_url.trim_end_matches('/'));
    let resp = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .map_err(|err| format!("failed to build UI status client: {err}"))?
        .post(&url)
        .json(&serde_json::json!({"after_request_rowid": 0}))
        .send()
        .await
        .map_err(|err| format!("failed to fetch {url}: {err}"))?;
    if !resp.status().is_success() {
        return Err(format!("failed to fetch {url}: HTTP {}", resp.status()));
    }
    resp.json::<UiObservationSnapshot>()
        .await
        .map_err(|err| format!("failed to parse {url}: {err}"))
}

async fn fetch_latest_non_fixture_session_age_seconds(
    base_url: &str,
) -> Result<Option<u64>, String> {
    let url = format!(
        "{}/api/sessions?limit=20&days=30",
        base_url.trim_end_matches('/')
    );
    let resp = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .map_err(|err| format!("failed to build UI sessions client: {err}"))?
        .get(&url)
        .send()
        .await
        .map_err(|err| format!("failed to fetch {url}: {err}"))?;
    if !resp.status().is_success() {
        return Err(format!("failed to fetch {url}: HTTP {}", resp.status()));
    }
    let body = resp
        .json::<UiSessionsResponse>()
        .await
        .map_err(|err| format!("failed to parse {url}: {err}"))?;
    Ok(body
        .sessions
        .iter()
        .filter(|session| !session.looks_like_local_fixture())
        .find_map(|session| session.started_at.as_deref().and_then(observed_age_seconds)))
}

impl UiSessionSummary {
    fn looks_like_local_fixture(&self) -> bool {
        self.session_id
            .as_deref()
            .is_some_and(|session_id| session_id.starts_with("fake-"))
            || self
                .model
                .as_deref()
                .is_some_and(|model| model.contains("fixture") || model.contains("fake"))
    }
}

async fn health_check(url: &str) -> bool {
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
    else {
        return false;
    };

    client
        .get(url)
        .send()
        .await
        .map(|resp| resp.status().is_success())
        .unwrap_or(false)
}

fn observed_age_seconds(iso: &str) -> Option<u64> {
    let observed = DateTime::parse_from_rfc3339(iso).ok()?;
    let age = Local::now()
        .signed_duration_since(observed.with_timezone(&Local))
        .num_seconds();
    Some(age.max(0) as u64)
}

fn latest_observation_is_after_enable(
    latest_observed_age_seconds: u64,
    config: &ui_config::UiConfigInspection,
) -> bool {
    let Some(enabled_at) = config.enabled_at_epoch_seconds else {
        return true;
    };
    let now = Local::now().timestamp().max(0) as u64;
    let observed_at = now.saturating_sub(latest_observed_age_seconds);
    observed_at.saturating_add(1) >= enabled_at
}

fn print_status_report(report: &UiStatusReport) {
    let state = serde_json::to_value(&report.state)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string());
    println!("codex-blackbox ui: {state}");
    println!("Config: {}", report.config.config_path);
    println!("Evidence: {}", report.evidence);
    if report.state == ui_status::UiStatusState::HttpResponsesUnparsed {
        println!("HTTP Responses: recent POST /backend-api/codex/responses reached Envoy.");
        println!(
            "Result: core did not persist provider=\"codex_responses\" evidence; request parsing or compression needs investigation."
        );
    } else if report.state == ui_status::UiStatusState::WebsocketOnlyUnobservable {
        println!("WebSocket: recent 426 GET /backend-api/codex/responses attempts observed.");
        println!(
            "Result: current safe UI mode cannot observe those turns because Codex UI did not fall back to HTTP Responses."
        );
    } else if !report.active_app_server_processes.is_empty() {
        println!("Restart required: active local Codex UI/app-server process detected.");
    }
    println!("{}", report.caveat);
}

#[cfg(test)]
mod tests {
    #[test]
    fn envoy_log_detection_finds_websocket_upgrade_required_access_log() {
        let logs = r#"{"status":426,"method":"GET","path":"/backend-api/codex/responses","upgrade":"websocket"}"#;

        assert_eq!(
            super::recent_envoy_ui_traffic_from_logs(logs),
            super::RecentEnvoyUiTraffic {
                http_responses_post: false,
                websocket_upgrade_required: true,
            }
        );
    }

    #[test]
    fn envoy_log_detection_finds_http_model_turn_posts() {
        let logs =
            r#"{"status":200,"method":"POST","path":"/backend-api/codex/responses","upgrade":""}"#;

        assert_eq!(
            super::recent_envoy_ui_traffic_from_logs(logs),
            super::RecentEnvoyUiTraffic {
                http_responses_post: true,
                websocket_upgrade_required: false,
            }
        );
    }

    #[test]
    fn envoy_log_detection_does_not_count_compact_as_model_turn_post() {
        let logs =
            r#"{"status":200,"method":"POST","path":"/backend-api/codex/responses/compact"}"#;

        assert_eq!(
            super::recent_envoy_ui_traffic_from_logs(logs),
            super::RecentEnvoyUiTraffic::default()
        );
    }
}

pub(crate) fn run_launch(dry_run: bool) -> i32 {
    let plan = ui_launch::platform_launch_plan();
    if dry_run {
        print!("{}", ui_launch::render_launch_plan(&plan));
        return 0;
    }
    match ui_launch::execute_launch_plan(&plan) {
        Ok(()) => {
            println!("Codex UI launch command completed.");
            println!("No processes were killed or restarted by Codex Blackbox.");
            0
        }
        Err(err) => {
            eprintln!("Error: {err}");
            1
        }
    }
}
