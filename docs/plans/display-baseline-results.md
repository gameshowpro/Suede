# [System A](../developer/test-systems.md#system-a) display baseline — September 28, 2026

**Historical baseline:** every Seascape result in this report measures the
existing Sway/Wayland presentation path. The standalone display probe presents
generated test colors; its submission rates are not comparable application FPS.
The subsequent [component integration](display-integration.md) adds direct
presentation to the real slicer and records its separate validation status.
The matched [direct-versus-Wayland results](display-component-results.md) are
reported separately. Do not interpret these baseline tables as direct-display
measurements.

No production Suede code or installed binary was changed to collect these
baseline measurements. Subsequent implementation is staged separately on
[System A](../developer/test-systems.md#system-a); this historical report makes no improvement or rollout claim.

The retained [System A](../developer/test-systems.md#system-a) baseline is the packaged NVIDIA 595.91.07 server driver.
It is installed persistently and has already survived reboots; the saved 580
packages are rollback material, not an automatic reversion. Before the next
hardware test, reboot to clear possible state from prior failing probes,
verify driver/kernel/configuration and health, then warm up the workload.
Capture the boot identity with each run. Compare Wayland and direct display
on this same driver to separate backend benefits from the driver upgrade.
Acceptance now explicitly requires reduced CPU/GPU overhead at equivalent
frame rate, synchronization, and latency; reductions remain to be established
with repeated full-pipeline measurements.

## Identity and method

- Suede `v0.1.14-9-ge20a8f5`; installed binary SHA-256:
  `eae12d476f02563f70f2463fedd99a19fe357a25055c676edae59071ba1b9bfa`.
- Quadro RTX 8000, NVIDIA 580.178.04; Linux 7.0.0-34-generic; Sway 1.11.
- Four 1920×1080 outputs, reporting 59.939 Hz in Sway; direct scanout enabled.
- The running browser was Google Chrome 151.0.7922.71. The system API also
  lists installed Chromium 153; that is not the browser executable used by
  these baseline workloads. Executable hashes are in the raw captures.
- Current canvas: 900×536, aspect 1.68, existing warp and blend geometry.
  Separate loaded runs used render width 3200 with the same aspect/geometry.
- Each workload: 30-second warmup, one 60-second capture, approximately
  one-second CPU/GPU samples. Frame figures use only newly observed reports,
  excluding the initial potentially old stats snapshot.
- CPU is percent of one core from process tick deltas; 100% means one core.
  Means exclude unpaired process samples. These CPU windows are not claimed
  to align exactly with Suede's approximately ten-second stats reports.

## Results

| Workload | Canvas width | Sway CPU | Slicer CPU | GPU utilization | Presented fps, mean | Straddles | Gate holds |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| As-found idle content | 900 | 0.17% | 0.56% | 0.0% | N/A | N/A | N/A |
| Sync, light source | 900 | 10.47% | 8.25% | 31.5% | 59.87 | 1 | 3 |
| Sync, Seascape source | 900 | 14.96% | 8.73% | 34.5% | 59.87 | 4 | 4 |
| Seascape content | 900 | 19.86% | 6.22% | 33.0% | 59.90 | 1 | 9 |
| Sync, Seascape source | 3200 | 19.27% | 10.30% | 55.3% | 59.55 | 17 | 20 |
| Seascape content | 3200 | 31.50% | 8.47% | 55.1% | 58.93 | 17 | 61 |

The idle run published no fresh frame intervals, consistent with damage-driven
idle behavior. It is not a zero-FPS presentation failure. The three animated
900-pixel runs contain five, six, and five fresh intervals respectively; raw
counts must be normalized by those intervals/frames before comparing arms.
Their maximum absolute per-output phase was 0.0096, 0.0135, and 0.0116 ms.
Every reported presentation on all four outputs used zero-copy scanout in
those three runs. The larger canvas increased cost but did not saturate this
GPU. These are loaded baselines, not measurements of a 99%-busy GPU.

Each condition has only one run; do not infer statistical confidence or a
performance gain from these values. Repeat the matching conditions after any
backend change and collect multiple alternating control/experimental runs.
The original full configuration was restored and checked for exact equality
after both the current-settings and larger-canvas matrices.

## Wayland baseline on NVIDIA 595.91.07

The six-condition Wayland matrix was repeated on [System A](../developer/test-systems.md#system-a) with NVIDIA package
`595.91.07-0ubuntu0.26.04.2`. The GPU remained a Quadro RTX 8000 and the kernel,
Suede binary, workload configuration, browser, and output setup matched the
580.178.04 captures. The running browser was Google Chrome 151.0.7922.71;
Chromium 153.0.8010.36 snap is separately listed by the system API and was not
the browser executable used for these workloads.

| Workload | Canvas width | Sway CPU | Slicer CPU | GPU utilization | Presented fps, mean | Straddles | Gate holds |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| As-found idle content | 900 | 0.17% | 0.34% | 0.03% | N/A | N/A | N/A |
| Sync, light source | 900 | 10.07% | 7.88% | 31.7% | 59.92 | 1 (5 windows) | 1 (5 windows) |
| Sync, Seascape source | 900 | 15.52% | 8.17% | 35.9% | 59.90 | 1 (6 windows) | 3 (6 windows) |
| Seascape content | 900 | 20.35% | 5.88% | 32.1% | 59.92 | 0 (5 windows) | 8 (5 windows) |
| Sync, Seascape source | 3200 | 14.81% | 8.53% | 55.1% | 59.87 | 1 (5 windows) | 3 (5 windows) |
| Seascape content | 3200 | 19.50% | 5.91% | 53.4% | 59.90 | 0 (5 windows) | 2 (5 windows) |

Each condition is one 60-second collection. CPU values are means of valid
process-counter samples, reported as percent of one core; GPU utilization is
the mean of available samples. FPS is the mean of fresh approximately
ten-second stats windows. Straddle and gate-hold cells show raw counts followed
by the number of fresh windows, so counts can be normalized across conditions.
Idle again had no fresh frame intervals; its presentation metrics are
unavailable, not zero. The configuration was restored and checked for exact
equality after this matrix.

The active 3200-pixel sync-stress run averaged 207.95 W, 72.37 °C, and 1910.5
MHz, compared with 196.30 W, 67.23 °C, and 1919 MHz on 580.178.04. The
3200-pixel Seascape-stress run averaged 213.01 W, 76.28 °C, and 1905 MHz,
compared with 208.35 W, 74.53 °C, and 1897.75 MHz. These are single-condition
observations with differing thermal conditions, not evidence of a statistically
established performance improvement. This section records the Wayland baseline
only; the direct-display timing and fbdev experiments below are separate tests.

Raw JSON, the exact collector used, configuration backups, and probe logs are
kept locally in `research/display-2026-09-28/` (ignored by Git because they
contain site information). The reproducible collection method and delegation
sequence are in [the baseline runbook](display-baseline.md).

## Research probes

An isolated headless-only Sway session, with a separate Chromium instance,
provided 900×536 GPU DMA-BUF capture to the existing slicer at about 58.8 fps.
This establishes capture feasibility, not daemon watchdog/readiness parity or
production output planning under headless-only Sway.

Vulkan enumeration found four displays and four planes. Its mode list reports
59.940 Hz where Sway reports 59.939 Hz. The probe selects the explicit Vulkan
mode rather than assuming the two APIs use identical rounding. Vulkan display
names also differ from DRM/Sway connector names; connector IDs are mapped with
`vkGetDrmDisplayEXT`, not matched by display name.

On NVIDIA 580.178.04, [System A](../developer/test-systems.md#system-a) exposed `VK_KHR_present_wait`/`VK_KHR_present_id` and
`VK_EXT_display_control`, but neither `VK_GOOGLE_display_timing` nor
`VK_EXT_present_timing` appeared in its capability dump. Completion waits and
vblank counters do not establish the per-image timestamp contract needed for
unchanged phase/lag statistics. Cross-display atomicity is also not guaranteed
by ordinary batched Vulkan presentation. See the primary references in
[the corrected architecture assumptions](display-baseline.md#corrected-architecture-assumptions).

The one-display test submitted 3,590 presents in 60.015 seconds (59.819/s)
and exited normally. The four-display test submitted 3,564 batches in 60.017
seconds (59.383/s per connector), then reported final-present completion on
all four connectors. **It did not exit within the controller's 75-second
limit** and was terminated by `timeout` (exit 124). The normal Sway and Suede
session restarted afterward. Instrumentation localized this to closing the DRM card after unloading the
Vulkan loader; every Vulkan destruction call had already returned. Keeping the
loader alive until after DRM close fixed the short four-display exit test. The
full-duration verification with that corrected ordering submitted 3,563 batches
in 60.009 seconds (59.375/s per connector), then completed every cleanup stage
and exited with status 0. This was a probe resource-lifetime bug, not an
established driver inability to release four displays.
Submission counts do not prove scanout FPS or phase.

An explicit SIGKILL of the four-display probe returned shell wait status 137.
A new process immediately reacquired all four connectors and completed a
five-second presentation run. Session recovery was controlled externally by
the test harness; this does not implement or validate production slicer crash
recovery under a direct backend.


Final restoration checks confirmed exact equality with the original saved
configuration, the original binary hash and `cap_sys_nice=ep`, four active
1920×1080 outputs at Sway's 59.939 Hz, the original app running, no divergences,
and all 17 health checks passing. No recovery timers remained pending.

The production backend remains unimplemented. The environment experiments below
establish a usable timing source on 595, but not working present barriers.
Neither fleet-wide support, synchronized cold start, nor production crash
recovery is established by these standalone probes.

## Environment follow-up

A read-only [System B](../developer/test-systems.md#system-b) inventory found an RTX A1000 on NVIDIA 550.163.01. This
historical inventory compared against [System A](../developer/test-systems.md#system-a)'s then-installed 580.178.04 driver.
Both surveyed driver versions advertised `VK_NV_present_barrier`
but not `VK_EXT_present_timing` or `VK_KHR_display_swapchain`. No driver
was changed for this inventory.

NVIDIA's [595 release announcement](https://forums.developer.nvidia.com/t/595-release-feedback-discussion/362561)
adds `VK_EXT_present_timing`. Its [Vulkan driver release notes](https://developer.nvidia.com/vulkan-driver)
also record a direct-display timing fix in developer driver 580.94.18.
Select a supported packaged driver containing this support; updating only the
Vulkan loader, tools, or SDK cannot add the driver's missing implementation.
Query actual surface timing stages and usable clock domains, then validate
per-image feedback before claiming phase/lag telemetry parity.

The [present-barrier extension](https://docs.vulkan.org/refpages/latest/refpages/source/VK_NV_present_barrier.html)
can synchronize corresponding presentation requests across swapchains; it is
not limited to multiple GPUs. Query its device feature and each direct-display
surface's support, then test it explicitly enabled. Advertisement alone is
insufficient, and presentation synchronization does not establish atomic mode
setting or physical scanout phase alignment.

[System A](../developer/test-systems.md#system-a) is the primary test workhorse and [System B](../developer/test-systems.md#system-b) is the production analog for
confirmation. Test on [System A](../developer/test-systems.md#system-a) first unless a documented hardware difference
confounds the result. The recorded [System B](../developer/test-systems.md#system-b) inventory above is historical; it
does not imply tests or upgrades have been run there.

### Completed [System A](../developer/test-systems.md#system-a) experiments

The exact 580 server driver packages were saved for rollback before installing
Ubuntu's `nvidia-driver-595-server=595.91.07-0ubuntu0.26.04.2`. The kernel and
production Suede binary remained unchanged. The six Wayland workloads above
were repeated on 595 before testing direct display.

| Experiment | Observed result |
| --- | --- |
| Present barrier, 580.178.04 | Extension, device feature, and all four surfaces report support; the first batched present fails with `ERROR_UNKNOWN` for connector 141. |
| Present barrier, 595.91.07 | Same advertised support and same failure, with and without the validation layer. |
| Present timing, 595.91.07 | `VK_EXT_present_timing` revision 3 and `VK_KHR_present_id2` are supported. All four surfaces return complete per-present timing records. |
| Shared display swapchains, 595.91.07 | `VK_KHR_display_swapchain` remains absent. A shared input canvas remains possible; shared scanout allocation is a separate capability. |
| Temporary `nvidia_drm.fbdev=0`, 595.91.07 | The short timing and barrier tests produced no kernel flip warnings/timeouts, unlike the original fbdev setting. The barrier still failed. Original boot settings were restored afterward. |

The timing stage used is `IMAGE_FIRST_PIXEL_OUT`, not
`IMAGE_FIRST_PIXEL_VISIBLE`. [System A](../developer/test-systems.md#system-a) exposes DEVICE and local presentation clock
domains, but no direct `CLOCK_MONOTONIC` presentation domain. Its
`timestampPeriod` is 1 ns, allowing DEVICE presentation nanoseconds to be mapped
to host monotonic time using `VK_KHR_calibrated_timestamps`. The probe records
calibration pairs about every 250 ms and rejects unbracketed events or pairs
whose clock-rate mismatch exceeds its consistency threshold. This is an
estimate with separately reported calibration deviation and observed mismatch,
not a proven error bound between samples.

A single start/end calibration over ten seconds drifted by 41,902 ns and was
correctly rejected. Periodic calibration in the fbdev-off ten-second test
mapped all 2,224 returned records (556 per output); all 38 adjacent sample pairs
passed the consistency checks. Maximum reported calibration deviation was
2,944 ns and maximum adjacent-pair mismatch was 456 ns. There were no missing,
duplicate, zero, or incomplete records. The largest equal-present-ID span
during startup was approximately 571 ms. After excluding the first two seconds
relative to the latest output's first timestamp, the maximum span was 1,280 ns.
Those measurements establish useful software feedback, not synchronized cold
start or physical genlock. The clear-color probe does not exercise the full
capture, warp, blend, and settle pipeline.

### Final verification on the retained configuration

With the original `fbdev=1` setting restored and validation disabled, the
60-second timing run submitted 3,562 batches in 60.007 seconds. Each of the
four outputs returned exactly IDs 1–3,562: 14,248 complete, nonzero records,
with no missing or duplicate IDs. All 238 adjacent calibration pairs passed
the consistency checks, mapping every record. Maximum reported calibration
deviation was 11,328 ns; maximum adjacent interval mismatch was 2,460 ns.
The whole-run clock mismatch was 195,419 ns, reinforcing the need for periodic
calibration rather than a single fixed offset.

The maximum equal-ID span was 457.6 ms during startup and 1,216 ns after the
same two-second exclusion used above. The software evidence therefore does
not satisfy the synchronized-cold-start gate. Normal teardown returned
success but took 13.112 seconds; total probe wall time was 77.27 seconds.
Kernel flip warnings and per-head flip timeouts returned with `fbdev=1`.
Automatic recovery restored the normal service. Validation-enabled tests had
reported no Vulkan validation errors; this does not rule out application or
driver defects.

[System A](../developer/test-systems.md#system-a) retains NVIDIA 595.91.07 and the original kernel. Final checks confirmed
exact restoration of the original Suede configuration and boot configuration
files, unchanged installed binary hash and capabilities, four active
1920×1080 outputs at 59.939 Hz, the original application running, no
divergences, all 17 health checks passing, and no pending display-recovery
timers. Exact 580 driver rollback packages remain saved on [System A](../developer/test-systems.md#system-a) at
`/var/tmp/suede-display-driver-20260928/rollback-packages/`. [System B](../developer/test-systems.md#system-b) was not
modified or used for these experiments.

The next gate is synchronized startup and reliable recovery on [System A](../developer/test-systems.md#system-a), followed
by integration of this timing contract with Suede's existing settle logic.
The repeated barrier failure is now explained; see the
[root cause section](#present-barrier-root-cause). An alternate synchronization
design is needed before claiming atomic wall updates. Full headless
daemon readiness/watchdog behavior also remains unverified. Confirm a viable
path on [System B](../developer/test-systems.md#system-b) afterward; use frame-coded camera observations to check wall
coherence independently of software feedback. CPU cost, GPU work, power, and
throughput remain measurable regardless of timestamp availability.

## [System B](../developer/test-systems.md#system-b) projector comparison

The user's paired-projector inventory justified a hardware comparison after
the [System A](../developer/test-systems.md#system-a) experiments. [System B](../developer/test-systems.md#system-b) remained on its existing RTX A1000,
NVIDIA 550.163.01, and Linux 6.12.101+deb13-amd64. No driver, boot setting,
production binary, or saved Suede configuration was changed.

Only three projectors were connected during the test: DP-5 (DRM connector 89),
DP-7 (97), and DP-8 (101). Configured output DP-6 (93) was already disconnected.
All active projectors used 1920×1200 at 59.950 Hz. EDID identifies DP-7 and DP-8
as the same product, with identical detailed 1920×1200 timing descriptors.
DP-5 advertises the same 154 MHz pixel clock and 2080×1235 totals, but different
sync-polarity flags. The missing projector prevented a four-output or
second-identical-pair test.

The same standalone probe was run without validation, with five-second
presentation intervals and independent timed session recovery:

| Test | Result |
| --- | --- |
| Ordinary presentation, all three | 292 batches in 5.003 s; final-present waits completed on all heads; normal exit 0. |
| Barrier, all three | First present failed with `ERROR_UNKNOWN`, reported for connector 101; exit 3. |
| Barrier, identical DP-7/DP-8 pair | First present failed with `ERROR_UNKNOWN`, reported for connector 101; exit 3. |
| Barrier, DP-7 alone | First present failed with `ERROR_UNKNOWN`, reported for connector 97; exit 3. |

Every requested surface, along with the device feature and extension, reported
barrier support, and barrier-enabled swapchain creation succeeded. The
ordinary control's normal teardown took 609 ms, with total wall time 7.816 s.
These short runs do not establish sustained performance or synchronization;
550 does not expose `VK_EXT_present_timing`. Different GPU, driver, kernel,
output count, and display mode prevent attributing the teardown difference
from [System A](../developer/test-systems.md#system-a) to any single factor.
Kernel logs also contain `nv_drm_revoke_modeset_permission` warnings, including
during the initial inventory before the presentation matrix. They are not
specific evidence of a barrier failure; recovery checks below concern the
restored application and output state, not absence of kernel warnings.

Failure with an identical pair makes mixed projector models a weaker
explanation; the single-projector failure also warrants a minimal feature
interaction/control test. The barrier runs followed each other in the same
session outage, so retained driver state after a prior failure is not excluded.
This evidence does not establish whether the defect is in the application,
driver, or required system setup.

Automatic recovery restored all three projectors and the original application.
Checks confirmed exact saved-configuration equality, unchanged binary hash
and capabilities, unchanged driver, original active output modes, no new
configuration divergences, no failed health checks, and no pending recovery
timer. The pre-existing disconnected DP-6 and video-decode warnings remain;
the initial phase warning was 1.1 ms and can change when the session restarts.
Raw logs, EDIDs, and before/after snapshots are in local, ignored research
notes for [System B](../developer/test-systems.md#system-b). Controller setup attempts
that stopped before running a probe are preserved separately from test logs.

### Relationship to the current Sway path

Suede already coordinates outputs above Wayland: in locked mode, it waits for
every participating output's prior `wp_presentation` outcome before submitting
the next wall update, and uses the same completed capture across the slices.
An unresponsive output is eventually excluded from the gate so it cannot
freeze the wall. Presentation feedback reports a realized presentation event;
it is distinct from a frame callback inviting the client to draw again. See
the [protocol source](https://raw.githubusercontent.com/wayland-mirror/wayland-protocols/main/stable/presentation-time/presentation-time.xml)
and the [current synchronization description](../how-it-works.md#keeping-the-displays-in-step).

Separately, Suede batches Sway output setup and can disable/re-enable all heads
together to improve startup phase alignment. The measured improvement on
NVIDIA is not a portable physical clock-lock guarantee. The feedback gate
prevents outputs from continually running ahead; it cannot retroactively
prevent a frame mismatch caused by one head missing its presentation deadline.
A present barrier coordinates the new presentation requests inside the driver.

Therefore working `VK_NV_present_barrier` is a candidate for stronger
coordination, not a feature the current Wayland path already depends on.
A direct backend could seek parity by preserving Suede's existing gate with
validated completion/timing feedback and independently solving output setup
and recovery. It must demonstrate that parity under load and faults before
replacing the current path; a failed vendor barrier alone does not prove that
parity is impossible.

## Seascape resolution sweep

**Presentation backend: existing Sway/Wayland, control arm only.** The direct
display arm still requires integration with Chrome capture and Suede's existing
warp/blend renderer before this sweep can be repeated under that path.

[System A](../developer/test-systems.md#system-a) was rebooted before this sweep, which ran September 29, 2026 UTC
(September 28 in the user's time zone). The new boot ID was
`39da87be-9c55-4e30-b90d-1aa55487e656`. NVIDIA 595.91.07, kernel
7.0.0-34-generic, the installed Suede binary identified above, and Google Chrome
151.0.7922.71 were retained. Health passed 17/17 after boot.

The existing Seascape application ran without a Suede test-pattern overlay.
Only canvas `renderWidth` varied between conditions; the 1.68 aspect, four
1920×1080 physical outputs at 59.939 Hz, warp/blend geometry, and remaining
configuration stayed fixed. Every point used a temporary preview with
revision/generation/epoch preconditions and independent timed rollback, followed
by exact restoration of committed revision 1979. No production binary changed.

The live Seascape page sets its WebGL canvas dimensions from the window size
times device pixel ratio and draws a fullscreen shader each animation callback.
Increasing canvas size therefore increases rendering work, rather than merely
scaling a fixed-size shader image. A source snapshot is saved with SHA-256
`73fb187dc11ee3ac2728cababb74a0b593b9fc20827ece0b79c71d7b9023e114`.
Each condition had a 30-second warmup and a 60-second collector run. The order
was 3200, 4500, 4000, 6000, 4500, 3200, 4500 pixels wide.

| Canvas | Run | Presented fps, mean | GPU utilization, mean | Total CPU, % of one core | GPU temperature, °C | Graphics clock, MHz |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 3200×1905 | 1 | 59.89 | 54.47% | 73.74% | 67.45 | 1917 |
| 4000×2381 | 1 | 59.84 | 70.65% | 56.45% | 79.13 | 1868 |
| 4500×2679 | 1 | 51.23 | 74.82% | 75.92% | 77.73 | 1849 |
| 6000×3571 | 1 | 35.88 | 91.62% | 75.19% | 81.77 | 1772 |
| 4500×2679 | 2 | 57.42 | 82.08% | 71.87% | 81.10 | 1816 |
| 3200×1905 | 2 | 59.28 | 56.22% | 67.90% | 76.98 | 1889 |
| 4500×2679 | 3 | 50.21 | 76.32% | 74.99% | 80.48 | 1837 |

FPS is the mean of fresh approximately ten-second presentation intervals;
4500 run 1 contains six intervals and every other run contains five. The first
potentially stale snapshot is excluded. Total CPU sums Sway, slicer, daemon,
and browser for each sample only when all four roles have valid tick deltas;
partial totals are excluded. GPU figures are utilization samples, not measured
shader execution time. All seven captures completed without collector errors,
and their executable hashes, driver, kernel, physical output configuration,
bootstrap, and compositor environment matched. Configuration differed only
in canvas width across the measured Seascape conditions.

**Use 4500×2679 as the primary workload for testing recovery of frame rate.**
All three runs failed to sustain the 59.939 Hz output rate, with means spanning
50.21–57.42 fps. Their lowest fresh-window rates were 49.63, 55.51, and 48.39
fps. The three runs recorded 33, 36, and 44 straddles, and 353, 153, and 283
gate holds respectively; these are raw counts over six, five, and five
intervals and must be normalized before comparing arms. Retain 4000×2381 as
a near-full-rate case and 6000×3571 as a heavier overload case; the latter has
only one exploratory run. The warmer 3200 control recovered close to full
rate but had some frame loss, so it is not an invariant zero-drop reference.

The variable 4500 results are real run variation, not an established backend
effect. Seascape animates its camera/scene, GPU temperature and clock changed,
and the driver/pipeline can vary; these data do not identify the cause of the
difference. Repeat and alternate control/experimental runs on the same driver
before claiming improvement. Higher achieved FPS at the same workload is a
throughput result; lower overhead at equivalent FPS is a separate comparison.

The captured `canvasFps` is the slicer's capture rate, not a direct measurement
of Chrome's rendered FPS. Likewise, `perFrameMs.gpu` measures host wall time
waiting for the Vulkan fence, not GPU timestamp execution time. This sweep
establishes a demanding Chrome workload that overloads the existing complete
pipeline; it does not isolate Chrome's shader as the sole bottleneck.

Final verification confirmed the original 900×536 canvas and active application,
all four physical output modes, exact committed configuration, unchanged Suede
hash/capabilities and driver, no divergences, 17/17 passing health checks, and
no pending resolution-recovery timers. Raw captures, summaries, controller
logs, and the complete remote evidence archive are under the ignored
`research/display-2026-09-28/resolution-sweep/` directory; the source machine
also retains `/var/tmp/suede-resolution-sweep-20260929/`.

## Present barrier root cause — September 29, 2026 {: #present-barrier-root-cause }

The barrier failure was traced with an `LD_PRELOAD` shim that logs every
nvidia-drm, NVKMS (`/dev/nvidia-modeset`), and Resource Manager ioctl the
NVIDIA user-space driver issues from the probe process, cross-checked against
the NVKMS sources in `open-gpu-kernel-modules` at tag 595.91.07. [System A](../developer/test-systems.md#system-a) stayed
on 595.91.07 and kernel 7.0.0-34 throughout; every run below used the standard
harness (Suede and the tty1 session stopped, automatic recovery afterwards).

1. **The driver implements `VK_NV_present_barrier` with an NVKMS swap group.**
   On the first barrier present it issues four `SET_MODE` calls and then
   `NVKMS_IOCTL_ALLOC_SWAP_GROUP`, which returns `EPERM`; the driver reports
   that as `VK_ERROR_UNKNOWN` on the last swapchain of the batch. NVKMS only
   allows swap groups, flip-lock groups, and framelock attributes for the
   modeset owner or sub-owner (`nvKmsOpenDevHasSubOwnerPermissionOrBetter`).
   A client that acquired its displays through `vkAcquireDrmDisplayEXT` never
   is one: nvidia-drm grants the driver's NVKMS handle per-head modeset
   permission only. This is independent of display count, model, and driver
   version, which matches the [System B](../developer/test-systems.md#system-b) single-projector and identical-pair
   failures.
2. **Sub-ownership can be forced from inside the process.** The DRM master may
   issue `DRM_IOCTL_NVIDIA_GRANT_PERMISSIONS(SUB_OWNER)` with a fresh NVKMS
   token, and the process can then call `NVKMS_IOCTL_ACQUIRE_PERMISSIONS` on
   the driver's own `/dev/nvidia-modeset` handle, found through
   `/proc/self/fd`. The grant must follow display acquisition (with full
   permissions already held, `vkAcquireDrmDisplayEXT` fails with
   `VK_ERROR_INITIALIZATION_FAILED`) and must be revoked before display
   release, because nvidia-drm rejects every atomic commit while it stands.
   With the grant, `ALLOC_SWAP_GROUP` and `SET_SWAP_GROUP_CLIP_LIST` succeed.
3. **The next step needs Quadro Sync hardware.** The driver then sets the disp
   attribute `FRAMELOCK_SYNC = 0`, which NVKMS rejects when the GPU has no
   framelock device (`SetFrameLockSync` returns false when `pFrameLockEvo` is
   null). When the shim reports success for that call, the driver registers a
   surface and a deferred request FIFO, queries the first display's dynamic
   data, and fails in user space with no further kernel call; the failure
   strings in `libnvidia-eglcore` sit beside `QuadroSyncServerDpy`,
   `QuadroSyncClientDpys`, and `QuadroSyncHouseOutput` registry-key parsing.
   NVKMS's own flip-lock group ioctl is a no-op for one GPU
   (`EnableLockGroupFlipLock`: "TODO: enable fliplock for single GPUs"), so no
   kernel path offers single-GPU cross-head flip lock outside swap groups.
4. **No driver release changes this.** The same permission check, the same
   framelock early return, and the same single-GPU TODO are present at tag
   615.71.09, the newest published kernel-module source (September 2026).
   The 595.44.15 Vulkan beta and the 610 and 615 branches list no barrier or
   direct-display changes, and 615.71.09 carries a separate regression of
   three-second blocking commits per head on compositor exit. [System A](../developer/test-systems.md#system-a)'s Quadro
   RTX 8000 accepts a Quadro Sync II board; [System B](../developer/test-systems.md#system-b)'s RTX A1000 has no sync
   connector, so a hardware barrier is not available on the production analog.

### Sub-ownership as a fix for teardown and kernel warnings

Sub-ownership also switches nvidia-drm into a passive mode: it stops handling
NVKMS flip events (the source of the `nv_flip == NULL` warnings) and does not
issue its own blocking commits while the grant stands. A matched pair of
ten-second `--present-timing` probe runs on the retained `fbdev=1` boot gave:

| Run | Kernel warnings | Flip event timeouts | Teardown | Probe wall | Process CPU (user / sys) |
| --- | ---: | ---: | ---: | ---: | ---: |
| Per-head modeset permission | 4 | 4 (3 s each) | 13,183 ms | 27.2 s | 0.35 s / 6.81 s |
| NVKMS sub-ownership | 0 | 0 | 929 ms | 15.2 s | 0.32 s / 7.02 s |

The presenter now grants sub-ownership after acquiring its displays, revokes
it before releasing them (also on the unverified-work path), and clears a
stale grant left by a crashed predecessor at startup; the probe exposes the
same behavior as `--nvkms-sub-owner` and `--revoke-sub-owner`, and the [System A](../developer/test-systems.md#system-a)
harness's recovery script revokes before restarting the session. The grant is
issued only when `DRM_IOCTL_VERSION` names `nvidia-drm`. Because nvidia-drm
refuses atomic commits while the grant stands, a compositor cannot light the
outputs until it is revoked; a crashed direct renderer therefore needs the
startup cleanup or the probe's revoke mode before Sway can present again.

### Where the direct path's CPU goes

Per-thread `/proc` sampling of the probe shows two different costs. About
4.8 s of kernel CPU accrues in the first five seconds, before any present:
the driver's display enumeration issues 163 `QUERY_DPY_DYNAMIC_DATA` calls of
up to 95 ms each, and perf attributes the busy time to one Resource Manager
function entered through `ioctl`. Steady-state presenting of four
1920×1080 heads then costs about 17% of one core in kernel time plus 2.5% in
user time. Per-ioctl timing during 7.4 s of presenting (442 batches) shows:

| Kernel call per present batch | Count | Total | Mean |
| --- | ---: | ---: | ---: |
| RM object allocation, class `0x0005` (`NV01_EVENT`) | 442 | 320 ms | 0.72 ms |
| RM object free (`NV_ESC_RM_FREE`) | 452 | 336 ms | 0.74 ms |
| `NVKMS_IOCTL_FLIP` (one per head) | 1,769 | 173 ms | 0.10 ms |

The allocate/free pair is issued once per `vkQueuePresentKHR`, not per
swapchain, so batching already amortizes it. It is unchanged without present
timing, without present IDs, and with three swapchain images instead of two,
so it is intrinsic to the driver's direct presentation and no application-side
change removes it.

The cost is a GSP artifact. With GSP firmware enabled (the 595 default on
Turing), each Resource Manager object allocation and free is a synchronous
round trip to the GPU's system processor. [System A](../developer/test-systems.md#system-a) was rebooted with
`NVreg_EnableGpuFirmware=0` (`/etc/modprobe.d/nvidia-gsp-off.conf`, retained;
the proprietary kernel module supports this on Turing, the open module does
not) and the same probe runs were repeated:

| Measure, four heads at 59.94 Hz | GSP on | GSP off |
| --- | ---: | ---: |
| Kernel time in ioctls per 7.4 s of presenting | 845 ms | 267 ms |
| RM allocation per batch, mean | 0.72 ms | 0.05 ms |
| RM free per batch, mean | 0.74 ms | 0.12 ms |
| `NVKMS_IOCTL_FLIP` per head, mean | 0.10 ms | 0.10 ms |
| Steady-state kernel CPU, one core | about 17% | about 4.6% |
| Steady-state user CPU, one core | about 2.5% | about 2.7% |
| Submitted batches per second | 56.9 | 57.0 |
| Probe teardown (sub-ownership) | 929 ms | 790 ms |

The one-off enumeration cost at startup is unchanged. These are standalone
probe measurements on one boot each; the full pipeline, and the Wayland path
on the same boot, still need the repeated matched runs the measurement
contract requires before any appliance-level claim. The boot also produced no
nvidia-drm flip warnings or timeouts across the sub-ownership runs.

One real-slicer direct run through the integration harness (four heads,
4500×2679 Seascape, 30 s, GSP off) confirmed the presenter change end to end:
the slicer logged the grant and the revoke, exited normally 0.7 s after its
run time (the earlier r2 runs needed about 13 s of teardown), the kernel log
stayed clean, and the appliance restored with 17/17 checks. Its second stats
interval reported 53.2 fps presented with 6 straddles; that is a single
unwarmed run and not a performance result.
