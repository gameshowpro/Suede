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
# on: only "usleep" and "nothing" export anything; anything else (absent,
# "default", commented out, or a typo) leaves the variable unset, since Mesa
# ignores it anyway and NVIDIA's own default is what "unset" means.
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
expect_gl_yield "no suede.toml"           ""                     unset
expect_gl_yield "gl_yield = \"default\""  'gl_yield = "default"'  unset
expect_gl_yield "gl_yield = \"usleep\""   'gl_yield = "usleep"'   USLEEP
expect_gl_yield "gl_yield = \"nothing\""  'gl_yield = "nothing"'  NOTHING
expect_gl_yield "gl_yield commented out"  '# gl_yield = "usleep"' unset
expect_gl_yield "an unrecognized value"   'gl_yield = "busy"'     unset

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
