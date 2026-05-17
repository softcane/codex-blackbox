# Developing On Codex Blackbox

Codex Blackbox has fake, preflight, dogfood, and live evidence categories.
Keep those separate in code, docs, tests, and release notes:

- Fake fixture evidence validates local contracts only. It can cover parser,
  persistence, metrics shape, watch, status, guard, and postmortem behavior, but
  it never proves live Codex support.
- Preflight evidence validates local configuration and login state. It must stop
  before launching a model turn unless the user explicitly approves a live run.
- Dogfood evidence is an intentionally real local Codex CLI run through
  `codex-blackbox run -- codex ...`.
- Live support evidence requires `codex-blackbox-core` to observe and persist at
  least one real `provider="codex_responses"` request for the run being claimed.

Useful local commands:

```bash
./test/harness-fast.sh
cargo fmt --check
cargo test --workspace
cargo clippy --workspace -- -D warnings
./test/validate-openai-config.sh
./test/e2e-openai-responses-full.sh
```

Start the local stack:

```bash
codex-blackbox up
docker compose logs -f codex-blackbox-core
```

Run a real dogfood check only when explicitly intended:

```bash
codex-blackbox run -- codex exec --sandbox read-only "Read README.md and summarize Codex Blackbox in 3 bullets. Do not edit files."
codex-blackbox status --json
codex-blackbox guard --json
codex-blackbox postmortem last
```

Postmortem reports include a top-level `flight_recorder` array. It is built
only from persisted `provider="codex_responses"` turns and contains compact
per-turn status, requested/served model, token, context, and duration fields.
It does not include prompt excerpts, raw tool inputs, cwd paths, or secrets.

Guard policy supports these local next-request rules:

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

Runtime guard enforcement only applies before sending the next request. It
cannot interrupt an already-streaming response. Dollar budgets are enforceable
only when pricing is trusted; unknown or untrusted pricing stays advisory while
trusted non-dollar rules can still block.

Release-facing claims require a real Codex model turn observed by
`codex-blackbox-core` with `provider="codex_responses"`. Fake fixture tests
validate local parser, persistence, API, watch, and dashboard contracts only.
Keep the fake regression in CI, but do not describe it as live support proof.

Before changing behavior, read the repository `AGENTS.md` and the module map in
that file. The highest-risk paths are request parsing, Responses SSE
accumulation, token accounting, persistence, watch replay, metrics labels, and
the CLI wrapper.
