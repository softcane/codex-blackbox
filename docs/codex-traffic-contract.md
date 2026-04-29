# Codex/OpenAI Traffic Contract

Status: fixture contract only. Coditor does not support real Codex/OpenAI
traffic yet. This document defines the fake Responses traffic shape used to
drive Phase 1 fixtures and pending tests until a real Codex capture verifies or
replaces it.

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

The copied Phase 0 baseline still routes Anthropic-shaped traffic. Envoy must
not be retargeted to OpenAI until Phase 2 or later explicitly implements and
validates the request parser, response accumulator, and routing/auth mode.

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

Blocking decisions before Phase 2:

- API-key mode vs ChatGPT-auth routing for the MVP.
- Whether Codex request compression is enabled by default and whether Coditor
  should disable it or decompress request bodies.
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
