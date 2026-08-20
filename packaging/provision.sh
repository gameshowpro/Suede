#!/usr/bin/env bash
#
# Prepare a machine to run as a Suede display appliance.
#
# This does the root-level work that the daemon deliberately will not do:
# auto-login, starting sway at boot, getting competing desktop environments
# out of the way, and opening the API port in the host firewall. It is
# idempotent — safe to re-run after an upgrade — and it never touches Suede's
# own configuration, which lives in the API and survives package operations.
#
#   sudo /usr/share/suede/provision.sh [--user NAME] [--no-reboot]
#                                      [--port N] [--no-firewall]

set -euo pipefail

APPLIANCE_USER="${SUDO_USER:-}"
ASK_REBOOT=1
# Must match DEFAULT_BIND in src/config.rs.
SUEDE_PORT=9088
OPEN_FIREWALL=1

while [[ $# -gt 0 ]]; do
  case "$1" in
    --user) APPLIANCE_USER="$2"; shift 2 ;;
    --no-reboot) ASK_REBOOT=0; shift ;;
    --port) SUEDE_PORT="$2"; shift 2 ;;
    --no-firewall) OPEN_FIREWALL=0; shift ;;
    -h|--help) sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 1 ;;
  esac
done

if [[ "$(id -u)" -ne 0 ]]; then
  echo "This script must run as root (use sudo)." >&2
  exit 1
fi

if [[ -z "$APPLIANCE_USER" ]]; then
  echo "Could not determine which user runs the appliance." >&2
  echo "Re-run with --user NAME." >&2
  exit 1
fi

if ! id "$APPLIANCE_USER" >/dev/null 2>&1; then
  echo "No such user: $APPLIANCE_USER" >&2
  exit 1
fi

USER_HOME="$(getent passwd "$APPLIANCE_USER" | cut -d: -f6)"
USER_UID="$(id -u "$APPLIANCE_USER")"
step() { echo; echo "==> $*"; }

step "Provisioning for user '$APPLIANCE_USER' (home: $USER_HOME)"

# --- 1. Packages --------------------------------------------------------
step "Checking required packages"
# swayidle is listed because the sway config this script writes relies on it
# to keep displays from blanking. Ubuntu's sway ends up shipping it anyway,
# but Debian's only *Suggests* it, so a trixie appliance ran with the
# `exec_always swayidle` line failing silently.
MISSING=()
for package in sway swayidle pipewire pipewire-pulse; do
  dpkg-query -W -f='${Status}' "$package" 2>/dev/null | grep -q "ok installed" || MISSING+=("$package")
done
if ((${#MISSING[@]})); then
  echo "Installing: ${MISSING[*]}"
  export DEBIAN_FRONTEND=noninteractive
  apt-get update -qq
  apt-get install -y "${MISSING[@]}"
else
  echo "All required packages are present."
fi

# The same candidates the launcher presets try, so this cannot disagree with
# what would actually be launched. `chromium` alone reported "no browser" on a
# machine running Google Chrome quite happily.
BROWSER=""
for candidate in chromium chromium-browser google-chrome-stable google-chrome firefox firefox-esr; do
  if command -v "$candidate" >/dev/null 2>&1; then BROWSER="$candidate"; break; fi
done
if [[ -z "$BROWSER" ]]; then
  echo "WARNING: no supported browser found."
  echo "         Suede looks for: chromium, chromium-browser, google-chrome-stable,"
  echo "         google-chrome, firefox, firefox-esr."
  echo "         On Debian:  apt install chromium"
  echo "         On Ubuntu the 'chromium' package is a snap wrapper, which is a poor"
  echo "         fit for an appliance; installing Google Chrome's .deb is usually"
  echo "         better. Chromium is also the only one whose autoplay policy Suede"
  echo "         can relax, so prefer it where sound matters."
else
  echo "Browser found: $BROWSER"
fi

# --- 1b. Device group membership ----------------------------------------
#
# An appliance has no seated graphical login, so systemd-logind never applies
# its `uaccess` ACLs to /dev/dri or /dev/snd — those are granted per session,
# to sessions attached to a seat, and a machine started by auto-login into a
# systemd user service does not reliably have one. Without the static groups
# the symptom is silent and confusing: sway may run fine while PipeWire finds
# no audio devices at all and falls back to a dummy sink, so everything looks
# healthy and nothing can be heard.
step "Adding '$APPLIANCE_USER' to the device groups"
for group in audio video render; do
  if ! getent group "$group" >/dev/null; then
    echo "  $group: no such group on this system, skipping"
  elif id -nG "$APPLIANCE_USER" | tr ' ' '
' | grep -qx "$group"; then
    echo "  $group: already a member"
  else
    usermod -aG "$group" "$APPLIANCE_USER"
    echo "  $group: added"
    GROUPS_CHANGED=1
  fi
done
if [[ -n "${GROUPS_CHANGED:-}" ]]; then
  echo
  echo "  NOTE: a running session keeps the groups it started with, so this"
  echo "        machine must be REBOOTED before PipeWire can open the sound"
  echo "        devices. Restarting the services is not enough."
fi

# --- 2. Auto-login on tty1 ----------------------------------------------
step "Configuring auto-login on tty1"
install -d /etc/systemd/system/getty@tty1.service.d
cat > /etc/systemd/system/getty@tty1.service.d/override.conf <<EOF
# Managed by Suede provisioning.
[Service]
ExecStart=
ExecStart=-/sbin/agetty --skip-login --nonewline --noissue --autologin $APPLIANCE_USER --noclear %I \$TERM
Type=idle
EOF

# --- 3. Start sway at login ---------------------------------------------
step "Configuring sway to start at login"

# Sway refuses to run on the NVIDIA proprietary driver unless told to, so a
# machine provisioned without this simply never comes up. Detected from the
# loaded module rather than from lspci, because what matters is the driver
# actually bound to the card.
SWAY_FLAGS=""
if [[ -d /sys/module/nvidia_drm ]]; then
  SWAY_FLAGS=" --unsupported-gpu"
  echo "  NVIDIA proprietary driver detected: adding --unsupported-gpu"
  if [[ "$(cat /sys/module/nvidia_drm/parameters/modeset 2>/dev/null)" != "Y" ]]; then
    echo "  WARNING: nvidia_drm modeset is off, and sway needs it. Add"
    echo "           nvidia_drm.modeset=1 to the kernel command line and reboot."
  fi
fi
BEGIN="# BEGIN SUEDE_PROVISION"
END="# END SUEDE_PROVISION"
PROFILE="$USER_HOME/.bash_profile"
touch "$PROFILE"
# Replace only our own block, so the user's profile is left intact.
if grep -qF "$BEGIN" "$PROFILE"; then
  sed -i "/$BEGIN/,/$END/d" "$PROFILE"
fi
cat >> "$PROFILE" <<EOF
$BEGIN
# Start sway on the first virtual terminal, where auto-login lands.
if [ "\$(tty)" = "/dev/tty1" ] && [ -z "\${WAYLAND_DISPLAY:-}" ]; then
  export MOZ_ENABLE_WAYLAND=1
  export XDG_SESSION_TYPE=wayland
  export XDG_CURRENT_DESKTOP=sway
  export XDG_SESSION_DESKTOP=sway
  # Without this, a window spanning several outputs is handed straight to each
  # display controller, so every screen shows the same part of it instead of
  # its own. Verified on the Nvidia proprietary driver.
  export WLR_SCENE_DISABLE_DIRECT_SCANOUT=1
  # The headless backend provides the projection canvas: an off-screen
  # output the app renders into, which the slicer cuts up per projector.
  export WLR_BACKENDS=drm,libinput,headless
  clear
  exec sway${SWAY_FLAGS} > "\$HOME/.sway.log" 2>&1
fi
$END
EOF
chown "$APPLIANCE_USER:$APPLIANCE_USER" "$PROFILE"

# A machine that already starts sway some other way now has two, and they
# will fight over the graphics card at the next boot.
RIVALS="$(grep -rlE '^ExecStart=.*sway'   "$USER_HOME/.config/systemd/user" 2>/dev/null || true)"
if [[ -n "$RIVALS" ]]; then
  echo
  echo "  WARNING: these user units also start sway:"
  echo "$RIVALS" | sed 's/^/           /'
  echo "           Two compositors cannot share the graphics card. Disable"
  echo "           either those units or the auto-login above before rebooting."
  RIVAL_SWAY=1
fi

# --- 4. Sway configuration ----------------------------------------------
step "Preparing the sway configuration"

# `install -d` applies ownership only to the directories named on the command
# line; parents it creates on the way stay root-owned. On a machine where no
# desktop session has ever run (a headless Debian install administered over
# SSH), $USER_HOME/.config does not exist yet, so creating the sway directory
# in one step would leave ~/.config itself owned by root. The user's systemd
# manager then cannot create ~/.config/systemd, and `systemctl --user enable`
# fails — with the misleading message "Unit ... does not exist" on systemd
# 257. Create the parent explicitly, and repair a wrong owner however it got
# there: the first root-run tool to touch a home directory tends to leave one
# behind, long before this script is involved.
if [[ ! -d "$USER_HOME/.config" ]]; then
  install -d -o "$APPLIANCE_USER" -g "$APPLIANCE_USER" "$USER_HOME/.config"
elif [[ "$(stat -c %U "$USER_HOME/.config")" != "$APPLIANCE_USER" ]]; then
  echo "  repairing ownership of $USER_HOME/.config (was $(stat -c %U "$USER_HOME/.config"))"
  chown "$APPLIANCE_USER:$APPLIANCE_USER" "$USER_HOME/.config"
fi
SWAY_DIR="$USER_HOME/.config/sway"
install -d -o "$APPLIANCE_USER" -g "$APPLIANCE_USER" "$SWAY_DIR"
SWAY_CONFIG="$SWAY_DIR/config"

if [[ ! -f "$SWAY_CONFIG" ]]; then
  # A minimal base config. Suede manages outputs and windows through IPC, so
  # this deliberately declares no outputs of its own.
  cat > "$SWAY_CONFIG" <<'EOF'
# Base sway configuration for a Suede appliance.
# Outputs and windows are managed by Suede over IPC; do not declare them here.
set $mod Mod4

# Never blank an appliance display.
exec_always swayidle -w timeout 0 ''

include /etc/sway/config.d/*
EOF
  chown "$APPLIANCE_USER:$APPLIANCE_USER" "$SWAY_CONFIG"
fi

# The daemon owns a marker-delimited block inside this file; adding it here
# means the very first boot already works. Suede keeps it up to date after that.
SWAY_BEGIN="# BEGIN SUEDE_CONFIG"
SWAY_END="# END SUEDE_CONFIG"
if ! grep -qF "$SWAY_BEGIN" "$SWAY_CONFIG"; then
  cat >> "$SWAY_CONFIG" <<EOF

$SWAY_BEGIN
# Managed by Suede. Edits inside this block are overwritten.
# Hand the session environment to systemd so the user service can reach sway.
exec systemctl --user import-environment WAYLAND_DISPLAY XDG_CURRENT_DESKTOP SWAYSOCK
exec systemctl --user start sway-session.target
# An appliance shows content, not window decorations.
default_border none
default_floating_border none
$SWAY_END
EOF
  chown "$APPLIANCE_USER:$APPLIANCE_USER" "$SWAY_CONFIG"
fi

# --- 5. sway-session.target ---------------------------------------------
step "Installing sway-session.target"
cat > /etc/systemd/user/sway-session.target <<'EOF'
[Unit]
Description=sway session
Documentation=man:systemd.special(7)
BindsTo=graphical-session.target
Wants=graphical-session-pre.target
After=graphical-session-pre.target
EOF

# --- 6. Enable the daemon for this user ---------------------------------
step "Enabling the suede user service"
loginctl enable-linger "$APPLIANCE_USER" >/dev/null 2>&1 || true
systemctl daemon-reload
if [[ -f /usr/lib/systemd/user/suede.service ]]; then
  # Show the outcome rather than swallowing it: a service that never got
  # enabled is invisible until the web UI cannot be reached.
  if ENABLE_OUT="$(runuser -l "$APPLIANCE_USER" -c \
      "XDG_RUNTIME_DIR=/run/user/$USER_UID systemctl --user enable suede.service" 2>&1)"; then
    echo "  suede.service enabled"
  else
    echo "  systemctl --user enable failed: ${ENABLE_OUT}"
    echo "  creating the enablement symlink directly instead"
    # Exactly what `enable` would have done: link the unit into the target
    # named by its [Install] section. Done as the user, so every directory
    # created on the way is owned by the user — and it needs no running user
    # manager, so it also covers provisioning from a root console before the
    # appliance user has ever logged in.
    runuser -u "$APPLIANCE_USER" -- \
      mkdir -p "$USER_HOME/.config/systemd/user/sway-session.target.wants"
    runuser -u "$APPLIANCE_USER" -- \
      ln -sfn /usr/lib/systemd/user/suede.service \
      "$USER_HOME/.config/systemd/user/sway-session.target.wants/suede.service"
  fi
else
  echo "NOTE: /usr/lib/systemd/user/suede.service is missing — install the .deb first."
fi

# --- 7. Get competing desktops out of the way ---------------------------
step "Disabling competing desktop environments"
for service in display-manager lightdm gdm3 sddm labwc wayfire-pi phosh; do
  if systemctl list-unit-files "${service}.service" >/dev/null 2>&1 &&
     systemctl is-enabled "${service}.service" >/dev/null 2>&1; then
    echo "  disabling ${service}"
    systemctl disable --now "${service}.service" >/dev/null 2>&1 || true
    systemctl mask "${service}.service" >/dev/null 2>&1 || true
  fi
done
# Raspberry Pi OS ships a labwc autostart entry that would fight sway.
rm -f /etc/xdg/autostart/labwc.desktop 2>/dev/null || true
rm -f "$USER_HOME/.config/autostart/labwc.desktop" 2>/dev/null || true

systemctl set-default multi-user.target >/dev/null 2>&1 || true

step "Opening the API port"
# Ubuntu Server enables ufw with SSH alone allowed, and its default policy
# drops rather than rejects — so a correctly running Suede times out from the
# network and looks like a machine that is switched off. This is the one place
# in provisioning with the privileges to fix that.
if [[ "$OPEN_FIREWALL" -eq 0 ]]; then
  echo "  skipped (--no-firewall)"
elif command -v ufw >/dev/null 2>&1 && ufw status 2>/dev/null | head -1 | grep -q "Status: active"; then
  if ufw status 2>/dev/null | grep -qE "^${SUEDE_PORT}(/tcp)?[[:space:]]"; then
    echo "  ufw already allows ${SUEDE_PORT}/tcp"
  else
    ufw allow "${SUEDE_PORT}/tcp" >/dev/null && \
      echo "  allowed ${SUEDE_PORT}/tcp through ufw" || \
      echo "  could not add the ufw rule; open ${SUEDE_PORT}/tcp by hand"
  fi
elif command -v firewall-cmd >/dev/null 2>&1 && firewall-cmd --state >/dev/null 2>&1; then
  firewall-cmd --permanent --add-port="${SUEDE_PORT}/tcp" >/dev/null && \
    firewall-cmd --reload >/dev/null && \
    echo "  allowed ${SUEDE_PORT}/tcp through firewalld"
else
  echo "  no active host firewall detected; nothing to open"
fi

step "Provisioning complete"
cat <<EOF

  User:         $APPLIANCE_USER
  Sway config:  $SWAY_CONFIG
  Service:      suede.service (systemd user unit)

  After a reboot the machine will log in, start sway, and start Suede.
  Open http://$(hostname):9088/ from another computer to configure it.
EOF

if [[ -n "${GROUPS_CHANGED:-}" ]]; then
  cat <<'EOF'

  NOTE: group membership changed. A running session keeps the groups it
  started with, so restarting the user services is not enough - the machine
  must be rebooted (or the user session fully terminated) before PipeWire
  can open the sound devices.
EOF
fi

if [[ -n "${RIVAL_SWAY:-}" ]]; then
  cat <<'EOF'

  UNRESOLVED: something else on this machine also starts sway, and both
  are now armed. On the next boot they will contend for the graphics
  card: one wins, the other fails, and whichever of them happens to
  publish SWAYSOCK last is the one Suede will attach to - which may be
  the one with no displays on it. Disable one before rebooting:

    systemctl --user disable --now <that unit>
EOF
fi

if [[ "$ASK_REBOOT" -eq 1 ]]; then
  echo
  read -r -p "Reboot now to apply everything? [y/N] " answer
  [[ "${answer,,}" == "y" ]] && reboot
fi
exit 0
