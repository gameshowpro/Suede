# Suede

Suede is a daemon with a name that is a hilarious pun on the word "Swayed". It integrates with [Sway](https://swaywm.org/) to turn a headless-ish Linux box into a remotely manageable display appliance. It adds three headline features:

1. Remote management through a well-documented REST + SSE API.
2. A reference implementation of the API served as a local web UI.
3. State persistence and restoration at boot.

Window launching and termination is particularly focused on the browsers Chromium and Firefox, running in kiosk mode.

## Primary use case

A rack-mounted media server drives 4 HDMI outputs. An operator on another machine uses the API (or the reference web UI) to:

- Enumerate the connected displays, their EDID identity (make/model/serial), and supported modes.
- Set each output's mode, position, scale, and options (adaptive sync, tearing, max render time).
- Launch one Chromium instance per output in kiosk mode, each loading a specified URI.
- Have all of the above survive a reboot with no operator intervention: the machine boots, Sway starts, Suede starts, and the last-applied configuration is restored automatically.

## Design principles

- **Declarative, not imperative.** Clients do not send "commands"; they write *desired state*. Suede persists it and runs a reconciler that continuously drives the live Sway session toward it — at boot, after an API write, and after hardware events (display hotplug, app crash). This makes persistence, boot-restore, and hotplug-recovery a single code path.
- **Sway's vocabulary.** The API sticks to the terminology, method names, and object definitions used by Sway wherever possible: *outputs* (not displays), *modes*, *workspaces*, *app_id*. Where new concepts are introduced (e.g. *apps* as managed launch specifications), naming stays consistent with the rest of the API.
- **Observed vs. desired are separate resources.** What Sway reports lives under read-only endpoints; what the client wants lives under read-write config endpoints. The two are never conflated.
- **Degrade gracefully.** If the desired state cannot be fully realized (an output is unplugged, a mode is unsupported), Suede applies as much as it can, reports the divergence via the API/SSE, and keeps trying as conditions change. It never discards desired state because it is currently unachievable.

## Non-goals

- General-purpose remote window management (arbitrary tiling, workspace choreography). Suede manages outputs and the lifecycle of apps it launched; it observes, but does not manage, other windows.
- TLS termination and user management. Deployments needing transport security should front Suede with a reverse proxy. (See [Security](#security).)
- Multi-seat or multi-session support. One Sway session per machine is assumed.
- Compositor abstraction. Suede targets Sway's IPC specifically. (The protocol is shared with i3, but output configuration is Sway-specific.)

## Architecture

```
┌────────────┐  REST/SSE  ┌───────────────────────────────┐  IPC (SWAYSOCK)  ┌──────┐
│ API client │◄──────────►│ Suede daemon                  │◄────────────────►│ Sway │
└────────────┘            │  ├ axum HTTP server           │                  └──┬───┘
┌────────────┐            │  ├ reconciler                 │   spawns/monitors   │
│ Web UI     │◄──────────►│  ├ app supervisor             ├──────────► chromium ┘
│ (bundled)  │            │  └ state store (JSON on disk) │            firefox …
└────────────┘            └───────────────────────────────┘
```

### Components

- **HTTP server** (`axum`): serves the versioned JSON API, the SSE event stream, the OpenAPI document, and the bundled reference UI.
- **Sway connection** (`swayipc-async`): one connection subscribed to `output`, `window`, and `workspace` events; short-lived connections for queries (`get_outputs`, `get_tree`) and `run_command`. Note Sway's `output` event carries no detail (`change: "unspecified"`), so it is used purely as a trigger to re-run `get_outputs`. A slow poll (default 5 s, configurable) backstops event delivery.
- **Reconciler**: single task that owns "make live match desired". Triggered (debounced, ~500 ms) by: startup, config writes, output events, and app exits. It computes a diff between observed and desired state and issues the minimal set of Sway commands / process operations.
- **App supervisor**: spawns configured apps as *direct child processes* of Suede (via `tokio::process`), not via Sway `exec`. This gives Suede the real PID for clean termination (SIGTERM, then SIGKILL after a timeout), exit-code observation, and restart policies — no `pkill` pattern matching. Environment (`WAYLAND_DISPLAY`, `XDG_RUNTIME_DIR`) is inherited from the session. Window→app association uses the window's `pid` from Sway's tree, falling back to `app_id` matching.
- **State store**: the desired-state document persisted as JSON on disk. See [State persistence](#state-persistence).

### Startup sequence

1. Locate the Sway socket: `$SWAYSOCK` → scan `$XDG_RUNTIME_DIR/sway-ipc.*` → `sway --get-socketpath`. Retry with backoff until found (Suede may start before Sway).
2. Query Sway version; gate version-dependent features (e.g. `tearing` requires Sway ≥ 1.10).
3. Load the persisted desired state (falling back to the `.bak` copy, then to empty).
4. Subscribe to events, take an initial snapshot of outputs and windows.
5. Run the reconciler: configure outputs, launch enabled apps, hide the cursor.
6. Start the HTTP server.

## API

All endpoints are prefixed `/api/v1`. All payloads are JSON with camelCase property names. The OpenAPI 3.1 document is generated with `utoipa` (schema names in PascalCase, one `operation_id` per handler matching the handler function name) and served at `/api-docs/openapi.json`; interactive documentation is served by Scalar (`utoipa-scalar`) at `/docs`.

Errors use RFC 9457 `application/problem+json`:

```json
{
  "type": "https://suede.gameshow.pro/errors/validation",
  "title": "Validation failed",
  "status": 422,
  "detail": "apps[0].id 'renderer 1' contains whitespace",
  "instance": "/api/v1/config/apps"
}
```

Status conventions: `400` malformed JSON, `404` unknown resource, `409` revision conflict (`If-Match`), `422` schema-valid JSON that fails semantic validation, `503` Sway IPC unavailable.

### Observed state (read-only)

| Method | Path | Description |
|---|---|---|
| `GET` | `/outputs` | All outputs as reported by Sway: name, active, make/model/serial, current mode, full mode list (deduplicated), position, scale, transform. |
| `GET` | `/outputs/{name}` | A single output. |
| `GET` | `/windows` | All windows in the tree: id, app_id, pid, title, geometry, fullscreen state, output, and — where Suede launched them — the owning app id. |
| `GET` | `/audio/outputs` | All audio sinks reported by PipeWire: stable id (`node.name`), human-readable description, availability, and whether it is Suede's null sink. See [Audio routing](#audio-routing). |
| `GET` | `/apps/{id}/status` | Runtime status of a managed app: `running` \| `starting` \| `stopped` \| `crashed` \| `backoff`, pid, start time, restart count, matched window ids. |
| `GET` | `/status` | Overall reconciliation status: `synced` \| `degraded` \| `reconciling`, plus a list of divergences (e.g. "output HDMI-A-3 in desired state but not connected"). |
| `GET` | `/system` | Suede version, Sway version, relevant package versions (sway, chromium, firefox, …), hostname, uptime. |
| `GET` | `/system/checks` | Environment health checks: id, status (`pass` \| `warn` \| `fail`), detail, and whether an automated fix is available. See [Environment preparation](#environment-preparation-and-health-checks). |
| `GET` | `/healthz` | Liveness: 200 when the HTTP server and Sway IPC connection are up. Unversioned, unauthenticated. |

### Desired state (read-write)

| Method | Path | Description |
|---|---|---|
| `GET` | `/config` | The entire desired-state document. |
| `PUT` | `/config` | Replace the entire document. Validated, persisted, then reconciled. |
| `GET/PUT` | `/config/outputs` | The outputs section. |
| `GET/PUT/DELETE` | `/config/outputs/{match}` | A single output config, keyed by its match rule (see below). |
| `GET/PUT` | `/config/apps` | The apps section. |
| `GET/PUT/DELETE` | `/config/apps/{id}` | A single app config. |
| `GET/PUT` | `/config/settings` | Daemon-level settings (cursor hiding, poll interval, …). |

Writes are validated synchronously (schema, mode plausibility, unique app ids) and return `200` with the persisted document once *saved* — not once applied. Application is asynchronous and observable via `/status` and SSE, because reconciliation may take seconds (mode sets) or be currently impossible (output unplugged). An optional `?wait=<seconds>` query parameter blocks the response until reconciliation settles or the timeout elapses, returning the resulting `/status` payload.

Imperative escape hatches (not persisted):

| Method | Path | Description |
|---|---|---|
| `POST` | `/apps/{id}/restart` | Kill and relaunch a managed app. |
| `POST` | `/apps/{id}/heartbeat` | Watchdog heartbeat from the rendered content. Accepted from loopback connections only; no other auth. See [App watchdog](#app-watchdog-heartbeats). |
| `POST` | `/system/checks/{id}/fix` | Run the automated remediation for a failing environment check. |
| `POST` | `/reconcile` | Force an immediate reconciliation pass. |
| `POST` | `/sway/command` | Raw Sway command passthrough (`{"command": "..."}` → Sway's response array). Disabled by default; enable via settings. For debugging only. |

### Events (SSE)

`GET /events` streams named server-sent events, each with a JSON payload:

- `outputs_changed` — payload: full current outputs array (same shape as `GET /outputs`).
- `windows_changed` — payload: the changed window and change type (`new`, `close`, `title`, `move`, `fullscreen_mode`, `floating`).
- `audio_outputs_changed` — payload: full current sink list (same shape as `GET /audio/outputs`).
- `app_status_changed` — payload: same shape as `GET /apps/{id}/status`.
- `checks_changed` — payload: same shape as `GET /system/checks`.
- `config_changed` — payload: the new desired-state document revision number and which section changed.
- `status_changed` — payload: same shape as `GET /status`.
- Heartbeat comment every 15 s to keep intermediaries from timing out the connection.

Streams are state-based rather than replayed: on (re)connect a client should re-fetch current state, then apply events. `Last-Event-ID` is therefore not supported.

### Data model

The desired-state document:

```jsonc
{
  "revision": 42,                    // server-managed, monotonic; used for optimistic concurrency (If-Match)
  "outputs": [
    {
      "match": { "name": "HDMI-A-1" },              // or { "make": "...", "model": "...", "serial": "..." }
      "enable": true,
      "mode": { "width": 1920, "height": 1080, "refreshHz": 60.0 },
      "position": { "x": 0, "y": 0 },
      "scale": 1.0,
      "transform": "normal",                        // normal|90|180|270|flipped|flipped-90|…
      "adaptiveSync": false,
      "allowTearing": false,                        // applied only when Sway ≥ 1.10
      "maxRenderTimeMs": null                       // null = off
    }
  ],
  "apps": [
    {
      "id": "renderer-1",                           // client-chosen, unique, stable
      "enabled": true,
      "launcher": {
        "kind": "chromium-kiosk",                   // chromium-kiosk | firefox-kiosk | exec
        "uri": "http://media-server.local/render/1",
        "showFpsCounter": false,
        "extraArgs": []                             // appended after the preset's arguments
      },
      "output": { "name": "HDMI-A-1" },             // window is moved here and fullscreened
      "fullscreen": true,
      "audio": { "output": "alsa_output.pci-0000_01_00.1.hdmi-stereo" },
                                                    // omit = don't touch routing; "output": null = route to the null sink
      "heartbeat": { "enabled": true, "timeoutSeconds": 25, "startupGraceSeconds": 60 },
      "restart": { "policy": "always", "delayMs": 1000, "maxDelayMs": 30000 }  // exponential backoff
    }
  ],
  "settings": {
    "hideCursor": true,                             // implemented via `seat * hide_cursor 1000` + park off-screen
    "outputPollIntervalSeconds": 5,
    "allowRawSwayCommands": false
  }
}
```

Notes:

- **Output matching.** Matching by connector `name` (`HDMI-A-1`) is the default and suits fixed installations. Matching by EDID (`make`/`model`/`serial`) is supported for cases where connector enumeration is unstable. A desired output with no currently matching connector is a reported divergence, not an error.
- **Layout is the client's job.** Positions are always explicit; Suede does no layout arithmetic (no "row" auto-layout). The reference UI offers left-to-right arrangement as a client-side convenience that simply computes `position` values, and any other client can do the same.
- **URI placeholders.** Launcher URIs may contain the tokens `{appId}` and `{heartbeatUrl}` (a loopback URL to this app's heartbeat endpoint), substituted at launch. This is how rendered content learns where to post its watchdog heartbeats without hard-coding host details.
- **Launcher presets.** `chromium-kiosk` expands to the battle-tested argument set from the prior .NET implementation (`--kiosk --password-store=basic --no-first-run --disable-infobars --disable-session-crashed-bubble --ozone-platform=wayland --force-device-scale-factor=1 --enable-features=VaapiVideoDecoder,… --ignore-gpu-blocklist --enable-zero-copy` etc.) plus the URI. `firefox-kiosk` expands to `--kiosk --new-instance --private-window <uri>`. `exec` is fully generic: `{ "kind": "exec", "command": "...", "args": [...] }`.
- **Multiple Chromium instances.** Chromium refuses a second instance sharing a profile, so Suede automatically assigns each `chromium-kiosk` app a private `--user-data-dir` under its state directory (e.g. `…/suede/profiles/renderer-1`). Profiles are wiped on launch by default (kiosk sessions should be stateless); a `persistProfile: true` flag opts out.
- **Window placement.** After launch, Suede waits for a window whose `pid` matches the spawned process (timeout: 15 s, then the app is `crashed`), moves it to the workspace pinned to the target output, and applies fullscreen. One dedicated workspace per output (`1` on the first output, `2` on the second, …) keeps placement deterministic.

## Daemon configuration

Two kinds of configuration exist, and the split is a hard rule: anything that must be known *before the API can serve* is **bootstrap config**, read once at startup from `$XDG_CONFIG_HOME/suede/suede.toml` (each value overridable by a `SUEDE_*` environment variable, which wins). Everything else is **desired state**, owned by the API and persisted separately (next section).

```toml
# $XDG_CONFIG_HOME/suede/suede.toml — bootstrap settings, read once at startup
bind = "0.0.0.0:9088"        # SUEDE_BIND
# token = "…"                # SUEDE_TOKEN — enables bearer auth and disables the web UI
# state_dir = "…"            # SUEDE_STATE_DIR — default $XDG_STATE_HOME/suede
docs_base_url = "https://suede.gameshow.pro/"   # base for health-check docsUrl links
```

A missing file means all defaults; the daemon never writes this file.

## State persistence

- The desired-state document is the *only* persisted state. Observed state is always re-derived from Sway.
- Location: `$XDG_STATE_HOME/suede/state.json` (i.e. `~/.local/state/suede/state.json` for the session user), overridable via `--state-dir`.
- Writes are atomic: write to a temp file in the same directory, fsync, rename over the target. The previous version is kept as `state.json.bak` and used as a fallback if the primary fails to parse.
- The document carries a `revision` counter and a schema `version`; future schema migrations happen on load.
- On boot, restoration is best-effort by design: the reconciler applies whatever subset of desired state is currently achievable (e.g. only 3 of 4 desired outputs connected → those 3 are configured and their apps launched; the 4th's app is held and launched automatically if the output appears).

## Reconciliation semantics

Each pass:

1. Snapshot observed outputs and windows.
2. **Outputs:** for each desired output with a connected match, diff current vs. desired (mode, position, scale, transform, adaptive sync, tearing, max render time) and issue only the commands for fields that differ. Connected outputs with no desired entry are left untouched by default (`disable` them by adding an entry with `"enable": false`). If any output was enabled/disabled, wait a settle period (~3 s) and re-snapshot before continuing.
3. **Apps:** for each enabled app whose target output is live: ensure the process is running (spawn if not, subject to restart backoff); ensure its window is on the right workspace/output and fullscreened. Kill processes for apps that were removed or disabled, and apps whose target output disappeared.
4. **Cursor:** if `hideCursor`, ensure `seat * hide_cursor` is set and park the pointer at the bottom-left beyond the layout.
5. Publish the resulting divergence list as `/status` and emit `status_changed`.

Failures of individual Sway commands are logged, reflected in `/status`, and retried on the next trigger — a failed pass never aborts the daemon or discards desired state.

## Audio routing

Audio is out of Sway's scope, so Suede talks to **PipeWire** directly (the `pipewire` crate), mirroring the output-like feature set the display side has:

- **Enumeration.** Suede monitors the PipeWire registry for nodes with `media.class == "Audio/Sink"` and exposes them at `GET /audio/outputs`. Registry add/remove events drive the `audio_outputs_changed` SSE event — no polling.
- **Stable identification.** Sinks are identified by PipeWire `node.name` (e.g. `alsa_output.pci-0000_01_00.1.hdmi-stereo-extra1`), which is derived from the hardware path and profile and is stable across reboots and replugging for fixed hardware. (PipeWire's numeric ids and `object.serial` are *not* stable and are never used in the API.) Each sink also carries its human-readable `node.description` and, where derivable from the ALSA device hierarchy, a hint associating an HDMI sink with its video connector.
- **Per-app routing.** Chromium and Firefox are PulseAudio clients (via `pipewire-pulse`), so routing is done at spawn time: Suede sets `PULSE_SINK=<node.name>` in the app's environment — a direct benefit of spawning apps as child processes rather than via Sway `exec`. Changing an app's `audio.output` in the desired state relaunches the app on the next reconcile pass (consistent with the declarative model); live migration of already-playing streams is deferred to v2.
- **Null routing.** At startup Suede ensures a virtual null sink named `suede-null` exists. Apps configured with `"audio": { "output": null }` are launched with `PULSE_SINK=suede-null`: the browser sees a fully functional audio device, but nothing is audible.
- **Divergence handling.** As with displays, a configured sink that is not currently present is a reported divergence, not an error; the app is still launched (audio falls back to the default sink) and relaunched with correct routing when the sink appears.

## App watchdog (heartbeats)

Process liveness alone cannot detect a hung page (Chromium happily keeps running while its content is frozen). Apps may therefore opt into a content-level watchdog:

- The app config carries `heartbeat: { enabled, timeoutSeconds, startupGraceSeconds }` (defaults: 25 s timeout, 60 s startup grace).
- The rendered content is expected to `POST /api/v1/apps/{id}/heartbeat` (empty body) every ~10 seconds, using the `{heartbeatUrl}` URI placeholder to learn the address. The endpoint is deliberately unauthenticated but accepted **only from loopback connections** — the kiosk browsers posting heartbeats always run on the same machine as Suede, and the endpoint is low-risk (worst case, a local process delays a watchdog restart).
- Arming: the watchdog arms on the *first* heartbeat received after launch. Until then only the startup grace period applies (covering page load); if no heartbeat arrives within `startupGraceSeconds`, or an armed app goes silent for `timeoutSeconds`, the process is killed and relaunched with its stored parameters, subject to the app's restart backoff policy.
- Watchdog trips are surfaced as `app_status_changed` events (status `crashed`, reason `heartbeatTimeout`) and counted in `/apps/{id}/status`.

## Environment preparation and health checks

Suede depends on an environment it does not own (Sway session, browsers, PipeWire, systemd units, auto-login). Rather than silently mutating that environment, Suede *verifies* it and offers explicit, user-triggered remediation:

- `GET /system/checks` runs/reports named checks, each `pass` | `warn` | `fail` with a detail message, a `docsUrl` linking to the relevant page of the published documentation site, and a `fixAvailable` flag. Initial check set:
  - Sway IPC socket reachable; Sway version sufficient for configured features (e.g. tearing).
  - Configured browsers *functional*, not merely present: `chromium --version` / `firefox --version` execute successfully, and the flags the presets rely on are accepted (a `--ozone-platform=wayland` dry probe for Chromium). Version is captured for `/system`.
  - PipeWire functional: the registry is reachable and `pipewire-pulse` is offering the Pulse socket (which browser audio routing depends on).
  - Suede systemd user unit installed, enabled, and tied to `sway-session.target`.
  - Sway config includes the Suede-managed block (see below); idle/DPMS configuration won't blank the outputs; no competing desktop environment is active.
  - State directory writable.
- **Failed checks are informative, not just red.** Each carries a human-readable explanation and links to a documentation page describing manual resolution (e.g. the package-install page for a missing browser). Suede deliberately does *not* offer package-installation fixes — that requires root and belongs to provisioning.
- `POST /system/checks/{id}/fix` performs the documented remediation for checks within the session user's power: install/enable Suede's own systemd user unit, and patch the user's Sway config. Config patching is **marker-delimited and idempotent**: Suede owns a `# BEGIN SUEDE_CONFIG` … `# END SUEDE_CONFIG` block (or a dedicated include file referenced by such a block) which it can remove and rewrite without disturbing the rest of the file — the same pattern as the prior project's provisioning script. Fixes that modify files outside Suede's own state directory say so in their check detail before being invoked. No fix ever runs implicitly.
- Root-level appliance provisioning — package install/upgrade, auto-login getty override, disabling competing desktop environments, boot splash — is out of Suede's runtime scope. It is handled by an idempotent `provision.sh` shipped in the package and documented on the site (modeled on the prior project's `upgrade.sh`), run once with sudo at install time and safe to re-run on upgrade.
- The reference UI surfaces failing checks as a banner and offers the fix buttons — first-run setup becomes: run the provisioning script, open the UI, click through the remaining prompts.
- Checks are re-evaluated periodically and on demand; changes emit the `checks_changed` SSE event.

## Security

- Bind address is configurable; default `0.0.0.0:9088` (remote management is the primary use case).
- **Unauthenticated by default.** The expected deployment is a trusted production LAN, and any same-machine web UI would necessarily expose a shared credential to its users anyway (visible in request headers), so a token is not the default posture.
- **Optional static bearer token** (`Authorization: Bearer …`), set via config file or environment variable, for deployments on less-trusted networks. When a token is configured: it is required on every endpoint except `/healthz` and `/apps/{id}/heartbeat`, and **the reference web UI is disabled** — serving a UI that embeds the token would defeat it. Token mode is for machine-to-machine API clients that hold the credential properly.
- `/apps/{id}/heartbeat` is always loopback-only and never requires the token (see [App watchdog](#app-watchdog-heartbeats)).
- No TLS: front with a reverse proxy (Caddy, nginx) where transport security is required.
- `/sway/command` passthrough and `exec`-kind launchers both allow arbitrary process execution as the session user; they are the same trust level as the rest of the API but `allowRawSwayCommands` defaults to off to prevent casual misuse.

## Reference web UI

A single-page application bundled into the binary (via `rust-embed` or equivalent) and served at `/`. It uses only the public API — it is a reference client, exercising every endpoint:

- Live diagram of outputs (position/size to scale), with per-output edit forms (mode picker populated from the observed mode list).
- App list with status badges, start/stop/restart, and launcher editing.
- A raw config editor (JSON, schema-validated client-side) for power users.
- Live updates via the SSE stream; a status banner surfaces divergences from `/status`.

No build-time coupling: the UI is plain TypeScript + a light framework (e.g. Preact or Svelte), compiled to static assets at build time.

## Deployment

- Suede runs as a **systemd user service** in the same session as Sway, e.g. `WantedBy=sway-session.target` (with `sway-session.target` activated from Sway config via `exec systemctl --user start sway-session.target`, per the standard convention). systemd provides daemon restart supervision; the socket-discovery retry loop covers the Sway-not-yet-up window.
- The appliance boot chain (auto-login via agetty override, Sway launch from the login shell, disabling competing desktop environments and screen blanking) is handled by the shipped `provision.sh` (see [Environment preparation](#environment-preparation-and-health-checks)), not by the daemon.
- Logging to journald via `tracing` with `tracing-journald`; log level via `RUST_LOG`.

### Packaging, install, and upgrade

Rust changes the deployment picture in one important way compared to .NET: the build output is a **single self-contained ELF binary** with no runtime to install — its only dynamic dependencies are system libc. "Deploying" is therefore just placing one file plus its service plumbing, which is exactly what a Debian package formalizes.

- **Package contents** (built by `cargo-deb`, which reads a `[package.metadata.deb]` section in `Cargo.toml` — no separate packaging tree to maintain):
  - `/usr/bin/suede` — the binary, web UI embedded.
  - `/usr/lib/systemd/user/suede.service` — the systemd *user* unit (`WantedBy=sway-session.target`).
  - `/usr/share/suede/provision.sh` — the idempotent root provisioning script.
  - `/usr/share/doc/suede/` — default config example, changelog.
  - Declared `Depends: sway, pipewire` and `Recommends: chromium, firefox`, so `apt` pulls the environment in.
- **First install** on a fresh device:
  1. Download the `.deb` for the architecture (`amd64`/`arm64`) from GitHub Releases.
  2. `sudo apt install ./suede_1.2.3-1_arm64.deb` — apt resolves the declared dependencies from the distro's repositories.
  3. `sudo /usr/share/suede/provision.sh` — auto-login, Sway autostart, desktop-environment cleanup.
  4. Reboot; Sway starts, the user unit starts Suede, remaining checks are green-lit through the reference UI.
- **Upgrade**: download the newer `.deb`, `sudo apt install ./suede_1.2.4-1_arm64.deb` again. `dpkg` replaces the files; the package `postinst` runs `systemctl --user daemon-reload` + restart for the logged-in session user. Desired state lives in `$XDG_STATE_HOME`, untouched by package operations, so configuration survives upgrades by construction. Re-running `provision.sh` after upgrade is safe (idempotent) but only needed when the provisioning itself changed.
- **Update visibility**: a `system` health check compares the running version against the latest GitHub release (network permitting) and surfaces "update available" with a link in the reference UI. Actually applying it needs root, so it stays a manual documented step in v1; hosting an apt repository on the GitHub Pages site — enabling plain `apt upgrade` and even `unattended-upgrades` — is on the v2 roadmap.
- **Versioning flow**: `Cargo.toml` `version` is the single source of truth, bumped by PR. CI on `main` tags `v{version}` if absent and creates the GitHub release with both `.deb`s attached and auto-generated notes (the same tag-and-release pattern as BgRaster).

## Tech stack

- **Language/runtime:** Rust, `tokio`.
- **HTTP:** `axum` for REST + SSE.
- **OpenAPI:** `utoipa` for generation; `utoipa-scalar` serving Scalar at `/docs` for local runtime exploration and debugging. A `suede openapi` CLI subcommand prints the same document to stdout without needing Sway or a network — used by CI to build the static API docs.
- **Sway IPC:** a direct implementation of the i3/Sway IPC protocol on `tokio::net::UnixStream` (see [Implementation deviations](#implementation-deviations)).
- **Audio:** PipeWire driven through its own CLI tools, `pw-dump` and `pw-cli`; per-app routing via `PULSE_SINK`.
- **Process supervision:** `tokio::process` with process-group kill semantics.
- **Serialization:** `serde` / `serde_json`.
- **Logging:** `tracing`, `tracing-journald`.
- **Documentation site:** MkDocs with the Material theme, embedding a vendored Scalar bundle for the API reference; published to GitHub Pages by CI.

### Implementation deviations

Three dependency choices changed during implementation. Each preserves a guarantee stated elsewhere in this document — a single self-contained binary whose only dynamic dependency is libc, cross-compilable to aarch64 without a custom sysroot.

1. **`swayipc-async` → a direct protocol implementation.** The crate is built on the smol/`futures-lite` ecosystem, not tokio, so using it inside a tokio/axum service would put two async reactors in one binary. The wire format is a six-byte magic string, a little-endian length, and a little-endian type; the implementation is ~60 lines and gives exact control over reconnection and backoff.
2. **The `pipewire` crate → `pw-dump` / `pw-cli`.** The crate links against `libpipewire-0.3`, which would add a build-time native dependency, break the "only libc" property asserted under [Packaging](#packaging-install-and-upgrade), and require a PipeWire-equipped arm64 sysroot for `cross` builds. The CLI tools provide the same capabilities — enumeration, change notification, null sink creation — with no build dependency. `pw-dump --monitor` is used purely as a change *trigger*, exactly as Sway's detail-free `output` event is, with a one-shot `pw-dump` supplying the authoritative list.
3. **The web UI ships without a build step.** Rather than TypeScript compiled to static assets, it is one self-contained HTML file embedded with `include_str!`. A reference client's value is in being readable and exercising every endpoint; requiring an npm toolchain in CI to ship it is a poor trade. `rust-embed` is unnecessary at this size.

Two behaviours were also refined against the specification once real runs exposed them:

- **Windowless apps.** An `exec` launcher that pins no output is not expected to map a window, so it reaches `running` on spawn and is exempt from the 15-second window timeout. Browser presets, and any app that pins an output, still must produce a window.
- **Halting.** An app whose restart policy declines a relaunch is explicitly halted, not merely left in `crashed`. Without that, the next reconciliation pass would start it again — `never` would have meant "always".

## CI and documentation site

Continuous integration runs on GitHub Actions, following the same layout as the BgRaster project: a single `ci.yml` with path-filter change detection (`dorny/paths-filter`) splitting *app* from *docs* changes, and a `workflow_dispatch` input `docs_only` to redeploy the site without rebuilding the app.

### App jobs

- **Lint & test** (every push/PR): `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, plus the web UI's typecheck/build. Runs on `ubuntu-latest`; no Sway required (Sway-dependent logic is behind a trait so IPC interactions are unit-testable against recorded fixtures).
- **Build & package** (push to `main`): release binaries for `x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu` (via `cross`), with the compiled web UI embedded; `.deb` packages for both architectures via `cargo-deb` (unit file, default config, postinst). Version is sourced from `Cargo.toml`; main-branch builds tag `v{version}` if the tag does not already exist and create/update a GitHub release carrying the `.deb`s (mirroring BgRaster's tag-and-release steps).

### Docs jobs

The documentation site is built with **MkDocs + Material** and deployed to **GitHub Pages** using the standard artifact flow (`actions/upload-pages-artifact` → `actions/deploy-pages`), gated on `main` like BgRaster's `build-docs` / `deploy-docs` jobs.

- **Generated API reference.** The docs build compiles Suede and runs `suede openapi > docs/generated/openapi.json`. A static Scalar page at `docs/api/index.html` renders that document; the Scalar `@scalar/api-reference` bundle is vendored into the docs assets at build time (no CDN dependency at view time). MkDocs copies both through verbatim, and the nav gets an "API Reference" entry pointing at `api/`. This guarantees the published API docs are generated from the exact code at the deployed commit — the same document `utoipa-scalar` serves at runtime.
- **MkDocs configuration** follows the BgRaster conventions: Material theme with auto/light/dark palettes, `pymdownx.superfences` with Mermaid, `content.code.copy` and instant navigation, `docs/generated/**` excluded from nav, and the release version injected via an `!ENV` variable set by the workflow.
- **Content**, mirroring the BgRaster site layout (`docs/index.md` home derived from the README via `pymdownx.snippets`, user pages at the top level, developer pages nested):
  - `index.md` — home; `getting-started.md` — install, `provision.sh`, first run; `configuration.md` — the desired-state document reference; `api/` — Scalar API reference; `troubleshooting.md` — failure modes and their health checks.
  - `developer/architecture.md`, `developer/specification.md` (this document), `developer/coding-standards.md`, `developer/deployment.md`.
  - `generated/**` git-ignored and excluded from nav, exactly as in BgRaster.
- `mkdocs build --strict` so broken links and nav errors fail CI.

## Repository layout

```
Suede/
├── Cargo.toml               # single binary crate; [package.metadata.deb] for packaging
├── src/
│   ├── main.rs              # CLI: `suede run` (default) | `suede openapi` (clap subcommands)
│   ├── api/                 # axum routers, DTOs, SSE hub, utoipa derives
│   ├── model/               # desired-state + observed-state types (serde + utoipa)
│   ├── sway/                # SwayClient trait; swayipc-async impl; fixtures/ for tests
│   ├── audio/               # AudioMonitor trait; PipeWire impl; null-sink management
│   ├── supervisor/          # app spawning, launcher presets, restart/backoff, watchdog
│   ├── reconciler/          # diff + apply passes, divergence reporting
│   ├── state/               # persistence: atomic writes, .bak fallback, schema migration
│   └── checks/              # environment checks and their fix actions
├── ui/                      # web UI source (TypeScript); builds to static assets embedded via rust-embed
├── packaging/               # suede.service, provision.sh, deb maintainer scripts
├── docs/                    # MkDocs site source (BgRaster layout; generated/ git-ignored)
├── mkdocs.yml
└── .github/workflows/ci.yml
```

## Testing strategy

CI has no Sway, no PipeWire, and no display — the architecture accounts for this:

- **All Sway interaction sits behind a `SwayClient` trait** (`get_outputs`, `get_tree`, `run_command`, `subscribe`). The production impl wraps `swayipc-async`; tests use a mock fed by JSON fixtures recorded from a real session (`swaymsg -t get_outputs --raw`, etc.) checked into `src/sway/fixtures/`.
- **The reconciler is pure logic**: given (observed, desired) it returns a plan (ordered list of Sway command strings + process operations) which a separate executor runs. Table-driven tests assert exact plans for scenarios: cold boot, mode change only, output unplugged, output reappears, app added/removed/disabled.
- **Audio behind an `AudioMonitor` trait**, same mock pattern.
- **API tests** drive the axum router in-process (`tower::ServiceExt::oneshot`) against the mocks, including SSE (read the first events off the stream) and auth behavior with/without token.
- **Persistence tests** use temp dirs: round-trip, atomic-rename crash simulation, corrupt-primary-falls-back-to-bak, unknown-schema-version migration.
- **Snapshot test on `suede openapi`** output so every endpoint/DTO change is reviewed in diffs.
- On-device smoke tests (real Sway, real displays, real audio) are a documented manual checklist in `developer/deployment.md`, not CI.

## Implementation phases

The build is split into phases designed to be implemented **strictly in order**; each leaves `main` releasable (CI green: `cargo fmt --check`, `clippy -D warnings`, tests, OpenAPI snapshot). Rules for the implementer:

- Do not widen a phase's scope; where later work is anticipated, leave a `// TODO(phase N):` comment.
- Every new endpoint gets utoipa annotations in the same change — the OpenAPI snapshot test enforces this.
- Every new Sway/PipeWire interaction goes through the trait, never directly, so it stays testable.

### Phase 0 — Scaffold and CI guardrails

**Scope:** Cargo project with the module skeleton above; `clap` CLI with `run` (default) and `openapi` subcommands; bootstrap config loading (`suede.toml` + `SUEDE_*` overrides); `tracing` + `tracing-journald` init; axum server serving `/healthz` and `/api-docs/openapi.json`; `ci.yml` with the lint/test job and OpenAPI snapshot test.
**Acceptance:** CI green from an empty checkout; `suede openapi` prints a valid OpenAPI 3.1 document; `suede run` serves `/healthz` → 200; `SUEDE_BIND` override works.

### Phase 1 — Sway read path (observed state)

**Scope:** socket discovery chain with retry/backoff; `SwayClient` trait + `swayipc-async` impl; version query and feature gating; node→`Output`/`Window` mapping; `GET /outputs`, `/outputs/{name}`, `/windows`, `/system` (versions), SSE `/events` with `outputs_changed`/`windows_changed`; event subscription with poll backstop.
**Acceptance:** fixture-based mapping tests (including mode dedup and `__i3` filtering); on real hardware, hotplugging a display produces `outputs_changed` within the poll interval; SSE reconnect delivers a heartbeat within 15 s.

### Phase 2 — Desired state and output reconciliation

**Scope:** state store (atomic persistence, `.bak` fallback, `revision`, `If-Match` conflict → 409); all `/config*` endpoints with validation (422 semantics); reconciler for **outputs only** — diff to minimal Sway commands, settle delay after enable/disable, divergence list; `GET /status`, `?wait=`, SSE `config_changed`/`status_changed`.
**Acceptance:** reconciler table tests assert exact command strings per scenario; on hardware, a reboot restores output configuration unattended; unplugging a configured output moves `/status` to `degraded` with a named divergence and replug returns it to `synced`.

### Phase 3 — App supervisor

**Scope:** `apps` config section; launcher presets (`chromium-kiosk`, `firefox-kiosk`, `exec`) and per-app `--user-data-dir` with wipe-by-default/`persistProfile`; spawn as child process with inherited session env; window association by pid with `app_id` fallback and 15 s launch timeout; workspace-per-output placement + fullscreen; restart policies with exponential backoff; kill on disable/remove/output-loss; `{appId}` URI placeholder; cursor hiding; `GET /apps/{id}/status`, `POST /apps/{id}/restart`, SSE `app_status_changed`.
**Acceptance:** `kill -9` of a browser triggers relaunch within the backoff window; an app whose output is unplugged is terminated and auto-launched on replug; two `chromium-kiosk` apps run simultaneously on different outputs; disabling an app terminates it within the SIGTERM→SIGKILL timeout.

### Phase 4 — Audio routing

**Scope:** `AudioMonitor` trait + PipeWire impl (sink enumeration keyed by `node.name`, registry change events); `suede-null` sink creation; `PULSE_SINK` injection at spawn; `audio` app-config section with relaunch-on-change; audio divergences in `/status`; `GET /audio/outputs`, SSE `audio_outputs_changed`.
**Acceptance:** sink list matches `wpctl status` on hardware; changing an app's `audio.output` relaunches it routed to the new sink; a null-routed app plays silently; a configured-but-absent sink is a divergence, not a launch failure.

### Phase 5 — Content watchdog

**Scope:** `heartbeat` app-config section; loopback-only unauthenticated `POST /apps/{id}/heartbeat`; `{heartbeatUrl}` placeholder; arming on first heartbeat, `startupGraceSeconds` before it, `timeoutSeconds` after it; kill/relaunch through the existing restart machinery with status reason `heartbeatTimeout`.
**Acceptance:** a test page that stops posting is relaunched within `timeoutSeconds` + restart delay; a page that never posts is relaunched after the startup grace; a non-loopback POST is rejected 403; heartbeats from one app never feed another's watchdog.

### Phase 6 — Environment checks and fixes

**Scope:** the full check set from [Environment preparation](#environment-preparation-and-health-checks) with `docsUrl` links (base URL from bootstrap config); fix actions for the systemd user unit and the marker-delimited Sway config block; periodic + on-demand re-evaluation; `packaging/provision.sh`; package-version capture into `/system`; SSE `checks_changed`.
**Acceptance:** every check has pass and fail unit tests; every fix is idempotent (invoking twice equals once); the Sway config patch preserves all user content outside the markers byte-for-byte; `provision.sh` runs twice cleanly on a fresh Debian VM.

### Phase 7 — Reference web UI

**Scope:** the UI described in [Reference web UI](#reference-web-ui), built in `ui/`, embedded via `rust-embed`, served at `/`; disabled (404 + log line) when a token is configured.
**Acceptance:** every API endpoint is exercisable through the UI; live SSE updates reflect a display hotplug without refresh; the binary builds with embedded assets in CI; token mode serves no UI bytes.

### Phase 8 — Packaging and release

**Scope:** `[package.metadata.deb]` (contents and dependencies per [Packaging](#packaging-install-and-upgrade)); `postinst` reload/restart; `cross` builds for `amd64`/`arm64` in CI; tag-and-release job keyed on `Cargo.toml` version; update-available health check.
**Acceptance:** the `.deb` installs on a fresh Debian and RPi OS image and the service starts on next login; upgrading preserves desired state and running config; CI produces a GitHub release with both `.deb`s when the version bumps.

### Phase 9 — Documentation site

**Scope:** `mkdocs.yml` modeled on BgRaster (Material, palettes, Mermaid, `--strict`); content pages per [Docs jobs](#docs-jobs); `suede openapi` → `docs/generated/openapi.json` + vendored Scalar page; Pages deploy jobs with `docs_only` dispatch; README finalized with badges and site links.
**Acceptance:** `mkdocs build --strict` green in CI; the published API reference matches the deployed commit's `suede openapi` output; `docs_only` dispatch redeploys the site without building the app.

## Implementation status

All ten phases are implemented. Verification is 231 unit and integration tests plus a 27-assertion end-to-end smoke test (`scripts/smoke-test.sh`) that drives the real binary over HTTP — killing a supervised process to prove it relaunches, restarting the daemon to prove configuration survives, and diffing `suede openapi` against the served document.

| Phase | State | Notes |
|---|---|---|
| 0 — Scaffold and CI guardrails | Done | `suede run` / `suede openapi`, bootstrap config, OpenAPI snapshot test |
| 1 — Sway read path | Done | Direct IPC, fixture-backed mapping tests, SSE, poll backstop |
| 2 — Desired state and output reconciliation | Done | Atomic persistence with `.bak`, `If-Match`, `?wait=`, pure planner |
| 3 — App supervisor | Done | Child processes, per-app profiles, placement, backoff, halting |
| 4 — Audio routing | Done | Sink enumeration, `PULSE_SINK` at launch, null sink |
| 5 — Content watchdog | Done | Loopback-only heartbeat, arms on first beat |
| 6 — Environment checks and fixes | Done | Seven checks, two remediations, `provision.sh` |
| 7 — Reference web UI | Done | Single embedded page; disabled in token mode |
| 8 — Packaging and release | Done | `cargo-deb`, `cross` matrix, tag-and-release |
| 9 — Documentation site | Done | MkDocs Material, generated API reference, Pages deploy |

Deferred from v1, and worth deciding before release:

- **Update-available health check.** Specced under [Packaging](#packaging-install-and-upgrade) as a check that compares the running version against the latest GitHub release. Not implemented: it is the only check that would need outbound network access, which is a meaningful change in posture for an appliance on a closed production LAN. Revisit alongside the v2 apt repository, which supersedes it.
- **On-device verification.** Everything above is verified against mocks and a real running daemon, but not against real displays, real browsers, or real PipeWire hardware. The manual checklist in `developer/deployment.md` covers what only hardware can prove.

## Resolved questions

1. **Automatic output layout** — rejected. Layout arithmetic belongs in clients; Suede takes explicit positions only. The reference UI provides row-arrangement as a client-side convenience. (See [Data model](#data-model).)
2. **Audio routing** — in scope for v1, via PipeWire: sink enumeration with change events, stable `node.name` identification, per-app sink selection at launch, and null-routing via a Suede-managed null sink. Re-routing relaunches the app in v1; live migration is on the v2 roadmap. (See [Audio routing](#audio-routing).)
3. **Managing Sway config fragments** — Suede does not silently prepare or mutate the environment. It ships health checks (`GET /system/checks`) with informative, docs-linked failure messages and explicit user-triggered remediations (`POST /system/checks/{id}/fix`) for what the session user can do: its own unit installation and marker-delimited Sway config patching. Root-level provisioning lives in the shipped idempotent `provision.sh`. (See [Environment preparation and health checks](#environment-preparation-and-health-checks).)
4. **Content watchdog** — accepted: per-app opt-in heartbeat posted every ~10 s; 25 s of silence kills and relaunches the app with stored parameters. No key: the endpoint is loopback-only and otherwise unauthenticated. (See [App watchdog](#app-watchdog-heartbeats).)
5. **API authentication** — unauthenticated by default (trusted LAN posture). The optional bearer token is for machine-to-machine deployments; configuring it disables the reference web UI, which could not embed the token without exposing it. (See [Security](#security).)
6. **Packaging and release flow** — `cargo-deb` packages installed via `apt install ./…deb` from GitHub Releases, `Cargo.toml` as the single version source with CI tagging. (See [Packaging, install, and upgrade](#packaging-install-and-upgrade).)

## v2 roadmap

- **Live audio stream migration**: change an app's sink without relaunching, by moving its PipeWire streams via metadata rather than `PULSE_SINK` at spawn.
- **apt repository on the GitHub Pages site**: enables plain `apt upgrade` / `unattended-upgrades` instead of manually fetching `.deb`s from Releases.
- **Per-app PipeWire volume control** through the API (natural companion to routing).
- Revisit `persistProfile` browser-state scenarios (e.g. pre-seeded certificates or credentials for protected content).