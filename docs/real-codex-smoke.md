# Real Codex Smoke And Dogfood Log

This log separates live ChatGPT/Codex evidence from fixture evidence. A passing
fake e2e is still not a real support claim; the entries below are real Codex
runs through the default `docker-compose.yml` and `envoy/envoy.yaml` path.

## 2026-04-30 UTC / 2026-05-01 Europe/Stockholm

### Preconditions

- `cargo test -p coditor-cli` passed.
- `./test/e2e-openai-responses-full.sh` passed first with run id
  `openai-full-e2e-1777587872-34990`.
- Local `codex login status` confirmed ChatGPT auth.
- The dogfood harness removed stale Compose services before starting the real
  stack, cleared Docker Compose `COMPOSE_FILE`, and pinned
  `CODITOR_COMPOSE_FILE` to the absolute default `docker-compose.yml` path.

### Codex Version

```text
codex-cli 0.125.0
```

### Config Used

The wrapper used command-line overrides only; it did not edit
`~/.codex/config.toml` and did not use `OPENAI_API_KEY`.

```text
-c 'chatgpt_base_url="http://127.0.0.1:10000/backend-api"'
-c 'model_provider="coditor-chatgpt"'
-c 'model_providers.coditor-chatgpt.name="OpenAI"'
-c 'model_providers.coditor-chatgpt.base_url="http://127.0.0.1:10000/backend-api/codex"'
-c 'model_providers.coditor-chatgpt.wire_api="responses"'
-c 'model_providers.coditor-chatgpt.requires_openai_auth=true'
-c 'model_providers.coditor-chatgpt.supports_websockets=false'
-c features.enable_request_compression=false
```

The wrapper also removed inherited parent-session `CODEX_CI`,
`CODEX_INTERNAL_ORIGINATOR_OVERRIDE`, `CODEX_SHELL`, and `CODEX_THREAD_ID`, and
closed child stdin for `codex exec`.

### Phase 9B Smoke Command

```sh
./test/dogfood-codex-sessions.sh --mode real --sessions 1 --repos same \
  --report-dir reports/dogfood/smoke-20260430T223720Z
```

Report artifacts are under `reports/dogfood/smoke-20260430T223720Z/`.

Observed real session id:

```text
019de08a-44dd-76a1-abda-a9daa96fbe04
```

Observed events and data:

- Child `codex exec` exited `0`.
- Coditor observed real `provider="codex_responses"` traffic through Envoy.
- `/watch` emitted `SessionStart`, `CodexTurnSummary`, and `ContextStatus`.
- Served model was captured as `gpt-5.5`.
- Smoke metrics showed 2 Codex requests and token kinds for input, cached input,
  uncached input, output, reasoning output, and total tokens.
- No Anthropic `CacheEvent` was emitted for Codex cached input.

### Phase 9C Dogfood Command

```sh
./test/dogfood-codex-sessions.sh --mode real --sessions 4 --repos mixed \
  --include-mcp --timeout-seconds 600 \
  --report-dir reports/dogfood/full-20260430T223917Z
```

Report artifacts are under `reports/dogfood/full-20260430T223917Z/`.

Outcome: `partial`.

Passed:

- 4 real Codex sessions exited `0`.
- Same-repo coverage: 3 sessions.
- Different-repo coverage: 1 session.
- MCP config was detected.
- `/watch` captured 4 `SessionStart` events, 11 `CodexTurnSummary` events, and
  11 `ContextStatus` events.
- SQLite persisted 4 Codex sessions and 11 request rows with served model
  values.
- SQLite token math did not double-count cached input.
- Prometheus exposed Codex request, token, duration, and context metrics without
  session-id labels.
- Grafana was reachable and provisioned `coditor-main` with panels backed by
  scraped Prometheus metrics.

Missing:

- Tool-oriented prompts produced Codex JSONL `command_execution` items, but
  Coditor did not emit `ToolUse` or `ToolResult` watch events for them.
- The MCP prompt reached `openaiDeveloperDocs`; the MCP tool call was cancelled
  inside the child session, and Coditor did not emit `McpEvent` watch events.

### Broader Calibration Update

A later 5-repo live validation is recorded under
`reports/live-codex-validation-20260430T234815Z/`.

That run captured:

- 5 real Codex sessions, all exit `0`.
- Marker-backed SQLite rows whose `initial_prompt` stores the real task instead
  of the injected AGENTS/environment preamble.
- Distinct persisted display names: `coditor`, `clauditor`, `claude-code`,
  `LLMs-from-scratch`, and `contextgc_poc`.
- Real shell `ToolUse`/`ToolResult` events from Codex JSONL
  `command_execution`.
- Real `McpEvent` rows from Codex JSONL `mcp_tool_call`.
- A real `SkillEvent` for `openai-docs`.
- Matching SQLite session/request token totals and Prometheus request/token
  totals.

Open: the MCP calls were still cancelled by the child session, so the MCP
success-result path remains unproven.

### Rollback

```sh
docker compose -p coditor -f docker-compose.yml down --remove-orphans -t 5
```

The rollback command was run after the dogfood pass and left no Coditor Compose
services running.

### Limitations

- Live support remains experimental until the MCP success-result path and
  future Codex version compatibility are validated.
- Real Codex request prompt excerpts now strip injected AGENTS/environment
  preambles before persistence; display names are stored separately.
- Pricing remains untrusted for budget enforcement.
- The smoke and dogfood reports prove this local ChatGPT subscription path on
  Codex 0.125.0; they do not prove future Codex versions or API-key mode.
