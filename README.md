# Codex Blackbox

Codex can finish successfully while leaving you with the harder question: what
actually happened in that session? Codex Blackbox runs Codex CLI through a local
Envoy proxy and records the Responses-shaped model-turn facts that are safe to
observe from the proxy boundary.

The first useful output is a redacted Codex Responses Postmortem. It shows the
session state, requested and served model, terminal Responses status, token
accounting, local cost estimate, evidence, caveats, and the next action worth
taking. Watch mode and Grafana are available when you need live activity or
history across sessions.

Codex Blackbox runs locally. The proxy, database, metrics endpoint, dashboard,
and CLI run on your machine. It does not send telemetry to a hosted Codex
Blackbox service.

## Codex Postmortem

```bash
codex-blackbox up
codex-blackbox run --watch -- codex exec --sandbox read-only "Summarize this repo in 3 bullets. Do not edit files."
codex-blackbox postmortem last
```

Postmortems are redacted by default. Use `codex-blackbox postmortem
<session_id>` for a specific session, `--no-redact` for local unredacted
evidence, and `--output <path>` to write the markdown report to a file.

The structured API contract is available at `GET /api/postmortem/last` and
`GET /api/postmortem/:session_id`. Add `?redact=false` only for local debugging.
Postmortems are deterministic and limited to Envoy-observed Responses evidence.

```markdown
# Codex Responses Postmortem

## Snapshot
- Session: 019e0743-63c2-7c61-b326-8088e4ae0c7b
- State: final or persisted snapshot
- Outcome: Likely Completed
- Requested Model: gpt-5.5
- Served Model: gpt-5.5
- Turns: 3
- Tokens: input 54231, cached 41600, uncached 12631, output 610, reasoning 445, local total 54841
- Local Estimate: $0.10
- Local Estimate Trust: untrusted for budget enforcement

## Caveats
- Evidence is limited to local Envoy-observed Codex Responses traffic.
- Tool-call rows are model-side intent only; local execution outcome is not observed.
- Cached input is token accounting only; lifecycle timing is not inferred.
```

## Quick Start

Install Codex Blackbox:

```bash
curl -fsSL https://raw.githubusercontent.com/softcane/codex-blackbox/main/install.sh | sh
```

Start the local stack and run Codex through the wrapper:

```bash
codex-blackbox doctor
codex-blackbox up
codex-blackbox run --watch -- codex exec --sandbox read-only "Read README.md and summarize the project. Do not edit files."
```

Open Grafana at
[http://127.0.0.1:3000/d/codex-blackbox-main](http://127.0.0.1:3000/d/codex-blackbox-main).
Anonymous viewer mode is enabled by the local stack.

## What Codex Blackbox Catches

- **Responses status:** completed, failed, incomplete, and unknown terminal
  statuses observed from Codex Responses streams.
- **Model route facts:** requested model from the request and served model from
  response headers or payload.
- **Token and context pressure:** input, cached input, uncached input, output,
  reasoning output, local total tokens, estimated context fill, and local cost
  estimate.
- **Accounting anomalies:** malformed or future provider payloads where local
  accounting rules differ from provider-reported totals.
- **Model-side tool-call intent:** tool calls the model attempted to emit, with
  explicit caveats that this is not proof of local tool result success or
  failure.

## Why It Is Safe To Run Locally

- **Local-first:** Envoy, `codex-blackbox-core`, SQLite, Prometheus, Grafana,
  and the CLI run on your machine. Default ports bind to `127.0.0.1`.
- **No user config mutation:** `codex-blackbox run -- codex ...` passes
  command-line Codex config overrides and does not edit `~/.codex/config.toml`.
- **No full transcript claim:** Codex Blackbox stores request/response facts,
  cleaned prompt excerpts, compact response summaries, and token/accounting
  fields. It is not a full local runtime trace.
- **Evidence is labeled:** Fake fixtures validate local contracts only.
  Live/dogfood claims require real Codex CLI traffic observed by
  `codex-blackbox-core` with `provider="codex_responses"`.

## Product Boundary

Codex Blackbox is a Codex Responses observability tool, not a complete Codex
runtime recorder. It does not use Codex hooks, local JSON stdout, or app-server
hook endpoints as telemetry sources.

Do not treat Codex Blackbox output as evidence for:

- local tool result success or failure
- MCP server lifecycle state
- skill lifecycle state
- cache TTL, cache expiry, or cache rebuild lifecycle
- provider quota, cap, or rate-limit window state
- permission approval or denial decisions

## Reference

Common commands:

- `codex-blackbox doctor`
- `codex-blackbox up`
- `codex-blackbox run -- codex exec --sandbox read-only "Prompt"`
- `codex-blackbox watch --url http://127.0.0.1:9091`
- `codex-blackbox sessions --limit 20 --days 7`
- `codex-blackbox postmortem last`
- `codex-blackbox postmortem last --no-redact`
- `curl -s 'http://127.0.0.1:9091/api/sessions?limit=5'`
- `curl -s http://127.0.0.1:9091/metrics`

Validation commands:

- `./test/validate-openai-config.sh`
- `./test/e2e-openai-responses.sh`
- `./test/observability-openai-responses.sh`
- `./test/e2e-openai-responses-full.sh`

The fake Responses tests do not contact OpenAI or launch Codex. They are local
contract checks only. Read [test/README.md](test/README.md) for the distinction
between fake, preflight, dogfood, and live evidence.

Developer notes live in [docs/reference/developing.md](docs/reference/developing.md).
