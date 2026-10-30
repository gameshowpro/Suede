#!/usr/bin/env bash
# End-to-end check that a 1:1 slice reaches each output pixel for pixel, on a
# real appliance: Suede's `coordinates` test pattern (static variant) and the
# same encoding drawn by Chromium (browser variant), each captured per output
# with grim and compared byte for byte by scripts/exact-mapping/compare_capture.py.
#
# Usage: scripts/verify-exact-mapping.sh <host>   (an ssh destination)
#
# Environment (all optional):
#   EXACT_PYTHON         remote python with numpy and Pillow (default python3;
#                        a leading ~ expands on the host)
#   EXACT_VARIANTS       "static browser" (default), or either one
#   EXACT_BROWSER_FLAGS  browser flag sets to run, from a b c d e (default
#                        "a b c"; d then runs only if a, b and c all mismatch):
#                          a  the launcher preset as is
#                          b  extraArgs --force-color-profile=srgb
#                          c  extraArgs --disable-gpu-rasterization
#                          d  extraArgs --disable-gpu-compositing (diagnostic)
#                          e  extraArgs --disable-accelerated-2d-canvas
#                             (diagnostic: is a mismatch specific to <canvas>?)
#   EXACT_OUT            local directory to copy captures and JSON reports to
#
# Everything runs over `ssh <host>` against the daemon's local API
# (http://localhost:9088). See scripts/exact-mapping/README.md.
#
# Idempotent, like scripts/profile-projection.sh, and stricter about it: every
# change is an *uncommitted* working copy (a full-document PUT without
# `committed: true`), including the temporary browser app and its activation,
# so nothing is ever persisted; POST /api/v1/config/revert at the end (from an
# EXIT trap, so also after a failure or an interrupt) brings back the saved
# document, its activeApp and its app list. The script refuses to start while
# someone else's working copy is live, and it ends by asserting that
# GET /api/v1/config is byte-identical to what it read at the start.
#
# Attribution: grim reads each output through the compositor's screencopy,
# not the scanout buffer. Normal-transform outputs are compared first, as the
# control; if one of them mismatches, the capture path is suspect and the
# rotated outputs' mismatches are reported INCONCLUSIVE, not FAIL. The
# static pattern is uploaded into the slicer directly, so it says nothing
# about the capture of the source output. Each browser run therefore also
# captures the source output itself (SOURCE lines, context rather than a
# verdict): a mismatch there is upstream of Suede's capture, in the browser
# or the compositor.
#
# Exit status: 0 only if every run had zero mismatches on every output and
# the restore verified; 1 otherwise; 2 when a precondition stopped the run
# before anything was changed.
set -uo pipefail

HOST="${1:?usage: scripts/verify-exact-mapping.sh <ssh host>}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
HERE="$ROOT/scripts/exact-mapping"
OUT="${EXACT_OUT:-}"

for file in compare_capture.py coordinates.html; do
  [[ -f "$HERE/$file" ]] || { echo "missing $HERE/$file" >&2; exit 2; }
done

REMOTE_DIR=$(ssh "$HOST" 'mktemp -d /tmp/suede-exact-mapping.XXXXXX') || exit 2
cleanup_local() {
  if [[ -n "$OUT" ]]; then
    mkdir -p "$OUT"
    scp -rq "$HOST:$REMOTE_DIR/results/." "$OUT/" 2>/dev/null \
      && echo "captures and reports copied to $OUT"
  fi
  ssh "$HOST" "rm -rf '$REMOTE_DIR'" 2>/dev/null
}
trap cleanup_local EXIT
scp -q "$HERE/compare_capture.py" "$HERE/coordinates.html" "$HOST:$REMOTE_DIR/" || exit 2

KEEP=0
[[ -n "$OUT" ]] && KEEP=1
ssh "$HOST" "EXACT_PYTHON=$(printf %q "${EXACT_PYTHON:-python3}") \
  EXACT_VARIANTS=$(printf %q "${EXACT_VARIANTS:-static browser}") \
  EXACT_BROWSER_FLAGS=$(printf %q "${EXACT_BROWSER_FLAGS:-}") \
  EXACT_KEEP=$KEEP bash -s -- '$REMOTE_DIR'" <<'REMOTE_SCRIPT'
set -uo pipefail
WORK="$1"
API=http://localhost:9088/api/v1
PY="${EXACT_PYTHON/#\~/$HOME}"
VARIANTS="$EXACT_VARIANTS"
FLAGSETS="${EXACT_BROWSER_FLAGS:-a b c}"
AUTO_D=0
[[ -z "$EXACT_BROWSER_FLAGS" ]] && AUTO_D=1
mkdir -p "$WORK/results" "$WORK/www"
mv "$WORK/coordinates.html" "$WORK/www/"

hr() { printf '\n=== %s ===\n' "$1"; }

APPLIED=0       # a working copy of ours may be live
HTTP_PID=""
TEMP_APPS=()    # ids of temporary apps we put in a working copy
NEW_PROFILES=() # profile directories that did not exist before this run
OVERALL=0
SUMMARY=()

# ---------------------------------------------------------------------------
# Restore: runs on every exit path once the first change may have been made.
# ---------------------------------------------------------------------------
restore() {
  local rc=$?
  trap - EXIT INT TERM HUP PIPE
  hr "Restore"
  local failed=0
  if (( APPLIED )); then
    local code
    code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$API/config/revert")
    echo "POST /config/revert -> $code"
    [[ "$code" == 200 ]] || failed=1
  fi
  if [[ -n "$HTTP_PID" ]]; then
    kill "$HTTP_PID" 2>/dev/null
    wait "$HTTP_PID" 2>/dev/null
    echo "stopped the temporary http server (pid $HTTP_PID)"
  fi

  if (( APPLIED )) && [[ -n "${ORIGINAL_ACTIVE:-}" ]]; then
    local state="" i
    for i in $(seq 1 120); do
      state=$(curl -s "$API/apps/$ORIGINAL_ACTIVE/status" | jq -r '.state // empty')
      [[ "$state" == running ]] && break
      sleep 1
    done
    echo "activeApp '$ORIGINAL_ACTIVE' state: ${state:-unknown} (was '${ORIGINAL_STATE:-unknown}' before the run)"
    if [[ "$state" != "${ORIGINAL_STATE:-running}" ]]; then
      echo "WARNING: '$ORIGINAL_ACTIVE' did not return to its earlier state within 120 s"
    fi
  fi

  local id
  for id in "${TEMP_APPS[@]}"; do
    local i
    for i in $(seq 1 30); do
      [[ "$(curl -s -o /dev/null -w '%{http_code}' "$API/apps/$id/status")" == 404 ]] && break
      sleep 1
    done
  done
  local dir
  for dir in "${NEW_PROFILES[@]}"; do
    # Only a directory this run caused to exist, and only under Suede's own
    # profile root with the probe's id prefix.
    case "$dir" in
      "$HOME"/.local/state/suede/profiles/exact-mapping-probe-?)
        if [[ -d "$dir" ]]; then rm -rf -- "$dir" && echo "removed temporary profile $dir"; fi ;;
      *) echo "WARNING: not removing unexpected profile path $dir" ;;
    esac
  done

  if (( ! APPLIED )); then
    echo "nothing was changed on this appliance"
  else
    curl -s -D "$WORK/after.headers" -o "$WORK/after.json" "$API/config"
    if cmp -s "$WORK/before.json" "$WORK/after.json"; then
      echo "RESTORE OK: GET /config is byte-identical to the start ($(sha256sum < "$WORK/after.json" | cut -c1-16), revision $(jq -r .revision "$WORK/after.json"), committed $(jq -r .committed "$WORK/after.json"))"
    else
      echo "RESTORE FAILED: GET /config differs from the start"
      diff <(jq -S . "$WORK/before.json") <(jq -S . "$WORK/after.json") | head -40
      failed=1
    fi
    local after_apps after_active
    after_apps=$(curl -s "$API/apps" | jq -c '[.[].id] | sort')
    after_active=$(jq -r '.activeApp // empty' "$WORK/after.json")
    if [[ "$after_apps" == "$BEFORE_APPS" ]]; then
      echo "RESTORE OK: app list identical ($(jq length <<< "$after_apps") apps)"
    else
      echo "RESTORE FAILED: app list was $BEFORE_APPS, now $after_apps"; failed=1
    fi
    if [[ "$after_active" == "$ORIGINAL_ACTIVE" ]]; then
      echo "RESTORE OK: activeApp '$after_active'"
    else
      echo "RESTORE FAILED: activeApp was '$ORIGINAL_ACTIVE', now '$after_active'"; failed=1
    fi
  fi

  if (( ${#SUMMARY[@]} )); then
    hr "Summary"
    printf '%s\n' "${SUMMARY[@]}"
  fi
  if [[ "${EXACT_KEEP:-0}" != 1 ]]; then rm -rf -- "$WORK"; fi

  if (( rc >= 128 )); then exit "$rc"; fi
  if (( rc == 2 && ! APPLIED )); then exit 2; fi
  if (( failed || OVERALL || rc )); then exit 1; fi
  exit 0
}
trap restore EXIT
trap 'exit 130' INT TERM HUP PIPE

die() { echo "ERROR: $*" >&2; exit 2; }

# ---------------------------------------------------------------------------
# Preconditions (nothing has been changed yet)
# ---------------------------------------------------------------------------
hr "Preconditions"
command -v jq >/dev/null || die "jq not found"
command -v grim >/dev/null || die "grim not found"
"$PY" -I -c 'import numpy, PIL' 2>/dev/null \
  || die "$PY lacks numpy or Pillow; set EXACT_PYTHON to one that has both"
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
WAYLAND_DISPLAY=""
for sock in "$XDG_RUNTIME_DIR"/wayland-*; do
  [[ "$sock" == *.lock ]] && continue
  [[ -S "$sock" ]] && WAYLAND_DISPLAY=$(basename "$sock") && break
done
[[ -n "$WAYLAND_DISPLAY" ]] || die "no wayland socket under $XDG_RUNTIME_DIR"
export WAYLAND_DISPLAY
# The headless output the browser renders into, if the compositor has one.
SOURCE_OUTPUT=""
for sock in "$XDG_RUNTIME_DIR"/sway-ipc.*.sock; do
  [[ -S "$sock" ]] || continue
  SOURCE_OUTPUT=$(SWAYSOCK="$sock" swaymsg -t get_outputs -r 2>/dev/null \
    | jq -r '[.[] | select(.name | startswith("HEADLESS"))][0].name // empty')
  [[ -n "$SOURCE_OUTPUT" ]] && break
done
echo "build: $(curl -s "$API/system" | jq -r '.buildId // "unknown"'); grim: $(dpkg-query -W -f '${Version}' grim 2>/dev/null || echo unknown); wayland: $WAYLAND_DISPLAY; python: $PY"

curl -s -D "$WORK/before.headers" -o "$WORK/before.json" "$API/config" || die "API unreachable"
[[ "$(jq -r .committed "$WORK/before.json")" == true ]] \
  || die "a working copy is live (committed: false); someone is editing - not touching it"
[[ "$(jq -r '.projection.temporary.highlightOverlaps // false' "$WORK/before.json")" == false ]] \
  || die "highlightOverlaps is on in a live working copy; not touching it"
BEFORE_APPS=$(curl -s "$API/apps" | jq -c '[.[].id] | sort')
ORIGINAL_ACTIVE=$(jq -r '.activeApp // empty' "$WORK/before.json")
ORIGINAL_STATE=""
[[ -n "$ORIGINAL_ACTIVE" ]] && ORIGINAL_STATE=$(curl -s "$API/apps/$ORIGINAL_ACTIVE/status" | jq -r '.state // empty')
echo "committed revision $(jq -r .revision "$WORK/before.json"), activeApp '${ORIGINAL_ACTIVE}' ($ORIGINAL_STATE), $(jq length <<< "$BEFORE_APPS") apps"

read -r CANVAS_W ASPECT < <(jq -r '.projection.canvas | "\(.renderWidth) \(.aspect)"' "$WORK/before.json")
[[ "$CANVAS_W" =~ ^[0-9]+$ ]] || die "the committed document has no projection.canvas"
# H = max(1, round(W / aspect)), half-way rounded up (docs/configuration.md).
CANVAS_H=$(jq -n --argjson w "$CANVAS_W" --argjson a "$ASPECT" '[1, ($w / $a + 0.5 | floor)] | max')
read -r ROWS COLUMNS < <(jq -r '.projection.arrangement | "\(.rows) \(.columns)"' "$WORK/before.json")
[[ "$ROWS" =~ ^[0-9]+$ && "$COLUMNS" =~ ^[0-9]+$ ]] \
  || die "the committed document has no projection.arrangement to derive 1:1 slices from"
echo "canvas ${CANVAS_W}x${CANVAS_H} (aspect $ASPECT), arrangement ${ROWS}x${COLUMNS}"

# The 1:1 placement: the arrangement solve at content scale 1 (a dry run).
curl -s -o "$WORK/arrangement.json" -w '' \
  "$API/projection/arrangement?rows=$ROWS&columns=$COLUMNS&contentScale=1"
jq -e '.outputs | length > 0' "$WORK/arrangement.json" >/dev/null \
  || die "the 1:1 arrangement dry run failed: $(cat "$WORK/arrangement.json")"

# One line per enabled output: name transform originX originY width height,
# normal-transform outputs first (they are the control). Each slice must be
# an integral pixel rect exactly the size of the output's logical raster.
jq -r --slurpfile arr "$WORK/arrangement.json" --argjson W "$CANVAS_W" \
      --argjson H "$CANVAS_H" --argjson A "$ASPECT" '
  ($arr[0].outputs | map({key: .key, value: .slice}) | from_entries) as $s
  | [ .outputs[] | select(.enable != false) | (.match.name // empty) as $n
      | (.transform // "normal") as $t
      | (.mode // .adopted.mode) as $m
      | (if ($t | test("90|270")) then [$m.height, $m.width] else [$m.width, $m.height] end) as $lr
      | $s[$n] as $sl
      | [$W * $sl.x, $A * $H * $sl.y, $W * $sl.width, $A * $H * $sl.height] as $px
      | ($px | map(. + 0.5 | floor)) as $r
      | {n: $n, t: $t, r: $r, lr: $lr, scale: (.scale // 1),
         integral: ([range(4)] | all(. as $i | (($px[$i] - $r[$i]) | fabs) < 0.001))} ]
  | sort_by(if .t == "normal" then 0 else 1 end)[]
  | "\(.n) \(.t) \(.r[0]) \(.r[1]) \(.r[2]) \(.r[3]) \(.lr[0]) \(.lr[1]) \(.scale) \(.integral)"
' "$WORK/before.json" > "$WORK/outputs.txt"
[[ -s "$WORK/outputs.txt" ]] || die "no enabled output with a match.name"
while read -r name t ox oy w h lw lh scale integral; do
  echo "  $name transform $t: slice $w x $h at ($ox,$oy), logical raster $lw x $lh, scale $scale"
  [[ "$integral" == true ]] || die "$name's 1:1 slice is not an integral pixel rect"
  [[ "$w" == "$lw" && "$h" == "$lh" ]] || die "$name's slice is ${w}x${h}, its raster ${lw}x${lh}: not 1:1"
  [[ "$scale" == 1 || "$scale" == 1.0 ]] || die "$name has output scale $scale"
done < "$WORK/outputs.txt"
NAMES=$(awk '{print $1}' "$WORK/outputs.txt" | jq -R . | jq -sc .)
# Every output under test must be attached: with one gone the slicer is
# down, its last control report goes stale, and grim cannot see the output.
MISSING=$(curl -s "$API/status" | jq -r --argjson names "$NAMES" \
  '[.divergences[]? | select(.kind == "output_not_connected" and (.subject as $s | $names | index($s))) | .subject] | join(" ")')
[[ -z "$MISSING" ]] || die "not connected: $MISSING (GET /api/v1/status divergences output_not_connected)"
for name in $(jq -r '.[]' <<< "$NAMES"); do
  grim -o "$name" -t png /dev/null 2>/dev/null < /dev/null \
    || die "grim cannot capture $name on $WAYLAND_DISPLAY"
done

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------
# Build and apply an uncommitted working copy from the saved document.
#   $1: test pattern ("" for none), $2: temporary app JSON ("" for none)
apply_working_copy() {
  local pattern="$1" app="${2:-null}"
  [[ -z "$app" ]] && app=null
  jq --slurpfile arr "$WORK/arrangement.json" --arg pattern "$pattern" --argjson app "$app" '
    ($arr[0].outputs | map({key: .key, value: .slice}) | from_entries) as $s
    | del(.revision) | .committed = false
    | .projection.mode = "simple" | .projection.blend = false | .projection.blackLift = 0
    | (if $pattern == "" then del(.projection.testPattern) else .projection.testPattern = $pattern end)
    | .outputs |= map((.match.name // "") as $n | if $s[$n] then .geometry.slice = $s[$n] else . end)
    | (if $app == null then . else .apps += [$app] | .activeApp = $app.id end)
  ' "$WORK/before.json" > "$WORK/working-copy.json"
  APPLIED=1
  local code
  code=$(curl -s -D "$WORK/put.headers" -o "$WORK/put.json" -w '%{http_code}' -X PUT \
    -H 'Content-Type: application/json' --data @"$WORK/working-copy.json" "$API/config")
  if [[ "$code" != 200 ]]; then
    echo "PUT /config (uncommitted) -> $code: $(head -c 600 "$WORK/put.json")"
    return 1
  fi
  GENERATION=$(tr -d '\r' < "$WORK/put.headers" | awk 'tolower($1) == "x-config-generation:" {print $2}')
  echo "working copy applied (uncommitted), generation $GENERATION"
}

# Wait until the slicer has installed the working copy, every output reports
# samplingMode exact, and has presented a frame at or after that install.
wait_installed() {
  local generation="$1" i stats
  for i in $(seq 1 120); do
    stats=$(curl -s "$API/projection/stats")
    if jq -e --argjson g "$generation" --argjson names "$NAMES" '
        .control as $c
        | (($c.configGeneration.applied // -1) >= $g)
          and ([$c.outputs[]? | select(.name as $n | $names | index($n))] | length) == ($names | length)
          and ([$c.outputs[] | select(.name as $n | $names | index($n))]
               | all(.samplingMode == "exact"
                     and ((.presentedGeneration // -1) >= ($c.childGeneration.applied // 0))))
      ' <<< "$stats" >/dev/null; then
      echo "installed: configGeneration.applied $(jq -c .control.configGeneration.applied <<< "$stats"); $(jq -c '[.control.outputs[] | "\(.name)=\(.samplingMode) presented \(.presentedGeneration)"]' <<< "$stats")"
      return 0
    fi
    sleep 0.5
  done
  echo "NOT INSTALLED within 60 s: $(jq -c '{configGeneration: .control.configGeneration, childGeneration: .control.childGeneration, outputs: .control.outputs}' <<< "$stats")"
  return 1
}

# Capture the source output the browser renders into (the compositor's
# headless output) and compare it with the whole canvas. Context, not a
# verdict: it does not change the exit status. $1: run label.
capture_source() {
  local label="$1" png="$WORK/results/$1-source.png" line
  if [[ -z "$SOURCE_OUTPUT" ]]; then
    echo "SOURCE $label: no headless output found; not captured"
    return
  fi
  if ! grim -o "$SOURCE_OUTPUT" -t png "$png" 2>"$WORK/grim.err" < /dev/null; then
    line="SOURCE $SOURCE_OUTPUT $label: grim failed: $(cat "$WORK/grim.err")"
  else
    line=$("$PY" -I "$WORK/compare_capture.py" "$png" --name "$SOURCE_OUTPUT" \
      --label "$label-source" --origin 0 0 --size "$CANVAS_W" "$CANVAS_H" \
      --transform normal --json "$WORK/results/$label-source.json" < /dev/null) \
      && rm -f "$png"
    line="SOURCE $line"
  fi
  echo "$line"
  SUMMARY+=("$line")
}

# Capture every output and compare it. $1: run label.
capture_and_compare() {
  local label="$1" control_failed=0 run_failed=0 line status
  while read -r name t ox oy w h _; do
    local png="$WORK/results/$label-$name.png"
    if ! grim -o "$name" -t png "$png" 2>"$WORK/grim.err" < /dev/null; then
      line="FAIL $name ($t) $label: grim failed: $(cat "$WORK/grim.err")"
      status=1
    else
      line=$("$PY" -I "$WORK/compare_capture.py" "$png" --name "$name" --label "$label" \
        --origin "$ox" "$oy" --size "$w" "$h" --transform "$t" \
        --json "$WORK/results/$label-$name.json" < /dev/null)
      status=$?
    fi
    if (( status != 0 )); then
      run_failed=1
      if [[ "$t" == normal ]]; then
        control_failed=1
      elif (( control_failed )); then
        line="INCONCLUSIVE${line#FAIL} [the normal-transform control failed: capture path suspect]"
      fi
    else
      # A matching capture needs no archive copy; the JSON report stays.
      rm -f "$png"
    fi
    echo "$line"
    SUMMARY+=("$line")
  done < "$WORK/outputs.txt"
  local stats
  stats=$(curl -s "$API/projection/stats")
  echo "context (not a verdict): samplingMode $(jq -c '[.control.outputs[] | "\(.name)=\(.samplingMode)"]' <<< "$stats"); lastInterval $(jq -c '[.lastInterval.outputs[]? | "\(.name) zeroCopy \(.zeroCopyPresented)/\(.presented)"]' <<< "$stats")"
  if (( control_failed )); then
    echo "CONTROL FAILED for $label: a normal-transform output mismatched, so grim's capture path is suspect; rotated results are not attributed to the slicer"
    SUMMARY+=("CONTROL FAILED $label")
  fi
  (( run_failed )) && OVERALL=1
  return "$run_failed"
}

# ---------------------------------------------------------------------------
# Static variant: Suede's own coordinates pattern
# ---------------------------------------------------------------------------
if [[ " $VARIANTS " == *" static "* ]]; then
  hr "Static variant: testPattern coordinates"
  if apply_working_copy coordinates "" && wait_installed "$GENERATION"; then
    sleep 2
    capture_and_compare static
  else
    SUMMARY+=("FAIL static: the working copy did not install with samplingMode exact")
    OVERALL=1
  fi
fi

# ---------------------------------------------------------------------------
# Browser variant: the same encoding drawn by Chromium
# ---------------------------------------------------------------------------
start_http_server() {
  local port
  for port in $(seq 18731 18799); do
    ss -Hltn "sport = :$port" | grep -q . && continue
    (cd / && exec python3 -I -m http.server --bind 127.0.0.1 --directory "$WORK/www" "$port") \
      >> "$WORK/http.log" 2>&1 < /dev/null &
    HTTP_PID=$!
    local i
    for i in $(seq 1 20); do
      if [[ "$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$port/coordinates.html")" == 200 ]]; then
        PAGE_URL="http://127.0.0.1:$port/coordinates.html"
        echo "serving $WORK/www at $PAGE_URL (pid $HTTP_PID)"
        return 0
      fi
      sleep 0.25
    done
    kill "$HTTP_PID" 2>/dev/null; wait "$HTTP_PID" 2>/dev/null; HTTP_PID=""
  done
  return 1
}

flag_args() {
  case "$1" in
    a) echo '[]' ;;
    b) echo '["--force-color-profile=srgb"]' ;;
    c) echo '["--disable-gpu-rasterization"]' ;;
    d) echo '["--disable-gpu-compositing"]' ;;
    e) echo '["--disable-accelerated-2d-canvas"]' ;;
  esac
}

browser_run() {
  local set="$1" id="exact-mapping-probe-$1" args app profile
  args=$(flag_args "$set")
  hr "Browser variant, flag set $set: extraArgs $args"
  app=$(jq -nc --arg id "$id" --arg uri "$PAGE_URL" --argjson args "$args" \
    '{id: $id, launcher: {kind: "chromium-kiosk", uri: $uri, extraArgs: $args}, persistProfile: false}')
  local preview
  preview=$(curl -s -X POST -H 'Content-Type: application/json' --data "$app" "$API/apps/preview")
  profile=$(jq -r '.profileDir // empty' <<< "$preview")
  echo "launch preview: program $(jq -r '.program // "none"' <<< "$preview"), profile ${profile:-none}"
  if [[ -n "$profile" && ! -e "$profile" ]]; then NEW_PROFILES+=("$profile"); fi
  TEMP_APPS+=("$id")
  : > "$WORK/http.log"

  if ! apply_working_copy "" "$app"; then
    SUMMARY+=("FAIL browser-$set: the working copy was refused"); OVERALL=1; return 1
  fi
  local state="" i
  for i in $(seq 1 90); do
    state=$(curl -s "$API/apps/$id/status" | jq -r '.state // empty')
    [[ "$state" == running ]] && break
    sleep 1
  done
  echo "app $id: ${state:-unknown}"
  local report=""
  for i in $(seq 1 120); do
    report=$(grep -ao 'GET /report?[^ ]*' "$WORK/http.log" | tail -1)
    [[ -n "$report" ]] && break
    sleep 0.5
  done
  if [[ -z "$report" ]]; then
    echo "the page never reported a draw"
    SUMMARY+=("FAIL browser-$set: the page never drew (app state ${state:-unknown})"); OVERALL=1
    return 1
  fi
  # Let any resize-triggered redraw land, then read the latest report.
  sleep 3
  report=$(grep -ao 'GET /report?[^ ]*' "$WORK/http.log" | tail -1)
  echo "page report: ${report#GET /report?}"
  local vw vh dpr
  vw=$(sed -E 's/.*[?&]w=([0-9]+).*/\1/' <<< "$report")
  vh=$(sed -E 's/.*[?&]h=([0-9]+).*/\1/' <<< "$report")
  dpr=$(sed -E 's/.*[?&]dpr=([0-9.]+).*/\1/' <<< "$report")
  if [[ "$vw" != "$CANVAS_W" || "$vh" != "$CANVAS_H" || "$dpr" != 1 ]]; then
    echo "WARNING: viewport ${vw}x${vh} at devicePixelRatio $dpr, canvas is ${CANVAS_W}x${CANVAS_H}: page pixels are not canvas pixels"
  fi
  if ! wait_installed "$GENERATION"; then
    SUMMARY+=("FAIL browser-$set: not installed with samplingMode exact"); OVERALL=1; return 1
  fi
  sleep 5
  capture_source "browser-$set"
  capture_and_compare "browser-$set"
}

if [[ " $VARIANTS " == *" browser "* ]]; then
  hr "Browser variant"
  start_http_server || die "could not start a local http server"
  any_pass=0
  for set in $FLAGSETS; do
    [[ -n "$(flag_args "$set")" ]] || { echo "unknown flag set '$set'"; continue; }
    if browser_run "$set"; then any_pass=1; fi
  done
  if (( AUTO_D && ! any_pass )); then
    echo "flag sets a, b and c all mismatched: running d (--disable-gpu-compositing) as a diagnostic"
    browser_run d
  fi
fi
REMOTE_SCRIPT
