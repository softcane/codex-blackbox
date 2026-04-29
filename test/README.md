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
