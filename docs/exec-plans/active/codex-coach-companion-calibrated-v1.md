# Goal

Make Coach/Companion ship honestly as Calibrated v1 by preserving hook coach
support as advisory evidence, keeping Envoy/core HTTP Responses as the durable
source, and removing any WebSocket-frame or proxy tool-success support claim.

# Non-goals

- Do not implement a dedicated Companion baseline UI panel.
- Do not implement local warning snooze or mute controls.
- Do not mutate user Codex config beyond existing explicit UI/coach commands.
- Do not convert hook-only evidence into durable Codex turn telemetry.

# Current State

- Existing uncommitted changes already add Coach/Companion decision reuse and
  hook-only companion session coverage.
- Postmortems are only built for sessions with `provider="codex_responses"`
  request or turn evidence.
- UI status already distinguishes HTTP Responses POST without core persistence
  from WebSocket-only 426 attempts.

# Work Slices

- Tighten UI/Desktop support wording so HTTP Responses via Envoy/core is the
  only live claim and WebSocket-only traffic is labeled unobservable,
  unsupported, and deferred.
- Verify hook-only companion sessions retain advisory hook evidence without
  durable proxy evidence or postmortem links.
- Verify shared decisions keep ended durable proxy sessions ended even when
  local pricing is untrusted.
- Keep proxy tool calls rendered as model-side intent only.

# Verification

- `git status --short --ignored`
- `cargo test`
- `./test/validate-openai-config.sh`
- `./test/e2e-coach-companion.sh`
- `git status --short --ignored`

# Progress Log

- 2026-05-27: Started cleanup after reading repository guidance, architecture,
  Coach/Companion PRD, GrillMe review, and relevant code paths.
- 2026-05-27: Tightened UI support wording, decision parity for ended sessions
  with untrusted-pricing-only signals, hook-only companion evidence handling,
  and regression tests.
- 2026-05-27: `cargo test`, `./test/validate-openai-config.sh`, and
  `./test/e2e-coach-companion.sh` passed. The Coach/Companion E2E was fake
  fixture evidence only and wrote artifacts under `reports/e2e/`.

# Decisions

- WebSocket-only UI traffic remains unsupported/deferred unless a future
  WebSocket relay is implemented.
- Hook `PostToolUse` can inform hook-result-adjacent coaching, but proxy
  `ToolUse` remains intent only.

# Rollback Or Recovery

Revert only this plan file and the scoped code/test hunks from this cleanup.
Preserve pre-existing uncommitted user changes in the worktree.
