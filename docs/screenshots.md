---
title: Screenshots
---

# Screenshots

These are captures of the bundled reference web UI, which the daemon serves at `http://<machine>:9088/`. Select any image to open it at full size. The Displays and System Profiles captures are taken with `scripts/capture-screenshots.sh` at 100% scale and the narrowest width that shows two columns; `scripts/screenshots/README.md` describes how to retake them.

## Displays {: #displays }

The **Displays** tab on a healthy system in Warp mode: the layout and canvas settings, the test pattern picker, a preview of the canvas with a **1:1 verified** chip on each output, the **Exact | Simple | Warp** toggle, the outputs table, the edge blending and frame timing cards, and, for the selected output, its settings, the **Slice from canvas** panel and the Warp pin editor.

[![The Displays tab with four connected, verified outputs, the canvas preview, the mode toggle, edge blending and frame timing cards, and the Slice from canvas panel and Warp geometry editor](assets/screenshots/displays.avif){ width="1856" height="3452" loading=lazy }](assets/screenshots/displays.avif)

Read more: [Projection modes](projection-modes.md), [Projection and edge blending](configuration.md#projection-edge-blending), [The whole-number chip](configuration.md#whole-number-chip), [Editing geometry in the web UI](configuration.md#geometry-editor), [Test patterns](configuration.md#projection-test-patterns), [Calibrate an overlapping or warped layout](getting-started.md#calibration).

## Arrange automatically in Exact {: #arrange-exact }

The **Arrange automatically** dialog in Exact mode. Overlaps are whole pixels, content scale is fixed at 100%, and the canvas is derived from the arrangement.

[![The Arrange outputs dialog in Exact mode, with rows, columns, overlaps in px, a fixed 100% content scale and the output order](assets/screenshots/details/arrange-exact.avif){ width="630" height="616" loading=lazy }](assets/screenshots/details/arrange-exact.avif)

Read more: [Arranging automatically in Exact](projection-modes.md#arrangement-exact).

## Arrange automatically in Simple and Warp {: #arrange-simple }

The same dialog in Simple and Warp. It chooses whether overlap or content scale is kept fixed and whether the arrangement uses all canvas pixels or keeps slices inside the canvas, and overlaps are percentages.

[![The Arrange outputs dialog in Simple and Warp mode, with the Overlap or Scale choice, the canvas coverage choice, rows, columns, content scale and overlaps in percent](assets/screenshots/details/arrange-simple.avif){ width="630" height="596" loading=lazy }](assets/screenshots/details/arrange-simple.avif)

Read more: [Arranging automatically in Simple and Warp](projection-modes.md#arrangement-free), [Grid arrangement](configuration.md#grid-arrangement).

## Displays with nothing connected {: #displays-disconnected }

The same tab with every output unplugged. Warnings at the top explain what is unavailable, the preview shows each output as not connected, and the Warp intent and calibration are retained while Simple mode is in effect.

[![The Displays tab with every output disconnected, showing warning banners and dashed placeholders in the canvas preview](assets/screenshots/displays-disconnected.avif){ width="1856" height="2894" loading=lazy }](assets/screenshots/displays-disconnected.avif)

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

## System Profiles {: #system-profiles }

The **System Profiles** tab with two saved profiles, the sections each one includes, and the buttons to activate, update, duplicate, download and delete it.

[![The System Profiles tab listing two saved profiles, each with the sections it includes and buttons to activate, update, duplicate, download and delete it](assets/screenshots/profiles.avif){ width="920" height="414" loading=lazy }](assets/screenshots/profiles.avif)

Read more: [System profiles](system-profiles.md), [System Profiles API](configuration.md#system-profiles).

## Saving a profile with some apps {: #profile-save }

The **Save new profile** editor with the **Apps** checkbox in its mixed state: ten of the thirteen apps are chosen, and the summary names the first few.

[![The Save new profile editor with the Apps checkbox in its mixed state and the summary "10 of 13 apps"](assets/screenshots/details/profile-save.avif){ width="920" height="323" loading=lazy }](assets/screenshots/details/profile-save.avif)

Read more: [Saving a profile](system-profiles.md#saving), [Choosing apps](system-profiles.md#choosing-apps).

## Choosing apps to save {: #profile-apps-picker }

The app picker that **Choose apps…** opens, with each app's launcher and target, the active app marked, and a live count.

[![The Choose apps to save dialog, listing every app with its launcher and target, three of them unchecked, the active app marked, and "10 of 13 selected"](assets/screenshots/details/profile-apps-picker.avif){ width="510" height="638" loading=lazy }](assets/screenshots/details/profile-apps-picker.avif)

Read more: [Choosing apps](system-profiles.md#choosing-apps).

## Activating a profile {: #profile-activate }

The **Activate** dialog: the sections to apply, with Apps narrowed to two of the profile's three apps, and a device mapping row for each saved display, each matched to a connected display.

[![The Activate dialog for a profile, with the five section checkboxes, the Apps checkbox mixed at "2 of 3 apps", the apps hint, and a device mapping row for each saved display, each matching a connected display](assets/screenshots/details/profile-activate.avif){ width="660" height="966" loading=lazy }](assets/screenshots/details/profile-activate.avif)

Read more: [Activating a profile](system-profiles.md#activating), [Device mapping](system-profiles.md#device-mapping).
