#!/usr/bin/env bash
# Phase 9B-pre no-credential Codex-through-fake-proxy smoke.
#
# This launches the real Codex CLI, but routes model traffic only to the local
# fake OpenAI Responses upstream. It must not use a real API key or validate
# real OpenAI/Codex support.
set -euo pipefail

cd "$(dirname "$0")/.."

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

RUN_TS="${CODITOR_FAKE_CODEX_SMOKE_TS:-$(date -u +%Y%m%dT%H%M%SZ)}"
RUN_ID="${CODITOR_FAKE_CODEX_SMOKE_RUN_ID:-codex-fake-smoke-$(date +%s)-$$}"
REPORT_DIR="${CODITOR_FAKE_CODEX_SMOKE_REPORT_DIR:-reports/smoke/$RUN_TS}"
ATTEMPT_DIR="$REPORT_DIR/final"
CODEX_HOME_DIR="${ATTEMPT_DIR}/codex-home"
CORE_URL="http://localhost:9091"
ENVOY_URL="http://localhost:10000"
PROMETHEUS_URL="http://localhost:9092"
GRAFANA_URL="http://localhost:3000"
COMPOSE_FILES=(-f docker-compose.yml -f test/docker-compose.openai-responses.yml)
PROMPT="Read AGENTS.md and docs/remaining-phases.md, then summarize the current next phase in 3 bullets. Do not edit files."
SMOKE_COMPLETED=0
WATCH_PID=""

mkdir -p "$ATTEMPT_DIR"

pass() { printf "%bPASS%b: %s\n" "$GREEN" "$NC" "$1"; }
info() { printf "%bINFO%b: %s\n" "$YELLOW" "$NC" "$1"; }

compose() {
    docker compose "${COMPOSE_FILES[@]}" "$@"
}

capture_logs() {
    compose ps >"$ATTEMPT_DIR/compose-ps-captured.txt" 2>&1 || true
    compose logs --no-color coditor-core >"$ATTEMPT_DIR/coditor-core.log" 2>&1 || true
    compose logs --no-color envoy >"$ATTEMPT_DIR/envoy.log" 2>&1 || true
    compose logs --no-color fake-openai >"$ATTEMPT_DIR/fake-openai.log" 2>&1 || true
}

cleanup_stack_on_failure() {
    if [ -n "$WATCH_PID" ]; then
        kill "$WATCH_PID" >/dev/null 2>&1 || true
    fi
    rm -rf "$CODEX_HOME_DIR"
    if [ "$SMOKE_COMPLETED" = "1" ] || [ "${CODITOR_FAKE_CODEX_SMOKE_KEEP_STACK:-0}" = "1" ]; then
        return
    fi
    compose down --remove-orphans -t 5 >/dev/null 2>&1 || true
}

cleanup_stack_on_success() {
    rm -rf "$CODEX_HOME_DIR"
    if [ "${CODITOR_FAKE_CODEX_SMOKE_KEEP_STACK:-0}" = "1" ]; then
        info "Leaving fake Codex smoke stack running because CODITOR_FAKE_CODEX_SMOKE_KEEP_STACK=1"
        return
    fi
    compose down --remove-orphans -t 5 >/dev/null
    pass "Fake Codex smoke stack stopped"
}

fail() {
    printf "%bFAIL%b: %s\n" "$RED" "$NC" "$1" >&2
    info "Artifacts and logs: $REPORT_DIR" >&2
    capture_logs
    exit 1
}

trap cleanup_stack_on_failure EXIT

require_cmd() {
    command -v "$1" >/dev/null 2>&1 || fail "Missing required command: $1"
}

for cmd in docker curl python3 cargo codex sqlite3 rg; do
    require_cmd "$cmd"
done

wait_for_http() {
    local label=$1
    local url=$2
    for _ in $(seq 1 60); do
        if curl -fsS --max-time 2 "$url" >/dev/null 2>&1; then
            return 0
        fi
        sleep 2
    done
    compose ps
    fail "$label did not become reachable at $url"
}

assert_file_contains() {
    local file=$1
    local needle=$2
    local label=$3
    grep -q "$needle" "$file" && pass "$label" || fail "$label missing '$needle' in $file"
}

assert_file_not_contains() {
    local file=$1
    local needle=$2
    local label=$3
    if grep -q "$needle" "$file"; then
        fail "$label unexpectedly contained '$needle' in $file"
    fi
    pass "$label"
}

config_fingerprint() {
    python3 - "$HOME/.codex/config.toml" <<'PY'
import hashlib
import sys
from pathlib import Path

path = Path(sys.argv[1])
if not path.exists():
    print("missing")
else:
    print(hashlib.sha256(path.read_bytes()).hexdigest())
PY
}

extract_session_id() {
    python3 - "$ATTEMPT_DIR/codex-events.jsonl" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as handle:
    for line in handle:
        if not line.strip():
            continue
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if event.get("type") == "thread.started" and event.get("thread_id"):
            print(event["thread_id"])
            raise SystemExit(0)
raise SystemExit(1)
PY
}

assert_json_path() {
    local file=$1
    local label=$2
    local program=$3
    python3 - "$file" "$program" <<'PY' && pass "$label" || fail "$label"
import json
import sys

path = sys.argv[1]
program = sys.argv[2]
with open(path, encoding="utf-8") as handle:
    data = json.load(handle)
scope = {"data": data}
exec(program, scope, scope)
PY
}

wait_for_sessions_api_session() {
    local file=$1
    local err_file=$2
    local deadline=$((SECONDS + 150))
    while [ "$SECONDS" -lt "$deadline" ]; do
        curl -fsS --max-time 5 "$CORE_URL/api/sessions?limit=200&days=1" >"$file" 2>"$err_file" || true
        if SESSION_ID="$SESSION_ID" python3 - "$file" <<'PY'
import json
import os
import sys

try:
    with open(sys.argv[1], encoding="utf-8") as handle:
        data = json.load(handle)
except (OSError, json.JSONDecodeError):
    raise SystemExit(1)
for item in data.get("sessions", []):
    if item.get("session_id") == os.environ["SESSION_ID"]:
        raise SystemExit(0)
raise SystemExit(1)
PY
        then
            pass "/api/sessions includes smoke session"
            return 0
        fi
        sleep 5
    done
    fail "/api/sessions includes smoke session"
}

copy_sqlite_snapshot() {
    local container_id
    container_id=$(compose ps -q coditor-core)
    [ -n "$container_id" ] || fail "coditor-core container id unavailable for DB snapshot"
    rm -rf "$ATTEMPT_DIR/db"
    mkdir -p "$ATTEMPT_DIR/db"
    docker cp "$container_id:/data/." "$ATTEMPT_DIR/db/" >/dev/null
}

write_db_checks() {
    SESSION_ID="$SESSION_ID" DB_PATH="$ATTEMPT_DIR/db/coditor.db" OUT_PATH="$ATTEMPT_DIR/db-checks.json" python3 - <<'PY'
import json
import os
import sqlite3

session_id = os.environ["SESSION_ID"]
db_path = os.environ["DB_PATH"]
out_path = os.environ["OUT_PATH"]
conn = sqlite3.connect(db_path)
conn.row_factory = sqlite3.Row

session = conn.execute(
    """
    SELECT session_id, model, request_count, total_codex_input_tokens,
           total_codex_cached_input_tokens, total_codex_uncached_input_tokens,
           total_codex_output_tokens, total_codex_reasoning_output_tokens,
           total_codex_tokens
    FROM sessions WHERE session_id = ?
    """,
    (session_id,),
).fetchone()
request = conn.execute(
    """
    SELECT request_id, session_id, provider, requested_model, served_model,
           codex_status, codex_input_tokens, codex_cached_input_tokens,
           codex_uncached_input_tokens, codex_output_tokens,
           codex_reasoning_output_tokens, codex_total_tokens, codex_response_id
    FROM requests WHERE session_id = ?
    """,
    (session_id,),
).fetchone()
turn = conn.execute(
    """
    SELECT request_id, session_id, provider, codex_status, codex_input_tokens,
           codex_cached_input_tokens, codex_uncached_input_tokens,
           codex_output_tokens, codex_reasoning_output_tokens, codex_total_tokens,
           codex_response_id
    FROM turn_snapshots WHERE session_id = ?
    """,
    (session_id,),
).fetchone()

payload = {
    "session": dict(session) if session else None,
    "request": dict(request) if request else None,
    "turn_snapshot": dict(turn) if turn else None,
}
with open(out_path, "w", encoding="utf-8") as handle:
    json.dump(payload, handle, indent=2)

if session is None or request is None or turn is None:
    raise SystemExit("missing SQLite row for smoke session")
if request["provider"] != "codex_responses" or turn["provider"] != "codex_responses":
    raise SystemExit("SQLite provider was not codex_responses")
if request["codex_status"] != "completed" or turn["codex_status"] != "completed":
    raise SystemExit("SQLite codex_status was not completed")
for row_name, row in [("request", request), ("turn_snapshot", turn)]:
    input_tokens = row["codex_input_tokens"] or 0
    cached = row["codex_cached_input_tokens"] or 0
    uncached = row["codex_uncached_input_tokens"] or 0
    output = row["codex_output_tokens"] or 0
    total = row["codex_total_tokens"] or 0
    if input_tokens != cached + uncached:
        raise SystemExit(f"{row_name} cached input double-count check failed")
    if total != input_tokens + output:
        raise SystemExit(f"{row_name} total token check failed")
PY
}

write_prometheus_queries() {
    PROMETHEUS_URL="$PROMETHEUS_URL" OUT_PATH="$ATTEMPT_DIR/prometheus-queries.json" python3 - <<'PY'
import json
import os
import time
import urllib.parse
import urllib.request

prometheus_url = os.environ["PROMETHEUS_URL"]
out_path = os.environ["OUT_PATH"]
queries = {
    "requests_total": "sum(coditor_requests_total)",
    "input_tokens": 'sum(coditor_tokens_total{kind="input"})',
    "output_tokens": 'sum(coditor_tokens_total{kind="output"})',
    "context_samples": 'sum(coditor_context_fill_percent_count{provider="codex_responses"})',
}


def get_json(path, params):
    query = "?" + urllib.parse.urlencode(params)
    with urllib.request.urlopen(prometheus_url + path + query, timeout=8) as response:
        return json.loads(response.read().decode("utf-8"))


def prom_value(expr):
    payload = get_json("/api/v1/query", {"query": expr})
    if payload.get("status") != "success":
        raise RuntimeError(payload)
    result = payload.get("data", {}).get("result", [])
    if not result:
        return 0.0
    return float(result[0]["value"][1])


def wait_until(label, expr, minimum):
    deadline = time.time() + 90
    last_value = 0.0
    while time.time() < deadline:
        last_value = prom_value(expr)
        if last_value >= minimum:
            return last_value
        time.sleep(2)
    raise SystemExit(f"{label} stayed below {minimum}: {last_value}")


values = {
    "requests_total": wait_until("requests_total", queries["requests_total"], 1.0),
    "input_tokens": wait_until("input_tokens", queries["input_tokens"], 1.0),
    "output_tokens": wait_until("output_tokens", queries["output_tokens"], 1.0),
    "context_samples": wait_until("context_samples", queries["context_samples"], 1.0),
}
with open(out_path, "w", encoding="utf-8") as handle:
    json.dump({"queries": queries, "values": values}, handle, indent=2)
PY
}

write_summary() {
    cat >"$REPORT_DIR/summary.md" <<EOF
# Coditor Fake Codex Smoke

Timestamp: $RUN_TS
Run id: $RUN_ID
Coditor commit: $(cat "$REPORT_DIR/coditor-commit.txt")
Codex version: $(cat "$REPORT_DIR/codex-version.txt")
Session id: $SESSION_ID
Codex exit code: $(cat "$ATTEMPT_DIR/codex-exit-code.txt")

## Result

PASS: actual Codex CLI reached the fake OpenAI Responses upstream through Coditor and exited successfully.

## Route

- Docker files: docker-compose.yml + test/docker-compose.openai-responses.yml
- Envoy config mounted: test/envoy.openai-responses.e2e.yaml
- Envoy listener: $ENVOY_URL
- Fake upstream log contains POST /v1/responses HTTP/1.1 200.

## Evidence

- /watch emitted session_start, codex_turn_summary, and context_status.
- /api/sessions and /api/diagnosis returned the smoke session.
- SQLite db-checks.json contains provider=codex_responses and non-double-counted cached input.
- Prometheus queries observed request, input-token, output-token, and context samples.
- Grafana health was ok and coditor-main was provisioned.
- Final attempt artifacts contain no api.openai.com or chatgpt.com references.

## Limitation

This validates no-credential Codex-through-fake-proxy behavior only. It does not validate real OpenAI API traffic or production readiness.
EOF

    cat >"$REPORT_DIR/summary.json" <<EOF
{
  "status": "pass_fake_only",
  "timestamp": "$RUN_TS",
  "run_id": "$RUN_ID",
  "session_id": "$SESSION_ID",
  "real_openai_validated": false,
  "final_attempt_no_api_openai_or_chatgpt_references": true
}
EOF
}

echo "=== Coditor Codex-through-fake-proxy Smoke ==="
info "run_id=$RUN_ID"
info "report_dir=$REPORT_DIR"

before_config=$(config_fingerprint)

echo "OPENAI_API_KEY=[dummy-redacted] cargo run -q -p coditor-cli -- run -- codex exec --cd $(pwd) --sandbox read-only --json --ignore-user-config --disable plugins --disable general_analytics \"$PROMPT\"" \
    >"$ATTEMPT_DIR/command-redacted.txt"
git rev-parse --short HEAD >"$REPORT_DIR/coditor-commit.txt"
codex --version >"$REPORT_DIR/codex-version.txt" 2>&1

info "Starting Docker Compose with fake OpenAI, Prometheus, and Grafana..."
compose down --remove-orphans -t 5 2>/dev/null || true
compose up -d --build coditor-core envoy fake-openai prometheus grafana

info "Waiting for fake stack..."
wait_for_http "coditor-core" "$CORE_URL/health"
wait_for_http "envoy fake route" "$ENVOY_URL/health"
wait_for_http "prometheus" "$PROMETHEUS_URL/-/ready"
wait_for_http "grafana" "$GRAFANA_URL/api/health"
compose exec -T fake-openai python -c "import urllib.request; print(urllib.request.urlopen('http://localhost:8000/health', timeout=2).read().decode().strip())" \
    >"$ATTEMPT_DIR/fake-openai-health.txt" 2>&1 || fail "fake-openai health check failed"
pass "Core, Envoy fake route, fake-openai, Prometheus, and Grafana are reachable"

compose ps >"$ATTEMPT_DIR/compose-ps-before.txt" 2>&1
docker inspect coditor-envoy-1 --format '{{range .Mounts}}{{println .Source "->" .Destination}}{{end}}' \
    >"$ATTEMPT_DIR/envoy-mounts.txt" 2>&1
assert_file_contains "$ATTEMPT_DIR/envoy-mounts.txt" "test/envoy.openai-responses.e2e.yaml" "Envoy uses fake upstream config"

curl -fsS --max-time 3 "$CORE_URL/health" >"$ATTEMPT_DIR/core-health.txt" 2>&1
curl -fsS --max-time 3 "$ENVOY_URL/health" >"$ATTEMPT_DIR/envoy-health-through-fake.txt" 2>&1

mkdir -p "$CODEX_HOME_DIR"
(curl -sS -N --max-time 60 -H "Accept: text/event-stream" "$CORE_URL/watch" \
    >"$ATTEMPT_DIR/watch.sse" 2>"$ATTEMPT_DIR/watch.err" || true) &
WATCH_PID=$!
sleep 1

info "Running actual Codex through coditor run with dummy local API key..."
set +e
CODEX_HOME="$CODEX_HOME_DIR" OPENAI_API_KEY=coditor-fake-smoke \
    cargo run -q -p coditor-cli -- run -- codex exec \
        --cd "$(pwd)" \
        --sandbox read-only \
        --json \
        --ignore-user-config \
        --disable plugins \
        --disable general_analytics \
        "$PROMPT" \
        >"$ATTEMPT_DIR/codex-events.jsonl" \
        2>"$ATTEMPT_DIR/codex-stderr.log" \
        </dev/null
codex_status=$?
set -e
printf "%s\n" "$codex_status" >"$ATTEMPT_DIR/codex-exit-code.txt"
wait "$WATCH_PID" 2>/dev/null || true
WATCH_PID=""
[ "$codex_status" = "0" ] || fail "codex exited with status $codex_status"
pass "Codex exited successfully through fake proxy"

after_config=$(config_fingerprint)
[ "$before_config" = "$after_config" ] || fail "~/.codex/config.toml changed"
pass "~/.codex/config.toml unchanged"

SESSION_ID=$(extract_session_id) || fail "could not extract Codex thread/session id"
export SESSION_ID
printf "%s\n" "$SESSION_ID" >"$ATTEMPT_DIR/session-id.txt"
sed -n 's/^data: //p' "$ATTEMPT_DIR/watch.sse" >"$ATTEMPT_DIR/watch.ndjson"

assert_file_contains "$ATTEMPT_DIR/codex-events.jsonl" "Workspace packages: coditor-core and coditor-cli." "Codex consumed fake fixture response"
assert_file_contains "$ATTEMPT_DIR/watch.ndjson" "$SESSION_ID" "watch captured smoke session"
assert_file_contains "$ATTEMPT_DIR/watch.ndjson" '"type":"session_start"' "watch captured SessionStart"
assert_file_contains "$ATTEMPT_DIR/watch.ndjson" '"type":"codex_turn_summary"' "watch captured Codex turn summary"
assert_file_contains "$ATTEMPT_DIR/watch.ndjson" '"type":"context_status"' "watch captured ContextStatus"
assert_file_not_contains "$ATTEMPT_DIR/watch.ndjson" '"type":"cache_event"' "watch did not emit Anthropic CacheEvent"

curl -fsS --max-time 5 "$CORE_URL/api/sessions" >"$ATTEMPT_DIR/sessions.json" 2>"$ATTEMPT_DIR/sessions.err"
curl -fsS --max-time 5 "$CORE_URL/api/diagnosis/$SESSION_ID" >"$ATTEMPT_DIR/diagnosis.json" 2>"$ATTEMPT_DIR/diagnosis.err"

SESSION_ID="$SESSION_ID" assert_json_path "$ATTEMPT_DIR/diagnosis.json" "/api/diagnosis returns smoke session" \
    'import os; assert data.get("session_id") == os.environ["SESSION_ID"]; assert data.get("degraded") is False'
info "Waiting for inactivity finalization so /api/sessions lists the smoke session..."
wait_for_sessions_api_session "$ATTEMPT_DIR/sessions-latest.json" "$ATTEMPT_DIR/sessions-latest.err"

copy_sqlite_snapshot
write_db_checks
assert_file_contains "$ATTEMPT_DIR/db-checks.json" '"provider": "codex_responses"' "SQLite captured codex_responses rows"

curl -fsS --max-time 5 "$CORE_URL/metrics" >"$ATTEMPT_DIR/metrics.prom" 2>"$ATTEMPT_DIR/metrics.err"
assert_file_contains "$ATTEMPT_DIR/metrics.prom" "coditor_requests_total" "core metrics expose request counter"
assert_file_contains "$ATTEMPT_DIR/metrics.prom" "coditor_tokens_total" "core metrics expose token counter"
assert_file_not_contains "$ATTEMPT_DIR/metrics.prom" "$SESSION_ID" "metrics do not leak smoke session id"
assert_file_not_contains "$ATTEMPT_DIR/metrics.prom" "session_id=" "metrics do not use session_id labels"
write_prometheus_queries
pass "Prometheus observed smoke request, tokens, and context sample"

curl -fsS --max-time 5 "$GRAFANA_URL/api/health" >"$ATTEMPT_DIR/grafana-health.json" 2>"$ATTEMPT_DIR/grafana-health.err"
curl -fsS --max-time 5 "$GRAFANA_URL/api/search?query=Coditor" >"$ATTEMPT_DIR/grafana-search.json" 2>"$ATTEMPT_DIR/grafana-search.err"
curl -fsS --max-time 5 "$GRAFANA_URL/api/dashboards/uid/coditor-main" >"$ATTEMPT_DIR/grafana-dashboard.json" 2>"$ATTEMPT_DIR/grafana-dashboard.err"
assert_json_path "$ATTEMPT_DIR/grafana-health.json" "Grafana health reports ok database" \
    'assert data.get("database") == "ok"'
assert_json_path "$ATTEMPT_DIR/grafana-search.json" "Grafana search finds coditor-main" \
    'assert any(item.get("uid") == "coditor-main" for item in data)'
assert_json_path "$ATTEMPT_DIR/grafana-dashboard.json" "Grafana dashboard uid is coditor-main" \
    'assert data.get("dashboard", {}).get("uid") == "coditor-main"'

capture_logs
compose ps >"$ATTEMPT_DIR/compose-ps-after.txt" 2>&1
assert_file_contains "$ATTEMPT_DIR/fake-openai.log" 'POST /v1/responses HTTP/1.1" 200' "fake-openai received Responses POST"
assert_file_contains "$ATTEMPT_DIR/envoy.log" '"/v1/responses"' "Envoy logged Responses route"
assert_file_contains "$ATTEMPT_DIR/envoy.log" '"upstream":"' "Envoy logged upstream host"
assert_file_contains "$ATTEMPT_DIR/coditor-core.log" "Codex response finalized" "coditor-core finalized Codex response"

if rg -n "api\\.openai\\.com|chatgpt\\.com|backend-api" "$ATTEMPT_DIR" >/dev/null; then
    rg -n "api\\.openai\\.com|chatgpt\\.com|backend-api" "$ATTEMPT_DIR" >&2 || true
    fail "final smoke artifacts referenced a real OpenAI/ChatGPT endpoint"
fi
pass "Final smoke artifacts contain no api.openai.com or chatgpt.com references"

write_summary

echo ""
echo "=== Codex-through-fake-proxy smoke checks passed ==="
info "Artifacts: $REPORT_DIR"
SMOKE_COMPLETED=1
cleanup_stack_on_success
