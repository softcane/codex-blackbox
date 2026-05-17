# Codex Blackbox

Codex Blackbox records what happened during a Codex CLI run.

It runs a local Envoy proxy, `codex-blackbox-core`, SQLite, Prometheus, and
Grafana. When Codex sends a model request through the proxy, the core service
stores the request and response evidence.

Use it when a run finishes and you want clear answers:

- did the model response complete, fail, or end incomplete?
- which model answered?
- how many tokens were used?
- how much input came from cache?
- is the context window getting tight?
- what is the next practical step?

The wrapper uses Codex's experimental subscription proxy path. Live support
claims should be tied to real Codex traffic observed with
`provider="codex_responses"`.

The animation below is a fixture-backed sample.

![demo](docs/demo.gif)

## Quick Start

Install:

```bash
curl -fsSL https://raw.githubusercontent.com/softcane/codex-blackbox/main/install.sh | sh
```

Check the local environment and start the stack:

```bash
codex-blackbox doctor
codex-blackbox up
```

Run Codex through the wrapper:

```bash
codex-blackbox run --watch -- codex
```

Read the latest postmortem:

```bash
codex-blackbox postmortem last
```

Open Grafana:

[http://127.0.0.1:3000/d/codex-blackbox-main](http://127.0.0.1:3000/d/codex-blackbox-main)

For a quick read-only check:

```bash
codex-blackbox run --watch -- codex exec --sandbox read-only "Read README.md and summarize this repo. Do not edit files."
```

## What Gets Recorded

Codex Blackbox records model-turn evidence observed through the local proxy:

- session id and turn number
- response status: completed, failed, incomplete, or unknown
- requested model and served model
- input, cached input, uncached input, output, and reasoning tokens
- local cost estimate when pricing is known
- context usage and accounting anomalies
- model-side tool requests
- a redacted prompt excerpt and response summary when available

Cached input is counted as part of input tokens. Reasoning tokens are output
detail. Unknown model pricing stays unpriced.

Tool requests are model intent observed in the response stream. For tool
execution results, inspect the tool output or runtime logs.

## Postmortems

Postmortems are redacted by default and are built from persisted evidence.

Example terminal output:

```text
┌─[ Codex Session Report ]─────────────────────────────────────────────────┐
│ Session   019e0743-63c2-7c61-b326-8088e4ae0c7b (redacted)                │
│ Result    Likely Completed; 3 turns                                      │
│ Model     gpt-5.5                                                        │
│ Usage     54841 local tokens; estimated $0.10                            │
└──────────────────────────────────────────────────────────────────────────┘

┌─[ Next Steps ]────────────────────────────────────────────────────────────┐
│ 1. Continue from the latest response summary.                             │
└──────────────────────────────────────────────────────────────────────────┘
```

Read a specific session:

```bash
codex-blackbox postmortem <session_id>
```

Show local debugging details:

```bash
codex-blackbox postmortem last --no-redact
```

Write Markdown to a file:

```bash
codex-blackbox postmortem last --output report.md
```

Control terminal color:

```bash
codex-blackbox postmortem last --color always
codex-blackbox postmortem last --color never
```

## Watch, Status, And Guard

`watch` streams local events while a Codex run is active:

```bash
codex-blackbox watch --url http://127.0.0.1:9091
codex-blackbox watch --postmortem
```

`status` renders the current decision footer:

```bash
codex-blackbox status
codex-blackbox status --json
```

`guard` renders the advisory decision for the next request:

```bash
codex-blackbox guard --json
```

Guard policies are checked before the next request is sent. A response already
in progress continues streaming.

If a guard policy file fails to load, Codex Blackbox reports the policy problem
and stays fail-open.

## Common Commands

```bash
codex-blackbox doctor
codex-blackbox up
codex-blackbox run --watch -- codex
codex-blackbox sessions --limit 20 --days 7
codex-blackbox recall "pricing"
codex-blackbox postmortem last
codex-blackbox status --json
codex-blackbox guard --json
codex-blackbox config codex
```

API shortcuts:

```bash
curl -s 'http://127.0.0.1:9091/api/sessions?limit=5'
curl -s 'http://127.0.0.1:9091/api/postmortem/last'
curl -s 'http://127.0.0.1:9091/api/guard-state'
curl -s http://127.0.0.1:9091/metrics
```

## Local Data

The stack stores data locally:

- SQLite keeps sessions, requests, turn summaries, diagnoses, and reports.
- Prometheus stores bounded metrics for requests, tokens, status, duration, and
  context usage.
- Grafana reads Prometheus and shows the local dashboard.

Prometheus labels are kept bounded. Session ids, prompts, request ids, response
ids, cwd values, and raw tool inputs are kept out of metric labels.

## Evidence And Testing

The project keeps evidence categories separate:

- Fixture tests cover local parser, persistence, API, watch, status, guard,
  metrics, dashboard, and postmortem contracts.
- Preflight checks validate local configuration and login state.
- Dogfood evidence comes from a real local Codex run through
  `codex-blackbox run -- codex ...`.
- Live support evidence requires a real `provider="codex_responses"` request
  observed and persisted by `codex-blackbox-core`.

Run the main local checks:

```bash
cargo fmt --check
cargo test --workspace
./test/validate-openai-config.sh
./test/e2e-openai-responses-full.sh
```

Run a real dogfood check only when you intend to spend a live Codex turn:

```bash
./test/dogfood-codex-sessions.sh
```

## Development

Developer notes live in [docs/reference/developing.md](docs/reference/developing.md).

The system map lives in [ARCHITECTURE.md](ARCHITECTURE.md).
