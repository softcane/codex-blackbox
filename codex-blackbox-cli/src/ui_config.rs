use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use toml_edit::{value, DocumentMut, Item, Table};

const OPENAI_BASE_URL: &str = "http://127.0.0.1:10000/backend-api/codex";
const LEGACY_CHATGPT_BASE_URL: &str = "http://127.0.0.1:10000/backend-api";
const LEGACY_MODEL_PROVIDER_ID: &str = "codex-blackbox-chatgpt";
const LEGACY_PROVIDER_NAME: &str = "OpenAI";
const LEGACY_WIRE_API: &str = "responses";
const STATE_FILE_NAME: &str = "codex-ui-state.json";

#[derive(Clone, Debug)]
pub(crate) struct UiConfigPaths {
    pub(crate) config_path: PathBuf,
    pub(crate) state_dir: PathBuf,
}

impl UiConfigPaths {
    pub(crate) fn resolve(
        config_path: Option<PathBuf>,
        state_dir: Option<PathBuf>,
    ) -> Result<Self, String> {
        Ok(Self {
            config_path: match config_path {
                Some(path) => path,
                None => default_codex_config_path()?,
            },
            state_dir: match state_dir {
                Some(path) => path,
                None => default_state_dir()?,
            },
        })
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct EnableOptions {
    pub(crate) force: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct EnableOutcome {
    pub(crate) changed: bool,
    pub(crate) backup_path: PathBuf,
    pub(crate) state_path: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
struct UiConfigStateFile {
    version: u32,
    config_path: String,
    backup_path: String,
    owned_provider_id: String,
    #[serde(default)]
    enabled_at_epoch_seconds: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum UiConfigStatus {
    NotConfigured,
    Configured,
    Misconfigured,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct UiConfigInspection {
    pub(crate) config_path: String,
    pub(crate) state_path: String,
    pub(crate) config_exists: bool,
    pub(crate) state_exists: bool,
    pub(crate) enabled_at_epoch_seconds: Option<u64>,
    pub(crate) status: UiConfigStatus,
}

pub(crate) fn target_config_toml() -> String {
    r#"openai_base_url = "http://127.0.0.1:10000/backend-api/codex"

[features]
enable_request_compression = false
"#
    .to_string()
}

pub(crate) fn enable(
    paths: &UiConfigPaths,
    options: EnableOptions,
) -> Result<EnableOutcome, String> {
    let state_path = paths.state_dir.join(STATE_FILE_NAME);
    let original = match fs::read_to_string(&paths.config_path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => {
            return Err(format!(
                "failed to read {}: {err}",
                paths.config_path.display()
            ))
        }
    };
    let mut doc = original.parse::<DocumentMut>().map_err(|err| {
        format!(
            "failed to parse {} as TOML: {err}",
            paths.config_path.display()
        )
    })?;
    if legacy_ui_config_present(&doc) {
        return Err(
            "refusing to enable UI mode while legacy Blackbox UI config is present; run `codex-blackbox ui disable` first"
                .to_string(),
        );
    }
    if openai_base_url_is_user_owned(&doc) && !state_path.is_file() && !options.force {
        return Err(format!(
            "refusing to overwrite existing user-owned `openai_base_url`; rerun with --force to replace it"
        ));
    }

    apply_target_config(&mut doc);
    let rendered = doc.to_string();
    let changed = rendered != original;
    if !changed && state_path.is_file() {
        let state_text = fs::read_to_string(&state_path)
            .map_err(|err| format!("failed to read state {}: {err}", state_path.display()))?;
        let state: UiConfigStateFile = serde_json::from_str(&state_text)
            .map_err(|err| format!("failed to parse state {}: {err}", state_path.display()))?;
        return Ok(EnableOutcome {
            changed: false,
            backup_path: PathBuf::from(state.backup_path),
            state_path,
        });
    }

    fs::create_dir_all(&paths.state_dir).map_err(|err| {
        format!(
            "failed to create state directory {}: {err}",
            paths.state_dir.display()
        )
    })?;
    let backup_path = paths
        .state_dir
        .join(format!("config-{}.toml", unix_nanos()));
    fs::write(&backup_path, &original)
        .map_err(|err| format!("failed to write backup {}: {err}", backup_path.display()))?;
    if let Some(parent) = paths.config_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
    }
    fs::write(&paths.config_path, rendered)
        .map_err(|err| format!("failed to write {}: {err}", paths.config_path.display()))?;

    let state = UiConfigStateFile {
        version: 1,
        config_path: paths.config_path.to_string_lossy().into_owned(),
        backup_path: backup_path.to_string_lossy().into_owned(),
        owned_provider_id: "openai".to_string(),
        enabled_at_epoch_seconds: Some(unix_secs()),
    };
    let state_json = serde_json::to_string_pretty(&state)
        .map_err(|err| format!("state encode failed: {err}"))?;
    fs::write(&state_path, state_json)
        .map_err(|err| format!("failed to write state {}: {err}", state_path.display()))?;

    Ok(EnableOutcome {
        changed,
        backup_path,
        state_path,
    })
}

pub(crate) fn disable(paths: &UiConfigPaths) -> Result<DisableOutcome, String> {
    let state_path = paths.state_dir.join(STATE_FILE_NAME);
    if !state_path.is_file() {
        return Ok(DisableOutcome {
            changed: false,
            state_path,
        });
    }

    let state_text = fs::read_to_string(&state_path)
        .map_err(|err| format!("failed to read state {}: {err}", state_path.display()))?;
    let state: UiConfigStateFile = serde_json::from_str(&state_text)
        .map_err(|err| format!("failed to parse state {}: {err}", state_path.display()))?;
    let backup_path = PathBuf::from(&state.backup_path);
    let backup_text = fs::read_to_string(&backup_path)
        .map_err(|err| format!("failed to read backup {}: {err}", backup_path.display()))?;
    let current_text = fs::read_to_string(&paths.config_path)
        .map_err(|err| format!("failed to read {}: {err}", paths.config_path.display()))?;
    let backup = backup_text.parse::<DocumentMut>().map_err(|err| {
        format!(
            "failed to parse backup {} as TOML: {err}",
            backup_path.display()
        )
    })?;
    let mut current = current_text.parse::<DocumentMut>().map_err(|err| {
        format!(
            "failed to parse {} as TOML: {err}",
            paths.config_path.display()
        )
    })?;

    restore_top_level_str_if_target(&mut current, &backup, "openai_base_url", OPENAI_BASE_URL);
    restore_top_level_str_if_target(
        &mut current,
        &backup,
        "chatgpt_base_url",
        LEGACY_CHATGPT_BASE_URL,
    );
    restore_top_level_str_if_target(
        &mut current,
        &backup,
        "model_provider",
        LEGACY_MODEL_PROVIDER_ID,
    );
    restore_feature_flag_if_target(&mut current, &backup);
    restore_legacy_provider_if_target(&mut current, &backup);

    let rendered = current.to_string();
    let changed = rendered != current_text;
    if changed {
        fs::write(&paths.config_path, rendered)
            .map_err(|err| format!("failed to write {}: {err}", paths.config_path.display()))?;
    }
    fs::remove_file(&state_path)
        .map_err(|err| format!("failed to remove state {}: {err}", state_path.display()))?;

    Ok(DisableOutcome {
        changed,
        state_path,
    })
}

pub(crate) fn inspect(paths: &UiConfigPaths) -> Result<UiConfigInspection, String> {
    let state_path = paths.state_dir.join(STATE_FILE_NAME);
    let config_exists = paths.config_path.is_file();
    let enabled_at_epoch_seconds = read_state_enabled_at(&state_path)?;
    let status = if !config_exists {
        UiConfigStatus::NotConfigured
    } else {
        let contents = fs::read_to_string(&paths.config_path)
            .map_err(|err| format!("failed to read {}: {err}", paths.config_path.display()))?;
        let doc = contents.parse::<DocumentMut>().map_err(|err| {
            format!(
                "failed to parse {} as TOML: {err}",
                paths.config_path.display()
            )
        })?;
        config_status_for_doc(&doc)
    };

    Ok(UiConfigInspection {
        config_path: paths.config_path.to_string_lossy().into_owned(),
        state_path: state_path.to_string_lossy().into_owned(),
        config_exists,
        state_exists: state_path.is_file(),
        enabled_at_epoch_seconds,
        status,
    })
}

fn read_state_enabled_at(state_path: &std::path::Path) -> Result<Option<u64>, String> {
    if !state_path.is_file() {
        return Ok(None);
    }
    let text = fs::read_to_string(state_path)
        .map_err(|err| format!("failed to read state {}: {err}", state_path.display()))?;
    let state: UiConfigStateFile = serde_json::from_str(&text)
        .map_err(|err| format!("failed to parse state {}: {err}", state_path.display()))?;
    Ok(state.enabled_at_epoch_seconds)
}

fn config_status_for_doc(doc: &DocumentMut) -> UiConfigStatus {
    let openai_base_url_matches =
        doc.get("openai_base_url").and_then(Item::as_str) == Some(OPENAI_BASE_URL);
    let feature_matches = doc
        .get("features")
        .and_then(|features| features.get("enable_request_compression"))
        .and_then(Item::as_bool)
        == Some(false);
    if legacy_ui_config_present(doc) {
        UiConfigStatus::Misconfigured
    } else if openai_base_url_matches && feature_matches {
        UiConfigStatus::Configured
    } else if openai_base_url_matches || feature_matches {
        UiConfigStatus::Misconfigured
    } else {
        UiConfigStatus::NotConfigured
    }
}

#[derive(Clone, Debug)]
pub(crate) struct DisableOutcome {
    pub(crate) changed: bool,
    pub(crate) state_path: PathBuf,
}

fn openai_base_url_is_user_owned(doc: &DocumentMut) -> bool {
    doc.get("openai_base_url")
        .and_then(Item::as_str)
        .is_some_and(|base_url| base_url != OPENAI_BASE_URL)
}

fn legacy_ui_config_present(doc: &DocumentMut) -> bool {
    doc.get("chatgpt_base_url").and_then(Item::as_str) == Some(LEGACY_CHATGPT_BASE_URL)
        || doc.get("model_provider").and_then(Item::as_str) == Some(LEGACY_MODEL_PROVIDER_ID)
        || legacy_provider_matches_target(doc)
}

fn legacy_provider_matches_target(doc: &DocumentMut) -> bool {
    let Some(provider) = legacy_provider_item(doc) else {
        return false;
    };
    item_str(provider, "name") == Some(LEGACY_PROVIDER_NAME)
        && item_str(provider, "base_url") == Some(OPENAI_BASE_URL)
        && item_str(provider, "wire_api") == Some(LEGACY_WIRE_API)
        && item_bool(provider, "requires_openai_auth") == Some(true)
        && item_bool(provider, "supports_websockets") == Some(false)
}

fn legacy_provider_item(doc: &DocumentMut) -> Option<&Item> {
    doc.get("model_providers")?.get(LEGACY_MODEL_PROVIDER_ID)
}

fn item_str<'a>(item: &'a Item, key: &str) -> Option<&'a str> {
    item.get(key)?.as_str()
}

fn item_bool(item: &Item, key: &str) -> Option<bool> {
    item.get(key)?.as_bool()
}

fn restore_top_level_str_if_target(
    current: &mut DocumentMut,
    backup: &DocumentMut,
    key: &str,
    target: &str,
) {
    if current.get(key).and_then(Item::as_str) != Some(target) {
        return;
    }
    match backup.get(key).cloned() {
        Some(original) => {
            current.as_table_mut().insert(key, original);
        }
        None => {
            current.as_table_mut().remove(key);
        }
    }
}

fn restore_feature_flag_if_target(current: &mut DocumentMut, backup: &DocumentMut) {
    let current_is_target = current
        .get("features")
        .and_then(|features| features.get("enable_request_compression"))
        .and_then(Item::as_bool)
        == Some(false);
    if !current_is_target {
        return;
    }

    let original = backup
        .get("features")
        .and_then(|features| features.get("enable_request_compression"))
        .cloned();
    let features = ensure_table(
        current
            .as_table_mut()
            .entry("features")
            .or_insert(Item::Table(Table::new())),
    );
    match original {
        Some(original) => {
            features.insert("enable_request_compression", original);
        }
        None => {
            features.remove("enable_request_compression");
        }
    }
    remove_empty_table_if_absent_in_backup(current, backup, "features");
}

fn restore_legacy_provider_if_target(current: &mut DocumentMut, backup: &DocumentMut) {
    if !legacy_provider_matches_target(current) {
        return;
    }

    let original = backup
        .get("model_providers")
        .and_then(|providers| providers.get(LEGACY_MODEL_PROVIDER_ID))
        .cloned();
    let providers = ensure_table(
        current
            .as_table_mut()
            .entry("model_providers")
            .or_insert(Item::Table(Table::new())),
    );
    providers.set_implicit(true);
    match original {
        Some(original) => {
            providers.insert(LEGACY_MODEL_PROVIDER_ID, original);
        }
        None => {
            providers.remove(LEGACY_MODEL_PROVIDER_ID);
        }
    }
    remove_empty_table_if_absent_in_backup(current, backup, "model_providers");
}

fn remove_empty_table_if_absent_in_backup(
    current: &mut DocumentMut,
    backup: &DocumentMut,
    key: &str,
) {
    let should_remove = current
        .get(key)
        .and_then(Item::as_table)
        .is_some_and(Table::is_empty)
        && backup.get(key).is_none();
    if should_remove {
        current.as_table_mut().remove(key);
    }
}

fn apply_target_config(doc: &mut DocumentMut) {
    doc["openai_base_url"] = value(OPENAI_BASE_URL);
    let features = ensure_table(
        doc.as_table_mut()
            .entry("features")
            .or_insert(Item::Table(Table::new())),
    );
    features["enable_request_compression"] = value(false);
}

fn ensure_table(item: &mut Item) -> &mut Table {
    if !item.is_table() {
        *item = Item::Table(Table::new());
    }
    item.as_table_mut().expect("item was just made a table")
}

fn unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn default_codex_config_path() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("CODEX_BLACKBOX_CODEX_CONFIG") {
        return Ok(PathBuf::from(path));
    }
    if let Some(codex_home) = std::env::var_os("CODEX_HOME") {
        return Ok(PathBuf::from(codex_home).join("config.toml"));
    }
    let Some(home) = std::env::var_os("HOME") else {
        return Err(
            "could not determine Codex config path; set CODEX_BLACKBOX_CODEX_CONFIG".to_string(),
        );
    };
    Ok(PathBuf::from(home).join(".codex/config.toml"))
}

fn default_state_dir() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("CODEX_BLACKBOX_UI_STATE_DIR") {
        return Ok(PathBuf::from(path));
    }
    if let Some(path) = std::env::var_os("CODEX_BLACKBOX_HOME") {
        return Ok(PathBuf::from(path).join("ui"));
    }
    if let Some(path) = std::env::var_os("XDG_DATA_HOME") {
        return Ok(PathBuf::from(path).join("codex-blackbox/ui"));
    }
    let Some(home) = std::env::var_os("HOME") else {
        return Err(
            "could not determine Codex Blackbox UI state path; set CODEX_BLACKBOX_UI_STATE_DIR"
                .to_string(),
        );
    };
    Ok(PathBuf::from(home).join(".local/share/codex-blackbox/ui"))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_test_dir(label: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time went backwards")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "codex-blackbox-ui-config-{label}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create test dir");
        path
    }

    #[test]
    fn dry_run_renders_exact_ui_config_shape() {
        assert_eq!(
            super::target_config_toml(),
            r#"openai_base_url = "http://127.0.0.1:10000/backend-api/codex"

[features]
enable_request_compression = false
"#
        );
    }

    #[test]
    fn enable_creates_backup_state_and_writes_expected_toml() {
        let dir = unique_test_dir("enable");
        let config_path = dir.join("config.toml");
        let state_dir = dir.join("state");
        fs::write(&config_path, "model = \"gpt-5\"\n").expect("write config");

        let outcome = super::enable(
            &super::UiConfigPaths {
                config_path: config_path.clone(),
                state_dir: state_dir.clone(),
            },
            super::EnableOptions::default(),
        )
        .expect("enable ui config");

        assert!(outcome.changed);
        assert!(outcome.backup_path.is_file());
        assert_eq!(
            fs::read_to_string(&outcome.backup_path).expect("read backup"),
            "model = \"gpt-5\"\n"
        );
        assert!(outcome.state_path.is_file());
        let written = fs::read_to_string(&config_path).expect("read config");
        assert!(written.contains("model = \"gpt-5\""));
        assert!(written.contains(&super::target_config_toml()));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn enable_is_idempotent_and_preserves_original_backup_state() {
        let dir = unique_test_dir("enable-idempotent");
        let config_path = dir.join("config.toml");
        let state_dir = dir.join("state");
        fs::write(&config_path, "model = \"gpt-5\"\n").expect("write config");
        let paths = super::UiConfigPaths {
            config_path: config_path.clone(),
            state_dir: state_dir.clone(),
        };

        let first = super::enable(&paths, super::EnableOptions::default()).expect("first enable");
        let first_state = fs::read_to_string(&first.state_path).expect("read first state");
        let first_config = fs::read_to_string(&config_path).expect("read first config");
        let second = super::enable(&paths, super::EnableOptions::default()).expect("second enable");

        assert!(!second.changed);
        assert_eq!(
            fs::read_to_string(&config_path).expect("read second config"),
            first_config
        );
        assert_eq!(
            fs::read_to_string(&first.state_path).expect("read second state"),
            first_state
        );
        let backup_count = fs::read_dir(&state_dir)
            .expect("read state dir")
            .filter(|entry| {
                entry
                    .as_ref()
                    .expect("state entry")
                    .file_name()
                    .to_string_lossy()
                    .starts_with("config-")
            })
            .count();
        assert_eq!(backup_count, 1);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn enable_refuses_user_owned_openai_base_url_unless_forced() {
        let dir = unique_test_dir("enable-conflict");
        let config_path = dir.join("config.toml");
        let state_dir = dir.join("state");
        let original = r#"openai_base_url = "http://localhost:1234/v1"
"#;
        fs::write(&config_path, original).expect("write config");
        let paths = super::UiConfigPaths {
            config_path: config_path.clone(),
            state_dir: state_dir.clone(),
        };

        let err = super::enable(&paths, super::EnableOptions::default())
            .expect_err("conflicting base URL should be refused");
        assert!(err.contains("openai_base_url"));
        assert!(err.contains("--force"));
        assert_eq!(
            fs::read_to_string(&config_path).expect("read config"),
            original
        );
        assert!(!state_dir.exists());

        let forced =
            super::enable(&paths, super::EnableOptions { force: true }).expect("forced enable");
        assert!(forced.changed);
        assert!(fs::read_to_string(&config_path)
            .expect("read forced config")
            .contains(r#"openai_base_url = "http://127.0.0.1:10000/backend-api/codex""#));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn enable_refuses_legacy_blackbox_ui_config_without_state() {
        let dir = unique_test_dir("enable-legacy-refuse");
        let config_path = dir.join("config.toml");
        let state_dir = dir.join("state");
        let original = r#"chatgpt_base_url = "http://127.0.0.1:10000/backend-api"
model_provider = "codex-blackbox-chatgpt"
"#;
        fs::write(&config_path, original).expect("write config");
        let paths = super::UiConfigPaths {
            config_path: config_path.clone(),
            state_dir: state_dir.clone(),
        };

        let err = super::enable(&paths, super::EnableOptions::default())
            .expect_err("legacy config should require disable first");
        assert!(err.contains("legacy Blackbox UI config"));
        assert_eq!(
            fs::read_to_string(&config_path).expect("read config"),
            original
        );
        assert!(!state_dir.exists());

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn enable_preserves_unrelated_user_settings() {
        let dir = unique_test_dir("enable-preserve");
        let config_path = dir.join("config.toml");
        let state_dir = dir.join("state");
        fs::write(
            &config_path,
            r#"model = "gpt-5"

[features]
approval_policy = "never"

[model_providers.user-provider]
name = "User"
base_url = "http://localhost:1234"
wire_api = "responses"
"#,
        )
        .expect("write config");

        super::enable(
            &super::UiConfigPaths {
                config_path: config_path.clone(),
                state_dir,
            },
            super::EnableOptions::default(),
        )
        .expect("enable");

        let written = fs::read_to_string(&config_path).expect("read config");
        assert!(written.contains("model = \"gpt-5\""));
        assert!(written.contains("approval_policy = \"never\""));
        assert!(written.contains("[model_providers.user-provider]"));
        assert!(written.contains("base_url = \"http://localhost:1234\""));
        assert!(written.contains(r#"openai_base_url = "http://127.0.0.1:10000/backend-api/codex""#));
        assert!(written.contains("enable_request_compression = false"));
        assert!(!written.contains("model_provider = \"codex-blackbox-chatgpt\""));
        assert!(!written.contains("[model_providers.codex-blackbox-chatgpt]"));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn disable_restores_owned_changes_without_deleting_later_user_edits() {
        let dir = unique_test_dir("disable");
        let config_path = dir.join("config.toml");
        let state_dir = dir.join("state");
        fs::write(
            &config_path,
            r#"model = "gpt-5"
model_provider = "openai"

[features]
approval_policy = "never"
"#,
        )
        .expect("write config");
        let paths = super::UiConfigPaths {
            config_path: config_path.clone(),
            state_dir: state_dir.clone(),
        };
        super::enable(&paths, super::EnableOptions::default()).expect("enable");
        let enabled = fs::read_to_string(&config_path).expect("read enabled");
        let edited = enabled
            .replace("model = \"gpt-5\"", "model = \"gpt-5.1\"")
            .replace(
                "[features]\napproval_policy = \"never\"",
                "[features]\napproval_policy = \"never\"\nafter_enable = true",
            );
        fs::write(&config_path, edited).expect("write user edits");

        let outcome = super::disable(&paths).expect("disable");
        assert!(outcome.changed);
        assert!(!outcome.state_path.exists());
        let disabled = fs::read_to_string(&config_path).expect("read disabled");
        assert!(disabled.contains("model = \"gpt-5.1\""));
        assert!(disabled.contains("model_provider = \"openai\""));
        assert!(disabled.contains("approval_policy = \"never\""));
        assert!(disabled.contains("after_enable = true"));
        assert!(!disabled.contains("openai_base_url"));
        assert!(!disabled.contains("chatgpt_base_url"));
        assert!(!disabled.contains("enable_request_compression"));
        assert!(!disabled.contains("codex-blackbox-chatgpt"));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn disable_removes_legacy_blackbox_ui_keys_from_existing_state() {
        let dir = unique_test_dir("disable-legacy");
        let config_path = dir.join("config.toml");
        let state_dir = dir.join("state");
        fs::write(
            &config_path,
            r#"model = "gpt-5"
model_provider = "openai"
"#,
        )
        .expect("write config");
        let paths = super::UiConfigPaths {
            config_path: config_path.clone(),
            state_dir: state_dir.clone(),
        };
        let outcome =
            super::enable(&paths, super::EnableOptions::default()).expect("enable safe config");
        assert!(outcome.backup_path.is_file());
        let legacy_config = r#"model = "gpt-5"
chatgpt_base_url = "http://127.0.0.1:10000/backend-api"
model_provider = "codex-blackbox-chatgpt"

[features]
enable_request_compression = false

[model_providers.codex-blackbox-chatgpt]
name = "OpenAI"
base_url = "http://127.0.0.1:10000/backend-api/codex"
wire_api = "responses"
requires_openai_auth = true
supports_websockets = false
"#;
        fs::write(&config_path, legacy_config).expect("write legacy config");

        let outcome = super::disable(&paths).expect("disable legacy config");
        assert!(outcome.changed);
        let disabled = fs::read_to_string(&config_path).expect("read disabled");
        assert!(disabled.contains("model = \"gpt-5\""));
        assert!(disabled.contains("model_provider = \"openai\""));
        assert!(!disabled.contains("chatgpt_base_url"));
        assert!(!disabled.contains("codex-blackbox-chatgpt"));
        assert!(!disabled.contains("enable_request_compression"));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn inspect_reports_missing_config_configured_and_misconfigured() {
        let dir = unique_test_dir("inspect");
        let config_path = dir.join("config.toml");
        let state_dir = dir.join("state");
        let paths = super::UiConfigPaths {
            config_path: config_path.clone(),
            state_dir,
        };

        let missing = super::inspect(&paths).expect("inspect missing");
        assert!(!missing.config_exists);
        assert_eq!(missing.status, super::UiConfigStatus::NotConfigured);

        super::enable(&paths, super::EnableOptions::default()).expect("enable");
        let configured = super::inspect(&paths).expect("inspect configured");
        assert!(configured.config_exists);
        assert!(configured.state_exists);
        assert_eq!(configured.status, super::UiConfigStatus::Configured);

        fs::write(
            &config_path,
            r#"openai_base_url = "http://127.0.0.1:10000/backend-api/codex"
"#,
        )
        .expect("write partial");
        let partial = super::inspect(&paths).expect("inspect partial");
        assert_eq!(partial.status, super::UiConfigStatus::Misconfigured);

        fs::write(
            &config_path,
            r#"chatgpt_base_url = "http://127.0.0.1:10000/backend-api"
model_provider = "codex-blackbox-chatgpt"

[features]
enable_request_compression = false
"#,
        )
        .expect("write legacy unsafe config");
        let legacy = super::inspect(&paths).expect("inspect legacy");
        assert_eq!(legacy.status, super::UiConfigStatus::Misconfigured);

        let _ = fs::remove_dir_all(dir);
    }
}
