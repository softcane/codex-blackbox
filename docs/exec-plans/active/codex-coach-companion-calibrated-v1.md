# Status

Complete. The product scope was tightened after implementation: Coach remains,
Grafana is the dashboard surface, and custom UI/Desktop/WebSocket support is
dropped from the product claim.

# Goal

Make Coach/Companion ship honestly as Calibrated v1 by preserving hook coach
support as advisory evidence, keeping Envoy/core HTTP Responses as the durable
source, using Grafana for dashboards, and removing UI/Desktop/WebSocket-frame
or proxy tool-success support claims.

# Non-goals

- Do not implement a dedicated Companion/browser UI.
- Do not implement a dedicated Companion baseline UI panel.
- Do not implement local warning snooze or mute controls.
- Do not claim Codex Desktop/UI observation.
- Do not mutate user Codex config for Coach/Grafana behavior.
- Do not convert hook-only evidence into durable Codex turn telemetry.

# Current State

- Existing uncommitted changes already add Coach/Companion decision reuse and
  hook-only companion session coverage.
- Postmortems are only built for sessions with `provider="codex_responses"`
  request or turn evidence.
- Public UI commands and the served Companion HTML page have been removed from
  the calibrated Coach/Grafana product.

# Work Slices

- Drop UI/Desktop support from the product wording entirely.
- Verify hook-only companion sessions retain advisory hook evidence without
  durable proxy evidence or postmortem links.
- Verify shared decisions keep ended durable proxy sessions ended even when
  local pricing is untrusted.
- Keep proxy tool calls rendered as model-side intent only.
- Keep Grafana/Prometheus as the dashboard direction.

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
- 2026-05-27: Product scope changed again: no custom Companion/browser UI,
  baseline panel, snooze/mute UI, Desktop/UI observation, or WebSocket
  observation. Grafana is the dashboard surface.

# Decisions

- UI/Desktop/WebSocket observation is dropped from the product scope.
- Hook `PostToolUse` can inform hook-result-adjacent coaching, but proxy
  `ToolUse` remains intent only.

# Rollback Or Recovery

Revert only this plan file and the scoped code/test hunks from this cleanup.
Preserve pre-existing uncommitted user changes in the worktree.
