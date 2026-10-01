# Selectable display presentation component

Status: the direct-display research spike is integrated into the real slicer
pipeline behind an explicit experimental selector. Single- and four-output
Vulkan validation runs on [System A](../developer/test-systems.md#system-a) pass; matched performance runs and the
unresolved NVIDIA teardown behavior are recorded in the
[component results](display-component-results.md). The daemon still launches
the existing Wayland path by default. The direct arm did not meet the measured
throughput or CPU/GPU headroom criteria on [System A](../developer/test-systems.md#system-a).
The earlier [Seascape sweep](display-baseline-results.md#seascape-resolution-sweep)
is exclusively the Wayland baseline, not a direct-display result.

## Shared pipeline

Both paths consume the same `SlicerSpec` (`src/projection/blend.rs`),
headless canvas capture, transfer tables, warp updates, adaptive black lift,
and blend shaders. `Gpu` (`src/projection/gpu.rs`) renders through a
common target interface:

- Wayland targets are exported dmabuf images, with foreign queue ownership
  transfers and compositor buffer-release tracking.
- Direct targets are acquired Vulkan display swapchain images, with acquisition
  semaphores, color-attachment transitions, and transitions to presentation.

Direct mode creates one Vulkan device for capture buffers, shader resources,
and swapchains. It introduces no intermediate blit or second GPU context for
presentation. The display component (`src/projection/gpu/display.rs`)
selects the explicit DRM card and matching physical device, validates modes
and surface capabilities, and batches content and sync-pattern presentations.
Wayland resources are separated from the common presenter state in
`slicer.rs` (`src/projection/slicer.rs`).

Both paths retain the same presentation gate and timing calculations. The
direct component associates feedback with the submitted snapshot and warp
revision; late feedback cannot acknowledge a newer revision. Completion can
open the gate even when its timestamp is missing. Missing timestamps contribute
no synthetic phase or offset measurements. Direct display does not populate
Wayland's zero-copy flag; interpret that counter only on the Wayland arm.

The first direct implementation requires `VK_EXT_present_timing`, present ID,
and present-wait support. It requests a scanout/visible stage, validates the
returned domain and stage, and maps device-clock timestamps only through
accepted adjacent calibration samples. The timestamp source is labeled as an
estimate. Batched presentation does not guarantee atomic multi-display flips;
`VK_NV_present_barrier` remains disabled: NVIDIA implements it with an NVKMS
swap group plus framelock attributes that need Quadro Sync hardware, and the
DRM-acquired display path lacks the NVKMS permission for either; see the
[root cause](display-baseline-results.md#present-barrier-root-cause). The
presenter does grant itself the missing NVKMS sub-ownership, which removes the
nvidia-drm flip warnings and the per-head three-second teardown timeouts.

Render-complete semaphores belong to swapchain images and are reused only
following acquisition. Normal shutdown verifies final presentation before
releasing displays and closing the DRM FD. If completion cannot be verified,
the component retains potentially live resources until process exit instead
of destroying them. Direct acquisition and explicit GPU/presentation waits have
timeouts. Driver calls and startup Wayland roundtrips can still stall, so the
CLI duration is not a hard watchdog; hardware experiments also use independent
session recovery.

## Explicit selection

The internal slice command accepts a JSON file selecting direct presentation:

```sh
suede slice --spec "$SLICE_SPEC" --presentation-config direct.json \
  --run-for-seconds 110
```

Omit `--presentation-config` for Wayland. The optional duration ends a bounded
experiment through normal cleanup. Without it the component runs until its
normal lifecycle ends. The GPU renderer is required for direct display; an
unavailable GPU or requested capability is an error, not a silent CPU fallback.

Example `direct.json`:

```json
{
  "card": "/dev/dri/card1",
  "outputs": [
    {
      "name": "DP-1",
      "connector_id": 129,
      "width": 1920,
      "height": 1080,
      "refresh_millihz": 59940
    }
  ]
}
```

Discover the actual card, connector IDs, and exact advertised Vulkan modes on
each machine. Entries must match the slice names and order. These example
values are not portable defaults. Unknown fields and invalid JSON are rejected.
The caller must arrange headless Sway for capture and make DRM master available;
the component never stops services or grants itself permissions.

For targeted diagnosis, `SUEDE_DIRECT_PROFILE=1` records aggregate wall and
calling-thread CPU time for acquisition, presentation, feedback, calibration,
and shutdown. It emits one JSON report on stderr after successful cleanup.
Profiling is disabled by default and performs no timing reads in that mode.

Stats add optional `presentationBackend` and `timestampSource` fields. Older
reports without these fields still deserialize. `presentedFps` retains its
existing meaning of submission cycles per second; per-output `presented`
counts come from presentation feedback. Do not equate submission rate with
physical presentation or Chrome's rendered frame rate.

For an isolated component test, `scripts/profile-display.py` can read
`--stats-file slice.jsonl --metadata-file metadata.json` while sampling live
CPU/GPU counters. It labels saved configuration metadata as a snapshot, does
not infer process liveness from the file, and excludes a stale initial stats
interval. Both comparison arms must use the same session method; daemon CPU
is absent in an experiment that stops the daemon in both arms.

## Remaining deployment work and acceptance

Status update: the appliance-level integration this section originally called
for is now built, behind `presentation = "direct"` in `suede.toml` (with
`allow_overlaps = true`), still experimental and still off by default. The
daemon reads a live output inventory itself (`src/drm_inventory.rs`: EDID
identity and exact mode timings from sysfs and `DRM_IOCTL_MODE_GETCONNECTOR`,
with no prior Wayland boot needed), owns the physical outputs through a
`SwayClient` wrapper (`src/sway/direct.rs`) so the reconciler and planner need
no direct-mode branch of their own, and the login profile plus the daemon
(`src/presentation.rs`) share a session lifecycle: resolution against the
compositor actually found, an idle black wall so the displays stay owned and
the console never shows through, confirmation from the first stats interval,
automatic fallback to a DRM-backed Wayland compositor on a refused preflight
or repeated slicer exits, and `suede display-reset` to clear any NVKMS
sub-ownership grant a crash left behind. The health checks, `/outputs` and
`/system` all understand the mode. See [VK_KHR direct
display](../developer/vk-khr.md) for the full session lifecycle, its limits,
and the crash-recovery runbook, and
[deployment](../developer/deployment.md#the-login-profile-block) for the
provisioning side.

What is still out of scope, deliberately — see [VK_KHR direct
display](../developer/vk-khr.md#limits-experimental) for the complete list:
`auto` selection or an API/UI switch; hotplug, mode/scale/transform changes,
adaptive sync, tearing or backgrounds on an owned output; EDID
re-verification after startup; more than one GPU or DRM card; present
barrier, flip lock, Quadro Sync or a phase guarantee; a fallback with no dark
gap, or stall detection for a session that is alive but not presenting; new
privileges or a systemd unit for Sway; `postinst` re-provisioning an existing
appliance, or a `provision.sh` flag. Non-NVIDIA drivers are allowed but
untested.

Out-of-date or suboptimal swapchains still fail for caller-managed restart;
in-place hotplug/mode recreation is not implemented. Static diagnostic patterns
with per-output source labels currently present each output separately;
real content and the animated sync pattern use a multi-display batch.

On this hardware, direct still does not meet a throughput or CPU/GPU headroom
bar over Wayland — see the [conclusions in VK_KHR direct
display](../developer/vk-khr.md#conclusions-direct-vulkan-path-vs-swaywayland).
Keep both presentation components until evidence supports retaining or
removing either one. Use the
[measurement contract](display-baseline.md#measurement-contract) for any
further comparison, including repeated 4500×2679 overload runs and a
matched-frame-rate lower-load case; report
throughput separately from CPU/GPU overhead, include thermal and clock
conditions, and preserve timing gaps. [System B](../developer/test-systems.md#system-b)'s driver 550 is expected to
fall back at preflight (it lacks `VK_EXT_present_timing`), so further
measurement there needs a newer driver first.
