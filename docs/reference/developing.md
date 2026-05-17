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

Release-facing claims require a real Codex model turn observed by
`codex-blackbox-core` with `provider="codex_responses"`. Fake fixture tests
validate local parser, persistence, API, watch, and dashboard contracts only.
Keep the fake regression in CI, but do not describe it as live support proof.

Before changing behavior, read the repository `AGENTS.md` and the module map in
that file. The highest-risk paths are request parsing, Responses SSE
accumulation, token accounting, persistence, watch replay, metrics labels, and
the CLI wrapper.
