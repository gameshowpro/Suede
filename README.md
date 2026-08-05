# Suede

[![Build Status](https://img.shields.io/github/actions/workflow/status/gameshowpro/Suede/ci.yml?branch=main&logo=github&style=flat-square)](https://github.com/gameshowpro/Suede/actions)
[![Latest Release](https://img.shields.io/github/v/release/gameshowpro/Suede?logo=github&style=flat-square)](https://github.com/gameshowpro/Suede/releases)
[![License](https://img.shields.io/badge/license-PolyForm%20Small%20Business%201.0.0-blue?style=flat-square)](LICENSE)
[![Sponsor on GitHub](https://img.shields.io/badge/sponsor-GitHub-EA4AAA?style=flat-square&logo=github-sponsors)](https://github.com/sponsors/gameshowpro)
[![Donate via PayPal](https://img.shields.io/badge/Donate-PayPal-blue.svg?style=flat-square&logo=paypal)](https://paypal.me/barjonas)

<!-- md-exclude-start -->
---
<!-- md-exclude-end -->

## Inspiration
Suede is a daemon with a name that is a hilarious pun on the word "Swayed". Media servers, video walls, and scoreboard displays in the AV and broadcast world are still too often driven by a full desktop OS being remote-controlled by hand. The [Sway](https://swaywm.org/) compositor already provides everything an unattended display appliance needs — precise output control and scriptable window management on minimal hardware — but no friendly way to drive it from across the network or to keep its state across reboots. The idea came from a production system built for a television studio, where a Raspberry Pi drove multi-display game graphics through Sway's IPC socket.

## Summary
Suede turns a Linux box running Sway into a remotely manageable display appliance. It exposes a well-documented REST + SSE API (and a bundled reference web UI) for configuring video outputs, routing audio, and launching kiosk-mode browsers — and it persists everything, so the machine boots straight back into its configured state with no operator intervention. Configuration is declarative: you describe the state you want, and Suede's reconciler keeps reality matching it through reboots, display hotplugs, and application crashes.

## Highlights

- **Declarative desired state** - write config through the API; a reconciler continuously drives Sway toward it at boot, after edits, and after hardware events.
- **Full output control** - per-display mode, position, scale, transform, adaptive sync, tearing, and max render time, with live enumeration of connected displays and their EDID identities.
- **Kiosk browser supervision** - one active application at a time, covering every display as a single canvas, with battle-tested kiosk arguments, per-app browser profiles, readiness gating, and crash-restart policies with backoff.
- **Audio routing** - enumerate PipeWire sinks with stable identifiers, route each app's audio to a chosen sink, or null-route it to silence.
- **Content watchdog** - pages can post heartbeats; a frozen page gets its browser killed and relaunched automatically.
- **Environment health checks** - Suede verifies its surroundings (Sway, browsers, PipeWire, its own service) and offers one-click fixes for what it can safely repair, with documentation links for the rest.
- **Live events** - Server-Sent Events stream every change: displays, windows, audio devices, app status, health.
- **Single binary** - one self-contained executable with the web UI embedded; shipped as a `.deb` for x86-64 and Raspberry Pi class aarch64 devices.
- **Documented API** - OpenAPI 3.1 generated from the code, browsable via Scalar both on the device and on the documentation site.

## Supported environments

Suede is new. The table below separates what has actually been run from what
is expected to work but has not been tried, because a requirements list that
quietly mixes the two is worth very little to somebody deciding whether to
deploy this.

| | Verified | Expected to work, but untested |
|---|---|---|
| OS | Ubuntu 26.04 LTS | Debian 12+, Raspberry Pi OS Bookworm+, other systemd Debian-family distributions |
| Architecture | x86-64 | aarch64 — cross-compiled and packaged by CI, but that binary has never been executed on real hardware |
| Compositor | Sway 1.11 | Sway 1.7–1.10. Tearing control is gated on ≥ 1.10, and the gate has never met a version without it |
| GPU | NVIDIA Quadro RTX 6000, proprietary driver — sway needs `--unsupported-gpu` | Intel, AMD, Raspberry Pi VideoCore |
| Displays | 2 × DisplayPort, including an overlapping edge-blended canvas | 3–4 outputs. The projection code is written for up to four and has only ever run on two |
| Audio | PipeWire, WirePlumber and `pipewire-pulse`; HDMI and USB sinks | Onboard analog sinks, Bluetooth |
| Browser | Google Chrome 151 via `chromium-kiosk` | Chromium proper. **`firefox-kiosk` has never been run at all** |
| Install | The `.deb` and `provision.sh` have both been installed and run on real hardware (Ubuntu 26.04 amd64) and, in a container, upgraded, removed and reset repeatedly | The same on Debian or Raspberry Pi OS, and on aarch64 |
| Privileges | Session user, with `audio`, `video` and `render` group membership | — |

Windows and macOS are not supported and will not be: the project is
deliberately Sway-specific.

Two gaps are worth stating on their own. **`firefox-kiosk` is implemented but
unexercised**, and Firefox is separately subject to an autoplay policy Suede
cannot switch off for it, so `chromium-kiosk` is the path to use where sound
matters. **Nothing has ever run on aarch64 or a Raspberry Pi**, despite the
packaging targeting both.

## Quick start

<!-- docs-tabs-start -->
### Option 1 - Install the package

[![Latest Release](https://img.shields.io/github/v/release/gameshowpro/Suede?logo=github&label=latest&style=flat-square)](https://github.com/gameshowpro/Suede/releases)

Download the `.deb` for your architecture (`amd64` or `arm64`) from the [Releases](https://github.com/gameshowpro/Suede/releases) page, then:

```bash
sudo apt install ./suede_*_arm64.deb   # pulls in sway and pipewire; recommends chromium
sudo /usr/share/suede/provision.sh     # one-time: auto-login, Sway autostart, kiosk cleanup
sudo reboot
```

After the reboot the machine logs in, starts Sway, and starts Suede. Open `http://<machine>:9088/` from another computer; the reference UI will walk you through any remaining setup via its health-check prompts.

### Option 2 - Build from source

[![Last commit](https://img.shields.io/github/last-commit/gameshowpro/Suede?logo=github&style=flat-square)](https://github.com/gameshowpro/Suede/commits/main)

```bash
git clone https://github.com/gameshowpro/Suede.git
cd Suede
cargo build --release          # binary at target/release/suede
cargo install cargo-deb && cargo deb   # or build the installable package
```

Suede has no native library dependencies, so a Rust toolchain is all you need.
<!-- docs-tabs-end -->

### Develop without hardware

```bash
cargo run -- run --mock        # in-memory compositor and audio, UI on :9088
scripts/dev-check.sh           # fmt, clippy, tests, end-to-end smoke test
```

`--mock` simulates three displays and two audio sinks, and applies output commands to its simulated state, so reconciliation behaves as it does on real hardware.

## Sample configuration

Four outputs tiled edge to edge as one 7680x1080 canvas, with a single kiosk
Chromium covering the lot and its audio locked to the HDMI sink - the entire
appliance in one API call:

```bash
curl -X PUT http://media-server:9088/api/v1/config -H "Content-Type: application/json" -d '{
  "committed": true,
  "outputs": [
    { "match": { "name": "HDMI-A-1" }, "enable": true, "mode": { "width": 1920, "height": 1080, "refreshHz": 60 }, "position": { "x": 0, "y": 0 } },
    { "match": { "name": "HDMI-A-2" }, "enable": true, "mode": { "width": 1920, "height": 1080, "refreshHz": 60 }, "position": { "x": 1920, "y": 0 } },
    { "match": { "name": "HDMI-A-3" }, "enable": true, "mode": { "width": 1920, "height": 1080, "refreshHz": 60 }, "position": { "x": 3840, "y": 0 } },
    { "match": { "name": "HDMI-A-4" }, "enable": true, "mode": { "width": 1920, "height": 1080, "refreshHz": 60 }, "position": { "x": 5760, "y": 0 } }
  ],
  "apps": [
    { "id": "renderer", "launcher": { "kind": "chromium-kiosk", "uri": "http://control.local/render" }, "audio": { "output": "alsa_output.platform-hdmi-sound.stereo-fallback" } },
    { "id": "standby",  "launcher": { "kind": "chromium-kiosk", "uri": "http://control.local/standby" } }
  ],
  "activeApp": "renderer"
}'
```

One application is active at a time and it always covers the whole canvas;
the rest of the list is a library to switch between, atomically:

```bash
curl -X POST http://media-server:9088/api/v1/apps/standby/activate
```

Overlap the outputs instead of tiling them - `"x": 1760` rather than `1920`,
say - and Suede switches to projection mode automatically: it renders the app
once into a headless canvas and slices it per projector, blending the seams.
See [projection and edge blending](https://suede.gameshow.pro/configuration/#projection-edge-blending).

The configuration is persisted immediately: reboot the machine and it comes back exactly like this, no operator required. Watch it happen live:

```bash
curl -N http://media-server:9088/api/v1/events
```

## Documentation

[Documentation root](https://suede.gameshow.pro/)

- [Getting started](https://suede.gameshow.pro/getting-started/) - installation, provisioning, and first run.
- [Configuration reference](https://suede.gameshow.pro/configuration/) - every section of the desired-state document.
- [API reference](https://suede.gameshow.pro/api/) - the full REST + SSE API, browsable via Scalar.
- [Troubleshooting](https://suede.gameshow.pro/troubleshooting/) - failure modes and the health checks that catch them.

## AI Disclosure
As a project started in 2026, yes, substantial parts of this application were built using AI tools. I could never have achieved the application scope, automation, test framework, and quality of documentation in my spare time without it. Be assured that the design, concept, functional testing, and documentation proof-reading burned many human neuron hours.

Suede is also built to make it easy for your LLM of choice to help you operate it. Point it at this documentation and the OpenAPI definition, describe your display setup, and have it generate the configuration for you.

## Authors

Hi, I'm Hamish Barjonas. I provide custom solutions for the broadcast production, live entertainment, and sports industries. Yes, including game shows. See more details [here](https://www.barjonas.com). As a keen FOSS advocate, I try to keep as much non-customer-specific code open for the wider community as possible, under the Game Show Pro umbrella. If you're in a related industry, I'd love to collaborate! You can contact me [here](https://barjonas.com/#contact).

## License

Suede is **source available**, not open source, and the distinction is worth
stating plainly: the [PolyForm Small Business License 1.0.0](LICENSE) permits
use for the benefit of a company with fewer than 100 people and under
USD 1,000,000 of revenue in the prior tax year. Individuals, hobbyists,
schools, charities and small production companies are inside that and owe
nothing. Larger organisations need a
[commercial licence](COMMERCIAL.md) — same code, nothing withheld.

Forks inherit these terms, because they are derivative works.

"Suede" is a trademark; the licence covers copyright and patents, not the
name. See [TRADEMARKS.md](TRADEMARKS.md) for what that does and does not
allow, which is more permissive than people usually expect.

Contributions are welcome and are asked for under a grant that allows both
licences — see [CONTRIBUTING.md](CONTRIBUTING.md) for why.

Copyright © 2026 Barjonas LLC. Commercial enquiries:
<https://barjonas.com/#contact>

