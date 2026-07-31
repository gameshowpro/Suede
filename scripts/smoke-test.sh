#!/usr/bin/env bash
# End-to-end smoke test against a running daemon in mock mode.
#
# Exercises the paths that unit tests cannot: real HTTP, real process
# supervision, real persistence across a restart.
set -uo pipefail

PORT="${PORT:-7099}"
BASE="http://127.0.0.1:${PORT}"
BIN="${BIN:-./target/debug/suede}"
PASS=0
FAIL=0

check() {
  local name="$1" expected="$2" actual="$3"
  if [[ "$expected" == "$actual" ]]; then
    echo "  PASS  $name"
    PASS=$((PASS + 1))
  else
    echo "  FAIL  $name: expected '$expected', got '$actual'"
    FAIL=$((FAIL + 1))
  fi
}

STATE_DIR="$(mktemp -d)"
CONFIG_DIR="$(mktemp -d)"
export SUEDE_STATE_DIR="$STATE_DIR"
export XDG_CONFIG_HOME="$CONFIG_DIR"
LOG="$(mktemp)"

cleanup() {
  [[ -n "${DAEMON_PID:-}" ]] && kill "$DAEMON_PID" 2>/dev/null
  wait "${DAEMON_PID:-}" 2>/dev/null
  rm -rf "$STATE_DIR" "$CONFIG_DIR"
}
trap cleanup EXIT

start_daemon() {
  "$BIN" run --mock --bind "127.0.0.1:${PORT}" >>"$LOG" 2>&1 &
  DAEMON_PID=$!
  for _ in $(seq 1 50); do
    if curl -sf "${BASE}/healthz" >/dev/null 2>&1; then return 0; fi
    sleep 0.2
  done
  echo "daemon failed to start; log:" && cat "$LOG"
  return 1
}

status_of() { curl -s -o /dev/null -w '%{http_code}' "$@"; }
json_of() { curl -s "$@"; }

echo "Starting daemon on port ${PORT}"
start_daemon || exit 1

echo
echo "Observed state"
check "healthz is 200" "200" "$(status_of "${BASE}/healthz")"
check "three outputs are visible" "3" \
  "$(json_of "${BASE}/api/v1/outputs" | python3 -c 'import sys,json;print(len(json.load(sys.stdin)))')"
check "output detail is served" "HDMI-A-1" \
  "$(json_of "${BASE}/api/v1/outputs/HDMI-A-1" | python3 -c 'import sys,json;print(json.load(sys.stdin)["name"])')"
check "unknown output is 404" "404" "$(status_of "${BASE}/api/v1/outputs/HDMI-A-99")"
check "windows are visible" "2" \
  "$(json_of "${BASE}/api/v1/windows" | python3 -c 'import sys,json;print(len(json.load(sys.stdin)))')"
check "audio sinks are visible" "2" \
  "$(json_of "${BASE}/api/v1/audio/outputs" | python3 -c 'import sys,json;print(len(json.load(sys.stdin)))')"
check "health checks run" "12" \
  "$(json_of "${BASE}/api/v1/system/checks" | python3 -c 'import sys,json;print(len(json.load(sys.stdin)))')"
check "system reports its version" "true" \
  "$(json_of "${BASE}/api/v1/system" | python3 -c 'import sys,json;print(str(len(json.load(sys.stdin)["suedeVersion"])>0).lower())')"

echo
echo "Desired state"
CONFIG='{
  "outputs": [
    {"match":{"name":"HDMI-A-3"},"enable":true,
     "mode":{"width":1920,"height":1080,"refreshHz":60},"position":{"x":3840,"y":0}}
  ],
  "apps": [
    {"id":"renderer-1","enabled":true,
     "launcher":{"kind":"exec","command":"sleep","args":["300"]},
     "heartbeat":{"enabled":true,"timeoutSeconds":25,"startupGraceSeconds":60}}
  ]
}'
RESULT="$(curl -s -X PUT "${BASE}/api/v1/config?wait=10" \
  -H 'content-type: application/json' -d "$CONFIG")"
check "write returns revision 1" "1" \
  "$(echo "$RESULT" | python3 -c 'import sys,json;print(json.load(sys.stdin)["revision"])')"

check "invalid config is 422" "422" \
  "$(curl -s -o /dev/null -w '%{http_code}' -X PUT "${BASE}/api/v1/config" \
     -H 'content-type: application/json' \
     -d '{"apps":[{"id":"x","launcher":{"kind":"exec","command":"true"}},
                  {"id":"x","launcher":{"kind":"exec","command":"true"}}]}')"
check "stale If-Match is 409" "409" \
  "$(curl -s -o /dev/null -w '%{http_code}' -X PUT "${BASE}/api/v1/config" \
     -H 'content-type: application/json' -H 'if-match: 0' -d '{}')"

echo
echo "Reconciliation"
check "inactive output was enabled" "true" \
  "$(json_of "${BASE}/api/v1/outputs/HDMI-A-3" | python3 -c 'import sys,json;print(str(json.load(sys.stdin)["active"]).lower())')"
check "status is synced" "synced" \
  "$(json_of "${BASE}/api/v1/status" | python3 -c 'import sys,json;print(json.load(sys.stdin)["state"])')"

echo
echo "Application supervision"
sleep 1
check "app is running" "running" \
  "$(json_of "${BASE}/api/v1/apps/renderer-1/status" | python3 -c 'import sys,json;print(json.load(sys.stdin)["state"])')"
APP_PID="$(json_of "${BASE}/api/v1/apps/renderer-1/status" | python3 -c 'import sys,json;print(json.load(sys.stdin)["pid"])')"
check "app has a pid" "true" "$([[ -n "$APP_PID" && "$APP_PID" != "None" ]] && echo true || echo false)"
check "loopback heartbeat is accepted" "204" \
  "$(status_of -X POST "${BASE}/api/v1/apps/renderer-1/heartbeat")"
check "unknown app heartbeat is 404" "404" \
  "$(status_of -X POST "${BASE}/api/v1/apps/ghost/heartbeat")"

echo "  ...killing the app process to prove it is relaunched"
kill -9 "$APP_PID" 2>/dev/null
sleep 3
NEW_PID="$(json_of "${BASE}/api/v1/apps/renderer-1/status" | python3 -c 'import sys,json;print(json.load(sys.stdin)["pid"])')"
check "app was relaunched with a new pid" "true" \
  "$([[ -n "$NEW_PID" && "$NEW_PID" != "None" && "$NEW_PID" != "$APP_PID" ]] && echo true || echo false)"

echo
echo "Events"
SSE="$(timeout 6 curl -sN "${BASE}/api/v1/events" &
       sleep 1
       curl -s -X POST "${BASE}/api/v1/reconcile" >/dev/null
       wait)" || true
check "SSE delivers named events" "true" \
  "$([[ "$SSE" == *"event: "* ]] && echo true || echo false)"

echo
echo "Persistence across restart"
kill "$DAEMON_PID" 2>/dev/null
wait "$DAEMON_PID" 2>/dev/null
check "state file was written" "true" \
  "$([[ -f "${STATE_DIR}/state.json" ]] && echo true || echo false)"
check "no orphaned app process" "true" \
  "$(pgrep -f 'sleep 300' >/dev/null && echo false || echo true)"

start_daemon || exit 1
# Give the boot-restore pass time to enable outputs (which includes a settle
# delay) and launch apps.
sleep 6
check "configuration survived the restart" "1" \
  "$(json_of "${BASE}/api/v1/config" | python3 -c 'import sys,json;print(json.load(sys.stdin)["revision"])')"
check "app was relaunched after restart" "1" \
  "$(json_of "${BASE}/api/v1/apps" | python3 -c 'import sys,json;print(len(json.load(sys.stdin)))')"
check "output configuration was reapplied" "true" \
  "$(json_of "${BASE}/api/v1/outputs/HDMI-A-3" | python3 -c 'import sys,json;print(str(json.load(sys.stdin)["active"]).lower())')"

echo
echo "OpenAPI"
check "openapi.json is served" "200" "$(status_of "${BASE}/api-docs/openapi.json")"
check "scalar docs are served" "200" "$(status_of "${BASE}/docs")"
# Compared as parsed JSON: the two differ only by a trailing newline.
check "cli emits the same document as the server" "true" \
  "$(python3 -c '
import json, subprocess, sys, urllib.request
cli = json.loads(subprocess.check_output([sys.argv[1], "openapi"]))
served = json.loads(urllib.request.urlopen(sys.argv[2]).read())
print(str(cli == served).lower())
' "$BIN" "${BASE}/api-docs/openapi.json")"

echo
echo "─────────────────────────────"
echo "  ${PASS} passed, ${FAIL} failed"
[[ "$FAIL" -eq 0 ]] || { echo; echo "daemon log:"; tail -30 "$LOG"; }
exit "$FAIL"
