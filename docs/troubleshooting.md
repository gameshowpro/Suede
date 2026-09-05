# Troubleshooting

Start with the health checks. `GET /api/v1/system/checks`, or the banner at the top of the web UI, covers most of what goes wrong and tells you which of it Suede can fix itself.

```bash
curl -s http://appliance:9088/api/v1/system/checks | python3 -m json.tool
```

Then look at `GET /api/v1/status`, which lists every piece of desired state that could not be realized.

## How warnings reach you {: #how-warnings-reach-you }

Suede raises two kinds of complaint, and both are built so that they end somewhere you can act.

A **check** is a fact about the machine — a missing package, a compositor setting that will silently break spanning. Each carries a `status` of `pass`, `warn` or `fail`, and:

| Field | Meaning |
| --- | --- |
| `docsUrl` | The page on <https://suede.gameshow.pro> describing the problem and its manual remedy. |
| `fixAvailable` | Whether Suede can apply the remedy itself. |
| `fixDescription` | Exactly what applying it would change, so you can decide before it happens. |

A **divergence** is a fact about your configuration — something you asked for that could not be applied. It carries a `kind`, the `subject` it concerns, and its own `docsUrl`. Divergences have no fix button, because the remedy is always to change what you asked for or to change the hardware.

`docsBaseUrl` in the bootstrap file decides where those links point; set it if you host your own copy of these pages.

The web UI collects both into a banner above every tab, failures first, and shows the fix description with a confirmation step before anything is applied — a fix writes to the machine, so it should never happen on a stray click. Applying one calls:

```bash
curl -X POST http://appliance:9088/api/v1/system/checks/direct-scanout/fix
```

which returns what it did. Only four checks offer this: `direct-scanout`, `sway-config`, `systemd-unit` and `pipewire`. Everything else needs a package installed or a cable moved, and says so.

## Suede is not reachable

```bash
systemctl --user status suede
journalctl --user -u suede -f
```

If the service is not running at all, the unit is probably not enabled — that is the `systemd-unit` check, and it has a fix button. If it restarts in a loop, the log will say why.

Remember the unit is a **user** service, tied to the session. `sudo systemctl status suede` will not find it.

### Enabling the unit fails with "Unit ... does not exist" {: #enable-unit-does-not-exist }

```
Failed to enable unit: Unit /home/you/.config/systemd/user/sway-session.target.wants/suede.service does not exist
```

The message (systemd 257, as shipped by Debian 13) is misleading: the unit is
not missing, the enablement symlink could not be **created** — because
`~/.config` itself is owned by root, so the user's systemd manager cannot make
`~/.config/systemd` inside it. A headless machine gets into that state
easily: no desktop session has ever run, so `~/.config` does not exist until
the first root-run tool — an editor under sudo, a hand-run `install -d` —
creates it, owned by root.

```bash
stat -c '%U' ~/.config          # must print your user, not root
sudo chown "$USER:$USER" ~/.config
systemctl --user enable suede.service
```

Provisioning checks for this and repairs it, whatever created the situation.

## "no sway IPC socket found"

Suede runs inside the Sway session and finds the socket through `$SWAYSOCK`, `$XDG_RUNTIME_DIR`, or `/run/user/*`. It waits for the socket rather than failing, so this usually means Sway is not running, or the service is running outside the session.

Over SSH, the variable is not set. Borrow it from the session:

```bash
export SWAYSOCK=$(ls /run/user/$(id -u)/sway-ipc.* | head -1)
swaymsg -t get_outputs
```

If Sway itself is not starting, check `~/.sway.log` and confirm auto-login is landing on `tty1`.

### A compositor restart strands the daemon {: #compositor-restart }

Sway's IPC socket path contains its process id, so a compositor that restarts
comes back on a different path. A running daemon captured `SWAYSOCK` from its
environment at launch and cannot follow, so it holds a socket that no longer
exists: `healthz` reports `"sway": false`, and reconciliation raises a
`sway_unreachable` divergence rather than claiming to be synced, because a
pass that cannot reach the compositor has verified nothing — the outputs it
lists are simply the last ones it saw.

The fix is to restart the daemon:

```bash
systemctl --user restart suede.service
```

On a correctly provisioned appliance this is automatic: `suede.service` is
`PartOf=graphical-session.target`, so a compositor going away stops it, and
starting the session starts it again with the new environment. It is worth
knowing about anyway, because a machine running a second compositor by hand
breaks that chain — whichever one publishes `SWAYSOCK` last is the one the
daemon inherits, and it may not be the one with the screens on it.

## A display stays dark

Check what Sway actually sees:

```bash
curl -s http://appliance:9088/api/v1/outputs | python3 -m json.tool
```

| Symptom | Cause |
|---|---|
| The output is missing entirely | Cable, EDID, or the connector is genuinely absent. `status` reports `output_not_connected` |
| Present but `"active": false` | No configuration entry, or one with `"enable": false` |
| Active but the wrong mode | The requested mode is not advertised; `status` reports `mode_unsupported` |
| Configured but nothing changed | Look for `command_failed` divergences — Sway rejected the command |

A configured output that is not connected is deliberately **not** an error. Suede keeps the configuration and applies it the moment the display appears.

### Everything reports success and the panel is still dark {: #mode-advertised-but-dark }

A display can advertise a timing in its EDID that it will not actually sync.
Nothing in the stack can see this: the kernel drives the signal without
error, Sway reports the output active in the requested mode, every health
check passes — and the panel shows nothing. It is real, not hypothetical: a
Samsung U28E510 4K monitor on a Raspberry Pi 5 stayed dark on the
1920×1080@60 it advertises, because its EDID carries *two* 1080p60 timings —
a DMT one it rejects (listed first, so that is what a request for `60`
gets) and the CEA-861 one it accepts.

When a display stays dark on a mode it claims to support:

1. Try the display's **preferred mode** first (the top of its `modes` list) —
   that one, it syncs.
2. Then try the neighbouring refresh variant — `59.94` instead of `60` (or
   `29.97` instead of `30`). The broadcast-rate variants are usually the
   CEA-861 timings, which HDMI-native displays are built around; an exact
   fractional refresh selects a single advertised mode instead of letting
   the compositor pick between identically-numbered ones.

## A browser will not start

```bash
curl -s http://appliance:9088/api/v1/apps | python3 -m json.tool
```

| State | Meaning |
|---|---|
| `waitingForOutput` | The target output is not connected or not enabled |
| `backoff` | It exited and is waiting out the restart delay; `detail` says why |
| `crashed` | The restart policy declined a relaunch |
| `starting` | Launched, but no window has appeared yet |

### "Failed to create a ProcessSingleton for your profile directory" {: #process-singleton }

The app crash-loops, and Chromium's own log says:

```
Failed to create /home/you/.local/state/suede/profiles/<app>/SingletonLock:
  Permission denied (13)
Failed to create a ProcessSingleton for your profile directory. ...
  Aborting now to avoid profile corruption.
```

Chromium is describing a symptom, not the cause. Nothing is corrupt: the
browser is a **snap**, and a confined snap may write anywhere in `$HOME`
*except* a hidden directory — which is exactly where Suede keeps its state.

Current versions of Suede do not choose a snap at all, so this only appears
when one was named deliberately with `launcher.program`, and even then the
profile is placed under `~/snap/<name>/common/suede-profiles/<app>` so it
works. If you are seeing it, either the daemon predates that behaviour or
something else is passing `--user-data-dir`. Check which binary is actually
being launched:

```bash
curl -s http://appliance:9088/api/v1/system/checks   | python3 -c 'import sys,json;print([c for c in json.load(sys.stdin) if c["id"]=="browsers"][0]["detail"])'
```

The reliable fix is a browser from a `.deb` rather than a snap — on Debian
`apt install chromium`, on Ubuntu Google Chrome's own package — because a
snap also restarts itself whenever it updates, which on an appliance means
the screens go blank mid-show.

An app that cannot run does not stay a private matter: after three
consecutive failed launches Suede raises an `app_crash_looping` divergence and
the appliance reports `degraded`, and an app whose restart policy declines a
relaunch raises `app_halted` at once. Both name the app and carry the reason,
so `GET /api/v1/status` is enough to see what is wrong:

```bash
curl -s http://appliance:9088/api/v1/status | python3 -m json.tool
```

The commonest reason is the simplest: the browser the app asks for is not
installed. A `firefox-kiosk` app on a machine with only Chromium can never
start, however healthy everything else is. The `browsers` health check reports
this before it happens, because it compares the *configured* apps against what
is actually present rather than just confirming that some browser exists:

```
FAIL  configured apps cannot start: test-card needs firefox or firefox-esr.
      Install the browser, or change those apps to one that is present
      (available: /usr/bin/google-chrome-stable ...)
```

Note that Firefox is also subject to the autoplay policy Suede cannot disable
for it — see [above](#autoplay) — so `chromium-kiosk` is the better default.

An app stuck in `starting` usually means the browser is failing before it maps a window. The `browsers` health check runs `chromium --version` to catch an unusable install. Beyond that, run the same command by hand in the session:

```bash
chromium --ozone-platform=wayland --kiosk http://example.com
```

!!! tip "Two Chromium instances, one profile"
    Chromium refuses to start a second instance sharing a profile. Suede gives every app its own `--user-data-dir` automatically, so this only bites if you have passed a conflicting `--user-data-dir` in `extraArgs`.

## A spanned window mirrors instead of spanning

Every display shows the *same* part of the page rather than its own slice, even
though `GET /windows` reports the window at the full width of the layout and
sway agrees.

This is not a layout problem. When wlroots can hand a fullscreen client buffer
straight to the display controller, each output scans that buffer out from its
own origin — so a 3840-wide window on two 1920-wide displays shows pixels
0–1920 on both. Everything reports as correct, which makes it very hard to spot
from the API alone.

Start sway with direct scanout disabled:

```bash
WLR_SCENE_DISABLE_DIRECT_SCANOUT=1 sway
```

`provision.sh` sets this for you. The `direct-scanout` health check warns
whenever an application is spanning a non-overlapping layout while the running
compositor was started without it:

```bash
curl -s http://appliance:9088/api/v1/system/checks   | python3 -c 'import sys,json;print([c for c in json.load(sys.stdin) if c["id"]=="direct-scanout"])'
```

Observed with the Nvidia proprietary driver. Per-output kiosks are unaffected —
each window covers one display, so the buffer and the output match.

## A page freezes but the browser keeps running

That is exactly what the content watchdog is for. Enable it on the app, and have the page post to `{heartbeatUrl}` every 10 seconds. Suede then kills and relaunches the browser after 25 seconds of silence.

Without heartbeats there is nothing to detect: from the outside, a frozen page and a working one look identical.

If the watchdog is firing when it should not, check that the page is actually posting — `lastHeartbeat` in the app status shows the last one received.

## Suede itself stops answering, but the process is still there

There are two ways a daemon can stop working, and only one of them is
obvious. If it crashes, systemd restarts it and the log says why. The other
is quieter: the process stays alive, keeps its listening socket, and stops
doing anything at all — no reply on the API, no reconciliation, nothing new
in the log. From the outside that is indistinguishable from a dead machine.

It has happened once, on a four-projector appliance, and it stayed that way
for eighty minutes until somebody with an SSH key looked. What was found:

```
$ ss -ltn 'sport = :9088'
State   Recv-Q  Send-Q  Local Address:Port
LISTEN  129     128           0.0.0.0:9088     ← queue full, nothing accepting
```

Zero CPU, zero context switches over twenty-five seconds, every thread parked
on a futex and none in `epoll_wait` — a stalled runtime rather than a busy
one. No panic, no OOM, nothing in the journal.

**Suede now reports its own liveness to systemd**, so this recovers by itself
in under a minute. The ping is sent from a task on the async runtime, which
is the part that matters: if the runtime stops turning, the ping stops with
it. `WatchdogSec=45s` in the unit sets the deadline, and the journal names it
plainly when it fires:

```
suede.service: Watchdog timeout (limit 45s)!
suede.service: Failed with result 'watchdog'.
suede.service: Scheduled restart job, restart counter is at 1.
```

If you see that line, the daemon stopped responding and was restarted for
you. It is worth investigating rather than ignoring: the unit sets
`WatchdogSignal=SIGABRT`, so the stuck process is aborted rather than merely
killed and leaves a core behind. `coredumpctl list suede` will find it, and
`coredumpctl gdb suede` opens it with the symbols the release build now
keeps.

To check the watchdog is actually armed:

```bash
systemctl --user show suede -p WatchdogUSec --value   # expect 45s
```

Freezing the daemon with `kill -STOP $(systemctl --user show suede -p MainPID --value)`
is a fair test — it is what a stalled runtime looks like to systemd, and the
service should come back on its own within about a minute.

## Audio goes to the wrong place, or nowhere

```bash
curl -s http://appliance:9088/api/v1/audio/outputs | python3 -m json.tool
wpctl status    # what PipeWire itself thinks
```

Use the `id` field (PipeWire's `node.name`) in the app's `audio.output`; it is stable across reboots. A configured sink that is absent is reported as an `audio_sink_not_present` divergence, and the app still launches on the default sink.

If no sinks appear at all, `pw-dump` is failing — check that PipeWire is running. If sinks appear but browsers have no audio device, `pipewire-pulse` is missing; browsers reach PipeWire through its PulseAudio compatibility layer, which is what `PULSE_SINK` routing depends on.

### Only a "Dummy Output" is listed {: #dummy-output }

The most common cause on an appliance, and the most confusing, because
everything else looks healthy: PipeWire is running, `pipewire-pulse` is
serving, and one sink is listed. But that sink is `auto_null`, the dummy
PipeWire invents when it can open no audio devices at all, and anything
routed to it is discarded. Suede reports this as a **warning** on the
PipeWire health check rather than a pass.

The cause is almost always device permissions. `/dev/snd/*` is owned by
`root:audio` with no world access, and the ACLs that normally grant a
desktop user access are applied by `systemd-logind` **per session, to
sessions attached to a seat**. An appliance auto-logs in and runs its
compositor from a systemd user service, which does not reliably get a seat,
so those ACLs are never applied. Confirm it directly:

```bash
id -nG | tr ' ' '
' | grep -x audio    # is the user in the group at all?
loginctl list-sessions                  # SEAT column empty means no ACLs
ls -l /dev/snd/                         # root:audio, mode 0660
```

The fix is static group membership, which does not depend on a session:

```bash
sudo usermod -aG audio "$USER"
sudo reboot
```

A reboot is genuinely required. A running process keeps the groups it
started with, so restarting the PipeWire units is not enough — they are
spawned by a user manager that still has the old set. Provisioning does this
for you (`provision.sh` adds `audio`, `video` and `render`); a machine set up
by hand is the usual way to end up here.

Changing an app's sink **relaunches** it. That is expected: routing is applied at launch.

### The page is silent, but everything looks right {: #autoplay }

If a sink exists, the app is running, and still nothing is heard, check
whether the page was ever allowed to start playing. Browsers block audio
until a "user gesture", and an appliance never provides one. The
`chromium-kiosk` preset disables that policy; a page run some other way (an
`exec` launcher, or `firefox-kiosk`) may still be blocked.

A page can report the answer itself — `new AudioContext().state` is
`suspended` when blocked and `running` when not. Writing it into
`document.title` makes it readable straight from the API, with no access to
the machine's screen:

```bash
curl -s http://appliance:9088/api/v1/windows | python3 -m json.tool
```

Whether audio is genuinely reaching a device is a separate question, and
PipeWire answers it: a playing app appears as a `Stream/Output/Audio` node.

```bash
pw-dump | grep -A2 Stream/Output/Audio
wpctl status          # sinks in state "running" are being fed
```

## A page cannot reach a camera or capture device

Three separate things have to be true, and all three fail the same way — a
black rectangle, or `NotReadableError`, with no clue which one it was. The
`capture-devices` health check covers the third.

| Requirement | Symptom when missing | Fix |
|---|---|---|
| A secure context | `navigator.mediaDevices` is `undefined`; no prompt, no error | Serve over `https://` or from loopback. Suede otherwise passes `--unsafely-treat-insecure-origin-as-secure` for the app's own origin automatically |
| Permission | `NotAllowedError`, or a prompt nobody can click | The kiosk preset passes `--auto-accept-camera-and-microphone-capture` |
| Access to the device node | `NotReadableError`, and an empty device list | Add the user to the `video` group |

The `capture-devices` check counts `/dev/videoN` nodes rather than cameras,
because telling the two apart needs a V4L2 ioctl. Expect a machine with no
camera at all to report several: a Raspberry Pi 5 presents seventeen — one HEVC
decoder and sixteen ISP nodes — and a single USB camera presents two, a capture
node and a metadata node. What matters is that none of them is unreadable.

The third is the one that catches people, because it is invisible from inside
the browser and looks exactly like a refused permission:

```bash
ls -l /dev/video*                  # note the owning group
id                                 # is this user in it?
sudo usermod -aG video $USER       # then log out and back in
```

Group membership is fixed when a session starts, so the change does nothing
until the appliance user logs in again — on an auto-login appliance, reboot.

!!! warning "Permission granted is not device names read"
    `--auto-accept-camera-and-microphone-capture` waves each request through
    without *persisting* a grant, and without a persisted grant
    `enumerateDevices()` returns entries whose `label` and `deviceId` are both
    empty strings. Passing through the first input still works; selecting a
    device *by name* cannot, because there is no name to match. For that, grant
    the permission with a policy instead — a JSON file in
    `/etc/opt/chrome/policies/managed/` (or `/etc/chromium/policies/managed/`)
    setting `VideoCaptureAllowedUrls` and `AudioCaptureAllowedUrls` to the app's
    origin. A live `MediaStreamTrack` always knows its own `label` either way.

## Configuration was lost

It should not be. Desired state lives in `$XDG_STATE_HOME/suede/state.json`, is written atomically, and keeps a `.bak`. Package upgrades do not touch it.

If Suede fell back to an empty document, the log says so at startup. The backup is still on disk:

```bash
ls -la ~/.local/state/suede/
```

A `state.json` written by a *newer* Suede is refused rather than downgraded, so rolling back a version can look like lost configuration. The file is intact; install the newer version again.

## Everything reconciles constantly

Watch the log at debug level:

```bash
RUST_LOG=suede=debug systemctl --user restart suede
journalctl --user -u suede -f
```

A pass that never converges usually means a command silently fails to take effect — the plan asks for something, Sway reports success, and the next query shows the old value. The `command_failed` divergences and the debug log showing the same commands repeating will identify which setting.

## Getting a clean look at the wire

```bash
# Everything Suede is doing, live
curl -N http://appliance:9088/api/v1/events

# Force a pass and see the result
curl -X POST http://appliance:9088/api/v1/reconcile | python3 -m json.tool
```
