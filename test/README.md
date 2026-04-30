# Test Harness Status

The files in this directory are unported and deferred. They were copied from
the Phase 0A baseline to preserve the local test-harness shape only.

UNPORTED: copied baseline, Codex support not implemented yet. These tests still
exercise Anthropic/Claude assumptions and are not Coditor validation.

## Phase 5A Fake OpenAI Responses E2E

`test/e2e-openai-responses.sh` is the first Coditor-specific fake upstream
check. It starts only `coditor-core`, Envoy, and `test/fake-openai.py` through
Docker Compose, sends `POST /v1/responses` through Envoy, and verifies streamed
Responses SSE plus Codex `SessionStart`/`ContextStatus` watch events.

Run it from the repository root:

```sh
./test/e2e-openai-responses.sh
```

The script uses checked-in fixtures only. It does not require OpenAI
credentials and must not be treated as real Codex support validation.

## Phase 8B Fake Observability Validation

`test/observability-openai-responses.sh` starts the fake OpenAI Responses stack
plus Prometheus and Grafana. It sends one fixture Responses request, verifies
Codex request/token/context/diagnosis metrics, checks that Prometheus labels do
not leak session ids, and confirms the provisioned Coditor dashboard loads with
Phase 8B panels backed by scraped metrics.

Run it from the repository root:

```sh
./test/observability-openai-responses.sh
```

This validation does not require OpenAI credentials and does not launch real
Codex sessions. Rate-limit header names remain unverified; the candidate file
under `test/fixtures/` is fixture-only documentation, not parser input.

## Phase 9A Full Fake OpenAI Responses E2E

`test/e2e-openai-responses-full.sh` is the broader fake regression gate before
any real Codex smoke test. It starts the fake OpenAI stack with Prometheus and
Grafana, sends parallel fixture `/v1/responses` requests with distinct prompts
and mixed cwd metadata, covers completed/failed/incomplete streams, exercises
split SSE chunking through Envoy, verifies late `/watch` replay, checks SQLite
Codex persistence and token accounting, checks Prometheus/Grafana, runs the
CLI `coditor run --dry-run -- codex ...` smoke, and finally stops
`coditor-core` to verify Envoy failure-open behavior.

Run it from the repository root:

```sh
./test/e2e-openai-responses-full.sh
```

The script writes failure artifacts and Compose logs under `/tmp` by default
and prints that path on failure. It does not require OpenAI credentials, does
not launch Codex, and is not a real Codex compatibility claim.

## Phase 5B Manual OpenAI API-Key Config

`docker-compose.openai.yml` is an opt-in Compose override that mounts
`envoy/envoy.openai.yaml`. It is for manual API-key-mode experiments only; the
default stack and CLI still use the copied unported runtime path.

Static validation:

```sh
./test/validate-openai-config.sh
```

This validation does not require OpenAI credentials and does not contact
OpenAI. ChatGPT-auth Codex backend routing is not supported here.
