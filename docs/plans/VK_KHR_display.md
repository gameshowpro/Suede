# Direct-to-Display Presentation via `VK_KHR_display`

**Status:** Proposed architectural plan. Gate 0 (research spike) is blocking — no implementation slice below should begin until both spikes pass. Capability survey completed 2026-09-18.

---

## 1. Motivation and Problem Statement

Today the slicer blends into DMA-BUF, attaches one `wlr-layer-shell` surface per projector, and lets Sway commit. That works and ships in production. However, it incurs specific architectural overheads:

- **Sway NVIDIA EGL Spin**: 0.34–0.41 cores on test rigs with scanout enabled, and up to 64% of a CPU core under load on an RTX A1000. This route bypasses compositor presentation overhead entirely rather than diagnosing driver-specific spin loops.
- **Direct Scanout is a Tuning Flag, Not an Invariant**: `allow_overlaps`, the `WLR_SCENE_DISABLE_DIRECT_SCANOUT` override, and `zeroCopyPresented` monitoring all exist to verify whether the zero-copy scanout path engaged. Directly owning the DRM commit eliminates this ambiguity.
- **Inferred vs. Authoritative Timing**: Suede currently reads flips through `wp_presentation` feedback. Direct presentation provides DRM vblank events directly.
- **Proactive Head Phase Alignment**: The `output-phase` health check and its blank-the-wall fix exist because Sway mode-sets heads in separate commits unless IPC commands are batched. A single multi-CRTC atomic commit aligns them by construction, every time.

**Strategic Benefit**: `VK_KHR_display` is cross-vendor. It avoids vendor lock-in and is confirmed present on both NVIDIA proprietary drivers and Mesa/V3DV.

---

## 2. Scope Boundaries

- **Not a warp change**: The homography, edge-blend ramps, black lift, and their shaders remain untouched. Only the blend pipeline's *output presentation target* moves.
- **Not a capture change**: Chromium still runs under Sway on a headless canvas output, captured over `zwlr_screencopy`/`ext-image-copy-capture-v1`.
- **Not a present-barrier project**: Multi-CRTC atomic commits handle single-GPU sync by construction. Present barrier extensions (`VK_NV_present_barrier`) remain an optional future consideration for multi-GPU topologies.

---

## 3. Capability Survey Findings

Hardware surveys (`vulkaninfo`) on target hardware platforms show:

- The complete acquisition chain — `VK_KHR_display`, `VK_KHR_get_display_properties2`, `VK_EXT_acquire_drm_display`, `VK_EXT_direct_mode_display`, and standard `VK_KHR_swapchain` — is present across NVIDIA and Mesa drivers.
- `VK_KHR_display_swapchain` is absent and **not needed**: standard `VK_KHR_swapchain` per connector surface is sufficient.
- `VK_EXT_display_control` (Vulkan vblank events) is available on NVIDIA, while Mesa/V3DV on Raspberry Pi 5 can drive timing directly from DRM page-flip completion events.
- Multi-head phase spread holds within a tight band on single GPUs, making atomic multi-CRTC commits effective for multi-projector synchronization.

---

## 4. Gate 0 — Research Spikes (Blocking)

Two validation spikes must pass before any implementation begins. Neither touches production source code.

### Spike A — Headless Sway Capture Pipeline
- **Objective**: Bring Sway up with `WLR_BACKENDS=headless` only, create one canvas-sized headless output, and confirm:
  1. Chromium launches and renders to the headless output.
  2. Suede watchdog, readiness, and spanning behaviors remain unchanged.
  3. `zwlr_screencopy` reliably yields DMA-BUF buffers at the canvas refresh rate.
- **Acceptance Criteria**: Capture flows at canvas rate with no DRM backend present; machine is cleanly restored.

### Spike B — Display Path on Proprietary Drivers
- **Objective**: Standalone prototype binary taking DRM master on the GPU card node, acquiring connectors via `VK_EXT_acquire_drm_display`, building `VkSurfaceKHR`, and presenting to standard `VK_KHR_swapchain` instances at native refresh.
- **Acceptance Criteria**: Four connectors present simultaneously at full display refresh rate with clean teardown and error recovery upon exit/termination.

---

## 5. Implementation Slices

### Slice 1 — Presentation Abstraction Trait
- **Components**: `src/projection/slicer.rs`, `src/projection/gpu.rs`.
- **Scope**: Extract presentation operations behind a backend-neutral trait:
  - Enumerate outputs (geometry, mode, refresh).
  - Acquire render target for output $N$ this frame.
  - Submit render target.
  - Report presentation feedback (timestamp ns, refresh ns, flags).
- **Invariant**: The Wayland implementation remains default; settle logic and telemetry (`straddles`, `phaseMs`, `lagFrames`, `gateHolds`) stay backend-neutral.

### Slice 2 — DRM Backend: Enumeration and Mode-Set
- **Components**: New `src/projection/display.rs`, `src/reconciler/mod.rs`.
- **Scope**: Acquire DRM master, enumerate connectors and modes, and apply output plans as a single multi-CRTC atomic commit. Handle hotplug and DPMS events.

### Slice 3 — Present Path and DRM-Event Pacing
- **Components**: `src/projection/display.rs`, `src/projection/gpu.rs`.
- **Scope**: Blend into swapchain images, present per connector, and drive the presentation gate from DRM page-flip completion events.

### Slice 4 — Configuration and Capability Reporting
- **Components**: `src/config.rs`, `src/model/observed.rs`, [Configuration](../configuration.md).
- **Scope**: Add a `presentation` setting (`auto`, `wayland`, `direct-drm`) with automatic fallback to Wayland when DRM master cannot be acquired. Expose active presentation backend in `GET /api/v1/projection/stats`.

### Slice 5 — Backend-Aware Health Checks
- **Components**: `src/checks/mod.rs` (`OUTPUT_PHASE`, `REFRESH_RATES`).
- **Scope**: Adjust health checks that rely on Sway IPC (such as `output-phase`) so they report not-applicable rather than warning when running under the direct DRM backend.

### Slice 6 — Privilege and Packaging
- **Components**: `packaging/`, provisioning scripts, [Getting Started](../getting-started.md).
- **Scope**: Configure least-privilege DRM master acquisition (e.g. `CAP_SYS_ADMIN` or systemd-logind/libseat integration) alongside the existing `CAP_SYS_NICE` capabilities.

### Slice 7 — Fault Recovery
- **Components**: `src/projection/manager.rs`, supervisor.
- **Scope**: Define crash recovery and fallback paths to ensure display output is automatically restored if the direct slicer process terminates.

### Slice 8 — Profiling & Validation
- **Scope**: Profile CPU utilization, inter-display synchronization, and frame timing under Wayland vs. direct presentation modes.

---

## 6. Success Gates

The implementation succeeds only when demonstrated on a 4-projector installation:

1. **EGL Spin Eliminated**: Compositor CPU overhead drops to near zero because the compositor is removed from the active output path.
2. **Synchronization Parity**: Straddles, lag frames, and frame rates match or improve upon the direct-scanout baseline.
3. **Cold-Boot Phase Alignment**: Outputs are phase-locked on cold boot via atomic multi-CRTC mode-setting without requiring blank-and-rearm workarounds.
4. **Clean Fallback**: Systems without direct DRM privileges fall back seamlessly to Wayland presentation.
