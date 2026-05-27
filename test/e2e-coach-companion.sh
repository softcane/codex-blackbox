#!/usr/bin/env bash
# Deterministic fake Coach and Companion E2E.
#
# This seeds only local fixture evidence. It does not contact OpenAI, does not
# launch Codex, and must not be used as live Codex support proof.
set -euo pipefail

cd "$(dirname "$0")/.."

RUN_ID="${CODEX_BLACKBOX_COACH_E2E_RUN_ID:-coach-companion-$(date +%s)-$$}"
ROOT="${TMPDIR:-/tmp}/codex-blackbox-e2e/$RUN_ID"
REPORT_DIR="${CODEX_BLACKBOX_COACH_E2E_REPORT_DIR:-reports/e2e/$RUN_ID}"
COMMANDS="$REPORT_DIR/commands-run.txt"
CORE_LOG="$REPORT_DIR/core.log"
DB="$ROOT/codex-blackbox.db"
CORE_PID=""

mkdir -p "$ROOT" "$REPORT_DIR" "$REPORT_DIR/screenshots"
: >"$COMMANDS"

log_cmd() {
    printf "%s\n" "$*" >>"$COMMANDS"
}

cleanup() {
    if [ -n "$CORE_PID" ]; then
        kill "$CORE_PID" >/dev/null 2>&1 || true
        wait "$CORE_PID" >/dev/null 2>&1 || true
    fi
}
trap cleanup EXIT

free_port() {
    python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
}

HTTP_PORT="$(free_port)"
GRPC_PORT="$(free_port)"
CORE_URL="http://127.0.0.1:$HTTP_PORT"

for repo in clean-readonly validation-failure unvalidated-edit repeated-failure risky-command high-context pricing-trust; do
    mkdir -p "$ROOT/$repo"
    printf "fixture repo: %s\n" "$repo" >"$ROOT/$repo/README.md"
done
printf "%s\n" clean-readonly validation-failure unvalidated-edit repeated-failure risky-command high-context pricing-trust >"$REPORT_DIR/temp-repos.txt"

log_cmd "cargo build -p codex-blackbox-core -p codex-blackbox-cli"
cargo build -p codex-blackbox-core -p codex-blackbox-cli >/dev/null

log_cmd "CODEX_BLACKBOX_DB_PATH=$DB CODEX_BLACKBOX_HTTP_ADDR=127.0.0.1:$HTTP_PORT CODEX_BLACKBOX_GRPC_ADDR=127.0.0.1:$GRPC_PORT target/debug/codex-blackbox-core"
CODEX_BLACKBOX_DB_PATH="$DB" \
CODEX_BLACKBOX_HTTP_ADDR="127.0.0.1:$HTTP_PORT" \
CODEX_BLACKBOX_GRPC_ADDR="127.0.0.1:$GRPC_PORT" \
    target/debug/codex-blackbox-core >"$CORE_LOG" 2>&1 &
CORE_PID="$!"

for _ in $(seq 1 80); do
    if curl -fsS "$CORE_URL/health" >/dev/null 2>&1; then
        break
    fi
    sleep 0.25
done
curl -fsS "$CORE_URL/health" >/dev/null

python3 - "$DB" "$RUN_ID" <<'PY'
import json
import sqlite3
import sys

db, run_id = sys.argv[1:3]
conn = sqlite3.connect(db)
conn.executescript(
    """
CREATE TABLE IF NOT EXISTS sessions (
    session_id TEXT PRIMARY KEY,
    started_at TEXT NOT NULL,
    ended_at TEXT,
    total_input_tokens INTEGER DEFAULT 0,
    total_output_tokens INTEGER DEFAULT 0,
    total_cache_read_tokens INTEGER DEFAULT 0,
    total_cache_creation_tokens INTEGER DEFAULT 0,
    total_codex_input_tokens INTEGER DEFAULT 0,
    total_codex_cached_input_tokens INTEGER DEFAULT 0,
    total_codex_uncached_input_tokens INTEGER DEFAULT 0,
    total_codex_output_tokens INTEGER DEFAULT 0,
    total_codex_reasoning_output_tokens INTEGER DEFAULT 0,
    total_codex_tokens INTEGER DEFAULT 0,
    total_cost_dollars REAL DEFAULT 0.0,
    cache_waste_dollars REAL DEFAULT 0.0,
    request_count INTEGER DEFAULT 0,
    model TEXT,
    display_name TEXT,
    initial_prompt TEXT
);
CREATE TABLE IF NOT EXISTS turn_snapshots (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    turn_number INTEGER NOT NULL,
    timestamp TEXT NOT NULL,
    input_tokens INTEGER,
    cache_read_tokens INTEGER DEFAULT 0,
    cache_creation_tokens INTEGER DEFAULT 0,
    output_tokens INTEGER,
    ttft_ms INTEGER,
    tool_calls TEXT,
    tool_failures INTEGER DEFAULT 0,
    gap_from_prev_secs REAL,
    context_utilization REAL,
    context_window_tokens INTEGER,
    frustration_signals INTEGER DEFAULT 0,
    requested_model TEXT,
    actual_model TEXT,
    response_summary TEXT,
    request_id TEXT,
    provider TEXT,
    codex_status TEXT,
    codex_input_tokens INTEGER DEFAULT 0,
    codex_cached_input_tokens INTEGER DEFAULT 0,
    codex_uncached_input_tokens INTEGER DEFAULT 0,
    codex_output_tokens INTEGER DEFAULT 0,
    codex_reasoning_output_tokens INTEGER DEFAULT 0,
    codex_total_tokens INTEGER DEFAULT 0,
    codex_response_id TEXT,
    codex_prompt_excerpt TEXT,
    codex_failure_detail TEXT,
    codex_incomplete_detail TEXT,
    codex_tool_calls TEXT,
    codex_accounting_anomalies TEXT
);
CREATE TABLE IF NOT EXISTS requests (
    request_id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    timestamp TEXT NOT NULL,
    model TEXT NOT NULL,
    input_tokens INTEGER,
    output_tokens INTEGER,
    cache_read_tokens INTEGER DEFAULT 0,
    cache_creation_tokens INTEGER DEFAULT 0,
    cost_dollars REAL,
    cost_source TEXT,
    trusted_for_budget_enforcement INTEGER DEFAULT 0,
    duration_ms INTEGER,
    tool_calls TEXT,
    cache_event TEXT,
    provider TEXT,
    requested_model TEXT,
    served_model TEXT,
    codex_status TEXT,
    codex_input_tokens INTEGER DEFAULT 0,
    codex_cached_input_tokens INTEGER DEFAULT 0,
    codex_uncached_input_tokens INTEGER DEFAULT 0,
    codex_output_tokens INTEGER DEFAULT 0,
    codex_reasoning_output_tokens INTEGER DEFAULT 0,
    codex_total_tokens INTEGER DEFAULT 0,
    codex_response_id TEXT,
    codex_prompt_excerpt TEXT,
    codex_failure_detail TEXT,
    codex_incomplete_detail TEXT,
    codex_tool_calls TEXT,
    codex_accounting_anomalies TEXT
);
CREATE TABLE IF NOT EXISTS coach_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT,
    turn_id TEXT,
    timestamp TEXT NOT NULL,
    evidence_source TEXT NOT NULL,
    category TEXT NOT NULL,
    reason_code TEXT,
    privacy TEXT NOT NULL,
    confidence TEXT NOT NULL,
    payload_summary TEXT NOT NULL DEFAULT '{}'
);
"""
)

def session(name):
    sid = f"fake-{run_id}-{name}"
    conn.execute(
        "INSERT OR REPLACE INTO sessions (session_id, started_at, ended_at, model, display_name) VALUES (?, ?, ?, ?, ?)",
        (sid, "2026-05-27T00:00:00Z", "2026-05-27T00:00:02Z", "gpt-codex-fixture", name),
    )
    return sid

def proxy_turn(name, status="completed", context=0.2, input_tokens=1000, output_tokens=100):
    sid = session(name)
    conn.execute(
        """
INSERT OR REPLACE INTO requests (
 request_id, session_id, timestamp, model, input_tokens, output_tokens, cost_dollars, cost_source,
 trusted_for_budget_enforcement, duration_ms, tool_calls, provider, requested_model, served_model,
 codex_status, codex_input_tokens, codex_cached_input_tokens, codex_uncached_input_tokens,
 codex_output_tokens, codex_reasoning_output_tokens, codex_total_tokens, codex_tool_calls,
 codex_accounting_anomalies
) VALUES (?, ?, ?, ?, ?, ?, 0.0, 'fake_fixture', 0, 100, '[]', 'codex_responses', ?, ?, ?, ?, ?, ?, ?, 0, ?, '[]', '[]')
""",
        (f"req-{sid}", sid, "2026-05-27T00:00:01Z", "gpt-codex-fixture", input_tokens, output_tokens, "gpt-codex-fixture", "gpt-codex-fixture", status, input_tokens, 100, input_tokens - 100, output_tokens, input_tokens + output_tokens),
    )
    conn.execute(
        """
INSERT INTO turn_snapshots (
 session_id, turn_number, timestamp, input_tokens, output_tokens, ttft_ms, tool_calls,
 context_utilization, context_window_tokens, requested_model, actual_model, response_summary,
 request_id, provider, codex_status, codex_input_tokens, codex_cached_input_tokens,
 codex_uncached_input_tokens, codex_output_tokens, codex_reasoning_output_tokens,
 codex_total_tokens, codex_tool_calls, codex_accounting_anomalies
) VALUES (?, 1, ?, ?, ?, 100, '[]', ?, 100000, ?, ?, 'fixture summary', ?, 'codex_responses', ?, ?, 100, ?, ?, 0, ?, '[]', '[]')
""",
        (sid, "2026-05-27T00:00:01Z", input_tokens, output_tokens, context, "gpt-codex-fixture", "gpt-codex-fixture", f"req-{sid}", status, input_tokens, input_tokens - 100, output_tokens, input_tokens + output_tokens),
    )
    return sid

def event(sid, category, reason, source="hook", payload=None, i=0):
    conn.execute(
        "INSERT INTO coach_events (session_id, turn_id, timestamp, evidence_source, category, reason_code, privacy, confidence, payload_summary) VALUES (?, ?, ?, ?, ?, ?, 'derived_private', 'high', ?)",
        (sid, "turn_1", f"2026-05-27T00:00:{10+i:02d}Z", source, category, reason, json.dumps(payload or {})),
    )

sessions = {
    "clean-readonly": proxy_turn("clean-readonly"),
    "high-context": proxy_turn("high-context", context=0.9, input_tokens=90000, output_tokens=1000),
    "pricing-trust": proxy_turn("pricing-trust"),
}
event(sessions["pricing-trust"], "pricing_trust_observed", "untrusted_pricing", "user_policy", {"trusted_for_budget_enforcement": False, "dollar_budget_configured": True})

sid = session("validation-failure")
for i in range(3):
    event(sid, "validation_failed", "test", payload={"validation_category": "test"}, i=i)
sessions["validation-failure"] = sid

sid = session("unvalidated-edit")
event(sid, "file_edit_observed", "file_edit_observed")
event(sid, "stop_observed", "stop_observed", i=1)
sessions["unvalidated-edit"] = sid

sid = session("repeated-failure")
event(sid, "supported_tool_failed", "bash", payload={"tool_category": "bash"}, i=0)
event(sid, "supported_tool_failed", "bash", payload={"tool_category": "bash"}, i=1)
sessions["repeated-failure"] = sid

conn.commit()
print(json.dumps(sessions))
PY

curl_json() {
    local url=$1
    local out=$2
    log_cmd "curl -fsS $url > $out"
    curl -fsS "$url" >"$out"
}

curl_json "$CORE_URL/api/companion/sessions?limit=20&days=30" "$REPORT_DIR/companion-sessions.json"
for name in clean-readonly validation-failure unvalidated-edit repeated-failure high-context pricing-trust; do
    sid="fake-$RUN_ID-$name"
    curl_json "$CORE_URL/api/companion/session/$sid" "$REPORT_DIR/companion-$name.json"
done

log_cmd "curl -fsS -X POST $CORE_URL/api/coach/hook (risky-command fixture)"
curl -fsS -X POST "$CORE_URL/api/coach/hook" \
    -H 'content-type: application/json' \
    --data '{"hook_event_name":"PreToolUse","session_id":"fake-'"$RUN_ID"'-risky-command","turn_id":"turn_1","tool_name":"Bash","tool_input":{"command":"git reset --hard HEAD"}}' \
    >"$REPORT_DIR/hook-action-log.json"

log_cmd "target/debug/codex-blackbox status --url $CORE_URL --json fake-$RUN_ID-high-context"
target/debug/codex-blackbox status --url "$CORE_URL" --json "fake-$RUN_ID-high-context" >"$REPORT_DIR/status.json"
log_cmd "target/debug/codex-blackbox guard --url $CORE_URL --json fake-$RUN_ID-high-context"
target/debug/codex-blackbox guard --url "$CORE_URL" --json "fake-$RUN_ID-high-context" >"$REPORT_DIR/guard.json"
log_cmd "target/debug/codex-blackbox postmortem --url $CORE_URL --output $REPORT_DIR/postmortem.md fake-$RUN_ID-high-context"
target/debug/codex-blackbox postmortem --url "$CORE_URL" --output "$REPORT_DIR/postmortem.md" "fake-$RUN_ID-high-context"
curl_json "$CORE_URL/metrics" "$REPORT_DIR/metrics.txt"

cat >"$REPORT_DIR/final-classification.json" <<JSON
{
  "run_id": "$RUN_ID",
  "artifact_dir": "$REPORT_DIR",
  "classification": {
    "fake_proxy_hook_e2e": "fake",
    "companion_ui_api": "fake",
    "live_cli_smoke": "skipped",
    "live_ui_desktop_smoke": "skipped"
  },
  "unsupported_or_untrusted": [
    "fixture evidence is not live Codex support proof",
    "hook evidence is advisory and incomplete",
    "dollar pricing remains advisory unless trusted_for_budget_enforcement is true"
  ]
}
JSON

printf "Coach companion fake E2E artifacts: %s\n" "$REPORT_DIR"
