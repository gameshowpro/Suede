#!/usr/bin/env bash
#
# Take a machine back to nothing, fetch the latest released package, install
# it and provision from scratch - the sequence a first-time user follows,
# run against a real appliance rather than a container.
#
#   sudo ./clean-slate.sh [--user NAME] [--yes] [--keep-state] [--tag vX.Y.Z]
#
# Order matters. The package is downloaded before anything is destroyed, so a
# network failure leaves the machine as it was; and it is downloaded to
# /var/tmp rather than /tmp, because /tmp is tmpfs on a lot of installs and
# this ends with a reboot.
#
# The reset script is taken out of the package just downloaded, not from the
# installed copy, so the reset that runs is the one that shipped with the
# build being tested - and it works when nothing is installed at all.

set -euo pipefail

REPO="gameshowpro/Suede"
APPLIANCE_USER="${SUDO_USER:-}"
ASSUME_YES=0
KEEP_STATE=0
TAG="latest"
STAGE=/var/tmp

while [[ $# -gt 0 ]]; do
  case "$1" in
    --user) APPLIANCE_USER="$2"; shift 2 ;;
    --yes|-y) ASSUME_YES=1; shift ;;
    --keep-state) KEEP_STATE=1; shift ;;
    --tag) TAG="$2"; shift 2 ;;
    -h|--help) sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 1 ;;
  esac
done

[[ "$(id -u)" -eq 0 ]] || { echo "Run this with sudo." >&2; exit 1; }
[[ -n "$APPLIANCE_USER" ]] && id "$APPLIANCE_USER" >/dev/null 2>&1 \
  || { echo "Which user runs the appliance? Pass --user NAME." >&2; exit 1; }

step() { echo; echo "==> $*"; }
ARCH="$(dpkg --print-architecture)"

# --- 1. Fetch first, destroy second -------------------------------------
step "Finding the latest release for $ARCH"
if [[ "$TAG" == "latest" ]]; then
  API="https://api.github.com/repos/$REPO/releases/latest"
else
  API="https://api.github.com/repos/$REPO/releases/tags/$TAG"
fi
read -r VERSION URL < <(curl -fsSL "$API" | python3 -c "
import json, sys
release = json.load(sys.stdin)
asset = next((a for a in release['assets']
              if a['name'].endswith('_${ARCH}.deb')), None)
if asset is None:
    sys.exit('no ${ARCH} package attached to %s' % release['tag_name'])
print(release['tag_name'], asset['browser_download_url'])
")
echo "  $VERSION -> $(basename "$URL")"

DEB="$STAGE/$(basename "$URL")"
curl -fsSL "$URL" -o "$DEB"
# A truncated or HTML-error download is a .deb that dpkg will refuse halfway
# through, by which point the machine has already been wiped.
dpkg-deb -I "$DEB" >/dev/null 2>&1 || { echo "  the download is not a valid package"; exit 1; }
echo "  downloaded $(du -h "$DEB" | cut -f1) to $DEB"
echo "  package version: $(dpkg-deb -f "$DEB" Version)"

INSTALLED="$(dpkg-query -W -f='${Version}' suede 2>/dev/null || echo 'not installed')"
echo "  currently installed: $INSTALLED"

if ((!ASSUME_YES)); then
  echo
  echo "  This wipes Suede, its configuration and saved state, undoes the"
  echo "  provisioning changes, then installs and provisions the package above."
  read -r -p "  Continue? [y/N] " answer
  [[ "${answer,,}" == "y" ]] || { echo "Nothing done. The package is at $DEB."; exit 0; }
fi

# --- 2. Reset, using the script from the build being tested --------------
step "Resetting the machine"
RESET="$STAGE/reset-machine.sh"
dpkg-deb --fsys-tarfile "$DEB" | tar -xO ./usr/share/suede/reset-machine.sh > "$RESET"
chmod +x "$RESET"
RESET_ARGS=(--user "$APPLIANCE_USER" --yes)
((KEEP_STATE)) && RESET_ARGS+=(--keep-state)
"$RESET" "${RESET_ARGS[@]}"

# --- 3. Install and provision --------------------------------------------
step "Installing $VERSION"
DEBIAN_FRONTEND=noninteractive apt-get install -y "$DEB"

step "Provisioning"
# --no-reboot so this script's own summary is the last thing on screen; the
# reboot is offered below instead.
/usr/share/suede/provision.sh --user "$APPLIANCE_USER" --no-reboot

# --- 4. Where that leaves things -----------------------------------------
step "Ready"
cat <<EOF

  Installed:  $(dpkg-query -W -f='${Version}' suede)
  Binary:     $(command -v suede)
  User:       $APPLIANCE_USER
  Package:    $DEB   (kept, in case you want to reinstall)

  Nothing is configured: no outputs, no applications. That is the point -
  this is the first-run state a new user sees.

  A reboot is needed before the function test. The compositor starts from
  the auto-login this just configured, and group membership is inherited
  from login rather than re-read, so a running session cannot pick it up.

  After the reboot:
    systemctl --user status suede
    curl -s http://127.0.0.1:9088/api/v1/status
    open http://$(hostname):9088/ from another machine

EOF

if ((!ASSUME_YES)); then
  read -r -p "  Reboot now? [y/N] " answer
  [[ "${answer,,}" == "y" ]] && reboot
fi
