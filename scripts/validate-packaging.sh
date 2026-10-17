#!/usr/bin/env bash
# Validate the things CI packaging depends on, without a full release build.
set -uo pipefail
cd "$(dirname "$0")/.."

FAIL=0
ok()   { echo "  PASS  $1"; }
bad()  { echo "  FAIL  $1"; FAIL=1; }

echo "Shell scripts parse"
for script in packaging/provision.sh packaging/postinst scripts/*.sh; do
  if bash -n "$script" 2>/dev/null || sh -n "$script" 2>/dev/null; then
    ok "$script"
  else
    bad "$script"
  fi
done

echo
echo "provision.sh --help lists every option it accepts"
HELP="$(bash packaging/provision.sh --help 2>&1)"
for option in $(grep -oE '^\s+--[a-z-]+\)' packaging/provision.sh | tr -d ' )'); do
  if grep -qF -- "$option" <<<"$HELP"; then ok "$option"; else bad "$option is undocumented"; fi
done

echo
echo "The login profile derives direct scanout from suede.toml"
# The block provision.sh writes into ~/.bash_profile reads the two bootstrap
# keys at every login, and on an appliance where sway is not a systemd unit it
# is the only thing that can follow the file — so it has to agree, case for
# case, with BootstrapConfig::scanout_expected in src/config.rs.
SCANOUT_SNIPPET="$(sed -n "/<<'SCANOUT_EOF'/,/^SCANOUT_EOF$/p" packaging/provision.sh | sed '1d;$d')"
[[ -n "$SCANOUT_SNIPPET" ]] || bad "could not find the SCANOUT_EOF block in provision.sh"
scanout_for() {  # $1 = suede.toml contents, empty for no file at all
  local home
  home="$(mktemp -d)"
  if [[ -n "$1" ]]; then
    mkdir -p "$home/.config/suede"
    printf '%s\n' "$1" > "$home/.config/suede/suede.toml"
  fi
  (
    HOME="$home"
    unset WLR_SCENE_DISABLE_DIRECT_SCANOUT
    eval "$SCANOUT_SNIPPET"
    echo "${WLR_SCENE_DISABLE_DIRECT_SCANOUT:-unset}"
  )
  rm -rf "$home"
}
expect_scanout() {  # $1 = label, $2 = toml, $3 = expected value of the variable
  local got
  got="$(scanout_for "$2")"
  if [[ "$got" == "$3" ]]; then ok "$1 -> $3"; else bad "$1 -> $got (expected $3)"; fi
}
# Anything but "the slicer owns every output and scanout was not turned off"
# exports the variable, because a spanning window scanned out mirrors.
expect_scanout "no suede.toml"                  ""                                             1
expect_scanout "allow_overlaps = false"         "allow_overlaps = false"                       1
expect_scanout "allow_overlaps commented out"   "# allow_overlaps = true"                      1
expect_scanout "direct_scanout = false alone"   "direct_scanout = false"                       1
expect_scanout "allow_overlaps = true"          "allow_overlaps = true"                        unset
expect_scanout "both keys true"                 "allow_overlaps = true
direct_scanout = true"                                                                         unset
expect_scanout "scanout turned off for the A/B" "allow_overlaps = true
direct_scanout = false"                                                                        1

echo
echo "The login profile chooses the direct-presentation session from suede.toml"
# The PRESENTATION_EOF block decides, at every login, whether sway starts
# headless-only for direct presentation or as the ordinary DRM session. It has
# to agree with suede::presentation::resolve (and BootstrapConfig's
# presentation_effective), and its runtime files are what the daemon reads.
PRESENTATION_SNIPPET="$(sed -n "/<<'PRESENTATION_EOF'/,/^PRESENTATION_EOF$/p" packaging/provision.sh | sed '1d;$d')"
[[ -n "$PRESENTATION_SNIPPET" ]] || bad "could not find the PRESENTATION_EOF block in provision.sh"
# $1 = suede.toml contents (empty for no file), $2 = previous session (empty
# for none), $3 = direct-attempts (empty for none), $4 = "marker" to start
# with a fallback marker. Prints: chosen session, WLR_BACKENDS,
# WLR_HEADLESS_OUTPUTS, the session file, the attempts file, whether a
# marker exists afterwards, and whether display-reset ran.
presentation_for() {
  local home runtime stub snippet
  home="$(mktemp -d)"
  runtime="$home/run"
  mkdir -p "$runtime/suede"
  if [[ -n "$1" ]]; then
    mkdir -p "$home/.config/suede"
    printf '%s\n' "$1" > "$home/.config/suede/suede.toml"
  fi
  [[ -n "${2:-}" ]] && printf '%s\n' "$2" > "$runtime/suede/session"
  [[ -n "${3:-}" ]] && printf '%s\n' "$3" > "$runtime/suede/direct-attempts"
  [[ "${4:-}" == "marker" ]] && echo '{"reason":"test","time":0,"bootId":""}' \
    > "$runtime/suede/presentation-fallback"
  # Never run the real binary from a test: it opens the DRM cards.
  stub="$home/suede-stub"
  printf '#!/bin/sh\necho "$@" >> "%s/reset.log"\n' "$home" > "$stub"
  chmod +x "$stub"
  snippet="${PRESENTATION_SNIPPET//\/usr\/bin\/suede/$stub}"
  (
    HOME="$home"
    XDG_RUNTIME_DIR="$runtime"
    unset WLR_BACKENDS WLR_HEADLESS_OUTPUTS suede_session
    eval "$snippet"
    printf '%s %s %s session=%s attempts=%s marker=%s reset=%s\n' \
      "$suede_session" "${WLR_BACKENDS:-unset}" "${WLR_HEADLESS_OUTPUTS:-unset}" \
      "$(cat "$runtime/suede/session" 2>/dev/null)" \
      "$(cat "$runtime/suede/direct-attempts" 2>/dev/null || echo none)" \
      "$([[ -e "$runtime/suede/presentation-fallback" ]] && echo yes || echo no)" \
      "$([[ -s "$home/reset.log" ]] && echo yes || echo no)"
  )
  rm -rf "$home"
}
expect_presentation() {  # $1 = label, $2 = expected line, then presentation_for's arguments
  local label="$1" expected="$2" got
  shift 2
  got="$(presentation_for "$@")"
  if [[ "$got" == "$expected" ]]; then ok "$label -> $got"; else bad "$label -> $got (expected $expected)"; fi
}
DIRECT_TOML='presentation = "direct"
allow_overlaps = true'
expect_presentation "no suede.toml" \
  "wayland unset unset session=wayland attempts=none marker=no reset=no" ""
expect_presentation "direct without allow_overlaps" \
  "wayland unset unset session=wayland attempts=none marker=no reset=no" 'presentation = "direct"'
expect_presentation "direct commented out" \
  "wayland unset unset session=wayland attempts=none marker=no reset=no" '# presentation = "direct"
allow_overlaps = true'
expect_presentation "presentation = wayland" \
  "wayland unset unset session=wayland attempts=none marker=no reset=no" 'presentation = "wayland"
allow_overlaps = true'
expect_presentation "direct with allow_overlaps" \
  "direct headless 1 session=direct attempts=1 marker=no reset=no" "$DIRECT_TOML"
expect_presentation "direct, second attempt" \
  "direct headless 1 session=direct attempts=2 marker=no reset=no" "$DIRECT_TOML" "" 1
expect_presentation "direct after a fallback this boot" \
  "wayland unset unset session=wayland attempts=none marker=yes reset=no" "$DIRECT_TOML" "" "" marker
expect_presentation "direct after 3 unconfirmed attempts" \
  "wayland unset unset session=wayland attempts=3 marker=yes reset=no" "$DIRECT_TOML" "" 3
expect_presentation "leaving a direct session resets the displays" \
  "wayland unset unset session=wayland attempts=none marker=yes reset=yes" "$DIRECT_TOML" direct "" marker
expect_presentation "a direct session after a direct session also resets" \
  "direct headless 1 session=direct attempts=2 marker=no reset=yes" "$DIRECT_TOML" direct 1
expect_presentation "leaving a wayland session does not reset" \
  "direct headless 1 session=direct attempts=1 marker=no reset=no" "$DIRECT_TOML" wayland
expect_presentation "a garbled attempts file counts as none" \
  "direct headless 1 session=direct attempts=1 marker=no reset=no" "$DIRECT_TOML" "" "x"
# The marker the profile writes must be one the daemon can read back.
MARKER_JSON="$(
  home="$(mktemp -d)"; mkdir -p "$home/.config/suede" "$home/run/suede"
  printf '%s\n' "$DIRECT_TOML" > "$home/.config/suede/suede.toml"
  echo 3 > "$home/run/suede/direct-attempts"
  ( HOME="$home"; XDG_RUNTIME_DIR="$home/run"; eval "$PRESENTATION_SNIPPET" )
  cat "$home/run/suede/presentation-fallback"; rm -rf "$home"
)"
if python3 -c 'import json,sys; m=json.loads(sys.argv[1]); assert m["reason"] and isinstance(m["time"], int) and "bootId" in m' "$MARKER_JSON" 2>/dev/null; then
  ok "the attempts marker is valid JSON with reason, time and bootId"
else
  bad "the attempts marker is not the JSON the daemon reads: $MARKER_JSON"
fi

echo
echo "The login profile picks the render node of the card that drives the displays"
# On a machine with more than one GPU (an integrated GPU beside the discrete
# card that drives the wall — System B's shape: card0/i915 with nothing
# connected, card1/nvidia with three connected outputs), the headless canvas
# must be allocated on the same device pick_direct_physical_device will later
# match by DRM primary node, or direct presentation finds no candidate GPU at
# all. The block reads SUEDE_DRM_SYSFS/SUEDE_DRM_DEV instead of the real
# /sys/class/drm and /dev/dri whenever they are set, which is only here.
RENDER_ROOT="$(mktemp -d)"
render_node_for() {  # each remaining arg is "card:connected-count[:renderD]"
  local drm dev home card_spec card connected render i
  rm -rf "$RENDER_ROOT"
  home="$RENDER_ROOT/home"
  drm="$RENDER_ROOT/drm"
  dev="$RENDER_ROOT/dev"
  mkdir -p "$drm" "$dev" "$home/.config/suede" "$home/run/suede"
  printf '%s\n' "$DIRECT_TOML" > "$home/.config/suede/suede.toml"
  for card_spec in "$@"; do
    IFS=':' read -r card connected render <<<"$card_spec"
    mkdir -p "$drm/$card"
    if [[ -n "$render" ]]; then
      mkdir -p "$drm/$card/device/drm/$render"
      : > "$dev/$render"
    fi
    i=0
    while [[ "$i" -lt "$connected" ]]; do
      mkdir -p "$drm/$card-DP-$i"
      echo connected > "$drm/$card-DP-$i/status"
      i=$((i + 1))
    done
    # Every card also has one disconnected connector, so "has a connector
    # directory at all" is never mistaken for "has a connected one".
    mkdir -p "$drm/$card-DP-idle"
    echo disconnected > "$drm/$card-DP-idle/status"
  done
  (
    HOME="$home"
    XDG_RUNTIME_DIR="$home/run"
    SUEDE_DRM_SYSFS="$drm"
    SUEDE_DRM_DEV="$dev"
    unset WLR_RENDER_DRM_DEVICE suede_session
    eval "$PRESENTATION_SNIPPET"
    echo "${WLR_RENDER_DRM_DEVICE:-unset}"
  )
}
expect_render_node() {  # $1 = label, $2 = expected WLR_RENDER_DRM_DEVICE, then card specs
  local label="$1" expected="$2" got
  shift 2
  got="$(render_node_for "$@")"
  if [[ "$got" == "$expected" ]]; then ok "$label -> $got"; else bad "$label -> $got (expected $expected)"; fi
}
expect_render_node "an idle iGPU beside the card that drives the wall (System B's shape)" \
  "$RENDER_ROOT/dev/renderD129" card0:0:renderD128 card1:3:renderD129
expect_render_node "the same, cards in the other order" \
  "$RENDER_ROOT/dev/renderD128" card0:3:renderD128 card1:0:renderD129
expect_render_node "a single card with nothing connected falls back to the first renderD*" \
  "$RENDER_ROOT/dev/renderD128" card0:0:renderD128
rm -rf "$RENDER_ROOT"

echo
echo "The login profile exports __GL_YIELD from suede.toml"
# The GL_YIELD_EOF block runs before both exec sway branches, so it has to
# agree with GlYieldMode::parse in src/config.rs about which values it acts
# on: absent (or commented out) and "usleep" export USLEEP, the recommended
# NVIDIA profile; "nothing" exports NOTHING; "default" leaves the variable
# unset, which is NVIDIA's own behavior. A typo exports nothing (the daemon
# refuses to start on it anyway). Mesa ignores the variable.
GL_YIELD_SNIPPET="$(sed -n "/<<'GL_YIELD_EOF'/,/^GL_YIELD_EOF$/p" packaging/provision.sh | sed '1d;$d')"
[[ -n "$GL_YIELD_SNIPPET" ]] || bad "could not find the GL_YIELD_EOF block in provision.sh"
gl_yield_for() {  # $1 = suede.toml contents, empty for no file at all
  local home
  home="$(mktemp -d)"
  if [[ -n "$1" ]]; then
    mkdir -p "$home/.config/suede"
    printf '%s\n' "$1" > "$home/.config/suede/suede.toml"
  fi
  (
    HOME="$home"
    unset __GL_YIELD
    eval "$GL_YIELD_SNIPPET"
    echo "${__GL_YIELD:-unset}"
  )
  rm -rf "$home"
}
expect_gl_yield() {  # $1 = label, $2 = toml, $3 = expected value of the variable
  local got
  got="$(gl_yield_for "$2")"
  if [[ "$got" == "$3" ]]; then ok "$1 -> $3"; else bad "$1 -> $got (expected $3)"; fi
}
expect_gl_yield "no suede.toml"           ""                     USLEEP
expect_gl_yield "gl_yield = \"default\""  'gl_yield = "default"'  unset
expect_gl_yield "gl_yield = \"usleep\""   'gl_yield = "usleep"'   USLEEP
expect_gl_yield "gl_yield = \"nothing\""  'gl_yield = "nothing"'  NOTHING
expect_gl_yield "gl_yield commented out"  '# gl_yield = "nothing"' USLEEP
expect_gl_yield "an unrecognized value"   'gl_yield = "busy"'     unset

echo
echo "Provisioning writes the API token and loopback bind only when asked"
# The BOOTSTRAP_FUNCTIONS block is the whole of what --token and --local-only
# do to suede.toml, plus the reboot decision. It runs here against a scratch home as the current user:
# no root, no firewall, no packages, nothing outside the temp directory. The
# no-terminal cases run with stdin from /dev/null; the prompt cases run under
# a pseudo-terminal so `[[ -t 0 ]]` is true, answering what the case says.
BOOTSTRAP_FUNCTIONS="$(sed -n '/^# BEGIN BOOTSTRAP_FUNCTIONS$/,/^# END BOOTSTRAP_FUNCTIONS$/p' packaging/provision.sh)"
[[ -n "$BOOTSTRAP_FUNCTIONS" ]] || bad "could not find the BOOTSTRAP_FUNCTIONS block in provision.sh"
SEC_HOME=""
SEC_OUT=""
SEC_TOML=""
# $1 = answers for the prompts, space separated ("-" for no terminal at all),
# $2 = TOKEN_MODE, $3 = LOCAL_ONLY ("", yes or no), $4 = existing suede.toml
# contents (empty for no file), $5 = REBOOT ("", yes or no). The transcript
# ends with REBOOT=yes or REBOOT=no, the decision, since nothing here reboots. Sets SEC_HOME (the scratch home), SEC_TOML and SEC_OUT (the
# transcript) for the caller to inspect; sec_done removes the scratch home.
run_security() {
  local answers="$1" mode="$2" local_only="$3" existing="$4" reboot="${5:-}"
  SEC_HOME="$(mktemp -d)"
  SEC_TOML="$SEC_HOME/.config/suede/suede.toml"
  if [[ -n "$existing" ]]; then
    mkdir -p "$SEC_HOME/.config/suede"
    printf '%s\n' "$existing" > "$SEC_TOML"
    chmod 644 "$SEC_TOML"
  fi
  export SEC_SCRIPT="$BOOTSTRAP_FUNCTIONS
SUEDE_TOML='$SEC_TOML'; APPLIANCE_USER=\"\$(id -un)\"; SUEDE_PORT=9088
TOKEN_MODE='$mode'; LOCAL_ONLY='$local_only'; REBOOT='$reboot'
configure_security
if reboot_wanted; then echo REBOOT=yes; else echo REBOOT=no; fi"
  if [[ "$answers" == "-" ]]; then
    SEC_OUT="$(bash -c "$SEC_SCRIPT" </dev/null 2>&1 | cat)"
  else
    SEC_OUT="$(python3 -I - "$answers" <<'PY'
import os, pty, select, sys
answers = sys.argv[1].split()
script = os.environ["SEC_SCRIPT"]
pid, fd = pty.fork()
if pid == 0:
    os.execvp("bash", ["bash", "-c", script])
out, answered = b"", 0
while True:
    ready, _, _ = select.select([fd], [], [], 10)
    if not ready:
        out += b"\n[timed out waiting for output]\n"
        break
    try:
        data = os.read(fd, 4096)
    except OSError:
        break
    if not data:
        break
    out += data
    while out.count(b"[y/N]") > answered:
        reply = answers[answered] if answered < len(answers) else "n"
        os.write(fd, (reply if reply != "_" else "").encode() + b"\r")
        answered += 1
os.waitpid(pid, 0)
sys.stdout.write(out.decode(errors="replace").replace("\r", ""))
PY
    )"
  fi
}
sec_done() { rm -rf "${SEC_HOME:?}"; }
sec_echo() { sed 's/^/        | /' <<<"$SEC_OUT"; }
sec_mode() { stat -c %a "$SEC_TOML" 2>/dev/null || echo missing; }
sec_token() { sed -n -E 's/^token = "(.*)"$/\1/p' "$SEC_TOML" 2>/dev/null; }
sec_check() {  # $1 = label, $2 = 0 for pass
  if [[ "$2" -eq 0 ]]; then ok "$1"; else bad "$1"; sec_echo; fi
}
OLD_TOKEN='token = "original-token-0123456789"'

# 1. No terminal, no flags: nothing changes, not even a file.
run_security - "" "" ""
sec_check "no terminal, no flags: no suede.toml appears" "$([[ ! -e "$SEC_TOML" ]]; echo $?)"
sec_done
run_security - "" "" 'bind = "0.0.0.0:9088"'
sec_check "no terminal, no flags: an existing file is untouched" \
  "$([[ "$(cat "$SEC_TOML")" == 'bind = "0.0.0.0:9088"' && "$(sec_mode)" == 644 ]]; echo $?)"
sec_done

# 2. --token generate with no terminal: written, mode 600, never printed.
run_security - generate "" ""
TOKEN="$(sec_token)"
sec_check "--token generate: a token of 40 safe characters is written" \
  "$([[ "$TOKEN" =~ ^[A-Za-z0-9]{40}$ ]]; echo $?)"
sec_check "--token generate: the file is mode 600 and owned by the user" \
  "$([[ "$(sec_mode)" == 600 && "$(stat -c %U "$SEC_TOML")" == "$(id -un)" ]]; echo $?)"
sec_check "--token generate: without a terminal the token is not printed" \
  "$([[ -n "$TOKEN" && "$SEC_OUT" != *"$TOKEN"* ]]; echo $?)"
echo "        provisioning transcript (--token generate, no terminal):"; sec_echo
sec_done

# 3. --token VALUE: stored as given, mode 600 even over a world-readable
# file, never echoed; the commented example key is rewritten, not duplicated.
run_security - "a-chosen-token-0123456789" "" '# token = "change-me"
bind = "0.0.0.0:9088"'
sec_check "--token VALUE: stored as given, replacing the commented example" \
  "$([[ "$(sec_token)" == "a-chosen-token-0123456789" && "$(grep -c '^token' "$SEC_TOML")" == 1 ]]; echo $?)"
sec_check "--token VALUE: a 644 file becomes 600, other keys survive, nothing echoed" \
  "$([[ "$(sec_mode)" == 600 && "$(grep -c '^bind = ' "$SEC_TOML")" == 1 && "$SEC_OUT" != *a-chosen-token-0123456789* ]]; echo $?)"
sec_done

# 4. An existing token is never replaced without a yes at a terminal.
run_security - generate "" "$OLD_TOKEN"
sec_check "--token generate over an existing token, no terminal: kept" \
  "$([[ "$(sec_token)" == "original-token-0123456789" ]]; echo $?)"
echo "        provisioning transcript (existing token, no terminal):"; sec_echo
sec_done
run_security "n n" generate "" "$OLD_TOKEN"
sec_check "--token generate over an existing token, answered n: kept" \
  "$([[ "$(sec_token)" == "original-token-0123456789" ]]; echo $?)"
sec_done
run_security "n y" generate "" "$OLD_TOKEN"
TOKEN="$(sec_token)"
sec_check "--token generate over an existing token, answered y: replaced, shown on the terminal" \
  "$([[ "$TOKEN" =~ ^[A-Za-z0-9]{40}$ && "$SEC_OUT" == *"$TOKEN"* && "$(sec_mode)" == 600 ]]; echo $?)"
sec_done
run_security - none "" "$OLD_TOKEN"
sec_check "--token none leaves an existing token alone" \
  "$([[ "$(sec_token)" == "original-token-0123456789" ]]; echo $?)"
sec_done
run_security - none "" ""
sec_check "--token none on a fresh home writes nothing" "$([[ ! -e "$SEC_TOML" ]]; echo $?)"
sec_done

# 5. The token question, under a pseudo-terminal. The pseudo-terminal answers
# each "[y/N]" in order; unanswered ones get n. With no --local-only flag the
# local-only question is asked second.
run_security y "" "" ""
TOKEN="$(sec_token)"
sec_check "token prompt answered y: asks, writes a token, shows it once, mode 600" \
  "$([[ "$SEC_OUT" == *"Require an API token? [y/N]"* && "$TOKEN" =~ ^[A-Za-z0-9]{40}$ && "$SEC_OUT" == *"$TOKEN"* && "$(sec_mode)" == 600 ]]; echo $?)"
echo "        provisioning transcript (token prompt answered y):"; sec_echo
sec_done
run_security n "" "" ""
sec_check "token prompt answered n: nothing is written" \
  "$([[ "$SEC_OUT" == *"Require an API token? [y/N]"* && ! -e "$SEC_TOML" ]]; echo $?)"
sec_done
run_security _ "" "" ""
sec_check "token prompt answered with just Enter: nothing is written" "$([[ ! -e "$SEC_TOML" ]]; echo $?)"
sec_done
run_security y "" "" "$OLD_TOKEN"
sec_check "token prompt with a token already set: not asked, token kept" \
  "$([[ "$SEC_OUT" != *"Require an API token?"* && "$(sec_token)" == "original-token-0123456789" ]]; echo $?)"
sec_done
run_security n "" yes ""
sec_check "--local-only yes does not suppress the token question (answered n: no token)" \
  "$([[ "$SEC_OUT" == *"Require an API token? [y/N]"* && "$SEC_OUT" != *"Bind Suede to this machine only"* && -z "$(sec_token)" ]]; echo $?)"
sec_done
run_security y "" no ""
TOKEN="$(sec_token)"
sec_check "--local-only no does not suppress the token question (answered y: token written)" \
  "$([[ "$SEC_OUT" == *"Require an API token? [y/N]"* && "$TOKEN" =~ ^[A-Za-z0-9]{40}$ && "$(grep -c '^bind' "$SEC_TOML")" == 0 ]]; echo $?)"
sec_done
run_security "n y" "" "" ""
sec_check "--token absent, --local-only absent: both asked, token n then local-only y" \
  "$([[ "$SEC_OUT" == *"Require an API token? [y/N]"* && "$SEC_OUT" == *"Bind Suede to this machine only, for use behind a reverse proxy? [y/N]"* \
     && -z "$(sec_token)" && "$(grep '^bind' "$SEC_TOML")" == 'bind = "127.0.0.1:9088"' ]]; echo $?)"
sec_done

# 6. The local-only question and its flag.
run_security - "" yes ""
sec_check "--local-only yes: bind is the loopback address on the port, no terminal needed" \
  "$([[ "$(grep '^bind' "$SEC_TOML")" == 'bind = "127.0.0.1:9088"' && "$(sec_mode)" == 600 ]]; echo $?)"
echo "        provisioning transcript (--local-only yes):"; sec_echo
sec_done
run_security - "" no 'bind = "0.0.0.0:9088"'
sec_check "--local-only no: an existing bind is left alone" \
  "$([[ "$(cat "$SEC_TOML")" == 'bind = "0.0.0.0:9088"' ]]; echo $?)"
sec_done
run_security - "" no 'bind = "127.0.0.1:9088"'
sec_check "--local-only no on a loopback-bound machine: bind left alone, and it says so" \
  "$([[ "$(cat "$SEC_TOML")" == 'bind = "127.0.0.1:9088"' && "$SEC_OUT" == *"already binds to loopback"* ]]; echo $?)"
echo "        provisioning transcript (--local-only no, already loopback):"; sec_echo
sec_done
run_security "n y" "" "" ""
sec_check "local-only prompt answered y: asks, writes the loopback bind" \
  "$([[ "$SEC_OUT" == *"Bind Suede to this machine only, for use behind a reverse proxy? [y/N]"* && "$(grep '^bind' "$SEC_TOML")" == 'bind = "127.0.0.1:9088"' ]]; echo $?)"
echo "        provisioning transcript (token n, local-only y):"; sec_echo
sec_done
run_security "n n" "" "" 'bind = "0.0.0.0:9088"'
sec_check "local-only prompt answered n: existing bind kept" \
  "$([[ "$SEC_OUT" == *"Bind Suede to this machine only"* && "$(cat "$SEC_TOML")" == 'bind = "0.0.0.0:9088"' ]]; echo $?)"
sec_done
run_security "n _" "" "" ""
sec_check "local-only prompt answered with just Enter: nothing is written" \
  "$([[ "$SEC_OUT" == *"Bind Suede to this machine only"* && ! -e "$SEC_TOML" ]]; echo $?)"
sec_done
run_security "n n" "" "" 'bind = "127.0.0.1:9088"'
sec_check "local-only prompt answered n on a loopback-bound machine: bind left alone, says so" \
  "$([[ "$(cat "$SEC_TOML")" == 'bind = "127.0.0.1:9088"' && "$SEC_OUT" == *"already binds to loopback"* ]]; echo $?)"
sec_done
run_security y none "" ""
sec_check "--token none does not suppress the local-only question (asked, answered y: bind written)" \
  "$([[ "$SEC_OUT" != *"Require an API token?"* && "$SEC_OUT" == *"Bind Suede to this machine only"* \
     && "$(grep '^bind' "$SEC_TOML")" == 'bind = "127.0.0.1:9088"' ]]; echo $?)"
sec_done
run_security n generate "" ""
TOKEN="$(sec_token)"
sec_check "--token generate does not suppress the local-only question (asked, answered n: token only)" \
  "$([[ "$SEC_OUT" != *"Require an API token?"* && "$SEC_OUT" == *"Bind Suede to this machine only"* \
     && -n "$TOKEN" && "$(grep -c '^bind' "$SEC_TOML")" == 0 ]]; echo $?)"
sec_done
run_security - generate yes 'bind = "0.0.0.0:9088"
allow_overlaps = true'
sec_check "--local-only yes with --token generate: bind rewritten once, other keys kept, token set" \
  "$([[ "$(grep -c '^bind' "$SEC_TOML")" == 1 && "$(grep '^bind' "$SEC_TOML")" == 'bind = "127.0.0.1:9088"' \
     && "$(grep -c '^allow_overlaps = true' "$SEC_TOML")" == 1 && -n "$(sec_token)" && "$(sec_mode)" == 600 ]]; echo $?)"
sec_done

# 6b. The reboot question and its flag. Only the decision is exercised.
run_security - "" "" "" ""
sec_check "reboot, no terminal, no flag: no reboot" "$([[ "$SEC_OUT" == *REBOOT=no ]]; echo $?)"
sec_done
run_security - "" "" "" yes
sec_check "--reboot yes: reboots without a terminal" "$([[ "$SEC_OUT" == *REBOOT=yes ]]; echo $?)"
sec_done
run_security - "" "" "" no
sec_check "--reboot no: no reboot, no terminal" "$([[ "$SEC_OUT" == *REBOOT=no ]]; echo $?)"
sec_done
run_security "n n y" "" "" "" ""
sec_check "reboot prompt answered y (after the token and local-only questions): reboots" \
  "$([[ "$SEC_OUT" == *"Reboot now to apply everything? [y/N]"* && "$SEC_OUT" == *REBOOT=yes ]]; echo $?)"
echo "        provisioning transcript (all three questions, last answered y):"; sec_echo
sec_done
run_security "n n n" "" "" "" ""
sec_check "reboot prompt answered n: no reboot" \
  "$([[ "$SEC_OUT" == *"Reboot now to apply everything? [y/N]"* && "$SEC_OUT" == *REBOOT=no ]]; echo $?)"
sec_done
run_security "n n _" "" "" "" ""
sec_check "reboot prompt answered with just Enter: no reboot" "$([[ "$SEC_OUT" == *REBOOT=no ]]; echo $?)"
sec_done
run_security "y" none yes "" ""
sec_check "--reboot absent is asked even when both other flags are given (answered y)" \
  "$([[ "$SEC_OUT" != *"Require an API token?"* && "$SEC_OUT" != *"Bind Suede to this machine only"* \
     && "$SEC_OUT" == *"Reboot now to apply everything? [y/N]"* && "$SEC_OUT" == *REBOOT=yes ]]; echo $?)"
sec_done
run_security "n n" "" "" "" yes
sec_check "--reboot yes does not suppress the other two questions" \
  "$([[ "$SEC_OUT" == *"Require an API token?"* && "$SEC_OUT" == *"Bind Suede to this machine only"* \
     && "$SEC_OUT" != *"Reboot now"* && "$SEC_OUT" == *REBOOT=yes ]]; echo $?)"
sec_done

# 7. Bad option values and the older spellings. Option parsing runs ahead of
# the root check, so the real script can be called.
for bad_args in "--reboot" "--reboot maybe" "--local-only maybe"; do
  # shellcheck disable=SC2086
  if OUT="$(bash packaging/provision.sh $bad_args 2>&1)"; then
    bad "provision.sh $bad_args should be refused"
  else
    ok "provision.sh $bad_args is refused: $(head -n1 <<<"$OUT")"
  fi
done
for bad_args in "--token" "--token short" "--token has/slash/in-it-0123456789" '--token a"quoted"token0123456789'; do
  # shellcheck disable=SC2086
  if OUT="$(bash packaging/provision.sh $bad_args 2>&1)"; then
    bad "provision.sh $bad_args should be refused"
  else
    ok "provision.sh $bad_args is refused: $(head -n1 <<<"$OUT")"
  fi
done

# The option parser alone, to see what each spelling sets. $0 is the real
# script so --help reads its header.
ARGUMENT_PARSING="$(sed -n '/^# BEGIN ARGUMENT_PARSING$/,/^# END ARGUMENT_PARSING$/p' packaging/provision.sh)"
[[ -n "$ARGUMENT_PARSING" ]] || bad "could not find the ARGUMENT_PARSING block in provision.sh"
parsed() {  # prints "LOCAL_ONLY=<> REBOOT=<> TOKEN_MODE=<>" for the arguments
  bash -c "set -euo pipefail; $ARGUMENT_PARSING"'
echo "LOCAL_ONLY=$LOCAL_ONLY REBOOT=$REBOOT TOKEN_MODE=$TOKEN_MODE"' provision.sh "$@" 2>&1
}
sec_check "bare --local-only is --local-only yes" "$([[ "$(parsed --local-only)" == "LOCAL_ONLY=yes REBOOT= TOKEN_MODE=" ]]; echo $?)"
sec_check "bare --local-only before another flag is --local-only yes" \
  "$([[ "$(parsed --local-only --token none)" == "LOCAL_ONLY=yes REBOOT= TOKEN_MODE=none" ]]; echo $?)"
sec_check "--local-only yes and --local-only no" \
  "$([[ "$(parsed --local-only yes)" == "LOCAL_ONLY=yes"* && "$(parsed --local-only no)" == "LOCAL_ONLY=no"* ]]; echo $?)"
sec_check "--no-reboot is --reboot no" \
  "$([[ "$(parsed --no-reboot)" == "LOCAL_ONLY= REBOOT=no TOKEN_MODE=" && "$(parsed --reboot no)" == "LOCAL_ONLY= REBOOT=no TOKEN_MODE=" ]]; echo $?)"
sec_check "--reboot yes" "$([[ "$(parsed --reboot yes)" == "LOCAL_ONLY= REBOOT=yes TOKEN_MODE=" ]]; echo $?)"
sec_check "no flags leave every answer empty (so a terminal is asked)" \
  "$([[ "$(parsed --port 9100)" == "LOCAL_ONLY= REBOOT= TOKEN_MODE=" ]]; echo $?)"
HELP="$(bash packaging/provision.sh --help)"
sec_check "--help documents the three flags, their values and the older spellings" \
  "$([[ "$HELP" == *"--token generate|VALUE|none"* && "$HELP" == *"--local-only yes|no"* && "$HELP" == *"--reboot yes|no"* \
     && "$HELP" == *"bare --local-only"* && "$HELP" == *"--no-reboot"* && "$HELP" != *"set -euo"* ]]; echo $?)"

echo
echo "nvidia_uvm is configured at boot on NVIDIA machines only"
UVM_FUNCTION="$(sed -n '/^# BEGIN NVIDIA_UVM_FUNCTION$/,/^# END NVIDIA_UVM_FUNCTION$/p' packaging/provision.sh)"
[[ -n "$UVM_FUNCTION" ]] || bad "could not find the NVIDIA_UVM_FUNCTION block in provision.sh"
# $1 = label, $2 = fake lspci output, $3 = 1 to fake /proc/driver/nvidia,
# $4 = 1 to fake a working modinfo, $5 = 1 to fake /usr/bin/nvidia-modprobe,
# $6 = expected: "none", "conf" or "conf+rule"
expect_uvm() {
  local root bin conf rule got
  root="$(mktemp -d)"; bin="$(mktemp -d)"
  printf '#!/bin/sh\n[ -n "%s" ] && echo "%s"\nexit 0\n' "$2" "$2" > "$bin/lspci"
  printf '#!/bin/sh\nexit %s\n' "$([[ "$4" == 1 ]] && echo 0 || echo 1)" > "$bin/modinfo"
  chmod +x "$bin/lspci" "$bin/modinfo"
  if [[ "$3" == 1 ]]; then mkdir -p "$root/proc/driver/nvidia"; : > "$root/proc/driver/nvidia/version"; fi
  if [[ "$5" == 1 ]]; then mkdir -p "$root/usr/bin"; printf '#!/bin/sh\n' > "$root/usr/bin/nvidia-modprobe"; chmod +x "$root/usr/bin/nvidia-modprobe"; fi
  conf="$root/etc/modules-load.d/nvidia-uvm.conf"
  rule="$root/etc/udev/rules.d/70-suede-nvidia-uvm.rules"
  # Run twice: the second pass proves it is idempotent.
  ( PATH="$bin:$PATH"; SUEDE_FAKE_ROOT="$root"; eval "$UVM_FUNCTION"; install_nvidia_uvm; install_nvidia_uvm ) >/dev/null 2>&1
  if [[ ! -e "$conf" && ! -e "$rule" ]]; then got=none
  elif [[ "$(cat "$conf" 2>/dev/null)" != "nvidia-uvm" ]]; then got="bad conf"
  elif [[ ! -e "$rule" ]]; then got=conf
  elif [[ "$(cat "$rule")" == 'KERNEL=="nvidia_uvm", RUN+="/usr/bin/nvidia-modprobe -c0 -u"' ]]; then got=conf+rule
  else got="bad rule"; fi
  if [[ "$got" == "$6" ]]; then ok "$1 -> $6"; else bad "$1 -> $got (expected $6)"; fi
  rm -rf "$root" "$bin"
}
expect_uvm "no NVIDIA anywhere"                    ""                              0 0 0 none
expect_uvm "PCI vendor 10de, nvidia-modprobe"      "01:00.0 0300: 10de:2204 (rev a1)" 0 0 1 conf+rule
expect_uvm "PCI vendor 10de, no nvidia-modprobe"   "01:00.0 0300: 10de:2204 (rev a1)" 0 0 0 conf
expect_uvm "loaded driver only"                    ""                              1 0 1 conf+rule
expect_uvm "installed module only"                 ""                              0 1 0 conf
expect_uvm "nvidia-modprobe alone is not NVIDIA"   ""                              0 0 1 none

echo
echo "Packaged assets exist"
python3 - <<'PY' || exit 1
import re, sys, pathlib
manifest = pathlib.Path("Cargo.toml").read_text()
block = manifest.split("[package.metadata.deb]", 1)[1]
assets = re.findall(r'\["([^"]+)",\s*"([^"]+)",\s*"(\d+)"\]', block)
missing = []
for source, dest, mode in assets:
    # The release binary only exists after a release build.
    if source.startswith("target/"):
        print(f"  SKIP  {source} (built by CI)")
        continue
    if pathlib.Path(source).exists():
        print(f"  PASS  {source} -> {dest}")
    else:
        print(f"  FAIL  {source} is missing")
        missing.append(source)
sys.exit(1 if missing else 0)
PY
[[ $? -eq 0 ]] || FAIL=1

echo
echo "Maintainer scripts are where cargo-deb expects them"
[[ -f packaging/postinst ]] && ok "packaging/postinst" || bad "packaging/postinst"

echo
echo "systemd unit is well formed"
grep -q '^\[Unit\]'    packaging/suede.service && ok "[Unit] section"    || bad "[Unit] section"
grep -q '^\[Service\]' packaging/suede.service && ok "[Service] section" || bad "[Service] section"
grep -q '^\[Install\]' packaging/suede.service && ok "[Install] section" || bad "[Install] section"
grep -q 'WantedBy=sway-session.target' packaging/suede.service \
  && ok "bound to sway-session.target" || bad "bound to sway-session.target"
grep -q 'ExecStart=/usr/bin/suede run' packaging/suede.service \
  && ok "ExecStart matches the packaged path" || bad "ExecStart matches the packaged path"

echo
echo "Example configuration is valid"
python3 -c "import json;json.load(open('docs/examples/four-output-appliance.json'))" \
  && ok "four-output-appliance.json parses" || bad "four-output-appliance.json parses"

echo
echo "Workflow references files that exist"
for path in scripts/smoke-test.sh scripts/build-docs.sh docs/requirements.txt mkdocs.yml; do
  [[ -f "$path" ]] && ok "$path" || bad "$path"
done

echo
echo "─────────────────────────────"
[[ "$FAIL" -eq 0 ]] && echo "  packaging looks sound" || echo "  packaging has problems"
exit "$FAIL"
