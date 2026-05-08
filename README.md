# Codex Blackbox

Codex Blackbox helps you answer a simple question after a Codex run:

What happened?

It runs Codex CLI through a local Envoy proxy, records the Codex Responses
traffic it can see, and gives you a short postmortem for the session.

It is meant for local debugging. The proxy, database, metrics, Grafana, and CLI
run on your machine.

## Quick Start

Install:

```bash
curl -fsSL https://raw.githubusercontent.com/softcane/codex-blackbox/main/install.sh | sh
```

Start the local stack:

```bash
codex-blackbox doctor
codex-blackbox up
```

Run Codex through the wrapper:

```bash
codex-blackbox run --watch -- codex exec --sandbox read-only "Read README.md and summarize this repo. Do not edit files."
```

Read the latest report:

```bash
codex-blackbox postmortem last
```

Open Grafana:

[http://127.0.0.1:3000/d/codex-blackbox-main](http://127.0.0.1:3000/d/codex-blackbox-main)

## What You Get

The postmortem is redacted by default. It shows:

- the session id
- whether the run completed, failed, or ended incomplete
- the requested model and served model
- input, cached input, uncached input, output, and reasoning tokens
- local token and cost estimates
- important signals, like high context use or model mismatch
- tool calls the model tried to make
- caveats about what Codex Blackbox could not see

Example:

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

## Recommendations
- Continue from the latest response summary if it matches the intended task.
```

For a specific session:

```bash
codex-blackbox postmortem <session_id>
```

For local debugging without redaction:

```bash
codex-blackbox postmortem last --no-redact
```

To write the report to a file:

```bash
codex-blackbox postmortem last --output report.md
```

## What It Can Tell You

Codex Blackbox can report facts that passed through the local proxy:

- Codex Responses requests
- response status: completed, failed, incomplete, or unknown
- requested model and served model
- token counts
- cached input as part of input, not extra input
- reasoning output as part of output-side detail, not extra output
- response ids
- model-side tool-call intent

## What It Cannot Tell You

Codex Blackbox does not see everything Codex does locally.

It cannot prove:

- whether a local tool call succeeded or failed
- whether an MCP server started, stopped, or failed
- whether a skill loaded or failed
- why a permission was approved or denied
- provider quota, cap, or rate-limit state
- cache TTL, cache expiry, or cache rebuild timing

When the report lists tool calls, read that as "the model tried to call this
tool", not "the tool succeeded".

## Common Commands

```bash
codex-blackbox doctor
codex-blackbox up
codex-blackbox watch --url http://127.0.0.1:9091
codex-blackbox sessions --limit 20 --days 7
codex-blackbox postmortem last
codex-blackbox postmortem last --no-redact
```

API shortcuts:

```bash
curl -s 'http://127.0.0.1:9091/api/sessions?limit=5'
curl -s 'http://127.0.0.1:9091/api/postmortem/last'
curl -s http://127.0.0.1:9091/metrics
```

## Testing

Local fake tests:

```bash
./test/validate-openai-config.sh
./test/e2e-openai-responses.sh
./test/observability-openai-responses.sh
./test/e2e-openai-responses-full.sh
```

These tests use fake Responses fixtures. They do not contact OpenAI, and they
do not prove live Codex support.

Live or dogfood evidence means a real Codex CLI run went through
`codex-blackbox run -- codex ...` and `codex-blackbox-core` recorded at least
one new request with `provider="codex_responses"`.

## Development

Developer notes:

[docs/reference/developing.md](docs/reference/developing.md)
