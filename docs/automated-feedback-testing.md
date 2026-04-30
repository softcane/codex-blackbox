# Automated Feedback Testing

This is the real dogfood target: run several Codex sessions through Coditor, then automatically report what worked and what is still missing across watch events, SQLite, Prometheus, Grafana, tools, and MCP telemetry.

This is different from the current fake Envoy e2e. The fake e2e proves the proxy path and Responses parser. The automated feedback test proves Coditor is useful with actual Codex sessions.

## Target Scenario

Run 3-4 Codex sessions through Coditor:

- at least two sessions in the same repo
- at least one session in a different repo
- at least one read-only prompt
- at least one prompt that triggers local tool calls
- at least one prompt that exercises MCP if any MCP servers are configured
- one session with enough context or repeated prompts to show cached input or context status

Each session should be intentionally small and reversible. Prefer prompts that inspect files, list project structure, summarize tests, or read local docs. Do not use destructive prompts for the dogfood harness.

## Proposed Harness

Add a script such as:

```sh
./test/dogfood-codex-sessions.sh --sessions 4 --mode real
```

Suggested options:

- `--mode fake`: use fake OpenAI Responses upstream.
- `--mode real`: run real Codex through the manual OpenAI API-key path.
- `--sessions N`: number of Codex sessions to launch.
- `--repos same|mixed`: run all sessions in one repo or across multiple repos.
- `--include-mcp`: include MCP-oriented prompts when MCP is configured.
- `--keep-stack`: leave Docker Compose running after the test.
- `--report-dir <path>`: write report artifacts to a chosen directory.

## What The Harness Should Do

1. Start the Coditor stack.
2. Verify `coditor-core`, Envoy, Prometheus, and Grafana are reachable.
3. Start a `/watch` capture before launching Codex sessions.
4. Launch 3-4 Codex sessions through `coditor run -- codex ...`.
5. Use prompt fixtures for:
   - read-only repo inspection
   - file search/read
   - local command/tool use
   - MCP use, if configured
6. Wait for sessions to finish.
7. Query Coditor HTTP APIs.
8. Query SQLite.
9. Query Prometheus.
10. Verify Grafana provisioning and dashboard availability.
11. Write a machine-readable and human-readable report.

## Assertions

### Watch Assertions

The captured `/watch` stream should show:

- one `SessionStart` per Codex session
- one `ContextStatus` per completed turn
- no Anthropic TTL/rebuild `CacheEvent` for Codex cached input
- `ModelFallback` only when requested and served model differ
- tool events when tool calls are observed
- MCP events when MCP was exercised and telemetry is available

### SQLite Assertions

When Codex persistence is implemented, SQLite should show:

- distinct session ids for parallel sessions in the same repo
- request rows for each completed turn
- token fields without cached-input double counting
- served model stored when available
- failed/incomplete outcomes represented without panics

If persistence is not implemented yet, the harness should report `missing: codex_sqlite_persistence` rather than failing unclearly.

### Prometheus Assertions

Prometheus should expose:

- Coditor request counters
- input and output token counters
- duration histograms or summaries
- context-status metrics
- model fallback counters when applicable
- no session id labels

If a metric is missing, the report should name the expected metric and mark it as missing.

### Grafana Assertions

The harness should verify:

- Grafana is reachable
- Coditor dashboard provisioning loads
- expected dashboard uid/title exists
- key panels reference existing Prometheus metrics

This can start as static provisioning validation plus HTTP availability. Full visual validation can come later.

Phase 8B adds a narrow local prerequisite for these checks:

```sh
./test/observability-openai-responses.sh
```

That script uses only the fake OpenAI Responses upstream. It starts
Prometheus and Grafana, verifies Codex request/token/context/diagnosis metrics,
checks that metric labels do not contain session ids, and confirms the
provisioned dashboard's Phase 8B panels reference metrics Prometheus has
scraped. It is not the Phase 9C dogfood harness and does not launch real Codex
sessions.

### Tool And MCP Assertions

Tool telemetry should verify:

- read/search/local command prompts produce tool events when Codex exposes them
- failed tools are marked as failures
- MCP prompts produce MCP events when MCP servers are configured
- missing MCP configuration is reported as skipped, not failed

## Report Format

Write artifacts under:

```text
reports/dogfood/<timestamp>/
```

Suggested files:

- `summary.json`: pass/fail/skip per check
- `summary.md`: human-readable feedback
- `watch.ndjson`: captured watch events
- `sessions.json`: `/api/sessions` response
- `diagnosis.json`: diagnosis output per session when available
- `metrics.prom`: Prometheus scrape or selected query output
- `db-checks.json`: SQLite assertion output
- `grafana.json`: dashboard/provisioning assertion output
- `codex-commands.log`: commands run, with secrets redacted
- `compose.log`: relevant Docker Compose logs

The goal is not just pass/fail. The report should say exactly what is left, for example:

```json
{
  "status": "partial",
  "passed": ["watch_session_start", "context_status", "prometheus_no_session_labels"],
  "missing": ["codex_sqlite_persistence", "mcp_event_correlation"],
  "failed": ["grafana_panel_metric_missing"],
  "skipped": ["mcp_prompts_no_servers_configured"]
}
```

## When To Start

Testing already exists at the unit, contract, and fake Envoy levels.

Start real multi-session dogfood testing only after these are true:

- Phase 4C: Codex SQLite persistence/schema mapping exists.
- Phase 6B: `coditor run -- codex ...` can intentionally route Codex through Coditor.
- Phase 9A: fake e2e has expanded enough to cover parallel sessions and failure-open behavior.

For the full version that checks tools, MCP, diagnosis, Prometheus, and Grafana, also complete:

- Phase 6C: watch/tmux Codex rendering polish.
- Phase 7: Codex hook/tool/MCP telemetry.
- Phase 8: diagnostics, observability validation, rate-limit boundary, and
  context intelligence.

## Minimal First Dogfood

The earliest useful real test is smaller:

- one same-repo Codex session
- one different-repo Codex session
- read-only prompts only
- assert `/watch` has `SessionStart` and `ContextStatus`
- assert no Anthropic `CacheEvent` TTL/rebuild fields
- assert Prometheus has basic request/token metrics

That can start after Phase 6B, but it should be labeled `smoke`, not full dogfood.

## Full Dogfood Gate

The full automated feedback harness is ready when it can run:

```sh
./test/dogfood-codex-sessions.sh --sessions 4 --repos mixed --include-mcp --mode real
```

and produce a report that answers:

- Did every session appear in `/watch`?
- Were parallel sessions kept distinct?
- Did DB rows match watch sessions?
- Did token accounting avoid cached-input double counting?
- Did Prometheus expose expected metrics without session labels?
- Did Grafana load dashboards backed by real metrics?
- Did tool and MCP events appear when expected?
- What is still missing, skipped, or failing?
