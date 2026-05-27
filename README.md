# Codex Blackbox

Codex can look productive while the useful evidence is buried in streaming
traffic, terminal scrollback, and token accounting.

Codex Blackbox makes a Codex run inspectable. It records each observed model
turn, shows whether the session is healthy, and produces a postmortem you can
act on instead of guessing what happened.

It answers the questions that matter after or during a run:

- Did Codex actually send model traffic through the observed path?
- Did the response complete, fail, or end incomplete?
- Which model was requested, and which model served the turn?
- How many tokens were used, including cached and uncached input?
- Is context pressure becoming the next problem?
- What is the next practical move?

## Start

Install:

```bash
curl https://raw.githubusercontent.com/softcane/codex-blackbox/main/install.sh | sh
```

Start the local stack:

```bash
codex-blackbox doctor
codex-blackbox up
```

Run Codex through Codex Blackbox:

```bash
codex-blackbox run codex
```

Read the live decision:

```bash
codex-blackbox status
```

Open the local companion:

```bash
open http://localhost:9091/companion
```

Read the postmortem:

```bash
codex-blackbox postmortem last
```

## Live Decision

`status` gives one line you can use while the run is active.

```text
codex-blackbox: Healthy | context 31% | continue
codex-blackbox: Careful | served model changed | narrow next prompt
codex-blackbox: Stop | response incomplete | inspect postmortem
codex-blackbox: Ended | 2 turns, 38K tokens | read postmortem
```

The live decision is deliberately small:

- `Healthy`: keep going.
- `Watching`: no model-turn evidence yet.
- `Careful`: continue narrowly.
- `Stop`: inspect before spending another turn.
- `Blocked`: local policy stopped the next request.
- `Cooldown`: wait before retrying.
- `Ended`: the session is ready for review.

The same decision object is used by `status`, `guard`, `watch`, the companion
API, hook coach responses, and postmortems.

## Companion

The companion UI is a local browser surface for one active Codex session. It is
served by `codex-blackbox-core`:

```bash
codex-blackbox up
open http://localhost:9091/companion
```

It shows active sessions, state, next action, evidence labels, timeline,
signals, token/context pressure, validation history, coach actions, and a
redacted postmortem link. It does not show raw prompts, outputs, commands,
paths, or tool arguments by default.

## Postmortem

The postmortem is the durable record of the run.

```bash
codex-blackbox postmortem last
```

It shows the session outcome, model route, token usage, cached input, context
fill, pricing trust, response status, and a per-turn Flight Recorder.

Cached input is part of input tokens. Reasoning tokens are output detail. Local
total tokens are input plus output.

## Guard

Guard answers whether the next Codex request should continue, warn, or block
from trusted local evidence:

```bash
codex-blackbox guard --json
```

Runtime guard enforcement is next-request-only. It cannot interrupt a response
that is already streaming. Token budgets block only with trusted session
evidence, and dollar budgets block only when pricing is trusted. Unknown or
untrusted pricing stays advisory.

Supported policy fields:

```toml
session_token_budget = 200000
session_cost_budget_dollars = 10.00
context_warn_percent = 70
context_block_percent = 85
failed_response_warn_count = 1
failed_response_block_count = 1
incomplete_response_warn_count = 1
incomplete_response_block_count = 1
unknown_response_warn_count = 1
unknown_response_block_count = 1
accounting_anomaly_warn_count = 1
accounting_anomaly_block_count = 1
model_mismatch_warn = true
model_mismatch_block = false
```

## Hook Coach

The hook coach is explicit and reversible. It uses Codex lifecycle hooks as
advisory hook evidence while keeping proxy-observed Responses traffic as the
durable authority for model turns.

```bash
codex-blackbox coach preview
codex-blackbox coach install
codex-blackbox coach status
codex-blackbox coach uninstall
```

Installed project-local hooks cover `PreToolUse`, `PostToolUse`,
`UserPromptSubmit`, and `Stop`. Warnings are the default. Blocks are limited to
high-confidence supported risky tool calls, such as destructive shell commands
seen by `PreToolUse`. Hook failures fail open by returning `{"continue":true}`.

Hook evidence can show supported tool starts/results and validation attempts,
but it is incomplete and labeled as `hook`. Proxy tool-call events remain
model-side intent only, never tool success.

## Baseline Learning

Baseline learning is optional and derived-only:

```bash
codex-blackbox baseline preview
codex-blackbox baseline learn
codex-blackbox baseline show
codex-blackbox baseline reset
codex-blackbox baseline disable
```

Stored facts are bounded categories such as validation category frequency,
common tool categories, typical context range, repeated failure reason codes,
and recovery pattern categories. Baselines do not store raw prompts, model
outputs, commands, tool inputs, paths, secrets, or transcripts.

## What Gets Recorded

Codex Blackbox records model-turn evidence observed from Codex Responses
traffic:

- session and turn identity
- completed, failed, incomplete, or unknown response status
- requested and served model
- input, cached input, uncached input, output, reasoning, and local total tokens
- context fill and accounting anomalies
- model-side tool-call intent
- normalized proxy, hook, transcript/offline, future app-server, and user-policy
  event categories
- redacted prompt and response summaries when available

Tool calls are observed intent from the model stream. They are not proof that a
local tool result succeeded.

Supported hook `PostToolUse` output can add hook-result evidence, but it is
always labeled as hook evidence and never replaces proxy evidence.

## Why The Evidence Is Trustworthy

Codex Blackbox treats observed Codex Responses traffic as the source of truth
for durable model turns. It does not turn local logs, hook evidence, transcript
audits, or fake fixtures into live support claims.

Fake Responses fixtures prove local parser and UI contracts. Live support
claims require a real Codex run with persisted `provider="codex_responses"`
traffic.

The wrapper uses command-line Codex config overrides for the child process. It
does not mutate `~/.codex/config.toml`.

## Experimental Codex UI Mode

Local Codex Desktop and local IDE extension app-server workflows can be routed
through the same Codex Blackbox Envoy/core pipeline with an explicit UI mode:

```bash
codex-blackbox ui doctor
codex-blackbox ui enable --dry-run
codex-blackbox ui enable
codex-blackbox ui status
codex-blackbox ui disable
```

`ui enable` is the only command that writes Codex user config, and it creates a
backup plus Blackbox-owned rollback state first. `codex-blackbox run codex`
keeps using transient command-line overrides and does not mutate
`~/.codex/config.toml`.

The generated UI config is:

```toml
openai_base_url = "http://127.0.0.1:10000/backend-api/codex"

[features]
enable_request_compression = false
```

UI mode deliberately leaves `chatgpt_base_url` and `model_provider` unchanged.
Those values are part of Codex Desktop's broader control plane and provider
identity. Changing them persistently can hide existing Desktop sessions by
making the UI list threads under a different provider id. Safe UI mode therefore
keeps the normal `openai` provider identity.

Current Codex Desktop builds can attempt Responses over WebSocket. The local
Envoy listener returns HTTP 426 for those upgrades; if Codex does not fall back
to HTTP Responses, there is no model-turn body for Blackbox to observe.
`ui status` reports this as `websocket_only_unobservable` instead of treating it
as a restart problem. Seamless support for that path requires a future
WebSocket relay.

If Envoy sees `POST /backend-api/codex/responses` but core cannot persist
`provider="codex_responses"` evidence, `ui status` reports
`http_responses_unparsed`. That means the app-server did reach the proxy, but
the observed request body shape or compression still needs parser support. Core
decodes `content-encoding: zstd` request bodies before parsing Responses JSON.

Codex's remote compaction endpoint (`/backend-api/codex/responses/compact`) is
passed through without ext_proc body inspection. Compaction payloads can exceed
Envoy's buffered-body limit, and they are not model-turn telemetry.

After enabling, restart local Codex Desktop or the local IDE extension
app-server so it reloads the Codex config. `ui launch` can start or focus Codex
Desktop on supported platforms, but enable does not require launch and Codex
Blackbox does not kill or restart UI processes.

This mode is experimental. It is scoped to local Codex Desktop and local IDE
extension app-server traffic. Hosted web/cloud Codex, API-key Codex routing,
generic system proxying, TLS MITM, WebSocket observation, hook evidence as UI
support proof, JSON stdout, app-server callbacks, and tool-result telemetry are
out of scope.

`ui status` is evidence-driven. It distinguishes not configured, configured
without observed traffic, recent observed traffic, stale observed traffic,
likely misconfigured or not-restarted app-server states, and the current
WebSocket-only and HTTP-unparsed states. The support-proof evidence source is still
Envoy-observed `provider="codex_responses"` traffic persisted by
`codex-blackbox-core`; Envoy WebSocket 426 logs only explain why no turn was
observed. Local fake fixture sessions are ignored for UI support status.

Fake fixtures and dry runs prove local contracts only. Live UI support claims
require a real Desktop/IDE smoke that observes persisted
`provider="codex_responses"` traffic.

## Commands

```bash
codex-blackbox run codex
codex-blackbox ui doctor
codex-blackbox ui enable --dry-run
codex-blackbox ui status
codex-blackbox coach preview
codex-blackbox coach install
codex-blackbox baseline preview
codex-blackbox watch
codex-blackbox status
codex-blackbox guard
codex-blackbox postmortem last
codex-blackbox sessions
```

Architecture and development notes live in [ARCHITECTURE.md](ARCHITECTURE.md).
