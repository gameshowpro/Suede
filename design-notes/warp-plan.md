# Warping: implementation plan

Status: plan, 2026-09-18. Nothing below is built yet. This is the sequence
for adding per-output geometry correction (corner pinning plus movable
centre lines) to the projection pipeline, ordered so the render path runs
and is measured on the reference machines before any API or UI is shaped
around it.

The design it implements is summarised in the first section so this file
stands alone. The rest is phases, each with its exit criteria.

---

## The design in one page

**Mapping.** Each output has ten parameters: four corners, one fraction for
the vertical centre line, one for the horizontal. The corners define a
homography, the exact inverse of off-axis projection onto a flat surface,
so straight lines stay straight and a corrected grid lands evenly spaced.
The centre lines are one-dimensional piecewise-linear remaps of the content
coordinate applied *before* the homography, so each is a straight line that
ends on the picture's edges by construction. Other interpolations later
replace the remap function; the shader does not change.

**Rendering.** Inverse-mapped, per fragment, inside the existing fullscreen
triangle draw in `blend.frag`. Per output pixel: one matrix multiply and
divide, two remap branches, one bilinear canvas fetch, one table fetch, one
multiply-add. The sampler becomes linear; `texelFetch` ignores the filter,
so the identity path stays integer-exact and in parity with the CPU blend.

**Precompute.** Everything that is fixed for a given set of parameters lives
in the per-output table the GPU path already reads, indexed by output pixel
exactly as today. The table builder maps each output pixel through the
warp, evaluates the canvas-space ramp and coverage shortfall at that point,
multiplies in the anti-aliased coverage of the picture border, and stores
zero outside the picture. The shader never computes coverage, never needs
screen-space derivatives, and never branches on ramps.

**Per-frame scalars.** The table stores *shape*, not *level*: per pixel a
16-bit ramp term (gamma applied, border coverage baked in) and an 8-bit
coverage-shortfall factor. The lift level is a push constant. That is what
lets adaptive black lift (phase 4) vary the lift every frame without
touching the table: `a = (1 − L·k)·r`, `b = L·k`, two multiplies.

**Geometry model.** Parameters are resolution-independent: an isotropic
unit in which the canvas is one wide and `1/aspect` high, values allowed
outside that range. The canvas aspect is configured; the render resolution
is chosen by the operator from a recommendation, never derived live from
the parameters (a corner drag must not resize the headless output). Seams
are intersections of the *quads*, evaluated in canvas space so both sides
sum to one even when a seam is skewed. Coverage for black lift counts
raster footprints (the output rectangle through the inverse warp), not
content quads. The current rectangle layout is the special case where every
quad is a pinned rectangle at the canvas density, and it is detected from
the matrix (an integer translation) so those outputs keep the exact path.

**Live updates.** The daemon sends parameter changes to the running slicer
over its stdin as JSON lines; the slicer rebuilds that output's table off
the frame path, swaps table and matrix in for the next frame, and
re-presents the last blended capture so a static page moves immediately. No
process restart per edit.

---

## Phase 0 — research spike

Branch `research/warp`, allowed to be throwaway. The goal is a number on
each reference machine, not a mergeable change.

Scope:

1. `blend.frag` and `gpu.rs`: linear sampler; push constants carry the
   inverse matrix as three `vec4`, the two centre fractions, the slice size
   and a warp flag (stays under the 128-byte guarantee the existing size
   test asserts); `texture()` path beside the existing `texelFetch` path,
   selected by the flag.
2. A table builder that evaluates through the warp, with the border
   coverage baked in. Reuse `pixel_transfer` for the ramp values; only
   *where* it is evaluated changes.
3. Parameters from a file named by an environment variable read by
   `suede slice`, in output pixels, re-read on a short mtime poll so a
   corner can be nudged from a shell while watching the wall. No daemon,
   API or reconciler involvement.
4. The `sync` pattern indexes its shapes at the content coordinate so it
   warps too. Static patterns are not routed through the shader yet; use an
   HTML grid page for the visual checks.
5. Run on the overlapping rigs only, so single-output slicing can wait.

Measure, per machine in `design-notes/test-log.md`'s table, with the flag
off, on with identity parameters, and on with a realistic keystone (corners
moved by a few percent) under both a static grid and a GPU-heavy page:

- `perFrameMs.gpu`, `presentedFps`, `canvasFps`, `captureIntervals`,
  `zeroCopyPresented` from `GET /projection/stats`.
- Table rebuild time for a 1920×1200 output, single-threaded and across the
  worker pool `blend_workers` already sizes.

Exit criteria:

- No measurable frame-rate regression between flag off and flag on with
  identity parameters, on all three machines.
- With keystone applied, presented rate within noise of identity on
  `turing-x86` and `ampere-x86`; on `v3d-arm` a bounded, recorded cost.
- `zeroCopyPresented` still equals `presented`: the warp must not disturb
  direct scanout, and there is no reason it should since the target image
  is unchanged.
- Visual: straight grid lines under keystone; no staircase on a tilted
  picture edge; white pattern even across a deliberately skewed seam.
- Table rebuild under ~50 ms, or a plan to get there (coarser evaluation
  with interpolation, or incremental rebuild of the changed region).

Things to try on the spike because they are cheap here and expensive
later:

- **Render scale.** Point the headless canvas at a smaller mode and launch
  Chromium with a fractional `--force-device-scale-factor` (the launcher
  already pins it to 1) so the page's CSS viewport stays the designed size
  while it rasterises fewer pixels. Confirm the page lays out identically
  and note Chromium's frame rate against canvas area. This decides whether
  render scale is a supported feature or a footgun.
- **naga.** Confirm the GLSL frontend accepts `texture()` with a separate
  sampler and the push-constant layout; it already rejects combined
  samplers. No derivatives are needed since coverage is baked.
- **Fragment atomics.** Check `fragmentStoresAndAtomics` on all three
  devices; phase 4 wants it.

## Phase 1 — production pipeline

Still no geometry-model change and no UI. Parameters stay in output pixels
at this phase and are carried on `SliceSpec`; the reconciler passes
identity for every output. Everything here is on the pipeline side and
independent of how phase 2 shapes the configuration.

1. `projection/warp.rs`, pure and testable like `blend.rs`: the warp type,
   identity, validation, forward map, inverse matrix, the centre-line
   remaps and their inverses, exact-path detection, and the table builder.
   Tests: identity maps pixel centres to themselves; corners map to the
   unit square; a centre line is straight and ends on the edges; forward
   and inverse agree; a rect-pinned quad at canvas density is detected as
   exact.
2. Table format v2: `(ramp16, shortfall8)` packed as today's `u32`, lift
   level in push constants. `Blend::rows` on the CPU path takes the same
   packing with the lift as a scalar; the parity test between the paths
   still holds for identity outputs.
3. Live update channel: the manager pipes the slicer's stdin; the slicer
   reads JSON lines on its event loop; the spawn fingerprint excludes every
   field that can be updated live; a table rebuild runs on a worker and is
   swapped in atomically; presenters are marked stale and re-presented from
   `last_blended_slot`.
4. Static test patterns through the shader on the GPU path: render the
   pattern once at canvas size into a host-visible image and blend from it
   as if it were a capture. That both warps the grid and guarantees a
   pattern experiences exactly what content does.
5. Slicing a single output when its warp is not identity: the planner's
   two-participant floor and the reconciler's slicing decision.
6. CPU path: warp ignored with a logged reason and a divergence, in the
   style of `projection_unavailable`.
7. Stats: warp active per output, last table rebuild time.
8. Docs: a "Warping" section in how-it-works.md covering the mapping, the
   order of operations (ramps and lift in canvas space, warp last), the
   exactness rule, and the CPU-path limitation.

Exit: the spike's numbers reproduced on the production branch;
`cargo test`, the OpenAPI snapshot and the docs link test clean; the
test log gains an entry.

## Phase 2 — geometry model

The canvas-space model in the daemon. This is the largest phase by surface
area but touches only pure code, configuration, and documentation.

1. Schema, with a `schema_version` bump and a lossless migration from the
   rectangle layout: aspect from the bounding box, quads from the
   rectangles, render size from the bounding box at scale one.
   - `projection.canvas`: `aspect`, `renderWidth` (height follows), and
     the operator's chosen scale for the record.
   - Per output, a `geometry` block with the ten parameters in canvas
     units. When present, it supersedes `position` for slicing; `position`
     keeps its meaning on the tiling appliance, where no canvas exists.
   - Reject a mix on one rig: either every sliced output has geometry or
     none does.
2. `blend.rs`: seams from quad intersections, attenuation evaluated at the
   canvas point along the seam's fade axis; coverage from raster
   footprints; contiguity validation over quads. The existing test suite
   is rewritten around quads, keeping every current case as a pinned-quad
   instance.
3. Reconciler: canvas size from configuration rather than the bounding
   box; exact-path detection per output; the slicer spec carries the
   quads and the canvas size.
4. Recommendation endpoint: `GET /projection/recommendation` returns the
   ideal resolution (the densest projector's densest region at one to one,
   rounded to a multiple of eight, height from the aspect), and presets at
   three-quarter and half. Power-of-two or "byte-boundary" sizes buy
   nothing on these drivers: tiling and pitch alignment are the driver's
   business through the DRM modifier, and Chromium's cost is linear in
   area. The one real constraint is the compositor's maximum output size,
   which the reconciler already discovers when the headless output is
   created and should surface in the same response.
5. Simple mode as a derivation, not a second pipeline: an endpoint (or the
   UI, client-side) that turns output modes plus a rectangle layout into
   rect-pinned quads and the matching canvas, so an operator who wants the
   current behaviour gets numbers that the exact-path detector accepts.
   With a render scale below one, one-to-one is gone by definition and the
   UI should say so.
6. OpenAPI snapshot, configuration.md's projection section, how-it-works.md.

Exit: an existing overlapping configuration migrates and produces
byte-identical frames (exact path detected on every output); a keystoned
configuration blends evenly across a skewed seam under the white pattern.

## Phase 3 — UI

1. Geometry editor: the eight boundary nodes, corners free, centre nodes
   constrained to their line, drags pushed uncommitted through the live
   channel with no restart.
2. Simple mode: the current rectangle editor, producing pinned quads.
3. Canvas panel: aspect, recommended resolution, adopt and preset buttons,
   render scale with the one-to-one warning.
4. Test pattern controls unchanged; the grid now shows through the warp.

## Phase 4 — adaptive black lift

Vary the lift level with how dark the picture is, with temporal smoothing,
so bright scenes keep their contrast and dark scenes hide the seam glow.

1. Measurement in the same pass: fragments on a coarse stride (every
   sixteenth pixel in each axis) atomically add their luminance to a
   host-visible counter; the fence wait the frame already does makes the
   read-back a memcpy. One frame of latency, which the smoothing swallows.
   Needs `fragmentStoresAndAtomics`, checked on the spike.
2. Control: an exponential filter with separate rise and fall time
   constants and a per-frame slew limit, in `warp.rs`'s sibling module,
   pure and testable. Output is the lift level pushed next frame.
3. Configuration: `blackLift` grows from a number to
   `{ mode: fixed | adaptive, level, riseMs, fallMs }`, with the plain
   number still accepted.
4. Stats: the current lift level and the measured mean luminance.

Nothing in the table changes: phase 1's split between shape and level is
what makes this phase additive.

## Later

- Bezier or mesh interpolation: a coarse per-output coordinate table,
  built on the CPU, sampled in place of the analytic mapping. Not
  half-float storage at 4K widths.
- Masking, and the raster overshoot onto a neighbour that the footprint
  coverage cannot fully compensate.
- Per-output gamma.
- Resampling in linear light rather than signal space, if the near-unity
  scale ever stops holding.

## Risks

- **naga's GLSL frontend** is the only shader compiler on the build
  machines and has already refused one construct. Checked on the spike.
- **Chromium under a fractional device scale** may lay pages out
  differently from the designed size. Checked on the spike; render scale
  is dropped or documented as a footgun if it does.
- **Table rebuild latency** during a drag. Mitigations exist (coarser
  evaluation, region-limited rebuild) if the spike's number is too high.
- **Text softening** under any non-identity warp is inherent to
  resampling and must be documented alongside the exactness rule.
- **The tiled overlay path** has no canvas and cannot warp; a warped rig
  is an `allow_overlaps` rig, and the docs should say so.
