//! Multi-projector support: canvas layout, edge blending, and projective
//! corner pinning (Warp) for overlapping installations.
//!
//! Sway never sees the overlap. Outputs sit edge to edge in the ordinary sway
//! layout, and the active app renders once into a headless canvas sized to
//! the whole arrangement. A child process this module manages — `suede
//! slice`, in [`slicer`] — captures that canvas every frame and cuts it into
//! one output-sized slice per projector, sampling each slice through its
//! source rectangle and, in Warp, a projective correction, and fading every
//! seam so the two copies of it sum to constant luminance. [`gpu`] keeps
//! that whole path on the GPU (capture, sample, blend, present) when a
//! Vulkan capability probe succeeds; [`slicer`] also carries a CPU blend
//! path for when it does not, or the operator pins `renderer: cpu`.
//!
//! A map of the pieces, roughly in the order data flows through them:
//!
//! - [`layout`] holds the one blend-weight rule, `Evaluator`: a source's
//!   share of a seam is its normalized distance to that seam's active edges,
//!   or its plain inward distance where no edge is active. Every canvas
//!   plan — including a legacy integer-position layout, which synthesizes
//!   one — carries a `LayoutSpec` built from this, so there is exactly one
//!   place seam weights are computed.
//! - [`blend`] plans the canvas itself: given each output's place in the
//!   layout, it produces the `CanvasPlan`/`SlicerSpec` the slicer is told to
//!   realize, and the pure per-pixel transfer arithmetic (gamma shaping,
//!   black-lift blending) both the GPU and CPU paths apply identically.
//! - [`warp`] (a re-export of `crate::warp_math`, kept buildable without
//!   this module's `projection` feature so pure mapping validation is
//!   always available) is the projective corner-pin math: it turns a
//!   destination quad, a center, and a source rectangle into the per-pixel
//!   source mapping a slice presents.
//! - [`warp_update`] builds that per-pixel mapping incrementally, one output
//!   at a time and bounded per pass, so an edit to one output's pins does
//!   not stall every other output's frame.
//! - [`control`] is the versioned, bounded protocol the manager and a
//!   running slicer child speak over its stdin/stdout: the initial spec, then
//!   incremental updates the child may discard if a newer one supersedes it
//!   before it is applied.
//! - [`manager`] owns the slicer child's lifecycle — starting it, replacing
//!   it on a configuration edit that cannot be sent as an update, restarting
//!   it if it dies — and, on a tiling appliance with no overlap at all, the
//!   set of [`overlay`] processes instead.
//! - [`overlay`] is `suede blend`, a tiny layer-shell client. With no seams
//!   to fade it has nothing to do; the one thing it still draws is a test
//!   pattern on a non-overlapping (tiled) appliance, so a pattern is
//!   available for bench alignment even where the canvas/slicer path never
//!   runs. [`pattern`] is the picture both this overlay and the slicer draw,
//!   defined once in canvas space so it is sampled through the same source
//!   rectangle and warp/blend path as real content.
//! - [`adaptive`] is the pure measurement and control math for adaptive
//!   black lift — the target/smoothing/slew arithmetic and the schedule that
//!   rate-limits sampling. [`gpu`] does the asynchronous submit/poll of the
//!   GPU-side luminance measurement itself, off the render thread.
//!
//! Two pieces exist for tests only and carry no runtime or public
//! configuration dependency: `seam_oracle`, an independently implemented
//! reference for [`layout::Evaluator`]'s blend weights, checked against
//! production per-pixel; and `warp_spike`, retained research fixtures for an
//! alternative (arbitrary-polygon) seam model that was not built.
//!
//! The whole module is compiled out without the `projection` cargo feature;
//! the configuration schema is not, so every build speaks the same API.

pub mod adaptive;
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

// Test-only independent reference for `layout::Evaluator`'s blend weights;
// no runtime or public configuration dependency.
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
