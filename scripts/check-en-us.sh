#!/usr/bin/env bash
# Fails when a tracked text file contains en-GB spelling. The project is
# written in en-US; a one-time sweep does not hold without a check, so this
# runs in CI and in scripts/dev-check.sh.
#
# Usage: scripts/check-en-us.sh            check every tracked file
#        scripts/check-en-us.sh FILE...    check only these files
#
# To allow a deliberate exception (a quotation, a third-party identifier),
# put `en-us-allow` in a comment on the same line.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

# Verb stems are matched only with an -ise style ending, so words whose en-US
# form is identical (exercise, promise, precise, otherwise, advertise,
# supervise, optimistic, synthesis, emphasis, analysis) never match. Letters
# are the only word characters here: `name_colour` and `tileColour` are hits
# too, which a plain `\b` would miss because `_` counts as a word character.
STEMS='organ|optim|initial|normal|serial|deserial|raster|synchron|resynchron|recogn|unrecogn|material|quant|summar|util|maxim|minim|parallel|character|symbol|unsymbol|custom|final|general|special|visual|priorit|standard|categor|author|central|local|memor|stabil|emphas|synthes'
ISE='is(e|ed|es|er|ers|ing|ation|ations|able)'
# Substrings that occur in no en-US word, matched anywhere, including inside
# snake_case and camelCase identifiers.
ANYWHERE='colour|centre|centring|neighbour|favour|honour|behaviour|flavour|labour|grey'
WORDS='analys(e|ed|es|ing)|labell(ed|ing)|modell(ed|ing)|travell(ed|ing)|signall(ed|ing)|metres?|millimetres?|enquir(y|ies|e|ed)|artefacts?|whilst|amongst|defence|offence|licence[sd]?|fulfils?|catalogue[sd]?'
PATTERN="(${ANYWHERE})|(^|[^A-Za-z])((${STEMS})${ISE}|${WORDS})([^A-Za-z]|\$)"

if [ "$#" -gt 0 ]; then
  files=("$@")
else
  mapfile -t files < <(git ls-files \
    ':!:Cargo.lock' ':!:LICENSE' ':!:*.spv' ':!:*.png' ':!:*.jpg' ':!:*.ico' \
    ':!:*.woff' ':!:*.woff2' ':!:docs/generated/**' ':!:scripts/check-en-us.sh')
fi

status=0
for file in "${files[@]}"; do
  [ -f "$file" ] || continue
  if hits=$(grep -nIiE "$PATTERN" "$file" | grep -v 'en-us-allow'); then
    while IFS= read -r line; do
      printf '%s:%s\n' "$file" "$line"
    done <<<"$hits"
    status=1
  fi
done

if [ "$status" -ne 0 ]; then
  echo >&2
  echo "en-GB spelling found. Use the en-US form, or add 'en-us-allow' on the line for a deliberate exception." >&2
fi
exit "$status"
