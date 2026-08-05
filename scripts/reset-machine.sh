#!/usr/bin/env bash
#
# Return a machine to the state it was in before it ever met Suede, so the
# next install can be tested honestly.
#
# Testing an installer on a machine that has already been installed on proves
# very little: the interesting failures are the ones that only happen the
# first time, and they hide behind whatever the last run left lying around.
#
#   sudo ./reset-machine.sh --user NAME [options]
#
#   --user NAME      the appliance user (default: $SUDO_USER)
#   --dry-run        list what would be removed and change nothing
#   --keep-state     leave the desired-state document and app profiles alone
#   --restore-desktop  re-enable any display manager provisioning masked
#   --yes            do not ask for confirmation
#
# What it does NOT touch, deliberately:
#   - Browsers, sway, PipeWire, or anything else installed as a dependency.
#     Those are ordinary packages that were probably wanted anyway, and
#     guessing which ones arrived because of Suede is how a reset script ends
#     up removing something that mattered.
#   - Any hand-built rig of your own (a compositor unit you wrote, a directory
#     of test binaries). It reports what it finds rather than deleting it.

set -euo pipefail

APPLIANCE_USER="${SUDO_USER:-}"
DRY_RUN=0
KEEP_STATE=0
RESTORE_DESKTOP=0
ASSUME_YES=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --user) APPLIANCE_USER="$2"; shift 2 ;;
    --dry-run) DRY_RUN=1; shift ;;
    --keep-state) KEEP_STATE=1; shift ;;
    --restore-desktop) RESTORE_DESKTOP=1; shift ;;
    --yes|-y) ASSUME_YES=1; shift ;;
    -h|--help) sed -n '2,26p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 1 ;;
  esac
done

if [[ "$(id -u)" -ne 0 ]]; then
  echo "This script must run as root (use sudo)." >&2
  exit 1
fi
if [[ -z "$APPLIANCE_USER" ]] || ! id "$APPLIANCE_USER" >/dev/null 2>&1; then
  echo "Which user ran the appliance? Re-run with --user NAME." >&2
  exit 1
fi

USER_HOME="$(getent passwd "$APPLIANCE_USER" | cut -d: -f6)"
USER_UID="$(id -u "$APPLIANCE_USER")"
step() { echo; echo "==> $*"; }
# Every destructive action goes through these two, so --dry-run is honest by
# construction rather than by remembering to check a flag each time.
act() { if ((DRY_RUN)); then echo "    would run: $*"; else "$@" >/dev/null 2>&1 || true; fi; }
drop() {
  [[ -e "$1" || -L "$1" ]] || return 0
  if ((DRY_RUN)); then echo "    would remove: $1"; else rm -rf "$1"; echo "    removed $1"; fi
}
as_user() {
  act runuser -u "$APPLIANCE_USER" -- \
    env "XDG_RUNTIME_DIR=/run/user/$USER_UID" "$@"
}

echo "Resetting for user '$APPLIANCE_USER' (home: $USER_HOME)"
((DRY_RUN)) && echo "DRY RUN - nothing will be changed."
if ((!DRY_RUN)) && ((!ASSUME_YES)); then
  echo
  echo "This removes Suede, its configuration, its saved desired state, and the"
  echo "changes provisioning made to logins and the compositor."
  read -r -p "Continue? [y/N] " answer
  [[ "${answer,,}" == "y" ]] || { echo "Nothing done."; exit 0; }
fi

# --- 1. Stop everything Suede is running --------------------------------
step "Stopping Suede and anything it launched"
as_user systemctl --user disable --now suede.service
# Slicers and blend overlays are children of the daemon, but a crashed daemon
# can leave them behind, and they hold the outputs.
for pattern in "suede slice" "suede blend" "suede run"; do
  if pgrep -f "$pattern" >/dev/null 2>&1; then
    echo "    stopping: $pattern"
    act pkill -f "$pattern"
  fi
done
# Kiosk browsers were started by the supervisor and outlive it.
if pgrep -u "$APPLIANCE_USER" -f "user-data-dir=.*suede" >/dev/null 2>&1; then
  echo "    stopping kiosk browsers started by Suede"
  act pkill -u "$APPLIANCE_USER" -f "user-data-dir=.*suede"
fi

# --- 2. The package, or a binary someone copied into place ---------------
step "Removing the package"
if dpkg-query -W -f='${Status}' suede 2>/dev/null | grep -q "^install ok installed"; then
  if ((DRY_RUN)); then echo "    would purge: suede"; else
    DEBIAN_FRONTEND=noninteractive apt-get purge -y suede >/dev/null 2>&1 \
      || dpkg --purge suede >/dev/null 2>&1 || true
    echo "    purged suede"
  fi
else
  echo "    no suede package installed"
fi
# A direct build leaves the same files without dpkg knowing about them.
for path in /usr/bin/suede /usr/local/bin/suede /usr/share/suede \
            /usr/share/doc/suede /usr/lib/systemd/user/suede.service \
            /etc/systemd/user/sway-session.target; do
  drop "$path"
done

# --- 3. Per-user configuration and state ---------------------------------
step "Removing Suede's own files"
drop "$USER_HOME/.config/systemd/user/suede.service"
drop "$USER_HOME/.config/systemd/user/default.target.wants/suede.service"
drop "$USER_HOME/.config/suede"
if ((KEEP_STATE)); then
  echo "    keeping $USER_HOME/.local/state/suede (--keep-state)"
else
  # Desired state, browser profiles, uploaded wallpapers, app logs.
  drop "$USER_HOME/.local/state/suede"
fi

# --- 4. What provisioning changed ----------------------------------------
step "Undoing the provisioning changes"
drop /etc/systemd/system/getty@tty1.service.d/override.conf
rmdir /etc/systemd/system/getty@tty1.service.d 2>/dev/null || true

PROFILE="$USER_HOME/.bash_profile"
if [[ -f "$PROFILE" ]] && grep -qF "# BEGIN SUEDE_PROVISION" "$PROFILE"; then
  if ((DRY_RUN)); then echo "    would strip the SUEDE_PROVISION block from $PROFILE"; else
    sed -i "/# BEGIN SUEDE_PROVISION/,/# END SUEDE_PROVISION/d" "$PROFILE"
    echo "    stripped the SUEDE_PROVISION block from $PROFILE"
  fi
fi

# The daemon owns a marked block inside the user's sway config; the rest of
# that file may well be theirs, so take the block and leave the file.
SWAY_CONFIG="$USER_HOME/.config/sway/config"
if [[ -f "$SWAY_CONFIG" ]] && grep -qF "# BEGIN SUEDE_CONFIG" "$SWAY_CONFIG"; then
  if ((DRY_RUN)); then echo "    would strip the SUEDE_CONFIG block from $SWAY_CONFIG"; else
    sed -i "/# BEGIN SUEDE_CONFIG/,/# END SUEDE_CONFIG/d" "$SWAY_CONFIG"
    echo "    stripped the SUEDE_CONFIG block from $SWAY_CONFIG"
  fi
fi

act systemctl daemon-reload

# --- 5. Things provisioning turned off ------------------------------------
step "Display managers provisioning may have masked"
MASKED=""
for service in display-manager lightdm gdm3 sddm labwc wayfire-pi phosh; do
  if [[ "$(systemctl is-enabled "${service}.service" 2>/dev/null)" == "masked" ]]; then
    MASKED="$MASKED $service"
  fi
done
if [[ -z "$MASKED" ]]; then
  echo "    none are masked"
elif ((RESTORE_DESKTOP)); then
  for service in $MASKED; do
    echo "    unmasking and enabling $service"
    act systemctl unmask "${service}.service"
    act systemctl enable "${service}.service"
  done
  act systemctl set-default graphical.target
else
  echo "   ${MASKED} are masked. They are left alone: on an appliance that is"
  echo "    usually wanted, and re-enabling a display manager on a machine with"
  echo "    a compositor already running is its own kind of mess."
  echo "    Pass --restore-desktop to undo it."
fi

# --- 6. Report what is deliberately untouched -----------------------------
step "Left alone"
RIVALS="$(grep -rlE '^ExecStart=.*sway' "$USER_HOME/.config/systemd/user" 2>/dev/null || true)"
[[ -n "$RIVALS" ]] && {
  echo "    compositor units you wrote yourself:"
  echo "$RIVALS" | sed 's/^/      /'
}
for dir in "$USER_HOME/suede-test" "$USER_HOME/suede-target"; do
  [[ -d "$dir" ]] && echo "    $dir (a rig of your own, not Suede's)"
done
if command -v ufw >/dev/null 2>&1 && ufw status 2>/dev/null | grep -qE "^9088"; then
  echo "    the ufw rule for 9088/tcp (harmless, and possibly yours)"
fi
echo "    sway, PipeWire and any browser: ordinary packages, wanted or not"
echo "      independently of Suede."

step "Done"
if ((DRY_RUN)); then
  echo "  Nothing was changed. Re-run without --dry-run to apply."
else
  echo "  Reboot before testing an install, so no part of the old session"
  echo "  survives into the new one - group membership and the compositor in"
  echo "  particular are inherited from login, not re-read."
fi
