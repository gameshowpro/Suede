---
title: Screenshots
---

# Screenshots

These are captures of the bundled reference web UI, which the daemon serves at `http://<machine>:9088/`. Select any image to open it at full size.

## Displays {: #displays }

The **Displays** tab on a healthy system: the layout and canvas settings, the test pattern picker, a preview of the canvas with the overlap regions drawn in, the outputs table, the edge blending and frame timing cards, and the per-output settings, shared content selection and Warp pin editor further down.

[![The Displays tab with four connected outputs, the canvas preview, edge blending and frame timing cards, and the Warp geometry editor](assets/screenshots/displays.avif){ width="1852" height="2070" loading=lazy }](assets/screenshots/displays.avif)

Read more: [Projection and edge blending](configuration.md#projection-edge-blending), [Editing geometry in the web UI](configuration.md#geometry-editor), [Test patterns](configuration.md#projection-test-patterns), [Calibrate an overlapping or warped layout](getting-started.md#calibration).

## Displays with nothing connected {: #displays-disconnected }

The same tab with every output unplugged. Warnings at the top explain what is unavailable, the preview shows each output as not connected, and the Warp intent and calibration are retained while Simple mode is in effect.

[![The Displays tab with every output disconnected, showing warning banners and dashed placeholders in the canvas preview](assets/screenshots/displays-disconnected.avif){ width="1853" height="2065" loading=lazy }](assets/screenshots/displays-disconnected.avif)

Read more: [An unplugged display changes nothing else](configuration.md#unplugged), [How warnings reach you](troubleshooting.md#how-warnings-reach-you).

## Apps {: #apps }

The **Apps** tab lists the managed applications and their state. The dialog is open on one of them, showing its launcher, URI, audio sink, content watchdog, readiness gate, the command and environment it will actually be launched with, and its capabilities.

[![The Apps tab with the edit dialog open for a Chromium kiosk application](assets/screenshots/apps.avif){ width="1855" height="2032" loading=lazy }](assets/screenshots/apps.avif)

Read more: [Applications](configuration.md#applications), [Launchers](configuration.md#launchers), [Waiting for a service to be ready](configuration.md#waiting-for-a-service-to-be-ready), [Audio routing](configuration.md#audio-routing).

## AV I/O {: #av-io }

The **AV I/O** tab: audio outputs (PipeWire sinks with their stable identifiers and levels), audio inputs, and video capture inputs.

[![The AV I/O tab listing audio outputs, audio inputs, and video inputs](assets/screenshots/av-io.avif){ width="1849" height="679" loading=lazy }](assets/screenshots/av-io.avif)

Read more: [Audio routing](configuration.md#audio-routing), [Audio goes to the wrong place, or nowhere](troubleshooting.md#audio-goes-to-the-wrong-place-or-nowhere), [A page cannot reach a camera or capture device](troubleshooting.md#a-page-cannot-reach-a-camera-or-capture-device).

## System {: #system }

The **System** tab: facts about the host, the restart and shut down controls, the slicer pipeline chart, and the Chromium, GPU and CPU panels.

[![The System tab with host facts, the slicer pipeline chart, and the Chromium, GPU and CPU panels](assets/screenshots/system.avif){ width="1848" height="2035" loading=lazy }](assets/screenshots/system.avif)

Read more: [Reading frame timing](how-it-works.md#reading-frame-timing), [Reading the GPU and Chromium panels](troubleshooting.md#gpu-and-chromium-panels), [Host power control](configuration.md#host-power).

## Health checks {: #health-checks }

The list of health checks, here all passing. Each one links to its documentation.

[![The health checks list with every check passing](assets/screenshots/health-checks.avif){ width="823" height="1977" loading=lazy }](assets/screenshots/health-checks.avif)

Read more: [How warnings reach you](troubleshooting.md#how-warnings-reach-you), [First run](getting-started.md#first-run).

## Screenshots that need refreshing {: #refresh }

The Displays tab gained the **Exact** mode button, first in the **Exact | Simple | Warp** toggle, and the Exact-only controls behind it. These captures predate that and show the old two-button toggle, so they need retaking:

- `assets/screenshots/displays.avif`
- `assets/screenshots/displays-disconnected.avif`
- `assets/screenshots/details/unplugged-outputs.avif`

`assets/screenshots/details/warp-editor.avif` was checked and does not need retaking: its crop is the Warp and geometry pin editor and does not include the mode toggle. The other captures are on other tabs. No procedure for retaking screenshots is documented in this repository, so this note does not give one.
