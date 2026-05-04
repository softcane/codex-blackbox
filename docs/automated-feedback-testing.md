# Automated Feedback Testing

This is the real dogfood target: run several Codex sessions through Coditor, then automatically report what worked and what is still missing across Envoy-observed watch events, SQLite, Prometheus, and Grafana.

This is different from the current fake Envoy e2e. The fake e2e proves the proxy path and Responses parser. The automated feedback test proves Coditor is useful with actual Codex sessions.

## Target Scenario

Run 3-4 Codex sessions through Coditor:

- at least two sessions in the same repo
- at least one session in a different repo
- at least one read-only prompt
- at least one prompt that may trigger local activity, without treating that
  activity as Codex telemetry unless it appears in Envoy-observed Responses
  traffic
- one session with enough context or repeated prompts to show cached input or context status

Each session should be intentionally small and reversible. Prefer prompts that inspect files, list project structure, summarize tests, or read local docs. Do not use destructive prompts for the dogfood harness.

## Harness

The real harness is:

```sh
./test/dogfood-codex-sessions.sh --sessions 4 --mode real
```

Suggested options:

- `--mode fixture`: use fake OpenAI Responses fixtures without launching Codex.
- `--mode real`: run real Codex through the ChatGPT/Codex subscription proxy path.
- `--sessions N`: number of Codex sessions to launch.
- `--repos same|mixed`: run all sessions in one repo or across multiple repos.
- `--include-mcp`: include MCP-oriented prompts as workload only when MCP is
  configured; lifecycle telemetry remains outside the Codex correctness
  surface.
- `--keep-stack`: leave Docker Compose running after the test.
- `--report-dir <path>`: write report artifacts to a chosen directory.

`--mode real` uses the local Codex ChatGPT login and can contact
`chatgpt.com`. It runs the subscription preflight first, starts `/watch`
capture, launches real `codex exec` sessions through `coditor run -- codex`,
then writes `summary.json`, `summary.md`, `/watch`, SQLite, Prometheus,
Grafana, command, and Compose artifacts under `reports/dogfood/<timestamp>/`.

`--mode fixture` delegates to `./test/e2e-openai-responses-full.sh` and is only
a Phase 9A fake regression convenience. It is not a real dogfood substitute.

First live result: on 2026-04-30 UTC / 2026-05-01 Europe/Stockholm, the harness
ran four real Codex 0.125.0 sessions and produced a `partial` report. Local
stdout-derived lifecycle observations from later calibration are quarantined
from the Codex product surface. The detailed log is in
`docs/real-codex-smoke.md`; the broader calibration report is under
`reports/live-codex-validation-20260430T234815Z/`.

## What The Harness Should Do

1. Start the Coditor stack.
2. Verify `coditor-core`, Envoy, Prometheus, and Grafana are reachable.
3. Start a `/watch` capture before launching Codex sessions.
4. Launch 3-4 Codex sessions through `coditor run -- codex ...`.
   Redirect each child process stdin from `/dev/null`; `codex exec` may read
   inherited stdin, and validation manifests must not be consumable by child
   sessions.
5. Use prompt fixtures for:
   - read-only repo inspection
   - file search/read
   - local command use, with no telemetry claim unless Envoy observes it
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
- no cache-event telemetry for Codex cached input
- `ModelFallback` only when requested and served model differ
- `CodexTurnSummary` status and token fields for model turns

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

- Coditor request counters with `provider="codex_responses"`
- input, cached-input, uncached-input, output, reasoning-output, and total token counters
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

Phase 9A adds the broader fake regression gate:

```sh
./test/e2e-openai-responses-full.sh
```

That script still uses only the fake OpenAI Responses upstream. It covers
parallel fake sessions with mixed cwd metadata, completed/failed/incomplete
Responses streams, split SSE chunking through Envoy, late `/watch` replay,
SQLite Codex persistence, Prometheus/Grafana provisioning, subscription-mode
CLI dry-run output, and Envoy failure-open behavior after `coditor-core` is
stopped. It is the fake prerequisite for Phase 9B, not a real Codex dogfood
harness.

The live Phase 9B direction is ChatGPT/Codex subscription auth. Use the default
`docker-compose.yml`, `envoy/envoy.yaml`, and the manual preflight:

```sh
cargo run -q -p coditor-cli -- preflight codex-subscription -- codex exec \
  --cd /Users/pradeepsingh/code/coditor \
  --sandbox read-only \
  "Read AGENTS.md and docs/remaining-phases.md, then summarize the current next phase in 3 bullets. Do not edit files."
```

The preflight starts the subscription-mode stack and prints the live command.
Use it before future live runs to confirm the config without launching a Codex
turn.

After explicit approval and a passing Phase 9A fake gate, the smallest real
smoke can be run through the same harness:

```sh
./test/dogfood-codex-sessions.sh --mode real --sessions 1 --repos same \
  --report-dir reports/dogfood/smoke-$(date -u +%Y%m%dT%H%M%SZ)
```

Record future smoke outcomes in `docs/real-codex-smoke.md` with the date, Codex
version, command/config, observed events, rollback command, and limitations.

### Envoy Tool Intent Assertions

Tool checks are limited to facts present in Envoy-observed Responses traffic:

- custom tool-call intent is reported when the Responses stream contains it
- local command results and tool failures are not reported as Codex truth
- MCP lifecycle events are skipped unless a future Responses field proves them

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
  "missing": ["codex_sqlite_persistence", "codex_turn_summary"],
  "failed": ["grafana_panel_metric_missing"],
  "skipped": ["non_envoy_lifecycle_telemetry"]
}
```

## When To Start

Testing already exists at the unit, contract, and fake Envoy levels.

Start real multi-session dogfood testing only after these are true:

- Phase 4C: Codex SQLite persistence/schema mapping exists.
- Phase 6B: `coditor run -- codex ...` can intentionally route Codex through Coditor.
- Phase 9A: fake e2e has expanded enough to cover parallel sessions,
  failed/incomplete streams, watch replay, persistence, observability, CLI
  dry-run, and failure-open behavior.

Those minimum gates were satisfied before the first live run recorded in
`docs/real-codex-smoke.md`.

For the full version that checks Envoy-derived response facts, diagnosis,
Prometheus, and Grafana, also complete:

- Phase 6C: watch/tmux Codex rendering polish.
- Phase 8: diagnostics, observability validation, rate-limit boundary, and
  context intelligence.

## Minimal First Dogfood

The earliest useful real test is smaller:

- one same-repo Codex session
- one different-repo Codex session
- read-only prompts only
- assert `/watch` has `SessionStart` and `ContextStatus`
- assert no cache-event telemetry for Codex cached input
- assert Prometheus has basic request/token metrics

That can start after Phase 6B, but it should be labeled `smoke`, not full dogfood.

## Full Dogfood Gate

The full automated feedback harness is ready when it can run:

```sh
./test/dogfood-codex-sessions.sh --sessions 4 --repos mixed --mode real --no-json
```

and produce a report that answers:

- Did every session appear in `/watch`?
- Were parallel sessions kept distinct?
- Did DB rows match watch sessions?
- Did token accounting avoid cached-input double counting?
- Did Prometheus expose expected metrics without session labels?
- Did Grafana load dashboards backed by real metrics?
- Did response status, token, model, and context signals appear from Envoy?
- What is still missing, skipped, or failing?

The current target intentionally excludes local stdout, hook, terminal, session
file, and app-server side channels from live Codex telemetry.
