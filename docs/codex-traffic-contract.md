# Codex/OpenAI Traffic Contract

Status: fixture/manual proxy contract only. Coditor has an experimental
ChatGPT/Codex subscription wrapper path, but real Codex/OpenAI traffic is not
validated yet.
This document defines the fake Responses traffic shape used to drive fixtures
and tests until a real Codex capture verifies or replaces it.

References:

- OpenAI Responses create reference:
  <https://developers.openai.com/api/reference/resources/responses/methods/create>
- OpenAI streaming guide:
  <https://developers.openai.com/api/docs/guides/streaming-responses>
- OpenAI Responses streaming events reference:
  <https://platform.openai.com/docs/api-reference/responses-streaming>

## Fixture Path

Coditor's checked-in fake Responses fixtures use:

```text
POST /v1/responses
```

That fixture path is not the live Codex CLI wrapper path. The live wrapper uses
the ChatGPT/Codex subscription backend base described in Phase 9B below:
`/backend-api` for auxiliary calls and `/backend-api/codex` for model turns.

## Request Fields Coditor Needs

Coditor needs these request-body fields from a Responses request:

- `model`: requested model id. This is the user-requested model, not necessarily
  the served model.
- `instructions`: system/developer instructions. May be a string or a structured
  message list.
- `input`: user-visible input and any previous conversation items. Coditor must
  support a plain string, a message object list, and structured content parts.
- `tools`: available tools. Coditor should treat this as capability metadata for
  session diagnostics, not proof that a tool was called.
- `reasoning`: reasoning effort/summary settings when present.
- `prompt_cache_key`: cache affinity key. OpenAI documents this as replacing
  `user` for cache optimization.
- `metadata`: official OpenAI structured metadata field.
- `client_metadata`: Codex-specific candidate field to verify in real captures.
  This is not assumed to be an official OpenAI Responses field.

For Coditor session identity, request metadata should be captured but not trusted
until real Codex traffic confirms the source of session ids, cwd, and hook
correlation.

## Expected SSE Event Types

Phase 1 fixtures include these Responses SSE events:

- `response.created`
- `response.output_item.added`
- `response.output_text.delta`
- `response.custom_tool_call_input.delta`
- `response.output_item.done`
- `response.completed`
- `response.failed`
- `response.incomplete`

The accumulator in Phase 2 should tolerate extra future event types. OpenAI API
compatibility rules allow new event types, so Coditor should ignore unknown
events unless they are required for correctness.

## Usage Mapping

Coditor's internal usage shape should use these names:

- `input_tokens`
- `cached_input_tokens`
- `output_tokens`
- `reasoning_output_tokens`
- `total_tokens`

OpenAI Responses usage currently reports cached input as a nested detail:

```json
{
  "input_tokens": 1280,
  "input_tokens_details": {
    "cached_tokens": 512
  },
  "output_tokens": 96,
  "output_tokens_details": {
    "reasoning_tokens": 32
  },
  "total_tokens": 1376
}
```

Coditor should map `input_tokens_details.cached_tokens` to
`cached_input_tokens`, and `output_tokens_details.reasoning_tokens` to
`reasoning_output_tokens`.

Important accounting invariant: cached input tokens are a subset of input
tokens. Runtime totals and context fill must not calculate
`input_tokens + cached_input_tokens`.

## Phase 4A Accounting Decisions

The Phase 4A adapter is a fixture-only finalization boundary. It maps a parsed
Responses request plus an accumulated Responses summary into a Codex turn
accounting summary, but it does not write SQLite rows, broadcast watch events,
or enforce budgets.

Token accounting decisions:

- `cached_input_tokens` remains a subset/details field of `input_tokens`.
- `uncached_input_tokens = input_tokens - cached_input_tokens`, saturating at
  zero if a malformed or future payload reports more cached tokens than input
  tokens.
- Local `total_tokens = input_tokens + output_tokens`; it does not add
  `cached_input_tokens` again.
- `reasoning_output_tokens` is tracked separately as output-side detail and is
  not added on top of `output_tokens` for local totals.
- If provider-reported `total_tokens` differs from the local total, Coditor
  records an accounting anomaly for later handling instead of changing the
  local rule.

Served model precedence is resolved before accounting: response headers
`openai-model` then `x-openai-model` win over the payload `response.model`;
payload model is used only when no served-model header was captured.

Pricing remains explicit and untrusted in Phase 4A. Known Codex/OpenAI API
models can produce an estimated API-price cost, with cached input charged as a
subset of input rather than added on top. Unknown OpenAI/Codex model ids still
produce an `UnknownModel`/unpriced result with no nonzero fallback cost and are
not trusted for budget enforcement.

## Phase 4B Finalization Boundary

Phase 4B wires the fixture-only Codex accounting summary into a minimal
in-memory finalization path. It still does not persist Codex turns to SQLite,
does not emit Anthropic cache TTL/rebuild events, and does not make real Codex
traffic supported.

Codex watch events are limited to:

- `SessionStart` when a session identity and first prompt excerpt are available.
- `ContextStatus` using `input_tokens / context_window_tokens`; cached input is
  not added again.
- `ModelFallback` when requested and served models differ.
- `CodexTurnSummary` with Codex-native status, requested/served model, cached
  input, uncached input, output, reasoning output, and total token fields. This
  does not imply Anthropic cache TTL/rebuild behavior.

Codex metrics record request count, input tokens, output tokens, duration,
context status, and model fallback labels where applicable. Known OpenAI model
pricing is stored as an untrusted API-price estimate; unknown Codex pricing
contributes no estimated cost and neither path is used for session budget
enforcement unless a trusted pricing catalog or reconciliation is provided.

## Phase 4C SQLite Persistence

Phase 4C persists fixture Codex turns to SQLite without reusing Anthropic cache
TTL/rebuild semantics. Codex rows use the existing `sessions`, `requests`, and
`turn_snapshots` tables so history remains repairable, but Codex-specific values
are stored in explicit `codex_*` fields.

Existing generic fields used for Codex:

- `sessions.session_id`: Codex session identity resolved from request metadata
  or fallback hash.
- `sessions.model`: requested Codex model for the session.
- `sessions.initial_prompt`: first user-visible prompt excerpt when available.
- `sessions.total_input_tokens` and `sessions.total_output_tokens`: generic
  totals for all persisted providers.
- `requests.request_id`, `requests.session_id`, and `requests.timestamp`: the
  immutable per-turn request row identity.
- `requests.input_tokens` and `requests.output_tokens`: Codex input/output
  totals, matching provider top-level usage.
- `turn_snapshots.input_tokens` and `turn_snapshots.output_tokens`: Codex
  input/output totals for the persisted turn.
- `turn_snapshots.requested_model` and `turn_snapshots.actual_model`: requested
  model and served model when known.

Codex-native fields added in Phase 4C:

- `sessions.total_codex_input_tokens`
- `sessions.total_codex_cached_input_tokens`
- `sessions.total_codex_uncached_input_tokens`
- `sessions.total_codex_output_tokens`
- `sessions.total_codex_reasoning_output_tokens`
- `sessions.total_codex_tokens`
- `requests.provider`
- `requests.requested_model`
- `requests.served_model`
- `requests.codex_status`
- `requests.codex_input_tokens`
- `requests.codex_cached_input_tokens`
- `requests.codex_uncached_input_tokens`
- `requests.codex_output_tokens`
- `requests.codex_reasoning_output_tokens`
- `requests.codex_total_tokens`
- `requests.codex_response_id`
- `requests.codex_prompt_excerpt`
- `requests.codex_tool_calls`
- `requests.codex_accounting_anomalies`
- `turn_snapshots.request_id`
- `turn_snapshots.provider`
- `turn_snapshots.codex_status`
- `turn_snapshots.codex_input_tokens`
- `turn_snapshots.codex_cached_input_tokens`
- `turn_snapshots.codex_uncached_input_tokens`
- `turn_snapshots.codex_output_tokens`
- `turn_snapshots.codex_reasoning_output_tokens`
- `turn_snapshots.codex_total_tokens`
- `turn_snapshots.codex_response_id`
- `turn_snapshots.codex_prompt_excerpt`
- `turn_snapshots.codex_tool_calls`
- `turn_snapshots.codex_accounting_anomalies`

For Codex rows, copied Anthropic cache columns remain non-authoritative:

- `requests.cache_read_tokens` is written as `0` for Codex.
- `requests.cache_creation_tokens` is written as `0` for Codex.
- `turn_snapshots.cache_read_tokens` is written as `0` for Codex.
- `turn_snapshots.cache_creation_tokens` is written as `0` for Codex.
- `requests.cache_event` is left `NULL` for Codex; Codex cached input does not
  emit or imply a TTL/rebuild cache event.

Known OpenAI API pricing is persisted as an explicit untrusted estimate using
the Codex-native cached-input fields. Unknown OpenAI/Codex pricing is persisted
as explicit unpriced zero cost with a
`codex_unpriced:unknown_model:<model>` cost source and
`trusted_for_budget_enforcement = 0`. This prevents historical summary paths
from falling back to copied Anthropic pricing for Codex models.

## Phase 5A Fake Envoy E2E

Phase 5A adds a local-only fake OpenAI Responses upstream for Envoy tests:

- `test/fake-openai.py` accepts `POST /v1/responses` and streams checked-in
  Responses SSE fixtures.
- `test/envoy.openai-responses.e2e.yaml` routes the local Envoy listener to the
  fake upstream while keeping ext_proc enabled, `failure_mode_allow: true`, and
  `response_body_mode: STREAMED`.
- `test/e2e-openai-responses.sh` sends a fixture request through Envoy, verifies
  streamed Responses events, checks Codex `SessionStart` and `ContextStatus`
  watch events, and asserts no Anthropic `CacheEvent` TTL/rebuild fields are
  emitted for the Codex turn.

This e2e path uses no OpenAI credentials and does not contact real OpenAI or
real Codex services.

## Phase 5B Default Codex Envoy Config

Phase 5B validates the default Envoy processing shape used by the Codex CLI
wrapper:

- `docker-compose.yml` mounts `envoy/envoy.yaml`.
- `envoy/envoy.yaml` routes `/backend-api` to `chatgpt.com:443`.
- The upstream uses TLS with SNI `chatgpt.com` and host rewrite `chatgpt.com`.
- ext_proc remains enabled with `failure_mode_allow: true`.
- request bodies remain `BUFFERED` because the current request parser expects a
  complete plaintext JSON body.
- response bodies remain `STREAMED` so response chunks reach the accumulator.

Example static validation:

```sh
./test/validate-openai-config.sh
```

This config is the default Codex CLI wrapper path. Live traffic is still not
validated until a ChatGPT-auth Codex smoke runs through it.

## Phase 6B CLI Wrapper

Phase 6B added a conservative `coditor run -- codex ...` wrapper. Phase 9B
keeps the wrapper subscription-only without editing `~/.codex/config.toml`.

The wrapper uses the installed Codex CLI's ChatGPT backend base URL shape.
The locally inspected Codex 0.125.0 source sends ChatGPT-auth auxiliary calls
through `chatgpt_base_url`, which defaults to
`https://chatgpt.com/backend-api/`, while model turns need the
`https://chatgpt.com/backend-api/codex` Responses route.

Coditor uses command-line config overrides only. It points auxiliary calls at
the local proxy and installs a custom `coditor-openai` model provider for model
turns so WebSocket transport can be disabled while preserving ChatGPT auth:

```text
-c 'chatgpt_base_url="http://127.0.0.1:10000/backend-api"'
-c 'model_provider="coditor-openai"'
-c 'model_providers.coditor-openai.name="OpenAI"'
-c 'model_providers.coditor-openai.base_url="http://127.0.0.1:10000/backend-api/codex"'
-c 'model_providers.coditor-openai.wire_api="responses"'
-c 'model_providers.coditor-openai.requires_openai_auth=true'
-c 'model_providers.coditor-openai.supports_websockets=false'
-c features.enable_request_compression=false
```

This path must preserve the current Codex ChatGPT login. It must not set
`OPENAI_API_KEY`, `env_key`, or `forced_login_method`.

The wrapper preserves user-provided Codex arguments after those overrides.

Use `coditor run --dry-run -- codex ...` or `coditor config codex` to inspect
the generated config without launching Codex.

## Phase 9B ChatGPT/Codex Subscription Envoy

`docker-compose.yml` mounts `envoy/envoy.yaml`. That Envoy config routes
`/backend-api` to `chatgpt.com:443`, sets host rewrite `chatgpt.com`, and
uses TLS SNI `chatgpt.com`. It leaves ext_proc enabled with buffered request
bodies and streamed response bodies so `coditor-core` can observe the request
and streamed response.

Manual preflight:

```sh
coditor preflight codex-subscription -- codex exec ...
```

The preflight verifies local Codex ChatGPT login, starts the subscription-mode
stack, and prints the exact command. It must stop before any real Codex call
until the user approves the live smoke.

## Phase 7 Fake Codex Hook Contract

Phase 7 adds a fixture-only hook endpoint:

```text
POST /api/hooks/codex
```

The endpoint accepts JSON bodies matching the checked-in
`coditor.codex_hook.v1` fixture contract under `test/fixtures/`. It always
returns a safe JSON response with HTTP `202 Accepted`; unknown events, missing
fields, or invalid JSON are reported as `ignored` rather than panicking. This
endpoint is not in the model traffic path, and hook failures must not affect
Responses forwarding.

Fixture fields Coditor currently understands:

- `schema`: expected fixture marker, currently `coditor.codex_hook.v1`.
- `event`: one of `prompt_submit`, `session_start`, `tool_start`,
  `tool_finish`, `tool_failure`, `mcp_tool_start`, `mcp_tool_finish`, or
  `mcp_tool_failure`. Legacy-style aliases such as `pre_tool_use` and
  `post_tool_use` are tolerated for fixture convenience.
- `session_id`: hook/session id emitted by the hook source.
- `proxy_session_id`: optional Coditor proxy session id. When present, Coditor
  records a `session_id -> proxy_session_id` correlation and emits watch events
  under the proxy session id.
- `request_id`: optional per-turn request id for fixture correlation. It is
  captured only as contract context in Phase 7 and is not persisted as a hook
  row.
- `cwd`: optional working directory. Prompt/session hooks use its final path
  component as the provisional watch display name.
- `model`: optional requested model string for provisional `SessionStart`.
- `source`: optional hook source label. Tool/MCP details may include it.
- `permission_mode` or `permission`: optional permission/mode label. Tool/MCP
  details may include it.
- `prompt`: optional prompt excerpt for `SessionStart`.
- `tool`: optional object with `id`, `name`, `input`, `outcome`, and
  `duration_ms` for regular tool events.
- `mcp`: optional object with `server`, `tool`, and `input` for MCP events.

Watch event mapping:

- `prompt_submit` and `session_start` emit a provisional `SessionStart`.
  Provisional sessions are in-memory only; durable SQLite request/session
  history still comes from proxy finalization.
- `tool_start` emits `ToolUse`.
- `tool_finish` emits `ToolResult` with a successful outcome.
- `tool_failure` emits `ToolResult` with a failed outcome.
- `mcp_tool_start`, `mcp_tool_finish`, and `mcp_tool_failure` emit `McpEvent`
  with `called`, `succeeded`, or `failed` event types.
- Codex Responses tool calls observed by the proxy also emit `ToolUse`.
  A short-lived dedupe key suppresses duplicate hook/proxy reports for the same
  session, tool name, and summary/outcome.

Limitations:

- This is not a real Codex hook schema. Real hook names, payload fields,
  permission labels, source labels, and MCP naming must be verified in a later
  capture.
- Hooks are not authoritative for cost, token accounting, context fill,
  persistence, diagnosis, or session completion.
- Hook session correlation is best-effort. `proxy_session_id` wins when present;
  remembered `session_id -> proxy_session_id` mappings are used for later hook
  payloads from the same fixture session.
- Dedupe is intentionally conservative and local to recent in-memory events.
  It prevents obvious watch duplication but is not durable cross-process state.
- `coditor config codex` prints the suggested hook endpoint read-only. Coditor
  does not modify `~/.codex/config.toml`.

## Phase 8A Codex Diagnostics

Phase 8A teaches diagnosis to read Codex-native signals from fixture Responses
turns, persisted Codex turn rows, and fake hook/MCP telemetry. It does not add
real rate-limit parsing and does not broaden the dogfood harness.

Codex diagnostic causes currently emitted:

- `codex_response_failed`: a persisted or in-memory Codex turn ended with
  `failed` status.
- `codex_response_incomplete`: a Codex turn ended with `incomplete` status.
- `codex_model_mismatch`: requested and served model strings differed.
- `codex_high_context_fill`: Codex input filled at least 80% of the configured
  context window.
- `codex_high_reasoning_share`: reasoning output was at least half of output
  tokens and at least 64 tokens.
- `codex_repeated_tool_failures`: three or more Codex tool failures were
  observed for the session.
- `codex_mcp_tool_failures`: two or more failed or denied MCP tool events were
  observed for the session.
- `codex_accounting_anomaly`: the Codex accounting adapter recorded one or more
  accounting anomalies.
- `codex_low_cached_input_reuse`: at least three Codex turns had less than 10%
  aggregate cached-input reuse.

Diagnosis behavior:

- Codex turns stored in `turn_snapshots` carry explicit `provider`,
  `codex_status`, cached-input, reasoning-output, and accounting-anomaly fields.
- `/api/diagnosis/<session_id>` can compute a Codex diagnosis from persisted
  turn snapshots when no stored `session_diagnoses` row exists yet.
- Fake hook `ToolResult` failures and fake MCP failure events can enrich Codex
  diagnosis when they correlate to a proxy session id.
- Non-heuristic Codex causes can emit watch `Diagnosis` events from the hot
  path without blocking on SQLite.
- Prometheus uses the existing bounded cause label path; no session-id labels
  are added.

Limitations:

- Cause thresholds are fixture-driven and conservative. They should be
  revisited after real Codex captures.
- Rate-limit headers are still unverified and intentionally not parsed.
- Hook tool failure correlation is best-effort and in-memory for live watch
  diagnosis; persisted diagnosis augments from `tool_outcomes` and `mcp_events`.
- Codex cached-input reuse is treated as a diagnosis signal only after at least
  three turns; OpenAI cache-affinity behavior still needs real validation.

## Phase 8B Observability Boundary

Phase 8B adds a local observability validation around the fake OpenAI Responses
stack. It starts Prometheus and Grafana with the fake upstream, sends a fixture
Responses turn, and verifies that Coditor exposes bounded Codex-relevant
metrics without requiring OpenAI credentials.

Prometheus checks currently cover:

- `coditor_requests_total{provider="codex_responses",model="<bounded label>"}`
- `coditor_tokens_total{provider="codex_responses",model="<bounded label>",kind="<token kind>"}`
- `coditor_turn_duration_seconds`
- `coditor_context_fill_percent`
- `coditor_sessions_degraded_total` with bounded `codex_*` cause labels
- `coditor_mcp_events_total`
- absence of session-id metric labels and fixture session-id label values

Grafana checks currently cover:

- Grafana HTTP health.
- provisioned `coditor-main` dashboard availability.
- Phase 8B dashboard panels for Codex/OpenAI request count, token kinds,
  context fill, and Codex diagnosis causes.
- dashboard panel expressions reference metrics that Prometheus has scraped.

Rate-limit boundary:

- Coditor still does not parse OpenAI rate-limit headers as production truth.
- `test/fixtures/openai_rate_limit_candidate_headers.txt` records candidate
  header names only as an unverified fixture boundary for future tests.
- Real header names, units, reset semantics, and availability must be captured
  from real Codex/OpenAI traffic before any production parser is added.

## Headers To Verify Later

The fake contract records these headers as candidates to verify in real Codex
traffic:

- `session_id`: possible Codex session or conversation id.
- `x-client-request-id`: possible stable per-turn request id.
- `openai-model`: possible served model id.
- `x-openai-model`: possible served model id.
- OpenAI rate-limit headers: exact names, presence, and units are unknown for
  Codex traffic and must be captured before implementation.

Model fallback detection should prefer the served model from response headers
when available, then response payload metadata if headers are absent.

## Current Unknowns

Resolved MVP decisions:

- ChatGPT subscription mode is the only Codex CLI wrapper direction and points at
  local `/backend-api` and `/backend-api/codex`, forwarding to `chatgpt.com`.
- Codex request compression is disabled for that wrapper path with
  `features.enable_request_compression=false`.

Still unknown until a real capture:

- Whether the live ChatGPT/Codex subscription response stream is byte-for-byte
  compatible with the existing Responses SSE accumulator.
- Codex hook schema and whether hooks are authoritative for sessions, only
  enrich proxy sessions, or only provide tool telemetry.
- Source of cwd/working directory: request metadata, Codex hook payload, config,
  or fallback inference.
- Session id precedence across `session_id` header, hook id, response id,
  conversation id, and fallback hash.
- Whether Codex emits tool ids/results in Responses SSE, hooks, or both.

Until these are resolved with a real capture, the fixtures under
`test/fixtures/` are only a planning and TDD artifact. They must not be used to
claim real Codex compatibility.
