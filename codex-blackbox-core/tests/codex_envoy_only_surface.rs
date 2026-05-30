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
        "codex-blackbox-cli/src",
        "codex-blackbox-core/src",
        "codex-blackbox-core/tests",
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
        if path == PathBuf::from("codex-blackbox-core/tests/codex_envoy_only_surface.rs") {
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
fn project_surface_does_not_use_previous_project_name() {
    let forbidden_terms = [
        ["codi", "tor"].concat(),
        ["Codi", "tor"].concat(),
        ["CODI", "TOR"].concat(),
    ];

    for path in collect_scanned_files() {
        if path == PathBuf::from("codex-blackbox-core/tests/codex_envoy_only_surface.rs") {
            continue;
        }
        let relative = path.to_string_lossy();
        for forbidden in &forbidden_terms {
            assert!(
                !relative.contains(forbidden),
                "{} must not retain the previous project name",
                path.display()
            );
        }

        let body = read_repo_file(path.to_str().expect("utf8 relative path"));
        for forbidden in &forbidden_terms {
            assert!(
                !body.contains(forbidden),
                "{} must not expose the previous project name",
                path.display()
            );
        }
    }
}

#[test]
fn codex_target_path_does_not_depend_on_local_json_stdout() {
    let cli = read_repo_file("codex-blackbox-cli/src/main.rs");
    for forbidden in [
        "codex_json_stdout",
        "codex-blackbox-cli-json",
        "codex_blackbox.codex_hook.v1",
        "/api/hooks/codex",
    ] {
        assert!(
            !cli.contains(forbidden),
            "normal Codex wrapper path must not use {forbidden}"
        );
    }

    let core = read_repo_file("codex-blackbox-core/src/main.rs");
    for forbidden in [
        "codex_hook",
        "codex-blackbox-cli-json",
        "codex_blackbox.codex_hook.v1",
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
fn custom_companion_browser_ui_is_not_served() {
    let core = read_repo_file("codex-blackbox-core/src/main.rs");
    for forbidden in [
        "COMPANION_HTML",
        "handle_companion_ui",
        ".route(\"/companion\"",
    ] {
        assert!(
            !core.contains(forbidden),
            "calibrated v1 must not serve the removed Companion browser UI via {forbidden}"
        );
    }

    assert!(
        core.contains("\"/api/companion/sessions\""),
        "redacted companion JSON APIs should remain for internal tests and future integrations"
    );
}

#[test]
fn codex_http_and_watch_surface_does_not_expose_removed_cache_or_quota_products() {
    let core = read_repo_file("codex-blackbox-core/src/main.rs");
    for forbidden in [
        "/api/cache-rebuilds",
        "handle_cache_rebuilds",
        "RateLimitStatus",
        "CacheEvent",
        "CacheWarning",
        "quota_burn_monitor",
        "CODEX_BLACKBOX_WEEKLY_TOKEN_BUDGET",
        "CODEX_BLACKBOX_CACHE_WARNING_SECS",
    ] {
        assert!(
            !core.contains(forbidden),
            "Codex product surface must not retain removed cache/quota path {forbidden}"
        );
    }

    let watch = read_repo_file("codex-blackbox-core/src/watch.rs");
    for forbidden in [
        "RateLimitStatus",
        "ToolResult",
        "SkillEvent",
        "McpEvent",
        "CacheEvent",
        "CacheWarning",
    ] {
        assert!(
            !watch.contains(forbidden),
            "watch events must not expose removed lifecycle/cache state {forbidden}"
        );
    }

    let cli = read_repo_file("codex-blackbox-cli/src/main.rs");
    for forbidden in [
        "RateLimitStatus",
        "ToolResult",
        "SkillEvent",
        "McpEvent",
        "CacheEvent",
        "CacheWarning",
        "QUOTA",
        "tokens_used_this_week",
        "--no-cache",
    ] {
        assert!(
            !cli.contains(forbidden),
            "watch rendering must not expose removed lifecycle/cache/quota field {forbidden}"
        );
    }

    let tmux = read_repo_file("codex-blackbox-cli/src/tmux.rs");
    for forbidden in [
        "RateLimitStatus",
        "ToolResult",
        "SkillEvent",
        "McpEvent",
        "CacheEvent",
        "CacheWarning",
        "QUOTA",
        "tokens_used_this_week",
        "--no-cache",
    ] {
        assert!(
            !tmux.contains(forbidden),
            "tmux rendering must not expose removed lifecycle/cache/quota field {forbidden}"
        );
    }

    let agents = read_repo_file("AGENTS.md");
    for forbidden in [
        "Codex hook payloads may create provisional",
        "tool failures,\n  MCP events",
        "`CacheEvent` behavior",
    ] {
        assert!(
            !agents.contains(forbidden),
            "repo instructions must not re-authorize removed telemetry: {forbidden}"
        );
    }
}

#[test]
fn codex_db_surface_does_not_persist_legacy_cache_or_lifecycle_evidence() {
    let core = read_repo_file("codex-blackbox-core/src/main.rs");
    for forbidden in [
        "RecordRequest",
        "WriteTurnSnapshot",
        "cache_hit_ratio: f64",
        "outcome, total_turns, total_cost, cache_hit_ratio",
        "CREATE TABLE IF NOT EXISTS tool_outcomes",
        "CREATE TABLE IF NOT EXISTS skill_events",
        "CREATE TABLE IF NOT EXISTS mcp_events",
        "DELETE FROM tool_outcomes",
    ] {
        assert!(
            !core.contains(forbidden),
            "Codex DB evidence surface must not retain legacy cache/lifecycle writer path {forbidden}"
        );
    }

    for required_migration in [
        "ALTER TABLE session_diagnoses DROP COLUMN cache_hit_ratio",
        "DROP TABLE IF EXISTS tool_outcomes",
        "DROP TABLE IF EXISTS skill_events",
        "DROP TABLE IF EXISTS mcp_events",
    ] {
        assert!(
            core.contains(required_migration),
            "Codex DB repair must remove legacy evidence surface: {required_migration}"
        );
    }

    let persisted_turn_loader_start = core
        .find("fn load_turn_snapshots_from_db")
        .expect("turn snapshot loader start");
    let persisted_turn_loader_end = core[persisted_turn_loader_start..]
        .find("#[derive(Debug)]\nstruct PersistedWatchSession")
        .map(|offset| persisted_turn_loader_start + offset)
        .expect("turn snapshot loader end");
    let persisted_turn_loader = &core[persisted_turn_loader_start..persisted_turn_loader_end];
    assert!(
        persisted_turn_loader.contains("provider = 'codex_responses'"),
        "fresh diagnosis must load only Envoy-backed Codex turn snapshots"
    );

    let persisted_diagnosis_start = core
        .find("fn persist_session_diagnosis_report")
        .expect("diagnosis persistence start");
    let persisted_diagnosis_end = core[persisted_diagnosis_start..]
        .find("fn build_fresh_diagnosis_report")
        .map(|offset| persisted_diagnosis_start + offset)
        .expect("diagnosis persistence end");
    let persisted_diagnosis = &core[persisted_diagnosis_start..persisted_diagnosis_end];
    assert!(
        persisted_diagnosis.contains("codex_envoy_diagnosis_report"),
        "stored diagnosis rows must be filtered to Envoy-derived Codex causes before persistence"
    );
    assert!(
        core.contains("fn repair_session_diagnosis_envoy_causes")
            && core.contains("repair_session_diagnosis_envoy_causes(conn)"),
        "DB repair must normalize existing stored diagnosis rows to Envoy-derived Codex causes"
    );

    let watch_sessions_start = core
        .find("fn load_persisted_watch_sessions")
        .expect("watch session loader start");
    let watch_sessions_end = core[watch_sessions_start..]
        .find("fn load_persisted_watch_turns")
        .map(|offset| watch_sessions_start + offset)
        .expect("watch session loader end");
    let watch_sessions = &core[watch_sessions_start..watch_sessions_end];
    assert!(
        !watch_sessions.contains("request_count > 0")
            && !watch_sessions.contains("s.model")
            && watch_sessions.contains("t.provider = 'codex_responses'"),
        "watch replay must start from persisted Codex turn evidence, not generic request_count"
    );

    let latest_summary_start = core
        .find("fn latest_response_summary_from_db")
        .expect("latest response summary helper start");
    let latest_summary_end = core[latest_summary_start..]
        .find("fn diagnosis_outcome_needs_refresh")
        .map(|offset| latest_summary_start + offset)
        .expect("latest response summary helper end");
    let latest_summary_helper = &core[latest_summary_start..latest_summary_end];
    assert!(
        latest_summary_helper.contains("provider = 'codex_responses'"),
        "session recall repair must only persist Codex-backed response summaries"
    );
    assert!(
        core.contains("fn repair_session_recall_codex_summaries")
            && core.contains("repair_session_recall_codex_summaries(conn)"),
        "DB repair must normalize existing session recall summaries to Codex-backed evidence"
    );

    let sessions_start = core
        .find("fn load_recent_codex_session_rows")
        .expect("recent sessions loader start");
    let sessions_end = core[sessions_start..]
        .find("async fn handle_sessions")
        .map(|offset| sessions_start + offset)
        .expect("recent sessions loader end");
    let sessions_loader = &core[sessions_start..sessions_end];
    assert!(
        sessions_loader.contains("r_codex.provider = 'codex_responses'")
            && sessions_loader.contains("t_codex.provider = 'codex_responses'")
            && !sessions_loader.contains("s.model"),
        "/api/sessions must start from Codex-backed session evidence and model fields"
    );

    let summary_start = core.find("fn query_summary").expect("summary query start");
    let summary_end = core[summary_start..]
        .find("fn summary_window_json")
        .map(|offset| summary_start + offset)
        .expect("summary query end");
    let summary_query = &core[summary_start..summary_end];
    assert!(
        summary_query.contains("r.provider = 'codex_responses'"),
        "/api/summary must aggregate only Envoy-backed Codex request rows"
    );

    let cost_start = core
        .find("fn compute_estimated_costs_for_sessions")
        .expect("cost helper start");
    let cost_end = core[cost_start..]
        .find("fn query_summary")
        .map(|offset| cost_start + offset)
        .expect("cost helper end");
    let cost_helper = &core[cost_start..cost_end];
    assert!(
        cost_helper.contains("provider = 'codex_responses'")
            && cost_helper.contains("estimate_codex_api_cost_dollars")
            && !cost_helper.contains("cache_creation_tokens"),
        "Codex session cost helper must not aggregate generic or legacy-cache request rows"
    );
    assert!(
        core.contains("local_estimate_total_cost_dollars")
            && core.contains("local_estimate_cost_dollars")
            && core.contains("local_estimate_cost_source")
            && core.contains("local_estimate_trusted_for_budget_enforcement"),
        "Codex cost API fields must be labeled as local estimates, not Envoy correctness truth"
    );
    assert!(
        core.contains("estimated_total_cost_dollars") && core.contains("estimated_cost_dollars"),
        "legacy estimated cost API fields may remain only as compatibility aliases"
    );

    let recall_start = core
        .find("async fn handle_recall")
        .expect("recall handler start");
    let recall_end = core[recall_start..]
        .find("fn load_degradation_view_from_db")
        .map(|offset| recall_start + offset)
        .expect("recall handler end");
    let recall_handler = &core[recall_start..recall_end];
    assert!(
        recall_handler.contains("req_evidence.provider = 'codex_responses'")
            && recall_handler.contains("turn_evidence.provider = 'codex_responses'")
            && !recall_handler.contains("s.model"),
        "/api/recall must return only Codex-backed session recall rows and model fields"
    );

    let degradation_start = core
        .find("fn load_degradation_view_from_db")
        .expect("degradation view loader start");
    let degradation_end = core[degradation_start..]
        .find("async fn handle_degradation")
        .map(|offset| degradation_start + offset)
        .expect("degradation view loader end");
    let degradation_loader = &core[degradation_start..degradation_end];
    assert!(
        degradation_loader.contains("provider = 'codex_responses'")
            && !degradation_loader.contains("codex_status IS NOT NULL")
            && !degradation_loader.contains("codex_cached_input_tokens > 0")
            && !degradation_loader.contains("codex_reasoning_output_tokens > 0")
            && !degradation_loader.contains("codex_accounting_anomalies IS NOT NULL"),
        "/api/degradation must use provider-scoped Codex evidence only"
    );
    assert!(
        core.contains("fn is_codex_envoy_degrading_cause")
            && core.contains("fn codex_envoy_public_degradation")
            && degradation_loader.contains("\"heuristic_signals\": heuristic_signals"),
        "/api/diagnosis and /api/degradation must share a Codex Envoy-only degradation contract while exposing heuristic signals separately"
    );

    assert!(
        core.contains("/api/postmortem/last")
            && core.contains("/api/postmortem/:session_id")
            && core.contains("postmortem::PostmortemTarget::Last")
            && core.contains("postmortem_redact_param"),
        "postmortem API routes must be explicit Codex routes with redaction control"
    );

    let postmortem = read_repo_file("codex-blackbox-core/src/postmortem.rs");
    assert!(
        postmortem.contains("const PROVIDER: &str = \"codex_responses\"")
            && postmortem.contains("WHERE t.session_id = ?1 AND t.provider = ?2")
            && postmortem.contains("FROM requests WHERE provider = ?1"),
        "postmortem reports must be provider-scoped to Codex Responses evidence"
    );
    for forbidden in [
        "ToolResult",
        "tool_result",
        "McpEvent",
        "MCP lifecycle",
        "SkillEvent",
        "skill lifecycle",
        "cache TTL",
        "cache rebuild",
        "quota",
        "provider cap",
        "codex_hook",
        "json stdout",
    ] {
        assert!(
            !postmortem.contains(forbidden),
            "postmortem surface must not expose unsupported Codex evidence claim {forbidden}"
        );
    }
}

#[test]
fn historical_monitor_does_not_compute_removed_cache_cost_or_tool_surfaces() {
    let core = read_repo_file("codex-blackbox-core/src/main.rs");
    let start = core
        .find("fn query_historical_window_from_db")
        .expect("historical query start");
    let end = core[start..]
        .find("fn query_historical_metrics")
        .map(|offset| start + offset)
        .expect("historical query end");
    let body = &core[start..end];

    for forbidden in [
        "tool_outcomes",
        "cache_event",
        "cache_read_tokens",
        "cache_creation_tokens",
        "estimate_cost_dollars",
        "estimated_cache",
    ] {
        assert!(
            !body.contains(forbidden),
            "historical monitor must not compute removed non-Envoy surface {forbidden}"
        );
    }
}

#[test]
fn core_metrics_register_only_envoy_derived_codex_families() {
    let metrics_source = read_repo_file("codex-blackbox-core/src/metrics.rs");
    let metrics = metrics_source
        .split("#[cfg(test)]")
        .next()
        .expect("metrics source before tests");
    let allowed_metric_families = [
        "codex_blackbox_requests_total",
        "codex_blackbox_tokens_total",
        "codex_blackbox_sessions_degraded_total",
        "codex_blackbox_codex_response_status_total",
        "codex_blackbox_model_fallback_total",
        "codex_blackbox_tool_calls_total",
        "codex_blackbox_turn_duration_seconds",
        "codex_blackbox_context_fill_percent",
    ];
    for metric in allowed_metric_families {
        assert!(
            metrics.contains(metric),
            "Envoy-derived metric family missing from metrics.rs: {metric}"
        );
    }

    let forbidden_metric_families = [
        "codex_blackbox_estimated_cost_dollars_total",
        "codex_blackbox_estimated_session_cost_dollars",
        "codex_blackbox_estimated_cache_waste_dollars_total",
        "codex_blackbox_cache_events_total",
        "codex_blackbox_tool_failures_total",
        "codex_blackbox_mcp_tool_calls_total",
        "codex_blackbox_mcp_tool_failures_total",
        "codex_blackbox_mcp_server_calls_total",
        "codex_blackbox_mcp_server_failures_total",
        "codex_blackbox_mcp_events_total",
        "codex_blackbox_skill_events_total",
        "codex_blackbox_active_sessions",
        "codex_blackbox_weekly_tokens_used",
        "codex_blackbox_weekly_tokens_remaining",
        "codex_blackbox_weekly_token_budget",
        "codex_blackbox_projected_exhaustion_seconds",
        "codex_blackbox_history_estimated_spend",
        "codex_blackbox_history_tool_failures",
        "codex_blackbox_history_cache_events",
        "codex_blackbox_history_sessions",
        "codex_blackbox_history_degraded",
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

    for non_envoy_token_kind in ["\"cache_read\"", "\"cache_create\""] {
        assert!(
            !metrics.contains(non_envoy_token_kind),
            "Codex metric labels must not pre-register non-Envoy token kind {non_envoy_token_kind}"
        );
    }

    assert!(
        metrics.contains("\"custom_tool_call\"")
            && metrics.contains("\"named_tool\"")
            && metrics.contains("trimmed.starts_with(\"custom_tool_call:\")"),
        "tool intent metric labels must collapse provider-generated tool names and item ids"
    );
}

#[test]
fn codex_dashboard_contains_only_envoy_derived_codex_metrics() {
    let dashboard = read_repo_file("grafana/dashboards/codex-blackbox.json");
    let parsed: Value = serde_json::from_str(&dashboard).expect("dashboard JSON");
    let panels = parsed
        .get("panels")
        .and_then(Value::as_array)
        .expect("dashboard panels");
    let forbidden_metric_families = [
        "codex_blackbox_skill_events_total",
        "codex_blackbox_mcp_events_total",
        "codex_blackbox_history_tool_failures",
        "codex_blackbox_active_sessions",
        "codex_blackbox_weekly_tokens",
        "codex_blackbox_history_estimated_spend",
        "codex_blackbox_history_sessions",
        "codex_blackbox_history_degraded",
        "codex_blackbox_hook_events_total",
        "codex_blackbox_validation_runs_total",
        "codex_blackbox_loop_signals_total",
        "codex_blackbox_coach_actions_total",
        "codex_blackbox_baseline_builds_total",
        "codex_blackbox_unvalidated_edit_signals_total",
    ];
    let forbidden_query_terms = ["cache_read", "cache_create"];
    let forbidden_panel_terms = [
        "Tool failures",
        "Skill lifecycle",
        "MCP lifecycle",
        "tool success",
        "Tool success",
        "succeeded",
        "Active sessions",
        "Coach",
        "coach",
        "hook",
        "Hook",
        "validation",
        "Validation",
        "baseline",
        "Baseline",
        "tmux",
        "preflight",
        "reconcile",
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
                for term in forbidden_query_terms {
                    assert!(
                        !expr.contains(term),
                        "Codex dashboard panel {title:?} must not query non-Envoy token kind {term}"
                    );
                }
            }
        }
    }

    assert!(
        dashboard.contains("codex_blackbox_codex_response_status_total"),
        "dashboard must use true Envoy-derived Codex response status counters"
    );
    for term in [
        "Coach",
        "coach",
        "hook",
        "Hook",
        "validation",
        "Validation",
        "baseline",
        "Baseline",
        "tmux",
        "preflight",
        "reconcile",
        "tool success",
        "succeeded",
    ] {
        assert!(
            !dashboard.contains(term),
            "dashboard must not expose disabled feature term {term:?}"
        );
    }
}

#[test]
fn codex_dashboard_panels_have_clear_enabled_query_contracts() {
    let dashboard = read_repo_file("grafana/dashboards/codex-blackbox.json");
    let parsed: Value = serde_json::from_str(&dashboard).expect("dashboard JSON");
    let panels = parsed
        .get("panels")
        .and_then(Value::as_array)
        .expect("dashboard panels");

    let mut titles = Vec::new();
    let mut expressions = Vec::new();
    for panel in panels {
        let title = panel.get("title").and_then(Value::as_str).unwrap_or("");
        let description = panel
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("");
        assert!(!title.trim().is_empty(), "dashboard panel missing title");
        assert!(
            !description.trim().is_empty(),
            "dashboard panel {title:?} missing plain-English description"
        );
        titles.push(title.to_string());

        if let Some(targets) = panel.get("targets").and_then(Value::as_array) {
            assert!(
                description.contains("Zero means")
                    || description.contains("No data means")
                    || description.contains("Empty means"),
                "dashboard panel {title:?} must explain its empty or zero state"
            );
            for target in targets {
                let expr = target.get("expr").and_then(Value::as_str).unwrap_or("");
                assert!(
                    !expr.trim().is_empty(),
                    "dashboard panel {title:?} target missing PromQL"
                );
                if panel.get("type").and_then(Value::as_str) == Some("timeseries") {
                    assert!(
                        expr.contains("rate(") || expr.contains("increase("),
                        "time-series panel {title:?} must use range/rate semantics: {expr}"
                    );
                }
                expressions.push(expr.to_string());
            }
        }
    }

    for required_title in [
        "Terminology Guide",
        "Requests",
        "Completed",
        "Failed",
        "Incomplete",
        "Unknown",
        "Cached Input",
        "Requests Per Minute",
        "Turn Latency P95",
        "Context Fill P95",
        "Tokens By Model",
        "Token Components",
        "Model Fallback",
        "Tool-Call Intent",
        "Guard Blocks",
        "Diagnostic Causes",
    ] {
        assert!(
            titles.iter().any(|title| title == required_title),
            "dashboard missing expected panel {required_title:?}"
        );
    }
    assert!(
        !titles.iter().any(|title| title == "Diagnostic Cause Guide"),
        "dashboard must not include the removed diagnostic guide text panel"
    );
    let panel_by_title = |want: &str| {
        panels
            .iter()
            .find(|panel| panel.get("title").and_then(Value::as_str) == Some(want))
            .unwrap_or_else(|| panic!("dashboard missing panel {want:?}"))
    };
    for (title, x, y) in [
        ("Requests", 0, 4),
        ("Completed", 8, 4),
        ("Failed", 16, 4),
        ("Incomplete", 0, 8),
        ("Unknown", 8, 8),
        ("Cached Input", 16, 8),
    ] {
        let panel = panel_by_title(title);
        let grid = panel
            .get("gridPos")
            .and_then(Value::as_object)
            .expect("top-card grid");
        assert_eq!(grid.get("x").and_then(Value::as_i64), Some(x), "{title} x");
        assert_eq!(grid.get("y").and_then(Value::as_i64), Some(y), "{title} y");
        assert_eq!(
            grid.get("w").and_then(Value::as_i64),
            Some(8),
            "{title} width"
        );
    }
    for title in [
        "Requests Per Minute",
        "Turn Latency P95",
        "Context Fill P95",
    ] {
        assert_eq!(
            panel_by_title(title).get("type").and_then(Value::as_str),
            Some("timeseries"),
            "{title} should use a time-series visualization"
        );
    }

    assert!(
        dashboard.contains("Since core start")
            && dashboard.contains("current `codex-blackbox-core` process lifetime"),
        "dashboard must define current-process lifetime semantics for since-start panels"
    );
    assert!(
        expressions
            .iter()
            .any(|expr| expr.contains("codex_blackbox_turn_duration_seconds_bucket")),
        "dashboard must include a latency panel backed by turn duration metrics"
    );
    assert!(
        expressions
            .iter()
            .any(|expr| expr.contains("codex_blackbox_context_fill_percent_bucket")),
        "dashboard must include a context fill panel backed by context histogram metrics"
    );
    assert!(
        expressions
            .iter()
            .any(|expr| expr.contains("codex_blackbox_tool_calls_total")),
        "dashboard must expose proxy-observed tool-call intent"
    );
    assert!(
        dashboard.contains("Tool-call intent")
            && !dashboard.contains("tool success")
            && !dashboard.contains("succeeded"),
        "tool panels must say intent without implying local tool success"
    );
    assert!(
        !expressions
            .iter()
            .any(|expr| expr.contains("estimated_cost") || expr.contains("cost_dollars")),
        "dashboard must not graph local/untrusted cost estimates"
    );
    let token_component_query = expressions
        .iter()
        .find(|expr| {
            expr.contains("kind=~\"input|uncached_input|cached_input|output|reasoning_output\"")
        })
        .expect("token component query");
    assert!(
        !token_component_query.contains("|total"),
        "token component panel must not invite adding local total to its parts"
    );
    let diagnostic_query = expressions
        .iter()
        .find(|expr| expr.contains("codex_blackbox_sessions_degraded_total"))
        .expect("diagnostic query");
    for cause in [
        "codex_response_failed",
        "codex_response_incomplete",
        "codex_model_mismatch",
        "codex_high_context_fill",
        "codex_high_reasoning_share",
        "codex_accounting_anomaly",
        "codex_low_cached_input_reuse",
    ] {
        assert!(
            diagnostic_query.contains(cause) && dashboard.contains(cause),
            "dashboard diagnostic surface missing postmortem cause {cause}"
        );
    }
    assert!(
        dashboard.contains("no new Codex turn has passed through this core yet")
            || dashboard.contains("no new Codex turn has passed through this process yet"),
        "terminology guide must explain why all current-process counters can be zero after restart"
    );
}
