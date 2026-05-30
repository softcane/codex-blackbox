# Codex Blackbox Architecture

This document is the top-level map for agents and humans working on Codex
Blackbox. It explains the durable shape of the system and points to the
implementation files that own each behavior. The repository `AGENTS.md` stays
short and links here when deeper context is needed.

## Product Boundary

Codex Blackbox observes Codex CLI traffic through a local Envoy proxy and
records authoritative model-turn evidence. The calibrated v1 product surface is
the proxy/core observer plus CLI doctor, up, run, watch, status, guard,
postmortem, and config workflows. Codex Blackbox does not use local JSON
stdout, Codex lifecycle hooks, app-server hook endpoints, Codex Desktop/UI
traffic, WebSocket frames, or inferred tool outcomes as authoritative
telemetry.

`codex-blackbox run -- codex ...` uses command-line config overrides and must
not mutate `~/.codex/config.toml`. Product support claims are limited to real
Codex Responses evidence persisted by core as `provider="codex_responses"`.
Local HTTP request decoding, including `content-encoding: zstd`, exists to keep
the proxy path parseable; it is not a Desktop/UI support claim.

The remote compaction path `/backend-api/codex/responses/compact` bypasses
ext_proc body inspection. Compaction requests are large control-plane payloads,
not model-turn telemetry, and buffering them can trigger Envoy's local 413
request-size protection before the upstream sees the request.

Hosted web/cloud Codex, Codex Desktop/UI observation, API-key Codex routing,
generic system proxying, TLS MITM, WebSocket frame observation, JSON stdout,
app-server callbacks, and tool-result telemetry are out of scope.

## Decision Flow

The internal decision stack uses one event pipeline:

```text
proxy collector      -> normalized events
offline transcript   -> normalized events, offline only
future app-server    -> normalized events, future client mode
user policy          -> normalized events

normalized events -> session state -> signal engine -> decision engine

decision engine -> CLI status/guard/watch
decision engine -> redacted local JSON API
decision engine -> postmortem
decision engine -> bounded Prometheus metrics
```

Every normalized event includes evidence source, timestamp, category,
bounded reason code, confidence, session/turn references when available, and a
privacy classification. Payload summaries are derived and redacted by default.

The v1 signal engine covers failed, incomplete, and unknown model responses,
high context, accounting anomalies, model fallback, untrusted pricing when a
dollar budget is configured, rate-limit pressure when available, and missing
durable evidence.

The decision states are `Healthy`, `Watching`, `Careful`, `Stop`, `Blocked`,
`Cooldown`, and `Ended`. The same serialized decision object is reused by CLI
status, guard, watch, local JSON APIs, postmortems, and Grafana-facing metrics.

Evidence classes must stay separate:

- Fake fixtures validate local contracts.
- Static checks validate configuration without launching a model turn.
- Dogfood and live smoke evidence require real Codex traffic observed by
  `codex-blackbox-core` with `provider="codex_responses"`.

No Desktop/UI support claim is part of the calibrated product scope. Fake
fixtures and local dry runs prove local contracts only.

## Runtime Flow

1. The CLI wrapper starts Codex with command-line config overrides.
2. Codex sends ChatGPT/Codex model traffic through Envoy.
3. Envoy `ext_proc` sends request and response bodies to
   `codex-blackbox-core`.
4. Core parses Responses-shaped request JSON and streamed response SSE.
5. Core computes token accounting, pricing status, watch events, persistence,
   metrics, and deterministic postmortems.
6. The CLI renders watch streams, session summaries, billing reconciliation
   commands, and postmortem reports.

## Module Map

Core parser and accounting modules:

- `codex-blackbox-core/src/codex_request.rs`: Responses request parsing,
  first prompt extraction, cwd extraction, and session identity precedence.
- `codex-blackbox-core/src/codex_response.rs`: SSE accumulation, terminal
  status extraction, usage extraction, served model precedence, and tool-call
  intent extraction.
- `codex-blackbox-core/src/codex_accounting.rs`: local Codex token rules,
  anomalies, failure/incomplete detail, and pricing status.
- `codex-blackbox-core/src/pricing.rs`: built-in model pricing catalog and
  unknown-model behavior.

Core runtime modules:

- `codex-blackbox-core/src/main.rs`: Envoy `ext_proc` runtime, SQLite schema,
  persistence, HTTP API, watch replay, background repairs, and application
  wiring.
- `codex-blackbox-core/src/watch.rs`: watch event types and broadcaster replay.
- `codex-blackbox-core/src/metrics.rs`: bounded Prometheus metric families and
  label normalization.
- `codex-blackbox-core/src/diagnosis.rs`: deterministic session diagnosis
  from Envoy-observed turn snapshots.
- `codex-blackbox-core/src/postmortem.rs`: deterministic postmortem report
  construction and redaction.
- `codex-blackbox-core/src/coach.rs`: internal normalized event model, derived
  session state, signal engine, and decision conversion.

CLI modules:

- `codex-blackbox-cli/src/main.rs`: command parsing, stack management,
  wrapper run plan, watch client, status/guard/postmortem/config commands, and
  terminal rendering.
- `codex-blackbox-cli/src/coach_commands.rs`,
  `codex-blackbox-cli/src/baseline_commands.rs`, and
  `codex-blackbox-cli/src/tmux.rs`: dormant implementation code behind
  internal feature gates that default to disabled and are not part of the
  default product surface.

Config and harness modules:

- `envoy/envoy.yaml`: default ChatGPT/Codex proxy route.
- `docker-compose.yml`: local stack.
- `test/validate-openai-config.sh`: static Envoy/Compose contract checks.
- `test/e2e-openai-responses-full.sh`: local fake Responses regression.
- `test/dogfood-codex-sessions.sh`: explicitly real dogfood harness.
- `test/harness-fast.sh`: fast local agent harness gate.

## Architectural Invariants

- Parse request and response shapes at the boundary before using their fields.
- Keep request rows immutable; repair derived session totals and summaries.
- Treat cached input as a subset of input tokens, never as extra tokens.
- Preserve completed, failed, incomplete, and unknown terminal statuses.
- Treat tool calls as model-side intent only.
- Keep disabled hook evidence out of the default product surface. If dormant
  code is re-enabled internally, hook evidence must remain advisory and visibly
  labeled separately from proxy evidence.
- Do not mutate user-level Codex config for the `run codex` wrapper path or
  for support claims.
- Keep Prometheus labels bounded; never label metrics with session ids, cwd,
  prompts, request ids, response ids, or raw tool inputs.
- Keep unsupported surfaces absent from watch, metrics, persistence, and public
  docs.
- Keep dormant baseline code unreachable from the default product surface.

## Known Structural Debt

Two files currently carry too much responsibility:

- `codex-blackbox-core/src/main.rs`
- `codex-blackbox-cli/src/main.rs`

Do not split them opportunistically during unrelated feature work. When there
is time for architecture work, move one behavior at a time into focused modules
with tests preserved at each step. Good seams are:

- Core: `ext_proc`, `http_api`, `persistence`, `finalization`, `watch_replay`,
  `runtime_state`.
- CLI: `run_plan`, `stack`, `watch_client`, `postmortem_render`, `config`.

## Verification Ladder

Use the narrowest verification that matches the change:

- Parser/accounting change: targeted Rust tests plus
  `codex_responses_contract` when relevant.
- Runtime/persistence/watch/metrics change: targeted Rust tests plus
  `codex_envoy_only_surface` when relevant.
- CLI wrapper change: CLI unit tests and `codex-blackbox-cli/tests/cli_smoke.rs`.
- Envoy or local-stack change: `./test/validate-openai-config.sh`.
- Cross-service fake contract change: `./test/e2e-openai-responses-full.sh`.
- Live support claim: real dogfood or smoke evidence, explicitly requested.
