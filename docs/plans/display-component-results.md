# Direct presentation component — September 28–29, 2026

The direct Vulkan presenter now runs the real Chrome capture, warp, blend,
and presentation pipeline on [System A](../developer/test-systems.md#system-a). It remains experimental and explicitly
selected at the internal slice boundary. Wayland remains the appliance default.
The [implementation and selection contract](display-integration.md) describes
what is shared and what still needs daemon/session integration.

## Identity and comparison method

- Base commit: `e20a8f5`; staged build: `e20a8f5-direct-component-r2`.
- Binary SHA-256:
  `0b062c02c1d097df2b65d4fbcde33d687b51f6ed7d3204423a2673732a3bcd03`.
  Both performance arms use this binary with `cap_sys_nice=ep`; the renderer
  reports realtime queue priority. The installed `/usr/bin/suede` is unchanged.
- [System A](../developer/test-systems.md#system-a): Quadro RTX 8000, NVIDIA 595.91.07, Linux 7.0.0-34-generic,
  Chrome 151.0.7922.71. Boot ID:
  `39da87be-9c55-4e30-b90d-1aa55487e656`. NVIDIA DRM fbdev remains enabled.
- Four 1920×1080 outputs: Sway reports 59.939 Hz; Vulkan advertises the
  corresponding 59940 mHz modes. Connector IDs: 129, 133, 137, 141.
- Seascape uses the exact HTML saved with the historical baseline, SHA-256
  `73fb187dc11ee3ac2728cababb74a0b593b9fc20827ece0b79c71d7b9023e114`.
  Both arms load the same local file to eliminate a fresh-profile DNS failure.
  Browser inspection verifies title, linked WebGL program, live context,
  visibility, device pixel ratio 1, and exact canvas dimensions before rendering.
  The test explicitly focuses and fullscreens the Seascape window.
- Both arms stop the daemon, launch Chrome with a fresh profile, and run the
  real `slice` command using a saved daemon-generated spec. Wayland keeps the
  normal DRM/headless compositor; direct mode uses a headless-only GLES2 Sway
  compositor on the same GPU. Session management is external test orchestration,
  not a shipped direct-mode launcher.
- Each performance run lasts 110 seconds: 30 seconds of renderer warmup,
  60 seconds of CPU/GPU collection, then normal renderer cleanup. Validation
  is disabled for performance. Original configuration, output modes, application,
  installed binary, and all 17 health checks are verified after each point.
- The collector reads completed JSONL stats from the standalone slicer and
  samples live processes/GPU counters. Saved API metadata is explicitly a
  snapshot. Fresh stats intervals overlap but do not exactly coincide with
  the CPU/GPU sample window. CPU totals include Chrome, Sway, and the slicer;
  the daemon is absent in both arms. CPU percentages use one core as 100%.

These runs form a new matched component comparison. Do not subtract their CPU
figures directly from the older full-appliance baseline, which included the
daemon, used the installed binary, and loaded the network URL/persistent profile.

## Correctness and recovery

Single-output and four-output runs completed with the Khronos validation
library confirmed in process mappings and no reported Vulkan validation errors.
Both used the same source/capture/warp pipeline as the performance tests.
The validation copy has no file capabilities so the loader accepts the layer
path; its medium queue priority and instrumentation make it unsuitable for
performance comparison.

A scheduling defect found during initial bring-up is fixed and regression-tested:
when direct feedback opens the presentation gate, event dispatch returns to the
render loop immediately instead of sleeping for a Wayland event. Initial runs
with a DNS error page or incorrect browser dimensions were rejected and are
not benchmark results.

A 35-second single-head run exited normally in 38.30 seconds. A 35-second
four-head run exited normally in 47.88 seconds. The four-head run reproduced
NVIDIA kernel flip-event warnings at first presentation and one roughly
three-second flip timeout per head during shutdown. This matches the earlier
fbdev-enabled spike. Normal cleanup returned success and the appliance recovered,
but Vulkan validation success does not establish kernel cleanliness or production
readiness. Driver teardown was an unresolved deployment gate at the time of
these runs; the presenter has since gained an NVKMS sub-ownership grant that
removed the warnings and timeouts in standalone probe runs (teardown 13.2 s to
0.9 s), recorded in the
[root cause section](display-baseline-results.md#present-barrier-root-cause).
The full pipeline has not yet been re-measured with it.

Batched presentation still allows different outputs to cross different refresh
boundaries. Per-present feedback is available, and direct device timestamps are
mapped to a calibrated monotonic estimate. This is not proof of physical genlock,
atomic flips, or equal timestamp accuracy between the backends. Source images
and browser readiness were verified; no camera-based physical synchronization
or projector color/geometry acceptance test was performed.

## Performance measurements

All eight completed captures passed workload, mode, backend-label, freshness,
and sample-coverage checks. Each FPS figure uses five fresh stats intervals
covering approximately 50 seconds within its 60-second collection window.
Per-output feedback rates agree with submission rates to within 0.03 FPS.

| Canvas | Arm / repeat | Submitted FPS | CPU, one core | GPU utilization | Straddles |
| --- | --- | ---: | ---: | ---: | ---: |
| 4500×2679 | Wayland 1 | 51.12 | 75.56% | 75.95% | 27 |
| 4500×2679 | Direct 1 | 46.98 | 90.68% | 78.48% | 323 |
| 4500×2679 | Wayland 2 | 56.96 | 70.79% | 82.35% | 60 |
| 4500×2679 | Direct 2 | 44.40 | 90.94% | 76.57% | 413 |
| 3200×1905 | Wayland 1 | 59.89 | 61.16% | 53.92% | 0 |
| 3200×1905 | Direct 1 | 54.79 | 75.95% | 54.45% | 203 |
| 3200×1905 | Wayland 2 | 59.71 | 73.97% | 54.60% | 2 |
| 3200×1905 | Direct 2 | 59.52 | 85.55% | 57.25% | 6 |

**The direct path did not improve throughput or CPU/GPU headroom in this
environment.** At 4500×2679 both repeats were slower and used more CPU than
their Wayland controls. At 3200×1905 one direct repeat approached full rate;
it still used 85.55% of one CPU core and 57.25% GPU utilization, versus its
Wayland control’s 73.97% and 54.60%. The other direct repeat fell to 54.79 FPS.
There are only two repeats per condition; these are observed results, not
a statistical confidence claim or a conclusion about every driver/GPU.

Direct also reported more whole-frame splits between outputs. Near-zero
phase modulo the refresh period does not contradict these straddles: a head
can be a complete refresh late while retaining the same phase. Both timestamp
sources were present in every measured interval, but their accuracy is not
assumed identical. All measured Wayland presentations reported zero-copy
scanout. That counter is not applicable to direct swapchains.

CPU attribution under overload explains why removing compositor presentation
was insufficient: the slicer used 11.31/14.73% of a core in the Wayland
repeats and 32.28/31.59% in the direct repeats. Sway’s cost varied across runs;
it did not consistently fall enough to offset that increase.
See the per-run raw summaries for all process groups. The profiler below
separates the added Vulkan call cost from shared rendering work.

Thermal and clock conditions were recorded, not fixed:

| Run | GPU temperature, mean | Graphics clock, mean | Power, mean |
| --- | ---: | ---: | ---: |
| w4500-r1 | 78.67 °C | 1837 MHz | 243.99 W |
| d4500-r1 | 80.08 °C | 1823 MHz | 248.64 W |
| w4500-r2 | 81.52 °C | 1807 MHz | 255.70 W |
| d4500-r2 | 80.58 °C | 1830 MHz | 248.64 W |
| w3200-r1 | 76.60 °C | 1892 MHz | 211.63 W |
| d3200-r1 | 76.75 °C | 1892 MHz | 215.23 W |
| w3200-r2 | 76.27 °C | 1893 MHz | 210.38 W |
| d3200-r2 | 76.82 °C | 1892 MHz | 217.11 W |

GPU utilization and power are system-level proxies. The slicer’s
`perFrameMs.gpu` measures host fence waiting, not GPU execution duration.
Lower GPU utilization at a lower achieved frame rate is not evidence of
additional rendering headroom.

Keep Wayland as the default and retain direct as an explicit experimental
component. Removing Wayland, enabling automatic direct selection, or claiming
a performance improvement is not supported by these results. Persistent
session integration, driver-clean shutdown, and synchronization acceptance
remain deployment gates.

## Targeted profiling and interruption

The separate `e20a8f5-direct-component-r3` build adds opt-in aggregate wall
and calling-thread CPU profiling without changing rendering behavior. Its
SHA-256 is `256e0754016575aeeeafb03675f54a6f2c78878f44006050db665c67780651ac`.
The eight comparison runs above all used the unchanged r2 binary. Diagnostic
and interruption checks are recorded separately from the performance window.

The 35-second profiling run submitted 1,409 batches. Aggregate measurements:

| Operation | Wall time, total | Calling-thread CPU, total |
| --- | ---: | ---: |
| Acquire images, all four heads | 6.28 ms | 6.20 ms |
| Batched `vkQueuePresentKHR` | 6,272.12 ms | 6,092.67 ms |
| Poll timing, all four heads | 412.91 ms | 414.19 ms |
| Clock calibration | 90.70 ms | 84.26 ms |
| Shutdown | 12,787.08 ms | 654.40 ms |

All CPU samples succeeded with no clock regressions. Wall and CPU clocks are
sampled separately; small differences include instrumentation overhead.
Acquisition averaged 4.46 microseconds per four-head batch. Present averaged
4.45 ms wall / 4.32 ms CPU per batch; excluding each metric’s largest sample
reduces those averages to 4.01 / 3.93 ms. The largest present samples were
620 ms wall and 555 ms CPU. These totals include startup and are diagnostic,
not a replacement for the steady-state comparison above.

The added cost is predominantly inside the Vulkan presentation call. Image
acquisition is not the bottleneck; polling and calibration are much smaller.
This supports investigating NVIDIA WSI/environment behavior before changing
shared capture-buffer ownership or removing its necessary completion waits.
The earlier fbdev-disabled spike removed kernel warnings/timeouts, but a
full-pipeline performance benefit under that setting remains unmeasured.
Do not assume it fixes these throughput or synchronization failures.

A separate run sent SIGTERM to the identified renderer after 17.5 seconds of
playback. The wrapper recorded signal termination (`returncode: -15`) after
30.26 seconds, including driver resource cleanup. The external recovery harness
restored the committed configuration, original output modes, original application,
and installed binary, with all 17 health checks passing. This proves the tested
process-death recovery procedure, not seamless fallback inside the daemon.
Final verification found no active test units or rollback timers.

## Evidence and local validation

Raw files are retained under `research/display-integration/` locally and
`/var/tmp/suede-direct-integration-20260929/` on [System A](../developer/test-systems.md#system-a). This includes test specs,
source checks, backend labels, binary identity, stats, process/GPU samples,
validation mappings, exit status, restoration checks, and kernel diagnostics.
The research directory is intentionally ignored by Git.

Local checks pass: 959 unit tests (12 ignored), 14 integration tests, 12 collector
tests, all-target Clippy with warnings denied, formatting, and the build without
default features. The last build emits an existing release-only dead-code warning;
the default debug Clippy check is clean.

## GSP off and NVKMS sub-ownership — September 29, 2026

[System A](../developer/test-systems.md#system-a)'s current boot (`7acdb7b6-98ff-4a20-8ccb-36843e03f509`) runs
`NVreg_EnableGpuFirmware=0` (`/etc/modprobe.d/nvidia-gsp-off.conf`) with NVIDIA
DRM `fbdev=1` still enabled, the same driver (595.91.07) and kernel
(7.0.0-34-generic) as the comparison above. The direct arm uses a build with
an in-process NVKMS sub-ownership grant, staged as
`suede` in the harness, SHA-256 `223c151061394d7201a4840ac08d255681887d7d093870e091dd16b4c1ca109a`,
`cap_sys_nice=ep`; the installed `/usr/bin/suede` is unchanged. The harness's
own preflight (committed config, installed-binary hash, synced state, output
modes) passed before every point; `before-config.json` was already at
revision 1981, matching the harness's recorded reference, so no refresh was
needed. Method, workload, and Seascape source are otherwise identical to the
comparison above: 110-second runs (30 s warmup, 60 s collection), daemon
stopped in both arms, one swapchain per connector for direct. The eight
points ran serially, labels prefixed `g` (`gw`/`gd`) to avoid colliding with
the existing run directories, alternating arm at each width:
`gw4500-r1, gd4500-r1, gw4500-r2, gd4500-r2, gw3200-r1, gd3200-r1, gw3200-r2,
gd3200-r2`. All eight completed and passed `analyze_component.py`'s
workload/freshness/sample-coverage checks (`performanceEligible: true`). The
appliance ended synced, revision 1981, checks 17 pass / 0 fail.

| Canvas | Config | Arm / repeat | Submitted FPS | CPU, one core | GPU utilization | Straddles |
| --- | --- | --- | ---: | ---: | ---: | ---: |
| 4500×2679 | GSP on (Sep 28–29) | Wayland 1 | 51.12 | 75.56% | 75.95% | 27 |
| 4500×2679 | GSP on (Sep 28–29) | Direct 1 | 46.98 | 90.68% | 78.48% | 323 |
| 4500×2679 | GSP on (Sep 28–29) | Wayland 2 | 56.96 | 70.79% | 82.35% | 60 |
| 4500×2679 | GSP on (Sep 28–29) | Direct 2 | 44.40 | 90.94% | 76.57% | 413 |
| 3200×1905 | GSP on (Sep 28–29) | Wayland 1 | 59.89 | 61.16% | 53.92% | 0 |
| 3200×1905 | GSP on (Sep 28–29) | Direct 1 | 54.79 | 75.95% | 54.45% | 203 |
| 3200×1905 | GSP on (Sep 28–29) | Wayland 2 | 59.71 | 73.97% | 54.60% | 2 |
| 3200×1905 | GSP on (Sep 28–29) | Direct 2 | 59.52 | 85.55% | 57.25% | 6 |
| 4500×2679 | GSP off + sub-ownership | Wayland 1 | 50.67 | 73.89% | 74.58% | 30 |
| 4500×2679 | GSP off + sub-ownership | Direct 1 | 53.21 | 84.36% | 79.00% | 36 |
| 4500×2679 | GSP off + sub-ownership | Wayland 2 | 56.72 | 73.21% | 82.45% | 54 |
| 4500×2679 | GSP off + sub-ownership | Direct 2 | 53.41 | 81.39% | 80.27% | 36 |
| 3200×1905 | GSP off + sub-ownership | Wayland 1 | 59.45 | 64.01% | 55.52% | 7 |
| 3200×1905 | GSP off + sub-ownership | Direct 1 | 59.70 | 83.28% | 57.88% | 1 |
| 3200×1905 | GSP off + sub-ownership | Wayland 2 | 59.49 | 66.24% | 55.40% | 5 |
| 3200×1905 | GSP off + sub-ownership | Direct 2 | 59.80 | 73.33% | 57.23% | 0 |

The GSP-on rows are reproduced unchanged from the table above for reference;
the two sets ran on different boots (GSP-on boot ended 05:31 UTC September 29,
GSP-off boot began immediately after) but the same driver, kernel, and
harness method.

Renderer exit timing (`slice-exit.json`, requested run 110 s) and kernel
diagnostics (`journalctl -k` over each point's own wall-time window, from
first `before-status.json` write to last `restored-outputs.json` write,
covering startup through teardown) for the same 16 points:

| Canvas | Config | Arm / repeat | Renderer exit, beyond 110 s | nvidia-drm flip warnings | Flip timeouts |
| --- | --- | --- | ---: | ---: | ---: |
| 4500×2679 | GSP on | Wayland 1 | 0.08 s | 0 | 0 |
| 4500×2679 | GSP on | Direct 1 | 12.87 s | 4 | 4 |
| 4500×2679 | GSP on | Wayland 2 | 0.07 s | 0 | 0 |
| 4500×2679 | GSP on | Direct 2 | 12.84 s | 4 | 4 |
| 3200×1905 | GSP on | Wayland 1 | 0.05 s | 0 | 0 |
| 3200×1905 | GSP on | Direct 1 | 12.85 s | 4 | 4 |
| 3200×1905 | GSP on | Wayland 2 | 0.05 s | 0 | 0 |
| 3200×1905 | GSP on | Direct 2 | 12.85 s | 4 | 4 |
| 4500×2679 | GSP off + sub-ownership | Wayland 1 | 0.02 s | 0 | 0 |
| 4500×2679 | GSP off + sub-ownership | Direct 1 | 0.68 s | 0 | 0 |
| 4500×2679 | GSP off + sub-ownership | Wayland 2 | 0.05 s | 0 | 0 |
| 4500×2679 | GSP off + sub-ownership | Direct 2 | 0.70 s | 0 | 0 |
| 3200×1905 | GSP off + sub-ownership | Wayland 1 | 0.04 s | 0 | 0 |
| 3200×1905 | GSP off + sub-ownership | Direct 1 | 0.69 s | 0 | 0 |
| 3200×1905 | GSP off + sub-ownership | Wayland 2 | 0.03 s | 0 | 0 |
| 3200×1905 | GSP off + sub-ownership | Direct 2 | 0.70 s | 0 | 0 |

Every GSP-on direct point shows exactly 4 `nvidia-drm` flip-event kernel
warnings and 4 flip timeouts (one per head) and 12.84–12.87 s of extra exit
time; every GSP-on Wayland point shows none, and exits within 0.08 s of the
requested duration. Every GSP-off + sub-ownership point, both arms, shows
zero flip warnings and zero flip timeouts. The remaining kernel-log lines in
each GSP-off window are unrelated: an `apparmor` `DENIED` audit line from a
`who` locale lookup, and one `workqueue: drm_fb_helper_damage_work hogged
CPU for >10000us 7 times` notice (the fbdev console damage worker), neither
of which is an `nvidia-drm` flip warning or timeout. GSP-off direct's exit
still takes longer than Wayland's (0.68–0.70 s versus 0.02–0.05 s), but this
is over two orders of magnitude smaller than the GSP-on direct overhead.

Thermal and clock conditions for the GSP-off + sub-ownership points (compare
against the equivalent GSP-on table above):

| Run | GPU temperature, mean | Graphics clock, mean | Power, mean |
| --- | ---: | ---: | ---: |
| gw4500-r1 | 73.25 °C | 1865 MHz | 237.39 W |
| gd4500-r1 | 79.02 °C | 1830 MHz | 246.45 W |
| gw4500-r2 | 81.03 °C | 1817 MHz | 256.39 W |
| gd4500-r2 | 80.10 °C | 1824 MHz | 245.04 W |
| gw3200-r1 | 76.28 °C | 1894 MHz | 212.43 W |
| gd3200-r1 | 76.52 °C | 1897 MHz | 218.54 W |
| gw3200-r2 | 76.23 °C | 1898 MHz | 211.96 W |
| gd3200-r2 | 76.23 °C | 1896 MHz | 217.43 W |

With GSP off and NVKMS sub-ownership, the two known kernel-level defects
(nvidia-drm flip-event warnings, per-head flip timeouts during teardown) did
not recur in either arm across these two repeats, and direct's renderer-exit
overhead fell from roughly 12.85 s to under 0.7 s. Throughput moved closer
to parity but did not consistently favor either arm: at 4500×2679, direct
led its Wayland control in one repeat (53.21 vs 50.67 FPS) and trailed in
the other (53.41 vs 56.72 FPS); at 3200×1905, direct was marginally ahead in
both repeats (59.70/59.80 vs 59.45/59.49 FPS), a difference too small to
call an improvement given only two repeats. Direct's CPU cost over Wayland
narrowed but did not close (10.47/8.18 percentage points at 4500×2679,
19.27/7.09 points at 3200×1905, versus 15.12/20.15 and 14.79/11.58 points
under GSP on) and straddles fell sharply at 4500×2679 (36/36 versus
323/413) and at 3200×1905 (1/0 versus 203/6), though direct's straddle count
still exceeded its own Wayland control at 4500×2679 (30/54) while falling
below it at 3200×1905. These are two repeats per condition on one boot; they
do not establish a driver- or GPU-independent result, and do not by
themselves change the conclusion that Wayland remains the default and only
production-proven path.

Raw outputs for these sixteen points, plus the analyzer's JSON summaries and
the launch script (`run_matrix_g.py`), are retained under
`research/display-integration/gsp-off/` locally and remain on [System A](../developer/test-systems.md#system-a) under
`/var/tmp/suede-direct-integration-20260929/`.

## Best-versus-best comparison — September 29, 2026

Slice P2b found that a compositor wait-mode change (`__GL_YIELD=USLEEP`, or
`WLR_RENDERER=vulkan` for the headless canvas Sway only) cut Sway CPU
sharply in single, un-repeated runs. This section repeats the GSP-off
matched comparison above with the best wait-mode setting applied to each
arm, to see whether that CPU saving survives a two-repeat comparison and
changes the throughput picture.

**Binary.** Built from the current tree (includes Slice P2a's two-pass,
on-demand-calibration timing-poll throttle) with
`SUEDE_BUILD_ID=v0.1.14-9-ge20a8f5-dirty cargo build --release`, staged as
`suede` in the harness, SHA-256
`09902ae44051081dfb4b0ba2b179b0421f0581f990bc4651b5f4b764c5e78a0d`,
`cap_sys_nice=ep`. The previous GSP-off/sub-ownership binary is kept as
`suede-p1-backup` (SHA-256
`223c151061394d7201a4840ac08d255681887d7d093870e091dd16b4c1ca109a`). No
`--profile-direct`: as P2b's validation noted, `analyze_component.py` cannot
attribute slicer CPU when the binary is named `suede-profile`. The installed
`/usr/bin/suede` is unchanged. Driver (595.91.07, GSP off), kernel
(7.0.0-34-generic), boot, and committed config (revision 1981) are the same
as the GSP-off comparison above.

**Configurations**, both widths, two repeats each, 16 runs total, strictly
serial:

- `W-base`: Wayland arm, tty1 DRM Sway as provisioned, no `__GL_YIELD`.
- `W-usleep`: Wayland arm, DRM Sway with `__GL_YIELD=USLEEP` (the P2b
  temporary `~/.bash_profile` line).
- `D-usleep`: direct arm, headless canvas Sway with
  `--canvas-env __GL_YIELD=USLEEP`.
- `D-vulkan`: direct arm, headless canvas Sway with
  `--canvas-env WLR_RENDERER=vulkan`.

Runs went in rounds of fixed order (`W-base, D-usleep, W-usleep, D-vulkan`),
alternating width each round (4500, 3200, 4500, 3200), so drift spreads
across configurations rather than accumulating in one arm. Before every
Wayland-arm run, `~/.bash_profile`'s state and the live Sway's
`/proc/<pid>/environ` were confirmed for that configuration, restarting
`getty@tty1` and reconnecting the daemon
(`systemctl --user set-environment SWAYSOCK=... WAYLAND_DISPLAY=...` then
`systemctl --user restart suede`, the same recipe `recover.sh` uses) only
when the state actually needed to change; the `__GL_YIELD` line was removed
again immediately after each `W-usleep` run.

All 16 runs exited 0 and every run's own preflight/postflight restored the
committed configuration, output modes, active app, and 17/17 health checks.
`~/.bash_profile` ended byte-identical to its recorded baseline, SHA-256
`552fa6f4ae6f7196b7d709e478e32b2c039c7bc9d60fa27ac83de9bd61fd9834`, and the
live Sway's environment carries no `__GL_YIELD`. Final appliance status:
synced, revision 1981, 17 pass / 0 warn / 0 fail, active app `arena-fx`
running. All 16 analyzer results reported `performanceEligible: true`, five
fresh stats intervals, and no errors or warnings.

Per-run results (analyzer output only):

| Canvas | Config | Repeat | Submitted FPS | Straddles | CPU browser | CPU slicer | CPU sway | CPU total |
| --- | --- | :-: | ---: | ---: | ---: | ---: | ---: | ---: |
| 4500×2679 | W-base | 1 | 57.79 | 36 | 28.88% | 16.31% | 25.94% | 71.13% |
| 4500×2679 | W-base | 2 | 50.17 | 43 | 20.65% | 10.90% | 44.43% | 75.98% |
| 4500×2679 | D-usleep | 1 | 44.88 | 115 | 23.58% | 19.50% | 7.91% | 51.00% |
| 4500×2679 | D-usleep | 2 | 49.09 | 78 | 25.67% | 23.87% | 6.05% | 55.59% |
| 4500×2679 | W-usleep | 1 | 48.80 | 94 | 23.43% | 9.25% | 13.77% | 46.46% |
| 4500×2679 | W-usleep | 2 | 57.20 | 66 | 24.69% | 15.70% | 11.78% | 52.16% |
| 4500×2679 | D-vulkan | 1 | 48.35 | 142 | 26.43% | 24.89% | 4.40% | 55.82% |
| 4500×2679 | D-vulkan | 2 | 47.64 | 146 | 26.55% | 24.69% | 4.39% | 55.73% |
| 3200×1905 | W-base | 1 | 59.87 | 0 | 28.99% | 6.19% | 38.07% | 73.23% |
| 3200×1905 | W-base | 2 | 59.39 | 15 | 27.36% | 6.93% | 29.20% | 63.49% |
| 3200×1905 | D-usleep | 1 | 59.80 | 2 | 24.31% | 11.51% | 6.74% | 42.61% |
| 3200×1905 | D-usleep | 2 | 59.62 | 6 | 23.85% | 11.54% | 7.46% | 42.91% |
| 3200×1905 | W-usleep | 1 | 59.86 | 0 | 25.46% | 5.44% | 13.27% | 44.24% |
| 3200×1905 | W-usleep | 2 | 59.28 | 7 | 25.39% | 4.07% | 14.37% | 43.83% |
| 3200×1905 | D-vulkan | 1 | 57.04 | 67 | 25.54% | 12.11% | 4.56% | 42.22% |
| 3200×1905 | D-vulkan | 2 | 57.02 | 69 | 26.29% | 11.93% | 4.23% | 42.46% |

All 16 renderer processes exited with `returncode: 0` (`slice-exit.json`);
none needed a rollback beyond the harness's own restore step.

Per-configuration means (average of the two repeats):

| Canvas | Config | Mean FPS | Mean straddles | Mean CPU browser | Mean CPU slicer | Mean CPU sway | Mean CPU total |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 4500×2679 | W-base | 53.98 | 39.5 | 24.77% | 13.60% | 35.18% | 73.55% |
| 4500×2679 | D-usleep | 46.99 | 96.5 | 24.63% | 21.69% | 6.98% | 53.29% |
| 4500×2679 | W-usleep | 53.00 | 80.0 | 24.06% | 12.47% | 12.78% | 49.31% |
| 4500×2679 | D-vulkan | 47.99 | 144.0 | 26.49% | 24.79% | 4.40% | 55.77% |
| 3200×1905 | W-base | 59.63 | 7.5 | 28.18% | 6.56% | 33.63% | 68.36% |
| 3200×1905 | D-usleep | 59.71 | 4.0 | 24.08% | 11.52% | 7.10% | 42.76% |
| 3200×1905 | W-usleep | 59.57 | 3.5 | 25.42% | 4.75% | 13.82% | 44.03% |
| 3200×1905 | D-vulkan | 57.03 | 68.0 | 25.92% | 12.02% | 4.40% | 42.34% |

**Cross-check.** Alongside every run, `capture_p2b.py` sampled per-thread
`/proc/<pid>/task/*/stat` for the whole Sway and slicer processes over the
run's T+40–60 s window (the same method P1 and P2b used), independent of
the analyzer's stats-based CPU attribution. Per-configuration means from
that independent sample (`cpu_from_samples.py`) agree with the analyzer's
`sway`/`slicer` columns above to within a few percentage points, which is
expected given the two methods cover overlapping but not identical windows:

| Canvas | Config | Cross-check sway | Cross-check slicer |
| --- | --- | ---: | ---: |
| 4500×2679 | W-base | 33.26% | 14.82% |
| 4500×2679 | D-usleep | 7.00% | 22.11% |
| 4500×2679 | W-usleep | 12.87% | 13.55% |
| 4500×2679 | D-vulkan | 4.21% | 24.29% |
| 3200×1905 | W-base | 33.37% | 6.71% |
| 3200×1905 | D-usleep | 6.84% | 11.32% |
| 3200×1905 | W-usleep | 13.66% | 4.69% |
| 3200×1905 | D-vulkan | 4.56% | 11.98% |

**What two repeats support.** Browser CPU is stable across every
configuration and arm (20.7–29.0%), as expected for an unchanged workload;
the differences below come from Sway and the slicer.

`__GL_YIELD=USLEEP` lowers Sway CPU in the real workload in both arms, not
just in P2b's single-run probes: at 3200×1905 it takes Wayland's Sway cost
from a 29.2–38.1% range down to 13.3–14.4% (`W-usleep` vs `W-base`), and at
4500×2679 from 25.9–44.4% down to 11.8–13.8%, each without a corresponding
FPS loss at 3200×1905 and with FPS within run-to-run noise at 4500×2679
(53.0 mean vs 54.0 mean). `W-usleep` is therefore at least as good as
`W-base` at both widths and is the better of the two Wayland configurations
measured here.

Comparing the two direct configurations, `D-usleep` has clearly fewer
straddles than `D-vulkan` at both widths (78/115 vs 142/146 at 4500×2679;
2/6 vs 67/69 at 3200×1905) and lower or comparable total CPU (53.3% vs
55.8% at 4500×2679; 42.8% vs 42.3% at 3200×1905, a difference too small to
separate from run-to-run variation given two repeats). `D-usleep` is the
better of the two direct configurations measured here.

Comparing the best of each arm (`W-usleep` vs `D-usleep`): at 3200×1905 the
two are close enough that two repeats do not distinguish them (FPS
59.57 vs 59.71, CPU 44.03% vs 42.76%, straddles 3.5 vs 4.0 — each
difference is within the spread between the two repeats of either
configuration). At 4500×2679, `W-usleep` is ahead on every measured axis:
higher FPS (53.00 vs 46.99), fewer straddles (80.0 vs 96.5), and lower CPU
(49.31% vs 53.29%). Neither width shows direct pulling ahead of Wayland once
each arm is given its own best wait-mode setting; the wait-mode change closes
most of the CPU gap this plan set out to find, but it does not do so by
making direct faster than Wayland, and at 4500×2679 direct still trails
noticeably on both frame rate and straddles.

This does not change the default: Wayland remains the default, and direct
remains an experimental, explicitly-selected component (per the D1 design).
Tuning the compositor's wait mode was the last squeeze identified by this
plan's profiling (Slices P1/P2a/P2b); it narrows the CPU difference without
closing the throughput/straddle gap, so the direct path still needs a new
idea or a sync board, not further wait-mode tuning, before it could be
considered for anything beyond an opt-in experiment. These are two repeats
per condition on one boot; they do not establish a driver- or
GPU-independent result.

Raw outputs for these sixteen points (controller and capture logs,
per-process CPU samples, analyzer JSON, `run_matrix_p3.py`,
`toggle_bash_profile.py`) are retained under
`research/display-integration/p3/` locally and remain on [System A](../developer/test-systems.md#system-a) under
`/var/tmp/suede-direct-integration-20260929/`.

### Attribution check — Slice P4, September 29, 2026

P3's direct 4500×2679 numbers (44.9–49.1 fps) were lower than the earlier
GSP-off direct runs on the pre-P2a binary (`gd4500-r1`/`gd4500-r2`: 53.21/53.41
fps, 36/36 straddles). This check isolates whether Slice P2a's binary change
(the two-pass, on-demand-calibration timing-poll throttle) or the
`__GL_YIELD=USLEEP` wait-mode setting explains the regression, by running the
same direct/USLEEP configuration at 4500×2679 on both binaries, plus a
matching `W-usleep` point, six runs total, two repeats, strictly serial. The
staged `suede` file was swapped (copy + `setcap cap_sys_nice+ep`) before each
direct run: `suede-p1-backup` (pre-P2a, SHA-256
`223c151061394d7201a4840ac08d255681887d7d093870e091dd16b4c1ca109a`) for
`D-usleep-old`, and the current build (SHA-256
`09902ae44051081dfb4b0ba2b179b0421f0581f990bc4651b5f4b764c5e78a0d`, kept as
`suede-p3-new-backup`) for `D-usleep-new`; the matrix ends with the current
build staged. All 6 runs exited 0, restored 17/17 each time, and the
analyzer reported `performanceEligible: true` with 5 fresh stats intervals
and no errors. Final state: staged `suede` SHA-256 `09902ae4...` (matches
the current build), `~/.bash_profile` SHA-256
`552fa6f4ae6f7196b7d709e478e32b2c039c7bc9d60fa27ac83de9bd61fd9834`, appliance
synced, revision 1981, 17 pass / 0 warn / 0 fail.

| Config | Repeat | Binary | Submitted FPS | Straddles | CPU browser | CPU slicer | CPU sway | CPU total |
| --- | :-: | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| D-usleep-old | 1 | pre-P2a (`223c1510`) | 52.29 | 85 | 23.47% | 19.01% | 8.22% | 50.78% |
| D-usleep-new | 1 | current (`09902ae4`) | 45.06 | 124 | 23.06% | 18.92% | 8.22% | 50.20% |
| W-usleep | 1 | current (`09902ae4`) | 49.18 | 99 | 23.82% | 8.63% | 13.84% | 46.29% |
| D-usleep-old | 2 | pre-P2a (`223c1510`) | 52.07 | 67 | 23.62% | 18.67% | 8.40% | 50.70% |
| D-usleep-new | 2 | current (`09902ae4`) | 48.00 | 77 | 24.35% | 21.20% | 7.24% | 52.82% |
| W-usleep | 2 | current (`09902ae4`) | 49.69 | 78 | 23.55% | 8.89% | 13.83% | 46.27% |

With `__GL_YIELD=USLEEP` held fixed on the canvas Sway, the pre-P2a build
ran 52.29/52.07 fps (85/67 straddles) against the current (P2a) build's
45.06/48.00 fps (124/77 straddles) — lower throughput and more straddles on
every repeat. Slicer CPU is not lower on the P2a build: 18.84% mean
(pre-P2a) versus 20.06% mean (current), and total CPU is likewise not lower
(50.74% mean versus 51.51% mean). The P2a timing-poll throttle was written
to reduce polling overhead, but at this canvas size and wait-mode setting it
cost throughput and straddles without any measurable CPU saving to show for
it — the opposite of its intent. Because a same-binary regression with no
compensating benefit is a straightforward case for reverting rather than
further tuning, Slice P2a's poll throttle is being reverted.

The pre-P2a direct build with USLEEP (52.18 fps mean, 76.0 straddles mean,
50.74% CPU mean) is within the run-to-run spread of this matrix's own
`W-usleep` point (49.435 fps mean, 88.5 straddles mean, 46.28% CPU mean):
slightly more frames (+2.75) and fewer straddles (−12.5) than `W-usleep`,
at about 4.5 percentage points more total CPU. That is closer to parity
than P3's comparison (which used the P2a-throttled build) showed, though
still not a case for switching the default — it is two repeats on one boot,
and direct still costs more CPU for a comparable frame rate.

Raw outputs for these six points are retained under
`research/display-integration/p4/` locally and remain on [System A](../developer/test-systems.md#system-a) under
`/var/tmp/suede-direct-integration-20260929/`.

## [System B](../developer/test-systems.md#system-b): unsupported-driver fallback (driver 550) — September 30, 2026

[System B](../developer/test-systems.md#system-b) runs NVIDIA 550.163.01
(proprietary module, Debian 13, kernel 6.12.101) with three 1920×1200
projectors on DP-5, DP-7 and DP-8 (DP-6 is configured but disconnected, so
its baseline is `degraded` with one `output_not_connected` divergence). The
test build was `v0.1.14-9-ge20a8f5-dirty` (`/usr/bin/suede` SHA-256
`62ca9753…`), installed from a local `cargo deb` package over the CI build
(`eae12d47…`), with the login profile re-rendered by `provision.sh`
(additions only: the `PRESENTATION_EOF` and `GL_YIELD_EOF` blocks; the sway
configuration, getty override, `sway-session.target`, `suede.toml`, groups
and default target were byte-identical afterward).

### Driver reporting on 550

Driver 550 prints no `GPU Firmware:` line in
`/proc/driver/nvidia/gpus/*/information` (`EnableGpuFirmware: 18` in
`params`), so `GspFirmware::Unknown` was added and `/system` reports:

```json
{"version": "550.163.01", "kernelModule": "proprietary",
 "gspFirmware": "unknown", "gspOptional": true, "newestTested": "595.91.07"}
```

`gsp-firmware` passes ("driver does not report GSP state (driver
550.163.01)") and `nvidia-driver-version` warns ("NVIDIA driver 550.163.01
is older than the newest tested release 595.91.07; behavior on older
drivers is not validated (per-present timing needs 595 or later)"). On the
Wayland session the appliance reported 18 pass, 1 warn (that check), 0 fail,
with `arena-fx` running and all three projectors at 1920×1200@59.95.

### Fallback sequence

`presentation = "direct"` was appended to `suede.toml`, then
`getty@tty1` and `suede` were restarted:

| Time | Event |
| --- | --- |
| 03:31:51.08 | `getty@tty1` restarted; the DRM Sway ends, projectors go dark |
| 03:31:51.35 | headless-only Sway up (`WLR_BACKENDS=headless`, `WLR_HEADLESS_OUTPUTS=1`), `session = direct`, `direct-attempts = 1` |
| 03:31:51.78 | `systemctl --user restart suede` |
| 03:32:07.03 | the old Wayland daemon is killed after its 15 s stop timeout (pre-existing: open event streams hold its SIGTERM path) |
| 03:32:07.08 | new daemon: presentation resolved, preflight passes, "presenting directly (experimental)" |
| 03:32:07.10 | direct slicer started |
| 03:32:07.17 | slicer fails: "renderer gpu was forced but is unavailable: Gpu::new: no Vulkan GPU matches primary DRM card 226:1 and render node Some((226, 128))" |
| 03:32:07.62 | slicer exit reaped; "direct presentation failed; falling back to wayland for the rest of this boot"; marker written |
| 03:32:07.66 | `display-reset` ran; headless Sway ended |
| 03:32:07.93 | auto-login's DRM Sway up (`session = wayland`) |
| 03:32:08.49 | systemd restarts the daemon (NRestarts 1); it resolves wayland from the marker |
| 03:32:09.40 | `arena-fx` running; all three projectors active at 1920×1200@59.95 |

The dark gap was 16.9 s from the getty restart to the DRM Sway, of which
15.2 s was the old daemon's stop timeout; from the new daemon's start to the
DRM Sway was 0.9 s, and from the slicer's failure 0.8 s. `/system` then
reported requested `direct`, effective `wayland`, and the reason:

> direct presentation fell back to wayland for the rest of this boot (it is
> tried again after a reboot): the slicer exited before direct presentation
> was confirmed (exit status: 1)

The marker held `{"reason":"the slicer exited before direct presentation was
confirmed (exit status: 1)", ...}`. Checks were 17 pass, 2 warn
(`direct-scanout` carrying the fallback reason, and `nvidia-driver-version`),
0 fail. The kernel log for the window contained only the two message kinds
already present on this machine before the test (the 550 module's
`nv_drm_revoke_modeset_permission` warning on every DRM file close, and
correctable PCIe AER errors from the GPU); no Xid, NVRM or modeset errors.

### Why the slicer refused

The refusal was not the missing present-timing capability. [System B](../developer/test-systems.md#system-b) has an
Intel iGPU as well: `renderD128` belongs to i915 and the NVIDIA card is
`card1`/`renderD129`. The login profile's direct branch exports the first
`/dev/dri/renderD*` as `WLR_RENDER_DRM_DEVICE`, so the headless Sway rendered
on the iGPU, its dmabuf feedback named `renderD128`, and the slicer found no
Vulkan device matching both that render node and the NVIDIA primary card.
With the render node fixed, 550 would still be refused at the next step:
`vulkaninfo` lists neither `VK_EXT_present_timing` nor `VK_KHR_present_id2`
for the RTX A1000 on this driver, which `pick_direct_physical_device` rejects
by name. The reason reported in `/system` carried only the exit status, not
the slicer's error; the cause was only in the daemon's journal. Both points
are addressed by the retest below.

### Restore

`suede.toml` was restored byte-identically (SHA-256 `fcd84f9d…`), the
marker and attempt counter removed, and `getty@tty1` and `suede` restarted:
DRM Sway, `/system` presentation wayland/wayland with no reason, 18 pass,
1 warn, 0 fail, `arena-fx` running, revision 1771 committed, the three
projectors at 1920×1200@59.95, no marker. The test build and its profile
(SHA-256 `d59ec094…`) stay installed for the driver upgrade that follows.

### Retest with the render-node, reason and shutdown fixes

The same test was repeated on a rebuilt package (`/usr/bin/suede` SHA-256
`b7155b3f…`, same build ID) carrying three fixes: the login profile picks
the render node of the card with connected displays, the fallback reason
carries the slicer's last error line and the slicer states a missing
present capability in one line, and the daemon's HTTP drain on SIGTERM is
bounded at 2 s. `provision.sh` re-rendered the profile (the render-node
logic was the only difference); every other provisioned file was
byte-identical. On the Wayland session beforehand: 18 pass, 1 warn
(`nvidia-driver-version`), 0 fail, and the Wayland daemon's restart logged
"not waiting any longer for open connections to close" 2.0 s after its
SIGTERM instead of being killed at 15 s.

The headless Sway now received `WLR_RENDER_DRM_DEVICE=/dev/dri/renderD129`
(the NVIDIA card; its `card1` has DP-5, DP-7 and DP-8 connected, the iGPU's
`card0` none), and the slicer refused for the capability it lacks:

| Time | Event |
| --- | --- |
| 14:35:34.57 | `getty@tty1` restarted; the DRM Sway ends, projectors go dark |
| 14:35:35.21 | headless-only Sway up on `renderD129`, `session = direct` |
| 14:35:35.27 | `systemctl --user restart suede`; the old daemon starts its shutdown |
| 14:35:37.27 | the old daemon stops at the 2 s drain bound |
| 14:35:37.32 | new daemon resolves direct, preflight passes, slicer started |
| 14:35:37.40 | slicer refuses (text below) |
| 14:35:37.83 | exit reaped on the 1 s tick; fallback; marker written |
| 14:35:37.85 | `display-reset`; headless Sway ended |
| 14:35:38.27 | auto-login's DRM Sway up |
| 14:35:38.65 | systemd restarts the daemon; it resolves wayland from the marker |
| 14:35:38.93 | Wayland slicer started |
| 14:35:40.32 | `arena-fx` running; three projectors at 1920×1200@59.95 |

`/system.presentation.reason`:

> direct presentation fell back to wayland for the rest of this boot (it is
> tried again after a reboot): the slicer exited before direct presentation
> was confirmed (exit status: 1): slicer failed: renderer gpu was forced but
> is unavailable: Gpu::new: direct presentation needs VK_EXT_present_timing
> and VK_KHR_present_id2; this device/driver (NVIDIA RTX A1000, 550.163.01)
> exposes neither

The marker's `reason` is the same text from "the slicer exited…" onward.
The dark gap from the getty restart to the DRM Sway was 3.7 s (4.4 s to
the Wayland slicer, 5.8 s to the app running), down from 16.9 s; 2.0 s of
it is the drain bound, which the old Wayland daemon spent waiting on an
open client connection. The direct daemon's own shutdown was not delayed
by the drain: its slicer had already exited, so it stopped 19 ms after
deciding to fall back. A live direct slicer's graceful stop racing the
drain could not be exercised on this driver, since no direct slicer ever
starts on it. Checks settled at 17 pass, 2 warn (`direct-scanout` with the
reason, `nvidia-driver-version`), 0 fail; for the first few seconds after
each daemon start the `/status` summary counted one more warn than the
check list itself, then caught up. The kernel log again held only the two
message kinds already present before the test.

Wayland was restored as before (original `suede.toml` byte-identical,
marker and attempt counter removed, getty and `suede` restarted): DRM Sway
in 0.7 s, presentation wayland/wayland with no reason, 18 pass, 1 warn,
0 fail, `arena-fx` running, revision 1771 committed, no marker. The rebuilt
package and its profile (SHA-256 `81021f62…`) stay installed for the driver
upgrade that follows.

## [System B](../developer/test-systems.md#system-b): driver 595.91.07 and first direct run — September 30, 2026

The driver was upgraded from Debian's 550.163.01 to 595.91.07-1 (proprietary
kernel module, built by DKMS for 6.12.101) from NVIDIA's Debian 13
repository, pinned at that version. GSP firmware is on (`GPU Firmware:
595.91.07`). The Suede package, login profile and `suede.toml` were the same
as in the retest above. The 550 packages and a rollback script are kept on
the machine.

### Wayland

The DRM Sway came up on DP-5, DP-7 and DP-8 at 1920×1200@59.95 with
`arena-fx` running. `/system` reported:

```json
{"version": "595.91.07", "kernelModule": "proprietary",
 "gspFirmware": "on", "gspOptional": true, "newestTested": "595.91.07"}
```

`nvidia-driver-version` passed ("driver 595.91.07 is at least as new as the
newest tested release 595.91.07") and `gsp-firmware` passed ("GSP firmware
on; no measured effect on the Wayland path"). The Wayland session's own
kernel log no longer shows the `nv_drm_revoke_modeset_permission` warning
550 printed on every DRM file close.

### Direct presentation

With `presentation = "direct"` and a restart of `getty@tty1` and `suede`,
the headless Sway rendered on the NVIDIA node (`renderD129`) and `/system`
reported requested and effective `direct` on DP-5, DP-7 and DP-8 with no
reason. The slicer took NVKMS sub-ownership 2.4 s after the new daemon
started (`backend vulkan-display`, timestamps from `VK_EXT_present_timing`).
`gsp-firmware` warned, as designed for GSP on with the proprietary module
presenting directly; `direct-scanout`, `real-displays` and `swaybg` reported
their direct-mode wording; no check failed.

The sync pattern, previewed uncommitted over the committed 2×2 warp
arrangement, presented on all three outputs:

| Metric (8 intervals of 10 s) | Value |
| --- | --- |
| Presented fps | 57.19 mean (56.89–57.51) |
| Straddles per interval | 21.9 mean (17–24) |
| Gate holds per interval | 48.1 mean (41–54) |
| Offset between outputs | 0.99 ms mean, 16.87 ms max (one refresh) |
| Per output, last interval | 574 presented, 0 discarded each |
| Phase (DP-5 reference) | DP-8 −0.370 ms, DP-7 −0.181 ms |
| Frames shown one refresh late | DP-5 0, DP-8 13, DP-7 23 |
| Slicer CPU | 26.5% of one core |

The straddles come from DP-7 and DP-8 occasionally presenting a frame one
refresh after DP-5 (`lagFrames.one`), not from phase: all three heads sit
within 0.4 ms of each other.

### Stopping a running direct slicer

**Daemon restart mid-session.** `systemctl --user restart suede` while the
sync pattern was presenting: the slicer logged "NVKMS sub-ownership revoked"
0.47 s after SIGTERM, inside the daemon's 2 s HTTP drain; the daemon stopped
at the drain bound (2.0 s), the new daemon resolved direct again 45 ms later
and its slicer was running 0.6 s after that, on the same headless Sway. The
two timers did not interfere: the slicer's graceful stop finished well
within the drain. The kernel log held only correctable PCIe AER messages.

**Back to Wayland.** With the sync pattern presenting, `suede.toml` was
restored and `getty@tty1` restarted. The daemon saw the compositor go
(0.16 s), the slicer revoked its grant 0.51 s after the restart and exited,
and the DRM Sway was up 1.5 s after the restart. The daemon then waited out
its 2 s drain and ran `display-reset` after the DRM Sway had already
started (harmless: the reset only acts on a card with no DRM master), and
systemd restarted it 2 s later; the appliance was healthy 5.9 s after the
restart with `/system` wayland/wayland, no marker, and the original
`suede.toml`. The kernel log again held only correctable PCIe AER messages.

### Hardware video decode after the upgrade

Right after the upgrade's reboot, `video-decode` and `decode-measured` warned
that every codec decoded in software. The cause is not specific to 595:
`nvidia-vaapi-driver` needs CUDA, CUDA needs the `nvidia-uvm` module and its
device node, and neither driver's packaging loads `nvidia-uvm` at boot. The
browser's sandboxed GPU process cannot load it, so a browser started before
anything else on the machine has initialized CUDA fails `vaInitialize`
("`nvidia_drv_video.so` init failed") and falls back to software decode.
The startup measurement is cached per driver, kernel, browser and build, and
on this boot it ran 12 s after the driver loaded. On the previous 550 boot,
`nvidia-uvm` had been loaded days before the measurement that passed.
Confirmed directly: with `nvidia_uvm` unloaded a fresh measurement decoded
everything in software; after an unsandboxed VA-API initialization loaded
it, the next measurement showed hardware decode for H.264, H.265, VP9 and
AV1 and both checks passed.

## [System B](../developer/test-systems.md#system-b) campaign: drivers, GSP, yield and alignment — September 30, 2026

Question: which driver, kernel module, GSP state, `gl_yield` setting and
presentation path gives the best-coordinated wall on
[System B](../developer/test-systems.md#system-b) (RTX A1000, three
1920×1200 outputs)? Answer: Wayland with the outputs phase-aligned after
session start, `gl_yield = usleep`, any driver 595 or newer.

### Method

- Live-daemon harness: the sync pattern previewed uncommitted over the
  committed 2×2 warp arrangement; every run restored the original
  `suede.toml` byte for byte.
- Two canvas widths: 1600 px (light) and 3909 px (saturated; GPU at
  95–100%).
- Runs of 110 s (30 s warmup, 60 s collect, 20 s tail), 2 repeats per
  point. The first metric is straddles per fresh 10 s interval, normalized
  by interval count (mean of the repeats).
- Total CPU is browser + Sway + slicer, as a percentage of one core, at the
  saturated width.
- Factors across the campaign: driver (550, 595.91.07, 610.57.04,
  615.71.09), kernel module (proprietary or open), GSP firmware (on or off),
  `gl_yield` (default or `usleep`), path (Wayland, phase-aligned Wayland,
  direct).
- Raw per-run data is kept locally.

### Results

| Configuration | Light: straddles / fps | Saturated: straddles / fps | Saturated CPU |
| --- | --- | --- | ---: |
| Aligned Wayland, usleep, 615 open | 0.0 / 59.9 | 3.6–4.3 / 20.4–20.9 | 40% |
| Aligned Wayland, usleep, 610 GSP on | 0.0 / 59.9 | 4.7 / 21.4 | 37% |
| Aligned Wayland, usleep, 595 GSP on | 0.0 / 59.9 | 6.2 / 20.3 | 36% |
| Aligned Wayland, default yield, 615 open | 0.0 / 59.9 | 2.6 / 21.0 | 103% |
| Unaligned Wayland, usleep, 615 open (production today) | 31.2 / 59.7 | 55.8 / 20.8 | 34% |
| Direct, usleep, 595 GSP off | 1.7 / 59.8 | 62.2 / 14.4 | 46% |
| Direct, usleep, 610 GSP off | 0.1 / 59.9 | 89.0 / 11.7 | 35% |
| Direct, usleep, 615 open (GSP on) | 512.4 / 53.3 | 65.3 / 7.1 | 61% |

### Per-driver notes

- **550.163.01 (Debian).** Unsupported: no `VK_EXT_present_timing` or
  `VK_KHR_present_id2`, so direct falls back to Wayland cleanly; see the
  [fallback section](#system-b-unsupported-driver-fallback-driver-550-september-30-2026).
- **595.91.07.** Full matrix run with GSP on and off. Direct needs GSP off
  to keep up at light load. GSP does not affect Wayland.
- **610.57.04.** The last release that ships the proprietary module, so the
  newest driver on which GSP can be turned off. Direct with GSP off matches
  aligned Wayland at light load and trails it under saturation.
- **615.71.09.** NVIDIA's repository ships only the open kernel module
  (GSP always on). The first direct attempt failed because 615 removed
  `NVKMS_IOCTL_CHECK_LUT_NOTIFIER`, shifting every NVKMS command from 13
  upward down by one: `ACQUIRE_PERMISSIONS` is 40 on 550 and 615 and 41 on
  580 through 610. Suede hard-coded 41, which on 615 is
  `REVOKE_PERMISSIONS`; NVKMS rejected it on its `paramSize` check. Suede
  now tries both indexes, ordered by the running driver version, and fails
  direct display creation (falling back with a reason) if the grant succeeds
  but no handle accepts the token. With the fix, direct works on 615 but is
  not competitive (table above). The suspected 3 s per-head exit regression
  was not reproduced: six Wayland `getty@tty1` restarts (three with flipping
  heads) and two direct-to-Wayland fallbacks logged no flip-event timeouts.

### Conclusions

- Phase alignment is the dominant factor: unaligned Wayland straddles
  31–56 per interval, aligned Wayland 0–6.
- GSP does not affect Wayland.
- `usleep` costs about 1 straddle per interval under saturation and saves
  about 60 points of one core.
- Direct matches Wayland only at light load with GSP off (proprietary
  module, so driver 610 or older), never under saturation, and is not
  competitive on 615's open module.
- Direct's heads hold a fixed 2–8 ms offset; aligned Wayland holds
  0.04–0.15 ms.
- Recommended: aligned Wayland, `gl_yield = usleep`, driver 615.71.09 open
  module (newest, NVIDIA's supported module, GSP irrelevant to Wayland).
  `NEWEST_TESTED_NVIDIA_DRIVER` is now 615.71.09.

Follow-ups: (1) automatic phase alignment after every session start (a
product change; the largest improvement found); (2) `gl_yield` default for
NVIDIA appliances; (3) loading `nvidia_uvm` at boot for hardware decode.
