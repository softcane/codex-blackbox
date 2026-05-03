use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

fn read_repo_file(path: &str) -> String {
    fs::read_to_string(repo_root().join(path)).unwrap_or_else(|err| panic!("read {path}: {err}"))
}

#[test]
fn codex_facing_docs_and_dashboard_do_not_use_legacy_provider_branding() {
    for path in [
        "README.md",
        "docs/codex-telemetry-without-jsonl-plan.md",
        "docs/codex-traffic-contract.md",
        "docs/real-codex-smoke.md",
        "docs/automated-feedback-testing.md",
        "docs/remaining-phases.md",
        "grafana/dashboards/coditor.json",
    ] {
        let body = read_repo_file(path);
        for forbidden in ["Anthropic", "anthropic", "Claude", "claude"] {
            assert!(
                !body.contains(forbidden),
                "{path} must not expose legacy-provider branding in the Codex surface"
            );
        }
    }
}

#[test]
fn codex_target_path_does_not_depend_on_local_json_stdout() {
    let cli = read_repo_file("coditor-cli/src/main.rs");
    for forbidden in [
        "codex_json_stdout",
        "coditor-cli-json",
        "coditor.codex_hook.v1",
        "/api/hooks/codex",
    ] {
        assert!(
            !cli.contains(forbidden),
            "normal Codex wrapper path must not use {forbidden}"
        );
    }

    let core = read_repo_file("coditor-core/src/main.rs");
    for forbidden in [
        "codex_hook",
        "coditor-cli-json",
        "coditor.codex_hook.v1",
        "/api/hooks/codex",
    ] {
        assert!(
            !core.contains(forbidden),
            "Codex core surface must not retain app-server hook side channel {forbidden}"
        );
    }

    let dogfood = read_repo_file("test/dogfood-codex-sessions.sh");
    assert!(
        !dogfood.contains("\n    --json\n"),
        "dogfood target path must not pass codex exec --json"
    );
    assert!(
        !dogfood.contains(".jsonl"),
        "dogfood target path must not capture local Codex stdout as JSONL"
    );

    let fixture_dir = repo_root().join("test/fixtures");
    for entry in fs::read_dir(&fixture_dir).unwrap_or_else(|err| panic!("read fixtures: {err}")) {
        let entry = entry.expect("read fixture entry");
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        assert!(
            !file_name.contains("codex_hook"),
            "Codex hook fixtures must not remain in {fixture_dir:?}: {file_name}"
        );
    }
}

#[test]
fn codex_dashboard_contains_only_envoy_derived_codex_metrics() {
    let dashboard = read_repo_file("grafana/dashboards/coditor.json");
    let parsed: Value = serde_json::from_str(&dashboard).expect("dashboard JSON");
    let panels = parsed
        .get("panels")
        .and_then(Value::as_array)
        .expect("dashboard panels");
    let forbidden_metric_families = [
        "coditor_skill_events_total",
        "coditor_mcp_events_total",
        "coditor_history_tool_failures",
        "coditor_active_sessions",
        "coditor_weekly_tokens",
        "coditor_history_estimated_spend",
        "coditor_history_sessions",
        "coditor_history_degraded",
    ];
    let forbidden_panel_terms = [
        "Tool failures",
        "Skill lifecycle",
        "MCP lifecycle",
        "Estimated Codex cost",
        "Active sessions",
    ];

    for panel in panels {
        let title = panel.get("title").and_then(Value::as_str).unwrap_or("");
        let description = panel
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("");
        for term in forbidden_panel_terms {
            assert!(
                !title.contains(term) && !description.contains(term),
                "Codex dashboard panel must not expose non-Envoy lifecycle/cost surface: {title}"
            );
        }

        if let Some(targets) = panel.get("targets").and_then(Value::as_array) {
            for target in targets {
                let expr = target.get("expr").and_then(Value::as_str).unwrap_or("");
                for metric in forbidden_metric_families {
                    assert!(
                        !expr.contains(metric),
                        "Codex dashboard panel {title:?} must not query {metric}"
                    );
                }
            }
        }
    }

    assert!(
        dashboard.contains("coditor_codex_response_status_total"),
        "dashboard must use true Envoy-derived Codex response status counters"
    );
}
