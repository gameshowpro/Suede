#!/usr/bin/env bash
# Profiles Suede's projection pipeline on a remote appliance: system/protocol
# identity, the committed layout, and three measurement passes (grid test
# pattern, content, GPU-saturating page), each read from
# GET /api/v1/projection/stats with concurrent slicer/sway CPU and GPU
# utilisation sampling over the same window. See design-notes/test-log.md
# ("Reproducing a run") for the measurement recipe this follows.
#
# Usage: scripts/profile-projection.sh [host]   (default: brain)
#
# Everything runs over `ssh <host>` against the daemon's local API
# (http://localhost:9088), so the host only needs to be reachable by ssh.
#
# Idempotent: the grid-pattern and content/GPU-saturating passes are applied
# as *uncommitted* working copies (a full-document PUT without
# `"committed": true"`) or via POST /api/v1/apps/{id}/activate (which does
# commit `activeApp` - restored to its original value at the end), and
# POST /api/v1/config/revert clears the working copy afterwards. The script
# never edits the committed layout or projection section itself - if the
# committed layout does not already overlap, apply one by hand first (see
# docs/configuration.md, "Projection and edge blending") and re-run this
# script for the after-profile.
set -uo pipefail

HOST="${1:-brain}"

ssh "$HOST" bash -s <<'REMOTE_SCRIPT'
set -uo pipefail
API=http://localhost:9088/api/v1

hr() { printf '\n=== %s ===\n' "$1"; }

# ---------------------------------------------------------------------------
# 1. System / protocol identity
# ---------------------------------------------------------------------------
hr "System identity (GET /api/v1/system)"
curl -s "$API/system"
echo

echo "sway: $(sway --version)"
echo "nvidia-smi:"
nvidia-smi --query-gpu=name,driver_version --format=csv,noheader

SWAYPID=$(pgrep -x sway | head -1)
echo "sway pid: ${SWAYPID:-none}; its WLR_* environment:"
if [[ -n "${SWAYPID:-}" ]]; then
  sudo tr '\0' '\n' < "/proc/$SWAYPID/environ" 2>/dev/null | grep '^WLR_' \
    || echo "  (no WLR_* vars, or /proc/$SWAYPID/environ unreadable)"
else
  echo "  (sway not running)"
fi

if ! command -v wayland-info >/dev/null 2>&1; then
  echo "wayland-info not found; installing wayland-utils..."
  sudo apt-get update -qq && sudo apt-get install -y wayland-utils
fi
WDISP=""
for sock in /run/user/"$(id -u)"/wayland-*; do
  [[ "$sock" == *.lock ]] && continue
  [[ -S "$sock" ]] && WDISP=$(basename "$sock") && break
done
echo "wayland-info globals of interest (WAYLAND_DISPLAY=${WDISP:-unset}):"
if [[ -n "$WDISP" ]]; then
  WAYLAND_DISPLAY="$WDISP" wayland-info 2>&1 \
    | grep -iE 'zwlr_export_dmabuf|ext_image_copy_capture|zwlr_screencopy|zwp_linux_dmabuf|wp_presentation|zwlr_layer_shell' \
    || echo "  (wayland-info ran but none of the filtered globals matched)"
else
  echo "  (no wayland-* socket found under /run/user/$(id -u))"
fi

# ---------------------------------------------------------------------------
# 2. Committed outputs + projection config
# ---------------------------------------------------------------------------
hr "Committed outputs + projection (GET /api/v1/config)"
CONFIG_JSON=$(curl -s "$API/config")
echo "$CONFIG_JSON" | jq '{revision, committed, activeApp, outputs, projection}'

hr "Health checks (GET /api/v1/system/checks)"
curl -s "$API/system/checks" | jq -c '.[] | select(.status != "pass")'
echo "(no output above means every check passes)"

# ---------------------------------------------------------------------------
# Helpers for the measurement passes
# ---------------------------------------------------------------------------
sample_window() {
  # Samples slicer + sway %cpu (every 2s) and GPU utilisation (every 2s) for
  # ~$2 seconds, then prints the GET /api/v1/projection/stats snapshot taken
  # right after - the slicer's own ten-second report covers the same window.
  local label="$1" dur="${2:-10}"
  echo "-- $label --"

  local gpu_log
  gpu_log=$(mktemp)
  timeout "${dur}s" nvidia-smi --query-gpu=utilization.gpu --format=csv,noheader -l 2 > "$gpu_log" &
  local gpu_bg=$!

  local n=$((dur / 2))
  echo "cpu samples (slicer, sway):"
  for _ in $(seq 1 "$n"); do
    local slicer_pid sway_pid line
    slicer_pid=$(pgrep -f '/usr/bin/suede slice' | head -1)
    sway_pid=$(pgrep -x sway | head -1)
    line="$(date +%H:%M:%S)"
    if [[ -n "$slicer_pid" ]]; then
      line+="  slicer %cpu=$(ps -o %cpu= -p "$slicer_pid" | tr -d ' ')"
    else
      line+="  slicer not running"
    fi
    if [[ -n "$sway_pid" ]]; then
      line+="  sway %cpu=$(ps -o %cpu= -p "$sway_pid" | tr -d ' ')"
    fi
    echo "  $line"
    sleep 2
  done

  wait "$gpu_bg" 2>/dev/null
  echo "gpu utilisation samples (nvidia-smi utilization.gpu, %):"
  sed 's/^/  /' "$gpu_log"
  rm -f "$gpu_log"

  echo "projection/stats:"
  curl -s "$API/projection/stats"
  echo
}

startup_log_line() {
  journalctl --user -u suede --no-pager 2>/dev/null | grep 'slicer: renderer' | tail -1
}

with_uncommitted_test_pattern() {
  # Full-document PUT with committed:false so the working copy is applied
  # without touching the saved document (the /config/projection section
  # endpoint always commits - see docs/configuration.md "Working copies and
  # the committed flag" - so it cannot be used for an uncommitted preview).
  local pattern="$1"
  echo "$CONFIG_JSON" \
    | jq --arg p "$pattern" 'del(.revision) | .projection.testPattern = $p | .committed = false' \
    | curl -s -X PUT -H "Content-Type: application/json" --data @- "$API/config" > /dev/null
}

clear_working_copy() {
  curl -s -X POST "$API/config/revert" > /dev/null
}

reachable() {
  local url="$1" code
  code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 "$url" 2>/dev/null || echo 000)
  [[ "$code" == "200" ]]
}

# ---------------------------------------------------------------------------
# 3. Pass 1 - grid test pattern, 60s settle, then 3x10s reports
# ---------------------------------------------------------------------------
hr "Pass 1: grid test pattern (60s settle, uncommitted working copy)"
with_uncommitted_test_pattern grid
sleep 60
echo "slicer start-up log line:"
startup_log_line
sample_window "grid pattern - window 1"
sample_window "grid pattern - window 2"
sample_window "grid pattern - window 3"
clear_working_copy

# ---------------------------------------------------------------------------
# 4. Pass 2 - content
# ---------------------------------------------------------------------------
ORIGINAL_ACTIVE_APP=$(echo "$CONFIG_JSON" | jq -r '.activeApp // empty')
hr "Pass 2: content (committed activeApp was '$ORIGINAL_ACTIVE_APP')"
ACTIVE_STATE=$(curl -s "$API/status" | jq -r '.activeApp.state // empty')
echo "committed activeApp '$ORIGINAL_ACTIVE_APP' reports state '$ACTIVE_STATE'"
CONTENT_APP="$ORIGINAL_ACTIVE_APP"
if [[ "$ACTIVE_STATE" != "running" ]]; then
  # The committed app isn't actually producing frames (e.g. waiting on an
  # external dependency) - fall back to a simple always-on page so the
  # "content" pass measures a real render rather than a static/blank canvas.
  # Restored to $ORIGINAL_ACTIVE_APP at the end of this script.
  CONTENT_APP="testcard"
  echo "'$ORIGINAL_ACTIVE_APP' is not running; activating '$CONTENT_APP' instead for this pass"
  curl -s -X POST "$API/apps/$CONTENT_APP/activate" > /dev/null
  sleep 20
fi
echo "slicer start-up log line:"
startup_log_line
sample_window "content ($CONTENT_APP) - window 1"
sample_window "content ($CONTENT_APP) - window 2"
sample_window "content ($CONTENT_APP) - window 3"

# ---------------------------------------------------------------------------
# 5. Pass 3 - GPU-saturating page, if reachable
# ---------------------------------------------------------------------------
hr "Pass 3: GPU-saturating page"
SEASCAPE_URL="https://testpatterns.gameshow.pro/seascape/index.html"
if reachable "$SEASCAPE_URL"; then
  echo "seascape reachable; activating"
  curl -s -X POST "$API/apps/seascape/activate" > /dev/null
  sleep 20
  echo "slicer start-up log line:"
  startup_log_line
  sample_window "gpu-saturating (seascape) - window 1"
  sample_window "gpu-saturating (seascape) - window 2"
  sample_window "gpu-saturating (seascape) - window 3"
else
  echo "seascape page ($SEASCAPE_URL) is not reachable from $(hostname) - skipping this pass"
fi

# ---------------------------------------------------------------------------
# 6. Restore
# ---------------------------------------------------------------------------
hr "Restoring committed activeApp"
if [[ -n "$ORIGINAL_ACTIVE_APP" ]]; then
  curl -s -X POST "$API/apps/$ORIGINAL_ACTIVE_APP/activate" > /dev/null
  echo "activated '$ORIGINAL_ACTIVE_APP'"
fi
curl -s http://localhost:9088/api/v1/status
echo

hr "Done"
REMOTE_SCRIPT
