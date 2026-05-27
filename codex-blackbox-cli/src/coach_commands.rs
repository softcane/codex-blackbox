use std::fs;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};

const BLACKBOX_HOOK_MARKER: &str = "codex-blackbox coach handle";

#[derive(Debug, Serialize)]
struct CoachStatus {
    hooks_file: String,
    exists: bool,
    installed_handlers: usize,
    events: Vec<String>,
    evidence: String,
}

pub(crate) fn run_preview(hooks_file: Option<PathBuf>, url: String) -> i32 {
    let path = resolve_hooks_file(hooks_file);
    println!("Codex Blackbox coach hook preview");
    println!("Hooks file: {}", path.display());
    println!("Scope: project-local Codex hooks.json");
    println!("Files modified: none");
    println!("{}", desired_hooks_json(&url));
    println!("Evidence: hook evidence is advisory and labeled separately from proxy evidence.");
    0
}

pub(crate) fn run_install(
    hooks_file: Option<PathBuf>,
    dry_run: bool,
    force: bool,
    url: String,
) -> i32 {
    let path = resolve_hooks_file(hooks_file);
    let desired = desired_hooks_value(&url);
    let existing = match read_hooks_json(&path) {
        Ok(value) => value,
        Err(err) if force => {
            eprintln!("Warning: replacing invalid hooks file because --force was set: {err}");
            json!({"hooks": {}})
        }
        Err(err) => {
            eprintln!("Error: {err}");
            return 1;
        }
    };
    let merged = merge_hooks(existing, desired);
    if dry_run {
        println!("Codex Blackbox coach hook install preview");
        println!("Hooks file: {}", path.display());
        println!("Files modified: none");
        println!(
            "{}",
            serde_json::to_string_pretty(&merged).unwrap_or_else(|_| "{}".to_string())
        );
        return 0;
    }

    if let Some(parent) = path.parent() {
        if let Err(err) = fs::create_dir_all(parent) {
            eprintln!("Error: failed to create {}: {err}", parent.display());
            return 1;
        }
    }
    let body = serde_json::to_string_pretty(&merged).unwrap_or_else(|_| "{}".to_string());
    if let Err(err) = fs::write(&path, format!("{body}\n")) {
        eprintln!("Error: failed to write {}: {err}", path.display());
        return 1;
    }
    println!("Codex Blackbox coach hooks installed.");
    println!("Hooks file: {}", path.display());
    println!("Review and trust hooks inside Codex with /hooks if Codex asks.");
    0
}

pub(crate) fn run_uninstall(hooks_file: Option<PathBuf>) -> i32 {
    let path = resolve_hooks_file(hooks_file);
    let existing = match read_hooks_json(&path) {
        Ok(value) => value,
        Err(err) => {
            eprintln!("Error: {err}");
            return 1;
        }
    };
    let cleaned = remove_blackbox_hooks(existing);
    let body = serde_json::to_string_pretty(&cleaned).unwrap_or_else(|_| "{}".to_string());
    if let Err(err) = fs::write(&path, format!("{body}\n")) {
        eprintln!("Error: failed to write {}: {err}", path.display());
        return 1;
    }
    println!("Codex Blackbox coach hooks removed.");
    println!("Hooks file: {}", path.display());
    0
}

pub(crate) fn run_status(hooks_file: Option<PathBuf>, json_output: bool) -> i32 {
    let path = resolve_hooks_file(hooks_file);
    let value = read_hooks_json(&path).unwrap_or_else(|_| json!({}));
    let (installed_handlers, events) = count_blackbox_hooks(&value);
    let status = CoachStatus {
        hooks_file: path.display().to_string(),
        exists: path.exists(),
        installed_handlers,
        events,
        evidence: "hook evidence is advisory; proxy evidence remains durable model-turn authority"
            .to_string(),
    };
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&status).unwrap_or_else(|_| "{}".to_string())
        );
    } else {
        println!("Codex Blackbox coach hooks");
        println!("Hooks file: {}", status.hooks_file);
        println!("Installed handlers: {}", status.installed_handlers);
        println!("Events: {}", status.events.join(", "));
        println!("Evidence: {}", status.evidence);
    }
    0
}

pub(crate) async fn run_handle(url: String) -> i32 {
    let mut input = String::new();
    if io::stdin().read_to_string(&mut input).is_err() {
        print_fail_open();
        return 0;
    }
    let payload = serde_json::from_str::<Value>(&input).unwrap_or_else(|_| json!({}));
    let endpoint = format!("{}/api/coach/hook", url.trim_end_matches('/'));
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
    {
        Ok(client) => client,
        Err(_) => {
            print_fail_open();
            return 0;
        }
    };
    match client.post(endpoint).json(&payload).send().await {
        Ok(resp) if resp.status().is_success() => match resp.text().await {
            Ok(text) if !text.trim().is_empty() => {
                println!("{text}");
            }
            _ => print_fail_open(),
        },
        _ => print_fail_open(),
    }
    0
}

fn print_fail_open() {
    let _ = writeln!(io::stdout(), r#"{{"continue":true}}"#);
}

fn resolve_hooks_file(path: Option<PathBuf>) -> PathBuf {
    if let Some(path) = path {
        return path;
    }
    if let Some(path) = std::env::var_os("CODEX_BLACKBOX_HOOKS_FILE") {
        return PathBuf::from(path);
    }
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".codex")
        .join("hooks.json")
}

fn read_hooks_json(path: &PathBuf) -> Result<Value, String> {
    if !path.exists() {
        return Ok(json!({"hooks": {}}));
    }
    let raw = fs::read_to_string(path)
        .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
    serde_json::from_str(&raw)
        .map_err(|err| format!("invalid hooks JSON {}: {err}", path.display()))
}

fn desired_hooks_json(url: &str) -> String {
    serde_json::to_string_pretty(&desired_hooks_value(url)).unwrap_or_else(|_| "{}".to_string())
}

fn desired_hooks_value(url: &str) -> Value {
    let command = format!(
        "codex-blackbox coach handle --url {}",
        shell_quote(url.trim_end_matches('/'))
    );
    json!({
        "hooks": {
            "PreToolUse": [{
                "matcher": "^(Bash|apply_patch|Edit|Write|mcp__.*)$",
                "hooks": [{
                    "type": "command",
                    "command": command,
                    "timeout": 10,
                    "statusMessage": "Checking Codex Blackbox coach"
                }]
            }],
            "PostToolUse": [{
                "matcher": "^(Bash|apply_patch|Edit|Write|mcp__.*)$",
                "hooks": [{
                    "type": "command",
                    "command": command,
                    "timeout": 10,
                    "statusMessage": "Recording Codex Blackbox evidence"
                }]
            }],
            "UserPromptSubmit": [{
                "hooks": [{
                    "type": "command",
                    "command": command,
                    "timeout": 10,
                    "statusMessage": "Checking Codex Blackbox coach"
                }]
            }],
            "Stop": [{
                "hooks": [{
                    "type": "command",
                    "command": command,
                    "timeout": 10,
                    "statusMessage": "Checking Codex Blackbox stop state"
                }]
            }],
            "PreCompact": [{
                "hooks": [{
                    "type": "command",
                    "command": command,
                    "timeout": 10,
                    "statusMessage": "Recording Codex Blackbox compaction evidence"
                }]
            }],
            "PostCompact": [{
                "hooks": [{
                    "type": "command",
                    "command": command,
                    "timeout": 10,
                    "statusMessage": "Recording Codex Blackbox compaction evidence"
                }]
            }]
        }
    })
}

fn merge_hooks(mut existing: Value, desired: Value) -> Value {
    existing = remove_blackbox_hooks(existing);
    if !existing.is_object() {
        existing = json!({});
    }
    if existing.get("hooks").and_then(Value::as_object).is_none() {
        existing["hooks"] = json!({});
    }
    let desired_hooks = desired
        .get("hooks")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let hooks = existing
        .get_mut("hooks")
        .and_then(Value::as_object_mut)
        .expect("hooks object");
    for (event, groups) in desired_hooks {
        let target = hooks.entry(event).or_insert_with(|| json!([]));
        if let Some(target_array) = target.as_array_mut() {
            if let Some(groups) = groups.as_array() {
                target_array.extend(groups.iter().cloned());
            }
        } else {
            *target = groups;
        }
    }
    existing
}

fn remove_blackbox_hooks(mut value: Value) -> Value {
    let Some(hooks) = value.get_mut("hooks").and_then(Value::as_object_mut) else {
        return value;
    };
    let events = hooks.keys().cloned().collect::<Vec<_>>();
    for event in events {
        let Some(groups) = hooks.get_mut(&event).and_then(Value::as_array_mut) else {
            continue;
        };
        for group in groups.iter_mut() {
            if let Some(handlers) = group.get_mut("hooks").and_then(Value::as_array_mut) {
                handlers.retain(|handler| !handler_is_blackbox(handler));
            }
        }
        groups.retain(|group| {
            group
                .get("hooks")
                .and_then(Value::as_array)
                .is_some_and(|handlers| !handlers.is_empty())
        });
    }
    hooks.retain(|_, groups| groups.as_array().is_some_and(|items| !items.is_empty()));
    value
}

fn handler_is_blackbox(handler: &Value) -> bool {
    handler
        .get("command")
        .and_then(Value::as_str)
        .is_some_and(|command| command.contains(BLACKBOX_HOOK_MARKER))
}

fn count_blackbox_hooks(value: &Value) -> (usize, Vec<String>) {
    let mut count = 0usize;
    let mut events = Vec::new();
    if let Some(hooks) = value.get("hooks").and_then(Value::as_object) {
        for (event, groups) in hooks {
            let event_count = groups
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|group| group.get("hooks").and_then(Value::as_array))
                .flatten()
                .filter(|handler| handler_is_blackbox(handler))
                .count();
            if event_count > 0 {
                count += event_count;
                events.push(event.clone());
            }
        }
    }
    events.sort();
    (count, events)
}

fn shell_quote(value: &str) -> String {
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | ':' | '.' | '_' | '-'))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::{count_blackbox_hooks, merge_hooks, remove_blackbox_hooks};
    use serde_json::json;

    #[test]
    fn install_merge_preserves_user_hooks_and_removes_old_blackbox_entries() {
        let existing = json!({
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [
                        {"type": "command", "command": "echo user"},
                        {"type": "command", "command": "codex-blackbox coach handle --url http://old"}
                    ]
                }]
            }
        });
        let desired = json!({
            "hooks": {
                "Stop": [{
                    "hooks": [{"type": "command", "command": "codex-blackbox coach handle --url http://new"}]
                }]
            }
        });
        let merged = merge_hooks(existing, desired);
        let (count, events) = count_blackbox_hooks(&merged);

        assert_eq!(count, 1);
        assert_eq!(events, vec!["Stop"]);
        assert!(serde_json::to_string(&merged)
            .expect("json")
            .contains("echo user"));
        assert!(!serde_json::to_string(&merged)
            .expect("json")
            .contains("http://old"));
    }

    #[test]
    fn uninstall_removes_empty_blackbox_groups() {
        let existing = json!({
            "hooks": {
                "Stop": [{
                    "hooks": [{"type": "command", "command": "codex-blackbox coach handle --url http://new"}]
                }]
            }
        });
        let cleaned = remove_blackbox_hooks(existing);
        let (count, events) = count_blackbox_hooks(&cleaned);

        assert_eq!(count, 0);
        assert!(events.is_empty());
        assert_eq!(cleaned, json!({"hooks": {}}));
    }
}
