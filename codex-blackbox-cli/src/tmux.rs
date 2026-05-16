use std::collections::HashMap;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use colored::Colorize;
use futures_util::StreamExt;

use crate::{event_session_id, WatchEvent, WatchRetryLog, WATCH_RECONNECT_DELAY};

// ---------------------------------------------------------------------------
// Tmux environment checks
// ---------------------------------------------------------------------------

/// Verify tmux is installed. Caller is responsible for ensuring we're inside
/// a tmux session (via `bootstrap_into_tmux` or otherwise).
pub fn check_tmux_installed() -> Result<(), String> {
    let status = Command::new("tmux")
        .arg("-V")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    match status {
        Ok(s) if s.success() => Ok(()),
        _ => Err("tmux not found. Install tmux first (brew install tmux).".into()),
    }
}

/// If we're not already inside a tmux session, replace this process with a new
/// tmux session that re-runs the same CLI with `--tmux` — so the user just
/// runs `codex-blackbox watch --tmux` and lands directly in the orchestrator.
///
/// Returns Ok(()) when we were already inside tmux (caller continues).
/// On successful exec, this function does not return.
pub fn bootstrap_into_tmux(
    url: &str,
    no_signals: bool,
    tmux_max_panes: usize,
    postmortem: bool,
    no_redact: bool,
) -> Result<(), String> {
    if std::env::var("TMUX").is_ok() {
        return Ok(());
    }
    check_tmux_installed()?;

    let cli =
        std::env::current_exe().map_err(|e| format!("cannot locate current executable: {}", e))?;
    let cli_str = cli
        .to_str()
        .ok_or_else(|| "executable path is not UTF-8".to_string())?
        .to_string();

    // Use a unique session name so parallel invocations don't collide.
    let session_name = format!("codex-blackbox-{}", std::process::id());
    let panes_str = tmux_max_panes.to_string();

    let mut args: Vec<String> = vec![
        "new-session".into(),
        "-s".into(),
        session_name,
        cli_str,
        "watch".into(),
        "--tmux".into(),
        "--url".into(),
        url.to_string(),
        "--tmux-max-panes".into(),
        panes_str,
    ];
    if no_signals {
        args.push("--no-signals".into());
    }
    if postmortem {
        args.push("--postmortem".into());
    }
    if no_redact {
        args.push("--no-redact".into());
    }

    use std::os::unix::process::CommandExt;
    let err = Command::new("tmux").args(&args).exec();
    // exec only returns on failure.
    Err(format!("failed to exec tmux: {}", err))
}

fn get_own_pane_id() -> Result<String, String> {
    let output = Command::new("tmux")
        .args(["display-message", "-p", "#{pane_id}"])
        .output()
        .map_err(|e| format!("Failed to get pane id: {}", e))?;
    if !output.status.success() {
        return Err("tmux display-message failed".into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn get_own_window_id() -> Result<String, String> {
    let output = Command::new("tmux")
        .args(["display-message", "-p", "#{window_id}"])
        .output()
        .map_err(|e| format!("Failed to get window id: {}", e))?;
    if !output.status.success() {
        return Err("tmux display-message failed".into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn run_tmux(args: &[&str]) -> Result<(), String> {
    let output = Command::new("tmux")
        .args(args)
        .output()
        .map_err(|e| format!("tmux command failed: {}", e))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !stderr.is_empty() {
        return Err(stderr);
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !stdout.is_empty() {
        return Err(stdout);
    }
    Err("unknown tmux error".into())
}

fn resolve_cli_path() -> String {
    std::env::current_exe()
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "codex-blackbox".into())
}

// ---------------------------------------------------------------------------
// Managed pane state
// ---------------------------------------------------------------------------

struct ManagedPane {
    pane_id: String,
    #[allow(dead_code)]
    session_id: String,
    display_name: String,
    model: String,
    /// Observed tool-use events seen by this watcher for the session.
    observed_tool_calls: u32,
    ended: bool,
    /// Time of the most recent event attributed to this session. Used to
    /// color the orchestrator row green (recent), yellow (warming down), or
    /// red (gone idle).
    last_activity: Instant,
    /// Latest context-fill snapshot — drives the compaction runway hint.
    fill_percent: Option<f64>,
    context_window_tokens: Option<u64>,
    turns_to_compact: Option<u32>,
    /// If the served model differed from the requested model.
    model_fallback: Option<(String, String)>,
    /// Latest Codex-native turn accounting summary.
    codex_turn: Option<CodexPaneTurnSummary>,
    applied_activity: Option<Activity>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CodexPaneTurnSummary {
    status: String,
    cached_input_tokens: u64,
    uncached_input_tokens: u64,
    output_tokens: u64,
    reasoning_output_tokens: u64,
    total_tokens: u64,
}

/// Activity level derived from `last_activity` elapsed time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Activity {
    Active,
    Warm,
    Idle,
    Ended,
}

/// Compact human formatting: 12345 → "12K", 3_400_000 → "3.4M".
fn format_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{}K", n / 1_000)
    } else {
        n.to_string()
    }
}

fn format_observed_tool_calls(n: u32) -> String {
    match n {
        0 => "no tool calls seen".to_string(),
        1 => "1 tool call seen".to_string(),
        _ => format!("{n} tool calls seen"),
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

fn pane_model_label(model: &str) -> String {
    if is_codex_model_name(model) {
        format!("CODEX \u{00b7} {model}")
    } else {
        model.to_string()
    }
}

fn model_change_hint_label(requested: &str, actual: &str) -> &'static str {
    let _ = (requested, actual);
    "model change"
}

fn context_window_label(context_window_tokens: Option<u64>) -> String {
    context_window_tokens
        .map(|tokens| format!(" of {} window", format_count(tokens)))
        .unwrap_or_default()
}

fn context_hint_text(
    indent: &str,
    fill_percent: f64,
    context_window_tokens: Option<u64>,
    turns_to_compact: Option<u32>,
) -> Option<String> {
    if fill_percent < 60.0 {
        return None;
    }
    let tail = match turns_to_compact {
        Some(0) => "at compaction threshold".to_string(),
        Some(n) => format!("~{} turns to compaction", n),
        None => "trajectory unknown".to_string(),
    };
    Some(format!(
        "{}context {:.0}%{} \u{00b7} {}",
        indent,
        fill_percent,
        context_window_label(context_window_tokens),
        tail
    ))
}

fn codex_turn_hint_text(indent: &str, summary: &CodexPaneTurnSummary) -> String {
    let reasoning_part = if summary.reasoning_output_tokens > 0 {
        format!(
            " \u{00b7} reasoning {}",
            format_count(summary.reasoning_output_tokens)
        )
    } else {
        String::new()
    };
    format!(
        "{}codex {} \u{00b7} cached input {} \u{00b7} uncached input {} \u{00b7} output {}{} \u{00b7} total {}",
        indent,
        summary.status,
        format_count(summary.cached_input_tokens),
        format_count(summary.uncached_input_tokens),
        format_count(summary.output_tokens),
        reasoning_part,
        format_count(summary.total_tokens)
    )
}

/// Coarse human duration: "45s" / "12m" / "3h 20m" / "5d 14h".
#[cfg(test)]
fn format_duration(secs: u64) -> String {
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

fn build_child_watch_command(
    cli_path: &str,
    session_id: &str,
    watch_url: &str,
    no_signals: bool,
    postmortem: bool,
    no_redact: bool,
) -> String {
    let mut cmd_parts = vec![
        cli_path.to_string(),
        "watch".to_string(),
        "--session".to_string(),
        session_id.to_string(),
        "--url".to_string(),
        watch_url.to_string(),
    ];
    if no_signals {
        cmd_parts.push("--no-signals".to_string());
    }
    if postmortem {
        cmd_parts.push("--postmortem".to_string());
    }
    if no_redact {
        cmd_parts.push("--no-redact".to_string());
    }
    shell_join(&cmd_parts)
}

impl ManagedPane {
    fn activity(&self) -> Activity {
        if self.ended {
            return Activity::Ended;
        }
        let idle = self.last_activity.elapsed();
        if idle < Duration::from_secs(10) {
            Activity::Active
        } else if idle < Duration::from_secs(30) {
            Activity::Warm
        } else {
            Activity::Idle
        }
    }
}

// ---------------------------------------------------------------------------
// TmuxOrchestrator
// ---------------------------------------------------------------------------

pub struct TmuxOrchestrator {
    panes: HashMap<String, ManagedPane>,
    own_pane_id: String,
    own_window_id: String,
    first_session_pane_id: Option<String>,
    watch_url: String,
    cli_path: String,
    no_signals: bool,
    postmortem: bool,
    no_redact: bool,
    max_panes: usize,
}

impl TmuxOrchestrator {
    pub fn new(
        watch_url: String,
        no_signals: bool,
        max_panes: usize,
        postmortem: bool,
        no_redact: bool,
    ) -> Result<Self, String> {
        let own_pane_id = get_own_pane_id()?;
        let own_window_id = get_own_window_id()?;
        let cli_path = resolve_cli_path();
        let orchestrator = Self {
            panes: HashMap::new(),
            own_pane_id,
            own_window_id,
            first_session_pane_id: None,
            watch_url,
            cli_path,
            no_signals,
            postmortem,
            no_redact,
            max_panes,
        };
        orchestrator.configure_pane_borders();
        Ok(orchestrator)
    }

    /// Create a tmux pane for a session. Returns the new pane_id.
    fn create_pane(
        &mut self,
        session_id: &str,
        display_name: &str,
        model: &str,
    ) -> Result<String, String> {
        // Build child command.
        let child_cmd = build_child_watch_command(
            &self.cli_path,
            session_id,
            &self.watch_url,
            self.no_signals,
            self.postmortem,
            self.no_redact,
        );

        // Determine split strategy.
        // Always target a specific pane so the split happens in the orchestrator's window.
        let mut args: Vec<String> = vec!["split-window".into()];
        if self.first_session_pane_id.is_none() {
            // First session pane: split the orchestrator pane vertically,
            // new pane above, give it 85%.
            args.extend_from_slice(&[
                "-v".into(),
                "-b".into(),
                "-l".into(),
                "85%".into(),
                "-t".into(),
                self.own_pane_id.clone(),
            ]);
        } else {
            // Subsequent panes: split the first session pane horizontally.
            args.extend_from_slice(&[
                "-h".into(),
                "-t".into(),
                self.first_session_pane_id.clone().unwrap(),
            ]);
        }
        // Don't steal focus, print new pane id.
        args.extend_from_slice(&[
            "-d".into(),
            "-P".into(),
            "-F".into(),
            "#{pane_id}".into(),
            child_cmd,
        ]);

        let output = Command::new("tmux")
            .args(&args)
            .output()
            .map_err(|e| format!("tmux split-window failed: {}", e))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("tmux split-window error: {}", stderr.trim()));
        }

        let pane_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if pane_id.is_empty() {
            return Err("tmux split-window returned empty pane id".into());
        }

        // Track the first session pane for subsequent splits.
        if self.first_session_pane_id.is_none() {
            self.first_session_pane_id = Some(pane_id.clone());
        }

        // Set pane title.
        let _ = Command::new("tmux")
            .args(["select-pane", "-t", &pane_id, "-T", display_name])
            .output();

        // Rebalance layout: tiled for session panes, then shrink orchestrator.
        self.rebalance_layout();

        // Store the managed pane.
        self.panes.insert(
            session_id.to_string(),
            ManagedPane {
                pane_id: pane_id.clone(),
                session_id: session_id.to_string(),
                display_name: display_name.to_string(),
                model: model.to_string(),
                observed_tool_calls: 0,
                ended: false,
                last_activity: Instant::now(),
                fill_percent: None,
                context_window_tokens: None,
                turns_to_compact: None,
                model_fallback: None,
                codex_turn: None,
                applied_activity: None,
            },
        );

        Ok(pane_id)
    }

    fn configure_pane_borders(&self) {
        if let Err(err) = run_tmux(&[
            "set-window-option",
            "-t",
            &self.own_window_id,
            "pane-border-status",
            "top",
        ]) {
            eprintln!(
                "{}",
                format!("tmux pane-border-status failed: {}", err).dimmed()
            );
        }
        if let Err(err) = run_tmux(&[
            "set-window-option",
            "-t",
            &self.own_window_id,
            "pane-border-format",
            "#{?@codex_blackbox_active,●,○} #{pane_title}",
        ]) {
            eprintln!(
                "{}",
                format!("tmux pane-border-format failed: {}", err).dimmed()
            );
        }
    }

    fn apply_pane_activity_style(pane_id: &str, activity: Activity) {
        let (active_flag, activity_label, border_style) = match activity {
            Activity::Active => ("1", "active", "fg=green,bold"),
            Activity::Warm => ("1", "warm", "fg=yellow"),
            Activity::Idle => ("0", "idle", "fg=red"),
            Activity::Ended => ("0", "ended", "fg=colour244"),
        };
        for (label, args) in [
            (
                "@codex_blackbox_active",
                vec![
                    "set-option",
                    "-p",
                    "-t",
                    pane_id,
                    "@codex_blackbox_active",
                    active_flag,
                ],
            ),
            (
                "@codex_blackbox_activity",
                vec![
                    "set-option",
                    "-p",
                    "-t",
                    pane_id,
                    "@codex_blackbox_activity",
                    activity_label,
                ],
            ),
            (
                "pane-border-style",
                vec![
                    "set-option",
                    "-p",
                    "-t",
                    pane_id,
                    "pane-border-style",
                    border_style,
                ],
            ),
            (
                "pane-active-border-style",
                vec![
                    "set-option",
                    "-p",
                    "-t",
                    pane_id,
                    "pane-active-border-style",
                    border_style,
                ],
            ),
        ] {
            if let Err(err) = run_tmux(&args) {
                eprintln!(
                    "{}",
                    format!("tmux {} update failed for {}: {}", label, pane_id, err).dimmed()
                );
            }
        }
    }

    fn rebalance_layout(&self) {
        // Target the orchestrator's pane so layout applies to its window.
        let _ = Command::new("tmux")
            .args(["select-layout", "-t", &self.own_pane_id, "tiled"])
            .output();
        let _ = Command::new("tmux")
            .args(["resize-pane", "-t", &self.own_pane_id, "-y", "7"])
            .output();
    }

    fn render_status(&mut self) {
        // Clear screen and move cursor to top-left.
        print!("\x1b[2J\x1b[H");

        println!("{}", "codex-blackbox watch (tmux mode)".bold());

        println!("{}", "\u{2500}".repeat(42).dimmed());

        if self.panes.is_empty() {
            println!("{}", "Waiting for sessions...".dimmed());
        } else {
            // Pad columns for alignment.
            let max_name = self
                .panes
                .values()
                .map(|p| p.display_name.len())
                .max()
                .unwrap_or(0)
                .max(8);
            let max_model = self
                .panes
                .values()
                .map(|p| pane_model_label(&p.model).len())
                .max()
                .unwrap_or(0)
                .max(5);

            for pane in self.panes.values_mut() {
                let activity = pane.activity();
                if pane.applied_activity != Some(activity) {
                    Self::apply_pane_activity_style(&pane.pane_id, activity);
                    pane.applied_activity = Some(activity);
                }
                let dot = match activity {
                    Activity::Active => "\u{25cf}".green(),
                    Activity::Warm => "\u{25cf}".yellow(),
                    Activity::Idle => "\u{25cf}".red(),
                    Activity::Ended => "\u{25cf}".dimmed(),
                };
                let status = match activity {
                    Activity::Ended => {
                        if let Some(summary) = &pane.codex_turn {
                            format!("ended \u{00b7} codex {}", summary.status)
                                .dimmed()
                                .to_string()
                        } else {
                            "ended".dimmed().to_string()
                        }
                    }
                    Activity::Active => format!(
                        "{} · active",
                        format_observed_tool_calls(pane.observed_tool_calls)
                    )
                    .green()
                    .to_string(),
                    Activity::Warm => format_observed_tool_calls(pane.observed_tool_calls)
                        .yellow()
                        .to_string(),
                    Activity::Idle => format!(
                        "{} · idle {}s",
                        format_observed_tool_calls(pane.observed_tool_calls),
                        pane.last_activity.elapsed().as_secs()
                    )
                    .red()
                    .to_string(),
                };
                let name_colored = match activity {
                    Activity::Active => pane.display_name.green().to_string(),
                    Activity::Warm => pane.display_name.yellow().to_string(),
                    Activity::Idle => pane.display_name.red().to_string(),
                    Activity::Ended => pane.display_name.dimmed().to_string(),
                };
                // Pad raw display name so columns align even after color codes;
                // colored::ColoredString doesn't count width for us.
                let name_padding = max_name.saturating_sub(pane.display_name.len());
                let model_label = pane_model_label(&pane.model);
                println!(
                    "  {} {}{}  {:<width_m$}  {}",
                    dot,
                    name_colored,
                    " ".repeat(name_padding),
                    model_label,
                    status,
                    width_m = max_model,
                );
                // Sub-row: Codex turn, context, fallback hints. Indent under
                // the pane row so the eye groups them. Only render hints that
                // are active / actionable.
                let name_indent = " ".repeat(4 + max_name);
                if let Some(summary) = &pane.codex_turn {
                    let msg = codex_turn_hint_text(&name_indent, summary);
                    let colored = match summary.status.as_str() {
                        "completed" => msg.green().dimmed().to_string(),
                        "failed" => msg.red().bold().to_string(),
                        "incomplete" => msg.yellow().bold().to_string(),
                        _ => msg.yellow().to_string(),
                    };
                    println!("{}", colored);
                }
                if let Some(fill) = pane.fill_percent {
                    if let Some(msg) = context_hint_text(
                        &name_indent,
                        fill,
                        pane.context_window_tokens,
                        pane.turns_to_compact,
                    ) {
                        if !pane.ended {
                            let colored = if fill >= 80.0 {
                                msg.red().bold().to_string()
                            } else {
                                msg.yellow().to_string()
                            };
                            println!("{}", colored);
                        }
                    }
                }
                if let Some((req, actual)) = &pane.model_fallback {
                    if !pane.ended {
                        println!(
                            "{}",
                            format!(
                                "{}\u{26a0}  {}: requested {}, served {}",
                                name_indent,
                                model_change_hint_label(req, actual),
                                req,
                                actual
                            )
                            .yellow()
                            .bold()
                        );
                    }
                }
            }
        }

        println!();
        println!("{}", "Ctrl+C to stop all panes".dimmed());
    }

    #[allow(dead_code)]
    fn cleanup(&self) {
        for pane in self.panes.values() {
            let _ = Command::new("tmux")
                .args(["kill-pane", "-t", &pane.pane_id])
                .output();
        }
    }

    /// Lazy discovery: create a pane for a session we haven't seen a SessionStart for.
    fn ensure_pane_exists(&mut self, session_id: &str, cleanup_pane_ids: &Arc<Mutex<Vec<String>>>) {
        if self.panes.contains_key(session_id) {
            return;
        }
        if self.panes.len() >= self.max_panes {
            return;
        }
        // Derive a short name from the session_id.
        let short_name = if session_id.len() > 12 {
            &session_id[8..12.min(session_id.len())]
        } else {
            session_id
        };
        match self.create_pane(session_id, short_name, "?") {
            Ok(pane_id) => {
                if let Ok(mut ids) = cleanup_pane_ids.lock() {
                    ids.push(pane_id);
                }
            }
            Err(e) => {
                eprintln!("{}", format!("Failed to create pane: {}", e).dimmed());
            }
        }
    }

    fn bump_activity(&mut self, session_id: &str) {
        if let Some(pane) = self.panes.get_mut(session_id) {
            if !pane.ended {
                pane.last_activity = Instant::now();
            }
        }
    }

    fn handle_event(&mut self, event: &WatchEvent, cleanup_pane_ids: &Arc<Mutex<Vec<String>>>) {
        // Touch activity timestamp on any session-scoped event so the status
        // coloring reflects real-time pulse, not only ToolUse.
        if let Some(sid) = event_session_id(event) {
            self.bump_activity(sid);
        }
        match event {
            WatchEvent::SessionStart {
                session_id,
                display_name,
                model,
                initial_prompt: _,
            } => {
                if let Some(pane) = self.panes.get_mut(session_id.as_str()) {
                    // Pane was lazy-discovered from an earlier ToolUse with a
                    // placeholder name/model. Promote it now that we have the
                    // real values. Rename the tmux pane title too so
                    // pane-border-status displays the right label.
                    let pane_id = pane.pane_id.clone();
                    pane.display_name = display_name.clone();
                    pane.model = model.clone();
                    let _ = Command::new("tmux")
                        .args(["select-pane", "-t", &pane_id, "-T", display_name])
                        .output();
                } else if self.panes.len() < self.max_panes {
                    match self.create_pane(session_id, display_name, model) {
                        Ok(pane_id) => {
                            if let Ok(mut ids) = cleanup_pane_ids.lock() {
                                ids.push(pane_id);
                            }
                        }
                        Err(e) => {
                            eprintln!(
                                "{}",
                                format!("Failed to create pane for {}: {}", display_name, e)
                                    .dimmed()
                            );
                        }
                    }
                }
                self.render_status();
            }

            WatchEvent::SessionEnd { session_id, .. } => {
                if let Some(pane) = self.panes.get_mut(session_id.as_str()) {
                    pane.ended = true;
                }
                self.render_status();
            }

            WatchEvent::ToolUse { session_id, .. } => {
                self.ensure_pane_exists(session_id, cleanup_pane_ids);
                if let Some(pane) = self.panes.get_mut(session_id.as_str()) {
                    pane.observed_tool_calls += 1;
                }
                self.render_status();
            }

            WatchEvent::ContextStatus {
                session_id,
                fill_percent,
                context_window_tokens,
                turns_to_compact,
            } => {
                self.ensure_pane_exists(session_id, cleanup_pane_ids);
                if let Some(pane) = self.panes.get_mut(session_id.as_str()) {
                    pane.fill_percent = Some(*fill_percent);
                    pane.context_window_tokens = *context_window_tokens;
                    pane.turns_to_compact = *turns_to_compact;
                }
                self.render_status();
            }

            WatchEvent::ModelFallback {
                session_id,
                requested,
                actual,
            } => {
                self.ensure_pane_exists(session_id, cleanup_pane_ids);
                if let Some(pane) = self.panes.get_mut(session_id.as_str()) {
                    pane.model_fallback = Some((requested.clone(), actual.clone()));
                }
                self.render_status();
            }

            WatchEvent::CodexTurnSummary {
                session_id,
                status,
                requested_model,
                served_model,
                cached_input_tokens,
                uncached_input_tokens,
                output_tokens,
                reasoning_output_tokens,
                total_tokens,
                ..
            } => {
                self.ensure_pane_exists(session_id, cleanup_pane_ids);
                if let Some(pane) = self.panes.get_mut(session_id.as_str()) {
                    pane.model = served_model.as_ref().unwrap_or(requested_model).to_string();
                    if served_model
                        .as_ref()
                        .is_some_and(|served| served != requested_model)
                    {
                        pane.model_fallback = Some((requested_model.clone(), pane.model.clone()));
                    }
                    pane.codex_turn = Some(CodexPaneTurnSummary {
                        status: status.clone(),
                        cached_input_tokens: *cached_input_tokens,
                        uncached_input_tokens: *uncached_input_tokens,
                        output_tokens: *output_tokens,
                        reasoning_output_tokens: *reasoning_output_tokens,
                        total_tokens: *total_tokens,
                    });
                }
                self.render_status();
            }

            WatchEvent::Lagged { .. } => {}

            _ => {
                // For other event types, ensure the session has a pane (lazy discovery)
                // but don't re-render status on every low-signal event.
                if let Some(sid) = event_session_id(event) {
                    self.ensure_pane_exists(sid, cleanup_pane_ids);
                }
            }
        }
    }

    async fn connect_and_process(
        &mut self,
        url: &str,
        cleanup_pane_ids: &Arc<Mutex<Vec<String>>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let resp = reqwest::Client::new()
            .get(url)
            .header("Accept", "text/event-stream")
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status()).into());
        }

        let mut stream = resp.bytes_stream();
        let mut line_buffer = String::new();
        let mut data_buffer = String::new();

        // Tick so the orchestrator re-renders even when the SSE stream is
        // quiet — otherwise the activity-colored dots stay green after a
        // session goes idle because no event triggers a refresh.
        let mut ticker = tokio::time::interval(Duration::from_secs(2));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                maybe_chunk = stream.next() => {
                    let chunk = match maybe_chunk {
                        Some(Ok(c)) => c,
                        Some(Err(e)) => return Err(e.into()),
                        None => break,
                    };
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
                                self.handle_event(&event, cleanup_pane_ids);
                            }
                            data_buffer.clear();
                        }
                    }
                }
                _ = ticker.tick() => {
                    // Just re-render; pane state already has timestamps.
                    self.render_status();
                }
            }
        }

        Ok(())
    }

    pub async fn run(mut self, watch_url: &str) -> Result<(), Box<dyn std::error::Error>> {
        self.render_status();

        // Shared list of pane IDs for the Ctrl+C handler to clean up.
        let cleanup_pane_ids: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let cleanup_ref = cleanup_pane_ids.clone();

        tokio::spawn(async move {
            tokio::signal::ctrl_c().await.ok();
            let pane_ids = cleanup_ref.lock().unwrap().clone();
            for pane_id in &pane_ids {
                let _ = Command::new("tmux")
                    .args(["kill-pane", "-t", pane_id])
                    .output();
            }
            eprintln!("\nCleaned up {} panes.", pane_ids.len());
            std::process::exit(0);
        });

        // Reconnect loop — same pattern as the regular watch mode.
        let mut retry_log = WatchRetryLog::default();
        loop {
            match self.connect_and_process(watch_url, &cleanup_pane_ids).await {
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
            self.render_status();
            tokio::time::sleep(WATCH_RECONNECT_DELAY).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        codex_turn_hint_text, context_hint_text, format_count, format_duration,
        format_observed_tool_calls, model_change_hint_label, pane_model_label, Activity,
        CodexPaneTurnSummary, ManagedPane,
    };
    use crate::WatchEvent;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    fn pane_with_last_activity(last_activity: Instant, ended: bool) -> ManagedPane {
        ManagedPane {
            pane_id: "%1".to_string(),
            session_id: "session_test".to_string(),
            display_name: "test".to_string(),
            model: "gpt-5.4".to_string(),
            observed_tool_calls: 0,
            ended,
            last_activity,
            fill_percent: None,
            context_window_tokens: None,
            turns_to_compact: None,
            model_fallback: None,
            codex_turn: None,
            applied_activity: None,
        }
    }

    fn active_pane() -> ManagedPane {
        let mut pane = pane_with_last_activity(Instant::now(), false);
        pane.applied_activity = Some(Activity::Active);
        pane
    }

    fn test_orchestrator(max_panes: usize) -> super::TmuxOrchestrator {
        super::TmuxOrchestrator {
            panes: HashMap::new(),
            own_pane_id: "%0".to_string(),
            own_window_id: "@0".to_string(),
            first_session_pane_id: None,
            watch_url: "http://localhost:9091".to_string(),
            cli_path: "codex-blackbox".to_string(),
            no_signals: false,
            postmortem: false,
            no_redact: false,
            max_panes,
        }
    }

    #[test]
    fn count_and_duration_formatting_are_compact() {
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(12_345), "12K");
        assert_eq!(format_count(3_400_000), "3.4M");

        assert_eq!(format_duration(45), "45s");
        assert_eq!(format_duration(12 * 60), "12m");
        assert_eq!(format_duration(3 * 60 * 60 + 20 * 60), "3h 20m");
        assert_eq!(format_duration(5 * 24 * 60 * 60 + 14 * 60 * 60), "5d 14h");
    }

    #[test]
    fn observed_tool_call_labels_match_counts() {
        assert_eq!(format_observed_tool_calls(0), "no tool calls seen");
        assert_eq!(format_observed_tool_calls(1), "1 tool call seen");
        assert_eq!(format_observed_tool_calls(2), "2 tool calls seen");
    }

    #[test]
    fn codex_pane_labels_do_not_use_legacy_cache_language() {
        assert_eq!(
            pane_model_label("gpt-codex-fixture"),
            "CODEX \u{00b7} gpt-codex-fixture"
        );
        assert_eq!(
            pane_model_label("unknown-model-fixture"),
            "unknown-model-fixture"
        );
        assert_eq!(
            model_change_hint_label("gpt-codex-fixture", "gpt-codex-served"),
            "model change"
        );

        let context =
            context_hint_text("    ", 75.0, Some(200_000), None).expect("context hint visible");
        assert_eq!(
            context,
            "    context 75% of 200K window \u{00b7} trajectory unknown"
        );

        let turn = codex_turn_hint_text(
            "    ",
            &CodexPaneTurnSummary {
                status: "completed".to_string(),
                cached_input_tokens: 512,
                uncached_input_tokens: 768,
                output_tokens: 96,
                reasoning_output_tokens: 32,
                total_tokens: 1_376,
            },
        );
        assert!(turn.contains("codex completed"));
        assert!(turn.contains("cached input 512"));
        assert!(turn.contains("reasoning 32"));
        assert!(!turn.contains("expires"));
        assert!(!turn.contains("rebuild"));
    }

    #[test]
    fn child_watch_command_preserves_session_url_and_visibility_flags() {
        assert_eq!(
            super::build_child_watch_command(
                "codex-blackbox",
                "session_a",
                "http://localhost:9091",
                false,
                false,
                false,
            ),
            "codex-blackbox watch --session session_a --url http://localhost:9091"
        );

        assert_eq!(
            super::build_child_watch_command(
                "/tmp/codex-blackbox cli",
                "session with spaces",
                "http://localhost:9091/watch?session=session with spaces",
                true,
                true,
                true,
            ),
            "'/tmp/codex-blackbox cli' watch --session 'session with spaces' --url 'http://localhost:9091/watch?session=session with spaces' --no-signals --postmortem --no-redact"
        );
    }

    #[test]
    fn pane_activity_tracks_idle_time_and_end_state() {
        assert_eq!(
            pane_with_last_activity(Instant::now(), false).activity(),
            Activity::Active
        );
        assert_eq!(
            pane_with_last_activity(Instant::now() - Duration::from_secs(15), false).activity(),
            Activity::Warm
        );
        assert_eq!(
            pane_with_last_activity(Instant::now() - Duration::from_secs(31), false).activity(),
            Activity::Idle
        );
        assert_eq!(
            pane_with_last_activity(Instant::now(), true).activity(),
            Activity::Ended
        );
    }

    #[test]
    fn ensure_pane_exists_respects_max_panes_before_shelling_out() {
        let mut orchestrator = test_orchestrator(0);
        let cleanup = Arc::new(Mutex::new(Vec::new()));

        orchestrator.ensure_pane_exists("session_new", &cleanup);

        assert!(orchestrator.panes.is_empty());
        assert!(cleanup.lock().expect("cleanup ids").is_empty());
    }

    #[test]
    fn session_start_respects_max_panes_before_shelling_out() {
        let mut orchestrator = test_orchestrator(0);
        let cleanup = Arc::new(Mutex::new(Vec::new()));

        orchestrator.handle_event(
            &WatchEvent::SessionStart {
                session_id: "session_new".to_string(),
                display_name: "api".to_string(),
                model: "gpt-5.4".to_string(),
                initial_prompt: Some("hello".to_string()),
            },
            &cleanup,
        );

        assert!(orchestrator.panes.is_empty());
        assert!(cleanup.lock().expect("cleanup ids").is_empty());
    }

    #[test]
    fn handle_event_updates_existing_pane_state() {
        let mut orchestrator = test_orchestrator(4);
        orchestrator
            .panes
            .insert("session_a".to_string(), active_pane());
        let cleanup = Arc::new(Mutex::new(Vec::new()));

        orchestrator.handle_event(
            &WatchEvent::SessionStart {
                session_id: "session_a".to_string(),
                display_name: "api".to_string(),
                model: "gpt-5.4".to_string(),
                initial_prompt: Some("hello".to_string()),
            },
            &cleanup,
        );
        let pane = orchestrator.panes.get("session_a").expect("pane");
        assert_eq!(pane.display_name, "api");
        assert_eq!(pane.model, "gpt-5.4");

        orchestrator.handle_event(
            &WatchEvent::ToolUse {
                session_id: "session_a".to_string(),
                timestamp: "2026-04-28T00:00:00Z".to_string(),
                tool_name: "Read".to_string(),
                summary: "src/main.rs".to_string(),
            },
            &cleanup,
        );
        assert_eq!(
            orchestrator
                .panes
                .get("session_a")
                .expect("pane")
                .observed_tool_calls,
            1
        );

        orchestrator.handle_event(
            &WatchEvent::ContextStatus {
                session_id: "session_a".to_string(),
                fill_percent: 82.0,
                context_window_tokens: Some(200_000),
                turns_to_compact: Some(1),
            },
            &cleanup,
        );
        let pane = orchestrator.panes.get("session_a").expect("pane");
        assert_eq!(pane.fill_percent, Some(82.0));
        assert_eq!(pane.context_window_tokens, Some(200_000));
        assert_eq!(pane.turns_to_compact, Some(1));

        orchestrator.handle_event(
            &WatchEvent::ModelFallback {
                session_id: "session_a".to_string(),
                requested: "gpt-5.5".to_string(),
                actual: "gpt-5.4".to_string(),
            },
            &cleanup,
        );
        assert_eq!(
            orchestrator
                .panes
                .get("session_a")
                .expect("pane")
                .model_fallback,
            Some(("gpt-5.5".to_string(), "gpt-5.4".to_string()))
        );

        orchestrator.handle_event(
            &WatchEvent::CodexTurnSummary {
                session_id: "session_a".to_string(),
                status: "incomplete".to_string(),
                requested_model: "gpt-codex-fixture".to_string(),
                served_model: Some("gpt-codex-served".to_string()),
                input_tokens: 1_280,
                cached_input_tokens: 512,
                uncached_input_tokens: 768,
                output_tokens: 96,
                reasoning_output_tokens: 32,
                total_tokens: 1_376,
            },
            &cleanup,
        );
        let pane = orchestrator.panes.get("session_a").expect("pane");
        assert_eq!(pane.model, "gpt-codex-served");
        assert_eq!(
            pane.model_fallback,
            Some((
                "gpt-codex-fixture".to_string(),
                "gpt-codex-served".to_string()
            ))
        );
        assert_eq!(
            pane.codex_turn,
            Some(CodexPaneTurnSummary {
                status: "incomplete".to_string(),
                cached_input_tokens: 512,
                uncached_input_tokens: 768,
                output_tokens: 96,
                reasoning_output_tokens: 32,
                total_tokens: 1_376,
            })
        );

        orchestrator
            .panes
            .get_mut("session_a")
            .expect("pane")
            .applied_activity = Some(Activity::Ended);
        orchestrator.handle_event(
            &WatchEvent::SessionEnd {
                session_id: "session_a".to_string(),
                outcome: "Likely Completed".to_string(),
                total_tokens: 123,
                total_turns: 4,
            },
            &cleanup,
        );
        assert!(orchestrator.panes.get("session_a").expect("pane").ended);
    }
}
