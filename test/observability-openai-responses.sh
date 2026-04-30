#!/usr/bin/env bash
# Phase 8B fake OpenAI Responses observability validation.
#
# This drives only local Docker Compose services and test/fake-openai.py. It
# does not require OpenAI credentials and must not contact real OpenAI/Codex.
set -euo pipefail

cd "$(dirname "$0")/.."

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

pass() { printf "%bPASS%b: %s\n" "$GREEN" "$NC" "$1"; }
info() { printf "%bINFO%b: %s\n" "$YELLOW" "$NC" "$1"; }
fail() { printf "%bFAIL%b: %s\n" "$RED" "$NC" "$1"; exit 1; }

require_cmd() {
    command -v "$1" >/dev/null 2>&1 || fail "Missing required command: $1"
}

for cmd in docker curl python3; do
    require_cmd "$cmd"
done

RUN_ID="${CODITOR_OBSERVABILITY_RUN_ID:-observability-openai-$(date +%s)-$$}"
SESSION_ID="phase-8b-${RUN_ID}"
REQUEST_ID="req-${RUN_ID}"
CORE_URL="http://localhost:9091"
ENVOY_URL="http://localhost:10000"
PROMETHEUS_URL="http://localhost:9092"
GRAFANA_URL="http://localhost:3000"
COMPOSE_FILES=(-f docker-compose.yml -f test/docker-compose.openai-responses.yml)
OBS_COMPLETED=0

compose() {
    docker compose "${COMPOSE_FILES[@]}" "$@"
}

cleanup_stack_on_failure() {
    if [ "$OBS_COMPLETED" = "1" ] || [ "${CODITOR_OBSERVABILITY_KEEP_STACK:-0}" = "1" ]; then
        return
    fi
    compose down --remove-orphans -t 5 >/dev/null 2>&1 || true
}

cleanup_stack_on_success() {
    if [ "${CODITOR_OBSERVABILITY_KEEP_STACK:-0}" = "1" ]; then
        info "Leaving observability stack running because CODITOR_OBSERVABILITY_KEEP_STACK=1"
        return
    fi
    compose down --remove-orphans -t 5 >/dev/null
    pass "Observability stack stopped"
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

echo "=== Coditor fake OpenAI Responses Observability Test ==="
info "run_id=$RUN_ID"
info "session_id=$SESSION_ID"
info "Starting Docker Compose with fake OpenAI, Prometheus, and Grafana..."
compose down --remove-orphans -t 5 2>/dev/null || true
compose up -d --build coditor-core envoy fake-openai prometheus grafana

info "Waiting for coditor-core, Envoy, Prometheus, and Grafana..."
wait_for_http "coditor-core" "$CORE_URL/health"
wait_for_http "envoy" "$ENVOY_URL/health"
wait_for_http "prometheus" "$PROMETHEUS_URL/-/ready"
wait_for_http "grafana" "$GRAFANA_URL/api/health"
pass "Core, Envoy, Prometheus, and Grafana are reachable"

info "Sending fixture Responses request through Envoy..."
response=$(curl -fsS --max-time 30 --no-buffer -N \
    -H "authorization: Bearer fake-openai-observability" \
    -H "content-type: application/json" \
    -H "accept-encoding: gzip" \
    -H "session-id: $SESSION_ID" \
    -H "x-client-request-id: $REQUEST_ID" \
    --data-binary @test/fixtures/openai_responses_minimal_text_request.json \
    "$ENVOY_URL/v1/responses")

assert_contains "$response" "response.completed" "Envoy streamed fixture completion"

core_metrics=$(curl -fsS "$CORE_URL/metrics")
assert_contains "$core_metrics" "coditor_context_fill_percent" "Core metrics expose context fill histogram"
assert_contains "$core_metrics" "coditor_sessions_degraded_total" "Core metrics expose diagnosis counters"

info "Checking Prometheus and Grafana observability contract..."
CORE_URL="$CORE_URL" \
PROMETHEUS_URL="$PROMETHEUS_URL" \
GRAFANA_URL="$GRAFANA_URL" \
SESSION_ID="$SESSION_ID" \
python3 - <<'PY'
import json
import os
import re
import time
import urllib.parse
import urllib.request
from pathlib import Path

core_url = os.environ["CORE_URL"]
prom_url = os.environ["PROMETHEUS_URL"]
grafana_url = os.environ["GRAFANA_URL"]
session_id = os.environ["SESSION_ID"]


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
    try:
        return float(result[0]["value"][1])
    except (KeyError, IndexError, ValueError):
        return None


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


wait_until(
    "Prometheus scrape for coditor-core",
    lambda: prom_value('up{job="coditor-core"}') == 1.0,
)
wait_until(
    "Prometheus observed fake Codex request",
    lambda: (prom_value("sum(coditor_requests_total)") or 0.0) >= 1.0,
)
wait_until(
    "Prometheus observed fake Codex input tokens",
    lambda: (prom_value('sum(coditor_tokens_total{kind="input"})') or 0.0) > 0.0,
)
wait_until(
    "Prometheus observed fake Codex output tokens",
    lambda: (prom_value('sum(coditor_tokens_total{kind="output"})') or 0.0) > 0.0,
)

required_metrics = {
    "request counter": "coditor_requests_total",
    "token counter": "coditor_tokens_total",
    "turn duration histogram": "coditor_turn_duration_seconds_count",
    "context fill histogram": "coditor_context_fill_percent_count",
    "context fill bucket": "coditor_context_fill_percent_bucket",
    "diagnosis counter": 'coditor_sessions_degraded_total{cause_type="codex_response_failed"}',
    "MCP lifecycle counter": "coditor_mcp_events_total",
}
for label, expr in required_metrics.items():
    wait_until(label, lambda expr=expr: len(prom_query(expr)) > 0)

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
        if session_id and session_id in str(value):
            fail(f"Prometheus metric label value leaked fixture session id: {item}")

with urllib.request.urlopen(core_url + "/metrics", timeout=8) as response:
    core_metrics = response.read().decode("utf-8")
if re.search(r"\{[^}]*session_id=", core_metrics):
    fail("core /metrics exposed a session_id label")
if session_id in core_metrics:
    fail("core /metrics leaked the fixture session id")

health = get_json(grafana_url, "/api/health")
if health.get("database") != "ok":
    fail(f"Grafana health did not report an ok database: {health}")

wait_until(
    "Grafana dashboard provisioning",
    lambda: any(
        item.get("uid") == "coditor-main"
        for item in get_json(grafana_url, "/api/search", {"query": "Coditor"})
    ),
)
dashboard_payload = get_json(grafana_url, "/api/dashboards/uid/coditor-main")
dashboard = dashboard_payload.get("dashboard") or {}
if dashboard.get("uid") != "coditor-main":
    fail(f"Grafana did not load coditor-main dashboard: {dashboard_payload}")

local_dashboard = json.loads(Path("grafana/dashboards/coditor.json").read_text())
panels = local_dashboard.get("panels", [])
required_panel_titles = {
    "Codex/OpenAI requests since start",
    "Codex/OpenAI tokens by kind",
    "Codex context fill p95",
    "Codex diagnosis causes",
}
panels_by_title = {panel.get("title"): panel for panel in panels}
missing = sorted(required_panel_titles - set(panels_by_title))
if missing:
    fail(f"Coditor dashboard is missing Phase 8B panels: {missing}")

metric_names = {
    item["metric"]["__name__"]
    for item in prom_query('{__name__=~"coditor_.*"}')
    if item.get("metric", {}).get("__name__")
}
for title in sorted(required_panel_titles):
    panel = panels_by_title[title]
    exprs = [
        target.get("expr", "")
        for target in panel.get("targets", [])
        if target.get("expr")
    ]
    if not exprs:
        fail(f"Panel {title!r} has no Prometheus expression")
    for expr in exprs:
        refs = sorted(set(re.findall(r"\bcoditor_[A-Za-z_:][A-Za-z0-9_:]*", expr)))
        missing_refs = [name for name in refs if name not in metric_names]
        if missing_refs:
            fail(f"Panel {title!r} references missing metrics {missing_refs} in {expr!r}")

print("Prometheus and Grafana observability assertions passed")
PY
pass "Prometheus exposes bounded Codex request/token/context/diagnosis metrics"
pass "Prometheus labels do not leak session ids"
pass "Grafana provisioning loads Coditor dashboard and Phase 8B panels"

echo ""
echo "=== Fake OpenAI Responses observability checks passed ==="
OBS_COMPLETED=1
cleanup_stack_on_success
