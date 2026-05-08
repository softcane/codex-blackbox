mod tmux;

use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead, BufReader, ErrorKind, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use chrono::{DateTime, Local};
use clap::{Parser, Subcommand};
use colored::Colorize;
use serde::{Deserialize, Serialize};

pub(crate) const WATCH_RECONNECT_DELAY: Duration = Duration::from_secs(3);

#[derive(Debug, Default)]
pub(crate) struct WatchRetryLog {
    last_error: Option<String>,
}

impl WatchRetryLog {
    pub(crate) fn retry_message(&mut self, error: impl std::fmt::Display) -> Option<String> {
        let error = error.to_string();
        if self.last_error.as_deref() == Some(error.as_str()) {
            return None;
        }

        self.last_error = Some(error.clone());
        Some(format!(
            "Waiting for codex-blackbox-core... (retrying every {}s; {})",
            WATCH_RECONNECT_DELAY.as_secs(),
            error
        ))
    }

    pub(crate) fn reset(&mut self) {
        self.last_error = None;
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "codex-blackbox",
    version,
    about = "Codex Blackbox observability proxy. Codex subscription wrapper is experimental."
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Check local developer prerequisites and stack health
    Doctor,

    /// Start the local Codex Blackbox stack with the ChatGPT/Codex Envoy proxy.
    Up {
        /// Start without Grafana once compose profiles support it
        #[arg(long)]
        no_grafana: bool,
    },

    /// Run a command through Codex Blackbox. Codex uses experimental subscription proxy overrides.
    Run {
        /// Start codex-blackbox watch alongside the child command
        #[arg(long)]
        watch: bool,

        /// Print the constructed child command without running it
        #[arg(long)]
        dry_run: bool,

        /// Command and arguments to run
        #[arg(required = true, num_args = 1.., trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },

    /// Live stream Codex Blackbox watch events
    Watch {
        /// Base URL of codex-blackbox-core
        #[arg(long, default_value = "http://localhost:9091")]
        url: String,

        /// Hide frustration signal events
        #[arg(long)]
        no_signals: bool,

        /// Filter to a specific session ID
        #[arg(long)]
        session: Option<String>,

        /// Split each session into its own tmux pane
        #[arg(long, conflicts_with = "session")]
        tmux: bool,

        /// Max tmux panes before refusing new sessions
        #[arg(long, default_value = "8")]
        tmux_max_panes: usize,
    },

    /// Show recent sessions
    Sessions {
        /// Base URL of codex-blackbox-core
        #[arg(long, default_value = "http://localhost:9091")]
        url: String,

        /// Number of sessions to show
        #[arg(long, default_value = "20")]
        limit: u32,

        /// Days to look back
        #[arg(long, default_value = "7")]
        days: u32,
    },

    /// Search across past session prompts and final summaries
    Recall {
        /// Base URL of codex-blackbox-core
        #[arg(long, default_value = "http://localhost:9091")]
        url: String,

        /// Number of matches to show
        #[arg(long, default_value = "5")]
        limit: u32,

        /// Days to look back
        #[arg(long, default_value = "30")]
        days: u32,

        /// Search query
        #[arg(required = true, num_args = 1..)]
        query: Vec<String>,
    },

    /// Render a deterministic Codex Responses postmortem report
    Postmortem {
        /// Base URL of codex-blackbox-core
        #[arg(long, default_value = "http://localhost:9091")]
        url: String,

        /// Show unredacted local evidence
        #[arg(long)]
        no_redact: bool,

        /// Write markdown to a file instead of stdout
        #[arg(long)]
        output: Option<PathBuf>,

        /// "last" or a specific session ID
        target: String,
    },

    /// Import a billed-cost reconciliation for a session
    Reconcile {
        /// Base URL of codex-blackbox-core
        #[arg(long, default_value = "http://localhost:9091")]
        url: String,

        /// Session ID to reconcile
        #[arg(long)]
        session: String,

        /// Billed cost in USD
        #[arg(long)]
        billed_cost: f64,

        /// Billing source label, e.g. invoice_2026q2
        #[arg(long)]
        source: String,

        /// Optional import timestamp in UTC ISO 8601
        #[arg(long)]
        imported_at: Option<String>,
    },

    /// Print read-only configuration previews
    Config {
        #[command(subcommand)]
        command: ConfigCommands,
    },

    /// Manual preflight checks for live smoke tests; never launches Codex turns
    Preflight {
        #[command(subcommand)]
        command: PreflightCommands,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigCommands {
    /// Print the Codex proxy configuration used by the run wrapper without applying it
    Codex,
}

#[derive(Debug, Subcommand)]
enum PreflightCommands {
    /// Verify local ChatGPT login, start subscription proxy stack, and print the live command
    CodexSubscription {
        /// Codex command to show, e.g. -- codex exec "prompt"
        #[arg(required = true, num_args = 1.., trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum WatchEvent {
    ToolUse {
        session_id: String,
        timestamp: String,
        tool_name: String,
        summary: String,
    },
    SessionStart {
        session_id: String,
        display_name: String,
        model: String,
        #[serde(default)]
        initial_prompt: Option<String>,
    },
    SessionEnd {
        session_id: String,
        outcome: String,
        total_tokens: u64,
        total_turns: u32,
    },
    FrustrationSignal {
        session_id: String,
        signal_type: String,
    },
    CompactionLoop {
        session_id: String,
        consecutive: u32,
        wasted_tokens: u64,
    },
    Diagnosis {
        session_id: String,
        report: DiagnosisReport,
    },
    ModelFallback {
        session_id: String,
        requested: String,
        actual: String,
    },
    CodexTurnSummary {
        session_id: String,
        status: String,
        requested_model: String,
        #[serde(default)]
        served_model: Option<String>,
        input_tokens: u64,
        cached_input_tokens: u64,
        uncached_input_tokens: u64,
        output_tokens: u64,
        reasoning_output_tokens: u64,
        total_tokens: u64,
    },
    ContextStatus {
        session_id: String,
        fill_percent: f64,
        #[allow(dead_code)]
        #[serde(default)]
        context_window_tokens: Option<u64>,
        #[serde(default)]
        turns_to_compact: Option<u32>,
    },
    // For the "lagged" pseudo-event from the server.
    #[serde(rename = "lagged")]
    Lagged { missed: u64 },
}

#[derive(Debug, Deserialize)]
struct DiagnosisReport {
    outcome: String,
    total_turns: u32,
    total_tokens: u64,
    #[allow(dead_code)]
    #[serde(default)]
    estimated_total_cost_dollars: Option<f64>,
    #[allow(dead_code)]
    cost_source: Option<String>,
    #[serde(default)]
    degraded: bool,
    degradation_turn: Option<u32>,
    causes: Vec<DegradationCause>,
    advice: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct DegradationCause {
    turn_first_noticed: u32,
    cause_type: String,
    detail: String,
    #[allow(dead_code)]
    estimated_cost: f64,
    is_heuristic: bool,
}

#[derive(Debug, Serialize)]
struct BillingReconciliationInput {
    session_id: String,
    source: String,
    billed_cost_dollars: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    imported_at: Option<String>,
}

const ENVOY_PROXY_URL: &str = "http://127.0.0.1:10000";
const CODEX_BLACKBOX_CORE_URL: &str = "http://127.0.0.1:9091";
const CODEX_BLACKBOX_CORE_HEALTH_URL: &str = "http://127.0.0.1:9091/health";
const GRAFANA_URL: &str = "http://127.0.0.1:3000";
const GRAFANA_DASHBOARD_URL: &str = "http://127.0.0.1:3000/d/codex-blackbox-main";
const CHATGPT_CODEX_BACKEND_PATH: &str = "/backend-api";
const CODEX_MODEL_BACKEND_PATH: &str = "/backend-api/codex";
const CODEX_MODEL_PROVIDER_ID: &str = "codex-blackbox-chatgpt";
const CODEX_REQUEST_COMPRESSION_FEATURE: &str = "enable_request_compression";
const CODEX_PARENT_SESSION_ENV_REMOVALS: &[&str] = &[
    "CODEX_CI",
    "CODEX_INTERNAL_ORIGINATOR_OVERRIDE",
    "CODEX_SHELL",
    "CODEX_THREAD_ID",
];
const DEFAULT_CORE_IMAGE: &str = concat!(
    "ghcr.io/softcane/codex-blackbox-core:v",
    env!("CARGO_PKG_VERSION")
);
const BUNDLED_ENVOY_YAML: &str = include_str!("../../envoy/envoy.yaml");
const BUNDLED_PROMETHEUS_YAML: &str = include_str!("../../prometheus/prometheus.yml");
const BUNDLED_GRAFANA_DASHBOARD_PROVIDER_YAML: &str =
    include_str!("../../grafana/provisioning/dashboards/codex-blackbox.yml");
const BUNDLED_GRAFANA_PROMETHEUS_DATASOURCE_YAML: &str =
    include_str!("../../grafana/provisioning/datasources/prometheus.yml");
const BUNDLED_GRAFANA_DASHBOARD_JSON: &str =
    include_str!("../../grafana/dashboards/codex-blackbox.json");

#[derive(Debug, Clone)]
struct ComposeCommand {
    program: String,
    args: Vec<String>,
    display: String,
}

#[derive(Debug)]
enum PortState {
    Available,
    CodexBlackboxService(String),
    Busy,
}

#[derive(Debug)]
enum WatchHandle {
    Plain(Child),
    TmuxSession(String),
}

impl WatchHandle {
    fn stop(&mut self) {
        match self {
            WatchHandle::Plain(child) => {
                let _ = child.kill();
                let _ = child.wait();
            }
            WatchHandle::TmuxSession(session) => {
                let _ = Command::new("tmux")
                    .args(["kill-session", "-t", session])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
        }
    }
}

fn envoy_proxy_url() -> String {
    std::env::var("CODEX_BLACKBOX_ENVOY_PROXY_URL").unwrap_or_else(|_| ENVOY_PROXY_URL.to_string())
}

fn codex_blackbox_core_url() -> String {
    std::env::var("CODEX_BLACKBOX_CORE_URL").unwrap_or_else(|_| CODEX_BLACKBOX_CORE_URL.to_string())
}

fn command_exists(name: &str) -> bool {
    command_path(name).is_some()
}

fn command_path(name: &str) -> Option<PathBuf> {
    let path = Path::new(name);
    if path.components().count() > 1 {
        return is_executable_file(path).then(|| path.to_path_buf());
    }

    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        let candidate = dir.join(name);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn run_quiet(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn command_stdout(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn docker_daemon_running() -> bool {
    command_exists("docker") && run_quiet("docker", &["info"])
}

fn docker_compose_command() -> Option<ComposeCommand> {
    if command_exists("docker") && run_quiet("docker", &["compose", "version"]) {
        return Some(ComposeCommand {
            program: "docker".to_string(),
            args: vec!["compose".to_string()],
            display: "docker compose".to_string(),
        });
    }

    if run_quiet("docker-compose", &["version"]) {
        return Some(ComposeCommand {
            program: "docker-compose".to_string(),
            args: Vec::new(),
            display: "docker-compose".to_string(),
        });
    }

    None
}

fn is_codex_blackbox_repo_root(path: &Path) -> bool {
    path.join("codex-blackbox-core").is_dir() && path.join("envoy").is_dir()
}

fn find_repo_root() -> Option<PathBuf> {
    let mut starts = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        starts.push(cwd);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            starts.push(parent.to_path_buf());
        }
    }
    starts.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")));

    for start in starts {
        for ancestor in start.ancestors() {
            if is_codex_blackbox_repo_root(ancestor) {
                return Some(ancestor.to_path_buf());
            }
        }
    }

    None
}

fn find_repo_compose_file() -> Option<PathBuf> {
    let root = find_repo_root()?;
    for name in [
        "docker-compose.yml",
        "docker-compose.yaml",
        "compose.yaml",
        "compose.yml",
    ] {
        let candidate = root.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn codex_blackbox_data_dir() -> Result<PathBuf, String> {
    if let Some(dir) = std::env::var_os("CODEX_BLACKBOX_HOME") {
        return Ok(PathBuf::from(dir));
    }
    if let Some(dir) = std::env::var_os("XDG_DATA_HOME") {
        return Ok(PathBuf::from(dir).join("codex-blackbox"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        return Ok(PathBuf::from(home).join(".local/share/codex-blackbox"));
    }
    Err("Could not determine a data directory; set CODEX_BLACKBOX_HOME.".to_string())
}

fn yaml_quote(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn yaml_quote_volume(source: &Path, target: &str, mode: &str) -> String {
    yaml_quote(&format!("{}:{target}:{mode}", source.to_string_lossy()))
}

fn write_if_changed(path: &Path, contents: &str) -> Result<(), String> {
    if path.is_file() {
        if let Ok(existing) = fs::read_to_string(path) {
            if existing == contents {
                return Ok(());
            }
        }
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create {}: {}", parent.display(), err))?;
    }
    fs::write(path, contents).map_err(|err| format!("failed to write {}: {}", path.display(), err))
}

fn bundled_compose_yaml(stack_dir: &Path) -> String {
    let envoy_config = yaml_quote_volume(
        &stack_dir.join("envoy/envoy.yaml"),
        "/etc/envoy/envoy.yaml",
        "ro",
    );
    let prometheus_config = yaml_quote_volume(
        &stack_dir.join("prometheus/prometheus.yml"),
        "/etc/prometheus/prometheus.yml",
        "ro",
    );
    let grafana_provisioning = yaml_quote_volume(
        &stack_dir.join("grafana/provisioning"),
        "/etc/grafana/provisioning",
        "ro",
    );
    let grafana_dashboards = yaml_quote_volume(
        &stack_dir.join("grafana/dashboards"),
        "/var/lib/grafana/dashboards",
        "ro",
    );

    format!(
        r#"services:
  envoy:
    image: envoyproxy/envoy:v1.32-latest
    volumes:
      - {envoy_config}
    ports:
      - "127.0.0.1:10000:10000"
    depends_on:
      codex-blackbox-core:
        condition: service_healthy
    healthcheck:
      test:
        - CMD-SHELL
        - >
          bash -c 'exec 3<>/dev/tcp/localhost/9901;
          printf "GET /ready HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n" >&3;
          head -n 1 <&3 | grep -q " 200 "'
      interval: 5s
      timeout: 3s
      retries: 5

  codex-blackbox-core:
    image: ${{CODEX_BLACKBOX_CORE_IMAGE:-{DEFAULT_CORE_IMAGE}}}
    expose:
      - "50051"
    ports:
      - "127.0.0.1:9091:9090"
    environment:
      - RUST_LOG=info
      - CODEX_BLACKBOX_SESSION_BUDGET_DOLLARS=0
      - CODEX_BLACKBOX_SESSION_BUDGET_TOKENS=0
      - CODEX_BLACKBOX_CIRCUIT_BREAKER_THRESHOLD=5
    volumes:
      - codex_blackbox_data:/data
    healthcheck:
      test: ["CMD", "wget", "--spider", "-q", "http://localhost:9090/health"]
      interval: 5s
      timeout: 3s
      retries: 5

  prometheus:
    image: prom/prometheus:v2.52.0
    ports:
      - "127.0.0.1:9092:9090"
    volumes:
      - {prometheus_config}
      - prometheus_data:/prometheus
    command:
      - "--config.file=/etc/prometheus/prometheus.yml"
      - "--storage.tsdb.path=/prometheus"
      - "--storage.tsdb.retention.time=30d"
      - "--web.enable-lifecycle"
    depends_on:
      codex-blackbox-core:
        condition: service_healthy

  grafana:
    image: grafana/grafana:11.1.0
    ports:
      - "127.0.0.1:3000:3000"
    volumes:
      - {grafana_provisioning}
      - {grafana_dashboards}
      - grafana_data:/var/lib/grafana
    environment:
      - GF_AUTH_ANONYMOUS_ENABLED=true
      - GF_AUTH_ANONYMOUS_ORG_ROLE=Viewer
      - GF_SECURITY_ADMIN_USER=admin
      - GF_SECURITY_ADMIN_PASSWORD=admin
      - GF_USERS_ALLOW_SIGN_UP=false
    depends_on:
      - prometheus

volumes:
  codex_blackbox_data:
  prometheus_data:
  grafana_data:
"#
    )
}

fn prepare_bundled_stack() -> Result<PathBuf, String> {
    let stack_dir = codex_blackbox_data_dir()?
        .join("stack")
        .join(env!("CARGO_PKG_VERSION"));
    write_if_changed(&stack_dir.join("envoy/envoy.yaml"), BUNDLED_ENVOY_YAML)?;
    write_if_changed(
        &stack_dir.join("prometheus/prometheus.yml"),
        BUNDLED_PROMETHEUS_YAML,
    )?;
    write_if_changed(
        &stack_dir.join("grafana/provisioning/dashboards/codex-blackbox.yml"),
        BUNDLED_GRAFANA_DASHBOARD_PROVIDER_YAML,
    )?;
    write_if_changed(
        &stack_dir.join("grafana/provisioning/datasources/prometheus.yml"),
        BUNDLED_GRAFANA_PROMETHEUS_DATASOURCE_YAML,
    )?;
    write_if_changed(
        &stack_dir.join("grafana/dashboards/codex-blackbox.json"),
        BUNDLED_GRAFANA_DASHBOARD_JSON,
    )?;
    let compose_path = stack_dir.join("docker-compose.yml");
    write_if_changed(&compose_path, &bundled_compose_yaml(&stack_dir))?;
    Ok(compose_path)
}

fn resolve_compose_file() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("CODEX_BLACKBOX_COMPOSE_FILE") {
        let path = PathBuf::from(path);
        let path = if path.is_absolute() {
            path
        } else {
            std::env::current_dir()
                .map_err(|err| format!("failed to resolve current directory: {err}"))?
                .join(path)
        };
        if path.is_file() {
            return Ok(path);
        }
        return Err(format!(
            "CODEX_BLACKBOX_COMPOSE_FILE points to {}, but that file does not exist.",
            path.display()
        ));
    }

    let force_bundled = std::env::var_os("CODEX_BLACKBOX_USE_BUNDLED_STACK").is_some();
    if !force_bundled {
        if let Some(path) = find_repo_compose_file() {
            return Ok(path);
        }
    }

    prepare_bundled_stack()
}

fn is_port_available(port: u16) -> bool {
    let loopback_addrs = [
        SocketAddr::from(([127, 0, 0, 1], port)),
        SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], port)),
    ];

    for addr in loopback_addrs {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
            return false;
        }
    }

    let v4_free = TcpListener::bind(("127.0.0.1", port)).is_ok();
    let v6_free = match TcpListener::bind(("::1", port)) {
        Ok(_) => true,
        Err(err) if err.kind() == ErrorKind::AddrNotAvailable => true,
        Err(_) => false,
    };

    v4_free && v6_free
}

fn codex_blackbox_container_for_port(port: u16) -> Option<String> {
    let output = Command::new("docker")
        .args(["ps", "--format", "{{.Names}}\t{{.Ports}}"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let marker = format!(":{port}->");
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let Some((name, ports)) = line.split_once('\t') else {
            continue;
        };
        if name.to_ascii_lowercase().contains("codex-blackbox") && ports.contains(&marker) {
            return Some(name.to_string());
        }
    }

    None
}

fn port_state(port: u16) -> PortState {
    if is_port_available(port) {
        PortState::Available
    } else if let Some(container) = codex_blackbox_container_for_port(port) {
        PortState::CodexBlackboxService(container)
    } else {
        PortState::Busy
    }
}

fn repo_file_available(relative: &str) -> Option<bool> {
    find_repo_root().map(|root| root.join(relative).is_file())
}

fn codex_stack_config_available() -> Option<bool> {
    repo_file_available("docker-compose.yml")
        .zip(repo_file_available("envoy/envoy.yaml"))
        .map(|(compose, envoy)| {
            compose
                && envoy
                && BUNDLED_ENVOY_YAML.contains("/backend-api")
                && BUNDLED_ENVOY_YAML.contains("chatgpt_codex_upstream")
                && BUNDLED_ENVOY_YAML.contains("chatgpt.com")
        })
}

fn fake_openai_e2e_available() -> Option<bool> {
    [
        "test/e2e-openai-responses.sh",
        "test/fake-openai.py",
        "test/docker-compose.openai-responses.yml",
        "test/envoy.openai-responses.e2e.yaml",
    ]
    .into_iter()
    .map(repo_file_available)
    .try_fold(true, |acc, available| {
        available.map(|available| acc && available)
    })
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

async fn wait_for_health(url: &str, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if health_check(url).await {
            return true;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    false
}

async fn codex_proxy_route_ready() -> bool {
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
    else {
        return false;
    };
    let Ok(resp) = client.get(envoy_proxy_url()).send().await else {
        return false;
    };
    if resp.status().as_u16() != 404 {
        return false;
    }
    resp.text()
        .await
        .map(|body| body.contains("/backend-api"))
        .unwrap_or(false)
}

async fn wait_for_codex_proxy_route_ready(timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if codex_proxy_route_ready().await {
            return true;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    false
}

async fn ensure_codex_stack_running() -> Result<(), String> {
    if health_check(CODEX_BLACKBOX_CORE_HEALTH_URL).await && codex_proxy_route_ready().await {
        return Ok(());
    }

    println!("Codex Blackbox Codex proxy stack is not ready; starting the local stack...");
    start_stack(false).await
}

fn print_check(symbol: &str, message: impl AsRef<str>) {
    println!("{} {}", symbol, message.as_ref());
}

fn push_unique(lines: &mut Vec<String>, line: impl Into<String>) {
    let line = line.into();
    if !lines.iter().any(|existing| existing == &line) {
        lines.push(line);
    }
}

async fn run_doctor() -> i32 {
    println!("Codex Blackbox doctor");
    println!("Status: experimental Codex ChatGPT subscription wrapper is available.");
    println!("Default stack: ChatGPT/Codex subscription Envoy proxy.");
    println!();

    let mut failed = false;
    let mut fixes = Vec::new();

    print_check("✓", format!("codex-blackbox {}", env!("CARGO_PKG_VERSION")));

    match command_path("codex") {
        Some(path) => {
            let version = command_stdout(path.to_string_lossy().as_ref(), &["--version"])
                .filter(|text| !text.is_empty())
                .unwrap_or_else(|| "version unknown".to_string());
            print_check(
                "✓",
                format!("codex CLI found at {} ({version})", path.display()),
            );
        }
        None => {
            print_check("⚠", "codex CLI not found in PATH");
            push_unique(
                &mut fixes,
                "Install Codex CLI before using `codex-blackbox run -- codex ...`.",
            );
        }
    }

    let docker_found = command_exists("docker");
    if docker_found {
        print_check("✓", "docker found");
    } else {
        failed = true;
        print_check("✗", "docker not found in PATH");
        push_unique(
            &mut fixes,
            "Install Docker Desktop or Docker Engine, then ensure `docker` is on PATH.",
        );
    }

    if docker_found && docker_daemon_running() {
        print_check("✓", "docker daemon running");
    } else {
        failed = true;
        print_check("✗", "docker daemon not reachable");
        push_unique(
            &mut fixes,
            "Start Docker Desktop or your Docker daemon, then rerun `codex-blackbox up`.",
        );
    }

    if let Some(compose) = docker_compose_command() {
        print_check(
            "✓",
            format!("docker compose available ({})", compose.display),
        );
    } else {
        failed = true;
        print_check("✗", "docker compose not available");
        push_unique(
            &mut fixes,
            "Install Docker Compose v2 or make `docker-compose` available on PATH.",
        );
    }

    match fake_openai_e2e_available() {
        Some(true) => {
            print_check("✓", "fake OpenAI Responses e2e available");
        }
        Some(false) => {
            print_check("⚠", "fake OpenAI Responses e2e files missing");
            push_unique(
                &mut fixes,
                "Run from the Codex Blackbox repository if you need `./test/e2e-openai-responses.sh`.",
            );
        }
        None => {
            print_check(
                "⚠",
                "fake OpenAI Responses e2e availability unknown outside repo",
            );
        }
    }

    match codex_stack_config_available() {
        Some(true) => {
            print_check("✓", "default ChatGPT/Codex Envoy config present");
        }
        Some(false) => {
            print_check("⚠", "default ChatGPT/Codex Envoy config missing");
            push_unique(
                &mut fixes,
                "Run from the Codex Blackbox repository so docker-compose.yml and envoy/envoy.yaml are available.",
            );
        }
        None => {
            print_check(
                "⚠",
                "ChatGPT/Codex config availability unknown outside repo",
            );
        }
    }

    if command_exists("tmux") {
        print_check("✓", "tmux found");
    } else {
        print_check("⚠", "tmux not found; --tmux watch mode will not work");
    }

    for port in [10000, 9091, 3000] {
        match port_state(port) {
            PortState::Available => print_check("✓", format!("port {port} available")),
            PortState::CodexBlackboxService(container) => {
                print_check(
                    "✓",
                    format!("port {port} used by Codex Blackbox ({container})"),
                );
            }
            PortState::Busy => {
                failed = true;
                print_check("✗", format!("port {port} is already in use"));
                push_unique(
                    &mut fixes,
                    format!(
                        "Free port {port}, or stop the process using it before running `codex-blackbox up`."
                    ),
                );
            }
        }
    }

    let core_healthy = health_check(CODEX_BLACKBOX_CORE_HEALTH_URL).await;
    if core_healthy {
        print_check("✓", "Codex Blackbox stack: codex-blackbox-core healthy");
    } else {
        print_check(
            "⚠",
            "Codex Blackbox stack: codex-blackbox-core not healthy or not running",
        );
        push_unique(&mut fixes, "Run: codex-blackbox up");
    }

    if health_check(GRAFANA_URL).await {
        print_check("✓", "Codex Blackbox stack: Grafana reachable");
    } else {
        print_check(
            "⚠",
            "Codex Blackbox stack: Grafana not reachable or not running",
        );
        push_unique(&mut fixes, "Run: codex-blackbox up");
    }

    println!();
    print_check(
        "⚠",
        "`codex-blackbox run -- codex ...` defaults to ChatGPT subscription proxy overrides",
    );
    print_check(
        "⚠",
        "Live ChatGPT/Codex subscription traffic is experimental; Codex 0.125.0 smoke is documented",
    );

    if !fixes.is_empty() {
        println!();
        println!("Fix:");
        for fix in fixes {
            println!("  {fix}");
        }
    }

    if failed {
        1
    } else {
        0
    }
}

async fn start_stack(no_grafana: bool) -> Result<(), String> {
    if no_grafana {
        // TODO: make Grafana optional through a compose profile without
        // changing the default all-in-one local stack.
        println!("⚠ --no-grafana is not implemented yet; starting the default stack");
    }

    let compose = docker_compose_command().ok_or_else(|| {
        "docker compose is not available. Run `codex-blackbox doctor`.".to_string()
    })?;

    if command_exists("docker") && !docker_daemon_running() {
        return Err(
            "Docker daemon is not reachable. Start Docker Desktop or your Docker daemon first."
                .to_string(),
        );
    }

    let compose_file = resolve_compose_file()?;
    let compose_root = compose_file
        .parent()
        .ok_or_else(|| "compose file has no parent directory".to_string())?;

    println!("Starting Codex Blackbox stack with {}...", compose.display);
    let _ = io::stdout().flush();
    let mut command = Command::new(&compose.program);
    command
        .args(&compose.args)
        .args(["-p", "codex-blackbox"])
        .arg("-f")
        .arg(&compose_file)
        .args(["up", "-d", "--build", "--remove-orphans"])
        .current_dir(compose_root)
        .env_remove("COMPOSE_FILE")
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let status = command
        .status()
        .map_err(|err| format!("failed to run {}: {}", compose.display, err))?;
    if !status.success() {
        return Err(format!("{} up -d failed", compose.display));
    }

    println!("Restarting Envoy so bind-mounted config is loaded...");
    let _ = io::stdout().flush();
    let envoy_status = Command::new(&compose.program)
        .args(&compose.args)
        .args(["-p", "codex-blackbox"])
        .arg("-f")
        .arg(&compose_file)
        .args(["restart", "envoy"])
        .current_dir(compose_root)
        .env_remove("COMPOSE_FILE")
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|err| format!("failed to run {} restart envoy: {}", compose.display, err))?;
    if !envoy_status.success() {
        return Err(format!("{} restart envoy failed", compose.display));
    }

    println!("Waiting for codex-blackbox-core health...");
    let _ = io::stdout().flush();
    if !wait_for_health(CODEX_BLACKBOX_CORE_HEALTH_URL, Duration::from_secs(90)).await {
        return Err(format!(
            "codex-blackbox-core did not become healthy at {CODEX_BLACKBOX_CORE_HEALTH_URL}"
        ));
    }
    println!("Waiting for Envoy ChatGPT/Codex route...");
    let _ = io::stdout().flush();
    if !wait_for_codex_proxy_route_ready(Duration::from_secs(90)).await {
        return Err(format!(
            "Envoy did not expose the ChatGPT/Codex route at {ENVOY_PROXY_URL}"
        ));
    }

    Ok(())
}

async fn run_up(no_grafana: bool) -> i32 {
    match start_stack(no_grafana).await {
        Ok(()) => {
            println!();
            println!("Codex Blackbox is up.");
            println!("  Default stack: ChatGPT/Codex subscription proxy.");
            println!("  Envoy proxy:    {ENVOY_PROXY_URL}");
            println!("  Codex Blackbox core: {CODEX_BLACKBOX_CORE_URL}");
            println!("  Grafana:        {GRAFANA_DASHBOARD_URL}");
            println!();
            println!("Next:");
            println!("  codex-blackbox run --watch -- codex exec --sandbox read-only \"Summarize this repo in 3 bullets. Do not edit files.\"");
            0
        }
        Err(err) => {
            eprintln!("Error: {err}");
            1
        }
    }
}

fn extract_run_watch(watch_flag: bool, command: Vec<String>) -> (bool, Vec<String>) {
    let mut watch = watch_flag;
    let mut child_command = Vec::with_capacity(command.len());
    for arg in command {
        if arg == "--watch" {
            watch = true;
        } else {
            child_command.push(arg);
        }
    }
    (watch, child_command)
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '_' | '-' | ':' | '='))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn shell_join(parts: &[String]) -> String {
    parts
        .iter()
        .map(|part| shell_quote(part))
        .collect::<Vec<_>>()
        .join(" ")
}

fn current_cli_path() -> String {
    std::env::current_exe()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "codex-blackbox".to_string())
}

fn tmux_session_name() -> String {
    format!("codex-blackbox-watch-{}", std::process::id())
}

fn start_watcher() -> Result<WatchHandle, String> {
    let cli_path = current_cli_path();
    let core_url = codex_blackbox_core_url();
    if command_exists("tmux") {
        let session = tmux_session_name();
        let command = shell_join(&[
            cli_path,
            "watch".to_string(),
            "--tmux".to_string(),
            "--url".to_string(),
            core_url.clone(),
        ]);
        let status = Command::new("tmux")
            .args(["new-session", "-d", "-s", &session, &command])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .status()
            .map_err(|err| format!("failed to start tmux watcher: {err}"))?;
        if !status.success() {
            return Err("failed to start tmux watcher".to_string());
        }
        println!("Watch: tmux attach -t {session}");
        return Ok(WatchHandle::TmuxSession(session));
    }

    let child = Command::new(cli_path)
        .args(["watch", "--url", core_url.as_str()])
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|err| format!("failed to start plain watcher: {err}"))?;
    println!("Watch: plain mode");
    Ok(WatchHandle::Plain(child))
}

fn exit_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RunMode {
    CodexSubscriptionProxy,
    PlainCommand,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ChildStdinMode {
    Inherit,
    Null,
}

impl ChildStdinMode {
    fn stdio(&self) -> Stdio {
        match self {
            ChildStdinMode::Inherit => Stdio::inherit(),
            ChildStdinMode::Null => Stdio::null(),
        }
    }
}

fn tmux_orchestrator_watch_url(base_url: &str) -> String {
    format!("{}/watch?replay=recent", base_url.trim_end_matches('/'))
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ChildRunPlan {
    command: String,
    args: Vec<String>,
    envs: Vec<(String, String)>,
    env_removals: Vec<String>,
    stdin_mode: ChildStdinMode,
    mode: RunMode,
    requires_codex_observation: bool,
}

fn is_codex_command(command: &str) -> bool {
    let path = Path::new(command);
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let file_stem = file_name.strip_suffix(".exe").unwrap_or(file_name);
    file_stem == "codex"
}

fn toml_string_literal(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

fn codex_subscription_config_overrides(
    chatgpt_base_url: &str,
    codex_model_base_url: &str,
) -> Vec<String> {
    vec![
        format!("chatgpt_base_url={}", toml_string_literal(chatgpt_base_url)),
        format!(
            "model_provider={}",
            toml_string_literal(CODEX_MODEL_PROVIDER_ID)
        ),
        format!(
            "model_providers.{CODEX_MODEL_PROVIDER_ID}.name={}",
            toml_string_literal("OpenAI")
        ),
        format!(
            "model_providers.{CODEX_MODEL_PROVIDER_ID}.base_url={}",
            toml_string_literal(codex_model_base_url)
        ),
        format!("model_providers.{CODEX_MODEL_PROVIDER_ID}.wire_api=\"responses\""),
        format!("model_providers.{CODEX_MODEL_PROVIDER_ID}.requires_openai_auth=true"),
        format!("model_providers.{CODEX_MODEL_PROVIDER_ID}.supports_websockets=false"),
        format!("features.{CODEX_REQUEST_COMPRESSION_FEATURE}=false"),
    ]
}

fn codex_config_args_for_subscription(
    chatgpt_base_url: &str,
    codex_model_base_url: &str,
) -> Vec<String> {
    codex_subscription_config_overrides(chatgpt_base_url, codex_model_base_url)
        .into_iter()
        .flat_map(|override_arg| ["-c".to_string(), override_arg])
        .collect()
}

fn codex_child_args_with_subscription_overrides(
    command_args: &[String],
    chatgpt_base_url: &str,
    codex_model_base_url: &str,
) -> Vec<String> {
    let config_args = codex_config_args_for_subscription(chatgpt_base_url, codex_model_base_url);
    let command_args = codex_args_without_json_stdout(command_args);
    if let Some(exec_index) = command_args
        .iter()
        .position(|arg| matches!(arg.as_str(), "exec" | "e"))
    {
        let mut args = Vec::with_capacity(command_args.len() + config_args.len());
        args.extend(command_args[..=exec_index].iter().cloned());
        args.extend(config_args);
        args.extend(command_args[exec_index + 1..].iter().cloned());
        args
    } else {
        let mut args = config_args;
        args.extend(command_args.iter().cloned());
        args
    }
}

fn codex_args_without_json_stdout(command_args: &[String]) -> Vec<String> {
    command_args
        .iter()
        .filter(|arg| arg.as_str() != "--json")
        .cloned()
        .collect()
}

fn codex_command_requires_observation(command_args: &[String]) -> bool {
    if command_args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h" | "--version" | "-V"))
    {
        return false;
    }

    command_args
        .iter()
        .any(|arg| matches!(arg.as_str(), "exec" | "e"))
}

fn build_child_run_plan(child_command: &[String]) -> Result<ChildRunPlan, String> {
    let Some(command) = child_command.first() else {
        return Err("missing command after `codex-blackbox run`".to_string());
    };
    let command_args = &child_command[1..];

    if is_codex_command(command) {
        let chatgpt_base_url = chatgpt_codex_proxy_base_url();
        let codex_model_base_url = codex_model_proxy_base_url();
        let is_exec_invocation = command_args
            .iter()
            .any(|arg| matches!(arg.as_str(), "exec" | "e"));
        let requires_codex_observation = codex_command_requires_observation(command_args);
        let args = codex_child_args_with_subscription_overrides(
            command_args,
            &chatgpt_base_url,
            &codex_model_base_url,
        );
        return Ok(ChildRunPlan {
            command: command.clone(),
            args,
            envs: Vec::new(),
            env_removals: CODEX_PARENT_SESSION_ENV_REMOVALS
                .iter()
                .map(|key| key.to_string())
                .collect(),
            stdin_mode: if is_exec_invocation {
                ChildStdinMode::Null
            } else {
                ChildStdinMode::Inherit
            },
            mode: RunMode::CodexSubscriptionProxy,
            requires_codex_observation,
        });
    }

    Ok(ChildRunPlan {
        command: command.clone(),
        args: command_args.to_vec(),
        envs: Vec::new(),
        env_removals: Vec::new(),
        stdin_mode: ChildStdinMode::Inherit,
        mode: RunMode::PlainCommand,
        requires_codex_observation: false,
    })
}

fn render_child_run_plan(plan: &ChildRunPlan) -> String {
    let mut lines = Vec::new();
    lines.push("Codex Blackbox run preview".to_string());
    match plan.mode {
        RunMode::CodexSubscriptionProxy => {
            lines.push("Mode: experimental Codex ChatGPT subscription proxy".to_string());
            lines.push(format!(
                "ChatGPT auxiliary base URL: {}",
                chatgpt_codex_proxy_base_url()
            ));
            lines.push(format!(
                "Codex model base URL: {}",
                codex_model_proxy_base_url()
            ));
            lines.push(format!(
                "Model provider override: {CODEX_MODEL_PROVIDER_ID} (ChatGPT auth, Responses HTTP)"
            ));
            lines.push("Config files: not modified".to_string());
            lines.push(format!(
                "Request compression: disabled with features.{CODEX_REQUEST_COMPRESSION_FEATURE}=false"
            ));
            lines.push(
                "Codex exec session persistence: wrapper does not inject --ephemeral".to_string(),
            );
            lines.push("Known Codex rollout-recording warning: suppressed".to_string());
            lines.push(
                "Auth: uses existing Codex ChatGPT login; OPENAI_API_KEY is not used".to_string(),
            );
            if plan.requires_codex_observation {
                lines.push(
                    "Post-run check: require Codex Blackbox to observe at least one Codex Responses request"
                        .to_string(),
                );
            }
        }
        RunMode::PlainCommand => {
            lines.push("Mode: plain child command (not proxied)".to_string());
            lines.push("Config files: not modified".to_string());
        }
    }

    lines.push("Command:".to_string());
    let mut command = Vec::with_capacity(plan.args.len() + 1);
    command.push(plan.command.clone());
    command.extend(plan.args.iter().cloned());
    lines.push(format!("  {}", shell_join(&command)));

    lines.push("Environment overrides:".to_string());
    if plan.envs.is_empty() {
        lines.push("  (none)".to_string());
    } else {
        for (key, value) in &plan.envs {
            lines.push(format!("  {key}={value}"));
        }
    }

    lines.push("Environment removals:".to_string());
    if plan.env_removals.is_empty() {
        lines.push("  (none)".to_string());
    } else {
        for key in &plan.env_removals {
            lines.push(format!("  {key}"));
        }
    }

    lines.push(match (&plan.mode, &plan.stdin_mode) {
        (RunMode::CodexSubscriptionProxy, ChildStdinMode::Null) => {
            "Child stdin: closed for Codex exec".to_string()
        }
        (_, ChildStdinMode::Null) => "Child stdin: closed".to_string(),
        (_, ChildStdinMode::Inherit) => "Child stdin: inherited".to_string(),
    });

    lines.join("\n") + "\n"
}

fn print_child_run_status(plan: &ChildRunPlan) {
    match plan.mode {
        RunMode::CodexSubscriptionProxy => {
            eprintln!(
                "Codex Blackbox: launching Codex with experimental ChatGPT subscription proxy settings."
            );
            eprintln!(
                "Codex Blackbox: ChatGPT auxiliary base URL {}",
                chatgpt_codex_proxy_base_url()
            );
            eprintln!(
                "Codex Blackbox: Codex model base URL {}",
                codex_model_proxy_base_url()
            );
            eprintln!(
                "Codex Blackbox: model provider override {CODEX_MODEL_PROVIDER_ID} uses ChatGPT auth with Responses HTTP."
            );
            eprintln!("Codex Blackbox: command-line config overrides only; ~/.codex/config.toml is not modified.");
            eprintln!(
                "Codex Blackbox: request compression disabled via features.{CODEX_REQUEST_COMPRESSION_FEATURE}=false."
            );
            eprintln!("Codex Blackbox: wrapper does not inject --ephemeral; Codex keeps its normal session persistence behavior.");
            eprintln!("Codex Blackbox: suppressing known Codex rollout-recording warning; Codex Blackbox records traffic via proxy.");
            eprintln!("Codex Blackbox: OPENAI_API_KEY is not used for subscription mode.");
            if !plan.env_removals.is_empty() {
                eprintln!(
                    "Codex Blackbox: removing inherited Codex parent-session env vars from the child process."
                );
            }
            if plan.requires_codex_observation {
                eprintln!("Codex Blackbox: will fail this run if codex-blackbox-core observes no Codex Responses request.");
            }
            if plan.stdin_mode == ChildStdinMode::Null {
                eprintln!("Codex Blackbox: child stdin is closed for Codex exec.");
            }
        }
        RunMode::PlainCommand => {
            eprintln!("Codex Blackbox: launching non-Codex child command without proxy overrides.");
        }
    }
}

fn run_command_with_env(
    command: &str,
    args: &[String],
    envs: &[(String, String)],
    env_removals: &[String],
    stdin_mode: &ChildStdinMode,
) -> Result<i32, String> {
    let mut child = Command::new(command);
    child.args(args);
    for key in env_removals {
        child.env_remove(key);
    }
    for (key, value) in envs {
        child.env(key, value);
    }
    let status = child
        .stdin(stdin_mode.stdio())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|err| format!("failed to spawn {command}: {err}"))?
        .wait()
        .map_err(|err| format!("failed while waiting for {command}: {err}"))?;

    Ok(exit_code(status))
}

fn run_codex_command_with_filtered_stderr(
    command: &str,
    args: &[String],
    envs: &[(String, String)],
    env_removals: &[String],
    stdin_mode: &ChildStdinMode,
) -> Result<i32, String> {
    let mut child = Command::new(command);
    child.args(args);
    for key in env_removals {
        child.env_remove(key);
    }
    for (key, value) in envs {
        child.env(key, value);
    }
    let mut child = child
        .stdin(stdin_mode.stdio())
        .stdout(Stdio::inherit())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("failed to spawn {command}: {err}"))?;

    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "failed to capture child stderr".to_string())?;
    let stderr_thread = std::thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines() {
            match line {
                Ok(line) if should_suppress_codex_stderr_line(&line) => {}
                Ok(line) => eprintln!("{line}"),
                Err(err) => {
                    eprintln!("Codex Blackbox: failed to read Codex stderr: {err}");
                    break;
                }
            }
        }
    });

    let status = child
        .wait()
        .map_err(|err| format!("failed while waiting for {command}: {err}"))?;
    let _ = stderr_thread.join();

    Ok(exit_code(status))
}

fn should_suppress_codex_stderr_line(line: &str) -> bool {
    line.contains("failed to record rollout items")
        || line == "Reading additional input from stdin..."
        || line.contains("write_stdin failed: stdin is closed for this session")
}

fn parse_codex_requests_total(metrics: &str) -> Option<f64> {
    let mut total = 0.0;
    let mut found = false;

    for line in metrics.lines().map(str::trim) {
        if line.starts_with("codex_blackbox_requests_total{")
            && line.contains(r#"provider="codex_responses""#)
        {
            let Some(value) = line
                .rsplit_once(' ')
                .and_then(|(_, value)| value.parse::<f64>().ok())
            else {
                continue;
            };
            total += value;
            found = true;
        }
    }

    found.then_some(total)
}

async fn fetch_codex_requests_total() -> Result<f64, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .map_err(|err| format!("failed to build metrics client: {err}"))?;
    let metrics_url = format!(
        "{}/metrics",
        codex_blackbox_core_url().trim_end_matches('/')
    );
    let resp = client
        .get(&metrics_url)
        .send()
        .await
        .map_err(|err| format!("failed to fetch {metrics_url}: {err}"))?;
    if !resp.status().is_success() {
        return Err(format!(
            "failed to fetch {metrics_url}: HTTP {}",
            resp.status()
        ));
    }
    let body = resp
        .text()
        .await
        .map_err(|err| format!("failed to read {metrics_url}: {err}"))?;
    parse_codex_requests_total(&body).ok_or_else(|| {
        "codex_blackbox_requests_total for provider=\"codex_responses\" is missing".to_string()
    })
}

async fn wait_for_codex_observation_delta(before: f64, timeout: Duration) -> Result<bool, String> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        let after = fetch_codex_requests_total().await?;
        if after > before {
            return Ok(true);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Ok(false)
}

async fn run_child_command(watch_flag: bool, dry_run: bool, command: Vec<String>) -> i32 {
    let (watch, child_command) = extract_run_watch(watch_flag, command);
    if child_command.is_empty() {
        eprintln!("Error: missing command after `codex-blackbox run`");
        return 1;
    }

    let plan = match build_child_run_plan(&child_command) {
        Ok(plan) => plan,
        Err(err) => {
            eprintln!("Error: {err}");
            return 1;
        }
    };

    if dry_run {
        print!("{}", render_child_run_plan(&plan));
        return 0;
    }

    let stack_result = match plan.mode {
        RunMode::CodexSubscriptionProxy => ensure_codex_stack_running().await,
        RunMode::PlainCommand => Ok(()),
    };
    if let Err(err) = stack_result {
        eprintln!("Error: {err}");
        return 1;
    }

    let observed_before = if plan.requires_codex_observation {
        match fetch_codex_requests_total().await {
            Ok(value) => Some(value),
            Err(err) => {
                eprintln!("Error: Codex Blackbox observation pre-check failed: {err}");
                return 1;
            }
        }
    } else {
        None
    };

    let mut watcher = if watch {
        match start_watcher() {
            Ok(handle) => Some(handle),
            Err(err) => {
                eprintln!("Error: {err}");
                return 1;
            }
        }
    } else {
        None
    };

    print_child_run_status(&plan);
    let result = match plan.mode {
        RunMode::CodexSubscriptionProxy => run_codex_command_with_filtered_stderr(
            &plan.command,
            &plan.args,
            &plan.envs,
            &plan.env_removals,
            &plan.stdin_mode,
        ),
        RunMode::PlainCommand => run_command_with_env(
            &plan.command,
            &plan.args,
            &plan.envs,
            &plan.env_removals,
            &plan.stdin_mode,
        ),
    };

    if let Some(handle) = watcher.as_mut() {
        handle.stop();
    }

    match result {
        Ok(code) => {
            if code == 0 {
                if let Some(before) = observed_before {
                    match wait_for_codex_observation_delta(before, Duration::from_secs(5)).await {
                        Ok(true) => {}
                        Ok(false) => {
                            eprintln!("Error: Codex exited successfully, but codex-blackbox-core did not observe any new provider=\"codex_responses\" request. Treating this as a failed Codex Blackbox proxy run.");
                            return 1;
                        }
                        Err(err) => {
                            eprintln!("Error: Codex Blackbox observation post-check failed: {err}");
                            return 1;
                        }
                    }
                }
            }
            code
        }
        Err(err) => {
            eprintln!("Error: {err}");
            1
        }
    }
}

fn codex_login_status_text() -> Result<String, String> {
    let output = Command::new("codex")
        .args(["login", "status"])
        .stdin(Stdio::null())
        .output()
        .map_err(|err| format!("failed to run `codex login status`: {err}"))?;
    let mut text = String::new();
    text.push_str(&String::from_utf8_lossy(&output.stdout));
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    if !output.status.success() {
        return Err("`codex login status` failed; run `codex login` first".to_string());
    }
    Ok(text.trim().to_string())
}

fn require_chatgpt_codex_login() -> Result<(), String> {
    let status = codex_login_status_text()?;
    let normalized = status.to_ascii_lowercase();
    if normalized.contains("not logged in") {
        return Err(
            "Codex is not logged in; run `codex login` and choose ChatGPT auth".to_string(),
        );
    }
    if normalized.contains("api key") && !normalized.contains("chatgpt") {
        return Err(
            "Codex appears to be using API-key auth; run `codex login` and choose ChatGPT auth"
                .to_string(),
        );
    }
    if !normalized.contains("chatgpt") {
        return Err(
            "`codex login status` did not confirm ChatGPT auth; refusing subscription preflight"
                .to_string(),
        );
    }
    Ok(())
}

async fn run_codex_subscription_preflight(command: Vec<String>) -> i32 {
    if command.is_empty() {
        eprintln!(
            "Error: missing Codex command after `codex-blackbox preflight codex-subscription --`"
        );
        return 1;
    }
    if !is_codex_command(&command[0]) {
        eprintln!("Error: subscription preflight expects a `codex` child command");
        return 1;
    }

    println!("Checking local Codex ChatGPT login...");
    if let Err(err) = require_chatgpt_codex_login() {
        eprintln!("Error: {err}");
        return 1;
    }
    println!("Codex ChatGPT login detected.");

    if let Err(err) = ensure_codex_stack_running().await {
        eprintln!("Error: {err}");
        return 1;
    }

    let plan = match build_child_run_plan(&command) {
        Ok(plan) => plan,
        Err(err) => {
            eprintln!("Error: {err}");
            return 1;
        }
    };

    let mut run_command = vec![
        current_cli_path(),
        "run".to_string(),
        "--".to_string(),
        plan.command,
    ];
    run_command.extend(command.into_iter().skip(1));

    println!();
    println!("Subscription proxy stack is ready. No Codex turn has been launched.");
    println!("Network path for the live smoke:");
    println!(
        "  Codex model turns -> {} -> Envoy ext_proc -> https://chatgpt.com{}",
        codex_model_proxy_base_url(),
        CODEX_MODEL_BACKEND_PATH
    );
    println!(
        "  Codex auxiliary calls -> {} -> Envoy ext_proc -> https://chatgpt.com{}",
        chatgpt_codex_proxy_base_url(),
        CHATGPT_CODEX_BACKEND_PATH
    );
    println!("Exact command to run after explicit approval:");
    println!("  {}", shell_join(&run_command));
    println!();
    println!("Cleanup:");
    println!("  docker compose -f docker-compose.yml down --remove-orphans -t 5");

    0
}

fn chatgpt_codex_proxy_base_url() -> String {
    format!(
        "{}{}",
        envoy_proxy_url().trim_end_matches('/'),
        CHATGPT_CODEX_BACKEND_PATH
    )
}

fn codex_model_proxy_base_url() -> String {
    format!(
        "{}{}",
        envoy_proxy_url().trim_end_matches('/'),
        CODEX_MODEL_BACKEND_PATH
    )
}

fn render_codex_config_preview() -> String {
    let subscription_overrides = codex_subscription_config_overrides(
        &chatgpt_codex_proxy_base_url(),
        &codex_model_proxy_base_url(),
    )
    .into_iter()
    .map(|override_arg| format!("  -c {}", shell_quote(&override_arg)))
    .collect::<Vec<_>>()
    .join("\n");
    format!(
        r#"Codex Blackbox Codex config preview (read-only)
Status: experimental ChatGPT subscription wrapper is the only Codex CLI path via:
  codex-blackbox run -- codex ...

Default Codex Blackbox stack:
  codex-blackbox up

Codex Blackbox passes these command-line overrides; ~/.codex/config.toml is not modified:
{subscription_overrides}

Codex Blackbox removes inherited parent-session variables from child Codex runs:
  CODEX_CI
  CODEX_INTERNAL_ORIGINATOR_OVERRIDE
  CODEX_SHELL
  CODEX_THREAD_ID

Codex Blackbox closes child stdin for Codex runs so codex exec cannot consume harness
manifests or shell-loop input.

Codex Blackbox does not pass codex exec --json and does not parse local Codex stdout
as live telemetry. Envoy-observed Responses traffic is the telemetry source.

# Codex CLI mode requires an existing Codex ChatGPT login and does not use OPENAI_API_KEY.
"#
    )
}

fn print_codex_config_preview() {
    print!("{}", render_codex_config_preview());
}

fn parse_local_datetime(iso: &str) -> Option<DateTime<Local>> {
    DateTime::parse_from_rfc3339(iso)
        .ok()
        .map(|dt| dt.with_timezone(&Local))
}

fn local_time_from_iso(iso: &str) -> String {
    parse_local_datetime(iso)
        .map(|dt| dt.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "??:??:??".to_string())
}

fn compact_datetime_from_iso(iso: &str) -> String {
    parse_local_datetime(iso)
        .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| iso.to_string())
}

fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{}K", tokens / 1_000)
    } else {
        format!("{}", tokens)
    }
}

#[cfg(test)]
fn format_duration_coarse(secs: u64) -> String {
    const M: u64 = 60;
    const H: u64 = 60 * M;
    const D: u64 = 24 * H;
    if secs < M {
        format!("{}s", secs)
    } else if secs < H {
        format!("{}m", secs / M)
    } else if secs < D {
        let h = secs / H;
        let m = (secs % H) / M;
        if m == 0 {
            format!("{}h", h)
        } else {
            format!("{}h {}m", h, m)
        }
    } else {
        let d = secs / D;
        let h = (secs % D) / H;
        if h == 0 {
            format!("{}d", d)
        } else {
            format!("{}d {}h", d, h)
        }
    }
}

fn truncate_for_box(s: &str, max_chars: usize) -> String {
    // Char-count safe; final ellipsis is counted within the cap.
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_chars {
        s.to_string()
    } else {
        let mut out: String = chars.iter().take(max_chars.saturating_sub(1)).collect();
        out.push('\u{2026}');
        out
    }
}

fn now_hms() -> String {
    Local::now().format("%H:%M:%S").to_string()
}

/// Extract session_id from any WatchEvent variant. Returns None for lagged
/// pseudo-events from the server.
pub(crate) fn event_session_id(event: &WatchEvent) -> Option<&str> {
    match event {
        WatchEvent::ToolUse { session_id, .. }
        | WatchEvent::SessionStart { session_id, .. }
        | WatchEvent::SessionEnd { session_id, .. }
        | WatchEvent::FrustrationSignal { session_id, .. }
        | WatchEvent::CompactionLoop { session_id, .. }
        | WatchEvent::Diagnosis { session_id, .. }
        | WatchEvent::ModelFallback { session_id, .. }
        | WatchEvent::CodexTurnSummary { session_id, .. }
        | WatchEvent::ContextStatus { session_id, .. } => Some(session_id.as_str()),
        WatchEvent::Lagged { .. } => None,
    }
}

/// Tracks active sessions for prefix tag display.
struct ActiveSessions {
    /// session_id -> display_name
    sessions: HashMap<String, String>,
}

impl ActiveSessions {
    fn new() -> Self {
        Self {
            sessions: HashMap::new(),
        }
    }

    fn add(&mut self, session_id: &str, display_name: &str) {
        self.sessions
            .insert(session_id.to_string(), display_name.to_string());
    }

    fn remove(&mut self, session_id: &str) {
        self.sessions.remove(session_id);
    }

    fn is_multi(&self) -> bool {
        self.sessions.len() > 1
    }

    /// Returns the prefix tag for a given session_id, padded to the max name width.
    /// Returns empty string when only 1 or 0 sessions are active.
    fn tag_for(&self, session_id: &str) -> String {
        if !self.is_multi() {
            return String::new();
        }
        let max_width = self.sessions.values().map(|n| n.len()).max().unwrap_or(0);
        let name = self
            .sessions
            .get(session_id)
            .map(|s| s.as_str())
            .unwrap_or("?");
        format!("[{:width$}]  ", name, width = max_width)
    }
}

/// Print a line with an optional session tag prefix.
fn print_tagged(tag: &str, line: &str) {
    if tag.is_empty() {
        println!("{}", line);
    } else {
        println!("{}{}", tag.dimmed(), line);
    }
}

fn is_codex_model_name(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    lower.contains("codex")
        || lower.starts_with("gpt-")
        || lower.starts_with("o1")
        || lower.starts_with("o3")
        || lower.starts_with("o4")
}

fn watch_model_label(model: &str) -> String {
    if is_codex_model_name(model) {
        format!("CODEX \u{00b7} {model}")
    } else {
        model.to_string()
    }
}

fn model_change_label(requested: &str, actual: &str) -> &'static str {
    let _ = (requested, actual);
    "MODEL CHANGE"
}

fn model_change_line(time: &str, requested: &str, actual: &str) -> String {
    format!(
        "{}  \u{26a0}  {}  requested {}, served {}",
        time,
        model_change_label(requested, actual),
        requested,
        actual
    )
}

fn context_window_label(context_window_tokens: Option<u64>) -> String {
    context_window_tokens
        .map(|tokens| format!(" of {} window", format_tokens(tokens)))
        .unwrap_or_default()
}

fn context_status_line(
    time: &str,
    fill_percent: f64,
    context_window_tokens: Option<u64>,
    turns_to_compact: Option<u32>,
) -> String {
    let label = match turns_to_compact {
        Some(0) => "at compaction threshold".to_string(),
        Some(n) => format!("~{} turns to compaction", n),
        None => "trajectory unknown".to_string(),
    };
    format!(
        "{}  CONTEXT  {:.0}%{} \u{00b7} {}",
        time,
        fill_percent,
        context_window_label(context_window_tokens),
        label
    )
}

struct CodexTurnSummaryLine<'a> {
    time: &'a str,
    status: &'a str,
    requested_model: &'a str,
    served_model: Option<&'a str>,
    input_tokens: u64,
    cached_input_tokens: u64,
    uncached_input_tokens: u64,
    output_tokens: u64,
    reasoning_output_tokens: u64,
    total_tokens: u64,
}

fn codex_turn_summary_line(summary: CodexTurnSummaryLine<'_>) -> String {
    let model_part = match summary.served_model {
        Some(served) if served != summary.requested_model => {
            format!("requested {}, served {}", summary.requested_model, served)
        }
        Some(served) => format!("served {}", served),
        None => format!("requested {}", summary.requested_model),
    };
    let reasoning_part = if summary.reasoning_output_tokens > 0 {
        format!(
            " + {} reasoning",
            format_tokens(summary.reasoning_output_tokens)
        )
    } else {
        String::new()
    };
    format!(
        "{}  CODEX   {} \u{00b7} {} \u{00b7} input {} ({} cached, {} uncached) \u{00b7} output {}{} \u{00b7} total {}",
        summary.time,
        summary.status,
        model_part,
        format_tokens(summary.input_tokens),
        format_tokens(summary.cached_input_tokens),
        format_tokens(summary.uncached_input_tokens),
        format_tokens(summary.output_tokens),
        reasoning_part,
        format_tokens(summary.total_tokens)
    )
}

fn parse_mcp_tool_name(tool_name: &str) -> Option<(&str, &str)> {
    let rest = tool_name.trim().strip_prefix("mcp__")?;
    let (server, tool) = rest.split_once("__")?;
    if server.is_empty() || tool.is_empty() {
        None
    } else {
        Some((server, tool))
    }
}

fn render_event(
    event: &WatchEvent,
    no_signals: bool,
    session_filter: &Option<String>,
    active: &mut ActiveSessions,
) {
    // Apply session filter to all events (except Lagged which has no session).
    if let Some(filter) = session_filter {
        if let Some(sid) = event_session_id(event) {
            if sid != filter {
                return;
            }
        }
    }

    // Update active session tracking BEFORE rendering.
    match event {
        WatchEvent::SessionStart {
            session_id,
            display_name,
            ..
        } => {
            active.add(session_id, display_name);
        }
        WatchEvent::SessionEnd { .. } => {
            // Remove AFTER rendering — so the tag is still available for the SessionEnd line.
        }
        _ => {}
    }

    // Compute tag for this event's session.
    let tag = event_session_id(event)
        .map(|sid| active.tag_for(sid))
        .unwrap_or_default();

    match event {
        WatchEvent::SessionStart {
            session_id: _,
            display_name,
            model,
            initial_prompt,
        } => {
            let time = now_hms();
            let model_label = watch_model_label(model);
            let header_inner = format!(
                "  {}  \u{00b7}  {}  \u{00b7}  {}  ",
                display_name, model_label, time
            );
            // Second line carries the user's prompt, if we captured one.
            let prompt_inner = initial_prompt
                .as_ref()
                .map(|p| format!("  \u{2192} {}  ", truncate_for_box(p, 90)));
            let width = header_inner
                .len()
                .max(prompt_inner.as_ref().map(|p| p.len()).unwrap_or(0))
                .max(57);
            println!();
            print_tagged(
                &tag,
                &format!("\u{250c}{}\u{2510}", "\u{2500}".repeat(width))
                    .cyan()
                    .to_string(),
            );
            print_tagged(
                &tag,
                &format!("\u{2502}{:width$}\u{2502}", header_inner, width = width)
                    .cyan()
                    .to_string(),
            );
            if let Some(line) = prompt_inner {
                print_tagged(
                    &tag,
                    &format!("\u{2502}{:width$}\u{2502}", line, width = width)
                        .cyan()
                        .dimmed()
                        .to_string(),
                );
            }
            print_tagged(
                &tag,
                &format!("\u{2514}{}\u{2518}", "\u{2500}".repeat(width))
                    .cyan()
                    .to_string(),
            );
        }

        WatchEvent::SessionEnd {
            session_id,
            outcome,
            total_tokens,
            total_turns,
        } => {
            let bar = "\u{2501}".repeat(58);
            print_tagged(&tag, &bar.dimmed().to_string());
            let outcome_colored = if outcome.contains("Completed") && !outcome.contains("Partially")
            {
                format!("{} \u{2713}", outcome).green().to_string()
            } else if outcome.contains("Partially Completed") {
                outcome.to_string().yellow().to_string()
            } else if outcome.contains("Abandoned") {
                outcome.to_string().dimmed().to_string()
            } else {
                outcome.to_string().yellow().to_string()
            };
            let tokens_display = format_tokens(*total_tokens);
            print_tagged(
                &tag,
                &format!(
                    "SESSION COMPLETE \u{00b7} {} tokens \u{00b7} {} turns \u{00b7} {}",
                    tokens_display, total_turns, outcome_colored
                ),
            );
            print_tagged(&tag, &bar.dimmed().to_string());

            // Remove after rendering.
            active.remove(session_id);
        }

        WatchEvent::ToolUse {
            session_id: _,
            timestamp,
            tool_name,
            summary,
        } => {
            let time = local_time_from_iso(timestamp);
            if let Some((server, tool)) = parse_mcp_tool_name(tool_name) {
                let summary_display = if summary.len() > 80 {
                    format!("{}...", &summary[..77])
                } else {
                    summary.clone()
                };
                let line = if summary_display.is_empty() {
                    format!("{}  MCP     {}.{}", time, server, tool)
                } else {
                    format!("{}  MCP     {}.{}  {}", time, server, tool, summary_display)
                };
                print_tagged(&tag, &line.cyan().to_string());
                return;
            }
            let label = format!("{:<6}", tool_name.to_uppercase());
            let summary_display = if summary.len() > 80 {
                format!("{}...", &summary[..77])
            } else {
                summary.clone()
            };

            let line = format!("{}  {}  {}", time, label, summary_display);
            let colored_line = match tool_name.as_str() {
                "Read" => line.cyan().to_string(),
                "Edit" => line.yellow().to_string(),
                "Write" => line.yellow().bold().to_string(),
                "Bash" | "bash" => line.white().to_string(),
                "Glob" | "Grep" => line.dimmed().to_string(),
                _ => line.white().to_string(),
            };
            print_tagged(&tag, &colored_line);
        }

        WatchEvent::ModelFallback {
            session_id: _,
            requested,
            actual,
        } => {
            let time = now_hms();
            print_tagged(
                &tag,
                &model_change_line(&time, requested, actual)
                    .yellow()
                    .bold()
                    .to_string(),
            );
        }

        WatchEvent::CodexTurnSummary {
            session_id: _,
            status,
            requested_model,
            served_model,
            input_tokens,
            cached_input_tokens,
            uncached_input_tokens,
            output_tokens,
            reasoning_output_tokens,
            total_tokens,
        } => {
            let time = now_hms();
            let line = codex_turn_summary_line(CodexTurnSummaryLine {
                time: &time,
                status,
                requested_model,
                served_model: served_model.as_deref(),
                input_tokens: *input_tokens,
                cached_input_tokens: *cached_input_tokens,
                uncached_input_tokens: *uncached_input_tokens,
                output_tokens: *output_tokens,
                reasoning_output_tokens: *reasoning_output_tokens,
                total_tokens: *total_tokens,
            });
            let colored = match status.as_str() {
                "completed" => line.green().to_string(),
                "failed" => line.red().bold().to_string(),
                "incomplete" => line.yellow().bold().to_string(),
                _ => line.yellow().to_string(),
            };
            print_tagged(&tag, &colored);
        }

        WatchEvent::ContextStatus {
            session_id: _,
            fill_percent,
            context_window_tokens,
            turns_to_compact,
        } => {
            // Only show context status when it actually matters — avoid
            // noise every turn when we're nowhere near compaction.
            if *fill_percent < 60.0 {
                return;
            }
            let time = now_hms();
            let line = context_status_line(
                &time,
                *fill_percent,
                *context_window_tokens,
                *turns_to_compact,
            );
            let colored = if *fill_percent >= 80.0 {
                line.red().bold().to_string()
            } else {
                line.yellow().to_string()
            };
            print_tagged(&tag, &colored);
        }

        WatchEvent::FrustrationSignal {
            session_id: _,
            signal_type,
        } => {
            if no_signals {
                return;
            }
            let time = now_hms();
            print_tagged(
                &tag,
                &format!(
                    "{}  \u{26a0} SIGNAL  {} pattern detected",
                    time, signal_type
                )
                .yellow()
                .to_string(),
            );
        }

        WatchEvent::CompactionLoop {
            session_id: _,
            consecutive,
            wasted_tokens,
        } => {
            let tokens_display = format_tokens(*wasted_tokens);
            let inner1 = format!(
                "  \u{1f504} POSSIBLE LOOP \u{00b7} {} rapid turns \u{00b7} ~{} tokens wasted?  ",
                consecutive, tokens_display
            );
            let inner2 = "  UNPORTED baseline: if model seems stuck, Ctrl+C and restart";
            let width = inner1.len().max(inner2.len()).max(57);
            print_tagged(
                &tag,
                &format!("\u{250c}{}\u{2510}", "\u{2500}".repeat(width))
                    .yellow()
                    .to_string(),
            );
            print_tagged(
                &tag,
                &format!("\u{2502}{:width$}\u{2502}", inner1, width = width)
                    .yellow()
                    .to_string(),
            );
            print_tagged(
                &tag,
                &format!("\u{2502}{:width$}\u{2502}", inner2, width = width)
                    .yellow()
                    .to_string(),
            );
            print_tagged(
                &tag,
                &format!("\u{2514}{}\u{2518}", "\u{2500}".repeat(width))
                    .yellow()
                    .to_string(),
            );
        }

        WatchEvent::Lagged { missed } => {
            println!(
                "{}",
                format!("[{} events missed \u{2014} channel overflowed]", missed).dimmed()
            );
        }

        WatchEvent::Diagnosis {
            session_id: _,
            report,
        } => {
            render_diagnosis(report, &tag);
        }
    }
}

fn cause_icon(cause_type: &str) -> &'static str {
    match cause_type {
        "codex_response_failed" | "codex_response_incomplete" => "\u{26a0}\u{fe0f}", // ⚠️
        "codex_model_mismatch" => "\u{21c4}",                                        // ⇄
        "codex_high_context_fill" => "\u{23f3}",                                     // ⏳
        "codex_high_reasoning_share" => "\u{25c9}",                                  // ◉
        "codex_accounting_anomaly" => "\u{203c}\u{fe0f}",                            // ‼️
        "codex_low_cached_input_reuse" => "\u{25cc}",                                // ◌
        _ => "\u{2022}",                                                             // •
    }
}

fn render_diagnosis(report: &DiagnosisReport, tag: &str) {
    if !report.degraded {
        return;
    }

    let bar = "\u{2501}".repeat(58);
    println!();
    print_tagged(tag, &bar.yellow().to_string());
    let tokens_display = format_tokens(report.total_tokens);
    print_tagged(
        tag,
        &format!(
            "SESSION COMPLETE \u{00b7} {} turns \u{00b7} {} tokens \u{00b7} {}",
            report.total_turns, tokens_display, report.outcome
        )
        .yellow()
        .to_string(),
    );
    print_tagged(tag, "");

    if let Some(turn) = report.degradation_turn {
        print_tagged(
            tag,
            &format!("Why it slowed down (from turn {}):", turn)
                .yellow()
                .to_string(),
        );
    } else {
        print_tagged(tag, &"Why it slowed down:".yellow().to_string());
    }

    for cause in &report.causes {
        let icon = cause_icon(&cause.cause_type);
        let heuristic_suffix = if cause.is_heuristic {
            format!("  {}", "(estimate)".dimmed())
        } else {
            String::new()
        };
        print_tagged(
            tag,
            &format!(
                "  {} turn {} \u{00b7} {}{}",
                icon, cause.turn_first_noticed, cause.detail, heuristic_suffix
            ),
        );
    }

    if !report.advice.is_empty() {
        print_tagged(tag, "");
        print_tagged(tag, &"Next time:".green().to_string());
        for a in &report.advice {
            print_tagged(tag, &format!("  {} {}", "\u{2192}".green(), a));
        }
    }

    print_tagged(tag, &bar.yellow().to_string());
    println!();
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Doctor => {
            std::process::exit(run_doctor().await);
        }
        Commands::Up { no_grafana } => {
            std::process::exit(run_up(no_grafana).await);
        }
        Commands::Run {
            watch,
            dry_run,
            command,
        } => {
            std::process::exit(run_child_command(watch, dry_run, command).await);
        }
        Commands::Watch {
            url,
            no_signals,
            session,
            tmux,
            tmux_max_panes,
        } => {
            if tmux {
                // Tmux orchestrator mode. Self-bootstrap into a tmux session
                // if we're not already inside one, so the user just runs
                // `codex-blackbox watch --tmux` once.
                if let Err(e) = tmux::bootstrap_into_tmux(&url, no_signals, tmux_max_panes) {
                    eprintln!("{}", e.red());
                    std::process::exit(1);
                }
                let orchestrator =
                    match tmux::TmuxOrchestrator::new(url.clone(), no_signals, tmux_max_panes) {
                        Ok(o) => o,
                        Err(e) => {
                            eprintln!("{}", format!("tmux init failed: {}", e).red());
                            std::process::exit(1);
                        }
                    };
                let watch_url = tmux_orchestrator_watch_url(&url);
                if let Err(e) = orchestrator.run(&watch_url).await {
                    eprintln!("{}", format!("Orchestrator error: {}", e).red());
                    std::process::exit(1);
                }
            } else {
                // Existing inline watch mode. When --session is set, pass it
                // as ?session=X so the server can inject a synthetic
                // SessionStart for mid-session joiners. Session ids are
                // server-generated and URL-safe by construction
                // (`session_<ts>_<hex>`), no escaping needed.
                let watch_url = match &session {
                    Some(sid) => format!("{}/watch?session={}", url.trim_end_matches('/'), sid),
                    None => format!("{}/watch", url.trim_end_matches('/')),
                };
                println!("Connecting to {}...", watch_url);
                let mut active = ActiveSessions::new();
                let mut retry_log = WatchRetryLog::default();

                loop {
                    match connect_and_stream(&watch_url, no_signals, &session, &mut active).await {
                        Ok(()) => {
                            retry_log.reset();
                            eprintln!(
                                "{}",
                                format!(
                                    "Connection closed. Reconnecting in {}s...",
                                    WATCH_RECONNECT_DELAY.as_secs()
                                )
                                .dimmed()
                            );
                        }
                        Err(e) => {
                            if let Some(message) = retry_log.retry_message(&e) {
                                eprintln!("{}", message.dimmed());
                            }
                        }
                    }
                    tokio::time::sleep(WATCH_RECONNECT_DELAY).await;
                }
            }
        }
        Commands::Sessions { url, limit, days } => {
            let sessions_url = format!(
                "{}/api/sessions?limit={}&days={}",
                url.trim_end_matches('/'),
                limit,
                days
            );
            match fetch_sessions(&sessions_url).await {
                Ok(()) => {}
                Err(e) => {
                    eprintln!("{}", format!("Error: {}", e).red());
                    std::process::exit(1);
                }
            }
        }
        Commands::Recall {
            url,
            limit,
            days,
            query,
        } => {
            let query = query.join(" ");
            let recall_url = format!("{}/api/recall", url.trim_end_matches('/'));
            match fetch_recall(&recall_url, &query, limit, days).await {
                Ok(()) => {}
                Err(e) => {
                    eprintln!("{}", format!("Error: {}", e).red());
                    std::process::exit(1);
                }
            }
        }
        Commands::Postmortem {
            url,
            no_redact,
            output,
            target,
        } => {
            let redact = !no_redact;
            match fetch_postmortem(&url, &target, redact, output.as_deref()).await {
                Ok(()) => {}
                Err(e) => {
                    eprintln!("{}", format!("Error: {}", e).red());
                    std::process::exit(1);
                }
            }
        }
        Commands::Reconcile {
            url,
            session,
            billed_cost,
            source,
            imported_at,
        } => {
            let reconcile_url =
                format!("{}/api/billing-reconciliations", url.trim_end_matches('/'));
            match post_reconciliation(
                &reconcile_url,
                &session,
                billed_cost,
                &source,
                imported_at.as_deref(),
            )
            .await
            {
                Ok(()) => {}
                Err(e) => {
                    eprintln!("{}", format!("Error: {}", e).red());
                    std::process::exit(1);
                }
            }
        }
        Commands::Config { command } => match command {
            ConfigCommands::Codex => {
                print_codex_config_preview();
            }
        },
        Commands::Preflight { command } => match command {
            PreflightCommands::CodexSubscription { command } => {
                std::process::exit(run_codex_subscription_preflight(command).await);
            }
        },
    }
}

async fn fetch_sessions(url: &str) -> Result<(), Box<dyn std::error::Error>> {
    let resp = reqwest::Client::new().get(url).send().await?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()).into());
    }
    let body: serde_json::Value = resp.json().await?;
    let cost_source = body
        .get("local_estimate_cost_source")
        .or_else(|| body.get("cost_source"))
        .and_then(|s| s.as_str())
        .unwrap_or("builtin_model_family_pricing");
    let trusted_for_budget_enforcement = body
        .get("local_estimate_trusted_for_budget_enforcement")
        .or_else(|| body.get("trusted_for_budget_enforcement"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let sessions = body.get("sessions").and_then(|s| s.as_array());
    let Some(sessions) = sessions else {
        println!("No sessions found.");
        return Ok(());
    };
    if sessions.is_empty() {
        println!("No sessions found.");
        return Ok(());
    }

    let cost_source_label = match cost_source {
        "builtin_model_family_pricing" => "built-in model-family pricing",
        other => other,
    };
    let trust_label = if trusted_for_budget_enforcement {
        "hard-stop dollar budgets enabled"
    } else {
        "dollar budgets advisory only"
    };
    println!(
        "{}",
        format!(
            "Local estimate source: {} · {}",
            cost_source_label, trust_label
        )
        .dimmed()
    );

    // Header
    println!(
        "{:<22} {:<24} {:<8} {:<22} {:<16} {:<14} {:<8} CAUSE",
        "SESSION", "REQUESTED/SERVED", "TURNS", "OUTCOME", "LOCAL EST.", "BILLED", "CACHED%"
    );
    println!("{}", "-".repeat(128));

    for s in sessions {
        let sid = s.get("session_id").and_then(|v| v.as_str()).unwrap_or("?");
        let label = s
            .get("display_name")
            .and_then(|v| v.as_str())
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(sid);
        let short_sid = truncate_for_box(label, 20);
        let model = session_model_label(s);
        let short_model = truncate_for_box(&model, 24);
        let turns = s.get("total_turns").and_then(|v| v.as_i64()).unwrap_or(0);
        let outcome = s.get("outcome").and_then(|v| v.as_str()).unwrap_or("?");
        let cost = s
            .get("local_estimate_total_cost_dollars")
            .or_else(|| s.get("estimated_total_cost_dollars"))
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let billed_cost = s.get("billed_cost_dollars").and_then(|v| v.as_f64());
        let cached_input = s
            .get("codex_cached_input_ratio")
            .and_then(|v| v.as_f64())
            .map(|ratio| ratio * 100.0);
        let cause = s
            .get("primary_cause")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let degraded = s.get("degraded").and_then(|v| v.as_bool()).unwrap_or(false);
        let billed_display = billed_cost
            .map(|value| format!("${value:.2}"))
            .unwrap_or_else(|| "-".to_string());

        let line = format!(
            "{:<22} {:<24} {:<8} {:<22} ${:<15.2} {:<14} {:<8} {}",
            short_sid,
            short_model,
            turns,
            outcome,
            cost,
            billed_display,
            cached_input
                .map(|value| format!("{value:.0}%"))
                .unwrap_or_else(|| "-".to_string()),
            cause
        );

        if degraded {
            println!("{}", line.yellow());
        } else if outcome.contains("Completed") && !outcome.contains("Partially") {
            println!("{}", line.green());
        } else if outcome.contains("Abandoned") {
            println!("{}", line.dimmed());
        } else {
            println!("{}", line);
        }
    }

    Ok(())
}

fn session_model_label(session: &serde_json::Value) -> String {
    let requested = session
        .get("requested_model")
        .and_then(|v| v.as_str())
        .filter(|value| !value.trim().is_empty());
    let served = session
        .get("served_model")
        .and_then(|v| v.as_str())
        .filter(|value| !value.trim().is_empty());
    match (requested, served) {
        (Some(requested), Some(served)) if requested != served => format!("{requested}->{served}"),
        (Some(requested), _) => requested.to_string(),
        (None, Some(served)) => served.to_string(),
        (None, None) => session
            .get("model")
            .and_then(|v| v.as_str())
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("?")
            .to_string(),
    }
}

async fn fetch_postmortem(
    base_url: &str,
    target: &str,
    redact: bool,
    output: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let target_path = if target == "last" {
        "last".to_string()
    } else {
        encode_path_segment(target)
    };
    let url = format!(
        "{}/api/postmortem/{}",
        base_url.trim_end_matches('/'),
        target_path
    );
    let resp = reqwest::Client::new()
        .get(&url)
        .query(&[("redact", redact.to_string())])
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()).into());
    }
    let report: serde_json::Value = resp.json().await?;
    let markdown = render_postmortem_markdown(&report);
    if let Some(path) = output {
        fs::write(path, markdown)
            .map_err(|err| format!("failed to write {}: {}", path.display(), err))?;
    } else {
        print!("{}", markdown);
    }
    Ok(())
}

fn encode_path_segment(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn render_postmortem_markdown(report: &serde_json::Value) -> String {
    let mut out = String::new();
    out.push_str("# Codex Responses Postmortem\n\n");
    render_postmortem_snapshot(report, &mut out);
    render_postmortem_signals(report, &mut out);
    render_postmortem_evidence(report, &mut out);
    render_postmortem_timeline(report, &mut out);
    render_postmortem_recommendations(report, &mut out);
    render_postmortem_caveats(report, &mut out);
    if let Some(prompt) = report
        .get("restart_prompt")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
    {
        out.push_str("\n## Restart Prompt\n");
        out.push_str("```text\n");
        out.push_str(prompt);
        out.push_str("\n```\n");
    }
    out
}

fn render_postmortem_snapshot(report: &serde_json::Value, out: &mut String) {
    let summary = report.get("summary").unwrap_or(&serde_json::Value::Null);
    let impact = report.get("impact").unwrap_or(&serde_json::Value::Null);
    let diagnosis = report.get("diagnosis").unwrap_or(&serde_json::Value::Null);
    out.push_str("## Snapshot\n");
    push_md_line(
        out,
        "Session",
        json_str(report, "session_id").unwrap_or("?"),
    );
    push_md_line(
        out,
        "State",
        if report
            .get("partial")
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
        {
            "partial snapshot"
        } else {
            "final or persisted snapshot"
        },
    );
    push_md_line(out, "Outcome", json_str(summary, "outcome").unwrap_or("?"));
    if let Some(model) = json_str(summary, "requested_model") {
        push_md_line(out, "Requested Model", model);
    }
    if let Some(model) = json_str(summary, "served_model") {
        push_md_line(out, "Served Model", model);
    }
    let turn_count = summary
        .get("turn_count")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    push_md_line(out, "Turns", &turn_count.to_string());
    let token_line = format!(
        "input {}, cached {}, uncached {}, output {}, reasoning {}, local total {}",
        number_field(impact, "input_tokens"),
        number_field(impact, "cached_input_tokens"),
        number_field(impact, "uncached_input_tokens"),
        number_field(impact, "output_tokens"),
        number_field(impact, "reasoning_output_tokens"),
        number_field(impact, "local_total_tokens"),
    );
    push_md_line(out, "Tokens", &token_line);
    let cost = impact
        .get("local_estimated_cost_dollars")
        .and_then(|value| value.as_f64())
        .unwrap_or(0.0);
    push_md_line(out, "Local Estimate", &format!("${cost:.2}"));
    if let Some(source) = json_str(impact, "local_estimate_source") {
        push_md_line(out, "Local Estimate Source", source);
    }
    if let Some(trusted) = impact
        .get("local_estimate_trusted_for_budget_enforcement")
        .and_then(|value| value.as_bool())
    {
        push_md_line(
            out,
            "Local Estimate Trust",
            if trusted {
                "trusted for budget enforcement"
            } else {
                "untrusted for budget enforcement"
            },
        );
    }
    if let Some(billed) = impact
        .get("billed_reconciliation")
        .and_then(|value| value.as_object())
        .and_then(|map| map.get("billed_cost_dollars"))
        .and_then(|value| value.as_f64())
    {
        push_md_line(out, "Billed", &format!("${billed:.2}"));
    }
    push_md_line(
        out,
        "Primary Cause",
        json_str(diagnosis, "primary_cause").unwrap_or("none"),
    );
    if let Some(prompt) = json_str(summary, "initial_prompt_excerpt") {
        push_md_line(out, "Prompt", prompt);
    }
    if let Some(summary_text) = json_str(summary, "final_response_summary") {
        push_md_line(out, "Final Summary", summary_text);
    }
}

fn render_postmortem_signals(report: &serde_json::Value, out: &mut String) {
    let Some(signals) = report.get("signals") else {
        return;
    };
    out.push_str("\n## Signals\n");
    if let Some(statuses) = signals
        .get("response_statuses")
        .and_then(|value| value.as_object())
    {
        let rendered = statuses
            .iter()
            .filter_map(|(status, count)| count.as_u64().map(|count| (status, count)))
            .filter(|(_, count)| *count > 0)
            .map(|(status, count)| format!("{status}: {count}"))
            .collect::<Vec<_>>()
            .join(", ");
        if !rendered.is_empty() {
            push_md_bullet(out, &format!("Responses statuses: {rendered}"));
        }
    }
    if let Some(context) = signals.get("context_fill") {
        let percent = context
            .get("max_percent")
            .and_then(|value| value.as_f64())
            .unwrap_or(0.0);
        push_md_bullet(out, &format!("Estimated context fill max: {percent:.1}%"));
    }
    if let Some(cache) = signals.get("cached_input_reuse") {
        if let Some(ratio) = cache.get("ratio").and_then(|value| value.as_f64()) {
            push_md_bullet(out, &format!("Cached input reuse: {:.0}%", ratio * 100.0));
        }
    }
    if let Some(reasoning) = signals.get("reasoning_output_share") {
        if let Some(ratio) = reasoning.get("max_ratio").and_then(|value| value.as_f64()) {
            push_md_bullet(
                out,
                &format!("Max reasoning-output share: {:.0}%", ratio * 100.0),
            );
        }
    }
    if let Some(counts) = signals
        .get("tool_call_intent_counts")
        .and_then(|value| value.as_object())
    {
        let rendered = counts
            .iter()
            .filter_map(|(tool, count)| count.as_u64().map(|count| (tool, count)))
            .filter(|(_, count)| *count > 0)
            .map(|(tool, count)| format!("{tool}: {count}"))
            .collect::<Vec<_>>()
            .join(", ");
        if !rendered.is_empty() {
            push_md_bullet(out, &format!("Tool-call intent: {rendered}"));
        }
    }
}

fn render_postmortem_evidence(report: &serde_json::Value, out: &mut String) {
    let rows = report
        .get("evidence")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    if rows.is_empty() {
        return;
    }
    out.push_str("\n## Evidence\n");
    for row in rows.iter().take(12) {
        let kind = json_str(row, "type").unwrap_or("signal");
        let signal = json_str(row, "signal").unwrap_or("unknown");
        let turn = row
            .get("turn")
            .and_then(|value| value.as_u64())
            .unwrap_or(0);
        let detail = json_str(row, "detail").unwrap_or("");
        push_md_bullet(out, &format!("[{kind}] turn {turn} {signal}: {detail}"));
    }
}

fn render_postmortem_timeline(report: &serde_json::Value, out: &mut String) {
    let rows = report
        .get("timeline")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    if rows.is_empty() {
        return;
    }
    out.push_str("\n## Timeline\n");
    for row in rows.iter().take(14) {
        let timestamp = json_str(row, "timestamp").unwrap_or("");
        let event = json_str(row, "event").unwrap_or("event");
        let detail = json_str(row, "detail").unwrap_or("");
        push_md_bullet(out, &format!("{timestamp} {event}: {detail}"));
    }
}

fn render_postmortem_recommendations(report: &serde_json::Value, out: &mut String) {
    let rows = report
        .get("recommendations")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    if rows.is_empty() {
        return;
    }
    out.push_str("\n## Recommendations\n");
    for row in rows {
        if let Some(item) = row.as_str() {
            push_md_bullet(out, item);
        }
    }
}

fn render_postmortem_caveats(report: &serde_json::Value, out: &mut String) {
    let rows = report
        .get("caveats")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    if rows.is_empty() {
        return;
    }
    out.push_str("\n## Caveats\n");
    for row in rows {
        if let Some(item) = row.as_str() {
            push_md_bullet(out, item);
        }
    }
}

fn push_md_line(out: &mut String, label: &str, value: &str) {
    out.push_str("- ");
    out.push_str(label);
    out.push_str(": ");
    out.push_str(value);
    out.push('\n');
}

fn push_md_bullet(out: &mut String, value: &str) {
    out.push_str("- ");
    out.push_str(value);
    out.push('\n');
}

fn json_str<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(|value| value.as_str())
}

fn number_field(value: &serde_json::Value, key: &str) -> String {
    value
        .get(key)
        .and_then(|value| value.as_u64())
        .map(|number| number.to_string())
        .unwrap_or_else(|| "0".to_string())
}

async fn post_reconciliation(
    url: &str,
    session: &str,
    billed_cost: f64,
    source: &str,
    imported_at: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    let payload = BillingReconciliationInput {
        session_id: session.to_string(),
        source: source.to_string(),
        billed_cost_dollars: billed_cost,
        imported_at: imported_at.map(|value| value.to_string()),
    };
    let resp = reqwest::Client::new()
        .post(url)
        .json(&payload)
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()).into());
    }
    let body: serde_json::Value = resp.json().await?;
    let inserted = body.get("inserted").and_then(|v| v.as_u64()).unwrap_or(0);
    println!(
        "{}",
        format!(
            "Imported {} billed reconciliation{}.",
            inserted,
            if inserted == 1 { "" } else { "s" }
        )
        .green()
    );
    Ok(())
}

async fn fetch_recall(
    url: &str,
    query: &str,
    limit: u32,
    days: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    let resp = reqwest::Client::new()
        .get(url)
        .query(&[
            ("q", query.to_string()),
            ("limit", limit.to_string()),
            ("days", days.to_string()),
        ])
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()).into());
    }

    let body: serde_json::Value = resp.json().await?;
    let hits = body.get("hits").and_then(|h| h.as_array());
    let Some(hits) = hits else {
        println!("No matches.");
        return Ok(());
    };
    if hits.is_empty() {
        println!("No matches for \"{}\".", query);
        return Ok(());
    }

    println!("Recall results for \"{}\":", query);
    println!();

    for (idx, hit) in hits.iter().enumerate() {
        let score = hit.get("score").and_then(|v| v.as_i64()).unwrap_or(0);
        let session_id = hit
            .get("session_id")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let started_at = hit
            .get("started_at")
            .and_then(|v| v.as_str())
            .map(compact_datetime_from_iso)
            .unwrap_or_else(|| "unknown time".to_string());
        let model = hit
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string();
        let outcome = hit.get("outcome").and_then(|v| v.as_str()).unwrap_or("?");
        let initial_prompt = hit
            .get("initial_prompt")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let final_response_summary = hit
            .get("final_response_summary")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let rank_line = format!(
            "{}. [{}] {} · {} · {}",
            idx + 1,
            score,
            session_id,
            model,
            started_at
        );
        println!("{}", rank_line.bold());
        println!("    Outcome: {}", outcome);
        if !initial_prompt.is_empty() {
            println!("    Prompt: {}", initial_prompt);
        }
        if !final_response_summary.is_empty() {
            println!("    Landed: {}", final_response_summary);
        }
        println!();
    }

    Ok(())
}

async fn connect_and_stream(
    url: &str,
    no_signals: bool,
    session_filter: &Option<String>,
    active: &mut ActiveSessions,
) -> Result<(), Box<dyn std::error::Error>> {
    let resp = reqwest::Client::new()
        .get(url)
        .header("Accept", "text/event-stream")
        .send()
        .await?;

    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()).into());
    }

    eprintln!("{}", "Connected. Watching for events...".green());

    let mut stream = resp.bytes_stream();
    use futures_util::StreamExt;

    let mut line_buffer = String::new();
    let mut data_buffer = String::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        let text = String::from_utf8_lossy(&chunk);
        line_buffer.push_str(&text);

        while let Some(newline_pos) = line_buffer.find('\n') {
            let line = line_buffer[..newline_pos]
                .trim_end_matches('\r')
                .to_string();
            line_buffer = line_buffer[newline_pos + 1..].to_string();

            if let Some(data) = line.strip_prefix("data: ") {
                data_buffer.push_str(data);
            } else if line.starts_with(": ") || line.starts_with(':') {
                continue;
            } else if line.is_empty() && !data_buffer.is_empty() {
                if let Ok(event) = serde_json::from_str::<WatchEvent>(&data_buffer) {
                    render_event(&event, no_signals, session_filter, active);
                }
                data_buffer.clear();
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        build_child_run_plan, chatgpt_codex_proxy_base_url, codex_command_requires_observation,
        codex_model_proxy_base_url, codex_subscription_config_overrides, codex_turn_summary_line,
        compact_datetime_from_iso, context_status_line, event_session_id, extract_run_watch,
        format_duration_coarse, format_tokens, local_time_from_iso, model_change_line,
        parse_codex_requests_total, parse_mcp_tool_name, push_unique, render_child_run_plan,
        render_codex_config_preview, shell_join, shell_quote, should_suppress_codex_stderr_line,
        tmux_orchestrator_watch_url, truncate_for_box, watch_model_label, yaml_quote,
        ActiveSessions, ChildStdinMode, Cli, CodexTurnSummaryLine, Commands, ConfigCommands,
        RunMode, WatchEvent, WatchRetryLog,
    };
    use chrono::{DateTime, Local};
    use clap::Parser;
    use std::fs;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn unique_test_dir(label: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time went backwards")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "codex-blackbox-cli-{label}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create test dir");
        path
    }

    fn serve_sse_chunks_once(chunks: Vec<String>) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind sse server");
        let url = format!("http://{}", listener.local_addr().expect("local addr"));
        let (tx, rx) = mpsc::channel();

        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept sse request");
            let mut request = Vec::new();
            let mut buffer = [0u8; 1024];
            loop {
                let n = stream.read(&mut buffer).expect("read request");
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..n]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            tx.send(String::from_utf8_lossy(&request).into_owned())
                .expect("send captured request");

            let content_len: usize = chunks.iter().map(|chunk| chunk.len()).sum();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                content_len
            )
            .expect("write sse response headers");

            for chunk in chunks {
                stream.write_all(chunk.as_bytes()).expect("write sse chunk");
                stream.flush().expect("flush sse chunk");
                thread::sleep(Duration::from_millis(5));
            }
        });

        (url, rx)
    }

    #[test]
    fn local_time_from_iso_converts_from_rfc3339() {
        let iso = "2026-04-21T13:04:39Z";
        let expected = DateTime::parse_from_rfc3339(iso)
            .expect("valid RFC3339 timestamp")
            .with_timezone(&Local)
            .format("%H:%M:%S")
            .to_string();
        assert_eq!(local_time_from_iso(iso), expected);
    }

    #[test]
    fn compact_datetime_from_iso_converts_from_rfc3339() {
        let iso = "2026-04-21T13:04:39Z";
        let expected = DateTime::parse_from_rfc3339(iso)
            .expect("valid RFC3339 timestamp")
            .with_timezone(&Local)
            .format("%Y-%m-%d %H:%M")
            .to_string();
        assert_eq!(compact_datetime_from_iso(iso), expected);
    }

    #[test]
    fn local_time_from_iso_falls_back_for_invalid_timestamps() {
        assert_eq!(local_time_from_iso("not-a-timestamp"), "??:??:??");
    }

    #[test]
    fn formatting_helpers_render_compact_user_text() {
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(12_345), "12K");
        assert_eq!(format_tokens(3_400_000), "3.4M");

        assert_eq!(format_duration_coarse(45), "45s");
        assert_eq!(format_duration_coarse(12 * 60), "12m");
        assert_eq!(format_duration_coarse(3 * 60 * 60 + 20 * 60), "3h 20m");
        assert_eq!(
            format_duration_coarse(2 * 24 * 60 * 60 + 5 * 60 * 60),
            "2d 5h"
        );

        assert_eq!(truncate_for_box("short", 10), "short");
        assert_eq!(truncate_for_box("abcdef", 4), "abc\u{2026}");
        assert_eq!(truncate_for_box("åäöabc", 4), "åäö\u{2026}");
    }

    #[test]
    fn shell_and_yaml_quoting_preserve_command_arguments() {
        assert_eq!(shell_quote("abc/def-123"), "abc/def-123");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("hello world"), "'hello world'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");

        assert_eq!(
            shell_join(&["codex-blackbox".to_string(), "hello world".to_string()]),
            "codex-blackbox 'hello world'"
        );
        assert_eq!(yaml_quote(r#"a\b"c"#), r#""a\\b\"c""#);
    }

    #[test]
    fn watch_labels_codex_models_without_legacy_provider_branding() {
        assert_eq!(
            watch_model_label("gpt-codex-fixture"),
            "CODEX \u{00b7} gpt-codex-fixture"
        );
        assert_eq!(
            watch_model_label("unknown-model-fixture"),
            "unknown-model-fixture"
        );

        let line = model_change_line("12:00:00", "gpt-codex", "gpt-codex-served");
        assert!(line.contains("MODEL CHANGE"));
        assert!(line.contains("served gpt-codex-served"));
        assert!(!line.contains("provider fallback"));
        assert!(!line.contains("fallback"));
    }

    #[test]
    fn watch_context_and_codex_turn_lines_use_codex_token_language() {
        let context = context_status_line("12:00:00", 75.0, Some(200_000), None);
        assert_eq!(
            context,
            "12:00:00  CONTEXT  75% of 200K window \u{00b7} trajectory unknown"
        );
        assert!(!context.contains("cache"));

        let summary = codex_turn_summary_line(CodexTurnSummaryLine {
            time: "12:00:00",
            status: "completed",
            requested_model: "gpt-codex-fixture",
            served_model: Some("gpt-codex-served"),
            input_tokens: 1_280,
            cached_input_tokens: 512,
            uncached_input_tokens: 768,
            output_tokens: 96,
            reasoning_output_tokens: 32,
            total_tokens: 1_376,
        });
        assert!(summary.contains("CODEX   completed"));
        assert!(summary.contains("input 1K (512 cached, 768 uncached)"));
        assert!(summary.contains("output 96 + 32 reasoning"));
        assert!(summary.contains("total 1K"));
        assert!(!summary.contains("expires"));
        assert!(!summary.contains("rebuild"));
    }

    #[test]
    fn command_path_accepts_explicit_executable_paths() {
        let dir = unique_test_dir("command-path");
        let executable = dir.join("fake-command");
        {
            let mut file = fs::File::create(&executable).expect("create executable");
            writeln!(file, "#!/bin/sh").expect("write executable");
        }
        #[cfg(unix)]
        {
            let mut permissions = fs::metadata(&executable).expect("metadata").permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&executable, permissions).expect("chmod executable");
        }

        assert_eq!(
            super::command_path(executable.to_str().expect("utf8 path")),
            Some(executable.clone())
        );
        assert!(super::command_exists(
            executable.to_str().expect("utf8 path")
        ));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn write_if_changed_creates_parent_dirs_and_preserves_identical_files() {
        let dir = unique_test_dir("write-if-changed");
        let path = dir.join("nested/config.yml");

        super::write_if_changed(&path, "one").expect("first write");
        let modified = fs::metadata(&path).expect("metadata").modified().ok();
        super::write_if_changed(&path, "one").expect("same write");
        assert_eq!(fs::read_to_string(&path).expect("read"), "one");
        if let Some(modified) = modified {
            assert_eq!(
                fs::metadata(&path).expect("metadata").modified().ok(),
                Some(modified)
            );
        }

        super::write_if_changed(&path, "two").expect("changed write");
        assert_eq!(fs::read_to_string(&path).expect("read"), "two");

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn run_watch_after_child_command_is_codex_blackbox_flag() {
        let cli = Cli::try_parse_from(["codex-blackbox", "run", "codex", "--watch"])
            .expect("run command parses");
        let Commands::Run {
            watch,
            dry_run,
            command,
        } = cli.command
        else {
            panic!("expected run command");
        };
        assert!(!dry_run);
        let (watch, command) = extract_run_watch(watch, command);
        assert!(watch);
        assert_eq!(command, vec!["codex"]);
    }

    #[test]
    fn run_watch_before_child_command_is_codex_blackbox_flag() {
        let cli = Cli::try_parse_from(["codex-blackbox", "run", "--watch", "codex"])
            .expect("run command parses");
        let Commands::Run {
            watch,
            dry_run,
            command,
        } = cli.command
        else {
            panic!("expected run command");
        };
        assert!(!dry_run);
        let (watch, command) = extract_run_watch(watch, command);
        assert!(watch);
        assert_eq!(command, vec!["codex"]);
    }

    #[test]
    fn run_preserves_child_flags() {
        let cli = Cli::try_parse_from([
            "codex-blackbox",
            "run",
            "codex",
            "--dangerously-skip-permissions",
            "--model",
            "gpt-5.5",
        ])
        .expect("run command parses");
        let Commands::Run {
            watch,
            dry_run,
            command,
        } = cli.command
        else {
            panic!("expected run command");
        };
        assert!(!dry_run);
        let (watch, command) = extract_run_watch(watch, command);
        assert!(!watch);
        assert_eq!(
            command,
            vec![
                "codex",
                "--dangerously-skip-permissions",
                "--model",
                "gpt-5.5"
            ]
        );
    }

    fn has_config_override(args: &[String], override_arg: &str) -> bool {
        args.windows(2)
            .any(|window| window[0] == "-c" && window[1] == override_arg)
    }

    #[test]
    fn codex_run_plan_uses_subscription_proxy_config_overrides_by_default() {
        let child_command = vec![
            "codex".to_string(),
            "exec".to_string(),
            "hello".to_string(),
            "--json".to_string(),
        ];
        let plan = build_child_run_plan(&child_command).expect("codex run plan");
        let overrides = codex_subscription_config_overrides(
            &chatgpt_codex_proxy_base_url(),
            &codex_model_proxy_base_url(),
        );
        let args_after_exec_and_overrides = 1 + overrides.len() * 2;

        assert_eq!(plan.mode, RunMode::CodexSubscriptionProxy);
        assert_eq!(plan.command, "codex");
        assert!(plan.envs.is_empty());
        assert_eq!(
            plan.env_removals,
            [
                "CODEX_CI".to_string(),
                "CODEX_INTERNAL_ORIGINATOR_OVERRIDE".to_string(),
                "CODEX_SHELL".to_string(),
                "CODEX_THREAD_ID".to_string()
            ]
        );
        for override_arg in &overrides {
            assert!(
                has_config_override(&plan.args, override_arg),
                "missing override {override_arg:?} in {:?}",
                plan.args
            );
        }
        assert!(has_config_override(
            &plan.args,
            "features.enable_request_compression=false"
        ));
        assert!(has_config_override(
            &plan.args,
            "chatgpt_base_url=\"http://127.0.0.1:10000/backend-api\""
        ));
        assert!(has_config_override(
            &plan.args,
            "model_provider=\"codex-blackbox-chatgpt\""
        ));
        assert!(has_config_override(
            &plan.args,
            "model_providers.codex-blackbox-chatgpt.base_url=\"http://127.0.0.1:10000/backend-api/codex\""
        ));
        assert!(has_config_override(
            &plan.args,
            "model_providers.codex-blackbox-chatgpt.wire_api=\"responses\""
        ));
        assert!(has_config_override(
            &plan.args,
            "model_providers.codex-blackbox-chatgpt.requires_openai_auth=true"
        ));
        assert!(has_config_override(
            &plan.args,
            "model_providers.codex-blackbox-chatgpt.supports_websockets=false"
        ));
        assert!(plan.requires_codex_observation);
        assert_eq!(plan.stdin_mode, ChildStdinMode::Null);
        assert!(!plan.args.iter().any(|arg| arg.contains("OPENAI_API_KEY")));
        assert!(!plan
            .args
            .iter()
            .any(|arg| arg.contains("forced_login_method")));
        assert!(!plan.args.iter().any(|arg| arg.contains("env_key")));
        assert_eq!(plan.args[0], "exec");
        assert_eq!(
            plan.args[args_after_exec_and_overrides..],
            ["hello".to_string()]
        );
        assert!(
            !plan.args.iter().any(|arg| arg == "--json"),
            "Codex Blackbox must not pass local Codex JSON stdout mode: {:?}",
            plan.args
        );
    }

    #[test]
    fn codex_run_plan_handles_explicit_codex_path() {
        let child_command = vec!["/opt/homebrew/bin/codex".to_string(), "--help".to_string()];
        let plan = build_child_run_plan(&child_command).expect("codex path run plan");

        assert_eq!(plan.mode, RunMode::CodexSubscriptionProxy);
        assert_eq!(plan.command, "/opt/homebrew/bin/codex");
        assert!(plan.envs.is_empty());
        assert!(plan.env_removals.contains(&"CODEX_THREAD_ID".to_string()));
        assert_eq!(plan.stdin_mode, ChildStdinMode::Inherit);
        assert!(!plan.requires_codex_observation);
        assert!(plan.args.ends_with(&["--help".to_string()]));
    }

    #[test]
    fn codex_observation_is_required_only_for_exec_turns() {
        assert!(codex_command_requires_observation(&[
            "exec".to_string(),
            "hello".to_string()
        ]));
        assert!(codex_command_requires_observation(&[
            "-m".to_string(),
            "gpt-5.4".to_string(),
            "e".to_string(),
            "hello".to_string()
        ]));
        assert!(!codex_command_requires_observation(&["--help".to_string()]));
        assert!(!codex_command_requires_observation(&[
            "login".to_string(),
            "status".to_string()
        ]));
    }

    #[test]
    fn tmux_orchestrator_requests_persisted_recent_replay() {
        assert_eq!(
            tmux_orchestrator_watch_url("http://localhost:9091/"),
            "http://localhost:9091/watch?replay=recent"
        );
    }

    #[test]
    fn watch_retry_log_suppresses_duplicate_waiting_messages() {
        let mut retry_log = WatchRetryLog::default();

        assert_eq!(
            retry_log.retry_message("connection refused").as_deref(),
            Some("Waiting for codex-blackbox-core... (retrying every 3s; connection refused)")
        );
        assert_eq!(retry_log.retry_message("connection refused"), None);
        assert_eq!(retry_log.retry_message("connection refused"), None);
        assert_eq!(
            retry_log.retry_message("HTTP 503").as_deref(),
            Some("Waiting for codex-blackbox-core... (retrying every 3s; HTTP 503)")
        );

        retry_log.reset();

        assert_eq!(
            retry_log.retry_message("HTTP 503").as_deref(),
            Some("Waiting for codex-blackbox-core... (retrying every 3s; HTTP 503)")
        );
    }

    #[test]
    fn non_codex_run_plan_is_plain_without_proxy_overrides() {
        let child_command = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "printf ok".to_string(),
        ];
        let plan = build_child_run_plan(&child_command).expect("plain run plan");

        assert_eq!(plan.mode, RunMode::PlainCommand);
        assert_eq!(plan.command, "/bin/sh");
        assert_eq!(plan.args, ["-c".to_string(), "printf ok".to_string()]);
        assert!(plan.envs.is_empty());
        assert!(plan.env_removals.is_empty());
        assert_eq!(plan.stdin_mode, ChildStdinMode::Inherit);
        assert!(!plan
            .args
            .iter()
            .any(|arg| arg.contains("codex-blackbox-openai-responses")));
    }

    #[test]
    fn run_plan_preview_is_read_only_for_codex() {
        let child_command = vec!["codex".to_string(), "exec".to_string(), "hello".to_string()];
        let plan = build_child_run_plan(&child_command).expect("codex run plan");
        let preview = render_child_run_plan(&plan);

        assert!(preview.contains("Codex Blackbox run preview"));
        assert!(preview.contains("experimental Codex ChatGPT subscription proxy"));
        assert!(preview.contains("Config files: not modified"));
        assert!(preview.contains("Environment overrides:\n  (none)"));
        assert!(preview.contains("Environment removals:\n  CODEX_CI"));
        assert!(preview.contains("CODEX_INTERNAL_ORIGINATOR_OVERRIDE"));
        assert!(preview.contains("CODEX_SHELL"));
        assert!(preview.contains("CODEX_THREAD_ID"));
        assert!(preview.contains("Child stdin: closed for Codex exec"));
        assert!(preview.contains("features.enable_request_compression=false"));
        assert!(preview.contains(
            "Model provider override: codex-blackbox-chatgpt (ChatGPT auth, Responses HTTP)"
        ));
        assert!(preview.contains("wrapper does not inject --ephemeral"));
        assert!(preview.contains("Known Codex rollout-recording warning: suppressed"));
        assert!(preview.contains("OPENAI_API_KEY is not used"));
        assert!(preview.contains(
            "Post-run check: require Codex Blackbox to observe at least one Codex Responses request"
        ));
        assert!(preview.contains("http://127.0.0.1:10000/backend-api"));
        assert!(preview.contains("http://127.0.0.1:10000/backend-api/codex"));
        assert!(preview.contains("codex exec -c"));
        assert!(preview.contains("hello"));
    }

    #[test]
    fn codex_request_metric_parser_sums_codex_provider() {
        let metrics = r#"
# HELP codex_blackbox_requests_total Total requests
# TYPE codex_blackbox_requests_total counter
codex_blackbox_requests_total{model="gpt-5.5",provider="codex_responses"} 2
codex_blackbox_requests_total{model="gpt-5.4",provider="codex_responses"} 3
codex_blackbox_requests_total{model="legacy_model",provider="legacy_provider"} 99
"#;

        assert_eq!(parse_codex_requests_total(metrics), Some(5.0));
        assert_eq!(parse_codex_requests_total("other_metric 1"), None);
    }

    #[test]
    fn codex_rollout_recording_warning_is_suppressed_narrowly() {
        assert!(should_suppress_codex_stderr_line(
            "2026-04-30T16:36:44Z ERROR codex_core::session: failed to record rollout items: thread missing"
        ));
        assert!(should_suppress_codex_stderr_line(
            "Reading additional input from stdin..."
        ));
        assert!(should_suppress_codex_stderr_line(
            "write_stdin failed: stdin is closed for this session; rerun exec_command with tty=true to keep stdin open"
        ));
        assert!(!should_suppress_codex_stderr_line(
            "2026-04-30T16:36:44Z ERROR codex_core::session: failed to connect to websocket"
        ));
        assert!(!should_suppress_codex_stderr_line(
            "ERROR codex_core::session: failed to read stdin: permission denied"
        ));
    }

    #[test]
    fn parser_applies_command_defaults() {
        let cli = Cli::try_parse_from(["codex-blackbox", "watch"]).expect("watch parses");
        let Commands::Watch {
            url,
            no_signals,
            session,
            tmux,
            tmux_max_panes,
        } = cli.command
        else {
            panic!("expected watch command");
        };
        assert_eq!(url, "http://localhost:9091");
        assert!(!no_signals);
        assert_eq!(session, None);
        assert!(!tmux);
        assert_eq!(tmux_max_panes, 8);

        let cli = Cli::try_parse_from(["codex-blackbox", "sessions"]).expect("sessions parses");
        let Commands::Sessions { url, limit, days } = cli.command else {
            panic!("expected sessions command");
        };
        assert_eq!(url, "http://localhost:9091");
        assert_eq!(limit, 20);
        assert_eq!(days, 7);

        let cli = Cli::try_parse_from(["codex-blackbox", "recall", "auth"]).expect("recall parses");
        let Commands::Recall {
            url,
            limit,
            days,
            query,
        } = cli.command
        else {
            panic!("expected recall command");
        };
        assert_eq!(url, "http://localhost:9091");
        assert_eq!(limit, 5);
        assert_eq!(days, 30);
        assert_eq!(query, vec!["auth"]);

        let cli =
            Cli::try_parse_from(["codex-blackbox", "config", "codex"]).expect("config parses");
        let Commands::Config { command } = cli.command else {
            panic!("expected config command");
        };
        assert!(matches!(command, ConfigCommands::Codex));
    }

    #[test]
    fn codex_config_preview_is_read_only_and_subscription_only() {
        let preview = render_codex_config_preview();

        assert!(preview.contains("read-only"));
        assert!(preview.contains("experimental ChatGPT subscription wrapper"));
        assert!(preview.contains("codex-blackbox up"));
        assert!(preview.contains("~/.codex/config.toml is not modified"));
        assert!(preview.contains(r#"-c 'chatgpt_base_url="http://127.0.0.1:10000/backend-api"'"#));
        assert!(preview.contains(r#"-c 'model_provider="codex-blackbox-chatgpt"'"#));
        assert!(preview.contains(
            r#"-c 'model_providers.codex-blackbox-chatgpt.base_url="http://127.0.0.1:10000/backend-api/codex"'"#
        ));
        assert!(
            preview.contains("-c model_providers.codex-blackbox-chatgpt.supports_websockets=false")
        );
        assert!(preview.contains("-c features.enable_request_compression=false"));
        assert!(preview.contains("does not pass codex exec --json"));
        assert!(preview.contains("Envoy-observed Responses traffic is the telemetry source"));
        assert!(preview.contains("does not use OPENAI_API_KEY"));
        assert!(preview.contains("Codex CLI mode requires an existing Codex ChatGPT login"));
        assert!(!preview.contains("forced_login_method"));
        assert!(!preview.contains("openai_base_url"));
    }

    #[test]
    fn active_sessions_tags_only_when_multiple_sessions_exist() {
        let mut active = ActiveSessions::new();
        active.add("session_a", "api");
        assert!(!active.is_multi());
        assert_eq!(active.tag_for("session_a"), "");

        active.add("session_b", "worker-long");
        assert!(active.is_multi());
        assert_eq!(active.tag_for("session_a"), "[api        ]  ");
        assert_eq!(active.tag_for("missing"), "[?          ]  ");

        active.remove("session_b");
        assert!(!active.is_multi());
    }

    #[tokio::test]
    async fn watch_stream_consumes_sse_chunks_and_applies_session_filter() {
        let chunks = vec![
            ": keepalive\n\n".to_string(),
            concat!(
                "data: {\"type\":\"session_start\",\"session_id\":\"session_other\",",
                "\"display_name\":\"other\",\"model\":\"gpt-5.5\"}\n\n"
            )
            .to_string(),
            concat!(
                "data: {\"type\":\"session_start\",\"session_id\":\"session_target\",",
                "\"display_name\":\"api\",\"model\":\"gpt-5.4\",",
                "\"initial_prompt\":\"investigate auth\"}\n\n"
            )
            .to_string(),
            concat!(
                "data: {\"type\":\"tool_use\",\"session_id\":\"session_target\",",
                "\"timestamp\":\"2026-04-28T00:00:00Z\",",
                "\"tool_name\":\"Read\",\"summary\":\"src/main.rs\"}\n\n"
            )
            .to_string(),
            concat!(
                "data: {\"type\":\"session_end\",\"session_id\":\"session_target\",",
                "\"outcome\":\"Likely Completed\",\"total_tokens\":1234,\"total_turns\":3}\n\n"
            )
            .to_string(),
        ];
        let (url, request_rx) = serve_sse_chunks_once(chunks);
        let mut active = ActiveSessions::new();
        let filter = Some("session_target".to_string());

        super::connect_and_stream(&url, false, &filter, &mut active)
            .await
            .expect("watch stream closes cleanly");

        let request = request_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("captured watch request");
        assert!(
            request.starts_with("GET / "),
            "unexpected request:\n{request}"
        );
        assert!(
            request
                .to_ascii_lowercase()
                .contains("accept: text/event-stream"),
            "missing SSE accept header:\n{request}"
        );
        assert!(
            active.sessions.is_empty(),
            "target session should be removed after session_end"
        );
    }

    #[test]
    fn watch_event_session_ids_skip_global_events() {
        let event = WatchEvent::ToolUse {
            session_id: "session_a".to_string(),
            timestamp: "2026-04-28T00:00:00Z".to_string(),
            tool_name: "Read".to_string(),
            summary: "src/main.rs".to_string(),
        };
        assert_eq!(event_session_id(&event), Some("session_a"));
        assert_eq!(
            event_session_id(&WatchEvent::CodexTurnSummary {
                session_id: "session_codex".to_string(),
                status: "completed".to_string(),
                requested_model: "gpt-codex-fixture".to_string(),
                served_model: None,
                input_tokens: 10,
                cached_input_tokens: 4,
                uncached_input_tokens: 6,
                output_tokens: 2,
                reasoning_output_tokens: 1,
                total_tokens: 12,
            }),
            Some("session_codex")
        );

        assert_eq!(event_session_id(&WatchEvent::Lagged { missed: 3 }), None);
    }

    #[test]
    fn mcp_tool_names_split_server_and_tool() {
        assert_eq!(
            parse_mcp_tool_name("mcp__github__get_issue"),
            Some(("github", "get_issue"))
        );
        assert_eq!(
            parse_mcp_tool_name(" mcp__server__tool__suffix "),
            Some(("server", "tool__suffix"))
        );
        assert_eq!(parse_mcp_tool_name("Read"), None);
        assert_eq!(parse_mcp_tool_name("mcp__github"), None);
        assert_eq!(parse_mcp_tool_name("mcp____tool"), None);
    }

    #[test]
    fn push_unique_preserves_first_occurrence_order() {
        let mut lines = Vec::new();
        push_unique(&mut lines, "one");
        push_unique(&mut lines, "two");
        push_unique(&mut lines, "one");
        assert_eq!(lines, vec!["one", "two"]);
    }

    #[test]
    fn bundled_compose_uses_release_image_and_quoted_volume_mounts() {
        let yaml = super::bundled_compose_yaml(Path::new("/tmp/codex-blackbox test"));
        assert!(yaml.contains(super::DEFAULT_CORE_IMAGE));
        assert!(yaml.contains("GET /ready HTTP/1.1"));
        assert!(yaml.contains("/dev/tcp/localhost/9901"));
        assert!(
            yaml.contains("\"/tmp/codex-blackbox test/envoy/envoy.yaml:/etc/envoy/envoy.yaml:ro\"")
        );
        assert!(yaml.contains(
            "\"/tmp/codex-blackbox test/grafana/dashboards:/var/lib/grafana/dashboards:ro\""
        ));
    }
}
