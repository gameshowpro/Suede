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

The timestamp semantics follow the
[Vulkan timestamp command specification](https://docs.vulkan.org/refpages/latest/refpages/source/vkCmdWriteTimestamp.html)
and the [Khronos timestamp-query example](https://docs.vulkan.org/samples/latest/samples/api/timestamp_queries/README.html).
