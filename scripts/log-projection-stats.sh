#!/usr/bin/env bash
# Records GET /api/v1/projection/stats on an appliance, one JSON object per
# line, each stamped with the UTC time it was read. Written for camera
# sessions: the `sync` test pattern draws the same UTC clock on every output,
# so a frame of video and a line of this log can be matched to each other
# without anyone writing anything down.
#
# Usage: scripts/log-projection-stats.sh [host] [seconds] [interval]
#          host      ssh alias of the appliance (default: workshop)
#          seconds   how long to record (default: 900)
#          interval  seconds between samples (default: 5)
#
# The slicer publishes a fresh interval every 10 s, so sampling faster than
# that repeats values - harmless, and it keeps the log dense enough that any
# moment in a video lands next to a line written seconds earlier.
#
# Writes to ~/sync-session/stats-<UTC stamp>.jsonl on the appliance and
# prints the path. Runs in the foreground; Ctrl-C stops it and leaves the
# file complete, because every line is flushed as it is written.
set -uo pipefail

HOST="${1:-workshop}"
SECONDS_TOTAL="${2:-900}"
INTERVAL="${3:-5}"

ssh "$HOST" bash -s "$SECONDS_TOTAL" "$INTERVAL" <<'REMOTE_SCRIPT'
set -uo pipefail
TOTAL="$1"
INTERVAL="$2"
API=http://localhost:9088/api/v1

mkdir -p ~/sync-session
STAMP=$(date -u +%Y%m%dT%H%M%SZ)
OUT=~/sync-session/stats-$STAMP.jsonl
echo "recording to $OUT on $(hostname) for ${TOTAL}s every ${INTERVAL}s"
echo "start (UTC): $(date -u +%H:%M:%S.%3N)  - the clock the sync pattern draws"

# One header line describing the machine and the arm, so a log read months
# later still says which side of the A/B it came from.
{
  printf '{"kind":"header","readAtUtc":"%s"' "$(date -u +%Y-%m-%dT%H:%M:%S.%3NZ)"
  printf ',"host":"%s"' "$(hostname)"
  printf ',"suede":%s' "$(curl -s "$API/system" | python3 -c 'import sys,json;d=json.load(sys.stdin);print(json.dumps({k:d.get(k) for k in ("suedeVersion","buildId","swayVersion")}))')"
  printf ',"bootstrap":%s' "$(python3 - <<'PY'
import json, os, re
path = os.path.expanduser("~/.config/suede/suede.toml")
keys = {}
try:
    for line in open(path):
        line = line.split("#", 1)[0].strip()
        m = re.match(r'^(\w+)\s*=\s*(.+)$', line)
        if m:
            keys[m.group(1)] = m.group(2).strip()
except OSError:
    pass
print(json.dumps(keys))
PY
)"
  # Whether the compositor is actually scanning out is a property of the
  # running sway, not of the file, so read it from the process itself.
  pid=$(pgrep -x sway | head -1)
  if [[ -n "$pid" ]] && tr '\0' '\n' < "/proc/$pid/environ" 2>/dev/null | grep -q '^WLR_SCENE_DISABLE_DIRECT_SCANOUT='; then
    printf ',"compositorScanout":false'
  else
    printf ',"compositorScanout":true'
  fi
  printf ',"config":%s' "$(curl -s "$API/config" | python3 -c 'import sys,json;c=json.load(sys.stdin);print(json.dumps({"activeApp":c.get("activeApp"),"projection":c.get("projection"),"outputs":c.get("outputs")}))')"
  printf '}\n'
} >> "$OUT"

END=$(( $(date +%s) + TOTAL ))
while [[ $(date +%s) -lt $END ]]; do
  READ_AT=$(date -u +%Y-%m-%dT%H:%M:%S.%3NZ)
  STATS=$(curl -s "$API/projection/stats")
  APP=$(curl -s "$API/status" | python3 -c 'import sys,json;a=json.load(sys.stdin).get("activeApp") or {};print(json.dumps({"id":a.get("id"),"state":a.get("state")}))' 2>/dev/null || echo 'null')
  printf '{"kind":"sample","readAtUtc":"%s","activeApp":%s,"stats":%s}\n' \
    "$READ_AT" "$APP" "${STATS:-null}" >> "$OUT"
  sleep "$INTERVAL"
done

echo "stop  (UTC): $(date -u +%H:%M:%S.%3N)"
echo "$(grep -c '"kind":"sample"' "$OUT") samples in $OUT"
REMOTE_SCRIPT
