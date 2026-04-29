# Coditor Build Plan

Coditor is the Codex-focused counterpart to Clauditor: live observability for Codex CLI and Codex app traffic, built around OpenAI Responses API semantics rather than Anthropic Messages semantics.

This plan treats `/Users/pradeepsingh/code/clauditor` as the reference implementation. The first implementation should be a controlled fork or selective copy into this repo, followed by deliberate replacement of Claude-specific behavior.

## Product Goal

Build a local observability proxy and CLI for Codex that shows, per Codex session:

- live session starts and ends
- model requested and model actually served
- token usage, cached input usage, output tokens, and reasoning output tokens
- tool calls and tool results where Codex exposes them
- context-window fill and compaction risk
- rate-limit status when OpenAI headers expose it
- request and session history in SQLite
- live `/watch` stream and optional tmux dashboard
- Prometheus metrics without high-cardinality session labels

The tool should preserve Clauditor's operational shape while making Codex/OpenAI the primary domain.

## Core Decision

Create a separate repo, not a multi-provider branch inside Clauditor.

Why:

- Clauditor's product contract is Claude-specific: Anthropic request format, Anthropic SSE events, Claude Code prompt cleanup, Anthropic cache TTL UX, Anthropic pricing, and `ANTHROPIC_BASE_URL` wrapping.
- Codex uses OpenAI Responses API traffic and Codex-specific session identifiers, hooks, headers, model metadata, cached-token accounting, and possibly ChatGPT-auth routing.
- A separate repo lets Coditor keep simpler invariants without provider conditionals scattered through hot-path code.

## Source References

Use these Clauditor files as the starting map:

- `/Users/pradeepsingh/code/clauditor/clauditor-core/src/main.rs`
  - `ResponseAccumulator` starts around line 1852.
  - `finalize_response` starts around line 2116.
  - `parse_request_body` starts around line 3288.
  - `process` ext_proc handler starts around line 4212.
  - `handle_watch` starts around line 4555.
  - HTTP router registers `/watch` around line 5473.
- `/Users/pradeepsingh/code/clauditor/clauditor-core/src/watch.rs`
  - `WatchEvent` starts around line 12.
  - `EventBroadcaster::subscribe_with_history` starts around line 182.
- `/Users/pradeepsingh/code/clauditor/clauditor-core/src/diagnosis.rs`
  - Copy the session registry and degradation-analysis concepts, then rename Claude-specific language.
- `/Users/pradeepsingh/code/clauditor/clauditor-core/src/metrics.rs`
  - Copy the low-cardinality Prometheus pattern.
- `/Users/pradeepsingh/code/clauditor/clauditor-core/src/pricing.rs`
  - Adapt for OpenAI model pricing and cached input pricing.
- `/Users/pradeepsingh/code/clauditor/clauditor-cli/src/main.rs`
  - `ANTHROPIC_BASE_URL` checks around lines 843 and 1102 must be replaced.
  - `WatchEvent` mirror starts around line 137.
  - `render_event` starts around line 1272.
- `/Users/pradeepsingh/code/clauditor/clauditor-cli/src/tmux.rs`
  - `bootstrap_into_tmux` starts around line 35.
  - event handling starts around line 760.
- `/Users/pradeepsingh/code/clauditor/envoy/envoy.yaml`
  - `failure_mode_allow: true` around line 68.
  - `response_body_mode: STREAMED` around line 75.
- `/Users/pradeepsingh/code/clauditor/test/`
  - Copy the e2e structure, then replace fake Anthropic payloads with fake OpenAI Responses SSE payloads.

## Second Pass: Code Alignment Notes

This pass compares the plan above to the actual Clauditor code. These are the places where the plan is directionally right but does not exactly match the implementation today.

### 1. "Copy mostly as-is" means copy shape, not internals

The plan says to copy the workspace shape mostly as-is. That matches the repo layout, but it does not match the core code shape.

Actual code:

- `/Users/pradeepsingh/code/clauditor/clauditor-core/src/main.rs` is a large monolith containing ext_proc handling, HTTP routes, SQLite schema and writer, request parsing, response finalization, hooks, quota monitor, cleanup, and many tests.
- Only `diagnosis.rs`, `metrics.rs`, `pricing.rs`, and `watch.rs` are separate core modules.
- The CLI is mostly one large `main.rs` plus `tmux.rs`.

Correction for Coditor:

- Copy the repo layout and operational pattern.
- Do not assume there are already clean provider modules to reuse.
- Create Coditor-specific parser, accumulator, finalizer, and runtime modules early, or the fork will inherit the same single-file coupling with a second protocol layered into it.

### 2. Phase 0 is too broad if it includes full renaming

The Phase 0 Definition of Done says all crates and binaries are named Coditor and no user-facing strings refer to Clauditor. That is much larger than a simple repo bootstrap.

Actual code has Clauditor naming in:

- Cargo workspace member names
- binary names
- Docker service names
- Envoy cluster names
- database path defaults
- environment variable names
- metric names
- README, hook examples, Grafana dashboards, Prometheus config, and CLI output

Correction for Coditor:

- Split Phase 0 into:
  - Phase 0A: copy skeleton and keep it clearly marked as an unported fork.
  - Phase 0B: mechanical rename of package, binary, service, env var, metric, and docs names.
- Do not count Phase 0 done just because `coditor --help` builds. The renamed stack can still be Anthropic-shaped underneath.

### 3. Hooks do not create sessions in Clauditor

The plan says Codex hook payloads should produce `SessionStart`. That is a Coditor design choice, not something copied from Clauditor.

Actual code:

- `/Users/pradeepsingh/code/clauditor/clauditor-core/src/main.rs:4529` handles `/api/hooks/claude-code`.
- `process_claude_code_hook_payload` emits `SkillEvent` and `McpEvent`.
- `SessionStart` is created in `finalize_response`, after the proxy has parsed and finalized a model response.
- Generic `ToolResult` events come from `parse_latest_tool_results`, which reads Anthropic `tool_result` blocks from the next request body, not from hooks.

Correction for Coditor:

- Treat hook-driven `SessionStart` as new behavior that must be designed and tested.
- Do not copy the Claude hook endpoint semantics as if they provide full session lifecycle.
- Decide whether Codex hooks are authoritative for sessions, only enrich proxy sessions, or only provide tool telemetry.

### 4. Tool results are request-body derived today

The plan says to use hooks for tool start and tool finish. That may be right for Codex, but it is not how Clauditor gets generic tool results today.

Actual code:

- `parse_latest_tool_results` reconstructs tool outcomes by pairing user `tool_result` blocks with earlier assistant `tool_use` blocks via `tool_use_id`.
- `ResponseAccumulator` sees tool starts from Anthropic streamed `content_block_start` events.
- Hook handling only has special handling for skills and MCP-style tools.

Correction for Coditor:

- Coditor needs a fresh tool telemetry contract.
- First verify whether Codex Responses SSE, Codex hooks, or both expose stable tool ids and results.
- Only then decide whether `ToolResult` belongs in the proxy parser, hook handler, or a correlation layer.

### 5. OpenAI cached tokens are not additive context tokens

The plan correctly calls out cached input tokens, but it does not yet warn that Clauditor's context math cannot be copied directly.

Actual code:

- Clauditor computes context fill from `input_tokens + cache_read_tokens + cache_creation_tokens`.
- It also adds those three input-side fields into runtime token totals.
- That is shaped around Anthropic usage fields.

Codex/OpenAI correction:

- OpenAI `cached_input_tokens` should be treated as a subset/details field of `input_tokens` unless verified otherwise.
- Context fill should be based on total input tokens for the turn, not `input_tokens + cached_input_tokens`.
- Cost should separate cached and uncached input by calculating `uncached_input_tokens = input_tokens - cached_input_tokens`.
- Runtime totals should not double count cached input.

### 6. Pricing behavior must change, not just model tables

The Phase 4 Definition of Done says unknown models should be explicitly unknown. That does not match Clauditor's current pricing behavior.

Actual code:

- `/Users/pradeepsingh/code/clauditor/clauditor-core/src/pricing.rs` falls back to built-in Anthropic family pricing.
- Unknown non-opus/non-haiku models effectively fall into the Sonnet-like default branch.
- Built-in pricing is marked untrusted for budget enforcement, but it still returns a nonzero cost.

Correction for Coditor:

- Implement an explicit unknown-pricing state or require a configured OpenAI pricing catalog before cost enforcement.
- Do not silently price unknown OpenAI/Codex models as a default family.
- Keep "estimated" and "trusted for budget enforcement" separate in UI and APIs.

### 7. "Upsert" is shorthand, not the exact DB writer behavior

The plan says aggregate/session tables are maintained by upserts. That matches the intent but not the exact code path.

Actual code:

- `DbCommand::InsertSession` uses `INSERT OR IGNORE`.
- `DbCommand::RecordRequest` uses `INSERT OR IGNORE` for immutable request rows.
- When a request row is newly inserted, the writer updates session totals with a separate `UPDATE sessions SET ...`.
- `turn_snapshots` are inserted as append-only rows.

Correction for Coditor:

- Preserve the intent: immutable request history plus repairable derived state.
- Do not assume a single SQL upsert statement exists to copy.
- Revisit schema names before copying `cache_read_tokens` and `cache_creation_tokens` into Coditor tables.

### 8. Compression has two separate problems

The plan says to strip or neutralize compression. The code only does one of those things.

Actual code:

- Request headers remove `accept-encoding`, preventing compressed upstream SSE responses.
- Request body parsing assumes the request body itself is plaintext JSON.
- Envoy request body mode is `BUFFERED`.

Codex correction:

- Keep stripping `accept-encoding` for response readability.
- Separately handle Codex request compression. For MVP, disable Codex request compression via config. Otherwise ext_proc receives compressed bytes and the request parser will fail.
- Add tests for `Content-Encoding` or the exact Codex compression mechanism once verified.

### 9. Failure-open is true after Envoy starts, not for full stack startup

The plan says traffic should still forward if Coditor core is stopped. That matches the Envoy filter invariant but not the full Docker startup behavior.

Actual code/config:

- Envoy has `failure_mode_allow: true`.
- Docker Compose has Envoy `depends_on` the core health check.

Correction for Coditor:

- Test two cases separately:
  - startup behavior: whether Envoy can start without a healthy core
  - runtime behavior: whether already-running Envoy forwards when core dies
- If Coditor wants full startup failure-open, Docker Compose must change too.

### 10. The e2e test is more than a fake upstream smoke test

The plan says to copy the e2e structure. That is too broad.

Actual code:

- `/Users/pradeepsingh/code/clauditor/test/e2e.sh` drives Docker Compose, fake Anthropic, Claude Code hooks, Prometheus, Grafana-adjacent metrics, SQLite assertions, `/watch`, and session APIs.
- `/Users/pradeepsingh/code/clauditor/test/parallel-sessions.sh` asserts distinct `sys_prompt_hash` values from logs.

Correction for Coditor:

- Copy the fake-upstream pattern and shell harness style.
- Do not copy the entire e2e surface into the first Coditor test.
- Start with a narrow fake Responses proxy test, then layer watch, DB, metrics, hooks, and dashboard assertions in later phases.

### 11. The current API surface is larger than the target architecture block

The target architecture block lists `/watch`, `/metrics`, `/api/sessions`, and `/api/diagnosis`.

Actual code also exposes:

- `/api/summary`
- `/api/recall`
- `/api/billing-reconciliations`
- `/api/degradation/:session_id`
- `/api/cache-rebuilds`
- `/api/hooks/claude-code`

Correction for Coditor:

- Decide which APIs are MVP.
- Do not copy billing reconciliation, recall, cache rebuilds, or degradation endpoints unless they have a Codex-native meaning.
- If copied, document them explicitly in the phase plan and tests.

### 12. `CacheEvent` does not fit OpenAI cleanly

The plan leaves open whether to keep `CacheEvent` or introduce `CachedInputEvent`. The code shows why this matters.

Actual code:

- `WatchEvent::CacheEvent` includes `cache_expires_at_epoch` and `estimated_rebuild_cost_dollars`.
- CLI and tmux render live TTL countdowns from those fields.
- Cache event types are Anthropic-specific: `hit`, `partial`, `cold_start`, `miss_ttl`, `miss_thrash`.

Correction for Coditor:

- Prefer a new `CachedInputEvent` unless OpenAI exposes a reliable TTL and rebuild-cost equivalent.
- If keeping `CacheEvent`, make the TTL and rebuild fields optional and ensure the CLI does not render a countdown for OpenAI cached-input data.

### 13. Model fallback detection needs a new source of truth

The plan says to use OpenAI headers such as `openai-model` or `x-openai-model`. That does not map directly to the current accumulator.

Actual code:

- Anthropic fallback detection reads `/message/model` from the streamed `message_start` event.
- `ResponseAccumulator` does not currently receive response headers except through `resp_acc.http_status`.

Correction for Coditor:

- Extend the response-header phase to capture served model headers before SSE parsing.
- Treat payload model fields and headers as separate possible sources.
- Define precedence and test it.

### 14. Rate limits should not inherit Anthropic quota-burn code

The plan says to parse OpenAI rate-limit headers where available. That is new behavior, not a direct port.

Actual code:

- Clauditor explicitly ignores Anthropic rate-limit headers for Claude Code subscription traffic.
- `RateLimitStatus` is synthesized by `quota_burn_monitor` from local token counters and SQLite history.

Correction for Coditor:

- Do not copy quota-burn semantics until there is a verified Codex equivalent.
- Add a clean OpenAI header parser first.
- Keep local budget projection separate from provider rate limits.

### 15. Session identity fallback needs a Codex cwd source

The plan says fallback identity is hash of cwd plus first user-visible input. That mirrors Clauditor's idea but not its implementation details.

Actual code:

- Clauditor extracts cwd from `Primary working directory:` inside the Anthropic `system` field.
- It hashes cwd plus the full first `messages[0]` text.
- `SessionStart` display name and prompt excerpt depend on that parser.

Correction for Coditor:

- Find a real Codex cwd source before relying on the fallback hash.
- Candidate sources: Codex request metadata, Codex hook `cwd`, transcript path, or wrapper-captured current directory.
- If no cwd is available, make the fallback hash include a Codex session/conversation header or wrapper-generated id.

### 16. Metrics labels may need stricter normalization for OpenAI models

The plan says to copy the low-cardinality metrics pattern. The code does normalize tools and some labels, but model labels still need review for Codex.

Actual code:

- Request metrics record model names.
- Historical metrics use fixed model buckets for Anthropic-oriented views.
- Tool labels are normalized, but model-name cardinality can grow if exact OpenAI dated/model-preview names are used directly.

Correction for Coditor:

- Decide whether metrics use exact model, normalized model family, or both.
- Do not expose unbounded per-session or per-conversation labels.
- Add tests for label normalization with Codex model ids.

## Third Pass: Rating

Overall rating: 8.2/10.

The plan is strong enough to guide a first implementation pass. It identifies the correct major replacement points, preserves the important proxy/watch/tmux/SQLite invariants, and now calls out the key places where Clauditor's actual code does not match the first-pass assumptions.

Why it is not higher:

- The Codex traffic contract is still not verified against a live/local Codex capture for the exact supported runtime mode.
- Phase 0 is still conceptually broad even with the second-pass warning; it should become explicit `0A skeleton` and `0B rename` tasks before execution.
- Hook/session correlation is still a design decision, not an implementation-ready contract.
- The plan has not yet been converted into small local issues with dependency order and acceptance tests.
- OpenAI/Codex pricing and cached-input semantics need a confirmed source before budget/cost enforcement can be trusted.

Subscores:

- Architecture direction: 9/10.
- Match to existing Clauditor code after second pass: 8/10.
- Implementation sequencing: 7.5/10.
- Testability: 8.5/10.
- Risk visibility: 8.5/10.
- Readiness for unattended implementation: 7/10.

To reach 9/10:

- Verify and document one real Codex request/response path.
- Decide MVP auth mode.
- Decide request compression handling.
- Split Phase 0 into explicit mechanical tasks.
- Convert the first vertical slices into `issues/*.md` using `to-issues` or `codex-ralph`.
- Choose `CachedInputEvent` vs adapted `CacheEvent`.

## Copy, Adapt, Do Not Copy

### Copy Mostly As-Is

- Workspace shape: Rust core crate, Rust CLI crate, Docker Compose, Envoy config, and test harness style.
- Ext_proc architecture: Envoy calls a local gRPC processor while forwarding traffic upstream.
- `/watch` SSE endpoint and event replay buffer.
- SQLite persistence pattern: immutable request rows plus derived aggregate/session tables.
- Metrics pattern: Prometheus exposition without session ids as labels.
- Tmux orchestration concept: one pane per active session, self-bootstrap when outside tmux.
- Diagnosis concept: in-memory session registry, per-turn snapshots, degradation analysis.
- Event model concept: `SessionStart`, `SessionEnd`, `ToolUse`, `ToolResult`, `ModelFallback`, `ContextStatus`, `RateLimitStatus`, warnings, and diagnostics.
- Failure posture: proxy observability must not become a hard dependency for model traffic.

### Adapt Deliberately

- Request parser:
  - From Anthropic `POST /v1/messages`.
  - To OpenAI/Codex `POST /v1/responses`.
  - Parse `model`, `instructions`, `input`, `tools`, `reasoning`, `prompt_cache_key`, and relevant `client_metadata`.
- Session identity:
  - From `hash(working_dir, full messages[0] text)`.
  - To Codex session/conversation identifiers first, then fallback to a stable hash of cwd plus first user input.
  - Prefer Codex's `session_id` request header when present.
- Response parser:
  - From Anthropic SSE event types such as `message_start`, `content_block_delta`, `message_delta`, and `message_stop`.
  - To OpenAI Responses SSE event types such as `response.created`, `response.output_item.added`, `response.output_item.done`, `response.output_text.delta`, `response.custom_tool_call_input.delta`, `response.completed`, `response.failed`, and `response.incomplete`.
- Token accounting:
  - From Anthropic input/output/cache-create/cache-read fields.
  - To OpenAI `input_tokens`, `cached_input_tokens`, `output_tokens`, `reasoning_output_tokens`, and `total_tokens`.
- Cache UX:
  - From Anthropic cache TTL and rebuild-cost countdowns.
  - To OpenAI cached-input visibility. Do not imply a TTL unless OpenAI exposes one.
- Model fallback:
  - From Anthropic `message.model` in the response.
  - To OpenAI response headers or payload metadata such as `openai-model` or `x-openai-model`.
- CLI wrapper:
  - From setting `ANTHROPIC_BASE_URL`.
  - To setting Codex config overrides such as `openai_base_url` or `model_providers.<id>.base_url`.
- Envoy upstream:
  - From Anthropic.
  - To OpenAI API-key mode first. Add ChatGPT-auth routing only after verifying the Codex traffic path.
- Hooks:
  - From Claude Code hooks endpoint.
  - To Codex hook payloads, likely via a small local hook helper that reads JSON on stdin and POSTs to Coditor.
- Pricing:
  - From Anthropic families.
  - To OpenAI/Codex model pricing, including cached input pricing and reasoning-output treatment.

### Do Not Copy Without Replacing

- `ANTHROPIC_BASE_URL` user checks and child command environment setup.
- Anthropic-only payload structs and assumptions in `parse_request_body`.
- Claude Code prompt cleanup regexes as the main session mechanism. Codex may have different preambles, and hook/session headers are stronger.
- Anthropic cache TTL countdown semantics.
- Anthropic quota-burn assumptions.
- Anthropic model-family pricing defaults.
- Anthropic Envoy host rewrite, SNI, and route naming.
- Any user-facing text that says Claude, Claude Code, Anthropic, Opus, Sonnet, Haiku, or cache-create/cache-read unless retained in compatibility notes.
- Any tests that assert Anthropic request/response event names.

## Coditor Invariants

Carry these forward or establish their Codex equivalent before implementation:

- Response-side ext_proc phases return `CONTINUE` immediately.
- Response body mode stays `STREAMED`.
- Envoy `failure_mode_allow` stays `true`.
- Strip `Accept-Encoding` so streamed SSE responses stay plaintext. Separately disable Codex request compression for MVP or implement request-body decompression explicitly.
- Broadcast watch events only after the ext_proc response to Envoy has been returned.
- `/watch` replays a short event history to avoid subscription races.
- `GET /watch?session=X` injects a synthetic `SessionStart` when the session registry knows the session.
- Request rows remain immutable.
- No `unwrap()` in ext_proc phase handlers.
- `/metrics` never blocks on per-session lookups and never uses session id as a label.
- Tmux dashboard self-bootstraps when requested and `TMUX` is unset.
- Every new watch event variant is mirrored in the core event enum, CLI event enum, renderer, and tmux handler.

## Target Architecture

```text
codex CLI/app
  -> local Coditor proxy URL configured through Codex config
  -> Envoy listener
  -> ext_proc gRPC to coditor-core
  -> upstream OpenAI Responses API or Codex backend

coditor-core
  -> parses request metadata
  -> streams response SSE parsing off the hot path
  -> writes immutable request rows and aggregate session rows
  -> broadcasts WatchEvent values
  -> exposes /watch, /metrics, /api/sessions, /api/diagnosis

coditor-cli
  -> starts/wraps Codex commands
  -> follows /watch
  -> renders inline and tmux views
  -> optionally installs/prints Codex hook config
```

## Relevant Global Skills

These skills should be referenced by implementation agents when available:

- `codex-primary-runtime`: Use when available for Codex-runtime conventions. Current local folder exists at `/Users/pradeepsingh/.codex/skills/codex-primary-runtime`, but it has no `SKILL.md` content yet, so the plan must not depend on it.
- `zoom-out`: Use before major source migration to keep the architecture map accurate.
- `to-prd`: Use to turn this plan into a PRD if the project needs product-level tracking.
- `to-issues`: Use after this plan is accepted to split work into vertical issue slices.
- `codex-ralph`: Use for local-file issue execution if the project wants a no-GitHub AFK loop using `issues/*.md`.
- `request-refactor-plan`: Use when extracting generic modules from Clauditor or when reshaping the copied code into provider-neutral pieces.
- `improve-codebase-architecture`: Use after the first working Codex slice to identify deep-module opportunities and reduce copied-code friction.
- `design-an-interface`: Use for the provider parser/accumulator interfaces before adding more than one Codex traffic mode.
- `domain-model`: Use if terms like Session, Turn, Tool Call, Cached Input, and Reasoning Token become ambiguous.
- `ubiquitous-language`: Use early to create `UBIQUITOUS_LANGUAGE.md` for Coditor terms.
- `grill-me-plan`: Use before implementation starts to stress-test this plan with user decisions.
- `grill-me`: Use for deeper challenge sessions when trade-offs are unclear.
- `tdd`: Use for parser, SSE accumulator, session identity, and watch-event behavior.
- `qa`: Use for manual QA sessions once a dev server/proxy is running.
- `setup-pre-commit`: Use after repo scaffolding to add formatting and test hooks.
- `git-guardrails-codex`: Use immediately after repo initialization to create project-local guardrails.
- `github-triage` and `triage-issue`: Use only if work moves into GitHub issues.
- `edit-article`: Use for polishing README, docs, ADRs, or release notes, not for code.
- `scaffold-exercises`: Not needed for the core product unless Coditor later includes training material.
- `write-a-skill`: Use only if Coditor needs its own reusable Codex skill.
- `spotvortex-brief`, `spotvortex-policy-audit`, `spotvortex-queue-curator`: Not relevant to Coditor unless this repo is later integrated with Spotvortex workflows.

## Phase 0: Repo Bootstrap

### Scope

Create the Coditor repo skeleton from Clauditor without changing product semantics yet.

### Work

- Initialize `/Users/pradeepsingh/code/coditor` as a git repo when the user approves.
- Copy Clauditor's Rust workspace structure:
  - core crate
  - CLI crate
  - Envoy config directory
  - Docker Compose file
  - test directory
  - wiki/docs structure
- Rename package, binary, service, and Docker identifiers from Clauditor to Coditor.
- Add a root `AGENTS.md` for Coditor-specific invariants.
- Add `UBIQUITOUS_LANGUAGE.md` with initial terms.
- Add `README.md` with a short "not implemented yet" warning and local dev commands.
- Add git guardrails and optional pre-commit hooks.

### Copy From Clauditor

- Workspace layout.
- Rust crate setup.
- Docker Compose shape.
- Envoy ext_proc filter shape.
- Test script style.
- Watch and tmux module structure.

### Do Not Copy Yet

- Anthropic payload tests as passing tests.
- Published install instructions.
- Any claim that the proxy already works with Codex.

### Definition of Done

- `git status` shows only intentional new Coditor files.
- `cargo metadata` succeeds.
- Crates and binaries are named Coditor.
- `coditor --help` builds and prints Coditor naming.
- `AGENTS.md` captures Coditor invariants and explicitly says Anthropic behavior is not authoritative.
- No user-facing strings refer to Clauditor except in attribution/history docs.

## Phase 1: Establish Codex/OpenAI Traffic Contract

### Scope

Verify the exact Codex request path and build a fake OpenAI Responses fixture before touching live traffic.

### Work

- Document Codex config options for proxying:
  - `openai_base_url`
  - `model_providers.<id>.base_url`
  - request compression feature flag
  - ChatGPT-auth vs API-key mode
- Capture or construct representative `POST /v1/responses` request JSON.
- Capture or construct representative Responses SSE events:
  - `response.created`
  - `response.output_item.added`
  - `response.output_text.delta`
  - `response.custom_tool_call_input.delta`
  - `response.output_item.done`
  - `response.completed`
  - failure/incomplete variants
- Identify required request and response headers:
  - `session_id`
  - `x-client-request-id`
  - `openai-model`
  - `x-openai-model`
  - rate-limit headers
- Decide whether Coditor MVP supports only API-key mode or both API-key mode and ChatGPT-auth mode.

### Source Guidance

- Replace Clauditor's `parse_request_body` contract at `/Users/pradeepsingh/code/clauditor/clauditor-core/src/main.rs:3288`.
- Replace `ResponseAccumulator` at `/Users/pradeepsingh/code/clauditor/clauditor-core/src/main.rs:1852`.
- Preserve the ext_proc phase shape in `process` at `/Users/pradeepsingh/code/clauditor/clauditor-core/src/main.rs:4212`.

### Definition of Done

- `docs/codex-traffic-contract.md` exists.
- Fake Responses request and SSE fixtures exist under `test/fixtures/`.
- The MVP auth/routing mode is explicitly chosen.
- Compression handling is decided: disabled for MVP or supported explicitly.
- There is a failing test or pending test that describes parsing a fake Responses turn end to end.

## Phase 2: Request Parsing and Session Identity

### Scope

Replace Anthropic request parsing with Codex/OpenAI request parsing.

### Work

- Add Codex request structs for Responses API fields Coditor needs.
- Extract:
  - requested model
  - cwd/working directory if available from metadata or hooks
  - first user-visible input
  - tool availability
  - reasoning effort/config
  - prompt cache key
  - session/conversation header
- Create Coditor session identity rules:
  - primary: Codex `session_id` header if present
  - secondary: Codex hook `session_id` if correlated
  - fallback: hash of cwd plus first user-visible input
- Keep request-body parsing lightweight and synchronous.
- Add unit tests for request variants:
  - plain text input
  - structured input array
  - tool-enabled request
  - no cwd present
  - missing session header
  - malformed body

### Copy From Clauditor

- Parsed request lifecycle shape.
- Budget-gate placement if retained.
- Session registry insertion concept.
- Prompt cleaning as a pattern, not the Claude regex list.

### Do Not Copy

- Anthropic `messages[0].content` assumptions.
- Claude Code boilerplate stripping as the identity mechanism.
- `Primary working directory` parsing as the only cwd source.

### Definition of Done

- Anthropic request parsing is no longer on the Coditor hot path.
- Session id is stable across chunks of one Codex session.
- Parallel fake Codex sessions produce distinct session keys.
- No ext_proc phase handler uses `unwrap()`.
- Request parser tests pass.

## Phase 3: Responses SSE Accumulator

### Scope

Parse OpenAI Responses SSE events into Coditor's internal turn summary.

### Work

- Implement a Codex Responses accumulator with support for:
  - text deltas
  - reasoning summary/text deltas where available
  - custom tool call input deltas
  - output item added/done
  - completed usage
  - failed/incomplete events
  - response model metadata from headers/payload
- Map OpenAI usage to Coditor usage:
  - input tokens
  - cached input tokens
  - output tokens
  - reasoning output tokens
  - total tokens
- Preserve response streaming behavior:
  - never buffer full SSE before parsing
  - never block Envoy broadcast path
  - return `CONTINUE` immediately for response-side ext_proc phases
- Add fixture-driven tests for each event type.

### Copy From Clauditor

- Incremental SSE chunk parsing structure.
- Deferred watch event concept.
- Frustration-signal scanning concept if still useful.
- Model fallback watch event concept.

### Do Not Copy

- Anthropic event names.
- Anthropic token field names.
- Anthropic tool-use block assumptions.

### Definition of Done

- Fake Responses SSE stream produces a complete turn summary.
- `response.completed.usage` drives token metrics.
- `cached_input_tokens` is captured separately.
- Reasoning output tokens are captured when present.
- Model fallback fires when served model differs from requested model.
- Tests cover split SSE frames across chunks.

## Phase 4: Finalization, Persistence, and Pricing

### Scope

Make end-of-turn finalization correct for Codex.

### Work

- Port `finalize_response` structure from Clauditor.
- Rename and adjust domain fields:
  - cache read/create -> cached input tokens
  - Claude model families -> OpenAI/Codex models
  - Anthropic quota -> OpenAI usage/rate-limit view
- Extend SQLite schema only where needed.
- Implement OpenAI pricing:
  - uncached input
  - cached input
  - output
  - reasoning output treatment
- Avoid double counting cached input tokens. Treat cached input as a subset of total input unless OpenAI traffic verification proves otherwise.
- Keep request rows immutable.
- Keep aggregate/session tables repairable via upsert.
- Add migration tests or schema smoke tests.

### Copy From Clauditor

- Session start/end finalization flow.
- SQLite writer pattern.
- Diagnosis snapshot generation.
- Context-status heuristic structure.
- Metrics update placement.

### Do Not Copy

- Anthropic cache TTL and rebuild-cost meaning.
- Anthropic weekly quota burn assumptions.
- Anthropic family fallback table.

### Definition of Done

- A fake Codex request/response writes one immutable request row.
- Session aggregate updates after each turn.
- Cost estimate is nonzero for known OpenAI/Codex models and explicitly unknown for unknown models.
- Cache event wording does not imply TTL.
- Context status broadcasts each completed turn.
- Prometheus metrics expose provider/model/status labels only at bounded cardinality.

## Phase 5: Envoy and Runtime Wiring

### Scope

Route Codex traffic through Coditor safely.

### Work

- Create Envoy config for OpenAI API-key mode:
  - local listener
  - ext_proc filter
  - TLS upstream to `api.openai.com`
  - host rewrite and SNI set correctly
  - `failure_mode_allow: true`
  - response body mode `STREAMED`
- Add a separate, clearly marked experimental config for ChatGPT-auth Codex backend if needed.
- Strip or neutralize compression where required.
- Add Docker Compose services with Coditor names.
- Add health checks and startup logs that show selected upstream.

### Copy From Clauditor

- Ext_proc filter skeleton.
- gRPC cluster to core.
- Failure-open posture.
- Docker Compose shape.

### Do Not Copy

- Anthropic upstream host, SNI, or route names.
- Anthropic-only request path assumptions.

### Definition of Done

- Docker Compose starts Coditor core and Envoy.
- `curl` against the local proxy can reach a fake upstream.
- If Coditor core is stopped, Envoy still forwards traffic.
- SSE response remains streamed through Envoy.
- Compression behavior is tested or forcibly disabled for MVP.

## Phase 6: CLI Wrapper and Watch UI

### Scope

Make `coditor` useful from the command line.

### Work

- Replace Clauditor CLI branding and commands.
- Replace `ANTHROPIC_BASE_URL` checks with Codex config checks.
- Add `coditor run -- codex ...` or equivalent wrapper that:
  - points Codex at local Coditor proxy
  - disables request compression for MVP if needed
  - preserves user environment
  - clearly prints proxy and watch URLs
- Port `coditor watch` inline rendering.
- Port `coditor watch --tmux`.
- Adjust event labels:
  - cached input instead of cache read/create
  - reasoning tokens
  - Codex model names
  - Codex session ids

### Copy From Clauditor

- CLI command shape.
- Watch renderer structure.
- Tmux bootstrap and pane management.
- Session-filter URL behavior.

### Do Not Copy

- `ANTHROPIC_BASE_URL` checks.
- Claude-specific model labels and cache language.
- User-facing examples using `claude`.

### Definition of Done

- `coditor doctor` or equivalent reports Codex proxy config status.
- `coditor run -- codex exec "..."` can route to fake upstream in tests.
- `coditor watch` renders fake session events.
- `coditor watch --tmux` self-bootstraps and shows one pane per session.
- CLI event enum matches core event enum.

## Phase 7: Codex Hooks Integration

### Scope

Use Codex hooks to improve tool/session fidelity where proxy traffic alone is insufficient.

### Work

- Add `/api/hooks/codex`.
- Add a small hook helper command if Codex hooks execute shell commands with JSON stdin.
- Parse hook events for:
  - session start
  - tool start
  - tool finish
  - cwd
  - model
  - permission mode/source where useful
- Correlate hook session ids with proxy session ids.
- Decide conflict rules when hook metadata and proxy metadata disagree.
- Add CLI command to print or install suggested hook config, without silently editing user config unless requested.

### Copy From Clauditor

- Hook endpoint shape.
- Watch event emission for tools, skills, and MCP-like events where applicable.
- Correlation through the session registry.

### Do Not Copy

- Claude Code hook payload names.
- Claude-specific tool classification.

### Definition of Done

- Fake Codex hook payload produces a `SessionStart`.
- Fake tool start/end payloads produce `ToolUse` and `ToolResult`.
- Hook events and proxy events coalesce into the same session, or the design explicitly documents why hook sessions are separate.
- Duplicate events are suppressed or rendered intentionally.
- Hook endpoint failures do not affect model traffic.

## Phase 8: Diagnostics, Rate Limits, and Context Intelligence

### Scope

Make Coditor useful for understanding degradation, not just logging requests.

### Work

- Adapt diagnosis rules to Codex:
  - high context fill
  - repeated tool failures
  - incomplete/failed responses
  - model mismatch
  - heavy reasoning token usage
  - low cached-input reuse on repeated context
- Parse OpenAI rate-limit headers where available.
- Emit `RateLimitStatus` events without using session ids as metric labels.
- Keep context projections clearly labeled as heuristic.
- Add `/api/diagnosis/<session_id>` and session listing endpoints.

### Copy From Clauditor

- Diagnosis endpoint shape.
- Session snapshot concept.
- Rate-limit watch event concept.
- Context-status watch event concept.

### Do Not Copy

- Anthropic subscription quota burn logic unless there is a verified Codex equivalent.
- Anthropic-specific fallback naming.

### Definition of Done

- Diagnosis endpoint returns useful output for fake sessions.
- Rate-limit headers produce watch events in fixture tests.
- Context status is emitted every completed turn.
- No metrics contain session id labels.
- Docs explain which signals are observed and which are heuristic.

## Phase 9: End-to-End Tests and Manual QA

### Scope

Prove the proxy, parser, persistence, watch, and CLI work together.

### Work

- Build fake OpenAI Responses upstream.
- Add `test/e2e.sh` equivalent:
  - start Docker Compose
  - send fake Responses request through Envoy
  - stream fake SSE response
  - assert core logs/events/db rows
- Add `test/parallel-sessions.sh` equivalent for Codex:
  - N parallel fake `/v1/responses` requests
  - distinct session ids or first user inputs
  - assert distinct session keys
- Add watch replay race test.
- Add failure-open test.
- Add CLI smoke tests.
- Manually test real Codex only after fake traffic is solid.

### Copy From Clauditor

- Test harness style.
- Parallel session regression concept.
- Fake upstream pattern.

### Do Not Copy

- Fake Anthropic payloads.
- Anthropic e2e assertions.

### Definition of Done

- Unit tests pass.
- Fake e2e passes without OpenAI credentials.
- Parallel session test passes for at least 4 sessions.
- Watch replay test proves late subscribers receive recent events.
- Failure-open test proves traffic still reaches upstream if core is unavailable.
- One real Codex smoke test is documented with date, config, and outcome.

## Phase 10: Documentation and Release Readiness

### Scope

Prepare Coditor for repeated local use and future contributors.

### Work

- Write README:
  - what Coditor is
  - supported Codex modes
  - installation
  - run/watch/tmux examples
  - limitations
  - privacy/security notes
- Write architecture docs based on Clauditor's wiki but Codex-native.
- Write troubleshooting docs:
  - Envoy logs
  - core logs
  - Codex config
  - compression symptoms
  - auth mode mismatch
  - hook config
- Add ADRs:
  - separate repo from Clauditor
  - API-key mode first vs ChatGPT-auth mode
  - session identity strategy
  - hook/proxy correlation strategy
- Add changelog and license review.

### Copy From Clauditor

- Architecture doc outline.
- Observability/troubleshooting categories.
- Local command style.

### Do Not Copy

- Claude-specific diagrams without redrawing them.
- Claims about cache TTL, Claude Code hooks, or Anthropic quota behavior.

### Definition of Done

- A new user can run the fake e2e from README instructions.
- A new user can configure Codex for the supported MVP mode.
- Architecture docs match the code.
- Known limitations are explicit.
- ADRs record the hard-to-reverse decisions.

## Recommended First Vertical Slices

Use `to-issues` or `codex-ralph` to turn these into local `issues/*.md` files:

1. Bootstrap Coditor workspace and rename binaries.
2. Add fake OpenAI Responses fixture and failing parser test.
3. Implement Responses request parser and session identity.
4. Implement Responses SSE accumulator for completed text-only turn.
5. Persist completed fake turn and broadcast `SessionStart` plus `ContextStatus`.
6. Add fake Envoy e2e through Docker Compose.
7. Replace CLI wrapper with Codex config override.
8. Add Codex hook endpoint and fake hook tests.
9. Add tmux rendering adjustments for Codex events.
10. Add real Codex smoke-test documentation.

Each slice should be demoable on its own and should leave the repo building.

## Open Decisions

Resolve these before coding beyond Phase 1:

- MVP auth mode: OpenAI API key only, ChatGPT-auth Codex backend only, or both.
- Compression: disable Codex request compression for MVP or support decompression in core.
- Session id precedence: header-only, hook-only, or merged precedence order.
- Hook installation: print instructions, write config on request, or wrapper-managed temporary config.
- Pricing source: hard-coded defaults, config file only, or both.
- Naming of cache events: keep `CacheEvent` for UI continuity or introduce `CachedInputEvent`.
- Whether to retain frustration-signal analysis for Codex text output.

## Risks

- Codex traffic path may differ between API-key mode and ChatGPT-auth mode.
- Codex config or hook schemas can change quickly; pin tested versions in docs.
- Request compression can make proxy parsing fail if not disabled or decoded.
- Tool telemetry may be incomplete from Responses SSE alone.
- OpenAI cached-input accounting does not provide the same TTL story as Anthropic cache events.
- Copying too much Claude-specific language will create misleading UI.
- Provider-general abstractions added too early may slow down the Codex MVP.

## Success Criteria

Coditor is ready for daily dogfood when:

- `coditor run -- codex ...` routes a real Codex session through the proxy.
- `coditor watch` shows live Codex session events with token usage and model metadata.
- Cached input tokens and reasoning tokens are visible when present.
- Tool events are visible through hooks or verified Responses events.
- Fake e2e and parallel-session tests pass without OpenAI credentials.
- Real smoke-test steps are documented and repeatable.
- The README states exactly which Codex auth/runtime modes are supported.
