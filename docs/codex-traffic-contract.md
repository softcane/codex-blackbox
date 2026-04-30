# Codex/OpenAI Traffic Contract

Status: fixture/manual proxy contract only. Coditor has an experimental manual
OpenAI API-key wrapper path, but real Codex/OpenAI traffic is not validated yet.
This document defines the fake Responses traffic shape used to drive fixtures
and tests until a real Codex capture verifies or replaces it.

References:

- OpenAI Responses create reference:
  <https://developers.openai.com/api/reference/resources/responses/methods/create>
- OpenAI streaming guide:
  <https://developers.openai.com/api/docs/guides/streaming-responses>
- OpenAI Responses streaming events reference:
  <https://platform.openai.com/docs/api-reference/responses-streaming>

## Expected Path

Coditor expects Codex/OpenAI model traffic to use:

```text
POST /v1/responses
```

The copied Phase 0 baseline still routes Anthropic-shaped traffic by default.
OpenAI routing remains opt-in through the manual OpenAI Compose override and
the experimental `coditor run -- codex ...` wrapper.

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

Pricing remains explicit and untrusted in Phase 4A. Unknown OpenAI/Codex model
ids produce an `UnknownModel`/unpriced result with no nonzero fallback cost and
are not trusted for budget enforcement.

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

Codex metrics record request count, input tokens, output tokens, duration,
context status, and model fallback labels where applicable. Unknown/untrusted
Codex pricing contributes no estimated cost and is not used for session budget
enforcement.

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
  emit or imply an Anthropic TTL/rebuild cache event.

Unknown OpenAI/Codex pricing is persisted as explicit unpriced zero cost with a
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

## Phase 5B Manual OpenAI API-Key Mode

Phase 5B adds an experimental/manual Envoy path for OpenAI API-key mode:

- `envoy/envoy.openai.yaml` routes `POST /v1/responses` to
  `api.openai.com:443`.
- The OpenAI upstream uses TLS with SNI `api.openai.com` and host rewrite
  `api.openai.com`.
- ext_proc remains enabled with `failure_mode_allow: true`.
- request bodies remain `BUFFERED` because the current request parser expects a
  complete plaintext JSON body.
- response bodies remain `STREAMED` so Responses SSE chunks reach the
  accumulator.
- `docker-compose.openai.yml` mounts the OpenAI Envoy config as an explicit
  override.

Example static validation:

```sh
./test/validate-openai-config.sh
```

Manual OpenAI mode is not the default stack. Phase 6B can point Codex at this
path with API-key config overrides, but real OpenAI/Codex traffic is still not
validated. ChatGPT-auth Codex backend routing is still unsupported and
unverified.

## Phase 6B CLI Wrapper

Phase 6B adds a conservative `coditor run -- codex ...` wrapper for manual
OpenAI API-key experiments. It does not edit `~/.codex/config.toml`; instead it
prepends command-line Codex config overrides:

```text
-c 'model_provider="coditor-openai-responses"'
-c 'model_providers.coditor-openai-responses.name="Coditor OpenAI Responses proxy"'
-c 'model_providers.coditor-openai-responses.base_url="http://127.0.0.1:10000/v1"'
-c 'model_providers.coditor-openai-responses.env_key="OPENAI_API_KEY"'
-c 'model_providers.coditor-openai-responses.wire_api="responses"'
-c 'forced_login_method="api"'
-c features.enable_request_compression=false
```

The wrapper preserves user-provided Codex arguments after these overrides and
prints that ChatGPT-auth backend routing is unsupported/unverified. Non-Codex
child commands still use the temporary unported `ANTHROPIC_BASE_URL` fallback.

Use `coditor run --dry-run -- codex ...` or `coditor config codex` to inspect
the generated config without launching Codex.

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

- API-key mode is the first manual wrapper path; ChatGPT-auth routing remains
  unsupported/unverified.
- Codex request compression is disabled for the wrapper with
  `features.enable_request_compression=false`.

Still unknown until a real capture:

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
