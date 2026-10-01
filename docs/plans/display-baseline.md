# Direct-display baseline and implementation sequence

The current Wayland path is the control arm. Capture it before installing a
changed Suede binary, changing output modes, or changing compositor launch
settings. Keep the raw artifacts private under `research/`; they contain site
configuration and application URLs.

## Measurement contract

Use `scripts/profile-display.py` on the appliance for each workload (Python
3.8 or newer). For example, after copying the script to [System A](../developer/test-systems.md#system-a):

```sh
python3 /tmp/profile-display.py --label W-sync --duration 60 \
  --output /tmp/suede-baseline/W-sync.json
```

Use a private output directory and copy results back before rebooting. The
collector does not activate workloads; apply temporary full-document previews
through the API, then revert them. Refuse to replace an existing unsaved working
copy and use revision/generation/epoch preconditions so another operator's edit
cannot be overwritten. Save the original document before the first edit; check
that it is restored exactly on success or interruption.

The collector reads telemetry without changing configuration. Record the binary SHA-256 and build
ID, kernel, GPU/driver, output roster and exact modes, canvas size, geometry,
blend settings, active app, renderer/priority startup line, bootstrap settings,
and the running compositor's `WLR_*` environment. Preserve the original
configuration before any workload change. Never use package version alone to
identify the binary: test appliances may have a hand-installed build.

Run these workloads with identical settings in both arms:

| Workload | Purpose |
| --- | --- |
| As-found static content, when actually static | Damage-driven idle cost; absence of fresh frame intervals is expected and must not become a fake zero-FPS failure |
| `sync`, with a light source app | Presentation pacing, CPU cost, per-output phase and lag |
| `sync`, with Seascape rendering behind it | Presentation pacing under GPU contention |
| Seascape without a pattern | Complete capture, warp, blend, and presentation path under load |

Use the [post-reboot Seascape resolution sweep](display-baseline-results.md#seascape-resolution-sweep)
to select demanding cases on [System A](../developer/test-systems.md#system-a): 4000×2381 near the refresh-rate limit,
4500×2679 for the onset of sustained frame loss, and 6000×3571 for heavier
overload, all at the saved 1.68 canvas aspect with physical output modes
unchanged. Keep 3200×1905 as a lighter control. The saved sweep uses the
installed Suede binary on NVIDIA 595.91.07; these are measured workloads, not
recommended production resolutions. Use the same Seascape page version and
repeat runs because its animated scene and system conditions vary.

Keep the same content URL and assets, browser version, canvas resolution,
output modes, geometry, and power settings. The first pass uses the current
configured canvas; a larger stress canvas is a separate experiment, never
silently substituted into one arm. Use a 30-second warmup and at least 60
seconds of measurements per workload. For a claimed performance improvement,
repeat each arm three times, alternating W/D/W where practical. A short initial
run establishes feasibility, not confidence intervals.

Sample CPU from `/proc/<pid>/stat` tick differences over monotonic elapsed time,
including process start time in identity. Report percent of one CPU core
(100% = one core); do not use lifetime-average `ps %cpu`. Re-enumerate processes
so a restart cannot silently reuse the old PID. Collect sway, slicer, daemon,
and browser totals separately. Missing counters and failed GPU probes are
missing data, not zeros. Sample GPU utilization, clocks, power, and temperature
so thermal or clock differences are visible. Profile sway stacks separately
from the clean timing run if attribution of the EGL spin is needed.

Retain the complete stats JSON with each fresh `lastInterval.measuredAt`, plus
wall-clock collection times and monotonic CPU window bounds. The API reports
roughly ten-second intervals; polling every second does not create ten
independent samples. Do not claim CPU and GPU sample windows exactly match
those reports. Compare:

- `canvasFps`, `presentedFps`, `perFrameMs`, stalls and superseded frames.
- Straddles per settled frame, gate holds per presentation cycle, and raw counts.
- Per-output presented/discarded counts, `phaseMs`, `lagFrames`, and refresh.
- Zero-copy ratio only when presented count is positive, only for Wayland.
- Sway, slicer, browser, and total CPU, GPU utilization and power.

Treat reduced CPU/GPU load as a backend acceptance gate. Compare Wayland and
direct-display runs on the same driver, kernel, workload, resolution, output
modes/geometry, and power configuration, with equivalent FPS, synchronization,
and latency results. Repeat comparable runs (three per arm, alternating
W/D/W where practical). Compare total relevant process CPU; a reduction in
Sway CPU must not merely shift equivalent cost into the slicer. Compare GPU
work or time when available. If only utilization is available, label it as a
utilization proxy, not a measurement of GPU work. Keep driver-upgrade gains
separate from backend gains by comparing both backends on the same driver.

For overloaded cases, also compare achieved frame rate at identical rendering
workload. A faster backend may raise utilization by producing more frames;
report that throughput gain separately from reduced overhead at a matched
frame rate. The collector's `canvasFps` is the capture rate, not Chrome's own
rendered-frame counter. Its `perFrameMs.gpu` is host time waiting for a Vulkan
fence, not GPU timestamp execution time. Neither metric alone attributes a
bottleneck to the browser's shader or establishes device-side GPU savings.

A presentation backend must identify its timestamp source and limitations.
Submission rate is not presentation rate. Vblank wakeup time is not a
per-image presentation timestamp. Missing timing cannot be reported as zero
phase, zero straddles, or a passed synchronization gate.

## Corrected architecture assumptions

The original proposal conflates two ownership models. With Vulkan display
swapchains, the Vulkan presentation engine owns mode changes and presentation;
the application does not issue those KMS atomic commits itself. Multiple
swapchains may be submitted together, but cross-display atomicity is supported
only where the implementation provides it. A common GPU clock is not proof of
phase alignment. See [VK_KHR_swapchain, issue 8](https://docs.vulkan.org/refpages/latest/refpages/source/VK_KHR_swapchain.html)
and [vkQueuePresentKHR](https://docs.vulkan.org/refpages/latest/refpages/source/vkQueuePresentKHR.html).

[VK_EXT_display_control](https://docs.vulkan.org/refpages/latest/refpages/source/VK_EXT_display_control.html)
provides fences/counters for display events. It does not supply the current
per-image timestamp/sequence feedback contract. Reading a Vulkan driver's DRM
event FD concurrently is not a portable fallback and may consume events owned
by the driver. Validate a supported timing extension and its actual behavior,
or choose an application-owned KMS path with exported Vulkan buffers instead.
[VK_GOOGLE_display_timing](https://docs.vulkan.org/refpages/latest/refpages/source/VK_GOOGLE_display_timing.html)
is one possible source of actual presentation timestamps, where available.

`vkAcquireDrmDisplayEXT` requires DRM master on the matching primary device and
requires the FD to remain open until the display is released. It does not
provide permission acquisition. Resolve seat ownership and session lifecycle
before choosing packaging privileges; do not add broad capabilities just to
make a prototype run. See [vkAcquireDrmDisplayEXT](https://docs.vulkan.org/refpages/latest/refpages/source/vkAcquireDrmDisplayEXT.html).

## Work sequence and delegation

[System A](../developer/test-systems.md#system-a) is the primary test workhorse; [System B](../developer/test-systems.md#system-b) is the production analog used
for confirmation. Run tests on [System A](../developer/test-systems.md#system-a) first unless a documented hardware
difference confounds the result.

1. **Baseline tooling and runs** — `gpt-6-luna` implements the bounded read-only
   collector; main agent reviews CPU math, freshness, identity, and runs it on
   [System A](../developer/test-systems.md#system-a). Preserve the original binary/configuration and restore workload edits.
2. **Gate A: capture** — main agent first tests an isolated headless-only sway,
   Chromium, and the current GPU slicer. Then validate daemon readiness,
   watchdog, and canvas planning in a headless deployment. An isolated capture
   pass alone is not the complete gate.
3. **Gate B: Vulkan presentation** — `gpt-6-sol` implements a standalone example
   outside production source. Main agent reviews Vulkan lifetime/synchronization
   and runs one/four-connector probes, normal teardown, termination, and recovery.
   Extension enumeration alone does not pass this gate. Measure actual timing
   and confirm that the backend can preserve the stats/gating contract.
   On [System A](../developer/test-systems.md#system-a), first preserve the installed old-driver baseline and complete the
   `VK_NV_present_barrier` device-feature and per-surface capability/behavior
   probes. Then upgrade [System A](../developer/test-systems.md#system-a) to a compatible supported driver, test
   `VK_EXT_present_timing` including supported surface stages and clock domains,
   and repeat the same Wayland baseline on that driver. Only after these [System A](../developer/test-systems.md#system-a)
   results, confirm relevant capability and presentation results on [System B](../developer/test-systems.md#system-b).
   See the [environment follow-up](display-baseline-results.md#environment-follow-up).
   These driver experiments are now recorded: 595 supplies usable timing,
   but the barrier fails on both drivers. Startup synchronization and
   production recovery still need a validated design on [System A](../developer/test-systems.md#system-a).
4. **Architecture decision** — main agent chooses Vulkan WSI or explicit KMS
   based on evidence. Define ownership, timestamp source, output inventory,
   session permissions, and recovery before modifying the rendering loop.
5. **Presentation seam** — delegate a narrowly scoped refactor to `gpt-6-sol`,
   keeping Wayland default and settle/gate math backend-neutral. Main agent
   reviews and checks hardware parity against the saved baseline.
6. **Backend integration** — main agent owns GPU image lifetime, pacing,
   reconciler changes, seat ownership, boot transitions, and recovery. Delegate
   contained configuration/reporting and health-check changes to `gpt-6-luna`
   once those interfaces are fixed. A Wayland fallback after headless boot must
   actually restore a DRM-backed compositor; changing an enum is insufficient.
7. **Acceptance** — complete the workload matrix, cold start, hotplug, and
   slicer kill/recovery on [System A](../developer/test-systems.md#system-a) first, then confirm on [System B](../developer/test-systems.md#system-b). Validate V3DV before claiming support;
   retain Wayland default until the hardware gates pass. Record recovery time
   and missing capabilities explicitly.

The historical plan's Sonnet/Opus labels describe task difficulty, not models
available in this session. Delegation above uses available models and keeps
architecture and hardware integration with the main agent.

## Standalone display probe

Build with `cargo build --example display_probe`. The default Linux run
inventories displays and reports extension and device-feature support without
presenting; per-surface checks run with presentation. Inventory requires an
explicit primary DRM node:

```sh
./target/debug/examples/display_probe --card /dev/dri/card1
```

Presentation additionally requires `--present --connectors 129,133,137,141
--width 1920 --height 1080 --refresh-millihz 59940 --seconds 60`; these IDs and
mode are examples, not portable defaults. Discover connector IDs from sysfs and
select an exact mode from the inventory. The caller must arrange a display
outage, free DRM master, and install independent timed session recovery before
running it. The probe never stops services or grants itself privileges.

Add `--present-barrier` to explicitly enable `VK_NV_present_barrier`, or
`--present-timing` to explicitly enable `VK_EXT_present_timing` feedback; both
options require `--present`. The probe reports extension and device-feature
availability, then checks support on every selected display surface. The
timing option also requires `VK_KHR_present_id2`,
`VK_KHR_calibrated_timestamps`, their required device features, and present
wait/ID support for final completion. Surface capability output includes
supported timing stages and time domains. A requested feature that is absent
on the device or a selected surface fails the probe.

The example reports batched submission rates and per-connector errors. Where
both present-wait and present-ID features are supported, it waits for the final
present before normal teardown. An unverified final presentation exits without
explicitly destroying potentially live Vulkan presentation resources, with an
error diagnostic. With `--present-timing`, it also logs each returned timing
record with connector, present ID, stage, time, domain, domain ID, and completion
status, then polls for up to one second after final-present completion to drain
feedback. Missing records after that deadline are reported as incomplete.
Complete nonzero timestamps returned in `CLOCK_MONOTONIC` are emitted directly
as comparable cross-output monotonic nanoseconds. Other domains require a
validated mapping; zero timestamps, absent records, and incomplete series
remain unavailable. The probe also samples
`DEVICE` and `CLOCK_MONOTONIC` together at startup, about every 250 ms during
presentation, and at the end. A DEVICE-domain present timestamp can be mapped
only when `timestampPeriod` is exactly 1 ns per tick and an adjacent pair of
calibration samples brackets it. The pair is accepted only when its wrapped
DEVICE span is unambiguous, monotonic time does not go backwards, and the
scaled interval mismatch is at most twice the larger reported deviation plus
2 ns. Other periods, zero/out-of-range timestamps, and pairs failing those guards leave
the mapping unavailable. The result is an estimate: reported deviations and
the interval check are not strict error bounds, and neither mapped timestamps
nor small per-ID spans prove physical scanout phase or wall coherence. Raw
records do not calculate Suede's production per-output phase, lag,
settled-frame straddles, or presentation-gate results. Successful rendering
alone does not satisfy the production gate.

On normal teardown, the probe releases acquired displays while the DRM card
FD is still open, closes that FD before destroying the Vulkan instance, and
keeps the Vulkan loader alive until after instance destruction. This follows
the display-acquisition FD lifetime and avoids unloading the loader before
Vulkan cleanup. OS process exit and successful compositor restart still need
a hardware recovery test.

Run a separate targeted correctness pass with
`VK_INSTANCE_LAYERS=VK_LAYER_KHRONOS_validation` when the layer is installed.
Exercise extension enablement, surface queries, swapchain presentation, and
teardown there; collect performance measurements in a run without the
validation layer. The timing FFI shim follows Vulkan-Headers 1.4.351 because
ash 0.38 does not yet expose these extension bindings.
