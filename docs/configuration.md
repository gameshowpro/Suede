# Configuration

There are two kinds of configuration, and the split is a hard rule:

- **Bootstrap configuration** is anything that must be known before the API can serve — the bind address, the token, the state directory. It lives in a file, is read once at startup, and is never written by Suede.
- **Desired state** is everything else: outputs, applications, audio routing, daemon settings. It is owned by the API, persisted by Suede, and reconciled continuously.

Terms below — canvas, slice, warp, output, display — are used exactly as
defined in the [pipeline vocabulary](how-it-works.md#vocabulary).

## Bootstrap configuration

Read from `$XDG_CONFIG_HOME/suede/suede.toml` (usually `~/.config/suede/suede.toml`). Every value but `allow_overlaps` and `direct_scanout` can be overridden by an environment variable, which wins — those two describe how the compositor was started, which a variable on Suede's own process cannot change. A missing file means all defaults.

```toml
--8<-- "examples/suede.toml"
```

| Key | Environment | Default | Meaning |
|---|---|---|---|
| `bind` | `SUEDE_BIND` | `0.0.0.0:9088` | Address the HTTP server binds to |
| `token` | `SUEDE_TOKEN` | unset | Bearer token; setting it disables the web UI |
| `state_dir` | `SUEDE_STATE_DIR` | `$XDG_STATE_HOME/suede` | Where desired state is persisted |
| `docs_base_url` | `SUEDE_DOCS_BASE_URL` | `https://suede.gameshow.pro/` | Base for health-check documentation links |
| `power` | `SUEDE_POWER` | `[]` (none) | Host power operations this appliance may perform — see [Host power control](#host-power) |
| `allowed_programs` | `SUEDE_ALLOWED_PROGRAMS` | the browsers Suede knows how to drive | Programs applications may launch — see [Allowed programs](#allowed-programs) |
| `allow_overlaps` | — | `false` | Whether outputs may overlap in canvas space, and so which display path this machine runs — see [Overlapping layouts and direct scanout](#direct-scanout) |
| `direct_scanout` | — | `true` | Whether the compositor may flip the slicer's buffers straight to the display controllers; only meaningful with `allow_overlaps = true` — see [Overlapping layouts and direct scanout](#direct-scanout) |

### Host power control {: #host-power }

`POST /api/v1/system/power` can ask the host to `reboot` or `poweroff`, but
only for the verbs listed in bootstrap's `power` (or `SUEDE_POWER`, a
comma-separated override, e.g. `SUEDE_POWER=reboot,poweroff`). Empty is the
default: a display appliance should not be able to turn itself off because
somebody found the button.

That list lives in the bootstrap file, not in desired state, and that is
deliberate: desired state is writable through the very API the permission is
meant to constrain, so a gate the caller can open by writing configuration is
not a gate. `GET /api/v1/system` reports the permitted verbs (`powerVerbs`)
so a UI can disable and explain its buttons rather than offering ones that
would be refused.

A request must also repeat the machine's hostname in `confirm`, exactly as
`GET /system` reports it. This is not a secret — it is proof the caller meant
*this* machine: a retry aimed at the wrong appliance, a stray script, or a
fuzzer does not know it, and the cost of being wrong here is a dark video
wall mid-show.

```bash
curl -X POST -H 'content-type: application/json' \
  -d '{"verb":"reboot","confirm":"wall-3"}' \
  http://appliance:9088/api/v1/system/power
```

The daemon runs as a user service, so the action still has to clear logind:
Suede runs plain `systemctl reboot` / `systemctl poweroff`, not `--user`, and
whether that succeeds is polkit's call, not Suede's. A refusal — no active
session, no policy grant — surfaces as `503` carrying polkit's own message.

!!! warning "A declaration of intent, not a security boundary"
    `power` decides which buttons the appliance offers, nothing more. An API
    client that can write configuration can already define an application
    that runs any program — see [Raw Sway commands](#raw-sway-commands) —
    so the protection against a hostile caller is the network and the bearer
    token, not this list. Do not treat it as one.

### Allowed programs {: #allowed-programs }

An `exec` launcher runs any executable verbatim, and applications are
configured through the API — so by default, any client that can write
configuration can run any program as the session user. `allowed_programs`
lets an appliance say "this machine only ever runs a browser":

```toml
allowed_programs = ["chromium", "google-chrome-stable"]
```

Matching is on the program's **file name**, so a path works the same as a
bare name: `/usr/local/bin/chromium` and `chromium` are the same program as
far as this list is concerned. The comparison is case-sensitive, as Linux
file names are.

The default is the browser binaries the launcher presets know how to find —
`chromium`, `chromium-browser`, `google-chrome-stable`, `google-chrome`,
`firefox`, and `firefox-esr` — so a stock appliance runs kiosk pages and
nothing else, without needing an explicit list. `["*"]` lifts the
restriction entirely, and an empty list (`allowed_programs = []`) permits
nothing, exactly as an empty allowlist means everywhere else in Suede — a
legitimate way to freeze a machine so that no application can start at all.

Like `power`, this lives in the bootstrap file rather than in desired state:
desired state — including which program an application launches — is
writable through the very API this key is meant to constrain, so a gate the
caller can open by writing configuration would not be a gate.

An application whose launcher names a program that is not on the list is
never spawned. Its status shows `crashed`, and `GET /api/v1/status` carries
an `app_program_not_allowed` divergence naming the app, the program it
asked for, and the key that would permit it:

```
app `signage` asks to launch `curl`, which is not in `allowed_programs`; add
it to suede.toml (or SUEDE_ALLOWED_PROGRAMS) and restart the daemon, or
change the app's launcher.
```

Because `allowed_programs` is bootstrap configuration, nothing short of
restarting the daemon can change what it permits — so, unlike an ordinary
crash, this is refused once and left alone rather than retried on the
restart timer, which could never do anything but fail again.

!!! warning "A declaration of intent, not a full boundary"
    This stops the direct route — an application configured to launch
    something other than a browser — and makes the intent explicit, but it
    does not contain a determined caller. Chromium itself accepts arguments
    such as `--gpu-launcher` that make it spawn a helper program of the
    caller's choosing, and `extraArgs` is part of desired state, which an API
    client can already write. As with `power`, the boundary that actually
    holds against a hostile client is the network and the bearer token — see
    [Raw Sway commands](#raw-sway-commands). Do not treat this list as more
    than what it is.

### Overlapping layouts and direct scanout {: #direct-scanout }

An appliance runs one of two display paths, and `allow_overlaps` is where it
says which; `direct_scanout` then decides, within the second path, whether the
compositor flips the slicer's buffers or composites them. Both are facts about
how the machine's compositor was started, which is why they live in the
bootstrap file and have no environment override: a client writing them through
the API would only be claiming an environment that is already fixed.

=== "`allow_overlaps = false` (default)"

    Sway tiles the layout itself. One application covers every display as a
    single window (`fullscreen enable global`), and a write whose output
    rectangles intersect is **rejected** — sway's single global coordinate
    space gives every output the same pixels in a shared region, so an
    overlap here is not a projector overlap, it is a layout that cannot be
    rendered as drawn. The error names both outputs and the overlap:

    ```
    outputs HDMI-A-1 and HDMI-A-2 overlap by 160x1080 pixels, and this
    appliance tiles: set allow_overlaps = true in suede.toml to project
    overlapping layouts through the slicer, or move them apart
    ```

    The compositor must run with `WLR_SCENE_DISABLE_DIRECT_SCANOUT=1`, or
    that one spanning window is handed straight to each display controller
    and every display shows the same part of it. `provision.sh` exports it,
    and the `direct-scanout` health check warns when it is missing.

    `direct_scanout` means nothing here, and writing `direct_scanout = true`
    beside it is refused at startup, naming both keys: it asks for exactly
    the arrangement that mirrors. Leaving the key out is not an error — its
    default is `true`, and a default cannot be a request.

=== "`allow_overlaps = true`"

    Every layout of two or more outputs goes through the slicer, overlapping
    or not. The application renders into the headless canvas and each display
    is handed one private, output-sized buffer — see
    [Projection and edge blending](#projection-edge-blending). A tiled layout
    is sliced with no seams, so it costs a capture and a blend pass and buys
    a display path a driver cannot mirror by mistake; `projection.blend`
    still governs the ramps wherever the layout does overlap.

    With `direct_scanout` at its default `true`, the compositor must run
    **without** `WLR_SCENE_DISABLE_DIRECT_SCANOUT`: no client ever spans the
    physical outputs, so the mirroring bug cannot happen, and the variable
    costs a fullscreen compositor pass per output per frame for nothing.
    The `direct-scanout` check inverts to match — it warns while the variable
    is set, and its fix removes the drop-in that sets it.

=== "`allow_overlaps = true`, `direct_scanout = false`"

    The same sliced layout, deliberately composited instead of flipped: the
    compositor must run **with** `WLR_SCENE_DISABLE_DIRECT_SCANOUT` again,
    and the check and its fix invert once more — the fix writes the drop-in
    rather than removing it.

    This is the comparison arm, not a lesser mode. Direct scanout removes one
    fullscreen compositor pass per output per frame, but it also changes how
    long the compositor holds each buffer and when the flip happens, and on
    one machine it cost more compositor CPU than it saved. The key exists so
    that is measurable on the machine in front of you rather than argued
    about.

    For future architectural plans to bypass the compositor presentation pass entirely via direct Vulkan display acquisition, see the [Direct-to-Display Plan](plans/VK_KHR_display.md).

A single-output layout is never sliced in any of the three.

No fix ever restarts sway: that would tear down every window on every display.
And on an appliance provisioned by `provision.sh` there is usually no unit to
write a drop-in on at all — sway is started by the login profile on tty1, and
that profile block derives `WLR_SCENE_DISABLE_DIRECT_SCANOUT` from these two
keys every time it runs. There, restarting the session *is* the fix.

`provision.sh --allow-overlaps` writes `allow_overlaps = true` into the user's
`suede.toml`, and `--no-direct-scanout` writes `direct_scanout = false` beside
it. Neither exports anything itself; the profile block reads the file.

With `allow_overlaps = true`, flipping `direct_scanout` and restarting sway
switches between a flipped and a composited wall while nothing else about the
machine changes, which is what makes the two directly comparable. [Comparing
the two](how-it-works.md#comparing-the-two) is the procedure, and what to read
in `GET /projection/stats` under each arm.

## Desired state

One JSON document, written through `PUT /api/v1/config` or section by section. Here is a complete four-output appliance:

```json
--8<-- "examples/four-output-appliance.json"
```

Writes are validated synchronously and return once **persisted**, not once applied — reconciliation may take seconds, or be impossible right now because a display is unplugged. Add `?wait=<seconds>` to block until it settles.

The document carries a server-managed persisted `revision`. Full-document
responses and every configuration write response return it as a quoted `ETag`;
send that value back as `If-Match` to make a write conditional. They also return
`X-Config-Generation`, an in-memory generation that changes for every preview,
save, or revert. Send it back as `If-Config-Generation` when editing a live
working copy. `X-Config-Epoch` identifies the daemon's current in-memory store;
send it back as `If-Config-Epoch`. A stale revision, generation, or epoch gets
`409 Conflict`. Use all three headers for an editor: the revision protects
saved documents, the generation prevents one client from overwriting another
client's preview that shares the same saved revision, and the epoch prevents a
daemon restart from making a reset generation counter look current.

### Outputs

| Field | Type | Default | Meaning |
|---|---|---|---|
| `match` | object | required | Which physical output this applies to |
| `enable` | bool | `true` | `false` actively disables the output |
| `mode` | object \| null | null | `{width, height, refreshHz}`; null leaves Sway's preferred mode, which Suede then pins — see [Adopted values](#adopted-values). A requested `refreshHz` is resolved to the nearest advertised rate within 1 Hz — see [Refresh rates](#refresh-rates) |
| `position` | object \| null | null | `{x, y}` in the global layout |
| `geometry` | object \| null | null | Shared canvas slice crop plus retained Warp correction; see [Canvas and warp geometry](#projection-geometry) |
| `scale` | number \| null | null | Output scale factor |
| `transform` | string \| null | null | `normal`, `90`, `180`, `270`, `flipped`, `flipped-90`… |
| `adaptiveSync` | bool | `false` | Variable refresh rate |
| `allowTearing` | bool | `false` | Applied only on Sway 1.10+; reported as a divergence otherwise |
| `maxRenderTimeMs` | number \| null | null | Frame render deadline; null means off |
| `background` | object \| null | null | What the output shows when no window covers it |
| `adopted` | object \| null | null | Read-only: what Suede pinned for a field left unset — see [Adopted values](#adopted-values) |
| `arrangeOffset` | object \| null | null | `{x, y}` nudge added to this output's slice by the grid arrangement solve only; see [Grid arrangement](#grid-arrangement) |

`match` selects by connector name, which is the normal case:

```json
{ "name": "HDMI-A-1" }
```

or by EDID, for installations where connector enumeration is unstable:

```json
{ "make": "Acme Displays", "model": "AD-2400", "serial": "0x00012345" }
```

Every field you specify must match. A configured output that is not currently connected is a reported divergence, not an error — Suede keeps the configuration and applies it when the display appears.

#### Refresh rates {: #refresh-rates }

A requested `refreshHz` is resolved against the modes the display advertises. An exact match within 0.01 Hz wins. Otherwise the nearest advertised rate within 1 Hz is selected and applied as the display advertises it: asking for 60 Hz on a display offering 59.951 Hz selects 59.951 and reports no divergence. Only a resolution the display does not offer, or a refresh rate more than 1 Hz from any advertised rate, raises `mode_unsupported`.

Two active displays running at different rates drift a whole frame apart
over time, so the `refresh-rates` health check warns whenever they are not
all at the same rate, and names a rate they all advertise when one exists.
What that drift looks like on a blended wall, and what the slicer does about
the difference that remains even at a shared rate, is in [Keeping the
displays in step](how-it-works.md#keeping-the-displays-in-step).

#### Connectors, not displays {: #connectors-not-displays }

Matching by `name` anchors an entry to the **socket on the graphics card**,
not to the display plugged into it. The kernel enumerates every connector at
driver probe, independently of what is attached, so `DP-2` means the same
port whether it has a projector on it, has had its cable pulled, or has never
had anything connected. Unplug an installation and plug it back into
different ports and each entry keeps applying to its own port.

That is usually what an appliance wants: the rigging defines which projector
is which. Matching by EDID instead (`make`/`model`/`serial`) anchors to the
*panel*, so configuration follows a display between ports — useful for a desk,
but note that identical projectors often ship with blank or duplicated EDID
serial numbers, which is exactly the case where it matters.

Two caveats on connector naming. DisplayPort MST hubs and daisy-chains create
connectors dynamically (`DP-1-1`, `DP-1-2`) whose numbering depends on the
chain, and adding or moving a GPU can renumber connectors, because the index
is per-card. A single-GPU appliance with directly-attached displays — the
normal case — is stable across reboots and any amount of replugging.

`GET /api/v1/ports` lists every connector the hardware has with its current
connection status, including ones sway cannot report because nothing is
attached. It exists so a client can offer the operator a real choice instead
of asking them to type a connector name; sway remains the authority on what
is actually driving a display.

```json
[ { "name": "DP-1", "connected": true }, { "name": "DP-2", "connected": false } ]
```

#### An unplugged display changes nothing else {: #unplugged }

An output that is configured but not attached keeps its place in the layout.
The canvas stays the size the configuration describes, every blend ramp stays
where it was, and the other displays carry on showing exactly the pixels they
showed a moment earlier — the missing output's region is simply not shown.

This is deliberate, and it is why the geometry is derived from the
configuration rather than from what is currently plugged in. The alternative
would mean one loose connector resizing the canvas mid-show, reflowing the
application, and moving the picture on every *working* projector. A rig can
therefore also be configured completely before the projectors are unpacked.

!!! info "Layout is the client's job, with one exception"
    Suede does no general layout arithmetic. Positions are always explicit, and stay the canonical stored form. The web UI offers left-to-right arrangement as a convenience that simply computes `position` values, and any client can do the same.

    The one exception, added 2026-09-21: solving a regular overlapping grid for the shared projection canvas (rows, columns, independent X/Y overlap, or a content scale) is arithmetic every slider-driven client needs to agree on, so the daemon exposes it as an API call instead of leaving it to each client to reimplement. See [Grid arrangement](#grid-arrangement).

!!! info "The Displays tab shows every connector"
    Being *in the layout* means having a configuration entry, which has
    nothing to do with what is plugged in. The layout diagram draws the
    configured entries, marking any with nothing attached; every remaining
    connector the machine has is listed below it as something to add. So a
    connector with no display can be placed in the layout, and a display
    removed from the layout returns to that list rather than disappearing.

!!! warning "Connected outputs with no entry are left alone"
    Suede only touches outputs you have configured. To turn one off, give it an entry with `"enable": false`.

#### Adopted values {: #adopted-values }

A `mode`, `scale` or `transform` left unset is not really unconfigured once
the appliance has booted once: Sway settles on *something* — usually
whatever the display advertises as preferred — and from then on that choice
is real, whether or not anyone wrote it down. On 2026-09-15 a four-projector
bench was rebooted and two displays came back at 3840x2160@60 instead of the
1920x1200@59.95 they had been running: nothing was misconfigured, nothing
had been configured at all, and the machine had simply been lucky until
then. The resolution change took the projection canvas from 9.2 to
19.2 megapixels and invalidated a set of performance measurements.

So each of these three fields is in one of three states:

| State | Who set it | Survives a reboot | Survives a display swap |
|---|---|---|---|
| **Unset** | nobody yet | no — Sway picks again, possibly differently | no |
| **Adopted** | Suede, from what settled | yes — pinned in `adopted` | no — a different display re-adopts fresh |
| **Set** | the operator | yes | yes — a divergence if the new display cannot deliver it |

An adopted value appears read-only in `adopted`, shaped like the field it
pins plus which display it was taken from and when:

```json
"adopted": {
  "mode": { "width": 1920, "height": 1200, "refreshHz": 59.95 },
  "scale": 1.0,
  "display": { "make": "Acme Displays", "model": "AD-2400", "serial": "0x00012345" },
  "capturedAt": 1757894400
}
```

The `refreshHz: 59.95` in the mode is what Suede observed the display settle on and pinned; it is not a value the operator supplied or needs to know.

It is only ever written by Suede, after a value has been observed twice in a
row (so a display still waking cannot pin a value that was never its final
answer) and only when nothing about that output diverged that pass — a
configuration that could not be fully applied has not settled. It is never
written while a working copy is live, so an edit in progress is never
rewritten underneath the operator.

Adopting never *overrides* the operator: `mode`, `scale` and `transform`
still mean exactly what they say, and setting one always wins over anything
adopted. Writing `"mode": null` does not erase history — it returns that
field to adoption, and the next two agreeing passes pin whatever the display
settles on next, which may be the same value or a different one.

The same display settling on a different value later is reported, not
silently repinned: Suede keeps applying the adopted value (surfacing as
`mode_unsupported` if the display truly no longer offers it) rather than
treating a later disagreement as a new truth. Only when the EDID identity on
that connector changes — a different physical display — does Suede discard
the old pin and adopt fresh for the new one.

**`position` is never adopted, and never will be.** In projection mode the
configured position is a canvas coordinate where the beams are meant to
overlap, while Sway is always handed a plain edge-to-edge tiling — on a
four-projector bench the configuration holds a 2x2 grid while Sway reports a
single row. Observed position is therefore not desired position, by design;
adopting it would flatten the layout and destroy the blend the next time the
outputs settled.

The reference UI offers two shortcuts onto this state machine. Per output,
**Pin current settings** promotes whatever is actually running — the
adopted value where one exists, else the observed mode/scale/transform —
into stated intent, exactly as if the operator had typed it in by hand: a
later display swap then raises a divergence instead of silently changing
the wall. **Pin all displays**, beside the layout, is the same action
applied to every configured, attached output at once — the commissioning
move once the wall looks right. **Clear pinned values** does the reverse,
setting `mode`, `scale` and `transform` back to null so the daemon adopts
afresh on its next settled pass. None of the three buttons touch `position`.

### Backgrounds and wallpapers

A blank display looks broken even when it is only a browser restarting. A
background gives an output something deliberate to show whenever no window
covers it — during a relaunch, or before the first app starts.

A background has three properties, all optional:

| Field | Type | Default | Meaning |
|---|---|---|---|
| `wallpaper` | string \| null | null | Id of an uploaded image. Absent means the color alone |
| `color` | string \| null | `#000000` | `#rrggbb`, used alone or wherever the image does not reach |
| `mode` | string | `fill` | `fill`, `fit`, `stretch`, `center`, `tile` |

The color is never left unstated. Every mode except `fill` and `stretch`
leaves part of the display uncovered, and an unpainted region shows whatever the
compositor last left there — usually a stale frame of the previous app.

#### Named backgrounds {: #named-backgrounds }

Define a background once and let any number of outputs name it. A multi-display installation
normally wants one look across every display, and repeating the same three
properties per output guarantees they drift apart the first time somebody edits
only three of four.

```json
{
  "backgrounds": [
    { "id": "lobby", "wallpaper": "lobby-art", "mode": "fill", "color": "#101820" },
    { "id": "curtain", "color": "#000000" }
  ],
  "outputs": [
    { "match": { "name": "HDMI-A-1" }, "background": "lobby" },
    { "match": { "name": "HDMI-A-2" }, "background": "lobby" }
  ]
}
```

Editing the preset repaints every output using it — the reference has not
changed, but Suede diffs the *resolved* properties, so the new picture reaches
the displays on the next pass.

An output's `background` accepts either form:

```json
"background": "lobby"
"background": { "wallpaper": "lobby-art", "mode": "fill", "color": "#101820" }
```

A bare string names a preset; an object spells the properties out. Both exist
because they serve different callers: the web UI wants one dropdown across every
display, while a script driving the API directly should not have to create a
preset to paint a single output.

Naming a preset that does not exist is rejected at the write, not at reconcile
time — a typo is a mistake in the request, and the writer is the only one who
can still fix it cheaply. Deleting a preset an output still names is refused
with `409`, because cascading would blank those displays.

```bash
curl -X PUT -H 'content-type: application/json' \
  -d '{"id":"lobby","wallpaper":"lobby-art","mode":"fill","color":"#101820"}' \
  http://appliance:9088/api/v1/config/backgrounds/lobby

curl http://appliance:9088/api/v1/config/backgrounds
curl -X DELETE http://appliance:9088/api/v1/config/backgrounds/lobby
```

#### Images

Upload images first, then refer to them by id:

```bash
curl -X PUT --data-binary @lobby.png http://appliance:9088/api/v1/wallpapers/lobby
curl http://appliance:9088/api/v1/wallpapers          # list
curl -X DELETE http://appliance:9088/api/v1/wallpapers/lobby
```

PNG and JPEG are accepted, up to 32 MB. The format is detected from the file's
own bytes rather than the request, so a mislabelled upload is refused outright
instead of leaving a background that silently fails to draw. An image still
referenced — by an output *or* by a named background — cannot be deleted.

In the web UI these live on the **Backgrounds** tab: upload images at the
bottom, define named backgrounds at the top with a live preview, then pick one
per display from the dropdown on the **Displays** tab.

!!! warning "Backgrounds need swaybg"
    Sway draws them by running `swaybg`. Without it the command *succeeds* and
    nothing appears — a black display with no error anywhere. The `swaybg`
    health check fails whenever an output configures a background and the
    program is missing.

### Applications

An application is a *launch specification*, not a window. That is what makes it restorable after a reboot.

**One application is active at a time — `activeApp` — and it always covers
the whole canvas.** The rest of the list is a library to switch between:
`POST /api/v1/apps/{id}/activate` swaps every display to another app atomically,
killing the previous one and launching the new; `POST /api/v1/apps/{id}/deactivate`
clears `activeApp` if this id is the one active. There is no per-app output
targeting and no per-app enable flag; the appliance is a single canvas, not a
window manager.

Both accept `?wait=<seconds>` like any other write. Without it, a
`GET /apps/{id}/status` called right after `activate` still reports the app's
state from before the switch, or `404` for an app just added and not yet
reconciled; with `?wait=`, the response only returns once the pass that
launched the app has run.

| Field | Type | Default | Meaning |
|---|---|---|---|
| `id` | string | required | Unique, stable; also used as a profile directory name |
| `launcher` | object | required | See below |
| `readiness` | object \| null | null | Wait for a URL to answer before launching |
| `env` | object | `{}` | Extra environment variables for the process |
| `audio` | object \| null | null | Absent leaves routing alone; see below |
| `heartbeat` | object \| null | null | Content watchdog |
| `restart` | object | always/1s/30s | Restart policy and backoff |
| `persistProfile` | bool | `false` | Keep the browser profile between launches |

!!! info "There is no per-app placement"
    An application does not choose an output, a workspace, or whether to go
    fullscreen. The active one always covers the whole canvas, and which one
    that is comes from `activeApp`. Placement is Suede's job, and it changes
    depending on whether the layout overlaps — see
    [projection](#projection-edge-blending).

!!! info "Unknown fields are refused"
    A write naming a field Suede does not recognize is rejected outright
    rather than partly applied. A typo in a key is otherwise invisible: the
    write succeeds, the setting is silently dropped, and the appliance
    quietly does something other than what was asked.

#### Driving a multi-display installation

One application covers every display as a single canvas. With a plain
edge-to-edge layout that is sway's `fullscreen enable global`; with an
overlapping layout Suede renders to a headless canvas and slices it. Either
way the configuration is the same:

```json
{
  "outputs": [
    {"match":{"name":"HDMI-A-1"},"enable":true,"mode":{"width":1920,"height":1080,"refreshHz":60},"position":{"x":0,"y":0}},
    {"match":{"name":"HDMI-A-2"},"enable":true,"mode":{"width":1920,"height":1080,"refreshHz":60},"position":{"x":1920,"y":0}},
    {"match":{"name":"HDMI-A-3"},"enable":true,"mode":{"width":1920,"height":1080,"refreshHz":60},"position":{"x":3840,"y":0}},
    {"match":{"name":"HDMI-A-4"},"enable":true,"mode":{"width":1920,"height":1080,"refreshHz":60},"position":{"x":5760,"y":0}}
  ],
  "apps": [
    {"id":"renderer",
     "launcher":{"kind":"chromium-kiosk","uri":"http://control.local/render"}}
  ],
  "activeApp": "renderer"
}
```

The page then sees one 7680x1080 viewport. Position the outputs to form the
canvas you want; Suede performs no layout arithmetic, so the geometry is
entirely yours.

!!! warning "Direct scanout has to match the display path"
    On a tiling appliance (`allow_overlaps = false`, the default) one window
    spans every output, and that needs `WLR_SCENE_DISABLE_DIRECT_SCANOUT=1`
    on the compositor. Without it, some drivers show the same part of the
    window on every display instead of spanning — see
    [troubleshooting](troubleshooting.md#a-spanned-window-mirrors-instead-of-spanning).
    The login profile `provision.sh` writes exports it — it derives the
    variable from these keys at every login — and the `direct-scanout` health
    check warns if it is missing.

    With `allow_overlaps = true` the rule is exactly inverted: no client spans
    the physical outputs, each display scans out its own slice, and the
    variable must *not* be set — unless `direct_scanout = false` asks for the
    composited arm of the comparison, which inverts it once more. See
    [Overlapping layouts and direct scanout](#direct-scanout).

!!! tip "Give the outputs matching heights"
    A spanned window covers the *bounding box* of every output. Where an output
    is shorter than its neighbors, the content below it falls outside any
    display and is simply not visible.

#### Launchers

=== "Chromium kiosk"

    ```json
    {
      "kind": "chromium-kiosk",
      "uri": "http://control.local/render/1",
      "showFpsCounter": false,
      "grantCapture": true,
      "extraArgs": [],
      "program": null
    }
    ```

    Expands to a kiosk argument set carried over from production use: `--kiosk`, `--password-store=basic` (no keyring prompt on a headless box), `--ozone-platform=wayland`, `--no-first-run`, `--autoplay-policy=no-user-gesture-required`, `--auto-accept-camera-and-microphone-capture`, hardware-decode and zero-copy flags, and a private `--user-data-dir`. `extraArgs` are appended before the URI.

    | Field | Meaning |
    |---|---|
    | `uri` | Page to load; `{appId}` and `{heartbeatUrl}` are expanded |
    | `showFpsCounter` | Chromium's frame-rate overlay, for diagnosing dropped frames |
    | `grantCapture` | Grant the page's own origin camera and microphone access in its profile, so it can list devices by name and choose one; `false` leaves only per-request auto-accept |
    | `extraArgs` | Appended after the preset, before the URI |
    | `program` | Which binary to launch, overriding the search |

    Without `program`, Suede tries `chromium`, `chromium-browser`,
    `google-chrome-stable` and `google-chrome` in that order, ignoring any
    that is a snap. Naming one settles it: a bare name is looked up on
    `PATH`, a path is used as given.

!!! warning "A snap browser does not count as installed"
    Ubuntu's `chromium` package is a shim for a snap, and Suede's search
    passes over it as though it were not there. A snap updates itself on its
    own schedule and restarts the browser when it does, which on an appliance
    means the displays go blank in the middle of a show — so it does not meet
    the point of the exercise, and a machine with only a snap is reported as
    having no browser at all:

    ```
    FAIL  no browser installed. A snap is present (/snap/bin/chromium) but
          Suede will not use one: ... Install one from a .deb - on Debian
          `apt install chromium`, on Ubuntu Google Chrome's own package.
    ```

    Skipping a snap is about not choosing one by accident. Choosing one on
    purpose is a decision, and it is honored — `launcher.program` overrides
    the search entirely:

    ```json
    { "kind": "chromium-kiosk", "uri": "http://…",
      "program": "/snap/bin/chromium" }
    ```

    That works, because Suede then puts the profile somewhere a confined snap
    can write. Snap's `home` interface covers `$HOME` but excludes hidden
    directories, and Suede's state lives under `~/.local/state`; a snap
    browser's profile therefore goes to
    `~/snap/<name>/common/suede-profiles/<app>`. Without that, Chromium
    cannot create its `SingletonLock`, aborts with a message about profile
    corruption, and the application crash-loops.

    `program` takes a bare name (looked up on `PATH`) or a path, and applies
    to `firefox-kiosk` too. It is the way to pin a specific browser for any
    reason, not just this one.

!!! info "Pages may make a sound without being clicked"
    Chromium normally suspends every `AudioContext`, `<audio>` and `<video>`
    until a "user gesture", and on an appliance no gesture is ever coming — a
    page that plays perfectly on a desk is simply silent on the machine. The
    preset therefore sets `--autoplay-policy=no-user-gesture-required`. The
    consent the policy exists to obtain was given when the operator chose what
    the machine runs.

!!! info "Pages may use a camera or a microphone without being asked"
    A capture permission prompt on an appliance is a dialog nobody will ever
    click, so a page wanting a camera, a microphone or a capture card simply
    never gets one. The preset therefore sets
    `--auto-accept-camera-and-microphone-capture`, on the same reasoning as
    autoplay: the operator chose what the machine runs.

    Note what the flag does **not** do: it waves each request through without
    *persisting* a grant, and without one `enumerateDevices()` returns blank
    labels and ids, so a page cannot choose a device by name. `grantCapture`
    (on by default) writes a persistent grant for the app's own origin into its
    private profile before every launch; the flag remains as the fallback for
    any other origin the page ends up on.

    `getUserMedia` also exists only in a secure context. `https://` and
    anything on loopback qualify; a plain-HTTP page served from another host
    does not, and finds the API simply absent — no prompt, no error. So when
    the configured `uri` is one of those, Suede passes
    `--unsafely-treat-insecure-origin-as-secure=<that origin>` automatically,
    naming only the origin the operator configured. HTTPS and loopback URIs get
    nothing added, and an `extraArgs` entry still overrides it.

    **Firefox has no equivalent flag.** Its autoplay control is a preference
    (`media.autoplay.default`), which needs a profile Suede does not currently
    manage for Firefox, so a `firefox-kiosk` app stays subject to the default
    blocking policy. Use `chromium-kiosk` where sound matters.

=== "Firefox kiosk"

    ```json
    {
      "kind": "firefox-kiosk",
      "uri": "http://control.local/render/1",
      "extraArgs": []
    }
    ```

    Expands to `--kiosk --new-instance --private-window`, with `MOZ_ENABLE_WAYLAND=1` in the environment.

=== "Any command"

    ```json
    {
      "kind": "exec",
      "command": "/usr/bin/mpv",
      "args": ["--fullscreen", "/srv/media/loop.mp4"]
    }
    ```

    Launched verbatim. A bare `exec` app is only expected to map a window if it pins an output.

#### Environment and hardware acceleration

`env` sets environment variables on the launched process. They are applied
last, so they override anything the launcher preset chose.

This is usually how graphics acceleration is configured, because the knobs are
environment variables rather than command-line flags:

```json
{
  "id": "wall",
  "launcher": { "kind": "chromium-kiosk", "uri": "http://control.local/wall" },
  "env": {
    "LIBVA_DRIVER_NAME": "nvidia",
    "NVD_BACKEND": "direct"
  }
}
```

!!! warning "Check that hardware video decode is really happening"
    The `chromium-kiosk` preset asks for VA-API decode, but that only takes
    effect if a VA-API driver for your GPU is installed. Without one,
    Chromium falls back to software decode *silently* — nothing fails, it
    just uses the CPU. Nvidia cards need `nvidia-vaapi-driver`; Intel needs
    `intel-media-va-driver`.

    On NVIDIA, the driver being installed is **still not enough** by itself:
    Chromium skips nvidia-drm devices outright ("Should skip nVidia device"
    in its GPU log) unless the `VaapiOnNvidiaGPUs` feature is enabled. The
    preset now enables it — measured on a Quadro RTX 8000, that one flag
    took H.264, H.265 and VP9 from software to hardware decode, with no
    `LIBVA_DRIVER_NAME` or `NVD_BACKEND` needed. If a broken driver ever
    makes it misbehave, repeat `--enable-features` in `extraArgs` without
    `VaapiOnNvidiaGPUs` — the later flag wins.

    The only honest answer comes from inside the browser, so ask it:
    **Check capabilities** in the application dialog launches this exact
    configuration — same browser, same arguments, same environment — against
    a page served by the daemon, which measures what the media APIs really
    say and reports back. A window opens on the appliance's displays for a
    few seconds. Per codec and resolution you get whether a *hardware*
    decoder accepted the configuration (WebCodecs), whether playback is
    expected to be smooth and power-efficient (`MediaCapabilities`), and the
    WebGL renderer string — `llvmpipe` or `SwiftShader` there means no GPU
    acceleration at all. The same measurement is available to any client as
    `POST /api/v1/apps/capabilities`, and the most recent one is served from
    `GET /api/v1/apps/capabilities/last`.

    The appliance also measures itself. At startup, if the browser, its
    configuration, the GPU driver, or Suede itself changed since the last
    measurement, the check runs once automatically — during boot, when its
    brief window is lost in the noise — and the `decode-measured` health
    check judges the result: a GPU that is software-decoding everything is a
    warning (the silent fallback), a software rasterizer is a warning, and a
    platform with no browser decode path (VideoCore) passes with the facts
    stated. With nothing changed, nothing is launched: the stored
    measurement stands. `measureCapabilitiesOnStart` in settings turns the
    automatic run off.

    Independent confirmation, if you want it: watch `nvidia-smi dmon -s u`
    and look at the `dec` column while a video plays.

    Rasterization, compositing, WebGL and CSS animation are a separate path and
    generally work without any of this — check the WebGL renderer string is
    your GPU rather than `llvmpipe` or `SwiftShader`.

    On a Raspberry Pi none of the VA-API advice applies: VideoCore has no
    VA-API at all, and Raspberry Pi OS's Chromium build drives the V4L2
    decoder directly. A Pi 5 exposes hardware HEVC only — H.264 lost its
    hardware path with the 2712 and decodes in software, which is fine at
    1080p and marginal above it. The `video-decode` health check reports
    which decoders the machine actually exposes.

#### Waiting for a service to be ready

A kiosk browser started before the service it points at is serving shows an
error page — and stays on it, because nothing reloads the tab. `readiness`
removes that race by gating the launch on the service answering:

```json
{
  "id": "renderer-1",
  "launcher": { "kind": "chromium-kiosk", "uri": "http://127.0.0.1:8080/wall" },
  "readiness": { "url": "http://127.0.0.1:8080/healthz" }
}
```

| Field | Type | Default | Meaning |
|---|---|---|---|
| `url` | string | required | URL to poll; `http://` only |
| `expectStatus` | array | `[]` | Status codes meaning ready; empty means any 2xx |
| `intervalSeconds` | number | `2` | Time between attempts |
| `timeoutSeconds` | number | `5` | Time allowed for one attempt |
| `giveUpAfterSeconds` | number \| null | null | Launch anyway after this long; null waits forever |

While waiting, the app reports `waitingForDependency` with the last failure in
its `detail`, so the reason is visible rather than guessed.

Waiting forever is the default deliberately: on an appliance, showing the
background until the service appears is better than showing an error page that
nobody will reload. Set `giveUpAfterSeconds` if you would rather see whatever
the browser makes of it.

The probe gates every launch, not just the first. A relaunch after a crash, a
heartbeat timeout, a manual restart, or activation all wait on it again, so a
dependency that dies while its app is running is caught the moment that app is
next launched, and the app goes back to `waitingForDependency` rather than
into a browser error page. `giveUpAfterSeconds` counts from the start of each
wait, so it restarts with every relaunch rather than accumulating across them.

!!! note "Only http://"
    The probe reads the status line and nothing more, so it deliberately has no
    TLS stack — that keeps Suede a single binary with no native dependencies.
    A readiness URL is almost always a loopback service. An `https://` URL is
    rejected when the configuration is written, rather than failing silently at
    launch.

#### URI placeholders

Launcher URIs and `exec` arguments may contain:

| Placeholder | Expands to |
|---|---|
| `{appId}` | The application's id |
| `{heartbeatUrl}` | A loopback URL for this app's heartbeat endpoint |

This is how page content learns where to post heartbeats without hard-coding host details.

#### Audio routing

The `audio` field distinguishes three cases, and the distinction is deliberate:

| Value | Meaning |
|---|---|
| absent | Use whatever PipeWire's default sink is **at each launch** |
| `{"output": "alsa_output.…"}` | Lock to that sink, by PipeWire `node.name` |
| `{"output": null}` | Lock to silence — Suede's null sink discards the audio |

An `audio` object may also carry `gainDb`, the level to hold that sink at:

| Field | Type | Default | Meaning |
|---|---|---|---|
| `output` | string \| null | — | Sink `node.name`, or `null` for silence |
| `gainDb` | number | `0.0` | Level for that sink in dB, from `-100.0` to `0.0` |

The first is a decision deferred, not a decision recorded. An app with no
`audio` field follows the machine's default sink wherever it goes, and the
default can move on its own — plugging in a USB headset is enough for
WirePlumber to promote it. Naming a sink pins the app to it regardless.

Get the available identifiers from `GET /api/v1/av` and use an id from `.audioOutputs`. Changing an app's sink relaunches it, because routing is applied at launch through `PULSE_SINK`.

##### Level

`gainDb` is desired state like everything else here, so Suede holds the named
sink at it and puts it back if something moves it. Only sinks an app names are
touched; the rest of the machine's audio is not Suede's business, and a sink
locked to silence has no level worth setting.

**Unity is the default, and usually the answer.** A sink left wherever the last
session put it passes signal at a level nobody knows, and the symptom —
everything works, quietly — is a wretched one to chase. On a digital output it
is worse than inconvenient: every decibel taken here is resolution discarded
*before* the link, and nothing downstream can put it back. Attenuate at the
amplifier. That is also why the scale stops at `0.0`: above unity a digital
sink has no headroom and can only clip.

`-100.0` means silence rather than a very small gain — the true zero of an
amplitude is minus infinity, which no configuration file can hold — and is
applied as an exact zero.

!!! warning "A mixer's number is not a level"
    PipeWire stores `channelVolumes` as linear amplitude, but `wpctl` and the
    mixer UIs built on it display its **cube root**. A sink showing `0.40` in
    `wpctl` is at 0.40³ = 0.064, which is **&minus;24 dB**, not &minus;8. Suede
    states levels in dB everywhere for exactly this reason, and writes them
    through the session manager so that `wpctl` agrees with what is audible.

    This is not a hypothetical: a sink at `wpctl` 0.40 on each end of a chain
    is 48 dB of attenuation, arrived at by two people who each thought they had
    turned it down slightly.

#### Restart policy

```json
{ "policy": "always", "delayMs": 1000, "maxDelayMs": 30000 }
```

`policy` is `always`, `on-failure` (non-zero exit only), or `never`. Delay doubles on each consecutive attempt up to `maxDelayMs`. An app whose policy declines a relaunch is left in `crashed` and will not restart until its configuration changes or `POST /apps/{id}/restart` is called.

#### Content watchdog

```json
{ "enabled": true, "timeoutSeconds": 25, "startupGraceSeconds": 60 }
```

Process liveness cannot detect a hung page — Chromium keeps running happily while its content is frozen. With the watchdog enabled, content is expected to `POST /api/v1/apps/{id}/heartbeat` roughly every 10 seconds:

```javascript
const heartbeat = new URL(location).searchParams.get("hb");
setInterval(() => fetch(heartbeat, { method: "POST" }), 10_000);
```

The watchdog **arms on the first heartbeat**. Before that, only `startupGraceSeconds` applies, which covers page load. Once armed, `timeoutSeconds` of silence kills and relaunches the app.

Heartbeats and the app's `state` are independent. A page can post its first heartbeat while the app still reports `starting`, because `running` waits for the window to be placed, and a placed window can equally precede the first heartbeat. A client that wants "launched and its content is alive" should wait for both: `state` is `running` and `lastHeartbeat` is set.

The endpoint is unauthenticated but accepted only from loopback, so the key-free design cannot be abused from the network.

It also answers cross-origin requests from any origin, including Chromium's private-network preflight, so page content served from another host or port (or opened as a local file) can post with a plain `fetch` and read the response: 404 means the app id in the URL is wrong, 403 means the request did not arrive from loopback. Log a non-2xx response so a misconfiguration is visible in the page console. Do not set `mode: "no-cors"`: it discards the response, and the response is the only way the page can notice either failure.

### Projection and edge blending {: #projection-edge-blending }

For up to eight projectors whose beams physically overlap, **the
layout is the projection configuration**. Position each output in canvas
space exactly as its beam lands on the surface — overlapping the neighbors by
however much the rigging actually overlaps, each seam its own amount, rows
and grids included. The canvas is the layout's bounding box, and the Displays
tab reports it live.

An overlapping layout is only accepted on an appliance provisioned for one:
set [`allow_overlaps = true`](#direct-scanout) in `suede.toml`, or
`provision.sh --allow-overlaps`. On the default tiling appliance an overlap
is rejected as a layout sway cannot render, naming both outputs.

The layout must be **contiguous**: every enabled output must chain back to
the first through overlaps or shared edges (any number of intermediates; a
corner-to-corner touch does not count). A gap would leave part of the canvas
mapped to no projector — content silently lost — so validation rejects it
like any other invalid write. Outputs with no configured `mode` or
`position` take their geometry from observation and are exempt from the
check.

Sway never sees the overlaps. It is handed a plain edge-to-edge tiling, and
the overlapping picture is made above it: the active app renders once into a
headless canvas the size of the layout, and the slicer cuts that canvas into
one blended, output-sized slice per projector — the two copies of every seam
summing to constant luminance on the surface. [The path of a
frame](how-it-works.md#the-path-of-a-frame) follows that pipeline stage by
stage, with what each stage costs and what a stall in each looks like in
`GET /projection/stats`.

What happens to a layout with **no** overlaps depends on
[`allow_overlaps`](#direct-scanout). On a tiling appliance it skips all of
this: sway tiles it directly, at zero cost. On an overlapping one there is
no second path, so every layout of two or more outputs is sliced — the
same capture, the same presenters, simply with no intersections, so no
ramps. That is not free, and it is not meant to be: it buys each display a
private, output-sized buffer the display controller can flip on its own —
which it will, unless [`direct_scanout = false`](#direct-scanout) asks sway
to composite the slices instead.

```json
"projection": {
  "mode": "simple",
  "blend": true, "gamma": 2.2, "blackLift": 0.04
}
```

Simple and Warp share one canvas and each output's `geometry.slice` crop.
Warp adds retained corner and center correction to the same content selection:

```json
"projection": {
  "mode": "warp",
  "canvas": { "aspect": 7.111111111111111, "renderWidth": 7680, "scale": 1.0 },
  "blend": true, "gamma": 2.2, "blackLift": 0.04
}
```

| Field | Type | Default | Meaning |
|---|---|---|---|
| `mode` | string | `simple` | `simple` crops and scales the shared canvas; `warp` adds retained geometric correction |
| `canvas` | object or null | `null` | Shared `aspect` and authoritative `renderWidth`. Required for Warp; when absent, Simple uses integer output positions. `scale` is deprecated metadata and has no rendering effect |
| `blend` | bool | `true` | `false` slices without ramps — overlapping beams still need the duplication, just unfaded |
| `gamma` | number | `2.2` | The projectors' transfer gamma, 1.0-4.0; shapes every ramp's fall-off |
| `blackLift` | number or object | `0.0` | Fixed compensation or optional adaptive compensation, with level 0-0.5; see below |
| `testPattern` | string or null | null | `grid`, `warp-alignment`, `white`, `black`, `gamma`, `identify`, `sync` - or null for content |
| `temporary` | object | `{ "highlightOverlaps": false }` | Temporary calibration aids that are never saved. `highlightOverlaps` draws the seam lineup lines; see [Highlighting overlaps](#highlight-overlaps) |
| `freeRun` | bool | `false` | Let each output take frames at its own pace instead of all together; see [Keeping the displays in step](how-it-works.md#keeping-the-displays-in-step) |
| `renderer` | string | `auto` | `auto`, `cpu`, or `gpu`; which pipeline the slicer blends with, see [Where the blend runs](how-it-works.md#where-the-blend-runs) |
| `arrangement` | object or null | `null` | Optional. The resolved record of the last applied [grid arrangement](#grid-arrangement): `{ rows, columns, overlapX, overlapY, contentScale }`. A record of intent, not canonical geometry — a later manual geometry edit leaves it in place |

#### Adaptive black lift

A numeric `blackLift` keeps the exact fixed transfer arithmetic. An explicit
`{"mode":"fixed","level":0.04}` has the same meaning. To vary compensation
with source content, use:

```json
"blackLift": {
  "mode": "adaptive",
  "level": 0.2,
  "darkThreshold": 0.02,
  "brightThreshold": 0.2,
  "riseMs": 1000,
  "fallMs": 250,
  "slewPerSecond": 0.1
}
```

`level` is required and remains in 0-0.5. The other fields default to the
values shown. Thresholds must satisfy `0 <= darkThreshold < brightThreshold <= 1`.
Rise and fall time constants are 1-3,600,000 milliseconds. Slew must be
positive and at most 10 lift units per second. All values must be finite.
These defaults are provisional and need tuning for the installation.
See the [complete adaptive example](examples/four-output-adaptive.json).
For the spatial shortfall-sharing formula, the exact adaptive target/smoothing arithmetic, and a design record of related ideas that were not built, see [Black lift](plans/black-offset.md).

The statistic is mean linear luminance after sRGB decoding with Rec.709 RGB
weights (0.2126, 0.7152, 0.0722). A regular grid of at most 256 by 256 source
texels selects the center of each grid cell before ramps, picture borders,
or lift. Black content and unused canvas count; allocation padding does not.
This sampled estimate can alias periodic thin lines and checkerboards; it is
not the mean of every source pixel.

Measurement is asynchronous and rate-limited to roughly 8 Hz (submitted no
more than once every 125 ms), independent of the capture and presentation
rate and of how many outputs overlap. It never blocks the render thread: the
GPU pass runs in its own command buffer and fence, and is collected only
once that fence has signaled — a capture that arrives before the rate limit
allows the next submission, or before the previous one has finished, is
simply not the one measured. `measurementMs` in `control.blackLift` is that
pass's submit-to-collection latency, not render-thread stall time, and
`captureId` names the exact capture the reported sample was taken from.

The target is `level * clamp((brightThreshold - luminance) /
(brightThreshold - darkThreshold), 0, 1)`. The shared controller uses monotonic
elapsed time, exponential smoothing with the appropriate rise/fall constant,
then slew limiting and clamping. It starts at configured level. Zero samples
retain the last valid target and report stale measurement. A bounded 20 ms
timer repaints retained static content until the controller settles; lift
ticks do not rebuild geometry tables or create new captures.

Adaptive transfer uses a separately tagged, quantized shape table. Numeric
and tagged fixed settings retain their existing byte-exact path. Calibration
patterns suspend adaptation and use the exact configured fixed transfer.
CPU rendering or unavailable GPU measurement uses configured fixed level and
reports the reason; ordinary rendering continues. Sampling and readback add
work and should be benchmarked on the target hardware before rollout.

`GET /api/v1/projection/stats` and projection events expose `control.blackLift`:
metric definition, availability/reason, stale measurement, calibration pause, capture ID, sample
count, luminance, measurement timestamp/age, target and applied levels,
measurement duration, and logical presentation generation. A logical
generation shares one level across outputs; independent physical scanout is
still governed by the existing presentation policy. Capture, logical,
configuration, and control generations are different identities. Sample age
continues increasing when the source is static, even after adaptation settles.

#### Canvas and warp geometry {: #projection-geometry }

`mode` is `simple` (the default) or `warp`. Both use the shared canvas and crop
when configured. The daemon retains saved correction and requested intent while
temporarily forcing effective simple mode on a machine without the required
warp capability; it reports that limitation as a health warning and restores
warp when capability returns. Switching modes does not silently overwrite the
shared content settings or correction.

Shared canvas rendering has explicit canvas and per-output geometry:

| Field | Type | Meaning |
|---|---|---|
| `canvas.aspect` | positive number | Canvas width divided by height in isotropic canvas units |
| `canvas.renderWidth` | integer | Chosen canvas width. It is authoritative; height is `round(renderWidth / aspect)`, at least one pixel |
| `geometry.slice` | rectangle | The content rectangle in canvas units (`x`, `y`, `width`, `height`) |
| `geometry.corners` | four pairs | Destination pins in output-local normalized coordinates, ordered TL, TR, BR, BL. Identity is `[[0,0],[1,0],[1,1],[0,1]]` |
| `geometry.center` | pair | Horizontal and vertical center fractions, normally `[0.5,0.5]`, each constrained to `0.01..=0.99` |
| `geometry.rasterFootprint` | rectangle | The calibrated light footprint in canvas units, independent of slice placement and pin edits |

!!! warning "Alpha-breaking rename: `source` is now `slice`"
    The field was named `geometry.source` before this release. Every response
    and every saved document now calls it `geometry.slice`; a `PUT` still
    accepts `source` as an alias so existing saved documents and older
    clients' writes keep working, but a client reading responses should look
    for `slice`.

Slice rectangles and destination pins answer different questions. The slice
selects which browser content an output shows; the pins move that picture in
the output raster. The footprint describes where the projector's full raster
lands, including black pixels outside a pinned picture. Pin edits never resize
the browser or change a neighbor's slice coverage.

Canvas coordinates use `[0,1] × [0,1/aspect]`. If `renderWidth` is `W`, the
canvas height is `H = max(1, round(W/aspect))`, with positive half-way values
rounded up; `renderWidth` is the chosen allocation, with no separate scale
field. A slice rectangle's
continuous pixel rectangle is `[W*x, aspect*H*y, W*width, aspect*H*height]`.
Corners are normalized to the output raster and are ordered top-left,
top-right, bottom-right, bottom-left. A shared canvas requires each enabled
configured output, including an unattached output, to have geometry and an
explicit or adopted mode. Warp additionally requires output scale `1.0` and
transform `normal`; selecting it never clears retained hardware settings.
Legacy `position` is not a second crop and is not required for fallback.
The roster has one to eight enabled participants, the
canvas must be complete, and the appliance must set `allow_overlaps = true`.

Every retained geometry is numerically validated even in Simple. With a shared
canvas in either mode, each slice must have finite positive dimensions, visible canvas area,
and coordinates bounded by ±16 times `max(1, 1/aspect)`. The clipped slice
graph must be connected through positive-area overlap or a shared edge
segment; point contacts and gaps do not connect it. A pair whose original
rectangles overlap by at least 80% of the smaller rectangle is a near-total
stack. Stacks are accepted only when every pair is a stack; mixed stack/seam
layouts are rejected. `rasterFootprint` is separate physical calibration and
is initially copied from `slice` by conversion.

#### Retaining and clearing calibration {: #retaining-calibration }

Calibration — `canvas` and each output's `geometry` — is precious: it is the
result of a physical alignment pass, not something a routine edit should be
able to erase by accident. So while `projection.mode` is `simple` (including
when there is no `projection` section at all), a `PUT` that omits `canvas`
or an output's `geometry` **keeps the previous value** rather than clearing
it — whether the write is a full-document `PUT /api/v1/config`, a section
`PUT /api/v1/config/projection` or `PUT /api/v1/config/outputs`, or a
single-output `PUT /api/v1/config/outputs/{key}`. This is what lets a Simple
client that only ever edits, say, `blend` or `gamma` do so without knowing
or restating the retained Warp calibration, and what lets switching from
Warp back to Simple and back again come back to the same pins. In `warp`
mode a `PUT` is literal: an omitted `geometry` is cleared, the same as any
other field a `PUT` does not mention.

To clear calibration explicitly regardless of mode, use the dedicated
routes instead of a `PUT`:

- `DELETE /api/v1/config/outputs/{key}/geometry` clears one output's
  `geometry` (slice crop, pins, center, and raster footprint).
- `DELETE /api/v1/config/projection/canvas` clears the shared `canvas`.

Both take the same preconditions as any other write, return the persisted
document, and are idempotent — clearing an already-clear field still
succeeds.

`GET /api/v1/projection/recommendation` is read-only. It uses the
effective working copy and reports its persisted `revision` and working-copy
`generation`, a requested aspect, ideal and admissible dimensions, and
`0.25`, `0.5`, `0.75`, and `1.0` scale presets. The ideal width samples the largest
singular value of the canvas-to-output Jacobian on a dense grid for every
output, evaluating both sides of center-remap breaks and applying a
5% engineering margin. It is explicitly `approximate: true`; it is guidance,
not a proof of the exact maximum. Ill-conditioned or non-finite geometry is
rejected. Preset widths round to the nearest positive integer (ties up), with
no multiple-of-eight restriction. Percentages use the ideal width, while the
admissible allocation limit is reported separately; an impossible preset is
disabled. Simple uses rectangular mapping and Warp uses retained intended
correction even during temporary capability fallback. The normalized density
baseline stays stable across width-only edits. Recommendations never change
`renderWidth`, slice dimensions, or configuration.

The known planning limits are a 32,768-pixel maximum dimension, 64 megapixels
for the canvas, and 32 megapixels per output. Driver and compositor allocation
limits are unknown and are listed in the response. The endpoint does not
resize a headless output or probe those limits.

CPU Simple supports fractional crops and content scaling in projection-enabled
builds. Simple presentation buffers follow the compositor's rotated/scaled
logical output dimensions, while content scale and density guidance refer to
the physical raster. Those buffers also obey the output allocation limit.
Builds without projection machinery cannot render arbitrary shared crops;
they report `projection_unavailable`. Direct tiling configurations must not
configure a shared canvas: it requires `allow_overlaps = true` and a slicer.

#### Grid arrangement {: #grid-arrangement }

Placing a regular grid of outputs on the shared canvas — the common case for
a projector wall — is arithmetic the daemon can do for you, so that every
client driving the same wall (the reference UI's **Arrange automatically**
dialog, a show controller with sliders) gets the same answer.
`PUT /api/v1/config/projection/arrangement` solves the grid and writes each
enabled output's `geometry.slice`; `GET /api/v1/projection/arrangement` runs
the same solve without writing anything, so a client can preview before
committing to a value.

```json
{
  "rows": 2,
  "columns": 2,
  "overlapX": 0.2,
  "overlapY": 0.05
}
```

Enabled outputs are placed in document order, `row = i / columns`,
`column = i % columns`; the enabled count must not exceed `rows * columns`,
or the request is `422`. The order of `outputs[]` is therefore meaningful
and settable: reordering the array changes which slot each output lands in
(the reference UI's Arrange dialog does this with up/down controls on each
row), and, being an ordinary part of the document, the order persists like
any other config write. Give either `overlapX` and `overlapY` together, or
`contentScale` alone — never both (`422`, "over-determined"), and never
neither (overlaps then default to `0`).

Connectivity plays no part in the solve: an enabled output whose display is
currently disconnected still takes its grid slot and is laid out exactly as
if it were attached. Its raster comes from `mode` (or a previously adopted
one) before ever falling back to what is currently observed, so the solve
does not depend on what happens to be plugged in at the moment it runs, and
the attached displays around it receive the correct slices regardless. The
disconnected output's own `geometry.slice` is written and applied like
everyone else's; the reconciler carries it through to the display the moment
it appears. The only failure mode is an enabled output with no configured,
adopted, *or* observed mode at all — nothing to size it by — which is the
existing `422` naming it: "output X has no known raster size".

**Overlaps are exact: the daemon never grows one.** With `overlapX`/`overlapY`,
each comes back in `arrangement` exactly as requested — the solver either
honors both as given or refuses the request; it never adjusts either one to
make the other fit. Full canvas coverage instead comes from the content
scale: with per-axis fits `fitX = requestedW / canvasW` and
`fitY = requestedH / canvasH` (the grid's raster extent at the requested
overlaps, over the canvas's), the daemon picks
`contentScale = min(fitX, fitY)` — the least-waste scale that still covers
the whole canvas. The axis with the smaller fit fills the canvas exactly;
the other **overhangs** it, centered, and the response reports how far as
`overhang` (a fraction of the canvas extent on that axis, split evenly
between both ends). Slice pixels that fall in the overhang render black —
wasted slice pixels at the edge of the wall, which is acceptable, unlike an
unused band of canvas that no projector shows. The rule needs no
seamed/unseamed case analysis: an axis without a seam (a single row or
single column) just has an inert overlap, and if it happens to be the axis
with the larger fit, it overhangs exactly like a seamed one would.
`impliedAspect` is the canvas aspect at which the *requested* overlaps would
fill both axes exactly, with no overhang; apply it explicitly if you want
the resolved overlaps to leave no overhang, since the endpoint never changes
the canvas aspect itself. With `contentScale` alone, the canvas fills
exactly on both axes and `overlapX`/`overlapY` are derived from it; if the
derived overlap would need to reach 100% or more — the content scale is too
small for this grid to reach the canvas edges — the result is `422`,
"content scale is too small for this grid", regardless of
`allowUnusedCanvas` below (a content-scale request already fills whenever a
fill is possible at all).

**An edge slice pushed entirely off the canvas is always a `422`, in every
mode.** Under a large enough aspect mismatch — reachable with three or more
rows or columns, since the overhang above splits between the grid's two ends
— an edge slice can lose *all* of the canvas rather than merely some of it;
that is refused rather than produced, naming the output: "output X's slice
falls entirely outside the canvas at these overlaps; the canvas aspect that
fits these overlaps exactly is {impliedAspect}". Because this can happen
even at small overlaps once the aspect mismatch is extreme, and because
whether a given overlap is safe depends on what the *other* axis is asked
for, a client cannot always tell from the numbers alone whether a request
will be refused — which is what `limits` (below) is for.

**`allowUnusedCanvas` (default `false`) keeps its old meaning: whether a
partial cover is acceptable, in exchange for never overhanging.** Set it to
`true` and the daemon fits the whole grid *inside* the canvas instead, at
`contentScale = max(fitX, fitY)`: the axis with the *larger* fit now fills
exactly, and the other is left short rather than overhanging — any smaller
scale would push a slice outside the canvas, any larger would cover less on
both axes. `unusedCanvas` reports whatever fraction is left uncovered on
each axis, centered on the canvas exactly as `overhang` is. Left `false`
(the default), an overlap request never leaves a band — see above, it
overhangs instead, so most callers (Zelus's slider-driven Arrange dialog
included) never need to send this flag at all; it exists for a caller that
would rather see the wall's inner boundary sit inside the projected surfaces
than see black slice pixels hanging off its edge. A content-scale solve that
would leave a band on an unseamed axis at the asked-for scale (a single row
or column shorter or narrower than the canvas at that scale) is still
refused with a `422` naming the axis, the band as a percentage of the
canvas, and the aspect that would close it — unless `allowUnusedCanvas: true`
accepts it instead. The flag is accepted on both the `GET` dry run and the
`PUT`, so a preview sees the same answer committing it would give.

This is a **behavior change from the last released 0.1.14**: a request that
used to succeed with a band left on the canvas (any single-row or
single-column grid on a mismatched canvas aspect, for instance) now
overhangs the canvas by default instead of leaving that band uncovered — set
`allowUnusedCanvas: true` to get the old fit-inside answer and its band
back. Anything driving arrangement against the old habit (a single-row
wall, or a canvas aspect picked before the outputs' own proportions were
final) will now see black at the wall's edge where it used to see an unused
band; that is expected, and `allowUnusedCanvas: true` restores the old
answer if the band is preferred.

**`limits` lets a client clamp its controls instead of guessing.** The
dry-run `GET`'s response also carries, for each axis, the closed range of
overlaps that stays valid — under the `0 <= overlap < 1` bound and the
off-canvas-slice rule above — while the *other* axis is held at the value
the request just asked for; it is computed against the same document state
the solve used. A slider-driven client (or a plain HTML range input) should
set its min/max straight from `limits.overlapX`/`limits.overlapY` rather
than allowing 0-100% and finding out from a `422` that a value near the
extreme was never valid. `hasSeam: false` means the axis has no seam at
all — a single row (for `overlapY`) or a single column (for `overlapX`) —
so its overlap changes nothing; a client should disable that axis's control
outright rather than let an inert value be edited. `limits` is omitted for
a `contentScale` request, which has no overlaps to limit.

`GET /api/v1/config/projection/arrangement` (the persisted record, below)
also carries `limits`, computed against the *recorded* arrangement's
rows/columns/overlaps together with the document's current canvas and
outputs — so the one read that seeds a rearrangement dialog's starting
position can seed its slider ranges too. It always computes those limits as
if `allowUnusedCanvas` were `false`, since the record does not store that
flag; a client that applied the recorded arrangement with the flag set
should keep that in mind. `limits` is omitted there (never `null`, and
never a reason to fail the read) whenever there is no record at all, or
whenever it cannot be computed against the document as it stands now — a
missing canvas, an output with no known raster, more enabled outputs than
the recorded grid holds, and so on.

The dry-run `GET` responds with the resolved values (`rows`, `columns`,
`overlapX`, `overlapY`, `contentScale`) under `arrangement`, together with
`unusedCanvas`, `overhang`, `impliedAspect`, `limits` (all above), each
output's resulting `slice` rectangle under `outputs`, any `warnings` (for
example, an overlap above 80%), and the `revision` and `generation` of the
document it was solved against. The `PUT` responds with the whole document
as usual, where the same values are `projection.arrangement`; it carries no
`limits` of its own — re-read either `GET` after a change to refresh them,
since they reflect the document state the solve ran against.

**`outputs[].arrangeOffset`** (`{x, y}`, normalized canvas units — the same
space as `geometry.slice` — default `{0, 0}`) nudges one output's placement
after the solve, without moving anything else in the grid: the solver adds
it to that output's computed `slice.x`/`slice.y` once placement is
otherwise finished. `unusedCanvas`, `overhang`, the `allowUnusedCanvas` gate,
and the off-canvas-slice gate above are all computed *before* offsets are
applied, so a nonzero offset can deliberately uncover canvas pixels —
nudging one projector to correct a mechanical misalignment, say — without
ever being mistaken for the coverage failure `allowUnusedCanvas` guards
against, or for a slice the solve would otherwise have refused.
`GET /api/v1/config/projection/arrangement`'s
`inEffect` applies the document's *current* offsets before comparing, so
editing an offset after an arrangement has been applied turns `inEffect`
`false`, exactly like any other manual geometry edit. The reference UI never
sets this field — it is locked at `{0, 0}` there — but preserves it verbatim
if a document already carries one, for a client that does set it.

The resolved values are also kept as `projection.arrangement`: a record of
intent, not canonical geometry. `slice` rectangles stay the canonical stored
form, exactly like a hand-edited grid, so a later manual geometry tweak is
never overwritten or reverted — it simply leaves the record in place.
`GET /api/v1/config/projection/arrangement` reports that record together with
`inEffect`, which is `true` only while re-solving it still reproduces the
current slices; it turns `false` the moment any slice is nudged by hand. A
client can use the record to prefill a rearrangement dialog, or a slider's
starting position, without tracking the grid itself. It also reports
`limits` for the recorded values — see above — so the same read seeds a
dialog's slider ranges as well as its starting position.

Centering the overhang (above) changes what `inEffect` reports for one
existing case: a document whose recorded arrangement is a *content-scale*
one, on an unseamed axis (a single row or column) that overhung the canvas,
used to have that overhang pinned to the canvas's first edge — origin `0` —
rather than centered. Re-solving that record now centers the overhang
instead, so `inEffect` reads `false` for such a record even though nothing
in the document was touched by hand. This is rare — it only ever affected a
content-scale arrangement whose one row or column already exceeded the
canvas — and is accepted as correct: `inEffect` is answering "does the
document still look like this record," and after this change it correctly
no longer does.

The `PUT` behaves like any other config write: `committed: false` (the
default) replaces the shared working copy and applies immediately; `committed:
true` persists it. It takes the same optional preconditions as any other
write, and none of the ownership rules change — see [Multiple
clients](multi-user.md), and especially [High-rate
writers](multi-user.md#high-rate-writers) before driving `overlapX` or
`overlapY` from a slider.

!!! warning "The CPU renderer restarts the slicer on every arrangement change"
    This is not specific to arrangement: the CPU renderer restarts its
    slicer on any geometry change, arrangement included, so a slider driven
    at interactive rates against a CPU-rendered appliance restarts the
    slicer at that rate. The GPU renderer's live control channel does not —
    an arrangement change updates it in place — which is what makes
    slider-driven arrangement practical. See [Where the blend
    runs](how-it-works.md#where-the-blend-runs).

`GET /api/v1/projection/stats` reports the live `geometry` status alongside
frame and lifecycle statistics. It includes requested and effective mode,
requested renderer, warp availability, a limitation reason when present, and
whether warp settings are retained. An effective simple mode therefore does
not imply that saved warp geometry was discarded.

`geometry.effectiveMode` is the one authoritative answer to "which mode is
actually running": it accounts for feature gating, `allowOverlaps`, and
capability-probe hysteresis. `control.effectiveMode` looks similar but is
only the running slicer child's own self-report of what it negotiated, with
none of that policy layered on — read `geometry.effectiveMode` unless the
question is specifically about the child process. Every mode field
(`geometry.requestedMode`, `geometry.effectiveMode`, `control.requestedMode`,
`control.effectiveMode`) and `control.outputs[].samplingMode` use the same
lowercase strings as `projection.mode` in configuration.

Complete schema examples are available for a [simple appliance](examples/four-output-appliance.json)
and a [warp appliance](examples/four-output-warp.json). The warp example
requires `allow_overlaps = true` and a verified GPU pipeline for activation.
The [shared canvas example](examples/four-output-shared-canvas.json) uses CPU
Simple with a 1824×1026 canvas, four 1920×1080 outputs, 200% content scale,
and a 2×2 arrangement with 10% overlap. Switching its mode to Warp uses the
same slice crops once a capable Auto/GPU renderer is selected.

The alpha schema is version 2. Existing files are not migrated; create or
write the current schema directly. There is no legacy seam-weight mode.

Slicing engages whenever the configured layout overlaps, with or without
this section — and on an `allow_overlaps` appliance whenever two or more
outputs take part at all; the section adds the blending. A single-output
warp layout is also sliced; a simple layout can keep a slicer active to probe
capability when warp settings are retained. A full overlap (a stacked projector, a
mirror) is duplicated at full strength and never ramped.

`renderer` chooses which pipeline the slicer blends with. `auto`, the
default, keeps every pixel on the GPU wherever the compositor and the driver
allow it, and falls back — logged, with the reason — to a shared-memory
capture and a CPU blend where they do not. `cpu` always takes that fallback;
`gpu` forces the GPU path and is a **startup error** when it is not actually
available, so a rig that must never fall back silently can say so. What the
two paths cost, the queue priority the GPU path negotiates, and the
arithmetic behind the ramps and `blackLift` are in [Where the blend
runs](how-it-works.md#where-the-blend-runs).

`freeRun` turns off the gate that keeps the wall in step: each output then
takes the newest frame the moment it is ready, independently of the others.
It is for installations that cannot be brought to a shared refresh rate; the
gate it disables, the trade that makes, and how to measure the result are in
[Keeping the displays in step](how-it-works.md#keeping-the-displays-in-step).

Canvas mode requires sway's headless backend
(`WLR_BACKENDS=drm,libinput,headless`, set by provisioning); without it Suede
reports `headless_unavailable` and tiles the layout unsliced.

!!! info "`testPattern` needs no overlaps"
    It is the one field here that is not about blending. Patterns are drawn
    per output in global layout coordinates, so they work on any layout at
    all — including a single display with nothing else configured. That is
    why the web UI puts the control with the **Layout**, not with these
    settings: the grid labels every tile with its output name and global
    coordinates, which makes it the quickest answer to "is this display
    live, which connector is it, and is the whole frame in view". The UI
    shows it as an uncommitted preview and never saves it; an API client
    that writes it with `committed: true` gets a machine that boots into a
    test pattern, which is rarely what anyone wants.

#### Sorting the cables out {: #identify }

`identify` puts the connector's name across the whole output, on a color
derived from that name, with its size and canvas position underneath.

It exists for the moment when the logical order and the physical order
disagree — when `DP-5` is throwing the picture that ought to be second from
the left. Remapping in software is one answer; moving the cable is often the
better one, because everything downstream then agrees, and for that you need
to know which socket is lighting which projector while standing at the rack.

The `grid` pattern names each output too, but in five-pixel text in the
corner of every tile: legible in a photograph, useless from across a room.
Here the name is scaled to the display, so it can be read at a glance, and
the background color means two projectors are never confused even when the
text is too far away to make out.

#### Backgrounds in canvas mode {: #canvas-backgrounds }

The slicer presents on the overlay layer, above everything sway draws on
those outputs — including their backgrounds. That is correct while an
application is producing frames and wrong the moment it is not, so the slicer
only runs when there is something to show: an active application, or a test
pattern. Deactivate the application and the slicer stands down, uncovering
the outputs so their configured backgrounds appear.

Because the pass that stops the slicer runs before the one that stops the
application, the background is already visible by the time the browser exits
— the change is a clean swap rather than a flash of one and then the other.

!!! note "A relaunch still shows black"
    The slicer stands down when no application is *active*, not when the
    active one happens to be restarting. During a relaunch the canvas is
    briefly empty and the projectors show black rather than the background.

#### Working copies and the committed flag {: #live-preview }

More than one client at a time, and what a client should do about it, is covered in [Multiple clients](multi-user.md).

The document carries a truth flag, `committed`. Reads report it honestly:
`true` for the saved document, `false` when a working copy is live. The one
exception is a working copy that differs from the saved document only in
[`projection.temporary`](#highlight-overlaps). It reads `committed: true`,
because saving it would change nothing. It is still live, and Save or Revert
still clears it. Writes use the flag to speak:

- `PUT /api/v1/config` with `"committed": true` validates and **persists** -
  the normal save.
- The same request **without** `committed: true` does everything except
  persist: the document is validated, applied to the outputs, and reconciled
  immediately - but disk keeps the last saved state, so a daemon restart
  returns to it.
- `POST /api/v1/config/revert` discards the working copy, re-applies the
  saved document, and returns it.

The web UI uses this grammar for the layout and projection editors: every
edit is pushed uncommitted as it is made - the picture follows the numbers as
you type - Save sends the same document with the flag set, and Revert calls
revert.

`GET /api/v1/config`, every write (including the section endpoints, which
always commit), and `POST /api/v1/config/revert` return the exact accepted
revision in `ETag` and effective working-copy identity in
`X-Config-Generation`, plus the store instance in `X-Config-Epoch`.

**The server holds at most one working copy, shared by everyone.** It is not
scoped to a client, a session, or a browser tab: it exists to let anyone
defer the decision to save or discard an edit, not to give one editor
exclusive possession of it. Any write, preview, commit, or revert applies to
whatever is currently live, whoever made it — a script's unconditional `PUT`
folds straight into another operator's in-progress edit rather than being
turned away, and an unconditional `POST /api/v1/config/revert` discards
whatever working copy is live, not only one this caller created. Nothing
about that is silent: every connected client, including third-party apps,
learns of the change in real time through the `config_changed` SSE event (see
[Events (SSE)](specification.md#events-sse)), so an operator watching the
page sees another operator's edit, commit, or revert happen live.

`If-Match`, `If-Config-Generation`, and `If-Config-Epoch` remain fully
optional compare-and-swap preconditions, for a client that wants its *own*
write rejected if the document moved since it last read it — a UI that wants
to warn about a conflicting edit rather than silently overwrite one, say. A
precondition that is actually sent and no longer matches still gets `409`;
one that is simply omitted never does. A write that does name a working copy
commits on that one working copy alone — never a hybrid of it and the last
saved document.

```bash
# A plain read establishes the working-copy identity to build on.
curl -sD - http://appliance:9088/api/v1/config/settings -o /dev/null \
  | grep -i x-config-generation
# x-config-generation: 4

# Optional: reject this write if the document has moved on since the read
# above (a stale value still gets 409; omitting the header never does).
curl -X PUT http://appliance:9088/api/v1/config/settings \
  -H 'Content-Type: application/json' \
  -H 'If-Config-Generation: 4' \
  -d '{"hideCursor": false, "outputPollIntervalSeconds": 5}'
```

#### Editing geometry in the web UI {: #geometry-editor }

In **Displays → Layout**, edit canvas aspect and render width, choose Simple
or Warp, then select an output in the table or diagram. Hardware settings,
shared content crop, and Warp correction use that one selection. Missing
calibration is initialized with identity pins and neutral centers; switching
modes preserves the shared composition and retained calibration.

Crop X/Y are canvas pixels. Content scale is enlargement from canvas pixels
to output pixels: 200% maps a 960×540 crop to a 1920×1080 output. This differs
from compositor **Output scale** and the read-only global **Rendering scale**.
The canonical stored values are normalized rectangles; displayed crop pixels
and percentages are derived, never separately stored. Width-only changes
preserve those rectangles, pins, and physical footprints, avoiding drift on
repeated resolution changes. Aspect edits keep crop pixel origins and content
scale anchored, exposing unused or out-of-bounds areas for adjustment.

**Arrange automatically** accepts positive integer rows/columns, a Content
scale percentage (100% is 1:1 pixel sampling), and separate **Overlap X** and
**Overlap Y** percentages of adjacent tile extents. Content scale and the two
overlaps are linked: editing either overlap fills Content scale, and editing
Content scale fills both overlaps. The dialog calls the daemon
(`GET /api/v1/projection/arrangement` while previewing,
`PUT /api/v1/config/projection/arrangement` on Apply) rather than solving
locally, so every client previewing the same grid — including a show
controller driving it by sliders — sees the same result. Both overlaps are
honored exactly: the axis with the larger resulting scale fills the canvas
and the other axis is centered with any unused canvas on either side, shown
in the dialog alongside the aspect that would remove it. See [Grid
arrangement](#grid-arrangement) for the full contract. It places enabled
configured outputs in table order, including disconnected outputs with known
modes. Disabled outputs and existing calibration stay unchanged; mixed-size
grids that fail slice topology validation are rejected. Arrangement is one
unsaved edit — `committed: false` — exactly like any other working-copy
change, so a subsequent Save or Revert still applies to the whole page.

Drag the four corner handles to place the picture inside the output raster.
The four center-line handles control two shared fractions: moving either end
of a line moves its partner. Use the numeric fields for precise coordinates,
or focus a handle and press an arrow key for a 0.001 step (Shift: 0.01).
Invalid edits show their reason and retain the last accepted shape. **Reset
centers** and **Reset pins** are also unsaved edits. Slice placement and the
physical raster footprint have separate controls; destination pin movement
does not change the selected browser content or its dimensions.

One **Save** persists the entire Displays page; **Revert** restores its committed
state, including canvas dimensions. Previews are serialized with both operations.
Switching output rows retains drafts, and invalid fields block Save. If another client or a
daemon restart changes the working copy, the editor keeps your local edits
and offers **Export local edits** or **Discard local edits and reload**.
Warp correction controls require verified capability. During CPU fallback,
Simple is visibly selected and Warp is disabled; normal health warnings explain
the reason. Shared crops and canvas remain editable. Saving those edits keeps
the requested Warp intent and calibration for capability recovery. Deliberately
selecting Simple changes that intent and survives reload.

Recommendations refresh automatically after material edits settle. They show
calculating, stale, invalid, and unavailable states; stale presets cannot apply.
Focus render width to expose 25%, 50%, 75%, and 100% buttons; keyboard or touch
activation fills that field. Width/aspect edits preview directly and can resize
and reflow the source browser. Rendering scale is chosen width / ideal width,
so 50% width corresponds to approximately 25% of the pixels at fixed aspect.
It can exceed 100% within allocation limits. Revert restores the previous size.

Pin toggles promote the exact resolved mode, output scale, or transform to an
explicit setting; unpinning returns to automatic adoption. Provenance labels
distinguish waiting, adopted, and pinned values. Position is never adopted,
and max render time Off is not automatic. Compact numeric labels retain full
canonical precision: focus/blur, selection, unrelated edits, and Save do not
round calibration. Raw config retains exact stored numbers.

The editor distinguishes server acceptance from slicer installation using
`control.configGeneration.applied` (the working-copy generation the current
slicer session has actually installed) against `control.configGeneration.requested`
and the current child session. Numeric working-copy generations are scoped to
`X-Config-Epoch`; they are never comparable with `control.childGeneration`,
which is the child's own protocol sequence and resets on every slicer
restart. Presentation receipt is reported per output, in
`control.outputs[].presentedGeneration`, and does not measure when the light
became visible. Pattern changes and a Save that removes a diagnostic pattern
preserve the stored geometry and mode.

#### Test patterns {: #projection-test-patterns }

Built into the blending component and drawn in **canvas** coordinates, so a
canvas-anchored feature lands at the same canvas position regardless of an
output's Content scale or slice crop: two aligned projectors superimpose
the pattern pixel for pixel. A pattern is sampled through exactly the same
slice-rectangle, warp, and blend path real content is — on both the GPU and
CPU rendering paths, and at any Content scale, not only 100% — so what you
align with a pattern is what content will actually experience; calibration
done on a pattern holds once you switch back to content. Ramps and black
lift apply to the pattern the same way. Patterns work regardless of `blend`,
because alignment comes first — and, being canvas-anchored rather than
per-output, they are unaffected by the overlapping-output limitation above,
which makes them the right tool for checking a rig before committing to a
layout.

| Pattern | For |
|---|---|
| `grid` | Geometry, focus, and seam alignment: 100 px color tiles with crosses, each labeled with its canvas pixel coordinates and the output name. Misaligned projectors show doubled crosses in the overlap; aligned ones show one. |
| `warp-alignment` | Corner and center-pin calibration: 10% grid lines and a center alignment circle on dark gray, drawn in canvas space so the same lines line up across outputs regardless of each one's slice crop. |
| `white` | The blend ramps in isolation, and brightness mismatch between projectors. |
| `black` | Tuning `blackLift`: the seams glow with doubled projector black; raise the lift until the rest of the image matches them. |
| `gamma` | Measuring `gamma`: candidate patches sit inside a stripe field that averages to half light. From a distance, the patch that melts into its stripes names the projector's gamma; the configured value is underlined. |
| `sync` | Measuring output-to-output presentation sync with a camera. Every output shows the *same* two-digit counter, drawn by the blending component itself and advanced once per present cycle, with a 16-bit binary strip of the same counter beside it, four large cells along the bottom edge carrying the counter's low four bits (most significant at the left, filled for one and hollow for zero), the output's name top left, and the stats snapshot id with a UTC `HH:MM:SS.mmm` clock bottom left. Photograph two or more outputs in one exposure at **1/1000 s or faster**; on DLP projectors take **two consecutive frames**, because the color wheel can leave a single exposure showing part of two refreshes. Report, per output, the number showing — or "two numbers visible" when an output straddles a refresh. Needs `allowOverlaps = true`. |

The gamma chart assumes the output runs at scale 1 (its stripes are
single-pixel rows); the other patterns have no such constraint.

`sync` is the only animated pattern, and the only one that needs the
blending component's slicer: it is drawn per frame through exactly the path
content takes, which is what makes a photograph of it a measurement of that
path rather than of a browser. On an appliance with `allowOverlaps = false`
sway tiles the layout directly and the per-output overlays are painted once,
so there is nothing to animate — those outputs show `--` and the words
`sync needs the slicer` instead of a counter frozen at one number, which
would read as perfect sync. Read the photograph against `straddles` and
`zeroCopyPresented` in `GET /api/v1/projection/stats` for the same interval:
matching digits with `straddles` at 0 means the outputs are in step, while
differing digits with `straddles` at 0 means the compositor's flip reports
and the light on the wall disagree.

For a *video* rather than a still — a high-speed clip of the whole wall,
measured offline by script — read the four big bottom cells instead of the
digits: they are each about a twelfth of the output wide, so they threshold
at any framing that fits four projectors in shot, and they give the frame
transitions and a lag of up to 15 frames without resolving anything smaller.
The bottom-left clock is UTC time of day to the millisecond, sampled once per
present cycle and therefore identical on every output of a frame; one legible
frame of the clip is enough to line the whole clip up against the stats log
and the journal, which are Unix time too.

#### Highlighting overlaps {: #highlight-overlaps }

**Highlight overlaps**, in the web UI's edge-blending panel, draws two
lines on every output along each seam it blends across. Use it to line up
warp geometry. In the API it is `projection.temporary.highlightOverlaps`.

- **Orange** marks where this output's blend begins: the last pixel at full
  strength and the first one that fades.
- **Blue** marks the far edge of the same overlap, where this output has
  faded out completely.

Where a seam is lined up, one output's orange lands exactly on its
neighbor's blue, and the pair adds up to a single **white** line. Adjust the
corner and center pins until every pair turns white. A misaligned pair shows
separate orange and blue lines, or orange and light-blue fringes about as
wide as the error. A fringe of up to about half an output pixel is normal
even at the best alignment, because neighboring projectors' pixel grids
don't line up exactly.

- **Width.** Each line is 2 pixels of the output's own resolution, measured
  before the warp. A warp that stretches an output also stretches its lines.
- **Color, and a gamma check.** The lines are drawn at full strength over
  the blend, not faded by it. Their green channel comes from `gamma`: it is
  `255 × 0.5^(1/gamma)`, 186 at the default 2.2, so each output contributes
  exactly half the green light and the pair sums to white. When the lines
  coincide but the result is tinted, `gamma` doesn't match the projectors.
  Lilac means the projectors' real gamma is higher than the setting; greenish
  means it is lower.
- **What to show underneath.** Black content (or the `black` pattern) gives
  the clearest reading: lined-up pairs are white lines on black, and
  misaligned ones are separate colored lines.
- **Where lines appear.** Lines appear only while `blend` is on and only at
  real seams. Nothing is drawn on the wall's outer edges or on stacked
  projectors. Where a horizontal and a vertical seam cross, orange wins.
  An overlap narrower than about two output pixels shows only orange.
- **Never saved.** Save and Revert both switch it off, and so does a daemon
  restart. Turning it on doesn't count as an unsaved edit; see
  [Working copies and the committed flag](#live-preview).
- **No cost when off.** The GPU and CPU renderers switch to a line-drawing
  variant only while it is on. With it off they run exactly the code they
  would without the feature.

`temporary` is the home for calibration aids like this one. Anything in it
is reset whenever a document is saved, by the server itself, whichever
client does the saving. `testPattern` predates it and is not reset this way:
the web UI clears it before saving, but an API client has to do so itself.

!!! warning "Keep an output at position 0,0"
    Sway anchors a spanned (`fullscreen global`) surface at the layout
    origin. If every remaining output sits away from `0,0` — say the
    left-most projector is unplugged — the spanned window is clipped to the
    missing region and the surviving outputs show only a sliver. Position
    layouts so one output starts at the origin, which a normal
    left-to-right arrangement does anyway.

Suede must be built with the `projection` cargo feature (on by default). A
build without it still accepts and stores this configuration, and reports a
`projection_unavailable` divergence if asked to blend.

### Settings

| Field | Default | Meaning |
|---|---|---|
| `hideCursor` | `true` | Hide the pointer and park it below the layout |
| `outputPollIntervalSeconds` | `5` | Backstop poll, in case an event is missed |
| `measureCapabilitiesOnStart` | `true` | Measure browser decode capabilities at startup when something changed — see below |

#### Raw Sway commands {: #raw-sway-commands }

`POST /sway/command` sends a string straight to Sway's IPC (`{"command": "..."}`,
for debugging). It is always available, and was never actually the privilege
boundary it looked like: an `exec`-kind launcher already runs any program as
the session user, and apps are configured through this same API, so a client
that could reach the endpoint could already run anything by defining an app.
A setting that implied a protection it did not provide (`allowRawSwayCommands`,
retired) was worse than not having one.

## Where state is stored

`$XDG_STATE_HOME/suede/state.json`, written atomically: a temp file is written and fsynced, then renamed over the target, and the previous version is kept as `state.json.bak`. A primary that is not valid JSON falls back to the backup, and a backup that is not valid JSON either falls back to an empty document — an appliance must still boot.

Package upgrades never touch this directory, so configuration survives them by construction.

### An old or invalid document never stops the daemon

Suede does not migrate documents between schema versions, and refusing to
boot on one is never the right answer. An old-schema document loads;
settings this build no longer understands, or values that fail validation,
are repaired field by field back to their defaults — a single bad field is
dropped, a section that cannot be read at all falls back to its default —
and the daemon starts from whatever is left rather than from nothing. Every
repair is logged and reported as a `state_document_repaired` divergence in
`GET /api/v1/status`; see [The saved file needed repair at
startup](troubleshooting.md#state-document-repaired) for what to do about
one. Nothing is silently rewritten: before a repaired document is saved
over the original, the file as found is copied to `state.json.rejected`, so
the exact bytes Suede would not load are still on disk to inspect or
recover fields from by hand.

The one document this does not apply to is one written by a *newer* Suede.
That is not a file to repair — this build cannot know what a field from the
future means — so it is refused outright, with a message naming the schema
found and the schema this build understands, rather than quietly discarding
a working configuration by starting from defaults.

### A write is memory-first

A write is accepted and takes effect — the reconciler drives toward it and
the response reports it as accepted — as soon as it validates, before it is
known to be on disk. Saving to `state.json` happens afterward, off the
request path. If that save fails (a full or read-only state directory,
commonly), the change stays live rather than being rolled back: memory and
the API response are never behind what the outputs are actually doing, only
disk can lag, and Suede keeps reporting a `state_not_persisted` divergence
in `GET /api/v1/status` until a later save reaches disk. See [A change is
live but was not saved](troubleshooting.md#state-not-persisted). Saves are
ordered by revision, so a slow save can never overwrite a newer one that
finished first.
