#!/usr/bin/env bash
# Retakes the docs screenshots of the web UI's Displays tab from a live
# appliance, encodes them to AVIF under docs/assets/screenshots/, and updates
# their width and height attributes in the docs. See
# scripts/screenshots/README.md.
#
# Usage: scripts/capture-screenshots.sh <base-url> <healthy|disconnected>
#   e.g. scripts/capture-screenshots.sh http://appliance:9088 healthy
#
# SCREENSHOT_ONLY=name,name retakes only those captures of the phase.
#
# Private words are rewritten before every capture. The rules come from two
# local files that are not in the repository: the machine names in the alias
# table of .claude/test-beds.md (each replaced by its alias, "system-a"), and
# .claude/screenshot-redactions.tsv (pattern, tab, replacement). Without that
# file the script refuses to run unless SCREENSHOT_ALLOW_UNREDACTED=1.
#
# The PNGs and a backup of the appliance's working copy stay in
# $SCREENSHOT_OUT (default: a new temporary directory) for review.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

base="${1:?usage: $0 <base-url> <healthy|disconnected>}"
phase="${2:?usage: $0 <base-url> <healthy|disconnected>}"
out="${SCREENSHOT_OUT:-$(mktemp -d -t suede-screenshots.XXXXXX)}"
rules=".claude/screenshot-redactions.tsv"
beds=".claude/test-beds.md"

if [ ! -f "$rules" ] && [ "${SCREENSHOT_ALLOW_UNREDACTED:-0}" != 1 ]; then
  echo "capture-screenshots: $rules is missing; refusing to capture without redaction rules." >&2
  exit 1
fi

SCREENSHOT_REDACTIONS="$(python3 - "$rules" "$beds" <<'PYEOF'
import json, re, sys
from pathlib import Path
rules, beds = Path(sys.argv[1]), Path(sys.argv[2])
pairs = []
if beds.exists():
    for line in beds.read_text(encoding="utf-8").splitlines():
        m = re.match(r"^\|\s*System ([A-Z])\s*\|\s*([^|]*?)\s*\|", line)
        if m and m.group(2):
            pairs.append([re.escape(m.group(2)), f"system-{m.group(1).lower()}"])
if rules.exists():
    for line in rules.read_text(encoding="utf-8").splitlines():
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        pattern, _, replacement = line.partition("\t")
        pairs.append([pattern.strip(), replacement.strip()])
print(json.dumps(pairs))
PYEOF
)"
export SCREENSHOT_REDACTIONS

NODE_PATH="${NODE_PATH:-/tmp/suede-ui-tests/node_modules}" \
PLAYWRIGHT_BROWSERS_PATH="${PLAYWRIGHT_BROWSERS_PATH:-/tmp/suede-ui-tests/browsers}" \
  node scripts/screenshots/capture.cjs "$base" "$out" "$phase"

# Same encoder settings as the existing captures: libaom, 4:4:4, CRF 40.
find "$out" -name '*.png' | sort | while read -r png; do
  name="${png#"$out"/}"
  name="${name%.png}"
  avif="docs/assets/screenshots/$name.avif"
  mkdir -p "$(dirname "$avif")"
  ffmpeg -nostdin -loglevel error -y -i "$png" -c:v libaom-av1 -crf 40 -pix_fmt yuv444p -still-picture 1 "$avif"
  size="$(ffprobe -v error -select_streams v:0 -show_entries stream=width,height -of csv=p=0:s=x "$avif")"
  echo "encoded $avif ($size)"
  python3 - "$name" "${size%x*}" "${size#*x}" <<'PYEOF'
import re, sys
from pathlib import Path
name, width, height = sys.argv[1:]
pattern = re.compile(r'(screenshots/' + re.escape(name) + r'\.avif\)\{ width=")[^"]*(" height=")[^"]*(")')
for page in Path("docs").rglob("*.md"):
    text = page.read_text(encoding="utf-8")
    updated = pattern.sub(lambda m: f"{m.group(1)}{width}{m.group(2)}{height}{m.group(3)}", text)
    if updated != text:
        page.write_text(updated, encoding="utf-8")
        print(f"  sizes updated in {page}")
PYEOF
done
echo "PNGs and the working-copy backup are in $out"
