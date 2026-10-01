#!/usr/bin/env bash
# Fails when a real test-appliance name leaks into the public docs, or when a
# "System X" alias appears without linking back to its profile.
#
# Public docs (docs/) refer to Suede's test appliances only by alias (System
# A, System B, System C, ...) - never by hostname, IP, username or SSID. The
# alias-to-real-name mapping lives in the "Alias lookup and current status"
# table at the top of .claude/test-beds.md, which is gitignored and local
# only, so a fresh checkout or CI has nothing to compare against: this check
# then exits 0 with a note. The second check (every "System X" occurrence is
# a link to its profile) does not depend on that file and always runs.
#
# Usage: scripts/check-machine-names.sh
set -uo pipefail
cd "$(git rev-parse --show-toplevel)"

DOCS_DIR="docs"
LOOKUP=".claude/test-beds.md"
PROFILE_PAGE="developer/test-systems.md"

status=0

if [ ! -f "$LOOKUP" ]; then
  echo "check-machine-names: $LOOKUP absent (CI or a fresh checkout) - skipping the real-name scan."
else
  # Rows look like "| System A | <real name> | Development workhorse | ... |".
  # The real name is the second column.
  names="$(awk -F'|' '
    /^\| *System [A-Z] *\|/ {
      name = $3
      gsub(/^[ \t]+|[ \t]+$/, "", name)
      if (name != "") print name
    }
  ' "$LOOKUP")"

  if [ -z "$names" ]; then
    echo "check-machine-names: no alias rows found in $LOOKUP's lookup table - nothing to scan for." >&2
    status=1
  else
    while IFS= read -r name; do
      [ -z "$name" ] && continue
      if hits="$(grep -rniwE -- "$name" "$DOCS_DIR")"; then
        echo "check-machine-names: real machine name '$name' found under $DOCS_DIR/ - use its alias instead:" >&2
        echo "$hits" >&2
        status=1
      fi
    done <<<"$names"
  fi
fi

# Second check: every "System A"/"System B"/"System C" (etc.) occurrence
# under docs/ must be the link text of a markdown link to the profile page.
# The profile page itself (docs/developer/test-systems.md) is exempt - its
# own "## System A {: #system-a }" headings are the target, not a mention.
link_status="$(python3 - "$DOCS_DIR" "$PROFILE_PAGE" <<'PYEOF'
import re
import sys
from pathlib import Path

docs_dir = Path(sys.argv[1])
profile_page = sys.argv[2]
profile_path = (docs_dir / profile_page).resolve()

mention_re = re.compile(r"System [A-Z]\b")
link_re = re.compile(r"\[([^\]]*)\]\(([^)]*)\)")

problems = []

for path in sorted(docs_dir.rglob("*.md")):
    if path.resolve() == profile_path:
        continue
    text = path.read_text(encoding="utf-8")

    # Positions covered by a link whose text names a system and whose target
    # actually points at that system's anchor on the profile page.
    covered = []
    for m in link_re.finditer(text):
        link_text, target = m.group(1), m.group(2)
        target_path, _, target_anchor = target.partition("#")
        resolved_target = (path.parent / target_path).resolve() if target_path else path.resolve()
        for sm in mention_re.finditer(link_text):
            system = sm.group(0)
            expected_anchor = system.lower().replace(" ", "-")
            if resolved_target == profile_path and target_anchor == expected_anchor:
                covered.append((m.start(1) + sm.start(), m.start(1) + sm.end()))
            else:
                problems.append(
                    f"{path}: '{system}' is linked, but not to "
                    f"{profile_page}#{expected_anchor} (target: {target})"
                )

    for m in mention_re.finditer(text):
        if not any(start <= m.start() and m.end() <= end for start, end in covered):
            line_no = text.count("\n", 0, m.start()) + 1
            problems.append(f"{path}:{line_no}: '{m.group(0)}' is not linked to {profile_page}")

if problems:
    print("\n".join(problems))
    sys.exit(1)
PYEOF
)"
link_exit=$?

if [ "$link_exit" -ne 0 ]; then
  echo "check-machine-names: unlinked or misdirected system alias found under $DOCS_DIR/:" >&2
  echo "$link_status" >&2
  status=1
fi

exit "$status"
