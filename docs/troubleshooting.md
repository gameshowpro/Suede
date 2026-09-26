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
daemon inherits, and it may not be the one with the displays on it.

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
Samsung U28E510 4K display on a Raspberry Pi 5 stayed dark on the
1920×1080@60 it advertises, because its EDID carries *two* 1080p60 timings —
a DMT one it rejects (listed first, so that is what a request for `60`
gets) and the CEA-861 one it accepts.

When a display stays dark on a mode it claims to support:

1. Try the display's **preferred mode** first (the top of its `modes` list) —
   that one, it syncs.
2. Then try the neighboring refresh variant — `59.94` instead of `60` (or
   `29.97` instead of `30`). The broadcast-rate variants are usually the
   CEA-861 timings, which HDMI-native displays are built around; an exact
   fractional refresh selects a single advertised mode instead of letting
   the compositor pick between identically-numbered ones.

### The displays are black but the slicer reports frames {: #slicer-presenting-to-nothing }

Only relevant to an overlapping (edge-blended) layout, where Suede's own
slicer process — not Sway — presents each projector's slice.

```bash
curl -s http://appliance:9088/api/v1/projection/stats | python3 -m json.tool
```

The signature is `canvasFps`/`presentedFps` staying healthy while every
output's `discarded` count climbs and `presented` stays at zero. That means
the slicer is still capturing the canvas and committing frames, but every
commit is landing on outputs the compositor has already destroyed — the
layer surfaces the slicer built at startup no longer belong to anything the
compositor is displaying. It happens whenever an output is disabled and re-enabled, or
unplugged and replugged, since either one destroys and recreates the
output in the compositor; a running slicer built against the old one has no
way to notice on its own unless told to.

Suede detects and corrects this itself now, three ways: the slicer notices
its own outputs disappearing from the Wayland registry and exits so the
daemon respawns it; failing that, the slicer notices two consecutive
ten-second intervals of presentation feedback answering "discarded" for
everything and exits anyway; and the reconciler forces a slicer restart on
any pass that actually changed which outputs are enabled, whether or not the
compositor removes the global. A `systemctl --user restart suede` remains a
manual fix for the same condition, and a fast way to confirm the diagnosis,
but should no longer be necessary.

## Warp mode will not activate

Only relevant to an overlapping (edge-blended) layout with `projection.mode`
set to `warp`. Check `geometry.warpAvailable` and `geometry.reason` in
`GET /api/v1/projection/stats`:

```bash
curl -s http://appliance:9088/api/v1/projection/stats | python3 -c \
  'import json,sys; print(json.load(sys.stdin)["geometry"])'
```

`warpAvailable` is tri-state: `true` (active), `false` (refused, with a
`reason`), or `null` (not yet verified — a capability probe is pending, or
none has run). A `false` result never discards the saved corner/center
calibration; it is retained and warp resumes automatically once the
condition clears. `reason` is one of:

- **"this build has no projection machinery; install a projection-enabled
  build"** — the running binary was compiled without the `projection` cargo
  feature. Install a build that has it; there is nothing to fix at runtime.
- **"warping requires allow_overlaps=true and the canvas slicer"** —
  `allow_overlaps` is not set in `suede.toml`. Re-run provisioning with
  `--allow-overlaps`, or set the key by hand and restart Sway. See
  [Overlapping layouts and direct scanout](configuration.md#direct-scanout).
- **"the selected CPU renderer supports simple rectangles only; select Auto
  or GPU to probe warp support"** — `projection.renderer` is explicitly
  `cpu`, which never attempts warp. Set it to `auto` (the default) or `gpu`.
- **A probe-in-progress or probe-not-yet-run message** ("waiting for the
  current capture/presentation capability probe", or "warp capability is not
  verified; run content or a calibration pattern to probe the selected
  pipeline") — warp has not been ruled in or out yet. Activate an app or a
  test pattern so the slicer starts and can negotiate the GPU path; if it
  stays `null`, check the daemon log for the capability probe's own error.

A dynamic reason from the running slicer child (a negotiation failure it
reported itself) can also appear here; it is not one of the four fixed
messages above and describes its own remediation.

### The wall falls back to a plain, unoverlapped arrangement {: #canvas-plan-failed }

A `canvas_plan_failed` divergence in `GET /api/v1/status` means the
configured canvas layout (Simple with a shared canvas, or Warp) could not be
turned into a plan at all — usually a momentarily inconsistent edit, such as
a slice rectangle mid-drag, rather than a lastingly broken document. Two
things happen while it lasts, and the divergence's `detail` says which:

- If a plan had previously been computed successfully, that last good
  arrangement keeps running — stale, but exactly as it looked a moment ago,
  and still not overlapping.
- If no plan has ever succeeded (for example, straight after a fresh
  configuration write that has not yet settled), the outputs fall back to a
  plain edge-to-edge tiling with no cropping or correction applied, rather
  than the raw configured positions — which, for an overlapping layout,
  would otherwise show the same pixels on more than one projector.

Check `GET /api/v1/config/projection` and each output's `geometry` for the
specific problem the divergence names (commonly a slice rectangle or a
corner pin that has drifted outside the canvas). Once the write that fixes
it lands, this divergence clears on the next reconciliation pass and the
wall returns to the configured arrangement.

### An output's settled value was rejected, not pinned {: #adopted-value-invalid }

`adopted_value_invalid` in `GET /api/v1/status` means an output settled on a
mode, scale, or transform that Sway reports back, but which the saved
document cannot actually hold — the leading case is a Warp output whose
compositor scale settles on anything other than `1.0`, which Warp mode does
not yet support. Ordinarily Suede pins ("adopts") whatever an output settles
on so a reboot keeps today's picture rather than re-negotiating from
scratch; this is the one case where it deliberately does not, because saving
that value would produce a document the daemon itself refuses to load again
next time.

The divergence's `subject` names the output and `detail` names the rejected
value. Nothing is lost — the previously saved value, or Sway's own default,
stays in effect — but the display will make the same unsupported choice
again at the next restart unless the configuration sets the value
explicitly (or the output leaves Warp mode, where the constraint does not
apply).

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

### The status says its program is not allowed {: #program-not-allowed }

```
app `signage` asks to launch `curl`, which is not in `allowed_programs`; add
it to suede.toml (or SUEDE_ALLOWED_PROGRAMS) and restart the daemon, or
change the app's launcher.
```

This is a deliberate refusal, not a fault: bootstrap's `allowed_programs`
(default: the browsers Suede knows how to drive) limits which programs any
application may launch, and this app's launcher names one that is not on the
list. It shows as `crashed`, and it stays that way — nothing retries it on a
timer, because nothing short of restarting the daemon can change what
`allowed_programs` permits, so retrying would only ever fail again the same
way.

Either add the program to `allowed_programs` (or `SUEDE_ALLOWED_PROGRAMS`)
and restart the daemon, or change the app to launch something already
permitted. See [Allowed programs](configuration.md#allowed-programs).

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
works. If you are seeing it, either the daemon predates that behavior or
something else is passing `--user-data-dir`. Check which binary is actually
being launched:

```bash
curl -s http://appliance:9088/api/v1/system/checks   | python3 -c 'import sys,json;print([c for c in json.load(sys.stdin) if c["id"]=="browsers"][0]["detail"])'
```

The reliable fix is a browser from a `.deb` rather than a snap — on Debian
`apt install chromium`, on Ubuntu Google Chrome's own package — because a
snap also restarts itself whenever it updates, which on an appliance means
the displays go blank mid-show.

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

The login profile `provision.sh` writes does this for you: it derives the
variable from `allow_overlaps` and `direct_scanout` in `suede.toml` at every
login, so on a default appliance it is always exported. The `direct-scanout`
health check warns whenever an application is spanning a tiled layout while
the running compositor was started without it. Its fix writes a systemd
drop-in on the compositor's unit — you restart the compositor yourself, since
that tears down every window — and where sway has no unit, because the login
profile started it, the fix says so and asks you to restart the session
instead:

```bash
curl -s http://appliance:9088/api/v1/system/checks   | python3 -c 'import sys,json;print([c for c in json.load(sys.stdin) if c["id"]=="direct-scanout"])'
```

Observed with the Nvidia proprietary driver. Per-output kiosks are unaffected —
each window covers one display, so the buffer and the output match.

!!! info "On an `allow_overlaps` appliance, this is the wrong fix"
    Everything above applies to the default tiling path, where one window
    genuinely spans every output. With
    [`allow_overlaps = true`](configuration.md#direct-scanout) no client ever
    spans the physical outputs: the app renders into the headless canvas and
    the slicer hands each display its own output-sized buffer, which is the
    case direct scanout was built for and cannot be mirrored by mistake.
    There the variable only costs a fullscreen compositor pass per output
    per frame, so the same check inverts — it warns while
    `WLR_SCENE_DISABLE_DIRECT_SCANOUT` is set, and its fix *removes* the
    drop-in. A machine showing this symptom while `allow_overlaps` is true is
    telling you something else is wrong: check that the slicer is running at
    all (`GET /api/v1/projection/stats`).

    The one exception is
    [`direct_scanout = false`](configuration.md#direct-scanout), which asks
    for the composited arm of the comparison on that same sliced layout: the
    variable is expected again, and the check and its fix invert back. Since
    no client spans the physical outputs either way, that costs frame rate,
    never correctness.

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

The release binary is stripped, so a backtrace taken straight off an
appliance names nothing. Each CI build keeps an unstripped copy of the same
compilation beside the package — download `suede-unstripped-amd64` (or
`-arm64`) from that run's artifacts and point gdb at it:

```bash
gdb -e suede-unstripped-amd64 -p $(systemctl --user show suede -p MainPID --value)     -batch -ex "thread apply all bt 12"
```

Threads parked in `park_internal` are idle workers and are not interesting.
Any thread stopped in `read_contended` or `write_contended` is waiting on a
lock, and two of those in different call paths is a deadlock.

To check the watchdog is actually armed:

```bash
systemctl --user show suede -p WatchdogUSec --value   # expect 45s
```

Freezing the daemon with `kill -STOP $(systemctl --user show suede -p MainPID --value)`
is a fair test — it is what a stalled runtime looks like to systemd, and the
service should come back on its own within about a minute.

## Audio goes to the wrong place, or nowhere

```bash
curl -s http://appliance:9088/api/v1/av | python3 -m json.tool
wpctl status    # what PipeWire itself thinks
```

Use the `id` field (PipeWire's `node.name`) from `.audioOutputs` in the app's `audio.output`; it is stable across reboots. A configured sink that is absent is reported as an `audio_sink_not_present` divergence, and the app still launches on the default sink.

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
the machine's display:

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
| Permission | `NotAllowedError`, or a prompt nobody can click | `grantCapture` (on by default) grants the app's origin; the kiosk preset's `--auto-accept-camera-and-microphone-capture` covers any other origin |
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
    empty strings, so a page cannot choose a device by name. `grantCapture` (on
    by default) closes that gap: before every launch Suede writes a persistent
    grant for the origin of the app's configured URI into its private profile.
    Blank labels therefore mean either `grantCapture` is off, or the page is on
    a different origin than the one configured — it redirected, or loads the
    capture code in a frame from another host. A live `MediaStreamTrack` always
    knows its own `label` either way.

## Configuration was lost

It should not be. Desired state lives in `$XDG_STATE_HOME/suede/state.json`, is written atomically, and keeps a `.bak` copy of the last successful save. Package upgrades do not touch it.

```bash
ls -la ~/.local/state/suede/
```

A `state.json` written by a *newer* Suede is refused rather than downgraded, so rolling back a version can look like lost configuration. The file is intact; install the newer version again.

### The saved file needed repair at startup {: #state-document-repaired }

An old, hand-edited, or otherwise malformed `state.json` does not stop the
daemon from starting. Instead it is repaired field by field — an unknown
field is dropped, a section that cannot be read falls back to its defaults,
and a document that still fails validation is degraded piece by piece until
it passes — and a `state_document_repaired` divergence appears in
`GET /api/v1/status` for every repair that was needed, naming what changed.
This is normal after restoring a very old backup or editing the file by
hand; it is not normal after an ordinary restart.

Nothing about the file as found is silently rewritten: it is copied to
`state.json.rejected` before the repaired document is saved over it, so the
original is always available to compare against or recover fields from by
hand:

```bash
diff <(python3 -m json.tool ~/.local/state/suede/state.json.rejected) \
     <(python3 -m json.tool ~/.local/state/suede/state.json)
```

Once the document has been re-saved through the API (even unchanged), the
repaired shape becomes the new saved one and the divergence stops appearing.

### A change is live but was not saved {: #state-not-persisted }

`state_not_persisted` in `GET /api/v1/status` means a write was accepted and
is already running on the outputs, but the save to `state.json` itself
failed — commonly a full or read-only state directory. `detail` names the
revision affected and the underlying error. The appliance is not
misconfigured; it is one restart away from losing exactly that change, so
treat this the same as a low-disk-space warning: free space or fix the
directory's permissions, then repeat the write (or any write — the next
successful save clears the divergence regardless of which one it was).

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
