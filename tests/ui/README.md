# Geometry editor browser checks

The application remains a single embedded HTML page with no frontend build.
These optional acceptance tests use Playwright and Chromium installed outside
the repository:

```sh
npm install --prefix /tmp/suede-ui-tests playwright
PLAYWRIGHT_BROWSERS_PATH=/tmp/suede-ui-tests/browsers \
  /tmp/suede-ui-tests/node_modules/.bin/playwright install chromium
NODE_PATH=/tmp/suede-ui-tests/node_modules \
PLAYWRIGHT_BROWSERS_PATH=/tmp/suede-ui-tests/browsers \
UI_EVIDENCE=/tmp/suede-ui-evidence node tests/ui/warp-editor.cjs
```

Run from the repository root. On hosts unsupported by the current Playwright
release, use a supported test host. The test fixture intercepts requests to
`suede.test`; it never connects to an appliance. `UI_EVIDENCE` is optional and
writes a browser/version/result record and screenshot.

The suite exercises real pointer events and keyboard focus, inverse-projective
center movement, invalid crossings, numeric edits, identity pin/center resets, shared crop/scale edits, automatic arrangement,
coalescing with delayed responses, final state delivery, Save/Revert barriers,
server rejection rollback, pattern preservation, capability fallback and
recovery, stale recommendations, direct resolution edits and focus presets, one
client's edit and revert arriving on another as a `config_changed` event
(fields, dirty state, the diagram, and a focused field held until it blurs),
resynchronization, and daemon restart identity.
It also checks adaptive compensation fields through pattern previews,
Save/Cancel, numeric fixed-mode restoration, and telemetry updates that never
send configuration writes. Stale samples, unavailable measurement, and
calibration pause have distinct status messages.

It uses a deterministic public-API fixture to control races. Rust API/state
checks independently exercise real HTTP handlers, atomic preconditions, and
validation. This suite does not measure the appliance's input-to-presentation
latency or replace GPU/optical acceptance.

## Manual appliance checklist

Use isolated test state/processes as described in the Warp handoff. Record
machine, binary identity, output modes, renderer, source workload, worker count,
and results. Unchecked items are not claimed as passed by the browser suite.

- [ ] On each output, drag all four corners and both ends of both center lines.
      Check the physical picture and compare numeric center fractions.
- [ ] Try crossings, a singular quad, off-raster pins, invalid source bounds,
      and invalid canvas dimensions. Verify the last accepted picture remains.
- [ ] Try keyboard and numeric edits, including keyboard-only focus order,
      touch pointer cancellation, and a narrow/mobile viewport.
- [ ] Leave a static browser source idle, sustain a drag, then release. Verify
      intermediate progress, eventual final geometry, unchanged neighbors,
      and unchanged browser dimensions during pin movement.
- [ ] Save or Cancel during a slow preview. Open two browser clients, edit
      concurrently, disconnect/reconnect, and restart the temporary daemon.
      There is one shared working copy: verify each client's edits, saves and
      reverts appear on the other as they happen, and that a reconnect
      resynchronizes to whatever the server currently holds.
- [ ] Switch every diagnostic pattern and return to content. Save and reload;
      confirm retained geometry and requested mode survive.
- [ ] Force CPU fallback, edit shared crop pixels/content scale and canvas width,
      save/reload, then restore GPU capability. Verify the framing and requested
      Warp intent survive along with retained pins, centers, and footprints.
- [ ] Edit geometry while an automatic recommendation is in flight. Verify stale
      presets cannot apply. Focus width and use all four presets by keyboard
      and touch; check the achieved width ratio and allocation limits. Width
      edits resize the source and Cancel restores it. Repeated width cycles
      preserve normalized crops; aspect edits keep crop pixels/content scale.
- [ ] Measure at least 20 isolated browser-input-to-presentation receipts and
      selected-output table builds, report nearest-rank p95, and distinguish
      compositor receipt from optical visibility. Targets: build <50 ms;
      browser input to receipt <100 ms under light load on the active rig.
- [ ] Perform optical skew/seam, high-contrast quality, representative playback,
      and platform fault checks under their separate rollout plans.

## Packet 7 source and live checks

[`warp-source.html`](warp-source.html) is a separate source page. Serve it on
the appliance, map its window to the configured source canvas, and verify
both its Sway rectangle and DOM dimensions before recording any result.
`?mode=grid&hud=0` selects a static grid without the diagnostic overlay;
`dark`, `bright`, and `moving` are also available. `window.warpSource.sample(label)`
records dimensions, DPR, responsive layout, canvas backing size, and WebGL
drawing-buffer size. `getSamples()` returns up to 2,000 timestamped events;
`setMode(mode)` changes the workload. The moving grid is synthetic content,
not representative video-playback evidence.

Run the fixture's local functional check with the same external Playwright
installation as the editor suite:

```sh
NODE_PATH=/tmp/suede-ui-tests/node_modules \
PLAYWRIGHT_BROWSERS_PATH=/tmp/suede-ui-tests/browsers \
node tests/ui/warp-source.cjs
```

The retained `packet7_private.py` (internal research record, not published)
starts a temporary production daemon, managed Chromium source, and separate
headless Sway on brain. Stage it, the selected candidate as `suede`, and the
source fixture as `warp-source.html` in a fresh `/tmp/suede-packet7-*` directory,
then run `python3 packet7_private.py --root /tmp/suede-packet7-<run>` on brain. It uses
loopback ports 19088 (daemon), 19089 (source), and 19222 (source CDP), checks
that they are unused, and records its own process identities. Stop it by
creating `<root>/stop`; it also expires after 15 minutes. Keep the SSH runner
attached so its cleanup completes. Its `exec` source launcher isolates the
source from unrelated automatic codec-probe browsers; it still exercises
Suede's real child supervision and window placement.

Forward the private daemon and CDP ports to the controller machine, then run:
(Replace the run ID and SHA-256 placeholders with the verified session values.)

The `research/warp/` harness scripts named below are local research tooling and
are not part of this repository; this section records how the live session was
driven, for whoever holds them.

```sh
NODE_PATH=/tmp/suede-ui-tests/node_modules \
PLAYWRIGHT_BROWSERS_PATH=/tmp/suede-ui-tests/browsers \
PACKET7_RUN=suede-packet7-RUNID \
PACKET7_CANDIDATE_SHA256=VERIFIED_64_HEX_DIGITS \
PACKET7_EVIDENCE=/tmp/new-packet7-browser-result.json \
node research/warp/packet7_browser.cjs
python3 research/warp/packet7_analyze.py /tmp/new-packet7-browser-result.json
```

The live harness writes private test configuration. Use only the isolated
session, never forward these ports to an installed service. It verifies the
run ID, candidate hash, and managed source child before any configuration write.
It checks real
accepted HTTP headers, config/control/session correlation, source DOM and
placement, width edits/Cancel, handles, and adaptive transitions. Its source probe
must match a single managed child on the canvas. The analyzer distinguishes
valid evidence, actual-size timing, and the 1920×1200 build target; a nonzero
exit can mean the requested target scope was not measured. Read its JSON.

Private headless receipts and local fixture passes do not close physical-rig,
touch, optical, or manual gates. Keep operator identity, date, workload,
candidate hash, observation, and raw evidence for each manual item in the
Packet 7 record (internal research record, not published). Pi testing remains on hold
until all required brain acceptance, including manual checks, is complete.
