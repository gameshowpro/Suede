# Suede performance test log

Suede's projection pipeline runs on very different hardware, and a change that
speeds one machine up can slow another down. The slicer in particular has two
implementations — a CPU one that copies through shared memory and a GPU one
that keeps every pixel in video memory — whose costs sit in completely
different places, so "it got faster" is only ever true of a named machine.

This file is the record of what each reference machine actually measured, kept
so the next change can be judged against all of them rather than against
whichever one was to hand. **Before merging a change that touches the slicer,
the reconciler's output handling, or anything the frame loop calls, re-run the
measurements and compare against the latest entry. A change that improves one
machine and regresses another is not ready**, whatever the headline number
says.

Every figure here is read from `GET /api/v1/projection/stats` or from the
slicer's own ten-second report in the journal. Nothing is estimated.

---

## Reference environments

Short ids are GPU family plus CPU architecture, because those two facts
predict the behaviour better than any label would.

| id | `turing-x86` | `ampere-x86` | `v3d-arm` |
|---|---|---|---|
| **role** | two-projector development rig | four-projector bench, the closest match to the production target | single-board appliance |
| **CPU** | Intel i7-12700, 20 threads | Intel i3-12100, 8 threads | 4× Cortex-A76 (Raspberry Pi 5 Model B rev 1.0) |
| **RAM** | 30 GB | 15 GB | 7 GB |
| **GPU** | Quadro RTX 8000 (Turing) | RTX A1000 (Ampere, 50 W small-form-factor) | VideoCore VII, V3D 7.1.7.0 |
| **driver** | NVIDIA 580.178.04 | NVIDIA 550.163.01 | V3DV Mesa 26.2.0, Vulkan 1.3.354 |
| **OS** | Ubuntu 26.04 LTS, kernel 7.0.0-31 | Debian 13 trixie, kernel 6.12.101 | Debian 13 trixie, kernel 6.18.39-rpi |
| **compositor** | sway 1.11, wlroots 0.19 | sway 1.10.1, wlroots 0.18 | sway 1.10.1, wlroots 0.18 |
| **outputs under test** | 2 × 1080p60 DisplayPort, blended | 4 × 1920×1200 @ 59.95, 2×2 grid | 2 × 1080p60 HDMI, 100 px overlap |
| **canvas** | 3420×1080 (3.7 Mpx) | 3840×2385 (9.2 Mpx) | 3740×1080 (4.0 Mpx) |
| **architecture** | x86_64 | x86_64 | aarch64 (cross-build required) |

What to keep in mind when reading the results:

- **`turing-x86` is the only one with headroom.** It has the smallest canvas
  and the largest GPU, so it reached full output rate even on the CPU path. It
  is therefore the machine most likely to *regress* and least likely to show a
  gain — which is exactly why it has to be measured.
- **`ampere-x86` is the reference for production.** Most outputs, biggest
  canvas, weakest of the two NVIDIA cards.
- **`v3d-arm` runs a different driver family** (Mesa rather than NVIDIA), so it
  catches assumptions quietly baked in against NVIDIA behaviour — it has
  already caught one. Build for it with `cross build --release --target
  aarch64-unknown-linux-gnu`, which needs a container runtime.
- wlroots 0.18 on `ampere-x86` and `v3d-arm` offers no
  `ext-image-copy-capture-v1`; only `turing-x86` has it. All three offer
  `zwlr_screencopy_manager_v1` v3, which is what the slicer uses.

---

## Entry 1 — GPU blend path, head alignment, queue priority

**Date:** 2026-09-15. **Baseline:** release 0.1.7, CPU/shm slicer.
**Candidate:** the Vulkan dmabuf blend (`projection.renderer: auto|cpu|gpu`),
batched sway output commands, the `output-phase` check and its re-align fix,
and Vulkan queue-priority negotiation with `CAP_SYS_NICE` granted by the
package.

Several things moved at once, so the credit is not evenly shared: on
`ampere-x86` the output refresh rates were already matched, and the heads were
re-aligned with a batched enable before the GPU measurement, so the straddle
figure belongs to the alignment and the frame rate to the GPU path.

### `ampere-x86` — four projectors, 3840×2385 canvas

| | before (CPU/shm) | after (GPU/dmabuf) |
|---|---|---|
| canvas fps | 32.8 | 59.95 |
| presented fps | 32.8 | 59.95 |
| captures within one canvas tick | about half (inferred) | 600 / 600 |
| straddles per interval | 75–101 | 0 |
| inter-output offset, mean | 5.5 ms | 0.02 ms |
| inter-output offset, max | 19.0 ms | 0.13 ms |
| head phase spread | 2.2–7.1 ms | 0.02 ms |
| per frame: waiting | 18.8–19.6 ms | 9.7–10.6 ms |
| per frame: snapshot | 3.0 ms | 0.0 ms |
| per frame: blending | 8.6 ms | 6.8 ms (of which GPU fence 6.7) |
| slicer CPU | 1.79 cores | 0.03 cores |
| compositor CPU | 0.61 cores | 0.31 cores |
| GPU utilisation | 38 % | 45–49 % |

Under a deliberately GPU-saturating page (a fullscreen shader):

| | before (CPU) | after, medium priority | after, realtime priority |
|---|---|---|---|
| canvas fps | 19.5–21.4 | 12.0–14.0 | 20.4 |
| per frame: waiting | 37–42 ms | 32–35 ms | 35 ms |
| GPU fence wait | n/a | 39–48 ms | 13–14 ms |

**That middle column is why the capability matters.** Without `CAP_SYS_NICE`
the NVIDIA driver grants only "medium" priority and the GPU path is *worse
under load than the CPU path it replaced*. The package sets the capability in
`postinst`; a machine that installs the binary by hand and skips `setcap` will
behave like the middle column. The slicer's start-up line reports which tier
it was granted.

### `v3d-arm` — two screens overlapping 100 px, 3740×1080 canvas

First pass was the single-output configuration, where no slicer runs at all:
all health checks pass, batched output commands apply and restore cleanly,
`GET /projection/stats` returns `null`, the web UI serves its new controls, and
the aarch64 binary loads with the Vulkan dependency dormant. Second pass added
the overlapping screen.

There is no instrumented 0.1.7 baseline on this machine, so the comparison is
the two renderers on the same build, which is the honest equivalent:

| | CPU/shm | GPU/dmabuf (what `auto` picks) |
|---|---|---|
| canvas fps | 10.5 | 12.6–15.6 |
| per frame: waiting | 73.0 ms | 8.6–10.9 ms |
| per frame: snapshot | 7.2 ms | 0.0 ms |
| per frame: blending | 14.7 ms | 53–70 ms (all GPU fence) |
| straddles | 0 | 0–12 |
| inter-output offset, mean | 4.5 ms | 4.3 ms |
| capture cadence | every 4th tick or worse | every 4th tick or worse |
| queue priority granted | n/a | `default` (no tiers offered) |

The GPU path wins and frees all four cores, so `auto` is right here too. But
**neither path gets near 60 on a canvas this wide**, and the bottleneck moved
rather than vanished: 53–70 ms of GPU fence wait for about 4 Mpx of trivial
shading is far more than the work justifies. Suspected serialisation against
the compositor's own submissions on V3D. Unexplained, and open.

**One general claim did not survive this machine.** The `output-phase`
re-align fix moved its two heads from 7.5 ms apart to 4.5 ms, not to zero.
Aligning heads by mode-setting them in a single commit is NVIDIA behaviour and
does not hold on the Broadcom driver, so the check can keep warning on a
Raspberry Pi after its own fix has run.

### `turing-x86` — not yet measured

Still on release 0.1.7 at the time of this entry. This is the gap, and it is
the machine most at risk: it already reaches full rate on the CPU path with a
3.4 ms blend. **The GPU path has nothing to win here and something to lose** —
a fence wait longer than that would cost latency for no throughput. If it does
regress, `renderer: auto` needs a rule preferring the CPU path when the CPU
path already meets the output rate.

Its 0.1.7 reference, both outputs at 60 Hz:

| | value |
|---|---|
| canvas / presented fps | 59.95 / 59.95 |
| straddles | 0 |
| inter-output offset, mean / max | 0.004 / 0.02 ms |
| per frame | waiting 12.3, snapshot 1.0, blending 3.4 ms |

---

## Regression gates

Read these as "must not get worse than the latest entry without a stated
reason".

1. **`ampere-x86` holds 59.95 fps** captured and presented, every capture in
   the one-tick bucket, straddles 0, offset under 0.1 ms mean.
2. **`ampere-x86` under a GPU-saturating page stays at or above the CPU path's
   20 fps.** This one has broken once already.
3. **`turing-x86` keeps its full rate and its sub-0.01 ms offset**, and its
   per-frame cost does not rise. Latency counts here, not only throughput.
4. **`v3d-arm` stays above the CPU path's 10.5 fps**, and its health checks
   stay green in the single-output configuration.
5. **On every machine, `framesSuperseded`, `stalls` and the log line's
   `buffer reuse` stay at 0.** Any of them going positive means the frame loop
   is dropping frames or racing the compositor.
6. **Every machine still starts the slicer.** On `renderer: auto` the fallback
   from GPU to CPU must stay logged but non-fatal.

## Reproducing a run

1. Build: `cargo build --release`, or `cross build --release --target
   aarch64-unknown-linux-gnu` for the Pi.
2. Install to `/usr/bin/suede`, run `setcap cap_sys_nice+ep /usr/bin/suede`
   (the package does this itself), then restart the service.
3. Wait two stats intervals, twenty seconds, before reading anything: the
   first is short and includes start-up.
4. Read `GET /api/v1/projection/stats` for the numbers, and
   `journalctl _COMM=suede | grep slicer:` for the renderer and queue-priority
   lines. On the Pi the daemon's log is under `_COMM=suede` rather than
   `--user -u suede`.
5. For the load case, activate a GPU-heavy page and repeat.
6. For CPU accounting, read process totals (`/proc/<pid>/stat` fields 14 and
   15) over a fixed interval. The blend workers are created and joined within
   each frame, so per-thread sampling misses them entirely.
