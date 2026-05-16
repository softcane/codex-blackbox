# Codex Blackbox

Codex runs can be hard to judge after the fact.

It may finish, but you still do not know what happened: which model answered,
whether the response actually completed, how many tokens were used, whether
cached input helped, or whether the run is worth continuing.

Codex Blackbox gives you a local postmortem for a Codex CLI session. It turns
the run into a short report with the outcome, model, token use, cost estimate,
important signals, and a practical next step.

It is built for local debugging. The database, metrics, dashboard, and CLI run
on your machine.

The demo image below is fixture-backed example output, not live support
evidence.

![demo](docs/demo.gif)

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

Run Codex normally through the wrapper:

```bash
codex-blackbox run --watch -- codex
```

Read the latest report:

```bash
codex-blackbox postmortem last
```

Render the latest advisory decision as a one-line footer or JSON:

```bash
codex-blackbox status
codex-blackbox status --json
codex-blackbox guard --json
```

Or opt in to automatic postmortem rendering when watch sees a completed idle
session:

```bash
codex-blackbox watch --postmortem
```

For a quick one-shot check instead of an interactive Codex session:

```bash
codex-blackbox run --watch -- codex exec --sandbox read-only "Read README.md and summarize this repo. Do not edit files."
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
- tool-call intent the model emitted, not proof that a tool succeeded

Example terminal display (fixture-style redacted sample, not live evidence):

```text
┌─[ Codex Responses Postmortem ]────────────────────────────────────────────┐
│ Session   019e0743-63c2-7c61-b326-8088e4ae0c7b (redacted)                │
│ Outcome   Likely Completed; 3 turns                                      │
│ Model     gpt-5.5                                                        │
│ Impact    54841 local tokens; local $0.10                                │
└──────────────────────────────────────────────────────────────────────────┘

┌─[ Recommendations ]───────────────────────────────────────────────────────┐
│ 1. Continue from the latest response summary.                             │
└──────────────────────────────────────────────────────────────────────────┘
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

The terminal view is styled for scanning; `--output` writes plain Markdown.

## What It Can Tell You

Codex Blackbox can report what it observed during the model run:

- did the model response complete, fail, or stop incomplete?
- which model was requested, and which model answered?
- how many input, cached input, uncached input, output, and reasoning tokens
  were used?
- what was the local cost estimate?
- did the run show context pressure, model mismatch, or accounting oddities?
- which tools did the model try to call?

## Common Commands

```bash
codex-blackbox doctor
codex-blackbox up
codex-blackbox watch --url http://127.0.0.1:9091
codex-blackbox watch --postmortem
codex-blackbox status --json
codex-blackbox guard --json
codex-blackbox sessions --limit 20 --days 7
codex-blackbox postmortem last
codex-blackbox postmortem last --no-redact
```

Guard checks are local and advisory by default. A configured token budget or
trusted cost budget can block only the next request before it is sent; it cannot
interrupt an already-streaming model response. If a guard policy file cannot be
loaded, Codex Blackbox fails open and reports the policy issue.

API shortcuts:

```bash
curl -s 'http://127.0.0.1:9091/api/sessions?limit=5'
curl -s 'http://127.0.0.1:9091/api/postmortem/last'
curl -s 'http://127.0.0.1:9091/api/guard-state'
curl -s http://127.0.0.1:9091/metrics
```

## Testing

Evidence categories:

- Fake fixtures validate local parser, persistence, watch, status, guard, and
  postmortem contracts. They do not contact OpenAI and do not prove live Codex
  support.
- Preflight checks validate local configuration and login state without
  launching a Codex model turn.
- Dogfood evidence means a real local Codex run was intentionally routed
  through `codex-blackbox run -- codex ...` and observed by
  `codex-blackbox-core`.
- Live support claims require real observed Codex Responses traffic persisted
  with `provider="codex_responses"`.

Local fake and static checks:

```bash
./test/validate-openai-config.sh
./test/e2e-openai-responses-full.sh
```

## Development

Developer notes:

[docs/reference/developing.md](docs/reference/developing.md)
