# PRD: Codex Blackbox Coach And Companion

Status: Implemented with calibrated v1 scope
Date: 2026-05-27

Implementation note: the v1 implementation uses
`codex-blackbox-core/src/coach.rs` for normalized events, derived session state,
signals, and shared decisions. The local companion is served at `/companion`
with redacted JSON under `/api/companion/...`. Hook coach install/status/remove
commands live under `codex-blackbox coach ...`; optional derived-only baseline
commands live under `codex-blackbox baseline ...`.

Calibration note: v1 support is proven for local contracts, fake Coach/Companion
E2E, live Codex CLI smoke, and local Codex Desktop/IDE app-server traffic that
falls back to HTTP Responses through Envoy/core. It does not claim WebSocket
frame observation, generic UI support, provider quota state, proxy-observed tool
success, or real billing. Baseline learning is CLI/API-backed derived state; a
dedicated companion baseline panel and local warning snooze/mute controls remain
deferred UI work.

Scope note: this PRD describes the full Coach/Companion product direction, but
the implemented release is Calibrated v1. Requirements marked deferred are not
part of the Calibrated v1 support claim and must not be used to claim "Full"
implementation.

## Problem Statement

Codex can run for many turns while the user is away or focused on other work.
Today Codex Blackbox can observe Codex Responses traffic, show status, enforce
some guard rules, and produce postmortems. That is useful, but it still leaves a
major product gap:

Users often learn too late that Codex was looping, retrying the same failure,
using too much context, ending incomplete, skipping validation, or doing risky
local work.

The current live surfaces are also not ideal for the desired audience:

- Native Codex CLI statusline does not support a custom external status item.
- Tmux is powerful but too power-user oriented to be the default product.
- Grafana is useful for aggregate metrics but not for understanding one active
  Codex session.
- Postmortems are useful after the run, but they do not prevent wasted turns
  during the run.
- Dollar cost is not a reliable primary signal for ChatGPT subscription proxy
  mode, where a displayed dollar amount may be only an API-equivalent estimate.

The user needs Codex Blackbox to become a simple, live, local coach: it should
notice bad patterns early, explain them in plain English, and show the next
useful action while keeping strict evidence boundaries.

## Solution

Build a Codex Blackbox Coach and Companion.

The first product wave has two main features:

1. A Codex hook coach/guard that can warn, block, add model-visible context, or
   continue a turn at high-signal moments.
2. A local companion UI that shows live session state, timeline, tool/validation
   evidence, context pressure, retry signals, and postmortem links without
   requiring tmux.

The core product flow is:

1. Codex Blackbox observes Codex model turns through the existing proxy path.
2. Explicitly installed coach hooks add supported-tool and lifecycle evidence.
3. All evidence is normalized into a shared session-state model.
4. A signal engine detects actionable patterns.
5. A decision engine turns signals into simple states: Healthy, Careful, Stop,
   Blocked, Cooldown, or Ended.
6. The companion UI, hook responses, CLI status/guard/watch, postmortem, and
   metrics all read from the same decision model.

The product does not claim unsupported capabilities:

- Proxy tool-call events remain model-side intent, not tool result success.
- Hook evidence is useful but incomplete and must be labeled separately.
- JSONL transcript parsing is offline-only unless the product boundary changes.
- Native Codex CLI footer integration remains deferred until Codex supports a
  custom statusline provider.
- Dollar cost is advisory unless pricing is trusted by explicit user contract or
  trusted pricing file.

## User Stories

1. As a Codex user, I want to see whether my active Codex session is Healthy,
   Careful, Stop, Blocked, Cooldown, or Ended, so that I know whether I need to
   intervene.
2. As a Codex user, I want a local companion UI instead of tmux as the default,
   so that I can inspect the session without learning a power-user workflow.
3. As a Codex user, I want Codex Blackbox to warn me when Codex repeats the same
   validation failure, so that I can stop blind retry loops.
4. As a Codex user, I want Codex Blackbox to warn when Codex edits files but
   does not validate, so that I do not trust untested changes.
5. As a Codex user, I want Codex Blackbox to detect incomplete or failed model
   responses, so that partial output is not treated as normal progress.
6. As a Codex user, I want Codex Blackbox to detect high context pressure, so
   that I know when the session is becoming less reliable.
7. As a Codex user, I want Codex Blackbox to detect repeated shell failures, so
   that Codex does not keep trying equivalent commands without inspection.
8. As a Codex user, I want Codex Blackbox to show the next action in plain
   English, so that I do not need to interpret raw metrics.
9. As a Codex user, I want warnings to appear inside Codex only when they are
   high-signal, so that the coach does not become noisy.
10. As a Codex user, I want the companion UI to show a timeline of important
    events, so that I can understand what happened during a long run.
11. As a Codex user, I want model turns to show token and context usage, so that
    I can see when the session got expensive in context terms.
12. As a Codex user, I want tool activity to be shown by category and evidence
    source, so that I know whether the fact came from proxy intent or hook
    result evidence.
13. As a Codex user, I want validation attempts to be grouped and summarized,
    so that I can tell whether Codex is making progress.
14. As a Codex user, I want the UI to link to the postmortem when the session
    ends, so that I can inspect the durable report.
15. As a Codex user, I want cost shown only when clearly labeled, so that I do
    not confuse an API-equivalent estimate with my real subscription bill.
16. As a Codex user, I want token and context budget warnings before dollar
    budgets, so that the product focuses on signals that are reliable for Codex.
17. As a Codex user, I want rate-limit pressure to be visible when available, so
    that I can choose whether to continue or wait.
18. As a Codex user, I want local learning from old failures to be optional, so
    that I control whether the tool looks at prior local history.
19. As a Codex user, I want baseline learning to store derived facts only, so
    that raw prompts, outputs, commands, and file paths are not retained.
20. As a Codex user, I want to reset learned baseline data, so that I can clear
    old project behavior.
21. As a Codex user, I want a preview before any hook or config installation, so
    that Codex Blackbox does not surprise-modify my Codex setup.
22. As a Codex user, I want an uninstall path for installed coach hooks, so that
    I can cleanly remove the integration.
23. As a Codex user, I want the wrapper run path to avoid hidden user-config
    mutation, so that normal Codex behavior stays predictable.
24. As a Codex user, I want Codex Blackbox to fail open when coach infrastructure
    is unavailable, so that observability does not break my work.
25. As a Codex user, I want hard blocks only for high-confidence cases or
    explicit policy, so that the coach does not prevent legitimate work.
26. As a Codex user, I want lower-confidence cases to be warnings, so that I can
    decide whether to continue.
27. As a Codex user, I want a short reason code for each warning or block, so
    that I can understand and tune behavior.
28. As a Codex user, I want to snooze a repeated warning category, so that I can
    reduce noise during an intentional workflow. Deferred from Calibrated v1.
29. As a Codex user, I want privacy-safe defaults, so that the product can be
    used in real repositories.
30. As a Codex user, I want fake fixture output to stay clearly labeled, so that
    test evidence is not confused with live Codex support proof.
31. As a maintainer, I want one normalized event model, so that proxy, hook,
    offline transcript, and future app-server evidence do not create separate
    feature-specific parsers.
32. As a maintainer, I want every normalized event to include an evidence
    source, so that UI and policy can explain trust level.
33. As a maintainer, I want proxy-observed model turns to remain authoritative
    for durable Codex turn accounting, so that existing reports remain correct.
34. As a maintainer, I want hook evidence to be useful but clearly advisory, so
    that incomplete hook coverage does not become a false enforcement claim.
35. As a maintainer, I want the signal engine to be testable without running
    Codex, so that behavior can be proven from fixtures.
36. As a maintainer, I want the decision engine to be shared across UI, hooks,
    CLI, guard, watch, and postmortem, so that surfaces do not disagree.
37. As a maintainer, I want metrics labels to stay bounded, so that Prometheus
    is safe and low-cardinality.
38. As a maintainer, I want no raw prompts, paths, commands, request IDs,
    response IDs, or session IDs in metric labels, so that telemetry cannot leak
    sensitive or high-cardinality values.
39. As a maintainer, I want cost metrics excluded from v1 Prometheus output, so
    that untrusted pricing does not become an operational truth.
40. As a maintainer, I want baseline learning to be derived-only, so that the
    feature can be tested and explained safely.
41. As a maintainer, I want companion UI data to come from the same local API as
    CLI status and postmortem, so that UI work does not fork the data model.
42. As a maintainer, I want explicit live smoke requirements before live Codex
    support claims, so that fake tests do not overstate capability.
43. As a maintainer, I want unsupported surfaces absent from public output, so
    that Codex Blackbox does not imply it observes provider quota, cache
    lifecycle, MCP lifecycle, skill lifecycle, or tool success where it does not.
44. As a maintainer, I want app-server integration treated as a future client
    mode, so that the first wave stays small and reliable.
45. As a maintainer, I want any future app-server client to reuse the same
    normalized event and decision contracts, so that it is a UI transport
    change, not a product rewrite.
46. As an operator, I want Grafana dashboards for aggregate trends, so that I
    can see broad health without inspecting each session.
47. As an operator, I want coach actions counted by bounded reason code, so that
    I can tune noisy warnings.
48. As an operator, I want validation failure trends by category, so that I can
    see whether Codex is commonly stuck on tests, type checks, lint, or unknown
    validation.
49. As an operator, I want context pressure distribution, so that I can see
    whether sessions regularly reach risky context levels.
50. As an operator, I want no per-session identifiers in metrics, so that
    dashboards remain aggregate and safe.

## Implementation Decisions

- The first wave prioritizes hook coach/guard and companion UI.
- The companion UI is the default rich surface. Tmux remains optional and
  advanced.
- Native Codex CLI statusline integration is not part of this PRD because Codex
  currently documents only built-in statusline items.
- The existing proxy path remains authoritative for durable model-turn evidence.
- Hook events are added as a separate evidence source for coach behavior.
- Hook evidence must never silently replace proxy evidence.
- Proxy-observed tool use continues to mean model-side tool-call intent only.
- Supported hook `PostToolUse` output can be used for hook-result evidence, but
  only with a visible hook evidence label.
- JSONL transcript reading is permitted only for offline audit and optional
  baseline learning unless the product boundary changes.
- App-server integration is deferred to a later custom-client mode, except for
  locally observed HTTP Responses fallback traffic through Envoy/core.
- The normalized event model is the central contract.
- Normalized events must include event type, timestamp, session/turn reference
  where appropriate, evidence source, confidence level, bounded category, and
  privacy classification.
- Session state is derived from normalized events and should be repairable.
- Signal rules operate on session state, not raw source-specific payloads.
- Decision output should stay simple: state, primary reason, next action,
  evidence source summary, optional policy block facts, and optional
  postmortem link.
- Hook coach responses should be short and specific.
- Hooks warn by default and block only when policy or high-confidence safety
  conditions justify it.
- A `Stop` hook can ask Codex to continue when the session ended without needed
  validation or without inspecting a repeated failure.
- A `PreToolUse` hook can block supported risky tool calls, but the product must
  explain that interception is incomplete.
- A `PostToolUse` hook can react to supported tool output but cannot undo side
  effects.
- Installation of hooks must be explicit. It needs preview, apply, status, and
  uninstall flows.
- The ordinary wrapper run path must not secretly mutate user-level Codex
  config.
- Project-local hook installation should be preferred for project-specific
  coaching. User-level installation can exist only as an explicit, reversible
  opt-in.
- Baseline learning is optional and derived-only.
- Baseline learning should not store raw prompts, raw outputs, raw commands,
  raw paths, or secrets.
- Baseline learning should produce bounded categories such as validation command
  category, repeated failure count, typical context range, and common recovery
  pattern.
- Baseline learning should tune warning thresholds and reduce noise, not create
  hard blocks by itself.
- Cost is not a core v1 signal.
- Token usage, cached input, output tokens, reasoning output tokens, context
  fill, retry count, validation result, and rate-limit pressure are core v1
  signals.
- Dollar estimates can appear in postmortem or UI only with a trust label.
- For ChatGPT subscription proxy mode, dollar values must be labeled
  API-equivalent estimate, not actual billed cost.
- LiteLLM/LIT pricing can be an optional fallback estimate source, but not a
  trusted budget source by default.
- Dollar budget enforcement requires trusted pricing, such as an explicit
  user-provided pricing file or contract-backed source.
- Prometheus metrics should be aggregate only.
- Companion UI should handle per-session detail. Grafana should handle
  aggregate trends.
- Every metric label must come from a fixed bounded set.
- Reason codes should be bounded and documented.
- Privacy tests are required for metrics, persistence, UI payloads, and
  baseline learning.
- Fake fixtures validate local contracts only.
- Live Codex support claims require real smoke or dogfood evidence.

## Feature Requirements

### 1. Normalized Event Model

The product must define one normalized event model used by proxy events, hook
events, offline transcript audit, and future app-server events.

Required event fields:

- event type
- timestamp
- evidence source
- confidence level
- session reference when available
- turn reference when available
- bounded category
- optional bounded reason code
- privacy classification
- payload summary with redacted/derived facts only

Required first event categories:

- model turn started
- model turn completed
- model turn failed
- model turn incomplete
- model turn unknown
- tool intent observed
- supported tool started
- supported tool completed
- supported tool failed
- validation started
- validation succeeded
- validation failed
- file edit observed
- prompt submitted
- stop observed
- compaction observed
- context pressure observed
- rate-limit pressure observed
- coach warning emitted
- coach block emitted

### 2. Session State

The product must derive session state from normalized events.

Required state fields:

- latest known session state
- turn count
- response status counts
- token totals
- cached input totals
- output totals
- reasoning output totals
- max context fill
- recent validation results
- repeated failure counters
- recent edit without validation flag
- recent risky command flag
- recent blind retry flag
- rate-limit pressure when available
- postmortem availability
- evidence source summary

### 3. Signal Engine

The product must detect these v1 signals:

- incomplete model response
- failed model response
- unknown model response
- high context fill
- repeated validation failure
- validation skipped after edits
- repeated shell/tool failure for same bounded category
- blind retry after failure
- risky supported tool call
- pricing untrusted while dollar budget configured
- rate-limit pressure when available
- missing durable Codex Responses evidence

Each signal must have:

- signal name
- severity
- bounded reason code
- evidence source
- short user-facing reason
- suggested next action

### 4. Decision Engine

The decision engine must output:

- state: Healthy, Watching, Careful, Stop, Blocked, Cooldown, or Ended
- primary reason
- next action
- list of active signals
- evidence source summary
- postmortem command or link when available
- optional block facts
- optional warning facts

Decision priority:

1. Explicit block or cooldown.
2. Stop-level safety or correctness issue.
3. Careful-level warning.
4. Watching when evidence is incomplete or session is in progress.
5. Healthy only when enough evidence exists and no active issue exists.
6. Ended when the session completed and postmortem is ready or no more live
   activity is expected.

### 5. Hook Coach

The hook coach must support explicit installation and uninstall.

Required hook behaviors:

- On supported pre-tool events, detect high-confidence risky commands or edits.
- On supported post-tool events, summarize failed validation and repeated
  failure patterns.
- On prompt submit, optionally add derived context when a user asks for a risky
  or unclear action.
- On stop, detect missing validation after edits and repeated unresolved
  failures.
- On compaction events, record that context was compacted without exposing the
  opaque compaction content.

Hook output rules:

- Short warning messages only.
- Block only with explicit policy or high-confidence safety cases.
- Add model-visible context only when it helps the next model step.
- Never include raw secrets.
- Avoid raw command echo unless explicitly allowed by privacy settings.
- Include a bounded reason code in every coach action recorded internally.

### 6. Companion UI

The companion UI must be local and simple.

Required Calibrated v1 views:

- Active sessions list.
- Session overview with state, reason, next action, and evidence sources.
- Timeline of model turns, validation, supported tool events, coach actions,
  context pressure, and postmortem readiness.
- Signal list grouped by severity.
- Token and context panel.
- Validation panel.
- Coach action history.
- Postmortem link or report preview.

Deferred Full-product UI views:

- Dedicated baseline status panel when baseline learning is enabled.

Required Calibrated v1 UI behavior:

- Show live updates without requiring page reload.
- Make evidence source visible.
- Prefer plain English over raw telemetry.
- Avoid exposing raw prompts, outputs, paths, commands, or tool arguments by
  default.
- Show when a signal is advisory because evidence is incomplete.

Deferred Full-product UI behavior:

- Allow users to snooze or mute warning categories locally.

### 7. Baseline Learning

Baseline learning must be optional.

Required Calibrated v1 commands:

- preview what would be learned
- learn baseline
- show baseline
- reset baseline
- disable baseline

Deferred Full-product controls:

- Companion UI baseline panel
- Companion UI baseline actions for preview, learn, show, reset, and disable

Allowed derived baseline facts:

- validation command category frequency
- validation failure count before success
- common command categories
- typical context range
- common repeated-failure reason codes
- common recovery pattern categories

Disallowed baseline data:

- raw prompts
- raw model outputs
- raw commands
- raw tool inputs
- raw file paths
- secrets
- full transcript storage

### 8. Metrics And Grafana

Prometheus metrics must remain bounded and aggregate.

Recommended new metric families:

- hook events by event, tool category, and result
- validation runs by category and result
- loop signals by signal, severity, and evidence source
- coach actions by action and reason code
- guard blocks by reason code
- baseline builds by scope and result
- unvalidated edit signals by severity

Allowed label examples:

- tool category: bash, apply_patch, mcp, other
- result: success, failure, blocked, unknown
- signal: repeated_validation_failure, blind_retry, unvalidated_edit,
  high_context, incomplete_response
- severity: healthy, watching, careful, stop, blocked
- evidence source: proxy, hook, transcript, user_policy, app_server

Forbidden labels:

- session id
- cwd
- prompt text
- request id
- response id
- raw command
- raw path
- raw tool input
- raw model output

Cost metrics are out of v1 Prometheus scope.

### 9. Cost And Pricing

The product must treat cost as secondary.

Required behavior:

- Show token and context data first.
- Show dollar cost only when clearly labeled.
- In ChatGPT subscription proxy mode, label dollar estimates as API-equivalent
  estimates.
- Do not claim real subscription billing cost.
- Do not use LiteLLM/LIT as trusted budget enforcement by default.
- Allow trusted dollar budget enforcement only with a trusted pricing source.
- Keep dollar estimates out of v1 Prometheus metrics.

### 10. Documentation

Documentation must explain:

- product promise
- evidence boundaries
- hook limitations
- native statusline limitation
- companion UI role
- tmux optional role
- app-server future role
- cost trust model
- baseline learning privacy model
- fake versus live evidence rules
- installation and uninstall behavior

## Testing Decisions

- Test the normalized event model with proxy fixtures.
- Test the normalized event model with hook fixtures.
- Test that each event carries evidence source.
- Test that proxy tool intent is never rendered as tool success.
- Test that hook result evidence is labeled as hook evidence.
- Test signal rules without running Codex.
- Test repeated validation failure detection.
- Test validation skipped after edit detection.
- Test failed, incomplete, and unknown response detection.
- Test high context detection.
- Test blind retry detection.
- Test pricing untrusted warning behavior.
- Test that untrusted pricing cannot enforce dollar blocks.
- Test that trusted pricing can enforce dollar blocks when configured.
- Test that LiteLLM/LIT fallback pricing is advisory unless explicitly trusted.
- Test decision priority.
- Test Healthy is not emitted when evidence is missing.
- Test Watch/Watching state for incomplete evidence.
- Test hook warning output shape.
- Test hook block output shape.
- Test hook continuation behavior from stop events.
- Test that hook failures fail open.
- Test install preview without writing.
- Test install apply only after explicit action.
- Test uninstall removes only Codex Blackbox-owned hook entries.
- Test wrapper run does not mutate user-level Codex config.
- Test companion UI API redaction.
- Test companion UI live update payloads.
- Test postmortem link availability.
- Test baseline preview stores nothing.
- Test baseline learn stores derived-only facts.
- Test baseline reset removes derived facts.
- Test baseline never stores raw prompts, outputs, commands, paths, or secrets.
- Test Prometheus labels stay bounded.
- Test forbidden labels are absent.
- Test cost metric remains absent in v1.
- Test fake fixtures are labeled as fake/local contract evidence.
- Test real live smoke before any live support claim.

Good tests should assert external behavior and public contracts. They should not
depend on private implementation details unless the contract itself is the
implementation boundary, such as normalized event schema or metrics labels.

## Local QA Requirements

Before live QA, run the narrowest relevant local checks for the changed modules:

- formatting checks
- normalized event unit tests
- signal engine unit tests
- decision engine unit tests
- hook fixture tests
- companion UI API tests
- metrics label tests
- baseline privacy tests
- existing proxy/parser/accounting tests when proxy events are affected
- existing watch/status/guard/postmortem tests when decision output changes
- fake Responses regression when the observed model-turn contract changes

Fake Responses regressions prove local contracts only. They do not prove live
Codex support.

## End-To-End Test Strategy

Full coverage must be layered. Do not rely on one real Codex run to prove every
signal because real model behavior is not deterministic enough. Use local
fixtures for exhaustive behavior coverage, then use live Codex CLI and live
Codex UI/Desktop smoke tests to prove the integration paths.

### Test Layers

| Layer | Runs real Codex? | Purpose | Required before support claim |
| --- | --- | --- | --- |
| Unit and contract tests | No | Prove normalized events, session state, signals, decisions, privacy, and metrics. | Yes |
| Fake proxy and hook E2E | No | Prove local proxy/hook contracts with deterministic fixtures. | Yes |
| Companion UI E2E | No by default | Prove UI rendering, live updates, redaction, and postmortem links from fixture/live API data. | Yes |
| Live Codex CLI smoke | Yes | Prove `codex-blackbox run -- codex exec ...` observes real Codex Responses traffic and updates coach state. | Yes |
| Live Codex UI/Desktop smoke | Yes | Prove explicit UI observation mode can observe local Codex UI/Desktop or IDE-extension HTTP Responses fallback traffic through Envoy/core when supported. | Required only for UI support claim |
| Dogfood run | Yes | Prove the product works across realistic repos and longer sessions. | Before release |

### Controlled Dummy Repos

Create temporary repos under a generated directory such as:

```text
$TMPDIR/codex-blackbox-e2e/<run-id>/
```

Each repo must be small, safe, and disposable. The harness should create and
delete these repos automatically unless `--keep-temp` is passed.

Required dummy repo templates:

| Repo | Purpose | Expected signals |
| --- | --- | --- |
| `clean-readonly` | Read files and summarize without edits. | Healthy/Ended, durable proxy evidence, postmortem ready. |
| `validation-failure` | Contains a tiny failing test that Codex can fix. | Validation failed, validation succeeded, timeline shows recovery. |
| `unvalidated-edit` | Contains code plus a validation command; prompt encourages a small edit. | Edit observed, missing validation warning or Stop continuation if validation is skipped. |
| `repeated-failure` | Contains a test failure that is hard to fix immediately. | Repeated validation failure, blind retry warning when fixture-driven. |
| `risky-command` | Fixture-only repo for dangerous command attempts. | `PreToolUse` block for supported tool path. |
| `high-context` | Fixture-only large transcript or proxy fixture. | High context signal without spending live tokens. |
| `pricing-trust` | Fixture-only token/cost cases. | Trusted and untrusted cost behavior. |

Only `clean-readonly`, `validation-failure`, and possibly `unvalidated-edit`
should be used as live Codex smoke repos. The risky-command, high-context, and
pricing-trust cases should be deterministic fixture tests unless a human
explicitly approves a live run.

### CLI E2E Flow

The CLI E2E harness should test the normal wrapper path without mutating
user-level Codex config.

Required flow:

1. Create the temp repo.
2. Start or verify the Codex Blackbox stack.
3. Run a preflight that stops before launching a model turn.
4. Run a real `codex exec` through `codex-blackbox run --watch --`.
5. Verify the wrapper observed at least one new
   `provider="codex_responses"` request.
6. Fetch `status --json`.
7. Fetch `guard --json`.
8. Fetch `postmortem`.
9. Fetch companion UI API state.
10. Verify the companion UI shows the same decision state as CLI status.
11. Verify no unsupported telemetry appears.
12. Save a run artifact directory with logs, JSON, postmortem, and screenshots
    when UI is involved.

Example shape:

```bash
codex-blackbox preflight codex-subscription -- \
  codex exec --cd "$TMP_REPO" --sandbox read-only \
  "E2E preflight: inspect the repo. Do not edit files."

codex-blackbox run --watch -- \
  codex exec --cd "$TMP_REPO" --sandbox workspace-write \
  "E2E smoke: fix the failing test, run validation, and summarize what changed."
```

The exact prompt can vary by repo template, but the harness must keep prompts
short and must record which signals were expected, observed, skipped, or
untrusted.

### Codex UI/Desktop E2E Flow

Codex UI/Desktop testing must be explicit because UI observation may require
user-level configuration changes or a supported UI mode. The wrapper path must
remain non-mutating. A UI/Desktop support claim is valid only for local HTTP
Responses fallback traffic observed and parsed through Envoy/core; WebSocket
frame observation is not claimed.

Required flow:

1. Create the temp repo.
2. Run UI mode preview and record the proposed config changes.
3. Apply UI mode only after explicit approval in the test command.
4. Start or focus local Codex UI/Desktop or the local IDE extension.
5. Run one small prompt in the temp repo.
6. Verify `codex-blackbox ui status --json` reports the observed mode.
7. Verify the core persisted real `provider="codex_responses"` evidence if the
   UI path used observable HTTP Responses traffic.
8. Verify the companion UI shows the UI-origin session separately from CLI
   sessions when enough evidence exists.
9. Disable UI mode with the Blackbox-owned rollback state.
10. Verify user-level config no longer contains Blackbox-owned UI routing
    changes.

Pass criteria:

- If HTTP Responses traffic is observed and parsed through Envoy/core, the UI
  smoke can claim UI/Desktop observation for that Codex version and mode.
- If the UI uses WebSocket-only Responses traffic that is not observed, the test
  must report `websocket_only_unobservable` or equivalent and cannot claim live
  UI support.
- If HTTP traffic is observed but not parsed, the test must report
  `http_responses_unparsed` or equivalent and treat it as parser work, not a
  supported UI claim.

### Companion UI Browser E2E Flow

The companion UI must be tested with deterministic fixture data and at least one
live CLI session.

Required checks:

- Active session appears.
- State badge matches CLI `status --json`.
- Timeline updates as new fixture/live events arrive.
- Evidence source is visible for proxy and hook-derived facts.
- Validation panel groups failed and successful validation attempts.
- Postmortem link appears only when ready.
- Raw prompts, raw outputs, raw commands, and raw paths are not displayed by
  default.
- Cost is absent or clearly marked advisory/API-equivalent.
- Narrow and mobile-ish viewports do not hide the state or next action.

### Hook E2E Flow

Hook E2E must be split into deterministic fixture coverage and one live smoke.

Fixture coverage must prove:

- `PreToolUse` warning.
- `PreToolUse` block.
- `PostToolUse` validation failure summary.
- `Stop` continuation prompt.
- hook failure fail-open behavior.
- hook evidence source labeling.
- no raw secret leakage.

Live smoke must prove only that:

- a real installed hook executes in a real Codex session,
- the hook action is recorded by Codex Blackbox,
- hook evidence is labeled as hook evidence,
- the hook can be uninstalled cleanly.

### Coverage Matrix

| Capability | Unit | Fake E2E | CLI live | UI live | Companion UI |
| --- | --- | --- | --- | --- | --- |
| Proxy model-turn parsing | Yes | Yes | Yes | Only HTTP Responses fallback through Envoy/core | Visible |
| Hook event ingestion | Yes | Yes | One path | One path if installed | Visible |
| Tool intent/result separation | Yes | Yes | Yes | Only HTTP Responses fallback through Envoy/core | Visible |
| Validation failure signal | Yes | Yes | Preferred | Optional | Visible |
| Repeated failure signal | Yes | Yes | Optional | Optional | Visible |
| Unvalidated edit signal | Yes | Yes | Optional | Optional | Visible |
| High context signal | Yes | Yes | No by default | No by default | Visible from fixture |
| Cost trust behavior | Yes | Yes | Advisory only | Advisory only | Visible/advisory |
| Metrics label safety | Yes | Yes | Optional scrape | Optional scrape | Not applicable |
| Postmortem consistency | Yes | Yes | Yes | If observable | Link/render |
| Config reversibility | Yes | Fixture | Wrapper non-mutating | Required | Not applicable |

### Artifact Requirements

Every full E2E run should write an artifact directory containing:

- run metadata
- temp repo template name
- Codex Blackbox command lines
- preflight output
- observed session IDs
- `status --json`
- `guard --json`
- postmortem JSON or Markdown
- companion UI API snapshot
- metrics scrape when relevant
- hook action log when hooks are installed
- UI screenshots for companion UI and UI/Desktop smoke
- final classification: real, fake, skipped, unsupported, or untrusted

## Live QA Requirements

Live QA is required before this feature is called supported for real Codex.

Minimum live proof:

- One real Codex run through the existing wrapper path observes new durable
  Codex Responses evidence.
- The companion UI shows the live session.
- The decision state updates from proxy evidence.
- At least one installed hook path is exercised with explicit user approval.
- Hook evidence is labeled separately from proxy evidence.
- A completed postmortem matches the live session state.
- No UI, metric, or report claims unsupported tool result success from proxy
  intent.
- No UI, metric, or report claims real dollar billing for ChatGPT subscription
  mode.
- Any UI/Desktop support claim is limited to local HTTP Responses fallback
  traffic observed through Envoy/core; WebSocket frame observation remains
  unsupported.

Dogfood proof:

- Multiple real sessions across at least two repositories.
- At least one session with validation success.
- At least one session with validation failure.
- At least one session with high context or incomplete/failed response, if
  naturally encountered.
- Postmortems saved for all sessions.
- Companion UI screenshots or captured reports for at least one active and one
  ended session.
- QA notes state which capabilities were real, fake, skipped, or untrusted.

## Out Of Scope

- Native Codex CLI custom statusline adapter.
- Tmux as the default experience.
- A full app-server custom client in v1.
- Hosted web/cloud Codex observation.
- Generic system proxying.
- TLS MITM.
- WebSocket frame observation.
- Tool result success from proxy-only evidence.
- MCP lifecycle reporting from proxy-only evidence.
- Skill lifecycle reporting from proxy-only evidence.
- Cache TTL or cache rebuild reporting.
- Provider quota or account cap claims from proxy-only evidence.
- Raw transcript as live authoritative telemetry.
- Raw prompt, output, command, path, or secret storage for baseline learning.
- Dollar-based v1 Prometheus metrics.
- Dollar budget blocking from untrusted pricing.

## Further Notes

This PRD intentionally changes the product center from "observe and explain a
Codex run after the fact" toward "coach Codex while it runs."

The durable evidence boundary still matters:

- Proxy evidence is authoritative for model turns.
- Hook evidence is useful for coach actions but incomplete.
- Transcript evidence is offline and unstable.
- App-server is a future rich-client integration path.
- User policy is trusted only when explicit.

Recommended implementation order:

1. Normalize events and session state.
2. Feed proxy evidence into the new model.
3. Add hook event ingestion behind explicit installation.
4. Add signal and decision rules.
5. Render companion UI from the same decision state.
6. Add optional baseline learning through derived-only CLI/API state.
7. Add bounded metrics for coach activity.
8. Add deferred Companion baseline panel and local warning snooze/mute controls
   if Full PRD completion is required.
9. Consider app-server or IDE packaging after the local companion UI is stable.

Official docs used during planning:

- Codex hooks: https://developers.openai.com/codex/hooks
- Codex slash commands and statusline: https://developers.openai.com/codex/cli/slash-commands
- Codex app-server: https://developers.openai.com/codex/app-server
- Codex configuration reference: https://developers.openai.com/codex/config-reference
- Responses API overview: https://developers.openai.com/api/reference/responses/overview
- Responses compaction: https://developers.openai.com/api/docs/guides/compaction
- OpenAI tools guide: https://developers.openai.com/api/docs/guides/tools
