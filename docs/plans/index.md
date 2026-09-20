# Engineering Plans

This section contains architectural specifications, design RFCs, and implementation plans for upcoming display presentation, projection calibration, and performance enhancements in Suede.

---

## Active & Proposed Plans

### [Black Offset (Luminance Lift / Floor Compensation)](black-offset.md)
* **Status**: Phase 1 Implemented & Validated; Phases 2 & 3 Proposed.
* **Focus**: Optical black floor compensation across multi-projector overlapping seams. Eliminates distracting bright bands in dark scenes by lifting less-overlapped regions to match the maximum overlap floor, with planned evolutions for dynamic non-linear roll-off and scene-adaptive temporal contrast masking.

### [Direct-to-Display Presentation via VK_KHR_display](VK_KHR_display.md)
* **Status**: Proposed Architectural Plan (Gate 0 research spike blocking).
* **Focus**: Bypassing the Wayland compositor output layer entirely by presenting directly to display hardware using Vulkan display extensions (`VK_KHR_display`, `VK_EXT_acquire_drm_display`). Eliminates compositor EGL spin, provides authoritative DRM vblank pacing, and guarantees atomic multi-head phase synchronization.
