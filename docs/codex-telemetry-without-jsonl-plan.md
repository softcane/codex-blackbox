# Codex Envoy-Only Telemetry Plan

Checkpoint: `22b6357 fix live codex telemetry and session identity`.

Goal: make Coditor a Codex-only observability proxy whose product-facing data is
limited to facts proven at the Envoy-observed Codex request/response layer.

If a data point cannot be produced from Envoy-observed Codex traffic, or from a
small deterministic calculation over that traffic, remove it from the
product-facing Codex surface.

## Hard Constraints

- No legacy-provider-specific paths, labels, config, dashboards, tests, docs, or
  user-facing claims in Coditor.
- No `codex exec --json` in the target validation path.
- No local Codex JSONL stdout parsing for production/live telemetry.
- No fake local tool, MCP, or skill truth from CLI stdout, hooks, terminal UI,
  inferred text, or local session files.
- No OpenAI API-key path for live Codex subscription validation.
- No zero-as-truth for unsupported data. If Envoy cannot prove it, remove the
  product-facing panel/metric/API field.
- No Phase 9C dogfood harness until the Envoy-only correctness surface is clean.

## What Stays

Keep data that is available from Envoy-observed Codex Responses traffic.

| Data point | Source | Confidence |
|---|---|---:|
| Codex request count | Envoy request plus finalized response | High |
| Request path/route | Envoy headers | High |
| Requested model | Responses request body `model` | High |
| Served model | response headers or Responses payload model | High |
| Response id | Responses SSE response metadata | High when present |
| Response status | Responses terminal event: completed, failed, incomplete, unknown | High |
| Prompt excerpt | Responses request body visible user input | Medium-High |
| Session id | explicit header/client metadata, else deterministic fallback hash | Medium |
| Repo/display name | cwd/client metadata/request preamble when present | Medium |
| Input tokens | Responses `usage.input_tokens` | High |
| Cached input tokens | Responses `usage.input_tokens_details.cached_tokens` | High |
| Uncached input tokens | `input - cached_input` | High |
| Output tokens | Responses `usage.output_tokens` | High |
| Reasoning output tokens | Responses `usage.output_tokens_details.reasoning_tokens` | High |
| Local total tokens | `input + output` | High |
| Token accounting anomalies | deterministic checks over usage fields | High |
| Context fill | input tokens divided by verified context window | Medium |
| Model mismatch/fallback | requested model compared with served model | Medium |
| Assistant output text | Responses output text stream/payload | High when surfaced |
| Custom tool-call intent | Responses `custom_tool_call` item if present | Medium |

## What Gets Dropped

Drop these from Codex product-facing Grafana, Prometheus, DB/API claims, and
watch/tmux summaries unless Envoy-observed Codex traffic explicitly contains the
fact.

| Data point | Action | Reason |
|---|---|---|
| Local shell command result | Drop | Envoy cannot prove local process exit/outcome. |
| Tool result success/failure | Drop | Current Envoy parser sees at most model-side intent, not local result. |
| MCP lifecycle | Drop | Envoy cannot currently prove MCP call/result/denial lifecycle. |
| Skill lifecycle | Drop | Envoy cannot prove skill expected/fired/missed/failed lifecycle. |
| Permission decisions | Drop | Not present in the Envoy-observed model stream. |
| Cache-event panels copied from non-Codex semantics | Drop | Codex cached input is already represented by Responses usage. |
| Tool/MCP/skill failure panels | Drop | Not Envoy-proven. |
| Tool/MCP/skill DB correctness claims | Drop | Current rows are not Envoy-only proof. |
| Recall quality based on final answer | Drop until response summaries are persisted from Envoy | Current Codex persistence does not reliably surface final response text. |
| Estimated spend as Codex truth | Drop from correctness dashboard | Cost needs pricing/reconciliation outside Envoy stream. |
| Weekly cap/quota truth | Drop from Codex correctness dashboard | Provider quota/cap is not in Envoy stream. |
| Active sessions as durable truth | Drop or label process-local only | It is in-memory state, not Envoy truth. |

Cost can return later as a separate "local estimate" feature, but it must not be
part of the Envoy-only correctness dashboard.

## Required Removals

1. Remove legacy-provider-specific naming and copied semantics from Codex-facing code,
   tests, docs, and dashboards.
2. Remove Codex-facing dashboard panels for tool/MCP/skill lifecycle unless they
   are backed by Envoy-observed Codex schema fields.
3. Remove Codex-facing cache-event panels that use non-Codex cache semantics.
4. Remove or quarantine JSONL-derived telemetry from normal `coditor run -- codex`
   behavior.
5. Remove user-facing claims that fake/API-key paths prove live Codex
   subscription behavior.
6. Remove Codex correctness claims from historical gauges that aggregate all
   providers without an Envoy-derived Codex filter.

## Required Changes

1. Make the normal Codex wrapper run without `--json` and without stdout
   telemetry parsing.
2. Keep Envoy routing as the single live telemetry source.
3. Make Prometheus Codex metrics provider-filtered and Envoy-derived.
4. Make Grafana Codex panels query only Envoy-derived Codex metrics.
5. Persist only Envoy-derived Codex facts as Codex truth:
   - request id
   - session id/source
   - requested model
   - served model
   - status
   - response id
   - prompt excerpt
   - token fields
   - context fill inputs
   - custom tool-call intent, if present
6. Expose requested and served model separately in APIs where model identity is
   shown.
7. Persist assistant response summaries from Envoy if recall/watch should show
   final Codex output.
8. Add tests that prove the target path has no `--json` dependency.
9. Add tests that fail when Codex-facing docs/dashboards contain
   legacy-provider-specific labels or semantics.
10. Add tests that fail when Codex-facing panels claim tool/MCP/skill lifecycle
    without Envoy-derived source fields.

## Grafana Decision Table

| Current panel/data point | Decision |
|---|---|
| Codex Responses requests since start | Keep |
| Codex requests by model since start | Keep |
| Codex Responses tokens by kind | Keep |
| Codex tokens by model since start | Keep |
| Codex cached input % since start | Keep |
| Codex context fill p95 | Keep, only with verified context window |
| Codex failed responses | Replace with true response-status counter |
| Codex incomplete responses | Replace with true response-status counter |
| Codex model fallbacks | Keep if requested/served model comparison is exact enough |
| Codex diagnosis cause labels | Keep only for Envoy-derived causes |
| Estimated Codex cost | Drop from correctness dashboard |
| Average estimated Codex session cost | Drop from correctness dashboard |
| Weekly tokens vs cap | Drop from correctness dashboard |
| Sessions finalized | Keep only if Codex/provider-filtered, otherwise rename/remove |
| Degraded sessions % | Keep only if Codex/provider-filtered and Envoy-derived causes only |
| Degraded causes by type | Keep only if Codex/provider-filtered and Envoy-derived causes only |
| Active sessions now | Drop or label process-local operational state |
| Tool failures | Drop |
| Tool failures by tool | Drop |
| Skill events | Drop |
| Skill misses/failures | Drop |
| Skill lifecycle by skill | Drop |
| MCP events | Drop |
| MCP failures/denials | Drop |
| MCP lifecycle by tool | Drop |

## Prometheus Decision Table

| Metric family | Decision |
|---|---|
| `coditor_requests_total{provider="codex_responses"}` | Keep |
| `coditor_tokens_total{provider="codex_responses"}` | Keep |
| `coditor_context_fill_percent{provider="codex_responses"}` | Keep |
| response-status counters | Add if missing |
| exact requested/served model mismatch counters | Keep or add |
| estimated cost counters | Drop from Codex correctness surface |
| session counters | Keep only if provider-filtered |
| degraded-cause counters | Keep only for Envoy-derived Codex causes |
| cache-event counters from copied semantics | Drop from Codex surface |
| tool-call counters | Keep only for Envoy-observed custom tool-call intent |
| tool-result/failure counters | Drop |
| MCP counters | Drop |
| skill counters | Drop |
| weekly budget/cap gauges | Drop from Codex correctness surface |
| historical gauges | Keep only if provider-filtered from Envoy-derived Codex rows |

## DB/API Decision Table

| Table/API field | Decision |
|---|---|
| `requests.provider` | Keep |
| `requests.requested_model` | Keep |
| `requests.served_model` | Keep |
| `requests.codex_status` | Keep |
| `requests.codex_*_tokens` | Keep |
| `requests.codex_response_id` | Keep |
| `requests.codex_prompt_excerpt` | Keep |
| `requests.codex_tool_calls` | Keep only as model-side intent |
| `requests.codex_accounting_anomalies` | Keep |
| `turn_snapshots` Codex token/model/status fields | Keep |
| `sessions` Codex totals | Keep, but repair from request rows |
| `sessions.model` | Change API output to show requested and served models separately |
| `tool_calls` as Codex truth | Drop or source-scope away from Codex correctness |
| `tool_outcomes` as Codex truth | Drop |
| `skill_events` as Codex truth | Drop |
| `mcp_events` as Codex truth | Drop |
| `/api/sessions` cost truth | Drop or label non-correctness local estimate |
| `/api/diagnosis` tool/MCP/skill causes | Drop |
| `/watch` tool/MCP/skill summaries | Drop unless Envoy-derived |
| `/watch` Codex turn summaries | Keep |
| `/watch` context status | Keep with context-window caveat |

## Implementation Order

1. Add guard tests for forbidden Codex-facing legacy-provider-specific terms and
   forbidden JSONL dependency.
2. Remove JSONL telemetry from the normal Codex wrapper path.
3. Remove or quarantine non-Envoy tool/MCP/skill telemetry from Codex-facing
   dashboards, metrics, APIs, watch, and docs.
4. Add true Envoy-derived response-status counters for failed/incomplete
   responses.
5. Provider-filter historical Codex gauges or remove them from Codex panels.
6. Split requested model and served model in API/session presentation.
7. Persist Envoy-derived Codex response summaries if recall/watch should show
   final output.
8. Re-run fake Responses e2e and then no-JSONL live smoke validation.

## Validation Commands

```sh
cargo fmt --check
cargo check
cargo test -p coditor-cli
cargo test --workspace --no-run
./test/e2e-openai-responses-full.sh
./test/dogfood-codex-sessions.sh --mode real --sessions 4 --repos mixed --no-json
git diff --check
git status --short
```

The dogfood command is the desired target. The `--no-json` flag is accepted so
the validation target is explicit; the harness must not parse local Codex stdout
as telemetry.

## Separate Codex Session Prompt

```text
You are working in /Users/pradeepsingh/code/coditor.

Goal: make Coditor's Codex telemetry surface Envoy-only and Codex-only.

Hard constraints:
- No legacy-provider-specific paths, labels, config, dashboards, tests, docs, or user-facing claims.
- Do not pass codex exec --json.
- Do not parse local Codex JSONL stdout as production/live telemetry.
- Do not use OPENAI_API_KEY for live Codex subscription validation.
- Anything not provable from Envoy-observed Codex request/response traffic must be removed from the product-facing Codex surface.
- Do not replace JSONL with hooks, terminal scraping, local session-file scraping, or app-server side channels.
- Do not start Phase 9C dogfood harness.
- Do not push.

Start by reading:
- AGENTS.md
- docs/codex-telemetry-without-jsonl-plan.md
- docs/codex-traffic-contract.md
- docs/real-codex-smoke.md
- test/dogfood-codex-sessions.sh
- coditor-cli/src/main.rs
- coditor-core/src/codex_request.rs
- coditor-core/src/codex_response.rs
- coditor-core/src/codex_accounting.rs
- coditor-core/src/metrics.rs
- coditor-core/src/main.rs
- grafana/dashboards/coditor.json

Task:
1. Add guard tests that fail on Codex-facing legacy-provider-specific labels/semantics and target-path JSONL dependency.
2. Remove JSONL telemetry from normal coditor run -- codex behavior.
3. Remove or quarantine Codex-facing tool/MCP/skill lifecycle panels, metrics, watch events, API claims, and DB correctness claims unless they are Envoy-derived.
4. Keep only Envoy-derived Codex facts: request count, route, requested model, served model, response id, status, prompt excerpt, token usage, token anomalies, context fill, model mismatch, output text, and custom tool-call intent when present.
5. Add true Envoy-derived response-status counters for failed/incomplete Codex responses.
6. Provider-filter or remove historical Codex panels that currently aggregate non-Codex rows.
7. Expose requested and served model separately in APIs where model identity is shown.
8. Persist Envoy-derived Codex response summaries if recall/watch should display final Codex output.
9. Run:
   cargo fmt --check
   cargo check
   cargo test -p coditor-cli
   cargo test --workspace --no-run
   ./test/e2e-openai-responses-full.sh
   git diff --check
   git status --short

Report:
- files changed
- what was kept because Envoy proves it
- what was removed because Envoy cannot prove it
- remaining blockers before no-JSONL live validation
- validation results
```
