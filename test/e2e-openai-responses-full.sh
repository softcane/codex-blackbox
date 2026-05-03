#!/usr/bin/env bash
# Phase 9A broader fake OpenAI Responses regression.
#
# This drives only local Docker Compose services and test/fake-openai.py. It
# does not require OpenAI credentials, does not launch Codex, and must not
# contact real OpenAI.
set -euo pipefail

cd "$(dirname "$0")/.."

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

RUN_ID="${CODITOR_FULL_E2E_RUN_ID:-openai-full-e2e-$(date +%s)-$$}"
REPORT_DIR="${CODITOR_FULL_E2E_REPORT_DIR:-${TMPDIR:-/tmp}/coditor-phase9a-${RUN_ID}}"
REQUEST_DIR="$REPORT_DIR/requests"
RESPONSE_DIR="$REPORT_DIR/responses"
LOG_DIR="$REPORT_DIR/logs"
REQUEST_MANIFEST="$REPORT_DIR/requests.tsv"
DB_COPY_DIR="$REPORT_DIR/db"
DB_SNAPSHOT="$DB_COPY_DIR/coditor.db"

CORE_URL="http://localhost:9091"
ENVOY_URL="http://localhost:10000"
PROMETHEUS_URL="http://localhost:9092"
GRAFANA_URL="http://localhost:3000"
COMPOSE_FILES=(-f docker-compose.yml -f test/docker-compose.openai-responses.yml)
FULL_E2E_COMPLETED=0

mkdir -p "$REQUEST_DIR" "$RESPONSE_DIR" "$LOG_DIR" "$DB_COPY_DIR"

pass() { printf "%bPASS%b: %s\n" "$GREEN" "$NC" "$1"; }
info() { printf "%bINFO%b: %s\n" "$YELLOW" "$NC" "$1"; }

capture_logs() {
    docker compose "${COMPOSE_FILES[@]}" ps >"$LOG_DIR/compose-ps.txt" 2>&1 || true
    docker compose "${COMPOSE_FILES[@]}" logs --no-color >"$LOG_DIR/compose.log" 2>&1 || true
}

fail() {
    printf "%bFAIL%b: %s\n" "$RED" "$NC" "$1" >&2
    info "Artifacts and logs: $REPORT_DIR" >&2
    capture_logs
    exit 1
}

require_cmd() {
    command -v "$1" >/dev/null 2>&1 || fail "Missing required command: $1"
}

for cmd in docker curl python3 cargo; do
    require_cmd "$cmd"
done

compose() {
    docker compose "${COMPOSE_FILES[@]}" "$@"
}

cleanup_stack_on_failure() {
    if [ "$FULL_E2E_COMPLETED" = "1" ] || [ "${CODITOR_FULL_E2E_KEEP_STACK:-0}" = "1" ]; then
        return
    fi
    compose down --remove-orphans -t 5 >/dev/null 2>&1 || true
}

cleanup_stack_on_success() {
    if [ "${CODITOR_FULL_E2E_KEEP_STACK:-0}" = "1" ]; then
        info "Leaving full fake E2E stack running because CODITOR_FULL_E2E_KEEP_STACK=1"
        return
    fi
    compose down --remove-orphans -t 5 >/dev/null
    pass "Full fake OpenAI Responses E2E stack stopped"
}

trap cleanup_stack_on_failure EXIT

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

assert_contains() {
    local haystack=$1
    local needle=$2
    local label=$3
    grep -q "$needle" <<<"$haystack" && pass "$label" || fail "$label missing '$needle'"
}

assert_file_contains() {
    local file=$1
    local needle=$2
    local label=$3
    grep -q "$needle" "$file" && pass "$label" || fail "$label missing '$needle' in $file"
}

generate_requests() {
    RUN_ID="$RUN_ID" REQUEST_DIR="$REQUEST_DIR" REQUEST_MANIFEST="$REQUEST_MANIFEST" python3 - <<'PY'
import json
import os
from pathlib import Path

run_id = os.environ["RUN_ID"]
request_dir = Path(os.environ["REQUEST_DIR"])
manifest = Path(os.environ["REQUEST_MANIFEST"])
repo = str(Path.cwd())
other_repo = "/tmp/coditor-phase9a-other-repo"

cases = [
    {
        "case": "same-a",
        "fixture": "completed",
        "cwd": repo,
        "prompt": f"Phase 9A {run_id}: inspect workspace Cargo metadata for same repo alpha.",
        "split": "0",
    },
    {
        "case": "same-b-split",
        "fixture": "completed",
        "cwd": repo,
        "prompt": f"Phase 9A {run_id}: summarize test scripts for same repo beta.",
        "split": "1",
    },
    {
        "case": "other-repo",
        "fixture": "completed",
        "cwd": other_repo,
        "prompt": f"Phase 9A {run_id}: inspect a different repo placeholder gamma.",
        "split": "0",
    },
    {
        "case": "failed",
        "fixture": "failed",
        "cwd": repo,
        "prompt": f"Phase 9A {run_id}: trigger failed Responses fixture delta.",
        "split": "0",
    },
    {
        "case": "incomplete",
        "fixture": "incomplete",
        "cwd": other_repo,
        "prompt": f"Phase 9A {run_id}: trigger incomplete Responses fixture epsilon.",
        "split": "0",
    },
]

with manifest.open("w", encoding="utf-8") as out:
    for item in cases:
        session_id = f"phase-9a-{run_id}-{item['case']}"
        request_id = f"req-{run_id}-{item['case']}"
        metadata_fixture = item["fixture"] if item["fixture"] in {"failed", "incomplete"} else "minimal_text"
        body = {
            "model": "gpt-codex-fixture",
            "instructions": "You are Codex running inside the local Phase 9A fake e2e contract.",
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [
                        {
                            "type": "input_text",
                            "text": item["prompt"],
                        }
                    ],
                }
            ],
            "reasoning": {"effort": "medium", "summary": "auto"},
            "prompt_cache_key": f"coditor-phase9a:{item['cwd']}",
            "metadata": {
                "cwd": item["cwd"],
                "coditor_fixture": metadata_fixture,
                "phase": "9A",
                "case": item["case"],
                "run_id": run_id,
            },
            "client_metadata": {
                "session_id": session_id,
                "x_client_request_id": request_id,
                "cwd": item["cwd"],
            },
            "stream": True,
        }
        path = request_dir / f"{item['case']}.json"
        path.write_text(json.dumps(body, indent=2), encoding="utf-8")
        out.write(
            "\t".join(
                [
                    item["case"],
                    session_id,
                    request_id,
                    item["fixture"],
                    item["cwd"],
                    item["prompt"],
                    str(path),
                    item["split"],
                ]
            )
            + "\n"
        )
PY
}

send_case() {
    local case_name=$1
    local session_id=$2
    local request_id=$3
    local fixture=$4
    local request_path=$5
    local split=$6
    local output_path="$RESPONSE_DIR/${case_name}.sse"
    local error_path="$RESPONSE_DIR/${case_name}.err"

    local http_code
    if [ "$split" = "1" ]; then
        if ! http_code=$(curl -sS --max-time 45 --no-buffer -N \
            -w "%{http_code}" \
            -o "$output_path" \
            -H "authorization: Bearer fake-openai-phase9a" \
            -H "content-type: application/json" \
            -H "accept-encoding: gzip" \
            -H "session-id: $session_id" \
            -H "x-client-request-id: $request_id" \
            -H "x-coditor-fixture: $fixture" \
            -H "x-coditor-split-sse: 1" \
            --data-binary @"$request_path" \
            "$ENVOY_URL/v1/responses" 2>"$error_path"); then
            printf "curl failed for %s; stderr in %s\n" "$case_name" "$error_path" >&2
            return 1
        fi
    else
        if ! http_code=$(curl -sS --max-time 45 --no-buffer -N \
            -w "%{http_code}" \
            -o "$output_path" \
            -H "authorization: Bearer fake-openai-phase9a" \
            -H "content-type: application/json" \
            -H "accept-encoding: gzip" \
            -H "session-id: $session_id" \
            -H "x-client-request-id: $request_id" \
            -H "x-coditor-fixture: $fixture" \
            --data-binary @"$request_path" \
            "$ENVOY_URL/v1/responses" 2>"$error_path"); then
            printf "curl failed for %s; stderr in %s\n" "$case_name" "$error_path" >&2
            return 1
        fi
    fi

    if [ "$http_code" != "200" ]; then
        printf "unexpected HTTP %s for %s; response in %s\n" "$http_code" "$case_name" "$output_path" >&2
        return 1
    fi

    case "$fixture" in
        completed)
            grep -q "response.completed" "$output_path" || return 1
            grep -q "Workspace packages: coditor-core and coditor-cli." "$output_path" || return 1
            ;;
        failed)
            grep -q "response.failed" "$output_path" || return 1
            grep -q "Fixture failure for Coditor contract tests." "$output_path" || return 1
            ;;
        incomplete)
            grep -q "response.incomplete" "$output_path" || return 1
            grep -q "Partial fixture output before max tokens." "$output_path" || return 1
            ;;
        *)
            printf "unknown fixture %s for %s\n" "$fixture" "$case_name" >&2
            return 1
            ;;
    esac
}

send_parallel_requests() {
    local pids=""
    while IFS=$'\t' read -r case_name session_id request_id fixture cwd prompt request_path split; do
        info "Starting parallel fixture request case=$case_name session=$session_id cwd=$cwd"
        send_case "$case_name" "$session_id" "$request_id" "$fixture" "$request_path" "$split" &
        pids="$pids $!"
    done <"$REQUEST_MANIFEST"

    local failed=0
    local pid
    for pid in $pids; do
        if ! wait "$pid"; then
            failed=1
        fi
    done
    [ "$failed" = "0" ] || fail "One or more parallel fake Responses requests failed; see $RESPONSE_DIR"
    pass "Parallel fake Responses requests completed"
}

expected_status_for_fixture() {
    case "$1" in
        failed) printf "failed" ;;
        incomplete) printf "incomplete" ;;
        *) printf "completed" ;;
    esac
}

expected_display_for_cwd() {
    local cwd=$1
    local base=${cwd##*/}
    if [ -n "$base" ]; then
        printf "%s" "$base"
    else
        printf "gpt-codex-fixture"
    fi
}

fetch_watch_for_session() {
    local session_id=$1
    local status=$2
    local display_name=$3
    local output_path="$RESPONSE_DIR/watch-${session_id}.sse"

    for _ in $(seq 1 15); do
        curl -sS --max-time 4 -H "Accept: text/event-stream" \
            "$CORE_URL/watch?session=$session_id" >"$output_path" 2>/dev/null || true
        if grep -q '"type":"session_start"' "$output_path" \
            && grep -q '"type":"codex_turn_summary"' "$output_path" \
            && grep -q '"type":"context_status"' "$output_path" \
            && grep -q "\"status\":\"$status\"" "$output_path" \
            && grep -q "\"display_name\":\"$display_name\"" "$output_path"; then
            return 0
        fi
        sleep 1
    done
    return 1
}

assert_watch_replay() {
    while IFS=$'\t' read -r case_name session_id request_id fixture cwd prompt request_path split; do
        local status
        local display_name
        status=$(expected_status_for_fixture "$fixture")
        display_name=$(expected_display_for_cwd "$cwd")
        fetch_watch_for_session "$session_id" "$status" "$display_name" \
            || fail "/watch replay missing expected events for $case_name ($session_id); see $RESPONSE_DIR/watch-${session_id}.sse"
        assert_file_contains "$RESPONSE_DIR/watch-${session_id}.sse" "$prompt" "watch replay includes initial prompt for $case_name"
    done <"$REQUEST_MANIFEST"
    pass "Late /watch subscribers replay Codex SessionStart, turn summary, and ContextStatus"
}

copy_db_snapshot() {
    local container_id
    container_id=$(compose ps -q coditor-core)
    [ -n "$container_id" ] || fail "coditor-core container id unavailable for DB snapshot"
    rm -rf "$DB_COPY_DIR"
    mkdir -p "$DB_COPY_DIR"
    docker cp "$container_id:/data/." "$DB_COPY_DIR/" >/dev/null
}

wait_for_db_rows() {
    for _ in $(seq 1 30); do
        copy_db_snapshot || true
        DB_SNAPSHOT="$DB_SNAPSHOT" REQUEST_MANIFEST="$REQUEST_MANIFEST" python3 - <<'PY' >/dev/null 2>&1 && return 0
import os
import sqlite3

db = os.environ["DB_SNAPSHOT"]
manifest = os.environ["REQUEST_MANIFEST"]
ids = [line.split("\t")[1] for line in open(manifest, encoding="utf-8") if line.strip()]
conn = sqlite3.connect(db)
placeholders = ",".join("?" for _ in ids)
request_count = conn.execute(
    f"SELECT COUNT(*) FROM requests WHERE session_id IN ({placeholders})", ids
).fetchone()[0]
turn_count = conn.execute(
    f"SELECT COUNT(*) FROM turn_snapshots WHERE session_id IN ({placeholders})", ids
).fetchone()[0]
if request_count >= len(ids) and turn_count >= len(ids):
    raise SystemExit(0)
raise SystemExit(1)
PY
        sleep 1
    done
    fail "SQLite did not contain all Phase 9A Codex requests after waiting"
}

assert_sqlite_persistence() {
    wait_for_db_rows
    DB_SNAPSHOT="$DB_SNAPSHOT" REQUEST_MANIFEST="$REQUEST_MANIFEST" python3 - <<'PY'
import json
import os
import sqlite3
import sys

db = os.environ["DB_SNAPSHOT"]
manifest = os.environ["REQUEST_MANIFEST"]
cases = []
for line in open(manifest, encoding="utf-8"):
    if not line.strip():
        continue
    case, session_id, request_id, fixture, cwd, prompt, request_path, split = line.rstrip("\n").split("\t")
    expected_status = {"failed": "failed", "incomplete": "incomplete"}.get(fixture, "completed")
    cases.append(
        {
            "case": case,
            "session_id": session_id,
            "request_id": request_id,
            "fixture": fixture,
            "cwd": cwd,
            "prompt": prompt,
            "status": expected_status,
        }
    )

conn = sqlite3.connect(db)
conn.row_factory = sqlite3.Row
ids = [case["session_id"] for case in cases]
placeholders = ",".join("?" for _ in ids)

def fail(message):
    raise SystemExit(message)

requests = {
    row["session_id"]: row
    for row in conn.execute(
        f"""
        SELECT session_id, request_id, provider, requested_model, served_model, codex_status,
               codex_input_tokens, codex_cached_input_tokens, codex_uncached_input_tokens,
               codex_output_tokens, codex_reasoning_output_tokens, codex_total_tokens,
               codex_prompt_excerpt
        FROM requests
        WHERE session_id IN ({placeholders})
        """,
        ids,
    )
}
turns = {
    row["session_id"]: row
    for row in conn.execute(
        f"""
        SELECT session_id, provider, codex_status, codex_input_tokens,
               codex_cached_input_tokens, codex_uncached_input_tokens,
               codex_output_tokens, codex_total_tokens
        FROM turn_snapshots
        WHERE session_id IN ({placeholders})
        """,
        ids,
    )
}
sessions = {
    row["session_id"]: row
    for row in conn.execute(
        f"SELECT session_id, model, initial_prompt, request_count FROM sessions WHERE session_id IN ({placeholders})",
        ids,
    )
}

if set(requests) != set(ids):
    fail(f"requests table missing sessions: {sorted(set(ids) - set(requests))}")
if set(turns) != set(ids):
    fail(f"turn_snapshots table missing sessions: {sorted(set(ids) - set(turns))}")
if set(sessions) != set(ids):
    fail(f"sessions table missing sessions: {sorted(set(ids) - set(sessions))}")

prompts = set()
for case in cases:
    req = requests[case["session_id"]]
    turn = turns[case["session_id"]]
    session = sessions[case["session_id"]]
    if req["provider"] != "codex_responses" or turn["provider"] != "codex_responses":
        fail(f"{case['case']} did not persist provider=codex_responses")
    if req["codex_status"] != case["status"] or turn["codex_status"] != case["status"]:
        fail(f"{case['case']} status mismatch: request={req['codex_status']} turn={turn['codex_status']}")
    if case["prompt"] not in (req["codex_prompt_excerpt"] or ""):
        fail(f"{case['case']} request prompt excerpt did not include distinct first prompt")
    if case["prompt"] not in (session["initial_prompt"] or ""):
        fail(f"{case['case']} session initial prompt did not include distinct first prompt")
    prompts.add(req["codex_prompt_excerpt"])
    if session["request_count"] < 1:
        fail(f"{case['case']} session request_count was not incremented")
    for row_name, row in [("request", req), ("turn", turn)]:
        input_tokens = row["codex_input_tokens"] or 0
        cached = row["codex_cached_input_tokens"] or 0
        uncached = row["codex_uncached_input_tokens"] or 0
        output = row["codex_output_tokens"] or 0
        total = row["codex_total_tokens"] or 0
        if input_tokens > 0 and input_tokens != cached + uncached:
            fail(f"{case['case']} {row_name} double-counted cached input: input={input_tokens} cached={cached} uncached={uncached}")
        if total > 0 and total != input_tokens + output:
            fail(f"{case['case']} {row_name} total tokens mismatch: total={total} input={input_tokens} output={output}")

if len(prompts) != len(cases):
    fail("persisted prompt excerpts were not distinct across Phase 9A requests")

status_counts = dict(
    conn.execute(
        f"SELECT codex_status, COUNT(*) FROM requests WHERE session_id IN ({placeholders}) GROUP BY codex_status",
        ids,
    ).fetchall()
)
if status_counts.get("failed", 0) < 1 or status_counts.get("incomplete", 0) < 1:
    fail(f"failed/incomplete statuses missing from SQLite: {status_counts}")

print(json.dumps({"checked_sessions": ids, "status_counts": status_counts}, indent=2))
PY
    pass "SQLite persisted parallel Codex turns, failed/incomplete statuses, and non-double-counted tokens"
}

assert_diagnosis_api() {
    local failed_session
    local incomplete_session
    failed_session=$(awk -F '\t' '$1 == "failed" {print $2}' "$REQUEST_MANIFEST")
    incomplete_session=$(awk -F '\t' '$1 == "incomplete" {print $2}' "$REQUEST_MANIFEST")
    curl -fsS "$CORE_URL/api/diagnosis/$failed_session" >"$RESPONSE_DIR/diagnosis-failed.json"
    curl -fsS "$CORE_URL/api/diagnosis/$incomplete_session" >"$RESPONSE_DIR/diagnosis-incomplete.json"
    assert_file_contains "$RESPONSE_DIR/diagnosis-failed.json" "codex_response_failed" "diagnosis API reports failed response"
    assert_file_contains "$RESPONSE_DIR/diagnosis-incomplete.json" "codex_response_incomplete" "diagnosis API reports incomplete response"
}

assert_prometheus_and_grafana() {
    PROMETHEUS_URL="$PROMETHEUS_URL" GRAFANA_URL="$GRAFANA_URL" REQUEST_MANIFEST="$REQUEST_MANIFEST" python3 - <<'PY'
import json
import os
import re
import time
import urllib.parse
import urllib.request

prom_url = os.environ["PROMETHEUS_URL"]
grafana_url = os.environ["GRAFANA_URL"]
manifest = os.environ["REQUEST_MANIFEST"]
session_ids = [line.split("\t")[1] for line in open(manifest, encoding="utf-8") if line.strip()]
expected_requests = len(session_ids)

def fail(message):
    raise SystemExit(message)

def get_json(base_url, path, params=None):
    query = "" if params is None else "?" + urllib.parse.urlencode(params, doseq=True)
    with urllib.request.urlopen(base_url + path + query, timeout=8) as response:
        return json.loads(response.read().decode("utf-8"))

def prom_query(expr):
    payload = get_json(prom_url, "/api/v1/query", {"query": expr})
    if payload.get("status") != "success":
        fail(f"Prometheus query failed for {expr}: {payload}")
    return payload.get("data", {}).get("result", [])

def prom_value(expr):
    result = prom_query(expr)
    if not result:
        return None
    return float(result[0]["value"][1])

def wait_until(label, predicate, timeout=90):
    deadline = time.time() + timeout
    last_error = None
    while time.time() < deadline:
        try:
            if predicate():
                return
        except Exception as exc:
            last_error = exc
        time.sleep(2)
    if last_error:
        fail(f"{label} did not become true: {last_error}")
    fail(f"{label} did not become true before timeout")

wait_until("Prometheus scrape for coditor-core", lambda: prom_value('up{job="coditor-core"}') == 1.0)
wait_until(
    "Prometheus observed all Phase 9A requests",
    lambda: (prom_value("sum(coditor_requests_total)") or 0.0) >= expected_requests,
)
wait_until(
    "Prometheus observed context fill samples",
    lambda: (prom_value('sum(coditor_context_fill_percent_count{provider="codex_responses"})') or 0.0) >= expected_requests,
)
wait_until(
    "Prometheus observed input tokens",
    lambda: (prom_value('sum(coditor_tokens_total{kind="input"})') or 0.0) > 0,
)
wait_until(
    "Prometheus observed output tokens",
    lambda: (prom_value('sum(coditor_tokens_total{kind="output"})') or 0.0) > 0,
)

required_metrics = [
    "coditor_requests_total",
    "coditor_tokens_total",
    "coditor_turn_duration_seconds_count",
    "coditor_context_fill_percent_count",
    'coditor_sessions_degraded_total{cause_type="codex_response_failed"}',
    'coditor_codex_response_status_total{status="failed"}',
    'coditor_codex_response_status_total{status="incomplete"}',
]
for expr in required_metrics:
    wait_until(f"metric {expr}", lambda expr=expr: len(prom_query(expr)) > 0)

now = int(time.time())
series = get_json(
    prom_url,
    "/api/v1/series",
    {
        "match[]": '{__name__=~"coditor_.*"}',
        "start": str(now - 3600),
        "end": str(now),
    },
)
if series.get("status") != "success":
    fail(f"Prometheus series query failed: {series}")
for item in series.get("data", []):
    for key, value in item.items():
        if key == "__name__":
            continue
        key_lower = key.lower()
        if "session_id" in key_lower or key_lower in {"session", "proxy_session"}:
            fail(f"Prometheus metric label uses a session-like key: {item}")
        for session_id in session_ids:
            if session_id in str(value):
                fail(f"Prometheus metric label value leaked session id: {item}")

health = get_json(grafana_url, "/api/health")
if health.get("database") != "ok":
    fail(f"Grafana health failed: {health}")
search = get_json(grafana_url, "/api/search", {"query": "Coditor"})
if not any(item.get("uid") == "coditor-main" for item in search):
    fail(f"Grafana did not provision coditor-main dashboard: {search}")
dashboard = get_json(grafana_url, "/api/dashboards/uid/coditor-main").get("dashboard", {})
if dashboard.get("uid") != "coditor-main":
    fail("Grafana dashboard uid mismatch")
panel_titles = {panel.get("title") for panel in dashboard.get("panels", [])}
required_panel_titles = {
    "Codex Responses requests since start",
    "Codex Responses tokens by kind",
    "Codex context fill p95 (5m)",
    "Codex failed responses since start",
    "Codex incomplete responses since start",
    "Codex diagnosis cause labels",
}
for title in required_panel_titles:
    if title not in panel_titles:
        fail(f"Grafana dashboard missing panel {title!r}")

metric_names = {
    item["metric"]["__name__"]
    for item in prom_query('{__name__=~"coditor_.*"}')
    if item.get("metric", {}).get("__name__")
}
for panel in dashboard.get("panels", []):
    if panel.get("title") not in required_panel_titles:
        continue
    for target in panel.get("targets", []) or []:
        expr = target.get("expr", "")
        for metric_name in sorted(set(re.findall(r"\bcoditor_[A-Za-z_:][A-Za-z0-9_:]*", expr))):
            if metric_name not in metric_names:
                fail(f"Grafana panel {panel.get('title')!r} references missing metric {metric_name}")

for panel in dashboard.get("panels", []):
    title = panel.get("title") or ""
    if "Codex" not in title:
        continue
    panel_text = json.dumps(panel).lower()
    for copied_model_term in (
        "cl" + "aude",
        "op" + "us",
        "son" + "net",
        "hai" + "ku",
        "legacy_" + "cl" + "aude",
    ):
        if copied_model_term in panel_text:
            fail(f"Grafana Codex panel {title!r} references legacy model term {copied_model_term!r}")

print("Prometheus and Grafana Phase 9A assertions passed")
PY
    pass "Prometheus and Grafana expose Phase 9A fake-session observability"
}

assert_cli_dry_run() {
    local output_path="$REPORT_DIR/coditor-run-dry-run.txt"
    cargo run -q -p coditor-cli -- run --dry-run -- codex exec "Phase 9A fake smoke" >"$output_path"
    assert_file_contains "$output_path" "Mode: experimental Codex ChatGPT subscription proxy" "CLI dry-run uses subscription proxy mode"
    assert_file_contains "$output_path" "Config files: not modified" "CLI dry-run is read-only"
    assert_file_contains "$output_path" "chatgpt_base_url" "CLI dry-run prints ChatGPT backend proxy override"
    assert_file_contains "$output_path" "model_provider=\"coditor-chatgpt\"" "CLI dry-run uses the Coditor ChatGPT subscription provider"
    assert_file_contains "$output_path" "model_providers.coditor-chatgpt.base_url=\"http://127.0.0.1:10000/backend-api/codex\"" "CLI dry-run routes model turns through Coditor"
    assert_file_contains "$output_path" "model_providers.coditor-chatgpt.requires_openai_auth=true" "CLI dry-run preserves ChatGPT auth"
    assert_file_contains "$output_path" "model_providers.coditor-chatgpt.supports_websockets=false" "CLI dry-run forces HTTP Responses through Envoy"
    assert_file_contains "$output_path" "OPENAI_API_KEY is not used" "CLI dry-run labels subscription auth"
    assert_file_contains "$output_path" "Environment removals:" "CLI dry-run prints inherited Codex env removals"
    assert_file_contains "$output_path" "CODEX_THREAD_ID" "CLI dry-run removes parent Codex thread env"
    assert_file_contains "$output_path" "Child stdin: closed for Codex exec" "CLI dry-run closes Codex stdin for harness safety"
    assert_file_contains "$output_path" "features.enable_request_compression=false" "CLI dry-run disables request compression"
    assert_file_contains "$output_path" "Post-run check: require Coditor to observe at least one Codex Responses request" "CLI dry-run labels observation gate"
    if grep -q "forced_login_method\\|env_key\\|openai_base_url\\|coditor-openai\\|coditor-openai-responses" "$output_path"; then
        fail "CLI dry-run should not print API-key or stale custom/fake-provider overrides"
    fi
    assert_file_contains "$output_path" "exec -c" "CLI dry-run attaches subscription overrides to Codex exec"
    assert_file_contains "$output_path" "'Phase 9A fake smoke'" "CLI dry-run preserves Codex prompt argument"
    if grep -q -- "--json" "$output_path"; then
        fail "CLI dry-run must not pass Codex local JSON stdout mode"
    fi
}

assert_failure_open_after_core_stop() {
    local request_path="$REQUEST_DIR/failure-open.json"
    local response_path="$RESPONSE_DIR/failure-open.sse"
    RUN_ID="$RUN_ID" REQUEST_PATH="$request_path" python3 - <<'PY'
import json
import os

run_id = os.environ["RUN_ID"]
path = os.environ["REQUEST_PATH"]
body = {
    "model": "gpt-codex-fixture",
    "instructions": "Local failure-open fixture after coditor-core is stopped.",
    "input": f"Phase 9A {run_id}: verify Envoy failure-open after core stop.",
    "metadata": {
        "cwd": "/tmp/coditor-failure-open",
        "coditor_fixture": "minimal_text",
        "phase": "9A",
        "case": "failure-open",
    },
    "client_metadata": {
        "session_id": f"phase-9a-{run_id}-failure-open",
        "x_client_request_id": f"req-{run_id}-failure-open",
    },
    "stream": True,
}
with open(path, "w", encoding="utf-8") as handle:
    json.dump(body, handle, indent=2)
PY

    info "Stopping coditor-core to verify Envoy failure-open behavior"
    compose stop coditor-core >/dev/null
    local http_code
    if ! http_code=$(curl -sS --max-time 45 --no-buffer -N \
        -w "%{http_code}" \
        -o "$response_path" \
        -H "authorization: Bearer fake-openai-phase9a" \
        -H "content-type: application/json" \
        -H "session-id: phase-9a-${RUN_ID}-failure-open" \
        -H "x-client-request-id: req-${RUN_ID}-failure-open" \
        -H "x-coditor-fixture: completed" \
        --data-binary @"$request_path" \
        "$ENVOY_URL/v1/responses" 2>"$RESPONSE_DIR/failure-open.err"); then
        fail "failure-open curl failed after coditor-core stop; see $RESPONSE_DIR/failure-open.err"
    fi
    [ "$http_code" = "200" ] || fail "failure-open expected HTTP 200, got $http_code; see $response_path"
    assert_file_contains "$response_path" "response.completed" "Envoy remains failure-open when coditor-core is stopped"
    assert_file_contains "$response_path" "Workspace packages: coditor-core and coditor-cli." "failure-open response came from fake upstream"
}

echo "=== Coditor full fake OpenAI Responses E2E Test ==="
info "run_id=$RUN_ID"
info "report_dir=$REPORT_DIR"

generate_requests
info "Starting Docker Compose with fake OpenAI, Prometheus, and Grafana..."
compose down --remove-orphans -t 5 2>/dev/null || true
compose up -d --build coditor-core envoy fake-openai prometheus grafana

info "Waiting for coditor-core, Envoy, Prometheus, and Grafana..."
wait_for_http "coditor-core" "$CORE_URL/health"
wait_for_http "envoy" "$ENVOY_URL/health"
wait_for_http "prometheus" "$PROMETHEUS_URL/-/ready"
wait_for_http "grafana" "$GRAFANA_URL/api/health"
pass "Core, Envoy, Prometheus, and Grafana are reachable"

send_parallel_requests
assert_watch_replay
assert_diagnosis_api
assert_sqlite_persistence
assert_prometheus_and_grafana
assert_cli_dry_run
assert_failure_open_after_core_stop

capture_logs
echo ""
echo "=== Full fake OpenAI Responses E2E checks passed ==="
FULL_E2E_COMPLETED=1
cleanup_stack_on_success
