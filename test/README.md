# Test Harness Status

This directory now contains Coditor-specific fake and real validation harnesses.
Some older copied baseline scripts may still exist, but the sections below name
the scripts that are intended to validate the Codex/OpenAI path.

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
Codex persistence and token accounting, checks Prometheus/Grafana, confirms the
CLI `coditor run --dry-run -- codex ...` uses the ChatGPT subscription proxy
override, and finally stops
`coditor-core` to verify Envoy failure-open behavior.

Run it from the repository root:

```sh
./test/e2e-openai-responses-full.sh
```

The script writes failure artifacts and Compose logs under `/tmp` by default
and prints that path on failure. It does not require OpenAI credentials, does
not launch Codex, and is not a real Codex compatibility claim.

## Phase 5B ChatGPT/Codex Config Validation

`docker-compose.yml` mounts `envoy/envoy.yaml`, the default ChatGPT/Codex
subscription proxy used by `coditor up` and `coditor run -- codex ...`.

Static validation:

```sh
./test/validate-openai-config.sh
```

This validation does not require credentials and does not contact OpenAI.
Live ChatGPT-auth Codex backend traffic is not validated here.

## Phase 9B ChatGPT/Codex Subscription Preflight

`docker-compose.yml` mounts `envoy/envoy.yaml`, which routes `/backend-api`
traffic to `chatgpt.com` with host rewrite and SNI. The wrapper sets
`chatgpt_base_url` to `/backend-api` for auxiliary backend calls and
adds a `coditor-chatgpt` custom provider at `/backend-api/codex` for model
turns. The provider keeps ChatGPT auth, disables Codex WebSockets so model
turns use HTTP Responses through Envoy, does not use `OPENAI_API_KEY`, does
not include `fake-openai`, and fails `codex exec` runs that exit successfully
without a new Coditor-observed Codex Responses request. The subscription
overrides are attached to the `exec` subcommand because Codex 0.125 validates
root-level `codex -c ... exec` overrides but does not carry them into the
in-process app-server thread start path. When Coditor is launched from Codex
Desktop, the wrapper also removes inherited parent-session `CODEX_*` variables
from the child Codex process so the run uses the explicit CLI config path.
The wrapper closes child stdin for Codex runs so `codex exec` cannot consume
loop manifests or other harness input.

The manual preflight command verifies local Codex ChatGPT login, starts the
subscription-mode Docker stack, and prints the exact live command without
launching a Codex turn:

```sh
cargo run -q -p coditor-cli -- preflight codex-subscription -- codex exec \
  --cd /Users/pradeepsingh/code/coditor \
  --sandbox read-only \
  "Read AGENTS.md and docs/remaining-phases.md, then summarize the current next phase in 3 bullets. Do not edit files."
```

Do not run the printed live command until explicitly approved. This preflight
stops before a model turn and does not validate live Codex subscription support.
Real multi-session dogfood is outside the Envoy-only cleanup gate.
