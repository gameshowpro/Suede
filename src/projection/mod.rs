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
pub mod manager;
#[cfg(unix)]
pub mod overlay;
pub mod pattern;

pub use blend::{overlay_specs, OverlaySpec, Participant};
pub use manager::BlendManager;
