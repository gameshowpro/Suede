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
center movement, invalid crossings, numeric edits, canonical reset/conversion,
coalescing with delayed responses, final state delivery, Save/Revert barriers,
server rejection rollback, pattern preservation, capability fallback and
recovery, stale recommendations, explicit resolution adoption, two-client
conflicts, resynchronization, and daemon restart identity.

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
      Verify explicit conflict handling and export/reload recovery.
- [ ] Switch every diagnostic pattern and return to content. Save and reload;
      confirm retained geometry and requested mode survive.
- [ ] Force CPU fallback, edit simple rectangles, save/reload, restore GPU
      capability, and restore warp. Verify retained calibration survives.
- [ ] Request a recommendation, edit geometry, then try the old preset. Get
      fresh guidance and deliberately adopt a new aspect/resolution. Verify
      only adoption changes browser dimensions/layout and Cancel restores them.
- [ ] Measure at least 20 isolated browser-input-to-presentation receipts and
      selected-output table builds, report nearest-rank p95, and distinguish
      compositor receipt from optical visibility. Targets: build <50 ms;
      browser input to receipt <100 ms under light load on the active rig.
- [ ] Perform optical skew/seam, high-contrast quality, representative playback,
      and platform fault checks under their separate rollout plans.
