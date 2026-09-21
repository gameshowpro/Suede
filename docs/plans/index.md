# Engineering Plans

This section contains design records of shipped projection features and proposed plans for upcoming display-presentation work in Suede.

---

## Shipped, documented here as a design record

### [Black lift](black-offset.md)
* **Status**: Shipped as `projection.blackLift` (fixed and adaptive). See the [Configuration reference](../configuration.md#adaptive-black-lift) for the schema.
* **Focus**: Optical black floor compensation across multi-projector overlapping seams, including content-luminance-adaptive control. The page also records one related idea that was drafted but never built.

## Active & Proposed Plans

### [Direct-to-Display Presentation via VK_KHR_display](VK_KHR_display.md)
* **Status**: Proposed Architectural Plan (Gate 0 research spike blocking).
* **Focus**: Bypassing the Wayland compositor output layer entirely by presenting directly to display hardware using Vulkan display extensions (`VK_KHR_display`, `VK_EXT_acquire_drm_display`). Eliminates compositor EGL spin, provides authoritative DRM vblank pacing, and guarantees atomic multi-head phase synchronization.
