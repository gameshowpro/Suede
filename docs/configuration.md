# Configuration

There are two kinds of configuration, and the split is a hard rule:

- **Bootstrap configuration** is anything that must be known before the API can serve — the bind address, the token, the state directory. It lives in a file, is read once at startup, and is never written by Suede.
- **Desired state** is everything else: outputs, applications, audio routing, daemon settings. It is owned by the API, persisted by Suede, and reconciled continuously.

## Bootstrap configuration

Read from `$XDG_CONFIG_HOME/suede/suede.toml` (usually `~/.config/suede/suede.toml`). Every value can be overridden by an environment variable, which wins. A missing file means all defaults.

```toml
--8<-- "examples/suede.toml"
```

| Key | Environment | Default | Meaning |
|---|---|---|---|
| `bind` | `SUEDE_BIND` | `0.0.0.0:9088` | Address the HTTP server binds to |
| `token` | `SUEDE_TOKEN` | unset | Bearer token; setting it disables the web UI |
| `state_dir` | `SUEDE_STATE_DIR` | `$XDG_STATE_HOME/suede` | Where desired state is persisted |
| `docs_base_url` | `SUEDE_DOCS_BASE_URL` | `https://suede.gameshow.pro/` | Base for health-check documentation links |

## Desired state

One JSON document, written through `PUT /api/v1/config` or section by section. Here is a complete four-output appliance:

```json
--8<-- "examples/four-output-appliance.json"
```

Writes are validated synchronously and return once **persisted**, not once applied — reconciliation may take seconds, or be impossible right now because a display is unplugged. Add `?wait=<seconds>` to block until it settles.

The document carries a server-managed `revision`. Send it back as `If-Match` to make a write conditional; a stale value gets `409 Conflict`.

### Outputs

| Field | Type | Default | Meaning |
|---|---|---|---|
| `match` | object | required | Which physical output this applies to |
| `enable` | bool | `true` | `false` actively disables the output |
| `mode` | object \| null | null | `{width, height, refreshHz}`; null leaves Sway's preferred mode |
| `position` | object \| null | null | `{x, y}` in the global layout |
| `scale` | number \| null | null | Output scale factor |
| `transform` | string \| null | null | `normal`, `90`, `180`, `270`, `flipped`, `flipped-90`… |
| `adaptiveSync` | bool | `false` | Variable refresh rate |
| `allowTearing` | bool | `false` | Applied only on Sway 1.10+; reported as a divergence otherwise |
| `maxRenderTimeMs` | number \| null | null | Frame render deadline; null means off |
| `background` | object \| null | null | What the output shows when no window covers it |

`match` selects by connector name, which is the normal case:

```json
{ "name": "HDMI-A-1" }
```

or by EDID, for installations where connector enumeration is unstable:

```json
{ "make": "Acme Displays", "model": "AD-2400", "serial": "0x00012345" }
```

Every field you specify must match. A configured output that is not currently connected is a reported divergence, not an error — Suede keeps the configuration and applies it when the display appears.

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

!!! info "Layout is the client's job"
    Suede does no layout arithmetic. Positions are always explicit. The web UI offers left-to-right arrangement as a convenience that simply computes `position` values, and any client can do the same.

!!! info "The Displays tab shows every connector"
    Being *in the layout* means having a configuration entry, which has
    nothing to do with what is plugged in. The layout diagram draws the
    configured entries, marking any with nothing attached; every remaining
    connector the machine has is listed below it as something to add. So a
    connector with no display can be placed in the layout, and a display
    removed from the layout returns to that list rather than disappearing.

!!! warning "Connected outputs with no entry are left alone"
    Suede only touches outputs you have configured. To turn one off, give it an entry with `"enable": false`.

### Backgrounds and wallpapers

A blank screen looks broken even when it is only a browser restarting. A
background gives an output something deliberate to show whenever no window
covers it — during a relaunch, or before the first app starts.

A background has three properties, all optional:

| Field | Type | Default | Meaning |
|---|---|---|---|
| `wallpaper` | string \| null | null | Id of an uploaded image. Absent means the color alone |
| `color` | string \| null | `#000000` | `#rrggbb`, used alone or wherever the image does not reach |
| `mode` | string | `fill` | `fill`, `fit`, `stretch`, `center`, `tile` |

The color is never left unstated. Every mode except `fill` and `stretch`
leaves part of the screen uncovered, and an unpainted region shows whatever the
compositor last left there — usually a stale frame of the previous app.

#### Named backgrounds {: #named-backgrounds }

Define a background once and let any number of outputs name it. A multi-display installation
normally wants one look across every screen, and repeating the same three
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
screen, while a script driving the API directly should not have to create a
preset to paint a single output.

Naming a preset that does not exist is rejected at the write, not at reconcile
time — a typo is a mistake in the request, and the writer is the only one who
can still fix it cheaply. Deleting a preset an output still names is refused
with `409`, because cascading would blank those screens.

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
    nothing appears — a black screen with no error anywhere. The `swaybg`
    health check fails whenever an output configures a background and the
    program is missing.

### Applications

An application is a *launch specification*, not a window. That is what makes it restorable after a reboot.

**One application is active at a time — `activeApp` — and it always covers
the whole canvas.** The rest of the list is a library to switch between:
`POST /api/v1/apps/{id}/activate` swaps every display to another app atomically,
killing the previous one and launching the new. There is no per-app output
targeting and no per-app enable flag; the appliance is a single canvas, not a
window manager.

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
    [projection](#projection-edge-blending). Documents written for an earlier
    version may carry `enabled`, `output`, `fullscreen` or `spanOutputs` on an
    app; those fields are ignored, not rejected, so check `activeApp` if
    nothing launches.

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

!!! warning "Start sway with direct scanout disabled"
    Spanning a non-overlapping layout needs
    `WLR_SCENE_DISABLE_DIRECT_SCANOUT=1` on the compositor.
    Without it, some drivers show the same part of the window on every display
    instead of spanning — see
    [troubleshooting](troubleshooting.md#a-spanned-window-mirrors-instead-of-spanning).
    `provision.sh` sets it, and the `direct-scanout` health check warns if it is
    missing.

!!! tip "Give the outputs matching heights"
    A spanned window covers the *bounding box* of every output. Where an output
    is shorter than its neighbours, the content below it falls outside any
    display and is simply not visible.

#### Launchers

=== "Chromium kiosk"

    ```json
    {
      "kind": "chromium-kiosk",
      "uri": "http://control.local/render/1",
      "showFpsCounter": false,
      "extraArgs": []
    }
    ```

    Expands to a kiosk argument set carried over from production use: `--kiosk`, `--password-store=basic` (no keyring prompt on a headless box), `--ozone-platform=wayland`, `--no-first-run`, `--autoplay-policy=no-user-gesture-required`, hardware-decode and zero-copy flags, and a private `--user-data-dir`. `extraArgs` are appended before the URI.

!!! info "Pages may make a sound without being clicked"
    Chromium normally suspends every `AudioContext`, `<audio>` and `<video>`
    until a "user gesture", and on an appliance no gesture is ever coming — a
    page that plays perfectly on a desk is simply silent on the machine. The
    preset therefore sets `--autoplay-policy=no-user-gesture-required`. The
    consent the policy exists to obtain was given when the operator chose what
    the machine runs.

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
    The `chromium-kiosk` preset asks for `VaapiVideoDecoder`, but that only
    takes effect if a VA-API driver for your GPU is installed. Without one,
    Chromium falls back to software decode *silently* — nothing fails, it just
    uses the CPU. Nvidia cards need `nvidia-vaapi-driver`; Intel needs
    `intel-media-va-driver`. Confirm from inside the browser with
    `navigator.mediaCapabilities.decodingInfo(...)`, whose `powerEfficient`
    flag is the honest answer, or watch `nvidia-smi dmon -s u` and look at the
    `dec` column while a video plays.

    Rasterisation, compositing, WebGL and CSS animation are a separate path and
    generally work without any of this — check the WebGL renderer string is
    your GPU rather than `llvmpipe` or `SwiftShader`.

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

The first is a decision deferred, not a decision recorded. An app with no
`audio` field follows the machine's default sink wherever it goes, and the
default can move on its own — plugging in a USB headset is enough for
WirePlumber to promote it. Naming a sink pins the app to it regardless.

Get the available identifiers from `GET /api/v1/audio/outputs`. Changing an app's sink relaunches it, because routing is applied at launch through `PULSE_SINK`.

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

The endpoint is unauthenticated but accepted only from loopback, so the key-free design cannot be abused from the network.

### Projection and edge blending {: #projection-edge-blending }

For two to four projectors whose beams physically overlap, **the
layout is the projection configuration**. Position each output in canvas
space exactly as its beam lands on the surface — overlapping the neighbours by
however much the rigging actually overlaps, each seam its own amount, rows
and grids included. The canvas is the layout's bounding box, and the Displays
tab reports it live.

The layout must be **contiguous**: every enabled output must chain back to
the first through overlaps or shared edges (any number of intermediates; a
corner-to-corner touch does not count). A gap would leave part of the canvas
mapped to no projector — content silently lost — so validation rejects it
like any other invalid write. Outputs with no configured `mode` or
`position` take their geometry from observation and are exempt from the
check.

Sway never sees any of this. It is always handed a plain edge-to-edge tiling
(sway cannot render overlapping outputs distinctly — its single global
coordinate space gives every output the same pixels in a shared region,
measured on hardware). Instead:

1. The active app renders once into a **headless canvas** the size of the
   layout's bounding box.
2. The **slicer** (`suede slice`, one process per installation) captures the canvas
   each frame, cuts out each projector's configured rectangle — intersecting
   regions are cut into *both* neighbours — applies the gamma-shaped blend
   ramps and black lift per pixel, and presents each slice fullscreen on its
   own output. The loop is damage-driven; a static page costs nothing.

Superimposed on the surface, the two copies of every seam sum to constant
luminance (measured: worst deviation 0.008 across a 160 px seam). A layout
with no overlaps skips all of this: sway tiles it directly, at zero cost.

```json
"projection": { "blend": true, "gamma": 2.2, "blackLift": 0.04 }
```

| Field | Type | Default | Meaning |
|---|---|---|---|
| `blend` | bool | `true` | `false` slices without ramps — overlapping beams still need the duplication, just unfaded |
| `gamma` | number | `2.2` | The projectors' transfer gamma, 1.0-4.0; shapes every ramp's fall-off |
| `blackLift` | number | `0.0` | Black-level compensation outside the seams, 0-0.5 |
| `testPattern` | string or null | null | `grid`, `white`, `black`, `gamma` - or null for content |

Slicing engages whenever the configured layout overlaps, with or without
this section; the section adds the blending. A full overlap (a stacked
projector, a mirror) is duplicated at full strength and never ramped.

**Blending is a ramp in light, not in signal.** A display raises its input
signal to a power (its gamma, typically 2.2), so a gradient linear in signal
leaves a bright band at every seam. Ramps are shaped as `ramp^(1/gamma)`.

**Black-level compensation.** Projector black is not zero light, so seams
glow on dark scenes. The seam cannot be darkened, so `blackLift` brightens
everything else to match: `out = lift + (1 - lift) * in` outside the seams.
Show the `black` test pattern and raise it until the projected image is even.

Canvas mode requires sway's headless backend
(`WLR_BACKENDS=drm,libinput,headless`, set by provisioning); without it Suede
reports `headless_unavailable` and tiles the layout unsliced.

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

The document carries a truth flag, `committed`. Reads report it honestly:
`true` for the saved document, `false` when a working copy is live. Writes
use it to speak:

- `PUT /api/v1/config` with `"committed": true` validates and **persists** -
  the normal save.
- The same request **without** `committed: true` does everything except
  persist: the document is validated, applied to the outputs, and reconciled
  immediately - but disk keeps the last saved state, so a daemon restart
  returns to it.
- `POST /api/v1/config/revert` discards the working copy, re-applies the
  saved document, and returns it.

Any committed write (including the section endpoints, which always commit)
supersedes a live working copy. The web UI uses this grammar for the layout
and projection editors: every edit is pushed uncommitted as it is made - the
picture follows the numbers as you type - Save sends the same document with the
flag set, and Cancel calls revert.

#### Test patterns {: #projection-test-patterns }

Built into the blending component, sized automatically to each output, and
drawn in **global** coordinates so features continue exactly across a seam:
two aligned projectors superimpose the pattern pixel for pixel. Ramps and
black lift apply to the pattern exactly as they would to content, so what you
align with is what content will experience. Patterns work regardless of
`blend`, because alignment comes first — and, being drawn globally, they are
unaffected by the overlapping-output limitation above, which makes them the
right tool for checking a rig before committing to a layout.

| Pattern | For |
|---|---|
| `grid` | Geometry, focus, and seam alignment: 100 px color tiles with crosses, each labelled with its global pixel coordinates and the output name. Misaligned projectors show doubled crosses in the overlap; aligned ones show one. |
| `white` | The blend ramps in isolation, and brightness mismatch between projectors. |
| `black` | Tuning `blackLift`: the seams glow with doubled projector black; raise the lift until the rest of the image matches them. |
| `gamma` | Measuring `gamma`: candidate patches sit inside a stripe field that averages to half light. From a distance, the patch that melts into its stripes names the projector's gamma; the configured value is underlined. |

The gamma chart assumes the output runs at scale 1 (its stripes are
single-pixel rows); the other patterns have no such constraint.

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
| `allowRawSwayCommands` | `false` | Enable `POST /sway/command` passthrough |

!!! danger "Raw command passthrough"
    `allowRawSwayCommands` permits arbitrary Sway commands, including `exec`. It is off by default and intended for debugging.

## Where state is stored

`$XDG_STATE_HOME/suede/state.json`, written atomically: a temp file is written and fsynced, then renamed over the target, and the previous version is kept as `state.json.bak`. A corrupt primary falls back to the backup, and a corrupt backup falls back to an empty document — an appliance must still boot.

Package upgrades never touch this directory, so configuration survives them by construction.
