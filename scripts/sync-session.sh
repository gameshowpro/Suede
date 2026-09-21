#!/usr/bin/env bash
# Guided camera session for measuring output-to-output sync on a projector
# wall, from the machine you are standing at rather than from a list of
# curl commands.
#
# Usage: scripts/sync-session.sh [host]     (default: workshop)
#
# Each take sets a condition, waits for it to settle, tells you when to roll
# the camera, samples GET /projection/stats throughout, and writes one line
# per event to a local manifest. The `sync` test pattern draws the same UTC
# clock on every output, so a frame of video and a line of the manifest can
# be matched afterwards without anyone writing anything down.
#
# The manifest is the deliverable alongside the footage: send both.
set -uo pipefail

HOST="${1:-workshop}"
API="http://localhost:9088/api/v1"

SESSION_DIR="sync-session/$(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p "$SESSION_DIR"
MANIFEST="$SESSION_DIR/manifest.jsonl"

BOLD=$'\e[1m'; DIM=$'\e[2m'; RED=$'\e[31m'; GREEN=$'\e[32m'; YELLOW=$'\e[33m'; OFF=$'\e[0m'

# -n is load-bearing: without it ssh swallows the script's own stdin and the
# menu spins reading nothing. Nothing here ever pipes into ssh.
remote() { ssh -n -o BatchMode=yes -o ConnectTimeout=10 "$HOST" "$@"; }
now_utc() { date -u +%Y-%m-%dT%H:%M:%S.%3NZ; }

# One JSON object per line. `jq -c -n` so a note containing a quote cannot
# corrupt the file the analysis depends on.
record() {
  local kind="$1"
  local extra="${2:-}"
  [ -z "$extra" ] && extra='{}'
  jq -c -n --arg kind "$kind" --arg at "$(now_utc)" --argjson extra "$extra" \
    '{kind:$kind, atUtc:$at} + $extra' >> "$MANIFEST"
}

stats_json() { remote "curl -s $API/projection/stats"; }

# --- condition control ------------------------------------------------------

# Whether the *running* compositor is scanning out, which is the only honest
# answer: the toml says what was asked for, the process says what is.
current_arm() {
  remote 'pid=$(pgrep -x sway | head -1); if [ -n "$pid" ] && tr "\0" "\n" < /proc/$pid/environ | grep -q "^WLR_SCENE_DISABLE_DIRECT_SCANOUT="; then echo composited; else echo scanout; fi'
}

set_app() {
  local app="$1"
  remote "curl -s -o /dev/null -X POST $API/apps/$app/activate"
}

show_pattern() {
  remote "curl -s $API/config | jq 'del(.revision) | .committed=false | .projection.testPattern=\"sync\"' | curl -s -o /dev/null -X PUT $API/config -H 'Content-Type: application/json' -d @-"
}

hide_pattern() { remote "curl -s -o /dev/null -X POST $API/config/revert"; }

align_heads() {
  echo "${YELLOW}Aligning the heads. The wall goes dark for a second or two.${OFF}"
  remote "curl -s -X POST $API/system/checks/output-phase/fix" | jq -r '.detail // .' 2>/dev/null
  echo "Waiting 25 s for the next slicer interval to report the result..."
  sleep 25
  phase_line
}

# --- reporting --------------------------------------------------------------

phase_line() {
  stats_json | jq -r '
    if .lastInterval == null then "  (no interval yet - is an app active?)"
    else .lastInterval as $s
      | "  phase: " + ([$s.outputs[] | "\(.name) \(.phaseMs * 100 | round / 100)ms"] | join("  "))
    end' 2>/dev/null || echo "  (stats unavailable)"
}

status() {
  local arm; arm=$(current_arm)
  echo
  echo "${BOLD}Host${OFF} $HOST   ${BOLD}Arm${OFF} $(
    [ "$arm" = scanout ] && echo "${GREEN}DIRECT SCANOUT${OFF}" || echo "${YELLOW}COMPOSITED${OFF}")"
  remote "curl -s $API/system" | jq -r '"  suede \(.suedeVersion) (\(.buildId))   sway \(.swayVersion)"'
  remote "curl -s $API/status" | jq -r '"  active app: \(.activeApp.id // "none") (\(.activeApp.state // "-"))"'
  local failing
  failing=$(remote "curl -s $API/system/checks" | jq -r '[.[] | select(.status != "pass") | "\(.id)=\(.status)"] | join(", ")')
  if [ -n "$failing" ]; then echo "  ${RED}checks: $failing${OFF}"; else echo "  ${GREEN}checks: all pass${OFF}"; fi
  stats_json | jq -r '
    if .lastInterval == null then "  slicer: no interval (no app active, or just restarted)"
    else .lastInterval as $s
      | "  last interval: \($s.presentedFps | floor) fps, straddles \($s.straddles), gate holds \($s.gateHolds)"
        + ", zero-copy " + (if ([$s.outputs[] | .zeroCopyPresented == .presented] | all) then "yes" else "no" end)
    end' 2>/dev/null
  phase_line
  echo "  session: $SESSION_DIR"
  echo
}

# Aggregate what the stats said across a take, so the operator knows whether
# the clip is worth keeping before moving on.
take_summary() {
  local from="$1"
  jq -s -r --arg from "$from" '
    [.[] | select(.kind=="sample" and .atUtc >= $from) | .stats.lastInterval | select(. != null)] as $s
    | if ($s | length) == 0 then "  no intervals captured - the take may have been shorter than 10 s"
      else
        "  intervals " + ($s | length | tostring)
        + " | fps " + (($s | map(.presentedFps) | add / length) | floor | tostring)
        + " | straddles " + ($s | map(.straddles) | add | tostring)
        + " | gate holds " + ($s | map(.gateHolds) | add | tostring)
        + " | superseded " + ($s | map(.framesSuperseded) | add | tostring)
        + "\n  lag>0 per output: "
        + ([$s[-1].outputs[] | .name] | join(" ")) + " -> "
        + ([range(0; ($s[-1].outputs | length)) as $i
            | ($s | map(.outputs[$i] | .lagFrames.one + .lagFrames.two + .lagFrames.more) | add | tostring)]
           | join(" "))
        + "\n  zero-copy: " + (if ($s | map(.outputs | map(.zeroCopyPresented == .presented) | all) | all) then "every frame" else "NOT every frame" end)
      end' "$MANIFEST"
}

# --- a take -----------------------------------------------------------------

run_take() {
  local name="$1" app="$2" pattern="$3" settle="${4:-20}"
  local arm; arm=$(current_arm)

  echo
  echo "${BOLD}Take: $name${OFF}  (arm: $arm, app: $app, pattern: $pattern)"
  set_app "$app"
  if [ "$pattern" = sync ]; then show_pattern; else hide_pattern; fi
  echo "Settling for ${settle} s..."
  sleep "$settle"
  status

  local failing
  failing=$(remote "curl -s $API/system/checks" | jq -r '[.[] | select(.status != "pass")] | length')
  if [ "${failing:-0}" != "0" ]; then
    echo "${RED}A health check is failing. Fix it (option a, or check the wall) before shooting.${OFF}"
    read -r -p "Continue anyway? [y/N] " go
    [ "$go" = y ] || return
  fi

  echo "${GREEN}${BOLD}READY.${OFF} Start the camera, then press Enter to mark the take's start."
  read -r
  local start; start=$(now_utc)
  record take_start "$(jq -c -n --arg n "$name" --arg a "$arm" --arg app "$app" --arg p "$pattern" \
    '{take:$n, arm:$a, app:$app, pattern:$p}')"
  echo "${BOLD}Recording from $start${OFF} - sampling stats every 5 s."
  echo "Press Enter when the camera stops."

  # Sample in the background until the operator says stop.
  (
    while :; do
      jq -c -n --arg at "$(now_utc)" --argjson s "$(stats_json)" \
        '{kind:"sample", atUtc:$at, stats:$s}' >> "$MANIFEST" 2>/dev/null
      sleep 5
    done
  ) &
  local sampler=$!
  read -r
  kill "$sampler" 2>/dev/null; wait "$sampler" 2>/dev/null

  record take_end "$(jq -c -n --arg n "$name" '{take:$n}')"
  echo "${BOLD}Take '$name' finished.${OFF} What the stats said:"
  take_summary "$start"
  echo
  read -r -p "Keep this take? [Y/n] " keep
  if [ "$keep" = n ]; then
    record take_discarded "$(jq -c -n --arg n "$name" '{take:$n}')"
    echo "Marked as discarded in the manifest - note the clip number so it can be skipped."
  fi
}

# --- arm switching ----------------------------------------------------------

switch_arm() {
  local target="$1"
  echo "${YELLOW}Switching to $target. sway restarts: the wall goes dark for about 20 s.${OFF}"
  read -r -p "Go ahead? [y/N] " go
  [ "$go" = y ] || return

  if [ "$target" = scanout ]; then
    remote "grep -q '^direct_scanout' ~/.config/suede/suede.toml && sed -i 's/^direct_scanout = .*/direct_scanout = true/' ~/.config/suede/suede.toml || echo 'direct_scanout = true' >> ~/.config/suede/suede.toml"
    remote "sed -i 's/^\([[:space:]]*\)export WLR_SCENE_DISABLE_DIRECT_SCANOUT=1/\1# export WLR_SCENE_DISABLE_DIRECT_SCANOUT=1/' ~/.bash_profile"
  else
    remote "grep -q '^direct_scanout' ~/.config/suede/suede.toml && sed -i 's/^direct_scanout = .*/direct_scanout = false/' ~/.config/suede/suede.toml || echo 'direct_scanout = false' >> ~/.config/suede/suede.toml"
    remote "sed -i 's/^\([[:space:]]*\)#[[:space:]]*export WLR_SCENE_DISABLE_DIRECT_SCANOUT=1/\1export WLR_SCENE_DISABLE_DIRECT_SCANOUT=1/' ~/.bash_profile"
  fi

  remote "sudo systemctl restart getty@tty1"
  echo "Waiting for sway..."; sleep 20
  remote "systemctl --user restart suede"; sleep 8

  local arm; arm=$(current_arm)
  if [ "$arm" != "$target" ]; then
    echo "${RED}The compositor came back as '$arm', not '$target'. Do not shoot until this is understood.${OFF}"
  else
    echo "${GREEN}Now in $arm.${OFF}"
  fi
  record arm_switch "$(jq -c -n --arg a "$arm" --arg want "$target" '{arm:$a, requested:$want}')"
  echo "A restart leaves the heads out of phase on this machine."
  read -r -p "Align them now? [Y/n] " al
  [ "$al" = n ] || align_heads
}

# --- menu -------------------------------------------------------------------

finish() {
  echo "Restoring the wall: pattern off, arena-fx active."
  hide_pattern; set_app arena-fx
  record session_end '{}'
  echo
  echo "${BOLD}Session complete.${OFF}"
  echo "Send these together:"
  echo "  - the video files, named so the take order is obvious"
  echo "  - $MANIFEST"
  echo
  exit 0
}

trap 'echo; echo "Interrupted - the manifest is intact at $MANIFEST"; exit 1' INT

record session_start "$(jq -c -n --arg h "$HOST" '{host:$h}')"
record machine "$(remote "curl -s $API/system")"

cat <<BANNER

${BOLD}Suede projector sync session${OFF}
Manifest: $MANIFEST

Shoot all four projectors in one frame, camera locked off - no pan, zoom or
refocus during or between takes. 240 fps or faster, manual exposure and
focus. The analysis reads the frame counter out of each quadrant, so the
framing must not change.

Suggested order (grouped by arm, because switching restarts sway):
  1. scanout / idle      2-3 min   the control: expect no disagreement at all
  2. scanout / loaded    30 s      events are ~4% here, so 30 s is plenty
  3. switch to composited, shoot loaded BEFORE aligning the heads   30 s
  4. align heads, composited / idle    2-3 min
  5. composited / loaded 30 s
BANNER

while :; do
  cat <<MENU

${BOLD}--------------------------------------------------${OFF}
  s) Status
  a) Align heads
  1) Take: ${BOLD}idle${OFF}      sync-test + sync pattern   (GPU ~34%)
  2) Take: ${BOLD}loaded${OFF}    seascape  + sync pattern   (GPU ~95%)
  3) Take: ${BOLD}browser${OFF}   sync-test as content, no pattern
        (the whole pipeline including Chromium, for comparison with 1)
  x) Switch arm
  n) Note
  q) Finish and pack up
MENU
  read -r -p "> " choice
  case "$choice" in
    s) status ;;
    a) align_heads ;;
    1) run_take "idle" sync-test sync ;;
    2) run_take "loaded" seascape sync 25 ;;
    3) run_take "browser-page" sync-test none ;;
    x)
      cur=$(current_arm)
      [ "$cur" = scanout ] && switch_arm composited || switch_arm scanout ;;
    n)
      read -r -p "note: " text
      record note "$(jq -c -n --arg t "$text" '{text:$t}')"
      echo "noted." ;;
    q) finish ;;
    *) echo "?" ;;
  esac
done
