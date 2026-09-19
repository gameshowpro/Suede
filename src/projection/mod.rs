//! Multi-projector support. Phase one: gamma-correct edge blending.
//!
//! The architecture, in one paragraph: outputs are positioned to *overlap* in
//! the ordinary sway layout, so a spanned window renders the shared strip on
//! both projectors; a small layer-shell overlay per projector then fades each
//! side of every seam with a ramp shaped for that projector's gamma, making
//! the summed light across the seam constant. Geometry (corner pinning) is a
//! later phase and belongs to the same module boundary: the ramp math in
//! [`blend`] is already the piece a warp client would embed in its shader.
//!
//! The whole module is compiled out without the `projection` cargo feature;
//! the configuration schema is not, so every build speaks the same API.

pub mod blend;
pub mod control;
#[cfg(unix)]
mod dmabuf;
#[cfg(unix)]
pub mod gpu;
pub mod manager;
#[cfg(unix)]
pub mod overlay;
pub mod pattern;
#[cfg(unix)]
pub mod slicer;
pub use crate::warp_math as warp;
pub mod layout;

// Packet 4 reference only: no runtime or public configuration dependency.
#[cfg(test)]
mod seam_oracle;

pub use blend::{
    canvas_plan, canvas_plan_with_warp_activation, overlay_specs, CanvasPlan, OverlaySpec,
    Participant, SlicerSpec, Slicing,
};
pub use manager::BlendManager;

#[cfg(all(unix, test))]
#[allow(dead_code)] // Retained research fixtures; never linked into production.
mod warp_spike;
#[cfg(unix)]
mod warp_update;
