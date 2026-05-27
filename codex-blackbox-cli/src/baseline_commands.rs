use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct ContextRange {
    min_percent: Option<f64>,
    max_percent: Option<f64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct DerivedBaselineFacts {
    validation_command_category_frequency: BTreeMap<String, u64>,
    common_command_categories: BTreeMap<String, u64>,
    typical_context_range: ContextRange,
    common_repeated_failure_reason_codes: BTreeMap<String, u64>,
    common_recovery_pattern_categories: BTreeMap<String, u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct BaselineFile {
    schema_version: u32,
    enabled: bool,
    learned_at_epoch_seconds: u64,
    privacy: String,
    facts: DerivedBaselineFacts,
}

pub(crate) async fn run_preview(url: String) -> i32 {
    match build_baseline(&url).await {
        Ok(file) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&file).unwrap_or_else(|_| "{}".to_string())
            );
            0
        }
        Err(err) => {
            eprintln!("Error: {err}");
            1
        }
    }
}

pub(crate) async fn run_learn(url: String, state: Option<PathBuf>) -> i32 {
    let path = resolve_state_file(state);
    match build_baseline(&url).await {
        Ok(file) => {
            if let Some(parent) = path.parent() {
                if let Err(err) = fs::create_dir_all(parent) {
                    eprintln!("Error: failed to create {}: {err}", parent.display());
                    return 1;
                }
            }
            let body = serde_json::to_string_pretty(&file).unwrap_or_else(|_| "{}".to_string());
            if let Err(err) = fs::write(&path, format!("{body}\n")) {
                eprintln!("Error: failed to write {}: {err}", path.display());
                return 1;
            }
            println!("Codex Blackbox baseline learned.");
            println!("Baseline: {}", path.display());
            println!("Privacy: derived-only; raw prompts, outputs, commands, paths, and secrets are not stored.");
            0
        }
        Err(err) => {
            eprintln!("Error: {err}");
            1
        }
    }
}

pub(crate) fn run_show(state: Option<PathBuf>, json_output: bool) -> i32 {
    let path = resolve_state_file(state);
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) => {
            eprintln!("Error: failed to read {}: {err}", path.display());
            return 1;
        }
    };
    let value = match serde_json::from_str::<BaselineFile>(&raw) {
        Ok(value) => value,
        Err(err) => {
            eprintln!("Error: invalid baseline {}: {err}", path.display());
            return 1;
        }
    };
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_string())
        );
    } else {
        println!("Codex Blackbox baseline");
        println!("Enabled: {}", value.enabled);
        println!("Privacy: {}", value.privacy);
        println!(
            "Validation categories: {}",
            value
                .facts
                .validation_command_category_frequency
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        );
        println!(
            "Context range: {}-{}%",
            value
                .facts
                .typical_context_range
                .min_percent
                .map(|v| format!("{v:.0}"))
                .unwrap_or_else(|| "?".to_string()),
            value
                .facts
                .typical_context_range
                .max_percent
                .map(|v| format!("{v:.0}"))
                .unwrap_or_else(|| "?".to_string())
        );
    }
    0
}

pub(crate) fn run_reset(state: Option<PathBuf>) -> i32 {
    let path = resolve_state_file(state);
    match fs::remove_file(&path) {
        Ok(()) => {
            println!("Codex Blackbox baseline reset.");
            println!("Baseline: {}", path.display());
            0
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            println!("Codex Blackbox baseline was already absent.");
            println!("Baseline: {}", path.display());
            0
        }
        Err(err) => {
            eprintln!("Error: failed to remove {}: {err}", path.display());
            1
        }
    }
}

pub(crate) fn run_disable(state: Option<PathBuf>) -> i32 {
    let path = resolve_state_file(state);
    let file = BaselineFile {
        schema_version: 1,
        enabled: false,
        learned_at_epoch_seconds: now_epoch_seconds(),
        privacy: "derived_only_no_raw_prompts_outputs_commands_paths_or_secrets".to_string(),
        facts: DerivedBaselineFacts::default(),
    };
    if let Some(parent) = path.parent() {
        if let Err(err) = fs::create_dir_all(parent) {
            eprintln!("Error: failed to create {}: {err}", parent.display());
            return 1;
        }
    }
    let body = serde_json::to_string_pretty(&file).unwrap_or_else(|_| "{}".to_string());
    if let Err(err) = fs::write(&path, format!("{body}\n")) {
        eprintln!("Error: failed to write {}: {err}", path.display());
        return 1;
    }
    println!("Codex Blackbox baseline disabled.");
    println!("Baseline: {}", path.display());
    0
}

async fn build_baseline(base_url: &str) -> Result<BaselineFile, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .map_err(|err| format!("failed to build baseline client: {err}"))?;
    let sessions_url = format!(
        "{}/api/companion/sessions?limit=20&days=30",
        base_url.trim_end_matches('/')
    );
    let sessions = client
        .get(&sessions_url)
        .send()
        .await
        .map_err(|err| format!("failed to fetch {sessions_url}: {err}"))?;
    if !sessions.status().is_success() {
        return Err(format!(
            "failed to fetch {sessions_url}: HTTP {}",
            sessions.status()
        ));
    }
    let sessions = sessions
        .json::<Value>()
        .await
        .map_err(|err| format!("failed to parse {sessions_url}: {err}"))?;
    let mut facts = DerivedBaselineFacts::default();
    let mut min_context = None::<f64>;
    let mut max_context = None::<f64>;

    for session_id in sessions
        .get("sessions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|session| session.get("session_id").and_then(Value::as_str))
        .take(20)
    {
        let url = format!(
            "{}/api/companion/session/{}",
            base_url.trim_end_matches('/'),
            session_id
        );
        let Ok(resp) = client.get(&url).send().await else {
            continue;
        };
        if !resp.status().is_success() {
            continue;
        }
        let Ok(snapshot) = resp.json::<Value>().await else {
            continue;
        };
        for item in snapshot
            .pointer("/state/recent_validation_results")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(category) = item.get("category").and_then(Value::as_str) {
                increment(&mut facts.validation_command_category_frequency, category);
            }
        }
        for event in snapshot
            .get("timeline")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(category) = event
                .pointer("/payload_summary/tool_category")
                .and_then(Value::as_str)
            {
                increment(&mut facts.common_command_categories, category);
            }
        }
        for signal in snapshot
            .get("signals")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(reason) = signal.get("reason_code").and_then(Value::as_str) {
                if reason.contains("failure") {
                    increment(&mut facts.common_repeated_failure_reason_codes, reason);
                }
                if signal.get("next_action").and_then(Value::as_str).is_some() {
                    increment(
                        &mut facts.common_recovery_pattern_categories,
                        "inspect_then_retry",
                    );
                }
            }
        }
        if let Some(fill) = snapshot
            .pointer("/state/max_context_fill_percent")
            .and_then(Value::as_f64)
        {
            min_context = Some(min_context.map(|current| current.min(fill)).unwrap_or(fill));
            max_context = Some(max_context.map(|current| current.max(fill)).unwrap_or(fill));
        }
    }
    facts.typical_context_range = ContextRange {
        min_percent: min_context,
        max_percent: max_context,
    };

    Ok(BaselineFile {
        schema_version: 1,
        enabled: true,
        learned_at_epoch_seconds: now_epoch_seconds(),
        privacy: "derived_only_no_raw_prompts_outputs_commands_paths_or_secrets".to_string(),
        facts,
    })
}

fn increment(map: &mut BTreeMap<String, u64>, key: &str) {
    *map.entry(key.to_string()).or_insert(0) += 1;
}

fn resolve_state_file(path: Option<PathBuf>) -> PathBuf {
    if let Some(path) = path {
        return path;
    }
    if let Some(path) = std::env::var_os("CODEX_BLACKBOX_BASELINE_FILE") {
        return PathBuf::from(path);
    }
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".codex-blackbox")
        .join("baseline.json")
}

fn now_epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::{increment, DerivedBaselineFacts};

    #[test]
    fn baseline_counts_are_bounded_categories_only() {
        let mut facts = DerivedBaselineFacts::default();
        increment(&mut facts.common_command_categories, "bash");
        increment(&mut facts.common_command_categories, "bash");
        increment(&mut facts.validation_command_category_frequency, "test");

        assert_eq!(facts.common_command_categories.get("bash"), Some(&2));
        assert_eq!(
            facts.validation_command_category_frequency.get("test"),
            Some(&1)
        );
    }
}
