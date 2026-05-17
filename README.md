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

## Postmortem

The postmortem is the durable record of the run.

```bash
codex-blackbox postmortem last
```

It shows the session outcome, model route, token usage, cached input, context
fill, pricing trust, response status, and a per-turn Flight Recorder.

Cached input is part of input tokens. Reasoning tokens are output detail. Local
total tokens are input plus output.

## What Gets Recorded

Codex Blackbox records model-turn evidence observed from Codex Responses
traffic:

- session and turn identity
- completed, failed, incomplete, or unknown response status
- requested and served model
- input, cached input, uncached input, output, reasoning, and local total tokens
- context fill and accounting anomalies
- model-side tool-call intent
- redacted prompt and response summaries when available

Tool calls are observed intent from the model stream. They are not proof that a
local tool result succeeded.

## Why The Evidence Is Trustworthy

Codex Blackbox treats observed Codex Responses traffic as the source of truth.
It does not turn local logs, hooks, or fake fixtures into live support claims.

Fake Responses fixtures prove local parser and UI contracts. Live support
claims require a real Codex run with persisted `provider="codex_responses"`
traffic.

The wrapper uses command-line Codex config overrides for the child process. It
does not mutate `~/.codex/config.toml`.

## Commands

```bash
codex-blackbox run codex
codex-blackbox watch
codex-blackbox status
codex-blackbox guard
codex-blackbox postmortem last
codex-blackbox sessions
```

Architecture and development notes live in [ARCHITECTURE.md](ARCHITECTURE.md).
