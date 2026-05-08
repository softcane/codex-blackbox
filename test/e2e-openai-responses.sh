#!/usr/bin/env bash
# Phase 5A fake OpenAI Responses E2E test.
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

RUN_ID="${CODEX_BLACKBOX_OPENAI_E2E_RUN_ID:-openai-e2e-$(date +%s)-$$}"
SESSION_ID="phase-5a-${RUN_ID}"
REQUEST_ID="req-${RUN_ID}"
CORE_URL="http://localhost:9091"
ENVOY_URL="http://localhost:10000"
COMPOSE_FILES=(-f docker-compose.yml -f test/docker-compose.openai-responses.yml)
E2E_COMPLETED=0

compose() {
    docker compose "${COMPOSE_FILES[@]}" "$@"
}

cleanup_e2e_stack_on_failure() {
    if [ "$E2E_COMPLETED" = "1" ] || [ "${CODEX_BLACKBOX_OPENAI_E2E_KEEP_STACK:-0}" = "1" ]; then
        return
    fi
    compose down --remove-orphans -t 5 >/dev/null 2>&1 || true
}

cleanup_e2e_stack_on_success() {
    if [ "${CODEX_BLACKBOX_OPENAI_E2E_KEEP_STACK:-0}" = "1" ]; then
        info "Leaving fake OpenAI Responses E2E stack running because CODEX_BLACKBOX_OPENAI_E2E_KEEP_STACK=1"
        return
    fi
    compose down --remove-orphans -t 5 >/dev/null
    pass "Fake OpenAI Responses E2E stack stopped"
}

trap cleanup_e2e_stack_on_failure EXIT

wait_for_core() {
    for _ in $(seq 1 60); do
        if [ "$(curl -fsS "$CORE_URL/health" 2>/dev/null || true)" = "ok" ]; then
            return 0
        fi
        sleep 2
    done
    compose ps
    fail "codex-blackbox-core did not become healthy"
}

wait_for_envoy() {
    for _ in $(seq 1 60); do
        if curl -sS --max-time 2 "$ENVOY_URL/health" >/dev/null 2>&1; then
            return 0
        fi
        sleep 2
    done
    compose ps
    fail "envoy did not open localhost:10000"
}

assert_contains() {
    local haystack=$1
    local needle=$2
    local label=$3
    grep -q "$needle" <<<"$haystack" && pass "$label" || fail "$label missing '$needle'"
}

assert_not_contains() {
    local haystack=$1
    local needle=$2
    local label=$3
    if grep -q "$needle" <<<"$haystack"; then
        fail "$label unexpectedly contained '$needle'"
    fi
    pass "$label"
}

fetch_watch_for_session() {
    local output
    for _ in $(seq 1 10); do
        output=$(curl -sS --max-time 4 -H "Accept: text/event-stream" \
            "$CORE_URL/watch?session=$SESSION_ID" 2>/dev/null || true)
        if grep -q '"type":"session_start"' <<<"$output" \
            && grep -q '"type":"codex_turn_summary"' <<<"$output" \
            && grep -q '"type":"context_status"' <<<"$output"; then
            printf "%s" "$output"
            return 0
        fi
        sleep 1
    done
    printf "%s" "$output"
    return 1
}

echo "=== Codex Blackbox fake OpenAI Responses E2E Test ==="
info "run_id=$RUN_ID"
info "session_id=$SESSION_ID"
info "Starting Docker Compose with fake OpenAI Responses upstream..."
compose down --remove-orphans -t 5 2>/dev/null || true
compose up -d --build codex-blackbox-core envoy fake-openai

info "Waiting for codex-blackbox-core and envoy..."
wait_for_core
wait_for_envoy
pass "Core and Envoy are reachable"

info "Sending fixture Responses request through Envoy..."
response=$(curl -fsS --max-time 30 --no-buffer -N \
    -H "authorization: Bearer fake-openai-e2e" \
    -H "content-type: application/json" \
    -H "accept-encoding: gzip" \
    -H "session-id: $SESSION_ID" \
    -H "x-client-request-id: $REQUEST_ID" \
    --data-binary @test/fixtures/openai_responses_minimal_text_request.json \
    "$ENVOY_URL/v1/responses")

assert_contains "$response" "event: response.created" "Stream contains response.created"
assert_contains "$response" "response.output_text.delta" "Stream contains text delta"
assert_contains "$response" "response.completed" "Stream contains response.completed"
assert_contains "$response" "Workspace packages: codex-blackbox-core and codex-blackbox-cli." "Stream contains fixture text"
pass "Envoy streamed fake OpenAI Responses SSE"

info "Checking Codex Blackbox watch events for Codex finalization..."
watch_output=$(fetch_watch_for_session) || fail "/watch did not expose Codex SessionStart, turn summary, and ContextStatus for $SESSION_ID; output: $watch_output"
assert_contains "$watch_output" '"type":"session_start"' "/watch exposes Codex SessionStart"
assert_contains "$watch_output" '"type":"codex_turn_summary"' "/watch exposes Codex turn summary"
assert_contains "$watch_output" '"type":"context_status"' "/watch exposes Codex ContextStatus"
assert_contains "$watch_output" '"cached_input_tokens"' "/watch exposes Codex cached input accounting"
assert_contains "$watch_output" '"reasoning_output_tokens"' "/watch exposes Codex reasoning output accounting"
assert_contains "$watch_output" "$SESSION_ID" "/watch is scoped to the fixture session"
assert_not_contains "$watch_output" '"type":"cache_event"' "/watch does not emit cache-event telemetry for Codex"
assert_not_contains "$watch_output" "cache_expires_at_epoch" "/watch has no cache TTL for Codex"
assert_not_contains "$watch_output" "estimated_rebuild_cost_dollars" "/watch has no rebuild estimate for Codex"

metrics=$(curl -fsS "$CORE_URL/metrics")
assert_contains "$metrics" "codex_blackbox_requests_total" "Core metrics endpoint is live"

postmortem=$(curl -fsS "$CORE_URL/api/postmortem/$SESSION_ID")
assert_contains "$postmortem" '"report_type": "codex_responses_postmortem"' "Postmortem API returns Codex Responses report"
assert_contains "$postmortem" '"redacted": true' "Postmortem API defaults to redacted output"
assert_contains "$postmortem" '"completed": 1' "Postmortem API reports completed Responses status"
assert_contains "$postmortem" '"cached_input_tokens"' "Postmortem API reports cached input accounting"
assert_not_contains "$postmortem" "tool_result" "Postmortem API has no tool-result surface"
assert_not_contains "$postmortem" "MCP lifecycle" "Postmortem API has no MCP lifecycle surface"
assert_not_contains "$postmortem" "cache TTL" "Postmortem API has no cache TTL surface"
assert_not_contains "$postmortem" "quota" "Postmortem API has no quota surface"

echo ""
echo "=== Fake OpenAI Responses E2E checks passed ==="
E2E_COMPLETED=1
cleanup_e2e_stack_on_success
