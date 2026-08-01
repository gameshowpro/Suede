# Suede

[![Build Status](https://img.shields.io/github/actions/workflow/status/gameshowpro/Suede/ci.yml?branch=main&logo=github&style=flat-square)](https://github.com/gameshowpro/Suede/actions)
[![Latest Release](https://img.shields.io/github/v/release/gameshowpro/Suede?logo=github&style=flat-square)](https://github.com/gameshowpro/Suede/releases)
[![License](https://img.shields.io/github/license/gameshowpro/Suede?style=flat-square)](https://github.com/gameshowpro/Suede/blob/main/LICENSE)
[![Sponsor on GitHub](https://img.shields.io/badge/sponsor-GitHub-EA4AAA?style=flat-square&logo=github-sponsors)](https://github.com/sponsors/gameshowpro)
[![Donate via PayPal](https://img.shields.io/badge/Donate-PayPal-blue.svg?style=flat-square&logo=paypal)](https://paypal.me/barjonas)

<!-- md-exclude-start -->
---
<!-- md-exclude-end -->

## Inspiration
Suede is a daemon with a name that is a hilarious pun on the word "Swayed". Media servers, video walls, and scoreboard displays in the AV and broadcast world are still too often driven by a full desktop OS being remote-controlled by hand. The [Sway](https://swaywm.org/) compositor already provides everything an unattended display appliance needs — precise output control and scriptable window management on minimal hardware — but no friendly way to drive it from across the network or to keep its state across reboots. Suede grew out of a production system built for a television studio, where a Raspberry Pi drove multi-display game graphics through Sway's IPC socket. This project generalizes that idea into a standalone, well-documented service.

## Summary
Suede turns a Linux box running Sway into a remotely manageable display appliance. It exposes a well-documented REST + SSE API (and a bundled reference web UI) for configuring video outputs, routing audio, and launching kiosk-mode browsers — and it persists everything, so the machine boots straight back into its configured state with no operator intervention. Configuration is declarative: you describe the state you want, and Suede's reconciler keeps reality matching it through reboots, display hotplugs, and application crashes.

## Highlights

- **Declarative desired state** - write config through the API; a reconciler continuously drives Sway toward it at boot, after edits, and after hardware events.
- **Full output control** - per-display mode, position, scale, transform, adaptive sync, tearing, and max render time, with live enumeration of connected displays and their EDID identities.
- **Kiosk browser supervision** - launch Chromium or Firefox per output with battle-tested kiosk arguments, automatic per-instance profiles, fullscreen placement, and crash-restart policies with backoff.
- **Audio routing** - enumerate PipeWire sinks with stable identifiers, route each app's audio to a chosen sink, or null-route it to silence.
- **Content watchdog** - pages can post heartbeats; a frozen page gets its browser killed and relaunched automatically.
- **Environment health checks** - Suede verifies its surroundings (Sway, browsers, PipeWire, its own service) and offers one-click fixes for what it can safely repair, with documentation links for the rest.
- **Live events** - Server-Sent Events stream every change: displays, windows, audio devices, app status, health.
- **Single binary** - one self-contained executable with the web UI embedded; shipped as a `.deb` for x86-64 and Raspberry Pi class aarch64 devices.
- **Documented API** - OpenAPI 3.1 generated from the code, browsable via Scalar both on the device and on the documentation site.

## Supported environments

| Component | Requirement |
|---|---|
| OS | Debian-family Linux (Debian 12+, Raspberry Pi OS) |
| Architecture | x86-64 or aarch64 |
| Compositor | Sway ≥ 1.7 (tearing control requires ≥ 1.10) |
| Audio | PipeWire with `pipewire-pulse` |
| Privileges | Runs as the session user; one-time provisioning requires sudo |

Windows and macOS are not supported - the project is intentionally Sway-specific.

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

Drive four HDMI outputs, each showing its own kiosk Chromium, with all audio following output 1 - the entire appliance in one API call:

```bash
curl -X PUT http://media-server:9088/api/v1/config -H "Content-Type: application/json" -d '{
  "outputs": [
    { "match": { "name": "HDMI-A-1" }, "enable": true, "mode": { "width": 1920, "height": 1080, "refreshHz": 60 }, "position": { "x": 0, "y": 0 } },
    { "match": { "name": "HDMI-A-2" }, "enable": true, "mode": { "width": 1920, "height": 1080, "refreshHz": 60 }, "position": { "x": 1920, "y": 0 } },
    { "match": { "name": "HDMI-A-3" }, "enable": true, "mode": { "width": 1920, "height": 1080, "refreshHz": 60 }, "position": { "x": 3840, "y": 0 } },
    { "match": { "name": "HDMI-A-4" }, "enable": true, "mode": { "width": 1920, "height": 1080, "refreshHz": 60 }, "position": { "x": 5760, "y": 0 } }
  ],
  "apps": [
    { "id": "renderer-1", "enabled": true, "launcher": { "kind": "chromium-kiosk", "uri": "http://control.local/render/1" }, "output": { "name": "HDMI-A-1" }, "audio": { "output": "alsa_output.platform-hdmi-sound.stereo-fallback" } },
    { "id": "renderer-2", "enabled": true, "launcher": { "kind": "chromium-kiosk", "uri": "http://control.local/render/2" }, "output": { "name": "HDMI-A-2" }, "audio": { "output": null } },
    { "id": "renderer-3", "enabled": true, "launcher": { "kind": "chromium-kiosk", "uri": "http://control.local/render/3" }, "output": { "name": "HDMI-A-3" }, "audio": { "output": null } },
    { "id": "renderer-4", "enabled": true, "launcher": { "kind": "chromium-kiosk", "uri": "http://control.local/render/4" }, "output": { "name": "HDMI-A-4" }, "audio": { "output": null } }
  ]
}'
```

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

Hi, I'm Hamish Barjonas. I provide custom solutions for the broadcast production, live entertainment, and sports industries. Yes, including game shows. See more details [here](https://www.barjonas.com). As a keen FOSS advocate, I try to keep as much non-customer-specific code open for the wider community as possible, under the Game Show Pro umbrella. If you're in a related industry, I'd love to collaborate! You can contact me [here](https://barjonas.com/contact).

## License

This project is licensed under the [MIT License](LICENSE).
