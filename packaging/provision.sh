#!/usr/bin/env bash
#
# Prepare a machine to run as a Suede display appliance.
#
# This does the root-level work that the daemon deliberately will not do:
# auto-login, starting sway at boot, getting competing desktop environments
# out of the way, and opening the API port in the host firewall. It is
# idempotent — safe to re-run after an upgrade — and it never touches Suede's
# desired state, which lives in the API and survives package operations.
#
#   sudo /usr/share/suede/provision.sh [--user NAME] [--port N]
#                                      [--token generate|VALUE|none]
#                                      [--local-only yes|no]
#                                      [--reboot yes|no]
#                                      [--no-firewall] [--allow-overlaps]
#                                      [--no-direct-scanout]
#
# Three questions are asked at a terminal, and each has exactly one flag that
# answers it instead. A question is asked only when its own flag is absent (one
# flag never suppresses another question). With no terminal and no flag, each
# question takes its default answer: nothing changes and the machine is not
# rebooted.
#
#   Question (default)                                   Flag
#   Require an API token? (no)                           --token generate|VALUE|none
#   Bind Suede to this machine only, for use behind      --local-only yes|no
#     a reverse proxy? (no)
#   Reboot now to apply everything? (no)                 --reboot yes|no
#
# --token decides whether the API requires a bearer token. `generate` writes a
# random 40-character token into the user's suede.toml (owned by the user, mode
# 600) and shows it once, on the terminal only. VALUE (16 or more of
# A-Z a-z 0-9 . _ ~ -) stores a token you chose and never echoes it. `none`
# leaves the API open and any token already there alone. An existing token is
# never replaced without a yes at a terminal, and when one is already set the
# token question is not asked.
#
# --local-only yes writes `bind = "127.0.0.1:<port>"` into suede.toml and skips
# the firewall rule: the shape for an appliance reached through a reverse
# proxy on the same machine or an SSH tunnel. See docs/reverse-proxy.md.
# `no` leaves the bind alone, including one that is already on loopback.
#
# Older spellings still work: a bare --local-only means --local-only yes, and
# --no-reboot means --reboot no.
#
# --allow-overlaps provisions the other display path: outputs may overlap in
# canvas space, every multi-output layout goes through the slicer, and each
# display scans out its own buffer. It writes `allow_overlaps = true` into the
# user's suede.toml.
#
# --no-direct-scanout writes `direct_scanout = false` beside it: the same
# sliced layout, composited by sway instead of flipped to the display
# controllers. That is the other arm of the A/B — flip the key, restart the
# session, and compare the projection stats.
#
# Neither option decides anything by itself. The login profile this script
# writes derives WLR_SCENE_DISABLE_DIRECT_SCANOUT from those two keys every
# time it starts sway, so editing suede.toml and restarting the session is
# enough; on an appliance where sway is not a systemd unit it is the only
# thing that can follow the file.

set -euo pipefail

# BEGIN ARGUMENT_PARSING
APPLIANCE_USER="${SUDO_USER:-}"
REBOOT=""
# Must match DEFAULT_BIND in src/config.rs.
SUEDE_PORT=9088
OPEN_FIREWALL=1
ALLOW_OVERLAPS=0
DIRECT_SCANOUT=1
TOKEN_MODE=""
LOCAL_ONLY=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --user) APPLIANCE_USER="$2"; shift 2 ;;
    --no-reboot) REBOOT=no; shift ;;
    --reboot)
      REBOOT="${2:-}"
      case "$REBOOT" in
        yes|no) ;;
        *) echo "--reboot needs yes or no" >&2; exit 1 ;;
      esac
      shift 2 ;;
    --port) SUEDE_PORT="$2"; shift 2 ;;
    --no-firewall) OPEN_FIREWALL=0; shift ;;
    --allow-overlaps) ALLOW_OVERLAPS=1; shift ;;
    --no-direct-scanout) DIRECT_SCANOUT=0; shift ;;
    --token)
      TOKEN_MODE="${2:-}"
      if [[ -z "$TOKEN_MODE" ]]; then
        echo "--token needs generate, none or a token value" >&2; exit 1
      fi
      # A value ends up inside a quoted TOML string and a sed replacement, so
      # it is held to a character set that needs no escaping in either.
      case "$TOKEN_MODE" in
        generate|none) ;;
        *)
          if ! [[ "$TOKEN_MODE" =~ ^[A-Za-z0-9._~-]{16,}$ ]]; then
            echo "--token: a value must be at least 16 characters from A-Z a-z 0-9 . _ ~ -" >&2
            echo "         (or use --token generate)" >&2
            exit 1
          fi ;;
      esac
      shift 2 ;;
    --local-only)
      # Bare --local-only is the older spelling of --local-only yes.
      case "${2:-}" in
        yes|no) LOCAL_ONLY="$2"; shift 2 ;;
        *) LOCAL_ONLY=yes; shift ;;
      esac ;;
    -h|--help) sed -n '2,60p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 1 ;;
  esac
done
# END ARGUMENT_PARSING

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
# One spanning window across every output, or one private buffer per output:
# the two display paths want opposite things from direct scanout, and the
# answer lives in suede.toml, not here. So the profile block below *derives*
# the variable at every login rather than having it baked in at provisioning
# time — on an appliance where sway starts from tty1 there is no systemd unit
# for the health check's fix to write a drop-in on, and this block is the only
# thing that can follow the file. Changing a key and restarting the session is
# then the whole A/B.
#
# Kept in its own heredoc, delimited by SCANOUT_EOF, so scripts/validate-packaging.sh
# can lift it out and run it against sample suede.toml files. Quoted, so every
# expansion inside happens at login and not now.
SCANOUT_BLOCK="$(cat <<'SCANOUT_EOF'
  # Direct scanout must match the display path recorded in suede.toml. The
  # compositor may flip a client buffer straight to a display controller only
  # where the slicer owns every output (allow_overlaps = true) and scanout has
  # not been turned off for comparison (direct_scanout = false). In every other
  # case — a tiling appliance, or no file at all — a window spanning the outputs
  # would be scanned out from each one's own origin and every screen would show
  # the same part of it, so the variable is exported. Verified on the Nvidia
  # proprietary driver. This mirrors BootstrapConfig::scanout_expected.
  suede_toml_flag() {
    sed -n -E "s/^[[:space:]]*$1[[:space:]]*=[[:space:]]*(true|false).*/\1/p" \
      "$HOME/.config/suede/suede.toml" 2>/dev/null | head -n1
  }
  if [ "$(suede_toml_flag allow_overlaps)" = "true" ] \
     && [ "$(suede_toml_flag direct_scanout)" != "false" ]; then
    unset WLR_SCENE_DISABLE_DIRECT_SCANOUT
  else
    export WLR_SCENE_DISABLE_DIRECT_SCANOUT=1
  fi
  unset -f suede_toml_flag
SCANOUT_EOF
)"

# Experimental direct presentation (`presentation = "direct"` in suede.toml):
# a headless-only sway holds the canvas the app renders into, and the Suede
# slicer drives the displays itself. Chosen here, at every login, because the
# compositor's backends are fixed when it starts. The daemon resolves the same
# keys the same way (suede::presentation::resolve), and asks this block for
# the ordinary session instead by writing the fallback marker and ending the
# headless sway; getty's auto-login then comes straight back here.
#
# Runtime state lives in $XDG_RUNTIME_DIR/suede, a tmpfs, so a fallback lasts
# for the rest of this boot only and direct is tried again after a reboot:
#   session                what the last login started (direct or wayland)
#   direct-attempts        headless starts this boot not yet confirmed by the
#                          daemon; the third unconfirmed one falls back
#   presentation-fallback  JSON {reason, time, bootId}; present = wayland
#
# For a direct session it also chooses which DRM card's render node the
# headless canvas is allocated on: the one with the most connected
# connectors, never one with none — see the comment above the
# WLR_RENDER_DRM_DEVICE loop below for why a multi-GPU machine needs this.
#
# Kept in its own heredoc, delimited by PRESENTATION_EOF, so
# scripts/validate-packaging.sh can run it against sample files. It sets
# suede_session, which the lines after it act on, rather than starting sway
# itself.
PRESENTATION_BLOCK="$(cat <<'PRESENTATION_EOF'
  # Direct presentation (experimental): see presentation in suede.toml.
  suede_state="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/suede"
  mkdir -p "$suede_state" 2>/dev/null
  suede_toml_word() {
    sed -n -E "s/^[[:space:]]*$1[[:space:]]*=[[:space:]]*\"?([A-Za-z_]+)\"?.*/\1/p" \
      "$HOME/.config/suede/suede.toml" 2>/dev/null | head -n1
  }
  # After a direct session, hand the displays back before a DRM sway wants
  # them: give the daemon's slicer time to release them itself, then clear
  # any NVKMS grant a killed slicer left behind.
  if [ "$(cat "$suede_state/session" 2>/dev/null)" = "direct" ]; then
    suede_wait=0
    while [ "$suede_wait" -lt 15 ] \
       && pgrep -u "$(id -u)" -f -- ' slice --spec .*--presentation-config' >/dev/null 2>&1; do
      sleep 1
      suede_wait=$((suede_wait + 1))
    done
    /usr/bin/suede display-reset >/dev/null 2>&1 || true
  fi
  suede_attempts="$(cat "$suede_state/direct-attempts" 2>/dev/null)"
  case "$suede_attempts" in ''|*[!0-9]*) suede_attempts=0 ;; esac
  suede_session=wayland
  if [ "$(suede_toml_word presentation)" = "direct" ] \
     && [ "$(suede_toml_word allow_overlaps)" = "true" ] \
     && [ ! -e "$suede_state/presentation-fallback" ]; then
    if [ "$suede_attempts" -lt 3 ]; then
      suede_session=direct
    else
      printf '{"reason":"%s","time":%s,"bootId":"%s"}\n' \
        "headless compositor failed to start 3 times" "$(date +%s)" \
        "$(cat /proc/sys/kernel/random/boot_id 2>/dev/null)" \
        > "$suede_state/presentation-fallback"
    fi
  fi
  echo "$suede_session" > "$suede_state/session"
  if [ "$suede_session" = "direct" ]; then
    echo $((suede_attempts + 1)) > "$suede_state/direct-attempts"
    export WLR_BACKENDS=headless
    export WLR_HEADLESS_OUTPUTS=1
    # Pick the render node of the DRM card that actually drives the physical
    # displays, not just the first one enumerated. On a machine with more
    # than one GPU — an integrated GPU beside the discrete card that outputs
    # to the wall, System B's shape — the first renderD* is often the iGPU's,
    # which has nothing connected. The headless canvas must be allocated on
    # the same device the direct-mode slicer will later match by DRM primary
    # node (pick_direct_physical_device in src/projection/gpu/display.rs), or
    # it finds no candidate GPU at all. The card with the most connected
    # connectors wins; a card with none is never chosen over one that has
    # any. SUEDE_DRM_SYSFS/SUEDE_DRM_DEV stand in for /sys/class/drm and
    # /dev/dri only under scripts/validate-packaging.sh's fake sysfs tree.
    suede_drm_sysfs="${SUEDE_DRM_SYSFS:-/sys/class/drm}"
    suede_drm_dev="${SUEDE_DRM_DEV:-/dev/dri}"
    suede_best_card=""
    suede_best_count=-1
    for suede_card_dir in "$suede_drm_sysfs"/card[0-9]*; do
      [ -d "$suede_card_dir" ] || continue
      # Skip per-connector directories (card1-DP-1); only bare cardN dirs
      # name a device.
      case "${suede_card_dir##*/}" in *-*) continue ;; esac
      suede_count=0
      for suede_status in "$suede_card_dir"-*/status; do
        [ -e "$suede_status" ] || continue
        [ "$(cat "$suede_status" 2>/dev/null)" = "connected" ] \
          && suede_count=$((suede_count + 1))
      done
      if [ "$suede_count" -gt "$suede_best_count" ]; then
        suede_best_count="$suede_count"
        suede_best_card="${suede_card_dir##*/}"
      fi
    done
    if [ -n "$suede_best_card" ] && [ "$suede_best_count" -gt 0 ]; then
      for suede_render in "$suede_drm_sysfs/$suede_best_card"/device/drm/renderD*; do
        [ -e "$suede_render" ] \
          && export WLR_RENDER_DRM_DEVICE="$suede_drm_dev/${suede_render##*/}"
        break
      done
    fi
    # No card reports a connected connector, or the chosen one has no render
    # node of its own: fall back to the first renderD* found at all, exactly
    # as before this rule existed.
    if [ -z "${WLR_RENDER_DRM_DEVICE:-}" ]; then
      for suede_render in "$suede_drm_dev"/renderD*; do
        [ -e "$suede_render" ] && export WLR_RENDER_DRM_DEVICE="$suede_render"
        break
      done
    fi
  fi
  unset -f suede_toml_word
  unset suede_state suede_attempts suede_wait suede_render suede_drm_sysfs \
    suede_drm_dev suede_best_card suede_best_count suede_card_dir \
    suede_count suede_status
PRESENTATION_EOF
)"

# NVIDIA's EGL busy-wait workaround (`gl_yield` in suede.toml, experimental):
# exported into Sway's environment before either branch below execs it, so it
# applies equally to the ordinary DRM session and the headless one that backs
# direct presentation. An absent key (or "usleep") exports USLEEP, the
# recommended NVIDIA profile; "default" exports nothing, leaving driver
# behavior unchanged, and "nothing" exports NOTHING. Mesa ignores the
# variable, so the default is harmless on other drivers. See docs/configuration.md and docs/developer/vk-khr.md for the
# measured trade-off.
#
# Kept in its own heredoc, delimited by GL_YIELD_EOF, so
# scripts/validate-packaging.sh can run it against sample suede.toml files.
GL_YIELD_BLOCK="$(cat <<'GL_YIELD_EOF'
  suede_gl_yield="$(sed -n -E 's/^[[:space:]]*gl_yield[[:space:]]*=[[:space:]]*"?([A-Za-z_]+)"?.*/\1/p' \
    "$HOME/.config/suede/suede.toml" 2>/dev/null | head -n1)"
  case "$suede_gl_yield" in
    ""|usleep) export __GL_YIELD=USLEEP ;;
    nothing) export __GL_YIELD=NOTHING ;;
  esac
  unset suede_gl_yield
GL_YIELD_EOF
)"

# --- 3b. nvidia_uvm at boot ---------------------------------------------
# The browser's VA-API decode needs nvidia_uvm, which nothing loads on its own.
# Only on machines with an NVIDIA GPU: a PCI device with vendor 10de, a loaded
# proprietary driver, or an installed nvidia module. SUEDE_FAKE_ROOT prefixes
# every path this function reads or writes so scripts/validate-packaging.sh
# can run it against a scratch tree; it is empty in real use.
step "Loading nvidia_uvm at boot (NVIDIA machines only)"
# BEGIN NVIDIA_UVM_FUNCTION
install_nvidia_uvm() {
  local root="${SUEDE_FAKE_ROOT:-}" found=""
  if lspci -n -d 10de: 2>/dev/null | grep -q .; then found="a PCI device with vendor 10de"
  elif [[ -e "$root/proc/driver/nvidia/version" ]]; then found="the loaded NVIDIA driver"
  elif modinfo nvidia >/dev/null 2>&1; then found="an installed nvidia module"
  fi
  if [[ -z "$found" ]]; then
    echo "  no NVIDIA GPU found: skipping"
    return 0
  fi
  echo "  NVIDIA GPU detected ($found)"
  install -d "$root/etc/modules-load.d"
  printf 'nvidia-uvm\n' > "$root/etc/modules-load.d/nvidia-uvm.conf"
  echo "  $root/etc/modules-load.d/nvidia-uvm.conf: nvidia-uvm"
  local rule="$root/etc/udev/rules.d/70-suede-nvidia-uvm.rules"
  if [[ -x "$root/usr/bin/nvidia-modprobe" ]]; then
    install -d "$root/etc/udev/rules.d"
    printf 'KERNEL=="nvidia_uvm", RUN+="/usr/bin/nvidia-modprobe -c0 -u"\n' > "$rule"
    echo "  $rule installed"
  else
    # Without nvidia-modprobe, module load plus the driver's own node
    # creation is all there is; drop a rule left by an earlier run.
    rm -f "$rule"
    echo "  nvidia-modprobe not found: relying on module load alone"
  fi
}
# END NVIDIA_UVM_FUNCTION
install_nvidia_uvm

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
$SCANOUT_BLOCK
$PRESENTATION_BLOCK
$GL_YIELD_BLOCK
  if [ "\$suede_session" = "direct" ]; then
    clear
    exec sway${SWAY_FLAGS} > "\$HOME/.sway.log" 2>&1
  fi
  unset suede_session
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

# --- 4b. Bootstrap configuration ----------------------------------------
#
# Only written when the display path was chosen explicitly: the default needs
# no file at all, and a provisioning run must not quietly rewrite settings an
# operator put here by hand. The login profile above reads whatever ends up
# here, so these two keys are the whole of the choice.
#
# Rewrites an existing key, commented or not, rather than appending a second
# one: TOML takes the first, so an appended line would be read as a no-op and
# the machine would boot into the wrong path.
#
# Kept between BEGIN/END BOOTSTRAP_FUNCTIONS markers so
# scripts/validate-packaging.sh can lift the functions out and run them against
# a scratch home. They use SUEDE_TOML, APPLIANCE_USER, SUEDE_PORT, TOKEN_MODE
# LOCAL_ONLY and REBOOT, all set before they are called.
# BEGIN BOOTSTRAP_FUNCTIONS
set_bootstrap_key() {
  local key="$1" value="$2" comment="$3" shown="$2"
  # A token never reaches the log of a provisioning run.
  [[ "$key" == "token" ]] && shown='"(hidden)"'
  if grep -qE "^[[:space:]]*#?[[:space:]]*$key[[:space:]]*=" "$SUEDE_TOML"; then
    sed -i -E "s|^[[:space:]]*#?[[:space:]]*$key[[:space:]]*=.*|$key = $value|" "$SUEDE_TOML"
  else
    printf '\n%s\n%s = %s\n' "$comment" "$key" "$value" >> "$SUEDE_TOML"
  fi
  echo "  $SUEDE_TOML: $key = $shown"
}

token_is_set() {
  grep -qE '^[[:space:]]*token[[:space:]]*=' "$SUEDE_TOML" 2>/dev/null
}

# Created only when something is about to be written, owner-only from the
# start: the file may end up holding a token.
ensure_suede_toml() {
  local group
  group="$(id -gn "$APPLIANCE_USER")"
  install -d -o "$APPLIANCE_USER" -g "$group" "$(dirname "$SUEDE_TOML")"
  [[ -e "$SUEDE_TOML" ]] || install -m 600 -o "$APPLIANCE_USER" -g "$group" /dev/null "$SUEDE_TOML"
}

random_token() {
  head -c 48 /dev/urandom | base64 -w0 | tr -d '/+=' | cut -c1-40
}

# Asks one yes/no question on the terminal; only ever called when stdin is one.
# Anything but an answer starting with y (including Enter) is no.
ask_yes_no() {
  local answer
  read -r -p "$1" answer || answer=""
  [[ "${answer,,}" == y* ]]
}

# Decide and write the two security settings. Each question is asked only when
# there is a terminal and its own flag (--token, --local-only) is absent; with
# no terminal it takes its default, so the API stays open and the bind alone.
# Never replaces a token that is already there without a yes at the terminal,
# and never prints one anywhere but the terminal. Leaves LOCAL_ONLY as yes or
# no for the firewall step.
configure_security() {
  local token="" shown_token=0
  local have_tty=0
  [[ -t 0 ]] && have_tty=1

  if [[ -z "$TOKEN_MODE" && "$have_tty" -eq 1 ]]; then
    if token_is_set; then
      echo "  A token is already set in $SUEDE_TOML; leaving it."
    elif ask_yes_no "Require an API token? [y/N] "; then
      TOKEN_MODE=generate
    fi
  fi

  if [[ -z "$LOCAL_ONLY" && "$have_tty" -eq 1 ]]; then
    if ask_yes_no "Bind Suede to this machine only, for use behind a reverse proxy? [y/N] "; then
      LOCAL_ONLY=yes
    else
      LOCAL_ONLY=no
    fi
  fi
  [[ -n "$LOCAL_ONLY" ]] || LOCAL_ONLY=no

  case "$TOKEN_MODE" in
    "") ;;
    none) echo "  token: none requested; any token already set is left as it is" ;;
    *)
      if token_is_set; then
        if [[ "$have_tty" -eq 1 ]] \
           && ask_yes_no "A token is already set in $SUEDE_TOML. Replace it? [y/N] "; then
          :
        else
          echo "  token: already set; not replaced (rerun at a terminal to replace it)"
          TOKEN_MODE=""
        fi
      fi
      if [[ -n "$TOKEN_MODE" ]]; then
        if [[ "$TOKEN_MODE" == generate ]]; then
          token="$(random_token)"
          shown_token=1
        else
          token="$TOKEN_MODE"
        fi
        # Mode first, so the file is never readable by others while it holds
        # the token.
        ensure_suede_toml
        chmod 600 "$SUEDE_TOML"
        set_bootstrap_key token "\"$token\"" \
"# Every API request must carry this as \`Authorization: Bearer <token>\`.
# Anything that fronts the appliance for people (a reverse proxy) supplies it
# for them. Keep this file readable by the appliance user only."
        chown "$APPLIANCE_USER:$(id -gn "$APPLIANCE_USER")" "$SUEDE_TOML"
        chmod 600 "$SUEDE_TOML"
        if [[ "$shown_token" -eq 1 && -t 1 ]]; then
          echo
          echo "  API token (shown once; it is stored in $SUEDE_TOML):"
          echo "    $token"
          echo
        elif [[ "$shown_token" -eq 1 ]]; then
          echo "  The generated token is stored in $SUEDE_TOML. It is not shown"
          echo "  here because this output is not a terminal."
        fi
      fi ;;
  esac

  if [[ "$LOCAL_ONLY" == yes ]]; then
    ensure_suede_toml
    set_bootstrap_key bind "\"127.0.0.1:${SUEDE_PORT}\"" \
"# Reachable from this machine only: a reverse proxy here or an SSH tunnel is
# the way in. The local listener for apps stays one port above."
  elif grep -qE '^[[:space:]]*bind[[:space:]]*=[[:space:]]*"127\.' "$SUEDE_TOML" 2>/dev/null; then
    echo "  bind: $SUEDE_TOML already binds to loopback; left as it is (local-only: no)"
  fi
  return 0
}

# Whether to reboot at the end: --reboot yes|no, otherwise asked at a terminal,
# otherwise no.
reboot_wanted() {
  case "$REBOOT" in
    yes) return 0 ;;
    no) return 1 ;;
  esac
  [[ -t 0 ]] || return 1
  echo
  ask_yes_no "Reboot now to apply everything? [y/N] "
}
# END BOOTSTRAP_FUNCTIONS

if [[ "$ALLOW_OVERLAPS" -eq 1 || "$DIRECT_SCANOUT" -eq 0 ]]; then
  step "Recording the display path in suede.toml"
  SUEDE_DIR="$USER_HOME/.config/suede"
  install -d -o "$APPLIANCE_USER" -g "$APPLIANCE_USER" "$SUEDE_DIR"
  SUEDE_TOML="$SUEDE_DIR/suede.toml"
  touch "$SUEDE_TOML"

  if [[ "$ALLOW_OVERLAPS" -eq 1 ]]; then
    set_bootstrap_key allow_overlaps true \
"# Outputs may overlap in canvas space, so every multi-output layout goes
# through the slicer and each display gets its own buffer. The login profile
# derives WLR_SCENE_DISABLE_DIRECT_SCANOUT from this and direct_scanout."
  fi
  if [[ "$DIRECT_SCANOUT" -eq 0 ]]; then
    set_bootstrap_key direct_scanout false \
"# Have the compositor composite each slice instead of flipping it straight to
# the display controller — the comparison arm. Only means anything while
# allow_overlaps is true; remove it (the default is true) to scan out again."
  fi
  chown "$APPLIANCE_USER:$APPLIANCE_USER" "$SUEDE_TOML"

  # The drop-in is the health check's business, not this script's: it lives in
  # the user's systemd directory and a running compositor has to be restarted
  # for either of them to matter. It only contradicts what was just written
  # when scanout is the thing being asked for.
  if [[ "$ALLOW_OVERLAPS" -eq 1 && "$DIRECT_SCANOUT" -eq 1 ]]; then
    LEFTOVER="$(ls "$USER_HOME"/.config/systemd/user/*.service.d/10-suede-scanout.conf 2>/dev/null || true)"
    if [[ -n "$LEFTOVER" ]]; then
      echo "  NOTE: a Suede scanout drop-in is still present and would undo this:"
      echo "$LEFTOVER" | sed 's/^/          /'
      echo "        Apply the direct-scanout check's fix, then restart sway."
    fi
  fi
fi

# --- 4c. API token and loopback bind -----------------------------------
#
# Defaults stay open: nothing here writes unless a flag asked for it or a
# person at a terminal said yes. LOCAL_ONLY stays empty (treated as no) when
# nothing was asked.
SUEDE_TOML="$USER_HOME/.config/suede/suede.toml"
if [[ -n "$TOKEN_MODE" || -n "$LOCAL_ONLY" || -t 0 ]]; then
  step "Securing the API"
  configure_security
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
if [[ "$LOCAL_ONLY" == yes ]]; then
  echo "  skipped (--local-only: nothing outside this machine should reach it)"
elif [[ "$OPEN_FIREWALL" -eq 0 ]]; then
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
  Open http://$(hostname):${SUEDE_PORT}/ from another computer to configure it.
EOF
if [[ "$LOCAL_ONLY" == yes ]]; then
  cat <<EOF2

  --local-only: the API answers on 127.0.0.1:${SUEDE_PORT} only. Reach it through
  a reverse proxy on this machine or an SSH tunnel; see docs/reverse-proxy.md.
EOF2
fi

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

if reboot_wanted; then
  reboot
fi
exit 0
