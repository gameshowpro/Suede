#!/usr/bin/env bash
#
# Prepare a machine to run as a Suede display appliance.
#
# This does the root-level work that the daemon deliberately will not do:
# auto-login, starting sway at boot, and getting competing desktop
# environments out of the way. It is idempotent — safe to re-run after an
# upgrade — and it never touches Suede's own configuration, which lives in the
# API and survives package operations.
#
#   sudo /usr/share/suede/provision.sh [--user NAME] [--no-reboot]

set -euo pipefail

APPLIANCE_USER="${SUDO_USER:-}"
ASK_REBOOT=1

while [[ $# -gt 0 ]]; do
  case "$1" in
    --user) APPLIANCE_USER="$2"; shift 2 ;;
    --no-reboot) ASK_REBOOT=0; shift ;;
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
MISSING=()
for package in sway pipewire pipewire-pulse; do
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

if ! command -v chromium >/dev/null 2>&1 && ! command -v firefox >/dev/null 2>&1; then
  echo "WARNING: neither chromium nor firefox is installed."
  echo "         Install one before configuring a kiosk app:  apt install chromium"
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
  clear
  exec sway > "\$HOME/.sway.log" 2>&1
fi
$END
EOF
chown "$APPLIANCE_USER:$APPLIANCE_USER" "$PROFILE"

# --- 4. Sway configuration ----------------------------------------------
step "Preparing the sway configuration"
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
  runuser -l "$APPLIANCE_USER" -c \
    "XDG_RUNTIME_DIR=/run/user/$USER_UID systemctl --user enable suede.service" >/dev/null 2>&1 \
    || echo "NOTE: enable suede.service after the next login (systemctl --user enable suede.service)"
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

step "Provisioning complete"
cat <<EOF

  User:         $APPLIANCE_USER
  Sway config:  $SWAY_CONFIG
  Service:      suede.service (systemd user unit)

  After a reboot the machine will log in, start sway, and start Suede.
  Open http://\$(hostname):7071/ from another computer to configure it.
EOF

if [[ "$ASK_REBOOT" -eq 1 ]]; then
  echo
  read -r -p "Reboot now to apply everything? [y/N] " answer
  [[ "${answer,,}" == "y" ]] && reboot
fi
exit 0
