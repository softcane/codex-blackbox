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

fn collect_scanned_files() -> Vec<PathBuf> {
    fn visit(root: &Path, path: &Path, out: &mut Vec<PathBuf>) {
        let entries = fs::read_dir(path).unwrap_or_else(|err| panic!("read {path:?}: {err}"));
        for entry in entries {
            let entry = entry.expect("read dir entry");
            let path = entry.path();
            let relative = path.strip_prefix(root).expect("strip repo root");
            if relative
                .components()
                .any(|part| part.as_os_str() == "target")
            {
                continue;
            }
            if path.is_dir() {
                visit(root, &path, out);
            } else if matches!(
                path.extension().and_then(|ext| ext.to_str()),
                Some("rs" | "md" | "json" | "sh" | "yml" | "yaml" | "toml" | "py")
            ) {
                out.push(relative.to_path_buf());
            }
        }
    }

    let root = repo_root();
    let mut files = vec![PathBuf::from("README.md")];
    for path in [
        "docs",
        "grafana",
        "coditor-cli/src",
        "coditor-core/src",
        "coditor-core/tests",
        "test",
    ] {
        visit(&root, &root.join(path), &mut files);
    }
    files.sort();
    files.dedup();
    files
}

#[test]
fn codex_surface_does_not_use_legacy_provider_branding() {
    let forbidden_terms = [
        "Anthropic",
        "anthropic",
        "Claude",
        "claude",
        "legacy_anthropic",
        "legacy_claude",
        "fake-anthropic",
        "/api/hooks/claude-code",
        "sonnet",
        "opus",
        "haiku",
    ];

    for path in collect_scanned_files() {
        if path == PathBuf::from("coditor-core/tests/codex_envoy_only_surface.rs") {
            continue;
        }
        let body = read_repo_file(path.to_str().expect("utf8 relative path"));
        for forbidden in forbidden_terms {
            assert!(
                !body.contains(forbidden),
                "{} must not expose legacy-provider term {forbidden:?} in the Codex surface",
                path.display()
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
    assert!(
        dogfood.contains("--no-json"),
        "dogfood harness must accept the documented no-JSON target-path flag"
    );
    for forbidden in ["thread.started", "stdout_session_ids"] {
        assert!(
            !dogfood.contains(forbidden),
            "dogfood harness must not merge local Codex stdout identity source {forbidden}"
        );
    }

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
fn core_metrics_register_only_envoy_derived_codex_families() {
    let metrics = read_repo_file("coditor-core/src/metrics.rs");
    let allowed_metric_families = [
        "coditor_requests_total",
        "coditor_tokens_total",
        "coditor_sessions_degraded_total",
        "coditor_codex_response_status_total",
        "coditor_model_fallback_total",
        "coditor_tool_calls_total",
        "coditor_turn_duration_seconds",
        "coditor_context_fill_percent",
    ];
    for metric in allowed_metric_families {
        assert!(
            metrics.contains(metric),
            "Envoy-derived metric family missing from metrics.rs: {metric}"
        );
    }

    let forbidden_metric_families = [
        "coditor_estimated_cost_dollars_total",
        "coditor_estimated_session_cost_dollars",
        "coditor_estimated_cache_waste_dollars_total",
        "coditor_cache_events_total",
        "coditor_tool_failures_total",
        "coditor_mcp_tool_calls_total",
        "coditor_mcp_tool_failures_total",
        "coditor_mcp_server_calls_total",
        "coditor_mcp_server_failures_total",
        "coditor_mcp_events_total",
        "coditor_skill_events_total",
        "coditor_active_sessions",
        "coditor_weekly_tokens_used",
        "coditor_weekly_tokens_remaining",
        "coditor_weekly_token_budget",
        "coditor_projected_exhaustion_seconds",
        "coditor_history_estimated_spend",
        "coditor_history_tool_failures",
        "coditor_history_cache_events",
        "coditor_history_sessions",
        "coditor_history_degraded",
    ];
    for metric in forbidden_metric_families {
        assert!(
            !metrics.contains(metric),
            "metrics.rs must not register non-Envoy Codex metric family {metric}"
        );
    }

    for non_envoy_cause in [
        "\"cache_miss_ttl\"",
        "\"cache_miss_thrash\"",
        "\"context_bloat\"",
        "\"model_fallback\"",
        "\"near_compaction\"",
        "\"tool_failure_streak\"",
        "\"harness_pressure\"",
        "\"compaction_suspected\"",
    ] {
        assert!(
            !metrics.contains(non_envoy_cause),
            "Codex metric labels must not pre-register non-Envoy cause {non_envoy_cause}"
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
