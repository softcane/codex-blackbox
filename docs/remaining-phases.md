# Remaining Phases

Current checkpoint: Phase 9B has a real Codex smoke result documented in
`docs/real-codex-smoke.md`. Phase 9C now has a real automated dogfood harness
and its first four-session run is `partial` because real tool and MCP watch
telemetry are still missing.

Coditor already has unit, contract, and fake Envoy e2e tests. The remaining question is when to start each kind of testing.

## Where We Are

Completed:

- Phase 0A: skeleton copy and repo bootstrap.
- Phase 0B: mechanical Coditor rename.
- Phase 1: Codex/OpenAI traffic contract and fixtures.
- Phase 2A: Codex request parser behind tests.
- Phase 2B: Codex request parser wired into request metadata path.
- Phase 3A: Codex Responses SSE accumulator behind tests.
- Phase 3B: Codex response accumulator wired into response path.
- Phase 4A: Codex accounting helpers behind tests.
- Phase 4B: minimal Codex finalization/watch/metrics path.
- Phase 5A: fake OpenAI Responses upstream through Envoy.
- Phase 5B: Responses Envoy config static validation.
- Phase 4C: Codex SQLite persistence and schema mapping.
- Phase 6A: CLI doctor and config groundwork.
- Phase 9A: broader fake OpenAI Responses e2e regression.
- Phase 9B: first real ChatGPT-auth Codex smoke test for Codex 0.125.0.

Implemented with remaining gaps:

- Phase 9C: `test/dogfood-codex-sessions.sh` can run 1-4 real Codex sessions
  and report passed, failed, skipped, and missing capabilities. The first full
  run passed session/watch/SQLite/Prometheus/Grafana coverage and reported
  missing real tool/MCP watch events.

## Remaining Work

The remaining work spans Phase 6C, real tool/MCP telemetry and diagnosis polish
from Phase 7 and Phase 8, the Phase 9C missing capabilities, and Phase 10.

For execution, split them into these smaller gates:

### Phase 6B: Safe `coditor run -- codex ...` Wiring

Completed as an experimental/manual wrapper path for Codex:

- points Codex at the local Coditor proxy with command-line `-c` overrides
- disables Codex request compression for MVP
- preserves user-provided Codex arguments
- uses the ChatGPT/Codex subscription backend override for live smoke
  planning
- avoids editing `~/.codex/config.toml`

It is still not proof that real Codex/OpenAI traffic works until the
subscription path is live-validated.

### Phase 6C: Watch And Tmux Codex Polish

Finish user-facing watch behavior:

- remove stale Claude/Anthropic labels from active Codex UI
- render Codex cached input, reasoning output, model change, and status
  language correctly
- keep no-TTL cached input behavior clear by using `CodexTurnSummary` instead
  of Anthropic-shaped `CacheEvent`
- ensure `watch --tmux` still self-bootstraps and renders Codex sessions

Done when inline watch and tmux views make sense for Codex fake sessions.

### Phase 7: Codex Hooks Integration

Add `/api/hooks/codex` and hook-helper/config output in a fake-first way:

- parse fake Codex hook payloads under the `coditor.codex_hook.v1` fixture
  contract
- treat prompt/session hooks as provisional in-memory watch sessions while the
  proxy remains authoritative for durable turns
- correlate hook ids with proxy session ids when `proxy_session_id` is present
- emit tool and MCP watch events without duplicating proxy tool starts

Done when fake hook payloads produce expected watch events and hook failures cannot affect model traffic.

### Phase 8: Diagnostics, Rate Limits, And Context Intelligence

Make diagnosis useful for Codex:

- failed/incomplete responses
- repeated tool failures
- model mismatch
- context pressure
- high reasoning token use
- low cached-input reuse
- OpenAI rate-limit headers if verified

Done when diagnosis endpoints and watch events use Codex-native signals and metrics still avoid session-id labels.

### Phase 9A: Broader Fake E2E Coverage

Completed as `./test/e2e-openai-responses-full.sh`. The latest prerequisite run
before the first live smoke passed with run id
`openai-full-e2e-1777587872-34990`.

Expand fake tests beyond the current text-only Envoy path:

- parallel session isolation
- watch replay race
- failure-open behavior after core shutdown
- failed and incomplete Responses streams
- CLI dry-run proves the Codex wrapper uses subscription proxy overrides
- no double-counting of cached input in e2e assertions

Done when fake e2e validates the proxy, parser, finalization, watch, CLI dry-run shape, and failure posture without OpenAI credentials.

### Phase 9B: First Real Codex Smoke Test

Completed for the local ChatGPT-auth Codex subscription path on
2026-04-30 UTC / 2026-05-01 Europe/Stockholm with Codex CLI 0.125.0. See
`docs/real-codex-smoke.md` for the exact config, command, observed events,
rollback command, and limitations.

Run a controlled real Codex session only after Phase 6B and enough fake e2e coverage are passing.

Minimum preconditions:

- CLI can point Codex at Coditor intentionally.
- Request compression is disabled or decoded.
- ChatGPT/Codex subscription proxy mode is documented.
- Fake e2e still passes.
- The smoke test has clear rollback instructions.

Done when the real smoke test is documented with date, Codex version, config, command, observed events, and limitations.

### Phase 9C: Automated Multi-Session Dogfood Feedback

Implemented as a real automated feedback harness, with the first full run
recorded as `partial` because real tool and MCP watch events were not emitted.

The harness described in
`docs/automated-feedback-testing.md`:

- launch 3-4 Codex sessions
- cover same-repo and different-repo sessions
- run read-only prompts, tool-call prompts, and MCP prompts when MCP is configured
- capture `/watch`
- query SQLite
- query Prometheus
- verify Grafana provisioning/dashboard availability
- write a report that names passed, failed, skipped, and missing capabilities

Done for the harness shape when it can say exactly what is left rather than
only pass/fail. The current report does that; the remaining work is to close
the missing real tool/MCP telemetry it names.

### Phase 10: Documentation And Release Readiness

Make the repo usable by someone who did not watch the migration happen:

- README
- architecture docs
- troubleshooting docs
- ADRs
- known limitations
- supported runtime/auth modes
- repeatable fake and real smoke-test commands

Done when docs match the code and do not overclaim support.

## Testing Gates

Testing has already started.

Current test level:

- Unit and contract tests pass.
- Fake OpenAI Responses Envoy e2e passes.
- Default ChatGPT/Codex Envoy config validates statically.

Next testing milestones:

1. Completed after Phase 6A: CLI doctor/config output testing.
2. Completed after Phase 6B: wrapper-level Codex command construction testing.
3. Completed after Phase 4C: SQLite assertions for Codex sessions and turns.
4. After Phase 6C: watch and tmux UX testing with fake Codex sessions.
5. After Phase 7: fake Codex hook/tool/MCP testing.
6. After Phase 8: diagnosis, Prometheus, and Grafana checks become meaningful.
7. After Phase 9A: full fake e2e regression testing.
8. Completed after Phase 9B: first real Codex smoke test.
9. Implemented after Phase 9C: automated 3-4 session dogfood feedback that
   currently reports missing real tool/MCP watch telemetry.

Do not treat Coditor as ready for daily dogfood until the Phase 9C report is
clean enough for the intended use, especially real tool and MCP telemetry.

## Short Answer

Left before the first small real Codex smoke test: none for the current local
Codex 0.125.0 subscription path. It ran on 2026-04-30 UTC and is documented in
`docs/real-codex-smoke.md`.

Left before the automated 3-4 session dogfood harness the user described:

- the harness exists and ran four real sessions
- the current report is `partial`
- real tool and MCP watch telemetry are missing

Phase 9C is now the automated feedback boundary across same/different repos,
read queries, tool-oriented prompts, MCP when configured, SQLite, Prometheus,
and Grafana.

Left before release-quality Coditor: watch/tmux polish, real tool/MCP telemetry,
diagnosis/rate-limit hardening, clean dogfood reports, and Phase 10
documentation/release readiness.
