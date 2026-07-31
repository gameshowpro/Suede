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
| `bind` | `SUEDE_BIND` | `0.0.0.0:7071` | Address the HTTP server binds to |
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
