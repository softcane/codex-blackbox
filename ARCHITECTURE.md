# Codex Blackbox Architecture

This document is the top-level map for agents and humans working on Codex
Blackbox. It explains the durable shape of the system and points to the
implementation files that own each behavior. The repository `AGENTS.md` stays
short and links here when deeper context is needed.

## Product Boundary

Codex Blackbox observes Codex CLI traffic through a local Envoy proxy and
records authoritative model-turn evidence. Experimental UI mode can explicitly
route local Codex Desktop and local IDE extension app-server traffic through the
same Envoy/core pipeline by writing reversible user-level Codex config. The
optional hook coach records supported Codex lifecycle hook evidence for
coaching only; hook evidence is incomplete, labeled separately, and never
replaces proxy evidence. Codex Blackbox does not use local JSON stdout,
app-server hook endpoints, or inferred tool outcomes as authoritative telemetry.

UI mode is explicit only. `codex-blackbox run -- codex ...` keeps using
command-line config overrides and must not mutate `~/.codex/config.toml`.
`codex-blackbox ui enable` is the mutating entry point, and disable uses
Blackbox-owned rollback state instead of blindly restoring a whole backup file.
Persistent UI mode must preserve Codex Desktop's provider identity: it sets
`openai_base_url` and disables request compression, but it must not set
`chatgpt_base_url` or replace `model_provider` with a Blackbox-specific id.
Changing the provider id hides existing Desktop sessions because app-server
thread listing defaults to the active provider. Current safe UI mode preserves
that provider identity, but Codex Desktop may still attempt Responses over
WebSocket. If the app does not fall back to HTTP Responses after Envoy returns
426, `ui status` reports `websocket_only_unobservable`; seamless UI observation
then requires a future WebSocket relay instead of another restart.
If Envoy sees HTTP Responses POST traffic but core does not persist
`provider="codex_responses"` evidence, `ui status` reports
`http_responses_unparsed`; that is a parser/body-shape problem, not a config
reload problem.
Core decodes `content-encoding: zstd` request bodies before parsing Responses
JSON so app-server compression does not block observation.

The remote compaction path `/backend-api/codex/responses/compact` bypasses
ext_proc body inspection. Compaction requests are large control-plane payloads,
not model-turn telemetry, and buffering them can trigger Envoy's local 413
request-size protection before the upstream sees the request.

UI mode is scoped to local Codex Desktop and local IDE extension app-server
traffic. Hosted web/cloud Codex, API-key Codex routing, generic system proxying,
TLS MITM, WebSocket frame observation, JSON stdout, app-server callbacks, and
tool-result telemetry are out of scope for UI mode.

## Coach And Companion Flow

The coach stack uses one event pipeline:

```text
proxy collector      -> normalized events
hook collector       -> normalized events
offline transcript   -> normalized events, offline only
future app-server    -> normalized events, future client mode
user policy          -> normalized events

normalized events -> session state -> signal engine -> decision engine

decision engine -> CLI status/guard/watch
decision engine -> hook coach response
decision engine -> companion UI/API
decision engine -> postmortem
decision engine -> bounded Prometheus metrics
```

Every normalized event includes evidence source, timestamp, category,
bounded reason code, confidence, session/turn references when available, and a
privacy classification. Payload summaries are derived and redacted by default.

The v1 signal engine covers failed, incomplete, and unknown model responses,
high context, repeated validation failure, unvalidated edits, repeated
supported-tool failure, blind retry, risky supported tool calls, untrusted
pricing when a dollar budget is configured, rate-limit pressure when available,
and missing durable evidence.

The decision states are `Healthy`, `Watching`, `Careful`, `Stop`, `Blocked`,
`Cooldown`, and `Ended`. The same serialized decision object is reused by CLI
status, guard, watch, hook coach responses, the companion API, and postmortems.

Evidence classes must stay separate:

- Fake fixtures validate local contracts.
- Preflight checks validate configuration without launching a model turn.
- Dogfood and live smoke evidence require real Codex traffic observed by
  `codex-blackbox-core` with `provider="codex_responses"`.

Fake UI fixtures and local dry runs prove local contracts only. Live UI support
claims require a real Desktop/IDE smoke with persisted
`provider="codex_responses"` traffic; `ui status` must not count known local
fake fixture sessions as live UI evidence.

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
- `codex-blackbox-core/src/coach.rs`: normalized event model, derived session
  state, signal engine, and companion decision conversion.

CLI modules:

- `codex-blackbox-cli/src/main.rs`: command parsing, stack management,
  wrapper run plan, watch client, sessions/recall/postmortem/reconcile
  commands, and terminal rendering.
- `codex-blackbox-cli/src/ui_config.rs`: experimental UI config
  planning/apply/disable, TOML-aware mutation, backup/state handling, and
  config inspection.
- `codex-blackbox-cli/src/ui_status.rs`: experimental UI status
  classification from config state, process hints, recent Envoy WebSocket 426
  access logs, and existing core observation evidence.
- `codex-blackbox-cli/src/ui_process.rs`: local Codex Desktop/app-server
  process detection for restart warnings only.
- `codex-blackbox-cli/src/ui_launch.rs`: safe platform launch planning for
  starting or focusing Codex Desktop without killing or restarting processes.
- `codex-blackbox-cli/src/coach_commands.rs`: explicit project-local hook
  preview, install, status, uninstall, and fail-open hook handler command.
- `codex-blackbox-cli/src/baseline_commands.rs`: optional derived-only baseline
  preview, learn, show, reset, and disable commands.
- `codex-blackbox-cli/src/tmux.rs`: tmux watch orchestration and per-session
  pane state.

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
- Keep hook evidence advisory and visibly labeled as `hook`; supported
  `PostToolUse` output may prove hook-observed result evidence but cannot prove
  proxy tool intent succeeded.
- Keep UI mode reversible and explicit; do not use UI config writes for the
  `run codex` wrapper path.
- Keep Prometheus labels bounded; never label metrics with session ids, cwd,
  prompts, request ids, response ids, or raw tool inputs.
- Keep unsupported surfaces absent from watch, tmux, metrics, persistence, and
  public docs.
- Keep baseline learning derived-only; never store raw prompts, outputs,
  commands, paths, tool inputs, secrets, or full transcripts.

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
