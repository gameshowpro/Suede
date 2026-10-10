# Projection GPU timing

The Frame timing panel separates device timestamp spans from host submission,
fence waits, and display feedback. The API exposes the same interval report at
`GET /api/v1/projection/stats` in `lastInterval.gpuTiming` and
`lastInterval.outputs[].commitToPresentMs`.

## What is measured

| Measurement | Boundaries and interpretation |
| --- | --- |
| GPU span | Vulkan TOP_OF_PIPE before source acquisition to BOTTOM_OF_PIPE after all output passes and source release. Elapsed device time, including stalls and preemption; not shader busy time. |
| Per-output GPU span | TOP before that output's target acquisition to BOTTOM after its release. Can include earlier outputs' pipeline backlog; overlapping spans must not be summed or treated as isolated pass costs. |
| Host submit | Wall time inside `vkQueueSubmit`. |
| Host completion | Submission entry to observing the existing fence's completion. Includes submit time, GPU time, queueing, and host wakeup. |
| Outside GPU span | Host completion minus the GPU span for the same sample. Residual time, not a direct measurement of queue waiting. |
| Calibrated queue delay | Submission entry to the first GPU timestamp, correlated to CLOCK_MONOTONIC. Includes driver/submission work and scheduling. |
| Completion observation | Final GPU timestamp to host fence observation, using that same clock correlation. |
| Request to present | Wayland feedback request immediately before surface commit to the protocol's displayed timestamp. Direct presentation starts immediately before the present request. Not capture-to-glass or photon latency. |

The existing `perFrameMs.gpu` field remains a **host fence wait**, averaged per
captured source frame, for compatibility. It overlaps `perFrameMs.blending`.
The new distributions instead report sampled submissions or displayed output
frames, with their own sample counts, mean, nearest-rank p95, and maximum.
Do not add the old and new durations together.

Device spans use the queue family's valid timestamp bits and the physical
device's `timestampPeriod`, including counter wrap handling. Correlated host
metrics additionally require `VK_KHR_calibrated_timestamps`, a 64-bit device
counter, and bracketing DEVICE/CLOCK_MONOTONIC calibration samples. Inconsistent
clock rates, out-of-range timestamps, and negative differences are rejected.
The reported calibration sample deviation is Vulkan's estimate for those
samples, not a total error bound or a guarantee against drift between them.
Wayland latency uses the clock announced by `wp_presentation`, not an assumed
clock or the local receipt time of the feedback event. If a compositor announces
`CLOCK_REALTIME`, wall-clock adjustments during an outstanding request can
distort or invalidate that sample.

Missing support or missing valid observations are reported as null, not zero.
An older slicer's absent fields also decode as null. A measured zero remains a
valid measurement. Presentation completions without timestamps still count
as presented, but do not create latency samples.

## Sampling and overhead controls

`SUEDE_GPU_TIMING` is a runtime diagnostic setting inherited by the slicer:

- `sampled` (default): timestamp one submission in 16.
- `full`: timestamp every submission for hardware experiments.
- `off`: omit GPU timestamp and host submission sampling.

The same binary supports all three modes; no test-only build is required for
metering. The implementation uses a fixed query pool, reads only after the
already-required projection fence completes, and uses query availability
without a `WAIT` flag. It adds no submission, idle operation, fence wait, or
extra synchronization barrier. Calibration capabilities are checked once.
Distributions retain the first 4,096 valid observations per reporting interval; the
query pool supports up to 64 output passes in a submission. Larger submissions
continue rendering and skip the sample with a query-failure count.

Sampling reduces timestamp, calibration, and readback overhead. A fixed
sampling cadence can alias a periodic workload, so use full metering for
comparative experiments. Device timestamps also affect scheduling enough
that their overhead must be measured on the target hardware. Display latency
bookkeeping remains active in `off`; that switch isolates the extra GPU meter,
not every existing or new statistics operation.

## Fixed-source output-count experiments

The diagnostic `SUEDE_TEST_OUTPUTS=DP-1,DP-2` selects which configured slices
Suede presents. It preserves the complete blend participant layout, source
canvas, and physical display configuration. It retains configured output order
and applies the same selection to live geometry updates. Empty, duplicate,
unknown output names and direct presentation are rejected. Leave this variable
unset outside controlled tests; it intentionally omits projection surfaces.

Compare one, two, and four selected outputs with identical source dimensions,
UE quality settings, camera tour, display modes, and input/presentation backend.
Keep all physical displays active so scanout load does not change. Record which
outputs were selected and whether unselected displays contain other content.
Use repeated, counterbalanced trials after warmup and compare unmetered,
sampled, and full metering on the same four-output workload. Keep timed runs
separate from validation layers, screenshots, and other diagnostic work.

Output-count differences measure incremental projection and compositor work
under that workload. GPU spans can still include contention with UE, and a
wall-clock telemetry window does not necessarily cover the same camera phases
as a frame-index-matched UE CSV range. Report those windows separately. Neither
higher engine FPS without Suede nor a smaller fence wait alone proves an
end-to-end improvement.

## GPU table memory

Projection storage buffers prefer coherent, CPU-mappable device-local memory
by default. This keeps frequently read per-pixel transfer tables close to the
GPU. The same policy applies to the small sync-pattern shape buffers. Image
uploads and GPU readback buffers keep their existing placement.

`SUEDE_GPU_TABLE_MEMORY` is a runtime diagnostic setting inherited by the slicer:

- `auto` (default): prefer mapped device-local storage, retain CPU table copies,
  and fall back to compatible coherent host-visible memory when necessary.
- `host`: restore the original first-compatible-host-memory policy and mapped
  table comparisons, without CPU copies, for diagnosis and baseline comparisons.

An invalid value logs a warning and uses `auto`. Restart the Suede service after
changing the setting; it is applied when the Vulkan device is created.

The preferred heap can be much smaller than total VRAM. Allocation or mapping
failures fall back to compatible coherent host-visible memory; device-loss errors
still propagate. Allocation logs report the actual memory type, heap, flags,
size, and fallback, so the policy alone does not prove local placement.
`fallback` reports a retry after an allocation or mapping failure; a device
with no eligible local type can select host memory directly with `fallback=false`.
The automatic policy retains packed CPU copies for table equality checks,
avoiding slow CPU reads of mapped device memory. This adds about 31.6 MiB of
live CPU table storage for four 1080p outputs. Table replacement temporarily
retains both old and new GPU allocations and CPU copies to preserve atomic
updates. On unified-memory GPUs, device-local memory may share physical memory
with the CPU. Performance gains and memory costs depend on the device; the
measurements below cover one discrete GPU.

The campaigns below used an earlier experimental binary whose equivalent
opt-in was `SUEDE_TEST_LOCAL_TABLES=1`. That experimental variable has been
replaced by the default automatic policy and the `host` diagnostic override.

The promoted default passed all seven offscreen rendering and table-update
tests with the setting unset, and again with `host`. It also passed the actual
heap-exhaustion test with the setting unset. All hardware runs enabled the
Khronos validation layer and reported no validation errors.

## Measurements on October 9, 2026

The baseline on [System A](test-systems.md#system-a) used Electric Dreams on
UE 5.8.3, a Quadro RTX 8000, and NVIDIA
615.71.09 with the open kernel module and GSP enabled. All four physical outputs
remained enabled at 1920x1080 and 59.939 Hz. The source stayed 1920x1143, with
the same fixed-step, 900-frame camera loop and plain Wayland input/output.
Unselected outputs showed static backgrounds. Results are specific to this
driver and workload configuration.

Each condition had three trials in counterbalanced order, with 60 seconds of
warmup and 60 seconds of wall-clock collection per trial. GPU numbers below
are sample-weighted means from complete reporting intervals. FPS numbers are
means of the three trial means. One sampled-mode trial overlapped a results
transfer and was excluded and repeated.

| Presented outputs | GPU span (ms) | Host completion (ms) | UE FPS, late cycle | Presented cycles/s |
| --- | --- | --- | --- | --- |
| 1 | 0.755 | 1.081 | 43.72 | 40.97 |
| 2 | 1.504 | 1.830 | 41.81 | 39.91 |
| 4 | 3.008 | 3.356 | 38.16 | 37.76 |

These output-count trials used full metering. Calibrated queue delay averaged
0.268, 0.270, and 0.283 ms respectively; completion observation averaged 0.058,
0.056, and 0.066 ms. All 16,201 GPU span samples were valid. The added device
span closely tracked output count while the time outside that span remained
roughly 0.3 ms. This identifies an output-dependent device cost, rather than
explaining the increase solely as waiting before execution.

The late UE CSV window contains frames 3600–4499, one complete camera cycle.
The originally selected frames 900–1799 gave 44.61, 42.29, and 38.88 FPS with
the same ordering. Both windows contained all 900 frames in every trial.
Presentation and GPU telemetry cover separate wall-clock intervals, so the
difference between engine and presented rates is not a matched-frame loss
measurement. Presented cycles/s is not multiplied by the number of heads.

### Metering overhead

The four-output workload used the same binary for all timing modes, with three
clean repetitions each:

| Meter mode | UE FPS, late cycle | Presented cycles/s | Slicer CPU (% of one core) |
| --- | --- | --- | --- |
| Off | 38.20 | 37.76 | 2.79 |
| Sampled, one in 16 | 38.31 | 37.90 | 2.80 |
| Full | 38.16 | 37.76 | 2.87 |

This experiment did not resolve an FPS penalty for sampling; the small rate
differences should not be interpreted as a benefit from enabling the meter.
Even full metering was close to off on this workload. Keep sampled metering in
production and expose it in the normal UI, with runtime `off` available for
diagnosis. A test-only build flag is unnecessary. This is evidence from one
GPU and workload, not a universal overhead bound; qualify other hardware and
high-frame-rate workloads separately. The comparison does not isolate the
always-enabled presentation-latency bookkeeping.

### Table placement and CPU comparisons

The device-span result suggested testing the transfer-table memory placement.
Four 1080p tables contain 33.18 MB of packed entries. This system selected system
memory (type 3, heap 1) by default; the experiment selected device-local mapped
memory (type 5, heap 2). The GPU's PCIe link was Gen 3 x16.

Moving the buffers alone was insufficient: the first candidate's packed-table
no-op comparison took about 1,002 ms, versus 126 ms with the original placement.
CPU reads of the device mapping made unchanged-table checks substantially worse.
The revised candidate keeps a CPU copy atomically with the GPU buffer. Means
from three optimized, idle hardware benchmark runs per condition were:

| Four-output host operation | Original placement (ms) | Local placement + CPU copy (ms) |
| --- | --- | --- |
| Fixed-table no-op | 637.10 | 4.78 |
| Fixed-table replacement | 25.01 | 19.64 |
| Packed-table no-op | 126.31 | 1.71 |
| Packed-table replacement | 10.62 | 17.13 |

The scene uses the fixed-table path. Fixed replacement reuses the packed CPU
buffer it already creates; borrowed packed replacement must make an extra copy,
which explains the observed tradeoff. These are table-operation timings, not
frame times or complete UI-edit latencies. Pixel-content assertions ran after
timing, and validation layers were disabled for these benchmarks.

Separate readback runs with the Khronos validation layer explicitly enabled
passed all seven tests in both modes, with no validation errors. They covered
144 shader cases (exact identity pixels, at most one byte of warped-channel
error), 864,000 dynamic pixels, cross-API no-ops, sparse outputs, and failed
replacement preserving both active table bytes and rendered pixels.

The final rendering comparison used the revised binary for both variants,
three paired repetitions, full metering, and the same 60-second warmup and
60-second measurement periods. Pair order alternated. Every trial verified
the actual memory type in allocation logs, with no fallback, and all timestamp
queries succeeded. Source and runtime fingerprints matched within each pair.

| Four-output measurement | Original placement | Local placement + CPU copy |
| --- | --- | --- |
| GPU span | 3.008 ms | 0.241 ms |
| Host submission-to-completion | 3.362 ms | 0.566 ms |
| Calibrated queue delay | 0.286 ms | 0.272 ms |
| Request to present, across output feedback samples | 11.776 ms | 11.541 ms |
| UE FPS, early cycle | 39.09 | 43.83 |
| UE FPS, late cycle | 38.27 | 43.06 |
| Presented cycles/s | 37.97 | 40.87 |

Mean paired improvements were **92.0% less GPU span**, **12.5% higher late-cycle
engine FPS**, and **7.6% more presented cycles/s**. The late engine gain ranged
from 12.3% to 12.6% across pairs; the presented-rate gain ranged from 7.1% to
8.3%. All six trials contained both complete 900-frame CSV windows, and the
early cycle also improved (12.2% mean paired gain). These are descriptive
results from three pairs, not a universal performance guarantee.

The nearly unchanged calibrated queue delay and much shorter device span
support transfer-table placement as a concrete bottleneck in this workload.
The change improves Suede's existing Wayland projection path. Request-to-present
latency changed only slightly, and remains distinct from capture-to-display
latency. A 92% reduction in projection span does not imply a 92% reduction in
the whole rendering pipeline.

Higher throughput also consumed more producer CPU: about 405% to 438% of one
core, while slicer CPU fell from 2.90% to 2.81%. GPU power stayed close to the
same level (254.1 to 255.7 W). The experiment demonstrates higher throughput,
not lower total machine resource use at a fixed frame rate.

The automatic policy adds CPU table storage, makes borrowed packed replacements
slower, and uses a small mapped device-local heap (246 MiB on this GPU), so
larger layouts can fall back to system memory. Allocation-failure fallback is
covered by unit tests and the hardware qualification below. Other GPU families
still need performance qualification; the measured speedup is not universal.
The campaign appliance was restored to its original idle configuration with
the experimental binary, default sampled metering, and storage and
output-selection overrides removed.

### Follow-up: output cost after local placement

A subsequent exploratory pass repeated the one-, two-, and four-output
conditions with local tables, one trial per condition in that order. The
binary, camera, source dimensions, physical scanout configuration, full
metering, and 60-second warmup/60-second collection recipe were unchanged.

| Presented outputs | GPU span (ms) | Host completion (ms) | UE FPS, late cycle | Presented cycles/s |
| --- | --- | --- | --- | --- |
| 1 | 0.065 | 0.385 | 45.66 | 42.41 |
| 2 | 0.123 | 0.458 | 44.56 | 41.96 |
| 4 | 0.239 | 0.571 | 43.14 | 41.04 |

The device-span increment from one to four outputs was 0.174 ms, versus
2.253 ms in the original system-memory baseline. Calibrated queue delay stayed
between 0.271 and 0.279 ms, and completion observation between 0.049 and
0.056 ms. No timestamp-query failures, superseded frames, stalls, or discarded
output frames occurred in the included collection intervals.

This supports transfer-table placement as the dominant measured projection
cost. It does not identify the remaining output-dependent engine cost:
the run order was not counterbalanced, GPU temperature rose during the pass,
and engine CSV and presentation telemetry still cover different windows.
Compositor work and capture scheduling remain candidates for a separate
experiment. Request-to-present latency was 10.11, 10.57, and 11.57 ms; this
elapsed display latency is not GPU processing time or evidence of an equivalent
amount of removable pipeline work.

### Follow-up: actual mapped-heap exhaustion

The ignored `local_table_fallback_under_small_heap_pressure` test qualified
the storage fallback on the same GPU, separately from timing campaigns and
with `VK_LAYER_KHRONOS_validation` enabled. It retained bounded production
buffer allocations, confirmed an actual `ERROR_OUT_OF_DEVICE_MEMORY` from
the preferred mapped-local type, and then replaced a 1024x1024 transfer table.
Allocation logs verified `fallback=true`, system-memory type 3 on heap 1,
and the complete 4 MiB replacement allocation. Exact shader pixels matched
both while the preferred heap was exhausted and after pressure was released.
No Vulkan validation errors were reported.

The test fails explicitly on unsupported layouts or drivers that permit
overcommit instead of producing the required exhaustion evidence. It covers
offscreen allocation, replacement, and rendering; it does not establish
compositor behavior or the performance of a large layout after fallback.

### Follow-up: matched 30 FPS resource use

Three four-output baseline/local pairs tested whether the shorter projection
span also reduces resource use at the same delivered rate. Pair order was
baseline/local, local/baseline, baseline/local. Both conditions used
`t.MaxFPS 30`, with `-UseFixedTimeStep` and `-fps=60` removed so the real-time
cap could operate. The frame-index camera loop still contained 900 frames.
These capped conditions are compared with each other, separately from the
uncapped fixed-step throughput trials.

Each trial used 60 seconds of warmup and 60 seconds of collection. The engine
window was frames 2700–3599, a complete later camera cycle; it is a separate
window from the wall-clock power and presentation telemetry. GPU timings pool
only complete reporting intervals inside collection. Other values below are
means of the three trial means.

| Four-output measurement at a 30 FPS cap | Original placement | Local placement + CPU copy |
| --- | --- | --- |
| UE FPS, later cycle | 29.998 | 29.999 |
| Presented cycles/s | 29.999 | 29.993 |
| GPU span | 3.009 ms | 0.235 ms |
| Host submission-to-completion | 3.276 ms | 0.493 ms |
| Calibrated queue delay | 0.165 ms | 0.164 ms |
| Request to present | 10.339 ms | 10.304 ms |
| GPU utilization | 69.16% | 60.36% |
| GPU board power | 232.25 W | 228.88 W |
| UE CPU, percent of one core | 332.06% | 331.72% |
| Slicer CPU, percent of one core | 2.921% | 2.661% |

The **8.8 percentage-point utilization reduction** is consistent with reducing
the measured device span by about 2.77 ms per submission, or roughly 83 ms per
second at 30 submissions/s. These spans include stalls, not just shader work.
Board power fell by only **3.37 W (about 1.4%)**. All three pairs
showed a reduction, ranging from 2.10 to 4.16 W (0.9% to 1.8%). Graphics clocks
were similar, averaging 1869 and 1873 MHz. This is a modest observed reduction
in total GPU board power, not a measurement of Suede-only power or whole-machine
energy savings. Producer CPU and display latency barely changed.

Every trial verified the requested storage type without fallback, used the
same binary and app arguments, and had zero timestamp-query failures. The
included presentation intervals had no superseded frames, stalls, or discarded
output frames. The original idle configuration and service environment were
restored afterward, with all 21 system checks passing.

The strongest supported improvement is to keep the frequently read transfer
tables in device-local memory and keep CPU-owned bytes for equality checks.
It recovers GPU headroom and increases uncapped throughput; it does not yield
an equally large power saving at a fixed rate. With four-output projection
below 0.25 ms, subsequent throughput investigations should isolate source
capture and compositor scheduling. The measured completion-observation delay
alone does not justify removing the fence that protects the Wayland handoff.

The timestamp semantics follow the
[Vulkan timestamp command specification](https://docs.vulkan.org/refpages/latest/refpages/source/vkCmdWriteTimestamp.html)
and the [Khronos timestamp-query example](https://docs.vulkan.org/samples/latest/samples/api/timestamp_queries/README.html).

### Parked experiment: capture scheduling and doubled source refresh (2026-10-09)

This section is the record of a parked line of work. It is kept so that the
idea is not retried from scratch: the hypothesis, what was tested, what was
measured, and why nothing shipped.

#### Hypothesis

With an uncapped Unreal Engine producer rendering about 44 FPS into a
1920x1143 source canvas, each of four 1080p outputs presented only about 41
updates/s. The projection GPU span was about 0.25 ms, while the wait from
arming a capture to its ready event was about 24 ms. The gap was therefore
suspected to be scheduling between producer, compositor capture, and
presentation rather than GPU cost. The candidates were:

- flush Wayland requests immediately after arming the next capture;
- copy every capture, or flush after each commit, or free-run the output gate;
- pin Sway's main thread to performance cores;
- raise the virtual (headless) source output's refresh above the physical
  59.939 Hz so the compositor would pick up new producer frames sooner, at
  2× and 4× and later 1.5× and 3×.

For the refresh change to be adopted it had to deliver distinct frames, not
merely more presentation cycles, and it had to not regress browser producers,
which are Suede's normal workload.

#### Tests

All trials ran on [System A](test-systems.md#system-a) (Quadro RTX 8000,
driver 615.71.09 open module, Sway 1.11) with the 1920x1143 source, four
1920x1080 outputs at 59.939 Hz, device-local tables, and full GPU timing.
Research builds were derived from commit `0bf8a45` with runtime-gated
selectors such as `SUEDE_TEST_SOURCE_RATE_MULTIPLIER`; the binaries, patches,
and harnesses stayed outside this repository. Every trial recorded per-output
presentation feedback, GPU telemetry, and for UE a CSV engine profile compared
on a matched later frame window. Each campaign saved and restored the
appliance's binary, configuration, and app state and requalified it with all
checks passing.

| Test | Workload | Design |
| --- | --- | --- |
| Early capture flush | UE uncapped | 3 counterbalanced pairs, 60 s warmup + 60 s collection |
| Scheduling screen | UE uncapped | 8 arms: 3 baselines, source 2×, source 4×, copy every capture, flush after commit, free-run; 30 s + 30 s |
| Sway affinity screen | UE uncapped | normal/P-core/P-core/normal, 30 s + 30 s |
| Source 2× confirmation | UE uncapped | 3 pairs, 60 s + 60 s |
| Source 2× at a 30 FPS cap | UE with `t.MaxFPS 30` | 2 pairs, 60 s + 60 s |
| Nearby rates | UE uncapped | 2× / 1.5× / 3× / 2×, 30 s + 30 s |
| Cross-workload | Chromium Seascape, Chromium test card, UE uncapped | 6 trials each in 1×/2×/2×/1×/1×/2× order, 60 s + 60 s |
| Frame uniqueness | all three workloads at 1× and 2× | diagnostic readback build; 24 consecutive presented frames on one output per condition |

The test card used `https://testpatterns.gameshow.pro/test-card/index.html`
with its frame counter as changing content; Chromium ran with `--mute-audio`
and no FPS overlay. UE ran Electric Dreams with `-ExecCmds=r.VSync 0,t.MaxFPS 0`,
`-UseFixedTimeStep`, and `-fps=60`. The frame-uniqueness build read back the
actual presentation image chosen by the blend pass, tied it to its surface
commit and presentation feedback, and compared exact RGB pixels; its readback
perturbs cadence, so it is not throughput evidence.

#### Results

Per-output presented updates/s unless stated. Changes are candidate versus
control.

| Test | Control | Candidate | Change |
| --- | ---: | ---: | ---: |
| Early capture flush | 40.541 | 40.323 | −0.54% (pairs −1.15, −0.65, +0.18%) |
| Screen, source 2× | 40.55–41.24 (3 baselines) | 42.80 | above the baseline range |
| Screen, source 4× | 40.55–41.24 | 40.77 | none; 21 superseded captures |
| Screen, other arms | 40.55–41.24 | 40.20–41.16 | none |
| Sway affinity | 41.995 / 40.917 | 40.535 / 40.945 | order-dependent, no gain |
| Source 2× confirmation | 40.863 | 42.576 | +4.19% (pairs +4.24, +4.11, +4.22%) |
| Source 2× at 30 FPS cap | 29.998 | 30.001 | none |
| Nearby rates, 1.5× / 3× | 2×: 43.04, 42.96 | 43.16 / 43.05 | within 0.5% |
| Cross-workload, Seascape | 59.93 | 54.78 | −8.60% (pairs −8.28, −8.64, −8.87%) |
| Cross-workload, test card | 59.09 | 58.42 | −1.13% (pairs −1.26, −0.94, −1.21%) |
| Cross-workload, UE | 40.87 | 43.01 | +5.24% (pairs +3.52, +6.07, +6.16%) |

Supporting observations:

- The UE gain was in presented updates only. Matched-window engine rate was
  43.32 versus 43.17 FPS, projection GPU span 0.242 ms at both rates, GPU
  utilization 96.7% at both, and board power 257.2 versus 257.5 W. There was
  no power saving at the 30 FPS cap either.
- At 2× the capture-ready wait shortened by about 1 ms (24.5 to 23.4 ms) and
  request-to-present latency was unchanged at about 11.5 ms.
- For Seascape at 2×, captured canvas frames fell from 59.9 to 56.2/s and
  superseded captures rose from 2 to 209. For the test card, captures rose
  from 59.1 to 65.5/s while presented updates fell, with superseded captures
  rising from 6 to 1,055. Chromium paces its rendering to the output refresh,
  so a 120 Hz virtual output asks it for more work; the exact mechanism of the
  presented-rate loss was not identified.
- Frame uniqueness: every condition yielded 24 of 24 distinct frames with no
  adjacent duplicates, consecutive commit sequence numbers, and 24 zero-copy
  `presented` outcomes. Seascape and UE changed across the whole picture
  between frames; the test card changed only in its top-right counter, which
  advanced on every presented frame. Under the instrumented cadence, 2×
  skipped more source captures than 1× (UE: 11 skips in 23 pairs versus 1),
  and pairs spanning a skip showed about 1.7 times the pixel delta, consistent
  with one missing fixed-step engine frame.
- No arm produced discarded output frames, stalls, or timestamp-query
  failures. Across the source-refresh work, 26 timed trials and 1,080 GPU
  telemetry samples used the same GPU, driver, and GSP state.

#### Conclusions

- **Doubling the virtual source refresh is a real but small gain for one
  case.** Uncapped UE over Wayland capture gained a repeatable 4–5% in
  presented updates, and those updates were distinct frames. It did not raise
  engine throughput, reduce projection GPU work, or save power. A capped
  producer gained nothing.
- **It regresses the normal workload.** Both Chromium pages lost presented
  updates, the heavier page by almost 9%. It cannot be a default.
- **It is not worth shipping as an option.** About two extra frames per second
  for one producer type, with an unexplained mechanism, does not justify a
  config surface and a headless mode change on app switch. The native
  texture-sharing work for UE bypasses screencopy and would make this effect
  moot.
- **The other scheduling candidates failed.** Early flushing, copying every
  capture, flushing after commit, free-running the gate, and core pinning
  showed no gain. The 24 ms capture wait against a 0.25 ms GPU span remains
  unexplained; it includes source availability and event scheduling and is not
  a measurement of compositor GPU cost.
- **No production default or option changed.** Reopen only if an uncapped UE
  producer over Wayland capture becomes a shipping configuration that texture
  sharing has not superseded. In that case test 1.5× first, since it matched 2×
  on UE and should cost browsers less, and add producer-frame identity to the
  counters before measuring further.

### Raspberry Pi shared-memory qualification (2026-10-09)

On [System C](test-systems.md#system-c), the Pi 5's V3DV driver (Mesa
26.2.0, Vulkan 1.3.354) exposed one 4 GiB Vulkan heap and one memory type:
`DEVICE_LOCAL | HOST_VISIBLE | HOST_COHERENT`. Both policies selected type 0
on heap 0 without fallback. The default therefore works directly on this
unified-memory device; it does not fail over from dedicated GPU memory to
CPU memory. There is no second memory heap to rescue an exhausted allocation.

Release-mode table benchmarks used three counterbalanced pairs, separately
from presentation trials, after compilation finished. The same binary ran with
`SUEDE_GPU_TABLE_MEMORY` unset or set to `host`. Each operation updated four
1920x1080 tables; these measure host elapsed time for offscreen updates,
not four-display Pi playback. Values below are means of the three runs.

| Four-output table operation | Host policy | Default policy |
| --- | --- | --- |
| Fixed-table initial installation | 44.94 ms | 42.16 ms |
| Fixed-table unchanged-content check | 198.51 ms | 12.73 ms |
| Fixed-table replacement | 46.76 ms | 52.59 ms |
| Packed-table initial installation | 19.38 ms | 37.16 ms |
| Packed-table unchanged-content check | 32.23 ms | 8.37 ms |
| Packed-table replacement | 22.56 ms | 33.55 ms |

CPU copies made the production fixed-table equality check about **15.6 times
faster**, with a roughly 5.8 ms replacement penalty in this benchmark. Packed
updates also had faster equality checks, but initial installation and replacement
paid for copying the bytes. Four outputs retained 31.6 MiB of CPU shadow data;
two outputs retain 15.8 MiB. These are logical allocation sizes, not predicted
changes in RSS or proportional set size (PSS), which also depend on allocator
retention and shared mappings.

The presentation campaign kept a 3740x1080 source canvas and two 1920x1080
outputs at 60 Hz, with a 100-pixel logical overlap. It used a frozen Mystify
fixture with seeded initial geometry and inline canvas styles; the external CEF
overlay stylesheet was omitted. Chromium 151 and Sway 1.10.1 were unchanged.
The existing direct-scanout-disable setting stayed in effect. Automatic output
realignment was disabled for every trial so it could not restart the slicer.

Three pairs ran in host/default, default/host, host/default order using build
`b0bb768-local-tables-pi`, full GPU timing, 30 seconds of warmup, and six complete
reporting intervals per trial. Compilation, validation layers, and screenshots
were excluded from these windows. FPS is weighted by interval duration; GPU
spans are weighted by valid sample count. Process CPU and memory were sampled
throughout the surrounding collection window, including the wait for a complete
first interval.

| Two-output Mystify measurement | Host policy | Default policy |
| --- | --- | --- |
| Captured canvas frames/s | 9.033 | 8.996 |
| Presented cycles/s | 5.547 | 5.528 |
| Projection GPU span | 24.096 ms | 23.792 ms |
| Host submission-to-completion | 24.238 ms | 23.927 ms |
| Slicer CPU, percent of one core | 0.369% | 0.358% |
| Sway CPU, percent of one core | 97.07% | 96.95% |
| Slicer PSS | 54.16 MiB | 62.23 MiB |

There was no consistent playback benefit: paired presented-rate changes were
-0.66%, +0.33%, and -0.69%. GPU-span changes also reversed direction between
pairs. All 2,005 timestamp samples were valid, and neither policy had a stall
or discarded output frame in the included intervals. Superseded frames remained
similar (631 host, 629 default), as did presentation gate holds (848 and 852).
No thermal throttling was reported; peak recorded temperature was 71.4 degrees C.
Calibrated queue-delay measurements were unavailable on this driver.

Sway consumed nearly one CPU core while the slicer consumed less than 0.4%.
Together with the much longer presentation cycle than projection GPU span,
this makes compositor and source-capture work a useful next profiling target.
These measurements do not identify a particular Sway function as the bottleneck.
Early online Seascape and Mystify readiness runs overlapped compilation and
were excluded; their long waits are not a controlled workload or metering result.

The Pi results support keeping the default without a Pi-specific allocation
rule. The benefit here is faster table comparisons, with additional memory and
some replacement cost; this does not reproduce the discrete GPU's placement
speedup. `SUEDE_GPU_TABLE_MEMORY=host` remains available for memory-constrained
installations or workloads dominated by frequent packed-table replacements.

The offscreen suite ran against the hardware V3DV driver with Khronos validation
enabled under both policies. Six of seven tests passed, including the new atomic
replacement, no-op, rollback, and shader-visible table checks. The existing
`shipped_shader_matches_reference_pixels` test stopped at the same one-level
RGB mismatch under both policies: case 1, RGBA source and target, pixel 48,
green channel, actual 1 versus expected 0. No Vulkan validation errors occurred.
This remains an unresolved exact-pixel parity issue; these results do not claim
that the entire cross-driver pixel suite passed. The separate-small-heap
exhaustion test is not applicable to this single-heap device.

The original installed binary, saved configuration, and active test-card app were
restored and verified afterward. The original 15 passing checks and two warnings
(direct scanout and output phase) remained, with no failed checks. The temporary
fixture server was stopped. Raw campaign, qualification, and restoration evidence
is retained under `/var/tmp/suede-pi-tables-20261009` on the test appliance.
