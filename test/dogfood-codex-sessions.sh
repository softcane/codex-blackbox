#!/usr/bin/env bash
# Real Codex multi-session dogfood feedback harness.
#
# In --mode real this launches real Codex sessions through Codex Blackbox's
# ChatGPT/Codex subscription proxy path. It uses the existing local Codex
# ChatGPT login, can contact chatgpt.com, and must not edit ~/.codex/config.toml.
# Run the static and fake gates first:
#
#   ./test/validate-openai-config.sh
#   ./test/e2e-openai-responses-full.sh
#
# The harness writes a report that names passed, failed, skipped, and missing
# capabilities instead of treating every unavailable real telemetry path as the
# same kind of failure.
set -euo pipefail

cd "$(dirname "$0")/.."

MODE="real"
SESSIONS=4
REPOS="mixed"
INCLUDE_MCP=0
KEEP_STACK=0
SAME_REPO="$(pwd)"
OTHER_REPO=""
REPORT_DIR=""
SESSION_TIMEOUT_SECONDS=360

CORE_URL="${CODEX_BLACKBOX_CORE_URL:-http://127.0.0.1:9091}"
ENVOY_URL="${CODEX_BLACKBOX_ENVOY_PROXY_URL:-http://127.0.0.1:10000}"
PROMETHEUS_URL="${CODEX_BLACKBOX_PROMETHEUS_URL:-http://127.0.0.1:9092}"
GRAFANA_URL="${CODEX_BLACKBOX_GRAFANA_URL:-http://127.0.0.1:3000}"
COMPOSE_PATH="${CODEX_BLACKBOX_COMPOSE_FILE:-docker-compose.yml}"
case "$COMPOSE_PATH" in
    /*) ;;
    *) COMPOSE_PATH="$(pwd)/$COMPOSE_PATH" ;;
esac

usage() {
    cat <<'EOF'
Usage: ./test/dogfood-codex-sessions.sh [options]

Options:
  --mode real|fixture       real launches Codex; fixture delegates to the fake regression
  --sessions N             number of sessions to launch in real mode, 1-4 (default: 4)
  --repos same|mixed       accepted for compatibility; real mode always creates disposable repos
  --include-mcp            include an MCP-oriented prompt when MCP config exists
  --same-repo PATH         trusted parent repo used for the report directory (default: cwd)
  --other-repo PATH        accepted for compatibility; disposable repos are auto-created
  --report-dir PATH        report artifact directory (default: reports/dogfood/<timestamp>)
  --timeout-seconds N      per-session timeout (default: 360)
  --no-json                accepted target-path flag; stdout telemetry parsing is disabled
  --keep-stack             leave Docker Compose stack running
  -h, --help               show this help
EOF
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --mode)
            MODE="${2:-}"
            shift 2
            ;;
        --sessions)
            SESSIONS="${2:-}"
            shift 2
            ;;
        --repos)
            REPOS="${2:-}"
            shift 2
            ;;
        --include-mcp)
            INCLUDE_MCP=1
            shift
            ;;
        --same-repo)
            SAME_REPO="${2:-}"
            shift 2
            ;;
        --other-repo)
            OTHER_REPO="${2:-}"
            shift 2
            ;;
        --report-dir)
            REPORT_DIR="${2:-}"
            shift 2
            ;;
        --timeout-seconds)
            SESSION_TIMEOUT_SECONDS="${2:-}"
            shift 2
            ;;
        --no-json)
            shift
            ;;
        --keep-stack)
            KEEP_STACK=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "Unknown option: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

if ! [[ "$SESSIONS" =~ ^[0-9]+$ ]] || [ "$SESSIONS" -lt 1 ] || [ "$SESSIONS" -gt 4 ]; then
    echo "--sessions must be an integer from 1 to 4" >&2
    exit 2
fi

if [ "$MODE" != "real" ] && [ "$MODE" != "fixture" ]; then
    echo "--mode must be real or fixture" >&2
    exit 2
fi

if [ "$REPOS" != "same" ] && [ "$REPOS" != "mixed" ]; then
    echo "--repos must be same or mixed" >&2
    exit 2
fi

RUN_STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
RUN_ID="phase9c-${RUN_STAMP}-$$"
if [ -z "$REPORT_DIR" ]; then
    REPORT_DIR="reports/dogfood/$RUN_STAMP"
fi
mkdir -p "$REPORT_DIR"

CHECKS_TSV="$REPORT_DIR/checks.tsv"
COMMAND_LOG="$REPORT_DIR/codex-commands.log"
SESSION_MANIFEST="$REPORT_DIR/sessions-manifest.tsv"
WATCH_SSE="$REPORT_DIR/watch.sse"
WATCH_NDJSON="$REPORT_DIR/watch.ndjson"
SUMMARY_JSON="$REPORT_DIR/summary.json"
SUMMARY_MD="$REPORT_DIR/summary.md"
PROMPT_MARKER="CODEX_BLACKBOX_DOGFOOD_${RUN_ID}"
CODEX_BIN="${CODEX_BIN:-codex}"
CODEX_BLACKBOX_BIN="${CODEX_BLACKBOX_BIN:-}"
STACK_ALREADY_READY=0
WATCH_PID=""
DOGFOOD_CODEX_HOME=""

: >"$CHECKS_TSV"
: >"$COMMAND_LOG"
: >"$SESSION_MANIFEST"

record() {
    local status="$1"
    local name="$2"
    local detail="${3:-}"
    detail="${detail//$'\n'/ }"
    detail="${detail//$'\t'/ }"
    printf "%s\t%s\t%s\n" "$status" "$name" "$detail" >>"$CHECKS_TSV"
}

require_cmd() {
    if ! command -v "$1" >/dev/null 2>&1; then
        record failed "required_command_$1" "missing required command: $1"
        echo "Missing required command: $1" >&2
        exit 1
    fi
}

shell_join() {
    local out=""
    local quoted
    for arg in "$@"; do
        printf -v quoted "%q" "$arg"
        if [ -n "$out" ]; then
            out="$out $quoted"
        else
            out="$quoted"
        fi
    done
    printf "%s" "$out"
}

run_with_optional_timeout() {
    local seconds="$1"
    shift
    if command -v timeout >/dev/null 2>&1; then
        timeout "$seconds" "$@"
    elif command -v gtimeout >/dev/null 2>&1; then
        gtimeout "$seconds" "$@"
    else
        "$@"
    fi
}

capture_for_seconds() {
    local seconds="$1"
    shift
    "$@" &
    local pid="$!"
    sleep "$seconds"
    kill "$pid" >/dev/null 2>&1 || true
    wait "$pid" >/dev/null 2>&1 || true
}

file_mtime() {
    local path="$1"
    if [ ! -e "$path" ]; then
        printf "missing"
        return 0
    fi
    stat -f "%m" "$path" 2>/dev/null || stat -c "%Y" "$path"
}

file_hash() {
    local path="$1"
    if [ ! -e "$path" ]; then
        printf "missing"
        return 0
    fi
    if command -v shasum >/dev/null 2>&1; then
        LC_ALL=C LANG=C shasum -a 256 "$path" | awk '{print $1}'
    else
        LC_ALL=C LANG=C sha256sum "$path" | awk '{print $1}'
    fi
}

snapshot_codex_config_before() {
    CODEX_CONFIG_PATH="${CODEX_HOME:-$HOME/.codex}/config.toml"
    CODEX_CONFIG_BEFORE_MTIME="$(file_mtime "$CODEX_CONFIG_PATH")"
    CODEX_CONFIG_BEFORE_HASH="$(file_hash "$CODEX_CONFIG_PATH")"
    CODEX_CONFIG_SNAPSHOT_TAKEN=1
    {
        printf "path\t%s\n" "$CODEX_CONFIG_PATH"
        printf "mtime\t%s\n" "$CODEX_CONFIG_BEFORE_MTIME"
        printf "sha256\t%s\n" "$CODEX_CONFIG_BEFORE_HASH"
    } >"$REPORT_DIR/codex-config-before.tsv"
}

prepare_temp_codex_home() {
    local real_codex_home="${CODEX_HOME:-$HOME/.codex}"
    DOGFOOD_CODEX_HOME="$(mktemp -d "${TMPDIR:-/tmp}/codex-blackbox-dogfood-codex-home.XXXXXX")"
    if [ ! -f "$real_codex_home/auth.json" ]; then
        record failed "temporary_codex_home" "$real_codex_home/auth.json is required for real Codex auth"
        return 1
    fi
    ln -s "$real_codex_home/auth.json" "$DOGFOOD_CODEX_HOME/auth.json"
    if [ -f "$real_codex_home/installation_id" ]; then
        ln -s "$real_codex_home/installation_id" "$DOGFOOD_CODEX_HOME/installation_id"
    fi
    printf "# Codex Blackbox dogfood temp config. Project trust writes here must not touch user config.\n" \
        >"$DOGFOOD_CODEX_HOME/config.toml"
    record passed "temporary_codex_home" "$DOGFOOD_CODEX_HOME"
}

check_codex_config_unchanged() {
    if [ "${CODEX_CONFIG_SNAPSHOT_TAKEN:-0}" != "1" ]; then
        return 0
    fi
    local after_mtime
    local after_hash
    after_mtime="$(file_mtime "$CODEX_CONFIG_PATH")"
    after_hash="$(file_hash "$CODEX_CONFIG_PATH")"
    {
        printf "path\t%s\n" "$CODEX_CONFIG_PATH"
        printf "mtime\t%s\n" "$after_mtime"
        printf "sha256\t%s\n" "$after_hash"
    } >"$REPORT_DIR/codex-config-after.tsv"
    if [ "$after_mtime" = "$CODEX_CONFIG_BEFORE_MTIME" ] \
        && [ "$after_hash" = "$CODEX_CONFIG_BEFORE_HASH" ]; then
        record passed "codex_config_unchanged" "$CODEX_CONFIG_PATH mtime/hash unchanged"
        CODEX_CONFIG_SNAPSHOT_TAKEN=checked
        return 0
    else
        record failed "codex_config_unchanged" "$CODEX_CONFIG_PATH changed during real validation"
        CODEX_CONFIG_SNAPSHOT_TAKEN=checked
        return 1
    fi
}

wait_for_http() {
    local label="$1"
    local url="$2"
    local tries="${3:-60}"
    for _ in $(seq 1 "$tries"); do
        if curl -fsS --max-time 2 "$url" >/dev/null 2>&1; then
            record passed "${label}_reachable" "$url"
            return 0
        fi
        sleep 2
    done
    record failed "${label}_reachable" "$url did not become reachable"
    return 1
}

wait_for_envoy_subscription_route() {
    local body
    for _ in $(seq 1 60); do
        body="$(curl -sS --max-time 2 "$ENVOY_URL" 2>/dev/null || true)"
        if grep -q "/backend-api" <<<"$body"; then
            record passed "envoy_subscription_route_reachable" "$ENVOY_URL"
            return 0
        fi
        sleep 2
    done
    record failed "envoy_subscription_route_reachable" "$ENVOY_URL did not expose the /backend-api route marker"
    return 1
}

compose() {
    env -u COMPOSE_FILE docker compose -p codex-blackbox -f "$COMPOSE_PATH" "$@"
}

capture_compose_logs() {
    compose ps >"$REPORT_DIR/compose-ps.txt" 2>&1 || true
    compose logs --no-color >"$REPORT_DIR/compose.log" 2>&1 || true
}

stop_watch_capture() {
    if [ -n "$WATCH_PID" ]; then
        kill "$WATCH_PID" >/dev/null 2>&1 || true
        wait "$WATCH_PID" >/dev/null 2>&1 || true
        WATCH_PID=""
    fi
}

cleanup() {
    stop_watch_capture
    capture_compose_logs
    if [ -n "$DOGFOOD_CODEX_HOME" ] && [ -f "$DOGFOOD_CODEX_HOME/config.toml" ]; then
        cp "$DOGFOOD_CODEX_HOME/config.toml" "$REPORT_DIR/temp-codex-config-after.toml" 2>/dev/null || true
    fi
    if [ -n "$DOGFOOD_CODEX_HOME" ] && [ -d "$DOGFOOD_CODEX_HOME" ]; then
        rm -rf "$DOGFOOD_CODEX_HOME"
    fi
    if [ "$MODE" = "real" ] && [ "$KEEP_STACK" = "0" ] && [ "$STACK_ALREADY_READY" = "0" ]; then
        compose down --remove-orphans -t 5 >"$REPORT_DIR/compose-down.log" 2>&1 || true
    fi
}
trap cleanup EXIT

write_summary() {
    REPORT_DIR="$REPORT_DIR" \
    CHECKS_TSV="$CHECKS_TSV" \
    SESSION_MANIFEST="$SESSION_MANIFEST" \
    WATCH_SSE="$WATCH_SSE" \
    WATCH_NDJSON="$WATCH_NDJSON" \
    SUMMARY_JSON="$SUMMARY_JSON" \
    SUMMARY_MD="$SUMMARY_MD" \
    PROMPT_MARKER="$PROMPT_MARKER" \
    EXPECTED_SESSIONS="$SESSIONS" \
    INCLUDE_MCP="$INCLUDE_MCP" \
    MCP_CONFIGURED="${MCP_CONFIGURED:-0}" \
    CORE_URL="$CORE_URL" \
    PROMETHEUS_URL="$PROMETHEUS_URL" \
    GRAFANA_URL="$GRAFANA_URL" \
    python3 - <<'PY'
import json
import os
import re
import sqlite3
import time
import urllib.parse
import urllib.request
from pathlib import Path

report_dir = Path(os.environ["REPORT_DIR"])
checks_tsv = Path(os.environ["CHECKS_TSV"])
manifest_path = Path(os.environ["SESSION_MANIFEST"])
watch_sse_path = Path(os.environ["WATCH_SSE"])
watch_ndjson_path = Path(os.environ["WATCH_NDJSON"])
watch_replay_sse_path = report_dir / "watch-replay.sse"
summary_json_path = Path(os.environ["SUMMARY_JSON"])
summary_md_path = Path(os.environ["SUMMARY_MD"])
prompt_marker = os.environ["PROMPT_MARKER"]
expected_sessions = int(os.environ["EXPECTED_SESSIONS"])
include_mcp = os.environ["INCLUDE_MCP"] == "1"
mcp_configured = os.environ["MCP_CONFIGURED"] == "1"
core_url = os.environ["CORE_URL"].rstrip("/")
prom_url = os.environ["PROMETHEUS_URL"].rstrip("/")
grafana_url = os.environ["GRAFANA_URL"].rstrip("/")

checks = {"passed": [], "failed": [], "skipped": [], "missing": []}
details = {}


def add(status, name, detail=""):
    checks.setdefault(status, []).append(name)
    if detail:
        details[name] = detail


if checks_tsv.exists():
    for raw in checks_tsv.read_text(encoding="utf-8").splitlines():
        if not raw.strip():
            continue
        parts = raw.split("\t", 2)
        while len(parts) < 3:
            parts.append("")
        status, name, detail = parts
        if status not in checks:
            status = "failed"
        add(status, name, detail)

manifest = []
if manifest_path.exists():
    for raw in manifest_path.read_text(encoding="utf-8").splitlines():
        if not raw.strip():
            continue
        case, repo_kind, repo, prompt, stdout_path, stderr_path, exit_path = raw.split("\t", 6)
        exit_code = None
        exit_file = Path(exit_path)
        if exit_file.exists():
            try:
                exit_code = int(exit_file.read_text(encoding="utf-8").strip())
            except ValueError:
                exit_code = None
        manifest.append(
            {
                "case": case,
                "repo_kind": repo_kind,
                "repo": repo,
                "prompt": prompt,
                "stdout_path": stdout_path,
                "stderr_path": stderr_path,
                "exit_path": exit_path,
                "exit_code": exit_code,
            }
        )

if len(manifest) >= expected_sessions:
    add("passed", "session_manifest_count", f"{len(manifest)} planned")
else:
    add("failed", "session_manifest_count", f"planned {len(manifest)} of {expected_sessions}")

for item in manifest:
    name = f"codex_session_{item['case']}"
    if item["exit_code"] == 0:
        add("passed", name, "exit 0")
    else:
        add("failed", name, f"exit {item['exit_code']}; see {item['stderr_path']}")

repo_paths = {item["repo"] for item in manifest}
workflow_kinds = {item["repo_kind"] for item in manifest}
if len(repo_paths) >= 3:
    add("passed", "disposable_repo_coverage", f"{len(repo_paths)} disposable repos")
else:
    add("failed", "disposable_repo_coverage", f"{len(repo_paths)} disposable repos")
for workflow in ["read", "write", "delete", "test"]:
    if workflow in workflow_kinds:
        add("passed", f"{workflow}_workflow_session", "planned real Codex session")
    else:
        add("failed", f"{workflow}_workflow_session", "missing planned real Codex session")
for item in manifest:
    repo = Path(item["repo"])
    if item["repo_kind"] == "write":
        notes = repo / "notes.md"
        if notes.exists() and prompt_marker in notes.read_text(encoding="utf-8", errors="replace"):
            add("passed", "write_workflow_artifact", str(notes))
        else:
            add("missing", "write_workflow_artifact", f"{notes} missing marker")
    elif item["repo_kind"] == "delete":
        obsolete = repo / "obsolete.txt"
        keep = repo / "keep.txt"
        if not obsolete.exists() and keep.exists():
            add("passed", "delete_workflow_artifact", "obsolete.txt removed and keep.txt preserved")
        else:
            add("missing", "delete_workflow_artifact", "delete workflow did not leave expected files")
    elif item["repo_kind"] == "test":
        stdout_body = Path(item["stdout_path"]).read_text(encoding="utf-8", errors="replace") if Path(item["stdout_path"]).exists() else ""
        stderr_body = Path(item["stderr_path"]).read_text(encoding="utf-8", errors="replace") if Path(item["stderr_path"]).exists() else ""
        combined = stdout_body + "\n" + stderr_body
        if "python3 -m unittest test_calc.py" in combined and ("OK" in combined or "Ran 1 test" in combined):
            add("passed", "test_workflow_artifact", "unittest command/result appeared in session output")
        else:
            add("missing", "test_workflow_artifact", "unittest command/result not found in captured output")

watch_events = []
if watch_sse_path.exists():
    with watch_sse_path.open(encoding="utf-8", errors="replace") as handle:
        for line in handle:
            line = line.rstrip("\n")
            if not line.startswith("data:"):
                continue
            payload = line[5:].strip()
            if not payload:
                continue
            try:
                watch_events.append(json.loads(payload))
            except json.JSONDecodeError:
                continue

watch_replay_events = []
if watch_replay_sse_path.exists():
    with watch_replay_sse_path.open(encoding="utf-8", errors="replace") as handle:
        for line in handle:
            line = line.rstrip("\n")
            if not line.startswith("data:"):
                continue
            payload = line[5:].strip()
            if not payload:
                continue
            try:
                watch_replay_events.append(json.loads(payload))
            except json.JSONDecodeError:
                continue

watch_ndjson_path.write_text(
    "".join(json.dumps(event, sort_keys=True) + "\n" for event in watch_events),
    encoding="utf-8",
)

marker_watch_events = [
    event for event in watch_events if prompt_marker in json.dumps(event, sort_keys=True)
]
session_ids = {
    event.get("session_id")
    for event in marker_watch_events
    if isinstance(event, dict) and event.get("session_id")
}
for event in watch_events:
    if not isinstance(event, dict):
        continue
    if event.get("session_id") in session_ids:
        marker_watch_events.append(event)

session_starts = [
    event for event in marker_watch_events if isinstance(event, dict) and event.get("type") == "session_start"
]
context_events = [
    event for event in marker_watch_events if isinstance(event, dict) and event.get("type") == "context_status"
]
turn_summaries = [
    event for event in marker_watch_events if isinstance(event, dict) and event.get("type") == "codex_turn_summary"
]
cache_events = [
    event for event in marker_watch_events if isinstance(event, dict) and event.get("type") == "cache_event"
]

if len(session_starts) >= expected_sessions:
    add("passed", "watch_session_start", f"{len(session_starts)} events")
else:
    add("missing", "watch_session_start", f"{len(session_starts)} events for marker {prompt_marker}")
if len(context_events) >= expected_sessions:
    add("passed", "watch_context_status", f"{len(context_events)} events")
else:
    add("missing", "watch_context_status", f"{len(context_events)} events")
if len(turn_summaries) >= expected_sessions:
    add("passed", "watch_codex_turn_summary", f"{len(turn_summaries)} events")
else:
    add("missing", "watch_codex_turn_summary", f"{len(turn_summaries)} events")
if cache_events:
    add("failed", "watch_no_codex_cache_event", f"unexpected cache events: {len(cache_events)}")
else:
    add("passed", "watch_no_codex_cache_event", "no cache_event observed for marker sessions")
replay_turns = [
    event for event in watch_replay_events
    if isinstance(event, dict)
    and event.get("type") == "codex_turn_summary"
    and (not session_ids or event.get("session_id") in session_ids)
]
if replay_turns:
    add("passed", "watch_replay_codex_turn_summary", f"{len(replay_turns)} replayed turn summaries")
else:
    add("missing", "watch_replay_codex_turn_summary", "no persisted Codex turn summaries replayed")
add("skipped", "tool_watch_events", "Envoy does not prove local tool result lifecycle")
add("skipped", "mcp_watch_events", "non-Envoy activity is outside the Codex telemetry surface")

sessions_json_path = report_dir / "sessions.json"
sessions_payload = {}
if sessions_json_path.exists():
    try:
        sessions_payload = json.loads(sessions_json_path.read_text(encoding="utf-8"))
    except json.JSONDecodeError:
        sessions_payload = {}
api_sessions = sessions_payload.get("sessions") or []
if api_sessions:
    add("passed", "api_sessions_query", f"{len(api_sessions)} sessions returned")
else:
    add("missing", "api_sessions_query", "no sessions returned by /api/sessions")

db_path = report_dir / "codex-blackbox.db"
db_session_ids = set()
if db_path.exists():
    try:
        conn = sqlite3.connect(db_path)
        conn.row_factory = sqlite3.Row
        query = """
            SELECT session_id, request_id, provider, requested_model, served_model, codex_status,
                   codex_input_tokens, codex_cached_input_tokens, codex_uncached_input_tokens,
                   codex_output_tokens, codex_reasoning_output_tokens, codex_total_tokens,
                   codex_prompt_excerpt
            FROM requests
            WHERE provider = 'codex_responses'
              AND (codex_prompt_excerpt LIKE ? OR session_id IN ({placeholders}))
        """
        ids = sorted(session_ids)
        placeholders = ",".join("?" for _ in ids) or "''"
        rows = list(
            conn.execute(
                query.format(placeholders=placeholders),
                [f"%{prompt_marker}%"] + ids,
            )
        )
        db_session_ids = {row["session_id"] for row in rows}
        if len(db_session_ids) >= expected_sessions:
            add("passed", "sqlite_codex_requests", f"{len(db_session_ids)} sessions")
        else:
            add("missing", "sqlite_codex_requests", f"{len(db_session_ids)} sessions")
        if db_session_ids:
            db_placeholders = ",".join("?" for _ in db_session_ids)
            session_row_count = conn.execute(
                f"SELECT COUNT(*) FROM sessions WHERE session_id IN ({db_placeholders})",
                sorted(db_session_ids),
            ).fetchone()[0]
            turn_row_count = conn.execute(
                f"SELECT COUNT(*) FROM turn_snapshots WHERE provider = 'codex_responses' AND session_id IN ({db_placeholders})",
                sorted(db_session_ids),
            ).fetchone()[0]
            if session_row_count >= len(db_session_ids):
                add("passed", "sqlite_session_rows", f"{session_row_count} session rows")
            else:
                add("missing", "sqlite_session_rows", f"{session_row_count} session rows for {len(db_session_ids)} sessions")
            if turn_row_count >= len(db_session_ids):
                add("passed", "sqlite_turn_rows", f"{turn_row_count} Codex turn rows")
            else:
                add("missing", "sqlite_turn_rows", f"{turn_row_count} Codex turn rows for {len(db_session_ids)} sessions")
        bad_math = []
        served_model_rows = 0
        for row in rows:
            input_tokens = row["codex_input_tokens"] or 0
            cached = row["codex_cached_input_tokens"] or 0
            uncached = row["codex_uncached_input_tokens"] or 0
            output = row["codex_output_tokens"] or 0
            total = row["codex_total_tokens"] or 0
            if input_tokens > 0 and input_tokens != cached + uncached:
                bad_math.append(row["request_id"])
            if total > 0 and total != input_tokens + output:
                bad_math.append(row["request_id"])
            if row["served_model"]:
                served_model_rows += 1
        if bad_math:
            add("failed", "sqlite_token_accounting", f"bad token math in requests: {bad_math}")
        elif rows:
            add("passed", "sqlite_token_accounting", "cached input was not double counted")
        else:
            add("missing", "sqlite_token_accounting", "no marker request rows to inspect")
        if served_model_rows:
            add("passed", "sqlite_served_model", f"{served_model_rows} rows have served_model")
        else:
            add("missing", "sqlite_served_model", "no marker rows stored served_model")
        (report_dir / "db-checks.json").write_text(
            json.dumps(
                {
                    "marker": prompt_marker,
                    "session_ids": sorted(db_session_ids),
                    "request_rows": [dict(row) for row in rows],
                },
                indent=2,
                sort_keys=True,
            ),
            encoding="utf-8",
        )
    except sqlite3.OperationalError as exc:
        if "no such table" in str(exc):
            add("missing", "sqlite_codex_persistence", str(exc))
        else:
            add("failed", "sqlite_query", str(exc))
    except Exception as exc:
        add("failed", "sqlite_query", str(exc))
else:
    add("missing", "sqlite_codex_persistence", "codex-blackbox.db was not copied from codex-blackbox-core")


def get_json(base_url, path, params=None, timeout=8):
    query = "" if params is None else "?" + urllib.parse.urlencode(params, doseq=True)
    with urllib.request.urlopen(base_url + path + query, timeout=timeout) as response:
        return json.loads(response.read().decode("utf-8"))


def post_json(base_url, path, payload, timeout=12):
    body = json.dumps(payload).encode("utf-8")
    request = urllib.request.Request(
        base_url + path,
        data=body,
        headers={"content-type": "application/json"},
        method="POST",
    )
    with urllib.request.urlopen(request, timeout=timeout) as response:
        return json.loads(response.read().decode("utf-8"))


def prom_query(expr):
    payload = get_json(prom_url, "/api/v1/query", {"query": expr})
    if payload.get("status") != "success":
        raise RuntimeError(f"Prometheus query failed for {expr}: {payload}")
    return payload.get("data", {}).get("result", [])


def prom_value(expr):
    result = prom_query(expr)
    if not result:
        return None
    return float(result[0]["value"][1])


prometheus_checks = {}
try:
    deadline = time.time() + 90
    while time.time() < deadline:
        value = prom_value('sum(codex_blackbox_requests_total{provider="codex_responses"})')
        if value and value >= 1:
            break
        time.sleep(2)
    prometheus_checks["codex_requests_total"] = prom_value(
        'sum(codex_blackbox_requests_total{provider="codex_responses"})'
    )
    token_kinds = ["input", "cached_input", "uncached_input", "output", "reasoning_output", "total"]
    token_values = {
        kind: prom_value(f'sum(codex_blackbox_tokens_total{{provider="codex_responses",kind="{kind}"}})')
        for kind in token_kinds
    }
    prometheus_checks["token_values"] = token_values
    if prometheus_checks["codex_requests_total"] and prometheus_checks["codex_requests_total"] >= 1:
        add("passed", "prometheus_codex_requests", str(prometheus_checks["codex_requests_total"]))
    else:
        add("missing", "prometheus_codex_requests", "codex_blackbox_requests_total provider=codex_responses missing")
    missing_tokens = [kind for kind, value in token_values.items() if value is None]
    if missing_tokens:
        add("missing", "prometheus_token_kinds", ", ".join(missing_tokens))
    else:
        add("passed", "prometheus_token_kinds", json.dumps(token_values, sort_keys=True))
    if prom_query("codex_blackbox_turn_duration_seconds_count"):
        add("passed", "prometheus_duration_metric", "codex_blackbox_turn_duration_seconds_count")
    else:
        add("missing", "prometheus_duration_metric", "codex_blackbox_turn_duration_seconds_count")
    if prom_query('codex_blackbox_context_fill_percent_count{provider="codex_responses"}'):
        add("passed", "prometheus_context_metric", "codex_blackbox_context_fill_percent_count")
    else:
        add("missing", "prometheus_context_metric", "codex_blackbox_context_fill_percent_count")
    now = int(time.time())
    series = get_json(
        prom_url,
        "/api/v1/series",
        {
            "match[]": '{__name__=~"codex_blackbox_.*"}',
            "start": str(now - 3600),
            "end": str(now),
        },
    )
    leaks = []
    for item in series.get("data", []):
        for key, value in item.items():
            if key == "__name__":
                continue
            key_lower = key.lower()
            if "session_id" in key_lower or key_lower in {"session", "proxy_session"}:
                leaks.append(item)
            for session_id in sorted(session_ids | db_session_ids):
                if session_id and session_id in str(value):
                    leaks.append(item)
    if leaks:
        add("failed", "prometheus_no_session_labels", json.dumps(leaks[:3], sort_keys=True))
    else:
        add("passed", "prometheus_no_session_labels", "no session ids in codex_blackbox_* metric labels")
except Exception as exc:
    add("failed", "prometheus_query", str(exc))

(report_dir / "metrics-checks.json").write_text(
    json.dumps(prometheus_checks, indent=2, sort_keys=True),
    encoding="utf-8",
)

metrics_prom = report_dir / "metrics.prom"
if metrics_prom.exists():
    metrics_body = metrics_prom.read_text(encoding="utf-8", errors="replace")
    forbidden_metric_terms = [
        "codex_blackbox_baseline_builds_total",
        "codex_blackbox_coach_actions_total",
        "codex_blackbox_hook_events_total",
        "codex_blackbox_loop_signals_total",
        "codex_blackbox_unvalidated_edit_signals_total",
        "codex_blackbox_validation_runs_total",
        "codex_blackbox_cache_events_total",
        "codex_blackbox_mcp_",
        "codex_blackbox_skill_events_total",
        "codex_blackbox_tool_failures_total",
        "quota",
        "tool_result",
    ]
    leaks = [term for term in forbidden_metric_terms if term in metrics_body]
    if leaks:
        add("failed", "metrics_disabled_surface_absent", ", ".join(leaks))
    else:
        add("passed", "metrics_disabled_surface_absent", "disabled metric families absent from rendered /metrics")
else:
    add("missing", "metrics_disabled_surface_absent", "metrics.prom artifact missing")

try:
    health = get_json(grafana_url, "/api/health")
    search = get_json(grafana_url, "/api/search", {"query": "Codex Blackbox"})
    dashboard_payload = get_json(grafana_url, "/api/dashboards/uid/codex-blackbox-main")
    dashboard = dashboard_payload.get("dashboard", {})
    grafana_result = {
        "health": health,
        "search": search,
        "dashboard_uid": dashboard.get("uid"),
        "dashboard_title": dashboard.get("title"),
    }
    (report_dir / "grafana.json").write_text(
        json.dumps(grafana_result, indent=2, sort_keys=True),
        encoding="utf-8",
    )
    if health.get("database") == "ok":
        add("passed", "grafana_health", "database ok")
    else:
        add("failed", "grafana_health", json.dumps(health, sort_keys=True))
    if any(item.get("uid") == "codex-blackbox-main" for item in search):
        add("passed", "grafana_dashboard_search", "codex-blackbox-main")
    else:
        add("missing", "grafana_dashboard_search", "codex-blackbox-main not found")
    if dashboard.get("uid") == "codex-blackbox-main":
        add("passed", "grafana_dashboard_uid", dashboard.get("title", ""))
    else:
        add("missing", "grafana_dashboard_uid", str(dashboard.get("uid")))
    dashboard_text = json.dumps(dashboard, sort_keys=True)
    forbidden_dashboard_terms = [
        "codex_blackbox_baseline_builds_total",
        "codex_blackbox_coach_actions_total",
        "codex_blackbox_hook_events_total",
        "codex_blackbox_loop_signals_total",
        "codex_blackbox_unvalidated_edit_signals_total",
        "codex_blackbox_validation_runs_total",
        "tool success",
        "succeeded",
        "preflight",
        "reconcile",
        "tmux",
    ]
    leaks = [term for term in forbidden_dashboard_terms if term in dashboard_text]
    if leaks:
        add("failed", "grafana_disabled_surface_absent", ", ".join(leaks))
    else:
        add("passed", "grafana_disabled_surface_absent", "dashboard contains no disabled metric families or feature wording")
    metric_names = {
        item.get("metric", {}).get("__name__")
        for item in prom_query('{__name__=~"codex_blackbox_.*"}')
        if item.get("metric", {}).get("__name__")
    }
    missing_panel_metrics = []
    for panel in dashboard.get("panels", []):
        title = panel.get("title", "")
        if "Codex" not in title:
            continue
        for target in panel.get("targets", []) or []:
            expr = target.get("expr", "")
            for metric_name in sorted(set(re.findall(r"\bcodex_blackbox_[A-Za-z_:][A-Za-z0-9_:]*", expr))):
                if metric_name not in metric_names:
                    missing_panel_metrics.append({"panel": title, "metric": metric_name})
    if missing_panel_metrics:
        add("failed", "grafana_panel_metrics", json.dumps(missing_panel_metrics[:5], sort_keys=True))
    else:
        add("passed", "grafana_panel_metrics", "Codex panel metrics are present in Prometheus")
    panel_exprs = [
        target.get("expr", "")
        for panel in dashboard.get("panels", [])
        for target in panel.get("targets", []) or []
        if target.get("expr")
    ]
    if any("codex_blackbox_turn_duration_seconds_bucket" in expr for expr in panel_exprs):
        add("passed", "grafana_latency_panel", "turn duration histogram is dashboarded")
    else:
        add("failed", "grafana_latency_panel", "turn duration histogram query missing")
    if any("codex_blackbox_tool_calls_total" in expr for expr in panel_exprs) and "Tool-Call Intent" in dashboard_text:
        add("passed", "grafana_tool_intent_panel", "tool-call intent panel present")
    else:
        add("failed", "grafana_tool_intent_panel", "tool-call intent panel missing")
    try:
        now_ms = int(time.time() * 1000)
        datasource_payload = post_json(
            grafana_url,
            "/api/ds/query",
            {
                "from": str(now_ms - 6 * 60 * 60 * 1000),
                "to": str(now_ms),
                "queries": [
                    {
                        "refId": "A",
                        "datasource": {"type": "prometheus", "uid": "prometheus"},
                        "expr": 'sum(codex_blackbox_requests_total{provider="codex_responses"})',
                        "instant": True,
                        "range": False,
                        "intervalMs": 30000,
                        "maxDataPoints": 1,
                    }
                ],
            },
        )
        (report_dir / "grafana-datasource-query.json").write_text(
            json.dumps(datasource_payload, indent=2, sort_keys=True),
            encoding="utf-8",
        )
        add("passed", "grafana_datasource_query_values", "queried codex request total through Grafana datasource")
    except Exception as exc:
        add("missing", "grafana_datasource_query_values", str(exc))
except Exception as exc:
    add("failed", "grafana_query", str(exc))

diagnosis_dir = report_dir / "diagnosis"
diagnosis_dir.mkdir(exist_ok=True)
postmortem_dir = report_dir / "postmortem"
postmortem_dir.mkdir(exist_ok=True)
for session_id in sorted(session_ids | db_session_ids):
    try:
        with urllib.request.urlopen(f"{core_url}/api/diagnosis/{urllib.parse.quote(session_id)}", timeout=8) as response:
            body = response.read().decode("utf-8")
        (diagnosis_dir / f"{session_id}.json").write_text(body, encoding="utf-8")
    except Exception:
        continue
    try:
        with urllib.request.urlopen(f"{core_url}/api/postmortem/{urllib.parse.quote(session_id)}?redact=true", timeout=8) as response:
            body = response.read().decode("utf-8")
        (postmortem_dir / f"{session_id}.json").write_text(body, encoding="utf-8")
        report = json.loads(body)
        if report.get("impact", {}).get("local_total_tokens", 0) >= 0 and report.get("caveats"):
            add("passed", f"postmortem_{session_id}", "redacted report with local totals and caveats")
        else:
            add("missing", f"postmortem_{session_id}", "missing local totals or caveats")
        if prompt_marker in body:
            add("failed", f"postmortem_redaction_{session_id}", "prompt marker leaked in redacted postmortem")
        else:
            add("passed", f"postmortem_redaction_{session_id}", "prompt marker redacted")
    except Exception as exc:
        add("missing", f"postmortem_{session_id}", str(exc))

status = "pass"
if checks["failed"]:
    status = "fail"
elif checks["missing"]:
    status = "partial"

summary = {
    "status": status,
    "run_id": os.environ.get("PROMPT_MARKER", "").replace("CODEX_BLACKBOX_DOGFOOD_", ""),
    "prompt_marker": prompt_marker,
    "report_dir": str(report_dir),
    "expected_sessions": expected_sessions,
    "observed_watch_session_ids": sorted(session_ids),
    "observed_db_session_ids": sorted(db_session_ids),
    "passed": checks["passed"],
    "failed": checks["failed"],
    "skipped": checks["skipped"],
    "missing": checks["missing"],
    "details": details,
}
summary_json_path.write_text(json.dumps(summary, indent=2, sort_keys=True), encoding="utf-8")

lines = [
    f"# Codex Blackbox Phase 9C Dogfood Report",
    "",
    f"- Status: `{status}`",
    f"- Prompt marker: `{prompt_marker}`",
    f"- Expected sessions: `{expected_sessions}`",
    f"- Watch session ids: {', '.join(sorted(session_ids)) or '(none)'}",
    f"- DB session ids: {', '.join(sorted(db_session_ids)) or '(none)'}",
    "",
]
for key in ["passed", "failed", "missing", "skipped"]:
    lines.append(f"## {key.title()}")
    values = checks[key]
    if values:
        for name in values:
            detail = details.get(name, "")
            suffix = f" - {detail}" if detail else ""
            lines.append(f"- `{name}`{suffix}")
    else:
        lines.append("- (none)")
    lines.append("")

summary_md_path.write_text("\n".join(lines), encoding="utf-8")
PY
}

finish_and_exit() {
    local code="$1"
    if [ "$MODE" = "real" ]; then
        if ! check_codex_config_unchanged; then
            code=1
        fi
    fi
    write_summary || true
    echo "Report: $REPORT_DIR"
    exit "$code"
}

for cmd in docker curl python3 "$CODEX_BIN"; do
    require_cmd "$cmd"
done

if [ "$MODE" = "fixture" ]; then
    record skipped "real_codex_sessions" "--mode fixture delegates to fake regression"
    CODEX_BLACKBOX_FULL_E2E_REPORT_DIR="$REPORT_DIR/fake-regression" ./test/e2e-openai-responses-full.sh
    record passed "fake_responses_regression" "see $REPORT_DIR/fake-regression"
    finish_and_exit 0
fi

snapshot_codex_config_before
prepare_temp_codex_home || finish_and_exit 1

if [ -z "$CODEX_BLACKBOX_BIN" ]; then
    require_cmd cargo
    cargo build -q -p codex-blackbox-cli
    CODEX_BLACKBOX_BIN="target/debug/codex-blackbox"
fi

if [ ! -x "$CODEX_BLACKBOX_BIN" ]; then
    record failed "codex_blackbox_binary" "$CODEX_BLACKBOX_BIN is not executable"
    finish_and_exit 1
fi
record passed "codex_blackbox_binary" "$CODEX_BLACKBOX_BIN"

CODEX_VERSION="$("$CODEX_BIN" --version 2>&1 | tr '\n' ' ' | sed 's/[[:space:]]*$//')"
printf "%s\n" "$CODEX_VERSION" >"$REPORT_DIR/codex-version.txt"
record passed "codex_version" "$CODEX_VERSION"

MCP_CONFIGURED=0
if [ -f "${CODEX_HOME:-$HOME/.codex}/config.toml" ] \
    && grep -q '^\[mcp_servers\.' "${CODEX_HOME:-$HOME/.codex}/config.toml"; then
    MCP_CONFIGURED=1
fi
if [ "$INCLUDE_MCP" = "1" ] && [ "$MCP_CONFIGURED" = "1" ]; then
    record passed "mcp_config_detected" "${CODEX_HOME:-$HOME/.codex}/config.toml"
elif [ "$INCLUDE_MCP" = "1" ]; then
    record skipped "mcp_config_detected" "no [mcp_servers.*] entries found"
fi

if curl -fsS --max-time 2 "$CORE_URL/health" >/dev/null 2>&1 \
    && curl -sS --max-time 2 "$ENVOY_URL" 2>/dev/null | grep -q "/backend-api"; then
    STACK_ALREADY_READY=1
fi
if [ "$STACK_ALREADY_READY" = "0" ]; then
    compose down --remove-orphans -t 5 >"$REPORT_DIR/compose-preclean.log" 2>&1 || true
    record passed "stack_preclean" "removed stale Codex Blackbox Compose services before real smoke"
fi

config_cmd=(
    "$CODEX_BLACKBOX_BIN" config codex
)
printf "%s\n" "$(shell_join "${config_cmd[@]}")" >"$REPORT_DIR/config-command.txt"
if "${config_cmd[@]}" >"$REPORT_DIR/config-codex.txt" 2>"$REPORT_DIR/config-codex.stderr" < /dev/null; then
    record passed "config_codex_preview" "read-only config preview captured"
else
    record failed "config_codex_preview" "see $REPORT_DIR/config-codex.stderr"
    finish_and_exit 1
fi

if [ "$STACK_ALREADY_READY" = "0" ]; then
    up_cmd=(
        env -u COMPOSE_FILE CODEX_BLACKBOX_COMPOSE_FILE="$COMPOSE_PATH"
        "$CODEX_BLACKBOX_BIN" up
    )
    printf "%s\n" "$(shell_join "${up_cmd[@]}")" >"$REPORT_DIR/up-command.txt"
    if "${up_cmd[@]}" >"$REPORT_DIR/up.log" 2>&1 < /dev/null; then
        record passed "codex_blackbox_up" "stack started through enabled CLI"
    else
        record failed "codex_blackbox_up" "see $REPORT_DIR/up.log"
        finish_and_exit 1
    fi
else
    record passed "codex_blackbox_up" "stack already reachable"
fi

SMOKE_PROMPT="$PROMPT_MARKER smoke-readiness: Read AGENTS.md and docs/reference/developing.md, then summarize the current evidence rules in 3 bullets. Do not edit files."
smoke_cmd=(
    env -u COMPOSE_FILE CODEX_BLACKBOX_COMPOSE_FILE="$COMPOSE_PATH" CODEX_HOME="$DOGFOOD_CODEX_HOME"
    "$CODEX_BLACKBOX_BIN" run --
    "$CODEX_BIN" exec
    --cd "$SAME_REPO"
    --sandbox read-only
    "$SMOKE_PROMPT"
)
printf "%s\n" "$(shell_join "${smoke_cmd[@]}")" >"$REPORT_DIR/smoke-command.txt"
if "${smoke_cmd[@]}" >"$REPORT_DIR/smoke.log" 2>&1 < /dev/null; then
    record passed "real_codex_smoke" "observed through enabled run wrapper"
else
    record failed "real_codex_smoke" "see $REPORT_DIR/smoke.log"
    finish_and_exit 1
fi

wait_for_http "codex_blackbox_core" "$CORE_URL/health" || finish_and_exit 1
wait_for_envoy_subscription_route || finish_and_exit 1
wait_for_http "prometheus" "$PROMETHEUS_URL/-/ready" || true
wait_for_http "grafana" "$GRAFANA_URL/api/health" || true

DISPOSABLE_REPO_ROOT="$REPORT_DIR/repos"
READ_REPO="$DISPOSABLE_REPO_ROOT/read-repo"
WRITE_REPO="$DISPOSABLE_REPO_ROOT/write-repo"
DELETE_REPO="$DISPOSABLE_REPO_ROOT/delete-repo"
TEST_REPO="$DISPOSABLE_REPO_ROOT/test-repo"
mkdir -p "$READ_REPO" "$WRITE_REPO" "$DELETE_REPO" "$TEST_REPO"

printf "# Read workflow repo\n\nmarker=%s\n" "$PROMPT_MARKER" >"$READ_REPO/README.md"
printf "alpha\nbeta\n" >"$READ_REPO/data.txt"

printf "# Write workflow repo\n\nCreate notes during validation.\n" >"$WRITE_REPO/README.md"

printf "# Delete workflow repo\n\nRemove obsolete.txt during validation.\n" >"$DELETE_REPO/README.md"
printf "obsolete marker %s\n" "$PROMPT_MARKER" >"$DELETE_REPO/obsolete.txt"
printf "keep marker %s\n" "$PROMPT_MARKER" >"$DELETE_REPO/keep.txt"

cat >"$TEST_REPO/calc.py" <<'EOF'
def add(a, b):
    return a + b
EOF
cat >"$TEST_REPO/test_calc.py" <<'EOF'
import unittest
from calc import add


class CalcTest(unittest.TestCase):
    def test_add(self):
        self.assertEqual(add(2, 3), 5)


if __name__ == "__main__":
    unittest.main()
EOF
printf "# Test workflow repo\n\nRun python3 -m unittest test_calc.py\n" >"$TEST_REPO/README.md"

if command -v git >/dev/null 2>&1; then
    for repo_path in "$READ_REPO" "$WRITE_REPO" "$DELETE_REPO" "$TEST_REPO"; do
        git -C "$repo_path" init -q || true
    done
fi
record passed "disposable_repos_created" "$READ_REPO $WRITE_REPO $DELETE_REPO $TEST_REPO"

declare -a CASES
declare -a REPO_KINDS
declare -a REPOS_FOR_CASE
declare -a PROMPTS

add_case() {
    CASES+=("$1")
    REPO_KINDS+=("$2")
    REPOS_FOR_CASE+=("$3")
    PROMPTS+=("$4")
}

add_case \
    "repo-read" \
    "read" \
    "$READ_REPO" \
    "$PROMPT_MARKER repo-read: Read README.md and data.txt in this disposable repo. Answer with exactly two bullets and do not edit files."

add_case \
    "repo-write" \
    "write" \
    "$WRITE_REPO" \
    "$PROMPT_MARKER repo-write: Create a file named notes.md containing one sentence with this marker. Then run ls and report the file you created."

if [ "$INCLUDE_MCP" = "1" ]; then
    record skipped "mcp_case" "read/write/delete/test coverage takes precedence in the default real validation"
fi

add_case \
    "repo-delete" \
    "delete" \
    "$DELETE_REPO" \
    "$PROMPT_MARKER repo-delete: Delete obsolete.txt, leave keep.txt untouched, then run ls and report which file was deleted."

add_case \
    "repo-test" \
    "test" \
    "$TEST_REPO" \
    "$PROMPT_MARKER repo-test: Run python3 -m unittest test_calc.py in this disposable repo. Do not edit files unless the test unexpectedly fails; report the command and result."

: >"$WATCH_SSE"
(curl -fsS --no-buffer -N -H "Accept: text/event-stream" "$CORE_URL/watch" >"$WATCH_SSE" 2>"$REPORT_DIR/watch.stderr" || true) &
WATCH_PID=$!
sleep 2

declare -a PIDS
declare -a EXIT_FILES

for index in $(seq 0 $((SESSIONS - 1))); do
    case_name="${CASES[$index]}"
    repo_kind="${REPO_KINDS[$index]}"
    repo_path="${REPOS_FOR_CASE[$index]}"
    prompt="${PROMPTS[$index]}"
    sandbox="read-only"
    if [ "$repo_kind" = "write" ] || [ "$repo_kind" = "delete" ]; then
        sandbox="workspace-write"
    fi
    stdout_path="$REPORT_DIR/session-${case_name}.stdout"
    stderr_path="$REPORT_DIR/session-${case_name}.stderr"
    exit_path="$REPORT_DIR/session-${case_name}.exit"
    session_cmd=(
        env -u COMPOSE_FILE CODEX_BLACKBOX_COMPOSE_FILE="$COMPOSE_PATH" CODEX_HOME="$DOGFOOD_CODEX_HOME"
        "$CODEX_BLACKBOX_BIN" run --
        "$CODEX_BIN" exec
        --cd "$repo_path"
        --sandbox "$sandbox"
        "$prompt"
    )
    printf "%s\n" "$(shell_join "${session_cmd[@]}")" >>"$COMMAND_LOG"
    printf "%s\t%s\t%s\t%s\t%s\t%s\t%s\n" \
        "$case_name" "$repo_kind" "$repo_path" "$prompt" "$stdout_path" "$stderr_path" "$exit_path" \
        >>"$SESSION_MANIFEST"
    (
        set +e
        run_with_optional_timeout "$SESSION_TIMEOUT_SECONDS" "${session_cmd[@]}" >"$stdout_path" 2>"$stderr_path" < /dev/null
        code=$?
        printf "%s\n" "$code" >"$exit_path"
        exit 0
    ) &
    PIDS+=("$!")
    EXIT_FILES+=("$exit_path")
done

for pid in "${PIDS[@]}"; do
    wait "$pid" || true
done

sleep 8
stop_watch_capture

capture_for_seconds 6 curl -fsS --no-buffer -N -H "Accept: text/event-stream" "$CORE_URL/watch?replay=1" \
    >"$REPORT_DIR/watch-replay.sse" 2>"$REPORT_DIR/watch-replay.stderr" || true
if [ -s "$REPORT_DIR/watch-replay.sse" ]; then
    record passed "watch_replay_artifact" "$REPORT_DIR/watch-replay.sse"
else
    record missing "watch_replay_artifact" "no replay SSE captured"
fi

curl -fsS --max-time 10 "$CORE_URL/api/sessions?limit=80&days=1" >"$REPORT_DIR/sessions.json" 2>"$REPORT_DIR/sessions.stderr" \
    && record passed "api_sessions_artifact" "$REPORT_DIR/sessions.json" \
    || record missing "api_sessions_artifact" "failed to fetch /api/sessions"

curl -fsS --max-time 10 "$CORE_URL/metrics" >"$REPORT_DIR/metrics.prom" 2>"$REPORT_DIR/metrics.stderr" \
    && record passed "core_metrics_artifact" "$REPORT_DIR/metrics.prom" \
    || record missing "core_metrics_artifact" "failed to fetch /metrics"

container_id="$(compose ps -q codex-blackbox-core 2>/dev/null || true)"
if [ -n "$container_id" ] && docker cp "$container_id:/data/." "$REPORT_DIR/" >/dev/null 2>&1; then
    record passed "sqlite_artifact" "$REPORT_DIR/codex-blackbox.db"
else
    record missing "sqlite_artifact" "failed to copy /data from codex-blackbox-core"
fi

capture_compose_logs

exit_code=0
for exit_path in "${EXIT_FILES[@]}"; do
    if [ ! -f "$exit_path" ] || [ "$(cat "$exit_path")" != "0" ]; then
        exit_code=1
    fi
done

finish_and_exit "$exit_code"
