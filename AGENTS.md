# Codex Blackbox Agent Instructions

These instructions apply to the whole repository unless a deeper `AGENTS.md`
overrides them.

## Product Boundary

- Codex Blackbox observes Codex traffic through a local Envoy proxy, `codex-blackbox-core`,
  and the `codex-blackbox` CLI.
- Fake OpenAI Responses fixtures validate local contracts only. Do not turn a
  fake e2e result into a live Codex support claim.
- Live support claims require explicit real smoke or dogfood evidence.
- Prefer the current code and tests over stale copied comments or old planning
  text.

## Implementation Map

- Request parsing: `codex-blackbox-core/src/codex_request.rs`
- Response SSE accumulation: `codex-blackbox-core/src/codex_response.rs`
- Turn accounting: `codex-blackbox-core/src/codex_accounting.rs`
- Pricing: `codex-blackbox-core/src/pricing.rs`
- Runtime, persistence, hooks, and Envoy ext_proc: `codex-blackbox-core/src/main.rs`
- Watch event types: `codex-blackbox-core/src/watch.rs`
- Metrics: `codex-blackbox-core/src/metrics.rs`
- CLI wrapper, preflight, watch rendering: `codex-blackbox-cli/src/main.rs`

## Worktree Rules

- Read the code path and nearby tests before changing behavior.
- Use `rg`/`rg --files` for searches.
- Check `git status --short` before and after meaningful edits.
- Do not revert or overwrite user changes.
- Keep changes scoped to the requested behavior and surrounding module.
- Use `apply_patch` for manual edits.
- Update tests and user-facing docs when behavior changes.

## Codex Routing

- `codex-blackbox run -- codex ...` uses the experimental ChatGPT subscription proxy
  path.
- The default Envoy listener routes `/backend-api` to `chatgpt.com`.
- Codex model turns use command-line config overrides:
  `chatgpt_base_url`, `openai_base_url`, `model_provider="openai"`, and
  `features.enable_request_compression=false`.
- Do not mutate `~/.codex/config.toml` for wrapper behavior.
- Do not inject `--ephemeral`; Codex keeps normal session persistence.
- Remove inherited parent-session `CODEX_*` variables listed in the CLI before
  spawning child Codex processes.
- `OPENAI_API_KEY` is not used for ChatGPT subscription proxy mode.
- A successful `codex exec` child run must still fail the wrapper if
  `codex-blackbox-core` observes no new `provider="codex_responses"` request.

## Request Parsing

- Parse model turns as Responses-shaped JSON.
- Skip non-model ChatGPT auxiliary request bodies under `/backend-api/`.
- Treat `/backend-api/codex/responses` as model-turn traffic.
- Request identity precedence is:
  `session_id`/`session-id` header, `x-client-request-id`, `client_metadata`,
  then stable fallback hash.
- Fallback hashes must be deterministic across processes.
- Keep request compression disabled or decode it before JSON parsing.
- Stripping `accept-encoding` only handles upstream response compression.

## Response Parsing

- Responses SSE parsing must handle split chunks.
- Unknown future event types should be ignored unless required for correctness.
- Preserve completed, failed, incomplete, and unknown statuses distinctly.
- Served model precedence is response header `openai-model`, then
  `x-openai-model`, then payload `response.model`.
- Tool calls reported in Responses output are proxy-observed tool starts, not
  proof of tool result success.

## Token Accounting

- `cached_input_tokens` is a subset of `input_tokens`.
- Never calculate context fill, runtime totals, persisted totals, or cost as
  `input_tokens + cached_input_tokens`.
- `uncached_input_tokens = input_tokens - cached_input_tokens`, saturating at
  zero for malformed or future payloads.
- `reasoning_output_tokens` is output-side detail; do not add it on top of
  `output_tokens`.
- Local total tokens are `input_tokens + output_tokens`.
- If provider-reported totals differ from local totals, record an anomaly
  instead of changing the local rule.
- Unknown or long-context model pricing must remain explicit, unpriced, and
  untrusted for budget enforcement.

## Watch And Hooks

- Proxy-observed model responses are authoritative for durable Codex turns.
- Do not use Codex hooks, local JSON stdout, or app-server hook endpoints as
  Codex telemetry sources.
- Watch `ToolUse` events are Envoy-observed model-side custom tool-call intent,
  not proof of tool result success.
- Do not expose `ToolResult`, `SkillEvent`, `McpEvent`, `CacheEvent`, cache TTL,
  cache rebuild, or provider quota/cap state in Codex watch or tmux surfaces.
- `CodexTurnSummary` is the Codex-native per-turn watch event.
- Watch replay and duplicate suppression are correctness behavior, not UI
  polish; keep them tested.

## Persistence And Metrics

- Persist Codex turns with `provider="codex_responses"` and explicit
  `codex_*` fields.
- Keep request rows immutable and derived session totals repairable.
- Preserve failed and incomplete response statuses even with partial usage or
  output.
- Persist Envoy-observed tool-call intent without inventing tool outcomes,
  MCP lifecycle events, skill lifecycle events, or cache events.
- Prometheus labels must stay bounded.
- Never use session ids, cwd values, prompts, request ids, response ids, or raw
  tool inputs as metric labels.
- Normalize model labels through the existing metrics helpers.

## Failure Lessons

- A passing build, renamed binary, or `--help` output is not support proof.
- Keep fake, preflight, live smoke, dogfood, and release claims separate.
- Config previews and preflights must stop before launching a real Codex turn
  unless the user explicitly approves the live call.
- Wrapper tests must prove args are preserved, config files are untouched,
  request compression is disabled, parent `CODEX_*` env is removed, and
  observation gating still works.
- Parser/accounting tests must cover failed streams, incomplete streams, split
  SSE chunks, served-model headers, cached-input subset math, and unknown
  pricing.
- Watch tests must cover replay, duplicate suppression, Codex turn summaries,
  context status, and no cache/lifecycle watch events for Codex cached input.
- Envoy `failure_mode_allow` only proves runtime failure-open after Envoy is
  already running; startup behavior is a separate compose/config concern.
- If a capability is absent or unverified, call it unknown, missing, skipped,
  or untrusted instead of filling in a plausible claim.

## Test Guidance

- For Rust changes, run targeted tests or `cargo test` when feasible.
- Static default config validation:
  `./test/validate-openai-config.sh`
- Narrow fake Responses proxy test:
  `./test/e2e-openai-responses.sh`
- Fake observability validation:
  `./test/observability-openai-responses.sh`
- Broader fake regression before real smoke:
  `./test/e2e-openai-responses-full.sh`
- If a relevant test is skipped, state why in the final response.
