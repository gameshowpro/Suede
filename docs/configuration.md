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

!!! info "Layout is the client's job"
    Suede does no layout arithmetic. Positions are always explicit. The web UI offers left-to-right arrangement as a convenience that simply computes `position` values, and any client can do the same.

!!! warning "Connected outputs with no entry are left alone"
    Suede only touches outputs you have configured. To turn one off, give it an entry with `"enable": false`.

### Backgrounds and wallpapers

A blank screen looks broken even when it is only a browser restarting. A
background gives an output something deliberate to show whenever no window
covers it — during a relaunch, or before the first app starts.

A background has three properties, all optional:

| Field | Type | Default | Meaning |
|---|---|---|---|
| `wallpaper` | string \| null | null | Id of an uploaded image. Absent means the colour alone |
| `color` | string \| null | `#000000` | `#rrggbb`, used alone or wherever the image does not reach |
| `mode` | string | `fill` | `fill`, `fit`, `stretch`, `center`, `tile` |

The colour is never left unstated. Every mode except `fill` and `stretch`
leaves part of the screen uncovered, and an unpainted region shows whatever the
compositor last left there — usually a stale frame of the previous app.

#### Named backgrounds {: #named-backgrounds }

Define a background once and let any number of outputs name it. A video wall
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
the wall on the next pass.

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

| Field | Type | Default | Meaning |
|---|---|---|---|
| `id` | string | required | Unique, stable; also used as a profile directory name |
| `enabled` | bool | `true` | Disabling terminates a running instance |
| `launcher` | object | required | See below |
| `output` | object \| null | null | Output to place the window on |
| `fullscreen` | bool | `true` | Fill the target output |
| `spanOutputs` | bool | `false` | Stretch one window across **every** output |
| `readiness` | object \| null | null | Wait for a URL to answer before launching |
| `env` | object | `{}` | Extra environment variables for the process |
| `audio` | object \| null | null | Absent leaves routing alone; see below |
| `heartbeat` | object \| null | null | Content watchdog |
| `restart` | object | always/1s/30s | Restart policy and backoff |
| `persistProfile` | bool | `false` | Keep the browser profile between launches |

#### Driving a video wall

Setting `spanOutputs: true` stretches a single window across the whole layout
rather than filling one output — sway's `fullscreen enable global`. This is how
one browser drives several displays as a single canvas:

```json
{
  "outputs": [
    {"match":{"name":"HDMI-A-1"},"enable":true,"mode":{"width":1920,"height":1080,"refreshHz":60},"position":{"x":0,"y":0}},
    {"match":{"name":"HDMI-A-2"},"enable":true,"mode":{"width":1920,"height":1080,"refreshHz":60},"position":{"x":1920,"y":0}},
    {"match":{"name":"HDMI-A-3"},"enable":true,"mode":{"width":1920,"height":1080,"refreshHz":60},"position":{"x":3840,"y":0}},
    {"match":{"name":"HDMI-A-4"},"enable":true,"mode":{"width":1920,"height":1080,"refreshHz":60},"position":{"x":5760,"y":0}}
  ],
  "apps": [
    {"id":"wall","enabled":true,"spanOutputs":true,
     "launcher":{"kind":"chromium-kiosk","uri":"http://control.local/wall"}}
  ]
}
```

The page then sees one 7680x1080 viewport. Position the outputs to form the
canvas you want; Suede performs no layout arithmetic, so the geometry is
entirely yours.

!!! warning "Start sway with direct scanout disabled"
    Spanning needs `WLR_SCENE_DISABLE_DIRECT_SCANOUT=1` on the compositor.
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

    Expands to a kiosk argument set carried over from production use: `--kiosk`, `--password-store=basic` (no keyring prompt on a headless box), `--ozone-platform=wayland`, `--no-first-run`, hardware-decode and zero-copy flags, and a private `--user-data-dir`. `extraArgs` are appended before the URI.

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
| absent | Do not touch routing; the app uses the default sink |
| `{"output": "alsa_output.…"}` | Route to that sink, by PipeWire `node.name` |
| `{"output": null}` | Route to Suede's null sink — the app plays silently |

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

For a wall of projectors whose images physically overlap, Suede can fade each
side of every seam so the doubled light adds up to one seamless picture. This
is phase one of projection support: it assumes the projectors handle their own
geometry (corner pinning), which most installation-class projectors do.

The pieces:

1. **Overlap comes from the layout.** Position the outputs so their
   rectangles intersect — a 160&nbsp;px seam between two 1920-wide projectors
   means positions `0` and `1760`. A spanned app
   ([`spanOutputs`](#driving-a-video-wall)) then renders the shared strip on
   both outputs. There is no separate overlap setting to fall out of step
   with the layout: the blend regions *are* the intersections.
2. **Blending is a ramp in light, not in signal.** A display raises its input
   signal to a power (its gamma, typically 2.2), so a gradient that is linear
   in signal is far from linear in light and produces a visible bright band
   at every seam. Suede shapes each ramp as `ramp^(1/gamma)` per projector,
   which makes the summed luminance across a seam constant — set each
   projector's measured gamma and the seams disappear.
3. **The overlays are tiny and passive.** Each projector gets one
   `suede blend` process: a black, input-transparent layer surface whose
   alpha channel holds the ramps. It is drawn once, costs nothing per frame,
   and clicks pass straight through. The reconciler starts, restarts, and
   retires them as the layout or configuration changes.

```json
"projection": {
  "blend": true,
  "outputs": [
    { "name": "DP-1", "gamma": 2.2 },
    { "name": "DP-2", "gamma": 2.15 }
  ]
}
```

| Field | Type | Default | Meaning |
|---|---|---|---|
| `blend` | bool | `true` | `false` skips the entire chain: overlays are torn down, nothing runs |
| `outputs[].name` | string | — | Connector name of a projector; unlisted outputs are untouched |
| `outputs[].gamma` | number | `2.2` | That projector's transfer gamma, 1.0–4.0 |

Only listed outputs take part, so an operator monitor beside the wall keeps
rendering normally. Setting `blend: false` — or removing the section with
`PUT /api/v1/config/projection` and a `null` body — stops every overlay and
skips all projection work; nothing is spawned and nothing is checked.

Rows, columns, and grids all work: seams are derived pairwise from wherever
listed outputs intersect, and in a 2×2 grid the corner region multiplies its
horizontal and vertical ramps, which sums correctly by construction.

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

!!! note "Phase two"
    Corner pinning and per-output source rectangles (for projectors without
    built-in geometry) are designed but not yet built; this configuration
    shape is forward-compatible with them. Measure each projector's gamma
    with a test pattern for best results — per-channel gamma is also planned.

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
