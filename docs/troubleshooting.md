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

## "no sway IPC socket found"

Suede runs inside the Sway session and finds the socket through `$SWAYSOCK`, `$XDG_RUNTIME_DIR`, or `/run/user/*`. It waits for the socket rather than failing, so this usually means Sway is not running, or the service is running outside the session.

Over SSH, the variable is not set. Borrow it from the session:

```bash
export SWAYSOCK=$(ls /run/user/$(id -u)/sway-ipc.* | head -1)
swaymsg -t get_outputs
```

If Sway itself is not starting, check `~/.sway.log` and confirm auto-login is landing on `tty1`.

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
whenever an app has `spanOutputs: true` while the running compositor was
started without it:

```bash
curl -s http://appliance:9088/api/v1/system/checks   | python3 -c 'import sys,json;print([c for c in json.load(sys.stdin) if c["id"]=="direct-scanout"])'
```

Observed with the Nvidia proprietary driver. Per-output kiosks are unaffected —
each window covers one display, so the buffer and the output match.

## A page freezes but the browser keeps running

That is exactly what the content watchdog is for. Enable it on the app, and have the page post to `{heartbeatUrl}` every 10 seconds. Suede then kills and relaunches the browser after 25 seconds of silence.

Without heartbeats there is nothing to detect: from the outside, a frozen page and a working one look identical.

If the watchdog is firing when it should not, check that the page is actually posting — `lastHeartbeat` in the app status shows the last one received.

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
