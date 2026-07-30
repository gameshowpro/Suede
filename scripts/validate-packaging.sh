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
