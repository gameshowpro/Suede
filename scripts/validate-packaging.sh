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
