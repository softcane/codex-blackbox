# Coditor Fake Codex Smoke

Timestamp: 20260430T100657Z
Coditor commit: f0c4b71
Codex version: codex-cli 0.122.0
Final attempt: attempt3
Final session id: 019ddde0-3124-7c03-ab92-cd7febebe196
Codex exit code: 0

## Result

PASS: actual Codex CLI reached the fake OpenAI Responses upstream through Coditor and exited successfully.

## Route

- Docker files: docker-compose.yml + test/docker-compose.openai-responses.yml
- Envoy config mounted: test/envoy.openai-responses.e2e.yaml
- Envoy listener: http://127.0.0.1:10000
- Upstream observed in Envoy logs: fake-openai container on port 8000
- Fake upstream log: POST /v1/responses HTTP/1.1 200

## Observed Coditor Evidence

- /watch emitted session_start, codex_turn_summary, and context_status.
- SQLite sessions, requests, and turn_snapshots contain provider=codex_responses for the final session.
- Token accounting: input=1280, cached=512, uncached=768, output=96, reasoning=32, total=1376.
- Prometheus sum(coditor_requests_total) reported 2 because attempts 2 and 3 both reached fake upstream.
- Grafana health was ok and coditor-main was provisioned.

## Attempts

- Initial default-CODEX_HOME attempt exited before model traffic because Codex was ChatGPT-authenticated while the wrapper forced API-key mode.
- Attempt 2 used a temporary CODEX_HOME and succeeded, but Codex performed an unrelated plugin metadata warmup request to chatgpt.com and got 401.
- Attempt 3 used a temporary CODEX_HOME plus --disable plugins and --disable general_analytics; it succeeded and captured no api.openai.com or chatgpt.com references in attempt3 artifacts.

## Limitations

This validates no-credential Codex-through-fake-proxy behavior only. It does not validate real OpenAI API traffic or production readiness.
