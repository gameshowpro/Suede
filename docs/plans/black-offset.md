# Black lift (edge-blend black-level compensation)

Status: **shipped**. The field is `projection.blackLift`, in both fixed and
adaptive form — see the [Adaptive black lift](../configuration.md#adaptive-black-lift)
section of the Configuration reference for the exact schema, defaults, and
validation bounds, and [Black-level compensation](../how-it-works.md#where-the-blend-runs)
for the transfer arithmetic and the test pattern used to tune it.

Credit where due: the idea of raising the black floor in less-overlapped
regions to match the most-overlapped one, so a dark scene shows no seams
without touching bright content, follows Fly Elise-ng's "Dynamic Black Offset
without masks" technique for projector edge blending. (Suede's implementation
was written independently against that description; no code or assets were
copied from it.)

This page is a design record: what actually shipped, cited against the code
that implements it, and — clearly separated — ideas that were drafted but
never built. It replaces an earlier draft of this page that documented a
`projection.blackOffset` schema (`mode: off|constant|dynamic`, `amount`,
`attackMs`, `releaseMs`). That schema never existed in `src/` or the OpenAPI
snapshot; nothing described under that name here ever shipped under it. If
you have configuration copied from that earlier draft, replace `blackOffset`
with `blackLift` using the schema linked above.

---

## What shipped

### The physical problem

A projector cannot subtract light: displaying digital black still emits some
physical light, so a point covered by more overlapping projectors shows more
black than a point covered by fewer. In an edge-blended installation this
shows up as visibly brighter seams and corners during dark scenes. Ramps
([Where the blend runs](../how-it-works.md#where-the-blend-runs)) fix gain
roll-off for non-black content; they do nothing for the black floor itself,
because there is no gain to roll off at zero signal.

### Spatial distribution: one lift, shared by shortfall

`blackLift` is calibrated as the lift that adds **one projector's worth** of
black. Suede distributes it spatially so the black floor is uniform across
the whole surface: at a point covered by `n` of the layout's `max` most-covered
projectors, each of the `n` covering projectors applies
`lift × (max − n) / n` — implemented as `Coverage::lift` in
[`src/projection/blend.rs:495`](https://github.com/gameshowpro/Suede/blob/main/src/projection/blend.rs).
A point at maximum coverage needs nothing (shortfall zero); a single-covered
point receives the full multiple. This is the same rule for both fixed and
adaptive `blackLift` — only the *level* fed into it differs between the two
modes.

Verified by tests in `src/projection/blend.rs`:

- `non_overlapping_topology_forces_lift_to_zero` — a layout with no
  overlapping projectors gets zero lift everywhere; there is nothing to
  match.
- `a_grid_lifts_every_region_by_its_own_shortfall` — in a 2×2 grid, a
  lone projector, a two-way seam, and the four-way center each receive their
  own correctly proportioned share.
- `total_black_is_even_across_a_grid` — the *total* emitted black, summed
  across projectors, is identical at every coverage level once lift is
  applied.
- `black_lift_applies_outside_seams_only` — the fixed-point `(a, b)`
  transfer-table packing and border-coverage attenuation are exercised
  together with the gamma shaping.

### Fixed mode

`"blackLift": 0.04` or `{"mode":"fixed","level":0.04}` — the level is
constant regardless of content. This is the byte-exact path the numeric form
has always used ([`src/model/black_lift.rs:40-58`](https://github.com/gameshowpro/Suede/blob/main/src/model/black_lift.rs)).

### Adaptive mode: content-luminance-dependent, whole-scene level

`{"mode":"adaptive", "level", "darkThreshold", "brightThreshold", "riseMs",
"fallMs", "slewPerSecond"}` varies the lift *level* fed into the spatial
distribution above with the source canvas's overall darkness, instead of
holding it constant:

- **Measurement.** Mean linear luminance (sRGB-decoded, Rec. 709 weights) is
  sampled on a regular grid of at most 256×256 source-canvas texel centers,
  before ramps, picture borders, or lift — so the controller never measures
  its own output ([`src/projection/adaptive.rs:11-47`](https://github.com/gameshowpro/Suede/blob/main/src/projection/adaptive.rs)).
  This is the GPU path only; CPU rendering or unavailable GPU measurement
  falls back to the configured fixed level and reports the reason.
- **Asynchronous, rate-limited submission.** The measurement pass never
  blocks the render thread. It runs in its own command buffer and fence,
  submitted no more than once every `MEASUREMENT_INTERVAL` (125 ms, roughly
  8 Hz — [`adaptive.rs:27`](https://github.com/gameshowpro/Suede/blob/main/src/projection/adaptive.rs)),
  and is collected only once that fence has signaled
  (`Gpu::submit_luminance_measurement` / `poll_luminance_measurement`,
  [`src/projection/gpu.rs`](https://github.com/gameshowpro/Suede/blob/main/src/projection/gpu.rs)):
  the render loop polls without waiting and simply asks again next capture if
  nothing is ready. `measurementMs` in `control.blackLift` reports that
  pass's submit-to-collection latency, and every collected sample is tagged
  with the capture it was actually taken from
  (`MeasurementSchedule`, [`adaptive.rs:193`](https://github.com/gameshowpro/Suede/blob/main/src/projection/adaptive.rs)).
- **Target.** `target = level × clamp((brightThreshold − luminance) / (brightThreshold − darkThreshold), 0, 1)`
  — full `level` at or below `darkThreshold`, zero at or above
  `brightThreshold`, linear between
  ([`target_for_luminance`, `src/projection/adaptive.rs:151`](https://github.com/gameshowpro/Suede/blob/main/src/projection/adaptive.rs)).
- **Smoothing.** Exponential smoothing toward the target with a separate
  time constant for rising (`riseMs`) and falling (`fallMs`) lift, then a
  slew-rate clamp (`slewPerSecond`), advanced by monotonic elapsed time so a
  paused or stalled controller cannot accumulate a giant step
  (`AdaptiveController::tick`, [`adaptive.rs:363`](https://github.com/gameshowpro/Suede/blob/main/src/projection/adaptive.rs)).
- **Shared across outputs.** One controller drives one logical generation
  shared by every output in the layout, so multiple projectors never show
  different lift levels on the same frame.
- **Static content.** A bounded 20 ms repaint timer (`CONTROLLER_TICK`,
  [`adaptive.rs:16`](https://github.com/gameshowpro/Suede/blob/main/src/projection/adaptive.rs))
  keeps retained static content repainting until the controller settles, with
  no new capture required.
- **Robustness.** A zero-sample or non-finite measurement retains the last
  valid target and reports a stale-measurement status rather than lifting to
  a bogus value. Calibration patterns pause adaptation and use the exact
  configured fixed transfer, so a chart used to tune the level is not itself
  chasing that level.

This mode addresses what the earlier draft of this page called "Phase 3,
time domain": the underlying idea — modulate the shared lift level from
measured scene luminance, with fast-attack/slow-release-style asymmetric
smoothing and multi-output synchronization — is what `blackLift: {mode:
adaptive}` is.

---

## Future ideas (not implemented)

Nothing below exists in `src/`, the public schema, or the OpenAPI snapshot.
These are design sketches kept for reference, not commitments.

### Per-pixel, signal-level-dependent roll-off

**The idea.** The spatial distribution above (and the adaptive controller
that scales it) both apply one lift level uniformly across an output's whole
picture. A genuinely per-pixel scheme would instead roll the lift off to
zero as *that pixel's own* input signal rises, so a bright highlight sitting
in an otherwise-dark, under-covered region is not needlessly lifted along
with the black around it — trading a coarser global control for finer local
contrast preservation.

**Sketch of a mechanism**, kept from the earlier draft for reference:

- A continuous roll-off `g(Y_in)` of local input luminance `Y_in`, holding at
  1.0 below a knee threshold and easing to 0.0 above it (a Hermite
  smoothstep was suggested, purely as one option — this was never
  benchmarked against alternatives).
- Combined with the existing spatial shortfall `b(u, v)` from the section
  above, giving a per-pixel output `V_out = a·V_in + b(u, v)·g(Y_in)`.
- Implemented as a 256×256 2D lookup table (`LUT[signal_in, overlap_weight]`)
  sampled once per channel per fragment, to keep the shader cost to a single
  texture fetch; a per-pixel 256-entry table was also considered and rejected
  outright on memory grounds (hundreds of megabytes per display).

**Why it has not been built.** The shipped adaptive mode already answers the
practical complaint that motivated this — a lifted black floor washing out
a dark scene — by varying the *whole-scene* level instead. Per-pixel
roll-off would only matter for scenes that mix deep black and a bright
highlight in the same frame at the same time, which is a narrower case than
it first appears once whole-scene adaptation is in place. It remains a
plausible follow-up if that narrower case turns out to matter in practice on
a real installation, not a gap in what shipped.
