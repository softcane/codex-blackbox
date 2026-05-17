mod tmux;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, BufRead, BufReader, ErrorKind, IsTerminal, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Local};
use clap::{Parser, Subcommand, ValueEnum};
use codex_blackbox_core::decision::{
    decide, CooldownFacts, Decision, DecisionState, ObservedSessionFacts,
};
use codex_blackbox_core::guard_policy::{
    evaluate_guard_policy, load_guard_policy_from_env, load_guard_policy_from_path,
    GuardCooldownEvidence, GuardEvidence, GuardPolicy, GuardPolicyIssue,
};
use colored::{control as color_control, Colorize};
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum ColorMode {
    Auto,
    Always,
    Never,
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

        /// Render the deterministic local postmortem when a session becomes ready
        #[arg(long)]
        postmortem: bool,

        /// Show unredacted automatic postmortems in watch --postmortem
        #[arg(long)]
        no_redact: bool,

        /// Footer color mode
        #[arg(long, value_enum, default_value_t = ColorMode::Auto)]
        color: ColorMode,

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

    /// Render the current Codex decision footer or JSON
    Status {
        /// Base URL of codex-blackbox-core
        #[arg(long, default_value = "http://localhost:9091")]
        url: String,

        /// Emit machine-readable uncolored decision JSON
        #[arg(long)]
        json: bool,

        /// Footer color mode
        #[arg(long, value_enum, default_value_t = ColorMode::Auto)]
        color: ColorMode,

        /// Render width for footer degradation tests or fixed-width terminals
        #[arg(long)]
        width: Option<usize>,

        /// "last" or a specific session ID
        #[arg(default_value = "last")]
        target: String,
    },

    /// Render the advisory guard decision for the next Codex request
    Guard {
        /// Base URL of codex-blackbox-core
        #[arg(long, default_value = "http://localhost:9091")]
        url: String,

        /// Optional TOML guard policy file
        #[arg(long)]
        policy: Option<PathBuf>,

        /// Emit machine-readable uncolored decision JSON
        #[arg(long)]
        json: bool,

        /// Footer color mode
        #[arg(long, value_enum, default_value_t = ColorMode::Auto)]
        color: ColorMode,

        /// Render width for footer degradation tests or fixed-width terminals
        #[arg(long)]
        width: Option<usize>,

        /// "last" or a specific session ID
        #[arg(default_value = "last")]
        target: String,
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

    /// Render a deterministic Codex session report
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

        /// Terminal color mode
        #[arg(long, value_enum, default_value_t = ColorMode::Auto)]
        color: ColorMode,

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
    PostmortemReady {
        session_id: String,
        total_turns: u32,
        total_tokens: u64,
        reason: String,
        postmortem_command: String,
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
    Cooldown {
        reason: String,
        #[serde(default)]
        retry_after_seconds: Option<u64>,
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
        "test/e2e-openai-responses-full.sh",
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
                "Run from the Codex Blackbox repository if you need `./test/e2e-openai-responses-full.sh`.",
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
    observation_prompt_excerpt: Option<String>,
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

fn codex_exec_option_consumes_value(arg: &str) -> bool {
    if arg.contains('=') {
        return false;
    }
    matches!(
        arg,
        "-c" | "--config"
            | "--enable"
            | "--disable"
            | "-i"
            | "--image"
            | "-m"
            | "--model"
            | "--local-provider"
            | "-p"
            | "--profile"
            | "-s"
            | "--sandbox"
            | "-C"
            | "--cd"
            | "--add-dir"
            | "-a"
            | "--ask-for-approval"
            | "--output-schema"
            | "--color"
            | "-o"
            | "--output-last-message"
    )
}

fn codex_observation_prompt_excerpt(prompt: &str) -> Option<String> {
    const PROMPT_EXCERPT_MAX_CHARS: usize = 320;
    let trimmed = prompt
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
        let mut out = trimmed
            .chars()
            .take(PROMPT_EXCERPT_MAX_CHARS)
            .collect::<String>();
        out.push_str("...");
        Some(out)
    }
}

fn codex_exec_prompt_hint(command_args: &[String]) -> Option<String> {
    let exec_index = command_args
        .iter()
        .position(|arg| matches!(arg.as_str(), "exec" | "e"))?;
    let mut idx = exec_index + 1;
    while idx < command_args.len() {
        let arg = &command_args[idx];
        if arg == "--" {
            idx += 1;
            continue;
        }
        if arg.starts_with('-') {
            if codex_exec_option_consumes_value(arg) {
                idx += 2;
            } else {
                idx += 1;
            }
            continue;
        }
        if matches!(arg.as_str(), "resume" | "review" | "help" | "-") {
            return None;
        }
        return codex_observation_prompt_excerpt(arg);
    }
    None
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
        let observation_prompt_excerpt = if requires_codex_observation {
            codex_exec_prompt_hint(command_args)
        } else {
            None
        };
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
            observation_prompt_excerpt,
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
        observation_prompt_excerpt: None,
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
                    "Post-run check: require Codex Blackbox to observe run-scoped Codex Responses evidence"
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
                eprintln!("Codex Blackbox: will fail this run if codex-blackbox-core observes no run-scoped Codex Responses request.");
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

#[derive(Debug)]
struct ChildProcessResult {
    exit_code: i32,
    codex_session_id: Option<String>,
}

fn run_command_with_env(
    command: &str,
    args: &[String],
    envs: &[(String, String)],
    env_removals: &[String],
    stdin_mode: &ChildStdinMode,
) -> Result<ChildProcessResult, String> {
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

    Ok(ChildProcessResult {
        exit_code: exit_code(status),
        codex_session_id: None,
    })
}

fn parse_codex_session_id_line(line: &str) -> Option<String> {
    let value = line.trim().strip_prefix("session id:")?.trim();
    let valid_uuid_shape = value.len() == 36
        && value.chars().enumerate().all(|(idx, ch)| {
            if matches!(idx, 8 | 13 | 18 | 23) {
                ch == '-'
            } else {
                ch.is_ascii_hexdigit()
            }
        });
    valid_uuid_shape.then(|| value.to_string())
}

fn run_codex_command_with_filtered_stderr(
    command: &str,
    args: &[String],
    envs: &[(String, String)],
    env_removals: &[String],
    stdin_mode: &ChildStdinMode,
) -> Result<ChildProcessResult, String> {
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
    let codex_session_id = Arc::new(Mutex::new(None::<String>));
    let stderr_session_id = codex_session_id.clone();
    let stderr_thread = std::thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines() {
            match line {
                Ok(line) if should_suppress_codex_stderr_line(&line) => {}
                Ok(line) => {
                    if let Some(session_id) = parse_codex_session_id_line(&line) {
                        if let Ok(mut captured) = stderr_session_id.lock() {
                            *captured = Some(session_id);
                        }
                    }
                    eprintln!("{line}");
                }
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

    let codex_session_id = codex_session_id.lock().ok().and_then(|value| value.clone());
    Ok(ChildProcessResult {
        exit_code: exit_code(status),
        codex_session_id,
    })
}

fn should_suppress_codex_stderr_line(line: &str) -> bool {
    line.contains("failed to record rollout items")
        || line == "Reading additional input from stdin..."
        || line.contains("write_stdin failed: stdin is closed for this session")
}

#[derive(Debug, Deserialize)]
struct CodexObservationSnapshot {
    #[serde(default)]
    latest_request_rowid: i64,
    #[serde(default)]
    matched: bool,
}

#[derive(Debug, Serialize)]
struct CodexObservationRequest<'a> {
    after_request_rowid: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_excerpt: Option<&'a str>,
}

async fn fetch_codex_observation_snapshot(
    after_request_rowid: i64,
    session_id: Option<&str>,
    prompt_excerpt: Option<&str>,
) -> Result<CodexObservationSnapshot, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .map_err(|err| format!("failed to build observation client: {err}"))?;
    let url = format!(
        "{}/api/observations/codex",
        codex_blackbox_core_url().trim_end_matches('/')
    );
    let resp = client
        .post(&url)
        .json(&CodexObservationRequest {
            after_request_rowid,
            session_id,
            prompt_excerpt,
        })
        .send()
        .await
        .map_err(|err| format!("failed to fetch {url}: {err}"))?;
    if !resp.status().is_success() {
        return Err(format!("failed to fetch {url}: HTTP {}", resp.status()));
    }
    resp.json::<CodexObservationSnapshot>()
        .await
        .map_err(|err| format!("failed to parse {url}: {err}"))
}

async fn wait_for_codex_observation_after(
    after_request_rowid: i64,
    session_id: Option<&str>,
    prompt_excerpt: Option<&str>,
    timeout: Duration,
) -> Result<bool, String> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        let snapshot =
            fetch_codex_observation_snapshot(after_request_rowid, session_id, prompt_excerpt)
                .await?;
        if snapshot.matched {
            return Ok(true);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Ok(false)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CodexObservationScope {
    SessionId,
    Prompt,
    ProcessStart,
}

fn codex_observation_missing_error(scope: CodexObservationScope) -> String {
    let scope = match scope {
        CodexObservationScope::SessionId => "matching the child Codex session id",
        CodexObservationScope::Prompt => "matching this codex exec prompt",
        CodexObservationScope::ProcessStart => "after this child process started",
    };
    format!(
        "Codex exited successfully, but codex-blackbox-core did not observe any new provider=\"codex_responses\" request {scope}. Treating this as a failed Codex Blackbox proxy run."
    )
}

fn enforce_codex_observation(observed: bool, scope: CodexObservationScope) -> Result<(), String> {
    if observed {
        Ok(())
    } else {
        Err(codex_observation_missing_error(scope))
    }
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
        match fetch_codex_observation_snapshot(0, None, None).await {
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
        Ok(child_result) => {
            if child_result.exit_code == 0 {
                if let Some(before) = observed_before {
                    let session_id = child_result.codex_session_id.as_deref();
                    let prompt_excerpt = plan.observation_prompt_excerpt.as_deref();
                    let observation_scope = if session_id.is_some() {
                        CodexObservationScope::SessionId
                    } else if prompt_excerpt.is_some() {
                        CodexObservationScope::Prompt
                    } else {
                        CodexObservationScope::ProcessStart
                    };
                    match wait_for_codex_observation_after(
                        before.latest_request_rowid,
                        session_id,
                        prompt_excerpt,
                        Duration::from_secs(5),
                    )
                    .await
                    {
                        Ok(observed) => {
                            if let Err(err) = enforce_codex_observation(observed, observation_scope)
                            {
                                eprintln!("Error: {err}");
                                return 1;
                            }
                        }
                        Err(err) => {
                            eprintln!("Error: Codex Blackbox observation post-check failed: {err}");
                            return 1;
                        }
                    }
                }
            }
            child_result.exit_code
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
        | WatchEvent::PostmortemReady { session_id, .. }
        | WatchEvent::ModelFallback { session_id, .. }
        | WatchEvent::CodexTurnSummary { session_id, .. }
        | WatchEvent::ContextStatus { session_id, .. } => Some(session_id.as_str()),
        WatchEvent::Cooldown { .. } | WatchEvent::Lagged { .. } => None,
    }
}

fn event_matches_session_filter(event: &WatchEvent, session_filter: &Option<String>) -> bool {
    session_filter
        .as_ref()
        .is_none_or(|filter| event_session_id(event).is_none_or(|session_id| session_id == filter))
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

#[derive(Default)]
struct DecisionSessionTracker {
    facts: HashMap<String, ObservedSessionFacts>,
}

impl DecisionSessionTracker {
    fn update(&mut self, event: &WatchEvent) -> Option<Decision> {
        if let WatchEvent::Cooldown {
            reason,
            retry_after_seconds,
        } = event
        {
            let facts = self
                .facts
                .entry("__codex_blackbox_cooldown__".to_string())
                .or_default();
            facts.cooldown = Some(codex_blackbox_core::decision::CooldownFacts {
                reason: reason.clone(),
                retry_after_seconds: *retry_after_seconds,
            });
            return Some(decide(facts));
        }

        let session_id = event_session_id(event)?.to_string();
        let facts = self
            .facts
            .entry(session_id.clone())
            .or_insert_with(|| ObservedSessionFacts {
                session_id: Some(session_id),
                ..Default::default()
            });

        match event {
            WatchEvent::SessionStart { .. } => {
                facts.observed_codex_responses = true;
            }
            WatchEvent::CodexTurnSummary {
                status,
                total_tokens,
                ..
            } => {
                facts.observed_codex_responses = true;
                facts.total_turns = facts.total_turns.saturating_add(1);
                facts.total_tokens = facts.total_tokens.saturating_add(*total_tokens);
                match status.as_str() {
                    "failed" => facts.failed_responses = facts.failed_responses.saturating_add(1),
                    "incomplete" => {
                        facts.incomplete_responses = facts.incomplete_responses.saturating_add(1)
                    }
                    "unknown" => {
                        facts.unknown_responses = facts.unknown_responses.saturating_add(1)
                    }
                    _ => {}
                }
            }
            WatchEvent::ContextStatus { fill_percent, .. } => {
                facts.max_context_fill_percent = Some(
                    facts
                        .max_context_fill_percent
                        .map(|current| current.max(*fill_percent))
                        .unwrap_or(*fill_percent),
                );
            }
            WatchEvent::ModelFallback { .. } => {
                facts.model_mismatch = true;
            }
            WatchEvent::SessionEnd {
                total_tokens,
                total_turns,
                ..
            } => {
                facts.observed_codex_responses = *total_turns > 0;
                facts.ended = true;
                facts.total_turns = *total_turns;
                facts.total_tokens = *total_tokens;
            }
            WatchEvent::PostmortemReady {
                total_turns,
                total_tokens,
                ..
            } => {
                facts.observed_codex_responses = true;
                facts.ended = true;
                facts.total_turns = *total_turns;
                facts.total_tokens = *total_tokens;
            }
            WatchEvent::ToolUse { .. }
            | WatchEvent::FrustrationSignal { .. }
            | WatchEvent::CompactionLoop { .. }
            | WatchEvent::Diagnosis { .. }
            | WatchEvent::Cooldown { .. }
            | WatchEvent::Lagged { .. } => return None,
        }

        Some(decide(facts))
    }
}

struct WatchRenderOptions {
    base_url: String,
    no_signals: bool,
    session_filter: Option<String>,
    postmortem: bool,
    redact_postmortem: bool,
    color_mode: ColorMode,
}

struct WatchRuntimeState {
    active: ActiveSessions,
    decisions: DecisionSessionTracker,
    last_rendered_decisions: HashMap<String, Decision>,
    rendered_postmortems: HashSet<String>,
}

impl WatchRuntimeState {
    fn new() -> Self {
        Self {
            active: ActiveSessions::new(),
            decisions: DecisionSessionTracker::default(),
            last_rendered_decisions: HashMap::new(),
            rendered_postmortems: HashSet::new(),
        }
    }

    fn remember_decision_if_changed(&mut self, event: &WatchEvent, decision: &Decision) -> bool {
        let key = match event {
            WatchEvent::Cooldown { .. } => Some("__codex_blackbox_cooldown__".to_string()),
            _ => event_session_id(event).map(ToString::to_string),
        };
        let Some(key) = key else {
            return true;
        };
        match self.last_rendered_decisions.get(&key) {
            Some(previous) if previous == decision => false,
            _ => {
                self.last_rendered_decisions.insert(key, decision.clone());
                true
            }
        }
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

fn stdout_is_terminal() -> bool {
    io::stdout().is_terminal()
}

fn state_ansi_prefix(state: DecisionState) -> &'static str {
    match state {
        DecisionState::Healthy => "\x1b[32m",
        DecisionState::Watching => "\x1b[36m",
        DecisionState::Careful => "\x1b[33m",
        DecisionState::Stop => "\x1b[31m",
        DecisionState::Blocked => "\x1b[1;31m",
        DecisionState::Cooldown => "\x1b[33m",
        DecisionState::Ended => "\x1b[2;90m",
    }
}

fn color_enabled(mode: ColorMode, stdout_is_tty: bool, no_color: bool) -> bool {
    match mode {
        ColorMode::Always => true,
        ColorMode::Never => false,
        ColorMode::Auto => stdout_is_tty && !no_color,
    }
}

fn one_line_segment(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_ascii(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    if max_chars <= 3 {
        return ".".repeat(max_chars);
    }
    let mut out: String = value.chars().take(max_chars - 3).collect();
    out.push_str("...");
    out
}

fn compact_decision_reason(reason: &str, width: usize) -> String {
    let reason = one_line_segment(reason);
    if reason.len() <= width {
        return reason;
    }

    let lower = reason.to_ascii_lowercase();
    let compact = if lower.contains("waiting for first observed codex responses request") {
        if width >= 19 {
            "waiting for request"
        } else {
            "waiting"
        }
    } else if lower.contains("waiting for first observed codex responses turn") {
        if width >= 16 {
            "waiting for turn"
        } else {
            "waiting"
        }
    } else if lower == "core unavailable" {
        if width >= 16 {
            "core unavailable"
        } else {
            "core down"
        }
    } else if lower == "response failed" {
        "failed"
    } else if lower == "response incomplete" {
        "incomplete"
    } else if lower == "accounting anomaly" {
        if width >= 18 {
            "accounting anomaly"
        } else {
            "accounting"
        }
    } else if lower == "served model changed" {
        "model changed"
    } else if lower == "unknown response status" {
        "unknown status"
    } else if lower == "local estimate untrusted" {
        if width >= 18 {
            "untrusted estimate"
        } else {
            "untrusted"
        }
    } else if lower == "token budget exceeded" {
        if width >= 15 {
            "budget exceeded"
        } else {
            "budget"
        }
    } else if lower == "upstream errors" {
        if width >= 15 {
            "upstream errors"
        } else {
            "upstream"
        }
    } else if lower.contains(" turns, ") && lower.ends_with(" tokens") {
        reason.trim_end_matches(" tokens")
    } else {
        reason.as_str()
    };

    if compact.len() <= width {
        compact.to_string()
    } else {
        truncate_ascii(compact, width)
    }
}

fn render_decision_footer_plain(decision: &Decision, width: usize) -> String {
    let width = width.max(1);
    let state = decision.state.label();
    let reason = one_line_segment(&decision.primary_reason);
    let action = one_line_segment(&decision.next_action);
    let command = decision
        .drill_down_command
        .as_deref()
        .map(one_line_segment)
        .filter(|value| !value.is_empty());

    let mut candidates = Vec::new();
    if let Some(command) = command.as_ref() {
        candidates.push(format!(
            "codex-blackbox: {state} | {reason} | {action} | {command}"
        ));
    }
    candidates.push(format!("codex-blackbox: {state} | {reason} | {action}"));
    candidates.push(format!("codex-blackbox: {state} | {reason}"));
    candidates.push(format!("cbb: {state} | {reason}"));
    candidates.push(format!("{state} | {reason}"));

    if let Some(candidate) = candidates.into_iter().find(|line| line.len() <= width) {
        return candidate;
    }

    let prefix = format!("{state} | ");
    if prefix.len() >= width {
        return truncate_ascii(&prefix, width);
    }
    let reason_width = width - prefix.len();
    format!("{prefix}{}", compact_decision_reason(&reason, reason_width))
}

fn render_decision_footer(
    decision: &Decision,
    width: usize,
    color_mode: ColorMode,
    stdout_is_tty: bool,
    no_color: bool,
) -> String {
    let plain = render_decision_footer_plain(decision, width);
    if color_enabled(color_mode, stdout_is_tty, no_color) {
        format!("{}{}\x1b[0m", state_ansi_prefix(decision.state), plain)
    } else {
        plain
    }
}

fn render_decision_footer_json(decision: &Decision) -> Result<String, serde_json::Error> {
    serde_json::to_string(decision)
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

        WatchEvent::PostmortemReady {
            reason,
            postmortem_command,
            ..
        } => {
            let _ = (reason, postmortem_command);
            // `watch --postmortem` handles this event outside the ordinary
            // event renderer. Plain watch remains quiet by default.
        }

        WatchEvent::Cooldown { .. } => {
            // The shared decision footer renders cooldown state.
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
            postmortem,
            no_redact,
            color,
            session,
            tmux,
            tmux_max_panes,
        } => {
            if tmux {
                // Tmux orchestrator mode. Self-bootstrap into a tmux session
                // if we're not already inside one, so the user just runs
                // `codex-blackbox watch --tmux` once.
                if let Err(e) = tmux::bootstrap_into_tmux(
                    &url,
                    no_signals,
                    tmux_max_panes,
                    postmortem,
                    no_redact,
                ) {
                    eprintln!("{}", e.red());
                    std::process::exit(1);
                }
                let orchestrator = match tmux::TmuxOrchestrator::new(
                    url.clone(),
                    no_signals,
                    tmux_max_panes,
                    postmortem,
                    no_redact,
                ) {
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
                let mut state = WatchRuntimeState::new();
                let options = WatchRenderOptions {
                    base_url: url.clone(),
                    no_signals,
                    session_filter: session.clone(),
                    postmortem,
                    redact_postmortem: !no_redact,
                    color_mode: color,
                };
                let mut retry_log = WatchRetryLog::default();

                loop {
                    match connect_and_stream(&watch_url, &options, &mut state).await {
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
        Commands::Status {
            url,
            json,
            color,
            width,
            target,
        } => match fetch_status(&url, &target, json, color, width).await {
            Ok(()) => {}
            Err(e) => {
                eprintln!("{}", format!("Error: {}", e).red());
                std::process::exit(1);
            }
        },
        Commands::Guard {
            url,
            policy,
            json,
            color,
            width,
            target,
        } => match fetch_guard(&url, &target, policy.as_deref(), json, color, width).await {
            Ok(()) => {}
            Err(e) => {
                eprintln!("{}", format!("Error: {}", e).red());
                std::process::exit(1);
            }
        },
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
            color,
            target,
        } => {
            let redact = !no_redact;
            match fetch_postmortem(&url, &target, redact, output.as_deref(), color).await {
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
    color_mode: ColorMode,
) -> Result<(), Box<dyn std::error::Error>> {
    let report = fetch_postmortem_report_json(base_url, target, redact).await?;
    if let Some(path) = output {
        let markdown = render_postmortem_markdown(&report);
        fs::write(path, markdown)
            .map_err(|err| format!("failed to write {}: {}", path.display(), err))?;
    } else {
        print_postmortem_terminal_report(&report, color_mode);
    }
    Ok(())
}

async fn fetch_postmortem_report_json(
    base_url: &str,
    target: &str,
    redact: bool,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
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
    Ok(resp.json().await?)
}

async fn fetch_guard_state_json(
    base_url: &str,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let url = format!("{}/api/guard-state", base_url.trim_end_matches('/'));
    let resp = reqwest::Client::new().get(&url).send().await?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()).into());
    }
    Ok(resp.json().await?)
}

fn cooldown_from_guard_state(state: &serde_json::Value) -> Option<CooldownFacts> {
    let cooldown = state.get("cooldown")?;
    if cooldown.is_null() {
        return None;
    }
    let reason = cooldown
        .get("reason")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("upstream errors")
        .to_string();
    Some(CooldownFacts {
        reason,
        retry_after_seconds: cooldown
            .get("retry_after_seconds")
            .and_then(|value| value.as_u64()),
    })
}

fn guard_cooldown_evidence(state: Option<&serde_json::Value>) -> Option<GuardCooldownEvidence> {
    state
        .and_then(cooldown_from_guard_state)
        .map(|cooldown| GuardCooldownEvidence {
            reason: cooldown.reason,
            retry_after_seconds: cooldown.retry_after_seconds,
        })
}

fn apply_guard_state_to_facts(facts: &mut ObservedSessionFacts, state: Option<&serde_json::Value>) {
    if let Some(cooldown) = state.and_then(cooldown_from_guard_state) {
        facts.cooldown = Some(cooldown);
    }
}

fn status_count(report: &serde_json::Value, status: &str) -> u32 {
    report
        .get("signals")
        .and_then(|signals| signals.get("response_statuses"))
        .and_then(|statuses| statuses.get(status))
        .and_then(|value| value.as_u64())
        .unwrap_or(0)
        .min(u32::MAX as u64) as u32
}

fn status_report_to_facts(report: &serde_json::Value) -> ObservedSessionFacts {
    let summary = report.get("summary").unwrap_or(&serde_json::Value::Null);
    let impact = report.get("impact").unwrap_or(&serde_json::Value::Null);
    let signals = report.get("signals").unwrap_or(&serde_json::Value::Null);
    let diagnosis = report.get("diagnosis").unwrap_or(&serde_json::Value::Null);
    let total_turns = summary
        .get("turn_count")
        .and_then(|value| value.as_u64())
        .unwrap_or(0)
        .min(u32::MAX as u64) as u32;
    let primary_cause_type = json_str(diagnosis, "primary_cause_type").unwrap_or("");
    let report_partial = report
        .get("partial")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let has_ended_at = json_str(summary, "ended_at").is_some_and(|value| !value.trim().is_empty());

    ObservedSessionFacts {
        session_id: json_str(report, "session_id").map(ToString::to_string),
        observed_codex_responses: true,
        ended: has_ended_at || !report_partial,
        total_turns,
        total_tokens: impact
            .get("local_total_tokens")
            .and_then(|value| value.as_u64())
            .unwrap_or(0),
        max_context_fill_percent: signals
            .get("context_fill")
            .and_then(|context| context.get("max_percent"))
            .and_then(|value| value.as_f64()),
        failed_responses: status_count(report, "failed"),
        incomplete_responses: status_count(report, "incomplete"),
        unknown_responses: status_count(report, "unknown"),
        model_mismatch: signals
            .get("model_mismatches")
            .and_then(|value| value.as_array())
            .is_some_and(|items| !items.is_empty())
            || primary_cause_type == "codex_model_mismatch",
        accounting_anomalies: signals
            .get("accounting_anomaly_count")
            .and_then(|value| value.as_u64())
            .unwrap_or(0)
            .min(u32::MAX as u64) as u32,
        local_estimate_trusted_for_budget_enforcement: impact
            .get("local_estimate_trusted_for_budget_enforcement")
            .and_then(|value| value.as_bool()),
        ..Default::default()
    }
}

fn guard_report_to_evidence(
    report: &serde_json::Value,
    guard_state: Option<&serde_json::Value>,
) -> GuardEvidence {
    let summary = report.get("summary").unwrap_or(&serde_json::Value::Null);
    let impact = report.get("impact").unwrap_or(&serde_json::Value::Null);
    let signals = report.get("signals").unwrap_or(&serde_json::Value::Null);
    let total_turns = summary
        .get("turn_count")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);

    GuardEvidence {
        session_id: json_str(report, "session_id").map(ToString::to_string),
        observed_codex_responses: total_turns > 0,
        applies_to_next_request: true,
        session_total_tokens: impact
            .get("local_total_tokens")
            .and_then(|value| value.as_u64()),
        session_estimated_cost_dollars: impact
            .get("local_estimated_cost_dollars")
            .or_else(|| impact.get("local_estimate_cost_dollars"))
            .and_then(|value| value.as_f64()),
        local_estimate_trusted_for_budget_enforcement: impact
            .get("local_estimate_trusted_for_budget_enforcement")
            .and_then(|value| value.as_bool())
            .unwrap_or(false),
        max_context_fill_percent: signals
            .get("context_fill")
            .and_then(|context| context.get("max_percent"))
            .and_then(|value| value.as_f64()),
        failed_responses: status_count(report, "failed"),
        incomplete_responses: status_count(report, "incomplete"),
        unknown_responses: status_count(report, "unknown"),
        accounting_anomalies: signals
            .get("accounting_anomaly_count")
            .and_then(|value| value.as_u64())
            .unwrap_or(0)
            .min(u32::MAX as u64) as u32,
        model_mismatch: signals
            .get("model_mismatches")
            .and_then(|value| value.as_array())
            .is_some_and(|items| !items.is_empty()),
        cooldown: guard_cooldown_evidence(guard_state),
    }
}

fn guard_issue_reason(issue: &GuardPolicyIssue) -> String {
    format!("{}: {}", issue.issue_type, issue.message)
}

fn guard_report_to_decision(
    report: &serde_json::Value,
    policy: &GuardPolicy,
    policy_issues: Vec<GuardPolicyIssue>,
    guard_state: Option<&serde_json::Value>,
) -> Decision {
    let mut facts = status_report_to_facts(report);
    let mut evaluation =
        evaluate_guard_policy(policy, &guard_report_to_evidence(report, guard_state));
    evaluation.policy_issues.extend(policy_issues);
    facts.policy_block = evaluation.block;
    facts.cooldown = evaluation.cooldown;
    facts.policy_issues = evaluation
        .policy_issues
        .iter()
        .map(guard_issue_reason)
        .collect();
    decide(&facts)
}

fn load_cli_guard_policy(
    policy_path: Option<&Path>,
) -> codex_blackbox_core::guard_policy::GuardPolicyLoad {
    match policy_path {
        Some(path) => load_guard_policy_from_path(path),
        None => load_guard_policy_from_env(|key| std::env::var(key).ok()),
    }
}

fn print_stdout_line(line: &str) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    stdout.write_all(line.as_bytes())?;
    stdout.write_all(b"\n")
}

fn print_decision_line(line: &str) -> Result<(), Box<dyn std::error::Error>> {
    match print_stdout_line(line) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == ErrorKind::BrokenPipe => Ok(()),
        Err(err) => Err(Box::new(err)),
    }
}

async fn fetch_status(
    base_url: &str,
    target: &str,
    json_output: bool,
    color_mode: ColorMode,
    width: Option<usize>,
) -> Result<(), Box<dyn std::error::Error>> {
    let decision = match fetch_postmortem_report_json(base_url, target, true).await {
        Ok(report) => {
            let guard_state = fetch_guard_state_json(base_url).await.ok();
            let mut facts = status_report_to_facts(&report);
            apply_guard_state_to_facts(&mut facts, guard_state.as_ref());
            decide(&facts)
        }
        Err(err) => {
            let guard_state = fetch_guard_state_json(base_url).await.ok();
            let message = err.to_string();
            let mut facts = if message.starts_with("HTTP 404") {
                ObservedSessionFacts::default()
            } else {
                ObservedSessionFacts {
                    core_unavailable: true,
                    ..Default::default()
                }
            };
            apply_guard_state_to_facts(&mut facts, guard_state.as_ref());
            decide(&facts)
        }
    };

    if json_output {
        print_decision_line(&render_decision_footer_json(&decision)?)?;
    } else {
        print_decision_line(&render_decision_footer(
            &decision,
            width.unwrap_or_else(terminal_width),
            color_mode,
            stdout_is_terminal(),
            std::env::var_os("NO_COLOR").is_some(),
        ))?;
    }
    Ok(())
}

async fn fetch_guard(
    base_url: &str,
    target: &str,
    policy_path: Option<&Path>,
    json_output: bool,
    color_mode: ColorMode,
    width: Option<usize>,
) -> Result<(), Box<dyn std::error::Error>> {
    let loaded = load_cli_guard_policy(policy_path);
    let decision = match fetch_postmortem_report_json(base_url, target, true).await {
        Ok(report) => {
            let guard_state = fetch_guard_state_json(base_url).await.ok();
            guard_report_to_decision(&report, &loaded.policy, loaded.issues, guard_state.as_ref())
        }
        Err(err) => {
            let guard_state = fetch_guard_state_json(base_url).await.ok();
            let message = err.to_string();
            let mut facts = if message.starts_with("HTTP 404") {
                ObservedSessionFacts::default()
            } else {
                ObservedSessionFacts {
                    core_unavailable: true,
                    ..Default::default()
                }
            };
            apply_guard_state_to_facts(&mut facts, guard_state.as_ref());
            facts.policy_issues = loaded.issues.iter().map(guard_issue_reason).collect();
            decide(&facts)
        }
    };

    if json_output {
        print_decision_line(&render_decision_footer_json(&decision)?)?;
    } else {
        print_decision_line(&render_decision_footer(
            &decision,
            width.unwrap_or_else(terminal_width),
            color_mode,
            stdout_is_terminal(),
            std::env::var_os("NO_COLOR").is_some(),
        ))?;
    }
    Ok(())
}

fn parse_percent_prefix(value: &str) -> Option<f64> {
    value.split_whitespace().find_map(|part| {
        part.trim_matches(|ch: char| !(ch.is_ascii_digit() || ch == '.' || ch == '%'))
            .strip_suffix('%')
            .and_then(|number| number.parse::<f64>().ok())
    })
}

fn humanize_postmortem_source(source: &str) -> String {
    if let Some(model) = source.strip_prefix("codex_unpriced:unknown_model:") {
        format!("no trusted price for {model}")
    } else {
        match source {
            "builtin_model_family_pricing" => "built-in model-family pricing".to_string(),
            "codex_unpriced" => "no trusted price for this model".to_string(),
            other => other.to_string(),
        }
    }
}

fn humanize_postmortem_cause(value: &str) -> String {
    match value {
        "none" => "No problem detected".to_string(),
        "Codex Responses failed" => "Model response failed".to_string(),
        "Codex Responses incomplete" => "Model response stopped incomplete".to_string(),
        "High reasoning-output share" => "High internal reasoning token share".to_string(),
        "Low cached-input reuse" => "Low prompt cache reuse".to_string(),
        "Observed Codex Responses signal" => "Observed model-response signal".to_string(),
        other => other.to_string(),
    }
}

fn humanize_postmortem_signal(signal: &str) -> String {
    match signal {
        "codex_response_failed" => "response failed".to_string(),
        "codex_response_incomplete" => "response stopped incomplete".to_string(),
        "codex_response_unknown" => "response status unknown".to_string(),
        "codex_model_mismatch" => "model changed".to_string(),
        "codex_accounting_anomaly" => "token accounting anomaly".to_string(),
        "codex_high_context_fill" => "high context use".to_string(),
        "codex_high_reasoning_share" => "high internal reasoning".to_string(),
        "codex_low_cached_input_reuse" => "low prompt cache reuse".to_string(),
        "codex_tool_call_intent" => "tool request".to_string(),
        other => other.replace('_', " "),
    }
}

fn humanize_postmortem_event(event: &str) -> String {
    match event {
        "session_start" => "session started".to_string(),
        "codex_turn" => "model turn".to_string(),
        "tool_call_intent" => "tool request".to_string(),
        "session_degraded" => "issue detected".to_string(),
        "latest_observation" => "latest check".to_string(),
        other => other.replace('_', " "),
    }
}

fn humanize_postmortem_detail(value: &str) -> String {
    match value {
        "Responses stream ended without a recognized terminal status." => {
            "Stream ended before a final status was recognized.".to_string()
        }
        "First persisted Codex Responses evidence for the session." => {
            "First model response observed for the session.".to_string()
        }
        "Direct degraded Codex Responses evidence was observed." => {
            "A failed or incomplete response was observed.".to_string()
        }
        "Latest persisted Codex Responses observation." => {
            "Latest model response observed.".to_string()
        }
        "max_output_tokens" => "hit max_output_tokens".to_string(),
        other => other
            .replace("Codex Responses", "model response")
            .replace("Responses status", "response status")
            .replace(
                "model-side tool-call intent observed",
                "tool request observed",
            )
            .replace("Provider-side detail", "Observed detail"),
    }
}

fn humanize_postmortem_note(key: &str, value: &str) -> String {
    match value {
        "Evidence is limited to local Envoy-observed Codex Responses traffic." => {
            "Only local proxy traffic was used; confirm separately before making live-support claims."
                .to_string()
        }
        "Tool-call rows are model-side intent only; local execution outcome is not observed." => {
            "Tool rows mean the model asked for a tool; they do not prove the tool ran or succeeded."
                .to_string()
        }
        "Cached input is token accounting only; lifecycle timing is not inferred." => {
            "Cached input is token accounting only; cache timing is not inferred.".to_string()
        }
        "Provider account-limit state and permission decisions are not observed." => {
            "Account limits and permission decisions are not visible here.".to_string()
        }
        "Inspect the persisted provider-side failure detail before retrying." => {
            "Read the failure detail above before retrying.".to_string()
        }
        "Continue with a narrower prompt or adjust the relevant output limit." => {
            "Continue with a smaller prompt, or raise the output limit if that was intentional."
                .to_string()
        }
        other if key == "recommendations" => humanize_postmortem_detail(other),
        other if key == "caveats" => humanize_postmortem_detail(other),
        other => other.to_string(),
    }
}

fn colorize_postmortem_value(label: &str, value: &str) -> String {
    let lower = value.to_ascii_lowercase();
    match label {
        "State" => {
            if lower.contains("complete") {
                value.green().bold().to_string()
            } else {
                value.yellow().bold().to_string()
            }
        }
        "Outcome" | "Result" => {
            if lower.contains("failed") || lower.contains("error") {
                value.red().bold().to_string()
            } else if lower.contains("completed") || lower.contains("complete") {
                value.green().bold().to_string()
            } else if lower.contains("incomplete") || lower.contains("partial") {
                value.yellow().bold().to_string()
            } else {
                value.to_string()
            }
        }
        "Session" | "Prompt" | "Final Answer" | "Final Summary" => value.bright_white().to_string(),
        "Requested Model" | "Served Model" | "Model" => value.cyan().to_string(),
        "Turns" => value.bright_white().to_string(),
        "Tokens" | "Impact" | "Usage" => value.yellow().to_string(),
        "Local Estimate" | "Estimated Cost" | "Billed" | "Billed Cost" | "Cost" => {
            value.magenta().to_string()
        }
        "Pricing" => {
            if lower.contains("no trusted") {
                value.yellow().bold().to_string()
            } else {
                value.magenta().to_string()
            }
        }
        "Redaction" => {
            if lower.contains("unredacted") {
                value.yellow().bold().to_string()
            } else {
                value.green().to_string()
            }
        }
        "Local Estimate Trust" | "Cost Confidence" => {
            if lower.contains("untrusted") || lower.contains("advisory") {
                value.red().bold().to_string()
            } else {
                value.green().to_string()
            }
        }
        "Primary Cause" | "Likely Cause" => {
            if lower == "none" || lower.contains("no problem") || lower.contains("no primary") {
                value.green().bold().to_string()
            } else {
                value.yellow().bold().to_string()
            }
        }
        "Responses statuses" | "Response results" => {
            if lower.contains("failed") || lower.contains("incomplete") {
                value.red().bold().to_string()
            } else if lower.contains("completed") {
                value.green().bold().to_string()
            } else {
                value.to_string()
            }
        }
        "Estimated context fill max" | "Context used" => match parse_percent_prefix(value) {
            Some(percent) if percent >= 80.0 => value.red().bold().to_string(),
            Some(percent) if percent >= 60.0 => value.yellow().bold().to_string(),
            Some(_) => value.green().to_string(),
            None => value.to_string(),
        },
        "Cached input reuse" | "Cache reuse" => match parse_percent_prefix(value) {
            Some(percent) if percent >= 60.0 => value.green().to_string(),
            Some(percent) if percent >= 30.0 => value.yellow().to_string(),
            Some(_) => value.red().bold().to_string(),
            None => value.to_string(),
        },
        "Max reasoning-output share" | "Reasoning share" => match parse_percent_prefix(value) {
            Some(percent) if percent >= 80.0 => value.red().bold().to_string(),
            Some(percent) if percent >= 60.0 => value.yellow().bold().to_string(),
            Some(_) => value.green().to_string(),
            None => value.to_string(),
        },
        "Tool-call intent" | "Tool requests" | "Scope" => value.cyan().to_string(),
        _ => value.to_string(),
    }
}

fn colorize_evidence_kind(kind: &str) -> String {
    match kind {
        "observed" | "direct" => kind.green().bold().to_string(),
        "pattern" | "heuristic" => kind.yellow().bold().to_string(),
        "calculated" | "derived" => kind.cyan().bold().to_string(),
        _ => kind.bright_white().to_string(),
    }
}

fn humanize_evidence_kind(kind: &str) -> &'static str {
    match kind {
        "direct" => "observed",
        "heuristic" => "pattern",
        "derived" => "calculated",
        _ => "signal",
    }
}

#[derive(Clone, Copy)]
enum PostmortemTone {
    Header,
    Good,
    Warn,
    Danger,
    Info,
    Muted,
}

struct PostmortemTerminalLine {
    plain: String,
    styled: String,
}

impl PostmortemTerminalLine {
    fn styled(plain: impl Into<String>, styled: impl Into<String>) -> Self {
        Self {
            plain: plain.into(),
            styled: styled.into(),
        }
    }
}

fn postmortem_tone_text(value: &str, tone: PostmortemTone) -> String {
    match tone {
        PostmortemTone::Header => value.truecolor(245, 132, 46).bold().to_string(),
        PostmortemTone::Good => value.green().to_string(),
        PostmortemTone::Warn => value.yellow().to_string(),
        PostmortemTone::Danger => value.red().to_string(),
        PostmortemTone::Info => value.cyan().to_string(),
        PostmortemTone::Muted => value.bright_black().to_string(),
    }
}

fn postmortem_visible_len(value: &str) -> usize {
    value.chars().count()
}

fn postmortem_terminal_width() -> usize {
    terminal_width().clamp(60, 120)
}

fn postmortem_wrap_words(value: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut current = String::new();

    for word in value.split_whitespace() {
        let word_len = postmortem_visible_len(word);
        if word_len > width {
            if !current.is_empty() {
                lines.push(current);
                current = String::new();
            }
            let mut chunk = String::new();
            for ch in word.chars() {
                if postmortem_visible_len(&chunk) == width {
                    lines.push(chunk);
                    chunk = String::new();
                }
                chunk.push(ch);
            }
            if !chunk.is_empty() {
                lines.push(chunk);
            }
            continue;
        }

        let next_len = if current.is_empty() {
            word_len
        } else {
            postmortem_visible_len(&current) + 1 + word_len
        };
        if next_len <= width {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
        } else {
            lines.push(current);
            current = word.to_string();
        }
    }

    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn postmortem_top_border(title: &str, width: usize, tone: PostmortemTone) -> String {
    let max_title_width = width.saturating_sub(10).max(1);
    let title = truncate_for_box(title, max_title_width);
    let prefix = format!("\u{250c}\u{2500}[ {title} ]");
    let fill = width.saturating_sub(postmortem_visible_len(&prefix) + 1);
    postmortem_tone_text(
        &format!("{prefix}{}\u{2510}", "\u{2500}".repeat(fill)),
        tone,
    )
}

fn postmortem_bottom_border(width: usize, tone: PostmortemTone) -> String {
    postmortem_tone_text(
        &format!(
            "\u{2514}{}\u{2518}",
            "\u{2500}".repeat(width.saturating_sub(2))
        ),
        tone,
    )
}

fn postmortem_box_line(
    line: &PostmortemTerminalLine,
    width: usize,
    tone: PostmortemTone,
) -> String {
    let inner_width = width.saturating_sub(4);
    let (content, visible_len) = if postmortem_visible_len(&line.plain) > inner_width {
        let truncated = truncate_for_box(&line.plain, inner_width);
        let visible_len = postmortem_visible_len(&truncated);
        (truncated, visible_len)
    } else {
        (line.styled.clone(), postmortem_visible_len(&line.plain))
    };
    let padding = inner_width.saturating_sub(visible_len);
    format!(
        "{} {}{} {}",
        postmortem_tone_text("\u{2502}", tone),
        content,
        " ".repeat(padding),
        postmortem_tone_text("\u{2502}", tone),
    )
}

fn print_postmortem_box(
    title: &str,
    lines: &[PostmortemTerminalLine],
    tone: PostmortemTone,
    width: usize,
) {
    if lines.is_empty() {
        return;
    }
    println!("{}", postmortem_top_border(title, width, tone));
    for line in lines {
        println!("{}", postmortem_box_line(line, width, tone));
    }
    println!("{}", postmortem_bottom_border(width, tone));
}

fn print_postmortem_section(
    title: &str,
    lines: &[PostmortemTerminalLine],
    tone: PostmortemTone,
    width: usize,
) {
    if lines.is_empty() {
        return;
    }
    println!();
    print_postmortem_box(title, lines, tone, width);
}

fn postmortem_key_value_lines(
    rows: Vec<(&str, String)>,
    content_width: usize,
) -> Vec<PostmortemTerminalLine> {
    let label_width = rows
        .iter()
        .map(|(label, _)| label.len())
        .max()
        .unwrap_or(8)
        .clamp(8, 28);
    let value_width = content_width.saturating_sub(label_width + 2).max(12);
    let mut lines = Vec::new();
    for (label, value) in rows {
        for (idx, chunk) in postmortem_wrap_words(&value, value_width)
            .into_iter()
            .enumerate()
        {
            let label_cell = if idx == 0 {
                format!("{label:<label_width$}")
            } else {
                " ".repeat(label_width)
            };
            let plain = format!("{label_cell}  {chunk}");
            let styled_label = if idx == 0 {
                label_cell.bright_blue().bold().to_string()
            } else {
                label_cell
            };
            let styled = format!(
                "{styled_label}  {}",
                colorize_postmortem_value(label, &chunk)
            );
            lines.push(PostmortemTerminalLine::styled(plain, styled));
        }
    }
    lines
}

fn postmortem_state(report: &serde_json::Value) -> &'static str {
    if report
        .get("partial")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
    {
        "partial - session may still be running"
    } else {
        "complete saved report"
    }
}

fn postmortem_token_line(impact: &serde_json::Value) -> String {
    format!(
        "input {} ({} cached, {} new), output {}, internal reasoning {}, total {}",
        number_field(impact, "input_tokens"),
        number_field(impact, "cached_input_tokens"),
        number_field(impact, "uncached_input_tokens"),
        number_field(impact, "output_tokens"),
        number_field(impact, "reasoning_output_tokens"),
        number_field(impact, "local_total_tokens"),
    )
}

fn postmortem_model_line(summary: &serde_json::Value) -> Option<String> {
    match (
        json_str(summary, "requested_model"),
        json_str(summary, "served_model"),
    ) {
        (Some(requested), Some(served)) if requested == served => Some(requested.to_string()),
        (Some(requested), Some(served)) => {
            Some(format!("requested {requested}, answered {served}"))
        }
        (Some(requested), None) => Some(requested.to_string()),
        (None, Some(served)) => Some(format!("answered {served}")),
        (None, None) => None,
    }
}

fn postmortem_local_estimate_line(impact: &serde_json::Value) -> String {
    let cost = impact
        .get("local_estimated_cost_dollars")
        .and_then(|value| value.as_f64())
        .unwrap_or(0.0);
    let mut parts = vec![format!("estimated ${cost:.2}")];
    if let Some(trusted) = impact
        .get("local_estimate_trusted_for_budget_enforcement")
        .and_then(|value| value.as_bool())
    {
        parts.push(if trusted {
            "budget stops enabled".to_string()
        } else {
            "budget stops advisory".to_string()
        });
    }
    if let Some(source) = json_str(impact, "local_estimate_source") {
        parts.push(humanize_postmortem_source(source));
    }
    parts.join(", ")
}

fn postmortem_response_statuses_line(signals: &serde_json::Value) -> Option<String> {
    signals
        .get("response_statuses")
        .and_then(|value| value.as_object())
        .map(|statuses| {
            statuses
                .iter()
                .filter_map(|(status, count)| count.as_u64().map(|count| (status, count)))
                .filter(|(_, count)| *count > 0)
                .map(|(status, count)| format!("{status}: {count}"))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .filter(|rendered| !rendered.is_empty())
}

fn postmortem_status_count(report: &serde_json::Value, status: &str) -> u64 {
    report
        .get("signals")
        .and_then(|signals| signals.get("response_statuses"))
        .and_then(|statuses| statuses.get(status))
        .and_then(|value| value.as_u64())
        .unwrap_or(0)
}

fn postmortem_report_tone(report: &serde_json::Value) -> PostmortemTone {
    let summary = report.get("summary").unwrap_or(&serde_json::Value::Null);
    let diagnosis = report.get("diagnosis").unwrap_or(&serde_json::Value::Null);
    let outcome = json_str(summary, "outcome")
        .unwrap_or("")
        .to_ascii_lowercase();
    let primary_cause = json_str(diagnosis, "primary_cause")
        .unwrap_or("")
        .to_ascii_lowercase();
    if postmortem_status_count(report, "failed") > 0
        || outcome.contains("failed")
        || outcome.contains("error")
        || primary_cause.contains("failed")
        || primary_cause.contains("error")
    {
        PostmortemTone::Danger
    } else if postmortem_status_count(report, "incomplete") > 0
        || outcome.contains("partial")
        || outcome.contains("incomplete")
        || (!primary_cause.is_empty() && primary_cause != "none")
    {
        PostmortemTone::Warn
    } else {
        PostmortemTone::Good
    }
}

fn postmortem_terminal_header_lines(
    report: &serde_json::Value,
    content_width: usize,
) -> Vec<PostmortemTerminalLine> {
    let summary = report.get("summary").unwrap_or(&serde_json::Value::Null);
    let impact = report.get("impact").unwrap_or(&serde_json::Value::Null);
    let redaction = if report
        .get("redacted")
        .and_then(|value| value.as_bool())
        .unwrap_or(true)
    {
        "redacted"
    } else {
        "unredacted"
    };

    let mut rows = vec![
        (
            "Session",
            format!(
                "{} ({redaction})",
                json_str(report, "session_id").unwrap_or("?")
            ),
        ),
        (
            "Result",
            format!(
                "{}; {} turns",
                json_str(summary, "outcome").unwrap_or("?"),
                summary
                    .get("turn_count")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(0)
            ),
        ),
    ];
    if let Some(model) = postmortem_model_line(summary) {
        rows.push(("Model", model));
    }
    rows.push((
        "Usage",
        format!(
            "{} local tokens; {}",
            number_field(impact, "local_total_tokens"),
            postmortem_local_estimate_line(impact)
        ),
    ));
    rows.push((
        "Scope",
        "Local proxy traffic only; tool rows show requests, not success.".to_string(),
    ));
    postmortem_key_value_lines(rows, content_width)
}

fn postmortem_snapshot_terminal_lines(
    report: &serde_json::Value,
    content_width: usize,
) -> Vec<PostmortemTerminalLine> {
    let summary = report.get("summary").unwrap_or(&serde_json::Value::Null);
    let impact = report.get("impact").unwrap_or(&serde_json::Value::Null);
    let diagnosis = report.get("diagnosis").unwrap_or(&serde_json::Value::Null);
    let cost = impact
        .get("local_estimated_cost_dollars")
        .and_then(|value| value.as_f64())
        .unwrap_or(0.0);
    let mut rows = vec![
        ("State", postmortem_state(report).to_string()),
        ("Tokens", postmortem_token_line(impact)),
        ("Estimated Cost", format!("${cost:.2}")),
        (
            "Likely Cause",
            humanize_postmortem_cause(json_str(diagnosis, "primary_cause").unwrap_or("none")),
        ),
    ];
    if let Some(source) = json_str(impact, "local_estimate_source") {
        rows.push(("Pricing", humanize_postmortem_source(source)));
    }
    if let Some(trusted) = impact
        .get("local_estimate_trusted_for_budget_enforcement")
        .and_then(|value| value.as_bool())
    {
        rows.push((
            "Cost Confidence",
            if trusted {
                "trusted for budget stops".to_string()
            } else {
                "untrusted - dollar budgets stay advisory".to_string()
            },
        ));
    }
    if let Some(billed) = impact
        .get("billed_reconciliation")
        .and_then(|value| value.as_object())
        .and_then(|map| map.get("billed_cost_dollars"))
        .and_then(|value| value.as_f64())
    {
        rows.push(("Billed Cost", format!("${billed:.2}")));
    }
    if let Some(prompt) = json_str(summary, "initial_prompt_excerpt") {
        rows.push(("Prompt", prompt.to_string()));
    }
    if let Some(summary_text) = json_str(summary, "final_response_summary") {
        rows.push(("Final Answer", summary_text.to_string()));
    }
    postmortem_key_value_lines(rows, content_width)
}

fn postmortem_signals_terminal_lines(
    report: &serde_json::Value,
    content_width: usize,
) -> Vec<PostmortemTerminalLine> {
    let Some(signals) = report.get("signals") else {
        return Vec::new();
    };
    let mut rows: Vec<(&str, String)> = Vec::new();
    if let Some(rendered) = postmortem_response_statuses_line(signals) {
        rows.push(("Response results", rendered));
    }
    if let Some(context) = signals.get("context_fill") {
        let percent = context
            .get("max_percent")
            .and_then(|value| value.as_f64())
            .unwrap_or(0.0);
        rows.push(("Context used", format!("{percent:.1}% max")));
    }
    if let Some(cache) = signals.get("cached_input_reuse") {
        if let Some(ratio) = cache.get("ratio").and_then(|value| value.as_f64()) {
            rows.push(("Cache reuse", format!("{:.0}% reused", ratio * 100.0)));
        }
    }
    if let Some(reasoning) = signals.get("reasoning_output_share") {
        if let Some(ratio) = reasoning.get("max_ratio").and_then(|value| value.as_f64()) {
            rows.push((
                "Reasoning share",
                format!("{:.0}% of output tokens", ratio * 100.0),
            ));
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
            rows.push(("Tool requests", rendered));
        }
    }
    postmortem_key_value_lines(rows, content_width)
}

fn postmortem_evidence_terminal_lines(report: &serde_json::Value) -> Vec<PostmortemTerminalLine> {
    report
        .get("evidence")
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
        .take(12)
        .enumerate()
        .map(|(idx, row)| {
            let kind = humanize_evidence_kind(json_str(row, "type").unwrap_or("signal"));
            let signal = humanize_postmortem_signal(json_str(row, "signal").unwrap_or("unknown"));
            let turn = row
                .get("turn")
                .and_then(|value| value.as_u64())
                .unwrap_or(0);
            let detail = postmortem_table_cell(&humanize_postmortem_detail(
                json_str(row, "detail").unwrap_or(""),
            ));
            let index = idx + 1;
            let plain = format!("{index}. [{kind}] {signal} turn {turn}: {detail}");
            let styled = format!(
                "{} [{}] {} {} {}",
                format!("{index}.").bright_white().bold(),
                colorize_evidence_kind(kind),
                signal.cyan(),
                format!("turn {turn}:").bright_black(),
                detail.bright_white()
            );
            PostmortemTerminalLine::styled(plain, styled)
        })
        .collect()
}

fn postmortem_timeline_terminal_lines(report: &serde_json::Value) -> Vec<PostmortemTerminalLine> {
    report
        .get("timeline")
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
        .take(14)
        .map(|row| {
            let timestamp = json_str(row, "timestamp").unwrap_or("");
            let event = humanize_postmortem_event(json_str(row, "event").unwrap_or("event"));
            let detail = postmortem_table_cell(&humanize_postmortem_detail(
                json_str(row, "detail").unwrap_or(""),
            ));
            let plain = format!("{timestamp}  {event}  {detail}");
            let styled = format!(
                "{}  {}  {}",
                timestamp.bright_black(),
                event.cyan(),
                detail.bright_white()
            );
            PostmortemTerminalLine::styled(plain, styled)
        })
        .collect()
}

fn postmortem_flight_model(row: &serde_json::Value) -> String {
    match (
        json_str(row, "requested_model"),
        json_str(row, "served_model"),
        row.get("model_mismatch")
            .and_then(|value| value.as_bool())
            .unwrap_or(false),
    ) {
        (Some(requested), Some(served), true) => format!("{requested}->{served}"),
        (Some(requested), _, _) => requested.to_string(),
        (None, Some(served), _) => served.to_string(),
        (None, None, _) => "unknown".to_string(),
    }
}

fn postmortem_flight_tokens(row: &serde_json::Value) -> String {
    let input = row
        .get("input_tokens")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    let cached = row
        .get("cached_input_tokens")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    let uncached = row
        .get("uncached_input_tokens")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    let output = row
        .get("output_tokens")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    let reasoning = row
        .get("reasoning_output_tokens")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    let total = row
        .get("local_total_tokens")
        .and_then(|value| value.as_u64())
        .unwrap_or(input.saturating_add(output));
    format!(
        "in {}, cached {}, uncached {}, out {}, reasoning {}, total {}",
        format_tokens(input),
        format_tokens(cached),
        format_tokens(uncached),
        format_tokens(output),
        format_tokens(reasoning),
        format_tokens(total)
    )
}

fn postmortem_flight_context(row: &serde_json::Value) -> String {
    let Some(percent) = row
        .get("context_fill_percent")
        .and_then(|value| value.as_f64())
    else {
        return "context unknown".to_string();
    };
    match row
        .get("context_window_tokens")
        .and_then(|value| value.as_u64())
    {
        Some(window) => format!("{percent:.1}% of {} window", format_tokens(window)),
        None => format!("{percent:.1}%"),
    }
}

fn postmortem_flight_duration(row: &serde_json::Value) -> String {
    row.get("duration_ms")
        .and_then(|value| value.as_u64())
        .map(|duration| format!("{duration}ms"))
        .unwrap_or_else(|| "duration unknown".to_string())
}

fn postmortem_flight_recorder_terminal_lines(
    report: &serde_json::Value,
) -> Vec<PostmortemTerminalLine> {
    report
        .get("flight_recorder")
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
        .take(20)
        .map(|row| {
            let turn = row
                .get("turn")
                .and_then(|value| value.as_u64())
                .unwrap_or(0);
            let status = json_str(row, "status").unwrap_or("unknown");
            let model = postmortem_flight_model(row);
            let tokens = postmortem_flight_tokens(row);
            let context = postmortem_flight_context(row);
            let duration = postmortem_flight_duration(row);
            let plain =
                format!("Turn {turn} | {status} | {model} | {tokens} | {context} | {duration}");
            let styled = format!(
                "{} {} {} {} {} {} {} {} {} {} {}",
                format!("Turn {turn}").bright_white().bold(),
                "|".bright_black(),
                status.cyan(),
                "|".bright_black(),
                model.bright_white(),
                "|".bright_black(),
                tokens.bright_white(),
                "|".bright_black(),
                context.bright_cyan(),
                "|".bright_black(),
                duration.bright_black()
            );
            PostmortemTerminalLine::styled(plain, styled)
        })
        .collect()
}

fn postmortem_string_list_terminal_lines(
    report: &serde_json::Value,
    key: &str,
    tone: PostmortemTone,
    content_width: usize,
) -> Vec<PostmortemTerminalLine> {
    let mut lines = Vec::new();
    for (idx, item) in report
        .get(key)
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
        .filter_map(|row| row.as_str())
        .enumerate()
    {
        let index = idx + 1;
        let item = postmortem_table_cell(&humanize_postmortem_note(key, item));
        let prefix = format!("{index}. ");
        let value_width = content_width
            .saturating_sub(postmortem_visible_len(&prefix))
            .max(12);
        for (line_idx, chunk) in postmortem_wrap_words(&item, value_width)
            .into_iter()
            .enumerate()
        {
            let line_prefix = if line_idx == 0 {
                prefix.clone()
            } else {
                " ".repeat(postmortem_visible_len(&prefix))
            };
            let plain = format!("{line_prefix}{chunk}");
            let styled_item = match tone {
                PostmortemTone::Warn => chunk.bright_cyan().bold().to_string(),
                PostmortemTone::Muted => chunk.dimmed().to_string(),
                _ => chunk.bright_white().to_string(),
            };
            let styled_prefix = if line_idx == 0 {
                format!("{index}.").bright_white().bold().to_string() + " "
            } else {
                line_prefix
            };
            lines.push(PostmortemTerminalLine::styled(
                plain,
                format!("{styled_prefix}{styled_item}"),
            ));
        }
    }
    lines
}

fn postmortem_restart_prompt_terminal_lines(
    report: &serde_json::Value,
    content_width: usize,
) -> Vec<PostmortemTerminalLine> {
    json_str(report, "restart_prompt")
        .filter(|value| !value.trim().is_empty())
        .map(|prompt| {
            postmortem_wrap_words(prompt, content_width)
                .into_iter()
                .map(|line| {
                    PostmortemTerminalLine::styled(line.clone(), line.bright_cyan().to_string())
                })
                .collect()
        })
        .unwrap_or_default()
}

fn print_postmortem_terminal_report(report: &serde_json::Value, color_mode: ColorMode) {
    color_control::set_override(color_enabled(
        color_mode,
        stdout_is_terminal(),
        std::env::var_os("NO_COLOR").is_some(),
    ));

    let width = postmortem_terminal_width();
    let content_width = width.saturating_sub(4);
    let report_tone = postmortem_report_tone(report);
    let signal_tone = match report_tone {
        PostmortemTone::Good => PostmortemTone::Info,
        other => other,
    };

    print_postmortem_section(
        "Codex Session Report",
        &postmortem_terminal_header_lines(report, content_width),
        PostmortemTone::Header,
        width,
    );
    print_postmortem_section(
        "At a Glance",
        &postmortem_snapshot_terminal_lines(report, content_width),
        report_tone,
        width,
    );
    print_postmortem_section(
        "Checks",
        &postmortem_signals_terminal_lines(report, content_width),
        signal_tone,
        width,
    );
    print_postmortem_section(
        "Flight Recorder",
        &postmortem_flight_recorder_terminal_lines(report),
        PostmortemTone::Info,
        width,
    );
    print_postmortem_section(
        "What Triggered This",
        &postmortem_evidence_terminal_lines(report),
        signal_tone,
        width,
    );
    print_postmortem_section(
        "Timeline",
        &postmortem_timeline_terminal_lines(report),
        PostmortemTone::Info,
        width,
    );
    print_postmortem_section(
        "Next Steps",
        &postmortem_string_list_terminal_lines(
            report,
            "recommendations",
            PostmortemTone::Warn,
            content_width,
        ),
        PostmortemTone::Warn,
        width,
    );
    print_postmortem_section(
        "Limits",
        &postmortem_string_list_terminal_lines(
            report,
            "caveats",
            PostmortemTone::Muted,
            content_width,
        ),
        PostmortemTone::Muted,
        width,
    );
    print_postmortem_section(
        "Continue Prompt",
        &postmortem_restart_prompt_terminal_lines(report, content_width),
        PostmortemTone::Info,
        width,
    );
    color_control::unset_override();
}

#[cfg(unix)]
fn terminal_width_from_stdout() -> Option<usize> {
    #[repr(C)]
    struct Winsize {
        ws_row: u16,
        ws_col: u16,
        ws_xpixel: u16,
        ws_ypixel: u16,
    }

    #[cfg(target_os = "linux")]
    const TIOCGWINSZ: std::os::raw::c_ulong = 0x5413;
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd"
    ))]
    const TIOCGWINSZ: std::os::raw::c_ulong = 0x40087468;
    #[cfg(not(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd"
    )))]
    return None;

    unsafe extern "C" {
        fn ioctl(
            fd: std::os::raw::c_int,
            request: std::os::raw::c_ulong,
            size: *mut Winsize,
        ) -> std::os::raw::c_int;
    }

    let mut size = Winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let rc = unsafe { ioctl(1, TIOCGWINSZ, &mut size) };
    (rc == 0 && size.ws_col >= 20).then_some(size.ws_col as usize)
}

#[cfg(not(unix))]
fn terminal_width_from_stdout() -> Option<usize> {
    None
}

fn terminal_width_from_columns_env() -> Option<usize> {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|width| *width >= 20)
}

fn terminal_width() -> usize {
    terminal_width_from_stdout()
        .or_else(terminal_width_from_columns_env)
        .unwrap_or(100)
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

fn postmortem_table_cell(value: &str) -> String {
    let single_line = value.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_for_box(&single_line, 140)
}

fn postmortem_column_widths(headers: &[&str]) -> Vec<usize> {
    match headers {
        ["Type", "Signal", "Turn", "Detail"] => vec![10, 28, 5, 0],
        ["Kind", "Check", "Turn", "Detail"] => vec![10, 28, 5, 0],
        ["Time", "Event", "Detail"] => vec![20, 18, 0],
        ["Time", "Step", "Detail"] => vec![20, 18, 0],
        ["Turn", "Time", "Status", "Model", "Tokens", "Context", "Duration"] => {
            vec![6, 20, 10, 18, 34, 18, 0]
        }
        _ => headers
            .iter()
            .enumerate()
            .map(|(idx, header)| {
                if idx + 1 == headers.len() {
                    0
                } else {
                    header.len().max(8)
                }
            })
            .collect(),
    }
}

fn push_postmortem_table_header(out: &mut String, headers: &[&str]) {
    let widths = postmortem_column_widths(headers);
    out.push_str("  ");
    for (idx, header) in headers.iter().enumerate() {
        let width = widths.get(idx).copied().unwrap_or(0);
        if width == 0 {
            out.push_str(header);
        } else {
            out.push_str(&format!("{:<width$}", header, width = width));
            out.push_str("  ");
        }
    }
    out.push('\n');
    out.push_str("  ");
    for (idx, header) in headers.iter().enumerate() {
        let width = widths.get(idx).copied().unwrap_or(0);
        let underline_width = if width == 0 {
            header.len().max(6)
        } else {
            width
        };
        out.push_str(&"-".repeat(underline_width));
        if width != 0 {
            out.push_str("  ");
        }
    }
    out.push('\n');
}

fn push_postmortem_table_row(out: &mut String, headers: &[&str], cells: &[String]) {
    let widths = postmortem_column_widths(headers);
    out.push_str("  ");
    for (idx, cell) in cells.iter().enumerate() {
        let width = widths.get(idx).copied().unwrap_or(0);
        let value = postmortem_table_cell(cell);
        if width == 0 {
            out.push_str(&value);
        } else {
            out.push_str(&format!("{:<width$}", value, width = width));
            out.push_str("  ");
        }
    }
    out.push('\n');
}

fn push_key_value_table(out: &mut String, rows: Vec<(&str, String)>) {
    let label_width = rows
        .iter()
        .map(|(field, _)| field.len())
        .max()
        .unwrap_or(5)
        .clamp(10, 28);
    for (field, value) in rows {
        out.push_str(&format!(
            "  {:<label_width$}  {}\n",
            field,
            postmortem_table_cell(&value),
            label_width = label_width
        ));
    }
    out.push('\n');
}

fn render_postmortem_markdown(report: &serde_json::Value) -> String {
    let mut out = String::new();
    out.push_str("# Codex Session Report\n\n");
    render_postmortem_snapshot(report, &mut out);
    render_postmortem_signals(report, &mut out);
    render_postmortem_flight_recorder(report, &mut out);
    render_postmortem_evidence(report, &mut out);
    render_postmortem_timeline(report, &mut out);
    render_postmortem_recommendations(report, &mut out);
    render_postmortem_caveats(report, &mut out);
    if let Some(prompt) = report
        .get("restart_prompt")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
    {
        out.push_str("\n## Continue Prompt\n");
        out.push_str("```text\n");
        out.push_str(prompt);
        out.push_str("\n```\n");
    }
    out
}

fn render_postmortem_flight_recorder(report: &serde_json::Value, out: &mut String) {
    let rows = report
        .get("flight_recorder")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    if rows.is_empty() {
        return;
    }
    out.push_str("\n## Flight Recorder\n");
    let headers = [
        "Turn", "Time", "Status", "Model", "Tokens", "Context", "Duration",
    ];
    push_postmortem_table_header(out, &headers);
    for row in rows.iter().take(20) {
        let turn = row
            .get("turn")
            .and_then(|value| value.as_u64())
            .unwrap_or(0);
        let timestamp = json_str(row, "timestamp").unwrap_or("");
        let status = json_str(row, "status").unwrap_or("unknown");
        push_postmortem_table_row(
            out,
            &headers,
            &[
                turn.to_string(),
                timestamp.to_string(),
                status.to_string(),
                postmortem_flight_model(row),
                postmortem_flight_tokens(row),
                postmortem_flight_context(row),
                postmortem_flight_duration(row),
            ],
        );
    }
}

fn render_postmortem_snapshot(report: &serde_json::Value, out: &mut String) {
    let summary = report.get("summary").unwrap_or(&serde_json::Value::Null);
    let impact = report.get("impact").unwrap_or(&serde_json::Value::Null);
    let diagnosis = report.get("diagnosis").unwrap_or(&serde_json::Value::Null);
    out.push_str("## At a Glance\n");
    push_md_line(
        out,
        "Session",
        json_str(report, "session_id").unwrap_or("?"),
    );
    push_md_line(out, "State", postmortem_state(report));
    push_md_line(out, "Result", json_str(summary, "outcome").unwrap_or("?"));
    if let Some(model) = postmortem_model_line(summary) {
        push_md_line(out, "Model", &model);
    }
    let turn_count = summary
        .get("turn_count")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    push_md_line(out, "Turns", &turn_count.to_string());
    push_md_line(out, "Tokens", &postmortem_token_line(impact));
    let cost = impact
        .get("local_estimated_cost_dollars")
        .and_then(|value| value.as_f64())
        .unwrap_or(0.0);
    push_md_line(out, "Estimated Cost", &format!("${cost:.2}"));
    if let Some(source) = json_str(impact, "local_estimate_source") {
        push_md_line(out, "Pricing", &humanize_postmortem_source(source));
    }
    if let Some(trusted) = impact
        .get("local_estimate_trusted_for_budget_enforcement")
        .and_then(|value| value.as_bool())
    {
        push_md_line(
            out,
            "Cost Confidence",
            if trusted {
                "trusted for budget stops"
            } else {
                "untrusted - dollar budgets stay advisory"
            },
        );
    }
    if let Some(billed) = impact
        .get("billed_reconciliation")
        .and_then(|value| value.as_object())
        .and_then(|map| map.get("billed_cost_dollars"))
        .and_then(|value| value.as_f64())
    {
        push_md_line(out, "Billed Cost", &format!("${billed:.2}"));
    }
    push_md_line(
        out,
        "Likely Cause",
        &humanize_postmortem_cause(json_str(diagnosis, "primary_cause").unwrap_or("none")),
    );
    if let Some(prompt) = json_str(summary, "initial_prompt_excerpt") {
        push_md_line(out, "Prompt", prompt);
    }
    if let Some(summary_text) = json_str(summary, "final_response_summary") {
        push_md_line(out, "Final Answer", summary_text);
    }
}

fn render_postmortem_signals(report: &serde_json::Value, out: &mut String) {
    let Some(signals) = report.get("signals") else {
        return;
    };
    out.push_str("\n## Checks\n");
    let mut rows: Vec<(&str, String)> = Vec::new();
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
            rows.push(("Response results", rendered));
        }
    }
    if let Some(context) = signals.get("context_fill") {
        let percent = context
            .get("max_percent")
            .and_then(|value| value.as_f64())
            .unwrap_or(0.0);
        rows.push(("Context used", format!("{percent:.1}% max")));
    }
    if let Some(cache) = signals.get("cached_input_reuse") {
        if let Some(ratio) = cache.get("ratio").and_then(|value| value.as_f64()) {
            rows.push(("Cache reuse", format!("{:.0}% reused", ratio * 100.0)));
        }
    }
    if let Some(reasoning) = signals.get("reasoning_output_share") {
        if let Some(ratio) = reasoning.get("max_ratio").and_then(|value| value.as_f64()) {
            rows.push((
                "Reasoning share",
                format!("{:.0}% of output tokens", ratio * 100.0),
            ));
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
            rows.push(("Tool requests", rendered));
        }
    }
    push_key_value_table(out, rows);
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
    out.push_str("\n## What Triggered This\n");
    let headers = ["Kind", "Check", "Turn", "Detail"];
    push_postmortem_table_header(out, &headers);
    for row in rows.iter().take(12) {
        let kind = humanize_evidence_kind(json_str(row, "type").unwrap_or("signal"));
        let signal = humanize_postmortem_signal(json_str(row, "signal").unwrap_or("unknown"));
        let turn = row
            .get("turn")
            .and_then(|value| value.as_u64())
            .unwrap_or(0);
        let detail = humanize_postmortem_detail(json_str(row, "detail").unwrap_or(""));
        push_postmortem_table_row(
            out,
            &headers,
            &[kind.to_string(), signal, turn.to_string(), detail],
        );
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
    let headers = ["Time", "Step", "Detail"];
    push_postmortem_table_header(out, &headers);
    for row in rows.iter().take(14) {
        let timestamp = json_str(row, "timestamp").unwrap_or("");
        let event = humanize_postmortem_event(json_str(row, "event").unwrap_or("event"));
        let detail = humanize_postmortem_detail(json_str(row, "detail").unwrap_or(""));
        push_postmortem_table_row(out, &headers, &[timestamp.to_string(), event, detail]);
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
    out.push_str("\n## Next Steps\n");
    for row in rows {
        if let Some(item) = row.as_str() {
            push_md_bullet(out, &humanize_postmortem_note("recommendations", item));
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
    out.push_str("\n## Limits\n");
    for row in rows {
        if let Some(item) = row.as_str() {
            push_md_bullet(out, &humanize_postmortem_note("caveats", item));
        }
    }
}

fn push_md_line(out: &mut String, label: &str, value: &str) {
    out.push_str(&format!(
        "  {:<24}  {}\n",
        label,
        postmortem_table_cell(value)
    ));
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

async fn maybe_render_watch_postmortem(
    event: &WatchEvent,
    options: &WatchRenderOptions,
    state: &mut WatchRuntimeState,
) {
    let WatchEvent::PostmortemReady { session_id, .. } = event else {
        return;
    };
    if !options.postmortem || !state.rendered_postmortems.insert(session_id.clone()) {
        return;
    }

    if let Err(err) = fetch_postmortem(
        &options.base_url,
        session_id,
        options.redact_postmortem,
        None,
        options.color_mode,
    )
    .await
    {
        eprintln!(
            "{}",
            format!("Error: failed to render postmortem for {session_id}: {err}").red()
        );
    }
}

async fn connect_and_stream(
    url: &str,
    options: &WatchRenderOptions,
    state: &mut WatchRuntimeState,
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
                    if event_matches_session_filter(&event, &options.session_filter) {
                        render_event(
                            &event,
                            options.no_signals,
                            &options.session_filter,
                            &mut state.active,
                        );
                        if let Some(decision) = state.decisions.update(&event) {
                            if state.remember_decision_if_changed(&event, &decision) {
                                let footer = render_decision_footer(
                                    &decision,
                                    terminal_width(),
                                    options.color_mode,
                                    stdout_is_terminal(),
                                    std::env::var_os("NO_COLOR").is_some(),
                                );
                                println!("{footer}");
                            }
                        }
                        maybe_render_watch_postmortem(&event, options, state).await;
                    }
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
        codex_exec_prompt_hint, codex_model_proxy_base_url, codex_subscription_config_overrides,
        codex_turn_summary_line, compact_datetime_from_iso, context_status_line,
        enforce_codex_observation, event_session_id, extract_run_watch, format_duration_coarse,
        format_tokens, guard_report_to_decision, local_time_from_iso, model_change_line,
        parse_codex_session_id_line, parse_mcp_tool_name, push_unique, render_child_run_plan,
        render_codex_config_preview, render_decision_footer, render_decision_footer_json,
        render_decision_footer_plain, shell_join, shell_quote, should_suppress_codex_stderr_line,
        status_report_to_facts, tmux_orchestrator_watch_url, truncate_for_box, watch_model_label,
        yaml_quote, ActiveSessions, ChildStdinMode, Cli, CodexObservationScope,
        CodexTurnSummaryLine, ColorMode, Commands, ConfigCommands, RunMode, WatchEvent,
        WatchRenderOptions, WatchRetryLog, WatchRuntimeState,
    };
    use chrono::{DateTime, Local};
    use clap::Parser;
    use codex_blackbox_core::decision::{
        decide, CooldownFacts, Decision, DecisionState, ObservedSessionFacts, PolicyBlockFacts,
    };
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

    fn serve_json_once(body: &str) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind json server");
        let url = format!("http://{}", listener.local_addr().expect("local addr"));
        let body = body.to_string();
        let (tx, rx) = mpsc::channel();

        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept json request");
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
                .expect("send captured json request");

            write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .expect("write json response");
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

    fn footer_facts() -> ObservedSessionFacts {
        ObservedSessionFacts {
            session_id: Some("session_footer".to_string()),
            observed_codex_responses: true,
            total_turns: 3,
            total_tokens: 42_000,
            max_context_fill_percent: Some(31.0),
            local_estimate_trusted_for_budget_enforcement: Some(true),
            ..Default::default()
        }
    }

    fn ansi_free(value: &str) -> String {
        let mut out = String::new();
        let mut chars = value.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '\x1b' && chars.peek() == Some(&'[') {
                let _ = chars.next();
                for ansi in chars.by_ref() {
                    if ansi.is_ascii_alphabetic() {
                        break;
                    }
                }
                continue;
            }
            out.push(ch);
        }
        out
    }

    fn assert_no_ansi(value: &str) {
        assert!(
            !value.contains("\x1b["),
            "expected no ANSI escapes, got {value:?}"
        );
    }

    #[test]
    fn footer_renders_all_decision_states_with_reason_and_action() {
        let cases: Vec<(&str, Decision)> = vec![
            ("Healthy", decide(&footer_facts())),
            ("Watching", decide(&ObservedSessionFacts::default())),
            {
                let mut facts = footer_facts();
                facts.max_context_fill_percent = Some(72.0);
                ("Careful", decide(&facts))
            },
            {
                let mut facts = footer_facts();
                facts.failed_responses = 1;
                ("Stop", decide(&facts))
            },
            (
                "Blocked",
                decide(&ObservedSessionFacts {
                    session_id: Some("session_footer".to_string()),
                    policy_block: Some(PolicyBlockFacts {
                        rule: "session_token_budget".to_string(),
                        reason: "token budget exceeded".to_string(),
                        current: Some("125000 tokens".to_string()),
                        limit: Some("120000 tokens".to_string()),
                        session_id: Some("session_footer".to_string()),
                        recovery_action: "restart narrower".to_string(),
                    }),
                    ..Default::default()
                }),
            ),
            (
                "Cooldown",
                decide(&ObservedSessionFacts {
                    cooldown: Some(CooldownFacts {
                        reason: "upstream errors".to_string(),
                        retry_after_seconds: Some(30),
                    }),
                    ..Default::default()
                }),
            ),
            {
                let mut facts = footer_facts();
                facts.ended = true;
                ("Ended", decide(&facts))
            },
        ];

        for (state_word, decision) in cases {
            let line = render_decision_footer_plain(&decision, 120);
            assert!(line.contains(state_word), "{state_word}: {line}");
            assert!(
                line.contains(&decision.primary_reason),
                "missing reason for {state_word}: {line}"
            );
            assert!(
                line.contains(&decision.next_action),
                "missing action for {state_word}: {line}"
            );
            assert!(!line.contains('\n'));
        }
    }

    #[test]
    fn footer_color_modes_respect_state_no_color_and_tty() {
        let states = [
            (decide(&footer_facts()), "\x1b[32m"),
            (decide(&ObservedSessionFacts::default()), "\x1b[36m"),
            {
                let mut facts = footer_facts();
                facts.max_context_fill_percent = Some(72.0);
                (decide(&facts), "\x1b[33m")
            },
            {
                let mut facts = footer_facts();
                facts.failed_responses = 1;
                (decide(&facts), "\x1b[31m")
            },
            (
                decide(&ObservedSessionFacts {
                    policy_block: Some(PolicyBlockFacts {
                        reason: "token budget exceeded".to_string(),
                        recovery_action: "restart narrower".to_string(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                "\x1b[1;31m",
            ),
            (
                decide(&ObservedSessionFacts {
                    cooldown: Some(CooldownFacts {
                        reason: "upstream errors".to_string(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                "\x1b[33m",
            ),
            {
                let mut facts = footer_facts();
                facts.ended = true;
                (decide(&facts), "\x1b[2;90m")
            },
        ];

        for (decision, expected_prefix) in states {
            let always = render_decision_footer(&decision, 120, ColorMode::Always, false, false);
            assert!(
                always.starts_with(expected_prefix),
                "wrong color for {:?}: {always:?}",
                decision.state
            );
            assert!(always.ends_with("\x1b[0m"));

            assert_no_ansi(&render_decision_footer(
                &decision,
                120,
                ColorMode::Never,
                true,
                false,
            ));
            assert_no_ansi(&render_decision_footer(
                &decision,
                120,
                ColorMode::Auto,
                true,
                true,
            ));
            assert_no_ansi(&render_decision_footer(
                &decision,
                120,
                ColorMode::Auto,
                false,
                false,
            ));
            assert!(
                render_decision_footer(&decision, 120, ColorMode::Auto, true, false)
                    .starts_with(expected_prefix)
            );
        }
    }

    #[test]
    fn footer_degrades_by_width_without_losing_state_or_reason() {
        let mut facts = footer_facts();
        facts.max_context_fill_percent = Some(72.0);
        let decision = decide(&facts);

        for width in [120, 100, 80, 60, 44, 24] {
            let rendered = render_decision_footer(&decision, width, ColorMode::Always, true, false);
            let plain = ansi_free(&rendered);
            assert!(
                plain.len() <= width,
                "width {width} produced {} chars: {plain}",
                plain.len()
            );
            assert!(plain.contains("Careful"), "width {width}: {plain}");
            assert!(plain.contains("context"), "width {width}: {plain}");
            assert!(!plain.contains('\n'));
        }
    }

    #[test]
    fn footer_degrades_standard_reasons_without_obscuring_them() {
        let cases = [
            (
                "Watching",
                decide(&ObservedSessionFacts::default()),
                ["waiting for request", "waiting"],
            ),
            {
                let mut facts = footer_facts();
                facts.failed_responses = 1;
                (
                    "Stop",
                    decide(&facts),
                    ["response failed", "response failed"],
                )
            },
            (
                "Blocked",
                decide(&ObservedSessionFacts {
                    policy_block: Some(PolicyBlockFacts {
                        reason: "token budget exceeded".to_string(),
                        recovery_action: "restart narrower".to_string(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ["budget exceeded", "budget"],
            ),
            (
                "Cooldown",
                decide(&ObservedSessionFacts {
                    cooldown: Some(CooldownFacts {
                        reason: "upstream errors".to_string(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ["upstream errors", "upstream"],
            ),
            {
                let mut facts = footer_facts();
                facts.local_estimate_trusted_for_budget_enforcement = Some(false);
                (
                    "Careful",
                    decide(&facts),
                    ["local estimate untrusted", "untrusted"],
                )
            },
            {
                let mut facts = footer_facts();
                facts.ended = true;
                ("Ended", decide(&facts), ["3 turns, 42K", "3 turns, 42K"])
            },
        ];

        for (state, decision, expected_by_width) in cases {
            for (width, expected) in [(44, expected_by_width[0]), (24, expected_by_width[1])] {
                let plain = render_decision_footer_plain(&decision, width);
                assert!(plain.len() <= width, "{state} width {width}: {plain}");
                assert!(plain.contains(state), "{state} width {width}: {plain}");
                assert!(
                    plain.contains(expected),
                    "{state} width {width} missing {expected:?}: {plain}"
                );
                assert!(!plain.contains("..."), "{state} width {width}: {plain}");
            }
        }
    }

    #[test]
    fn footer_includes_postmortem_command_for_risky_states_when_it_fits() {
        let mut careful = footer_facts();
        careful.max_context_fill_percent = Some(72.0);

        let mut stop = footer_facts();
        stop.failed_responses = 1;

        let blocked = ObservedSessionFacts {
            session_id: Some("session_footer".to_string()),
            policy_block: Some(PolicyBlockFacts {
                reason: "token budget exceeded".to_string(),
                recovery_action: "restart narrower".to_string(),
                ..Default::default()
            }),
            ..Default::default()
        };

        let cooldown = ObservedSessionFacts {
            session_id: Some("session_footer".to_string()),
            cooldown: Some(CooldownFacts {
                reason: "upstream errors".to_string(),
                ..Default::default()
            }),
            ..Default::default()
        };

        let mut ended = footer_facts();
        ended.ended = true;

        for decision in [
            decide(&careful),
            decide(&stop),
            decide(&blocked),
            decide(&cooldown),
            decide(&ended),
        ] {
            let line = render_decision_footer_plain(&decision, 120);
            assert!(
                line.contains("codex-blackbox postmortem session_footer"),
                "missing drill-down command for {:?}: {line}",
                decision.state
            );
        }
    }

    #[test]
    fn footer_json_output_is_uncolored_and_machine_readable() {
        let mut facts = footer_facts();
        facts.max_context_fill_percent = Some(72.0);
        let decision = decide(&facts);
        let json = render_decision_footer_json(&decision).expect("render decision json");

        assert_no_ansi(&json);
        let value: serde_json::Value = serde_json::from_str(&json).expect("parse footer json");
        assert_eq!(value.get("state").and_then(|v| v.as_str()), Some("careful"));
        assert_eq!(
            value.get("primary_reason").and_then(|v| v.as_str()),
            Some("context 72%")
        );
        assert_eq!(
            value.get("next_action").and_then(|v| v.as_str()),
            Some("narrow next prompt")
        );
        assert_eq!(
            value.get("drill_down_command").and_then(|v| v.as_str()),
            Some("codex-blackbox postmortem session_footer")
        );
        assert_eq!(
            value
                .pointer("/correlation/session_id")
                .and_then(|v| v.as_str()),
            Some("session_footer")
        );
    }

    #[test]
    fn status_postmortem_report_maps_to_public_decision_json() {
        let report = serde_json::json!({
            "session_id": "session_status",
            "partial": false,
            "summary": {"turn_count": 3},
            "impact": {
                "local_total_tokens": 42000,
                "local_estimate_trusted_for_budget_enforcement": true
            },
            "signals": {
                "response_statuses": {
                    "completed": 2,
                    "failed": 1,
                    "incomplete": 0,
                    "unknown": 0
                },
                "context_fill": {"max_percent": 31.0},
                "model_mismatches": [],
                "accounting_anomaly_count": 0
            },
            "diagnosis": {"primary_cause_type": "codex_response_failed"}
        });
        let decision = decide(&status_report_to_facts(&report));
        let rendered = render_decision_footer_json(&decision).expect("status json");

        assert_no_ansi(&rendered);
        assert!(rendered.contains("\"state\":\"stop\""));
        assert!(rendered.contains("\"session_id\":\"session_status\""));
        assert!(rendered.contains("codex-blackbox postmortem session_status"));
    }

    #[test]
    fn status_report_uses_ended_at_even_when_report_is_partial() {
        let report = serde_json::json!({
            "session_id": "session_status",
            "partial": true,
            "summary": {
                "turn_count": 1,
                "ended_at": "2026-05-16T19:34:31Z"
            },
            "impact": {
                "local_total_tokens": 18446,
                "local_estimate_trusted_for_budget_enforcement": false
            },
            "signals": {
                "response_statuses": {
                    "completed": 1,
                    "failed": 0,
                    "incomplete": 0,
                    "unknown": 0
                },
                "context_fill": {"max_percent": 9.2},
                "model_mismatches": [],
                "accounting_anomaly_count": 0
            },
            "diagnosis": {"primary_cause_type": "none"}
        });
        let decision = decide(&status_report_to_facts(&report));

        assert_eq!(decision.state, DecisionState::Ended);
        assert_eq!(decision.primary_reason, "1 turns, 18K tokens");
    }

    #[test]
    fn guard_postmortem_report_maps_policy_block_to_shared_decision_json() {
        let report = serde_json::json!({
            "session_id": "session_guard",
            "partial": false,
            "summary": {"turn_count": 3},
            "impact": {
                "local_total_tokens": 125000,
                "local_estimated_cost_dollars": 2.50,
                "local_estimate_trusted_for_budget_enforcement": true
            },
            "signals": {
                "response_statuses": {
                    "completed": 3,
                    "failed": 0,
                    "incomplete": 0,
                    "unknown": 0
                },
                "context_fill": {"max_percent": 31.0},
                "model_mismatches": [],
                "accounting_anomaly_count": 0
            },
            "diagnosis": {"primary_cause_type": null}
        });
        let decision = guard_report_to_decision(
            &report,
            &codex_blackbox_core::guard_policy::GuardPolicy {
                session_token_budget: Some(120_000),
                session_cost_budget_dollars: None,
                ..Default::default()
            },
            Vec::new(),
            None,
        );
        let rendered = render_decision_footer_json(&decision).expect("guard json");

        assert_no_ansi(&rendered);
        let value: serde_json::Value = serde_json::from_str(&rendered).expect("guard decision");
        assert_eq!(value.get("state").and_then(|v| v.as_str()), Some("blocked"));
        assert_eq!(
            value.pointer("/policy_block/rule").and_then(|v| v.as_str()),
            Some("session_token_budget")
        );
        assert_eq!(
            value
                .pointer("/policy_block/current")
                .and_then(|v| v.as_str()),
            Some("125000 tokens")
        );
        assert_eq!(
            value
                .pointer("/policy_block/session_id")
                .and_then(|v| v.as_str()),
            Some("session_guard")
        );
    }

    #[test]
    fn guard_postmortem_report_maps_codex_native_policy_block_to_decision_json() {
        let report = serde_json::json!({
            "session_id": "session_guard",
            "partial": false,
            "summary": {"turn_count": 3},
            "impact": {
                "local_total_tokens": 125000,
                "local_estimated_cost_dollars": 2.50,
                "local_estimate_trusted_for_budget_enforcement": false
            },
            "signals": {
                "response_statuses": {
                    "completed": 3,
                    "failed": 0,
                    "incomplete": 0,
                    "unknown": 0
                },
                "context_fill": {"max_percent": 90.0},
                "model_mismatches": [],
                "accounting_anomaly_count": 0
            },
            "diagnosis": {"primary_cause_type": null}
        });
        let decision = guard_report_to_decision(
            &report,
            &codex_blackbox_core::guard_policy::GuardPolicy {
                session_cost_budget_dollars: Some(1.00),
                context_block_percent: Some(85.0),
                ..Default::default()
            },
            Vec::new(),
            None,
        );
        let rendered = render_decision_footer_json(&decision).expect("guard json");

        assert_no_ansi(&rendered);
        let value: serde_json::Value = serde_json::from_str(&rendered).expect("guard decision");
        assert_eq!(value.get("state").and_then(|v| v.as_str()), Some("blocked"));
        assert_eq!(
            value.pointer("/policy_block/rule").and_then(|v| v.as_str()),
            Some("context_block_percent")
        );
        assert_eq!(
            value
                .pointer("/policy_block/current")
                .and_then(|v| v.as_str()),
            Some("90.0%")
        );
        assert_eq!(
            value
                .pointer("/policy_block/limit")
                .and_then(|v| v.as_str()),
            Some("85.0%")
        );
    }

    #[test]
    fn guard_policy_load_failure_maps_to_advisory_decision() {
        let report = serde_json::json!({
            "session_id": "session_guard",
            "partial": false,
            "summary": {"turn_count": 1},
            "impact": {
                "local_total_tokens": 1000,
                "local_estimated_cost_dollars": 0.01,
                "local_estimate_trusted_for_budget_enforcement": true
            },
            "signals": {"response_statuses": {"completed": 1}},
            "diagnosis": {}
        });
        let decision = guard_report_to_decision(
            &report,
            &codex_blackbox_core::guard_policy::GuardPolicy::default(),
            vec![codex_blackbox_core::guard_policy::GuardPolicyIssue {
                issue_type: "policy_load_failed".to_string(),
                message: "failed to load guard policy /tmp/policy.toml".to_string(),
                recovery_action: "fix policy or continue unguarded".to_string(),
            }],
            None,
        );

        assert_eq!(decision.state, DecisionState::Careful);
        assert_eq!(decision.primary_reason, "guard policy issue");
        assert!(decision.policy_block.is_none());
        assert!(decision
            .secondary_reasons
            .iter()
            .any(|reason| reason.contains("policy_load_failed")));
    }

    #[test]
    fn guard_state_cooldown_maps_to_shared_decision() {
        let report = serde_json::json!({
            "session_id": "session_guard",
            "partial": false,
            "summary": {"turn_count": 1},
            "impact": {
                "local_total_tokens": 1000,
                "local_estimated_cost_dollars": 0.01,
                "local_estimate_trusted_for_budget_enforcement": true
            },
            "signals": {"response_statuses": {"completed": 1}},
            "diagnosis": {}
        });
        let guard_state = serde_json::json!({
            "cooldown": {
                "reason": "upstream errors",
                "retry_after_seconds": 30
            }
        });

        let decision = guard_report_to_decision(
            &report,
            &codex_blackbox_core::guard_policy::GuardPolicy::default(),
            Vec::new(),
            Some(&guard_state),
        );

        assert_eq!(decision.state, DecisionState::Cooldown);
        assert_eq!(decision.primary_reason, "upstream errors");
        assert_eq!(decision.next_action, "wait before retry");
    }

    #[test]
    fn decision_tracker_waits_for_turn_evidence_after_session_start() {
        let mut tracker = super::DecisionSessionTracker::default();
        let decision = tracker
            .update(&WatchEvent::SessionStart {
                session_id: "session_tracking".to_string(),
                display_name: "api".to_string(),
                model: "gpt-5.5".to_string(),
                initial_prompt: None,
            })
            .expect("session start yields decision");

        assert_eq!(decision.state, DecisionState::Watching);
        assert_eq!(
            decision.primary_reason,
            "waiting for first observed Codex Responses turn"
        );

        let decision = tracker
            .update(&WatchEvent::CodexTurnSummary {
                session_id: "session_tracking".to_string(),
                status: "completed".to_string(),
                requested_model: "gpt-5.5".to_string(),
                served_model: Some("gpt-5.5".to_string()),
                input_tokens: 1000,
                cached_input_tokens: 500,
                uncached_input_tokens: 500,
                output_tokens: 100,
                reasoning_output_tokens: 20,
                total_tokens: 1100,
            })
            .expect("turn summary yields decision");

        assert_eq!(decision.state, DecisionState::Healthy);
    }

    #[test]
    fn decision_tracker_renders_global_cooldown_event() {
        let mut tracker = super::DecisionSessionTracker::default();
        let decision = tracker
            .update(&WatchEvent::Cooldown {
                reason: "upstream errors".to_string(),
                retry_after_seconds: Some(30),
            })
            .expect("cooldown decision");

        assert_eq!(decision.state, DecisionState::Cooldown);
        assert_eq!(decision.primary_reason, "upstream errors");
        assert_eq!(
            decision.drill_down_command.as_deref(),
            Some("codex-blackbox postmortem last")
        );
    }

    #[test]
    fn watch_runtime_suppresses_duplicate_ended_footer_decisions() {
        let mut state = WatchRuntimeState::new();
        let end = WatchEvent::SessionEnd {
            session_id: "session_done".to_string(),
            outcome: "Likely Completed".to_string(),
            total_tokens: 1100,
            total_turns: 1,
        };
        let ready = WatchEvent::PostmortemReady {
            session_id: "session_done".to_string(),
            total_turns: 1,
            total_tokens: 1100,
            reason: "session idle enough to review".to_string(),
            postmortem_command: "codex-blackbox postmortem session_done".to_string(),
        };

        let first = state.decisions.update(&end).expect("ended decision");
        assert!(state.remember_decision_if_changed(&end, &first));

        let second = state.decisions.update(&ready).expect("ready decision");
        assert_eq!(first, second);
        assert!(!state.remember_decision_if_changed(&ready, &second));
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
        assert_eq!(plan.observation_prompt_excerpt.as_deref(), Some("hello"));
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
        assert_eq!(plan.observation_prompt_excerpt, None);
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
    fn codex_exec_prompt_hint_extracts_prompt_after_options() {
        assert_eq!(
            codex_exec_prompt_hint(&[
                "exec".to_string(),
                "--sandbox".to_string(),
                "read-only".to_string(),
                "-m".to_string(),
                "gpt-5.5".to_string(),
                "Read README.md\n\nSummarize it.".to_string()
            ])
            .as_deref(),
            Some("Read README.md Summarize it.")
        );
        assert_eq!(
            codex_exec_prompt_hint(&["e".to_string(), "resume".to_string(), "--last".to_string()]),
            None
        );
        assert_eq!(
            codex_exec_prompt_hint(&["exec".to_string(), "-".to_string()]),
            None
        );
    }

    #[test]
    fn codex_session_id_line_parser_accepts_only_uuid_shape() {
        assert_eq!(
            parse_codex_session_id_line("session id: 019e32db-c479-7562-a331-6dcbd248b780")
                .as_deref(),
            Some("019e32db-c479-7562-a331-6dcbd248b780")
        );
        assert_eq!(
            parse_codex_session_id_line("user: session id: 019e32db-c479-7562-a331-6dcbd248b780"),
            None
        );
        assert_eq!(parse_codex_session_id_line("session id: not-a-uuid"), None);
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
            "Post-run check: require Codex Blackbox to observe run-scoped Codex Responses evidence"
        ));
        assert!(preview.contains("http://127.0.0.1:10000/backend-api"));
        assert!(preview.contains("http://127.0.0.1:10000/backend-api/codex"));
        assert!(preview.contains("codex exec -c"));
        assert!(preview.contains("hello"));
    }

    #[test]
    fn observation_gate_fails_successful_codex_run_without_new_codex_responses_request() {
        let err = enforce_codex_observation(false, CodexObservationScope::ProcessStart)
            .expect_err("missing observation fails");

        assert!(err.contains("Codex exited successfully"));
        assert!(err.contains("provider=\"codex_responses\""));
        assert!(err.contains("after this child process started"));

        let prompt_err = enforce_codex_observation(false, CodexObservationScope::Prompt)
            .expect_err("missing prompt match fails");
        assert!(prompt_err.contains("matching this codex exec prompt"));

        let session_err = enforce_codex_observation(false, CodexObservationScope::SessionId)
            .expect_err("missing session match fails");
        assert!(session_err.contains("matching the child Codex session id"));
        assert!(enforce_codex_observation(true, CodexObservationScope::SessionId).is_ok());
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
            postmortem,
            no_redact,
            color,
            session,
            tmux,
            tmux_max_panes,
        } = cli.command
        else {
            panic!("expected watch command");
        };
        assert_eq!(url, "http://localhost:9091");
        assert!(!no_signals);
        assert!(!postmortem);
        assert!(!no_redact);
        assert_eq!(color, ColorMode::Auto);
        assert_eq!(session, None);
        assert!(!tmux);
        assert_eq!(tmux_max_panes, 8);

        let cli = Cli::try_parse_from(["codex-blackbox", "status"]).expect("status parses");
        let Commands::Status {
            url,
            json,
            color,
            width,
            target,
        } = cli.command
        else {
            panic!("expected status command");
        };
        assert_eq!(url, "http://localhost:9091");
        assert!(!json);
        assert_eq!(color, ColorMode::Auto);
        assert_eq!(width, None);
        assert_eq!(target, "last");

        let cli = Cli::try_parse_from(["codex-blackbox", "guard"]).expect("guard parses");
        let Commands::Guard {
            url,
            policy,
            json,
            color,
            width,
            target,
        } = cli.command
        else {
            panic!("expected guard command");
        };
        assert_eq!(url, "http://localhost:9091");
        assert_eq!(policy, None);
        assert!(!json);
        assert_eq!(color, ColorMode::Auto);
        assert_eq!(width, None);
        assert_eq!(target, "last");

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
        let filter = Some("session_target".to_string());
        let options = WatchRenderOptions {
            base_url: url.clone(),
            no_signals: false,
            session_filter: filter,
            postmortem: false,
            redact_postmortem: true,
            color_mode: ColorMode::Never,
        };
        let mut state = WatchRuntimeState::new();

        super::connect_and_stream(&url, &options, &mut state)
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
            state.active.sessions.is_empty(),
            "target session should be removed after session_end"
        );
        assert!(
            !state.decisions.facts.contains_key("session_other"),
            "filtered-out sessions must not affect decision state"
        );
        assert!(
            state.decisions.facts.contains_key("session_target"),
            "target session should update decision state"
        );
    }

    #[tokio::test]
    async fn watch_stream_ignores_postmortem_ready_without_opt_in() {
        let chunks = vec![concat!(
            "data: {\"type\":\"postmortem_ready\",\"session_id\":\"session_ready\",",
            "\"total_turns\":1,\"total_tokens\":1100,",
            "\"reason\":\"session idle enough to review\",",
            "\"postmortem_command\":\"codex-blackbox postmortem session_ready\"}\n\n"
        )
        .to_string()];
        let (url, _request_rx) = serve_sse_chunks_once(chunks);
        let options = WatchRenderOptions {
            base_url: "http://127.0.0.1:1".to_string(),
            no_signals: false,
            session_filter: None,
            postmortem: false,
            redact_postmortem: true,
            color_mode: ColorMode::Never,
        };
        let mut state = WatchRuntimeState::new();

        super::connect_and_stream(&url, &options, &mut state)
            .await
            .expect("watch stream closes cleanly");

        assert!(state.rendered_postmortems.is_empty());
    }

    #[tokio::test]
    async fn watch_postmortem_respects_session_filter_before_fetching() {
        let chunks = vec![concat!(
            "data: {\"type\":\"postmortem_ready\",\"session_id\":\"session_other\",",
            "\"total_turns\":1,\"total_tokens\":1100,",
            "\"reason\":\"session idle enough to review\",",
            "\"postmortem_command\":\"codex-blackbox postmortem session_other\"}\n\n"
        )
        .to_string()];
        let (url, _request_rx) = serve_sse_chunks_once(chunks);
        let options = WatchRenderOptions {
            base_url: "http://127.0.0.1:1".to_string(),
            no_signals: false,
            session_filter: Some("session_target".to_string()),
            postmortem: true,
            redact_postmortem: true,
            color_mode: ColorMode::Never,
        };
        let mut state = WatchRuntimeState::new();

        super::connect_and_stream(&url, &options, &mut state)
            .await
            .expect("filtered postmortem event should not fetch");

        assert!(state.rendered_postmortems.is_empty());
        assert!(!state.decisions.facts.contains_key("session_other"));
    }

    #[tokio::test]
    async fn watch_postmortem_fetches_redacted_report_once_when_opted_in() {
        let chunks = vec![concat!(
            "data: {\"type\":\"postmortem_ready\",\"session_id\":\"session_ready\",",
            "\"total_turns\":1,\"total_tokens\":1100,",
            "\"reason\":\"session idle enough to review\",",
            "\"postmortem_command\":\"codex-blackbox postmortem session_ready\"}\n\n",
            "data: {\"type\":\"postmortem_ready\",\"session_id\":\"session_ready\",",
            "\"total_turns\":1,\"total_tokens\":1100,",
            "\"reason\":\"session idle enough to review\",",
            "\"postmortem_command\":\"codex-blackbox postmortem session_ready\"}\n\n"
        )
        .to_string()];
        let (watch_url, _watch_request_rx) = serve_sse_chunks_once(chunks);
        let (api_url, api_request_rx) = serve_json_once(
            r#"{
              "schema_version": 1,
              "session_id": "session_ready",
              "redacted": true,
              "partial": false,
              "summary": {"outcome": "Likely Completed", "turn_count": 1},
              "diagnosis": {"primary_cause": "none"},
              "impact": {
                "input_tokens": 1000,
                "cached_input_tokens": 500,
                "uncached_input_tokens": 500,
                "output_tokens": 100,
                "reasoning_output_tokens": 20,
                "local_total_tokens": 1100,
                "local_estimated_cost_dollars": 0.0
              },
              "signals": {"response_statuses": {"completed": 1}},
              "evidence": [],
              "timeline": [],
              "recommendations": [],
              "caveats": ["Evidence is limited to local Envoy-observed Codex Responses traffic."]
            }"#,
        );
        let options = WatchRenderOptions {
            base_url: api_url,
            no_signals: false,
            session_filter: None,
            postmortem: true,
            redact_postmortem: true,
            color_mode: ColorMode::Never,
        };
        let mut state = WatchRuntimeState::new();

        super::connect_and_stream(&watch_url, &options, &mut state)
            .await
            .expect("watch stream closes cleanly");

        let request = api_request_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("captured postmortem request");
        assert!(
            request.starts_with("GET /api/postmortem/session_ready?redact=true "),
            "unexpected postmortem request:\n{request}"
        );
        assert_eq!(state.rendered_postmortems.len(), 1);
        assert!(state.rendered_postmortems.contains("session_ready"));
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
        assert_eq!(
            event_session_id(&WatchEvent::PostmortemReady {
                session_id: "session_ready".to_string(),
                total_turns: 1,
                total_tokens: 1100,
                reason: "session idle enough to review".to_string(),
                postmortem_command: "codex-blackbox postmortem session_ready".to_string(),
            }),
            Some("session_ready")
        );

        assert_eq!(event_session_id(&WatchEvent::Lagged { missed: 3 }), None);
        assert_eq!(
            event_session_id(&WatchEvent::Cooldown {
                reason: "upstream errors".to_string(),
                retry_after_seconds: Some(30),
            }),
            None
        );
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
