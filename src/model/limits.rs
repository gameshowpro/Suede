//! Numeric ceilings for canvas and output raster sizes.
//!
//! Kept in one place because the same three numbers were previously
//! restated in [`crate::model::geometry`], [`crate::model::desired`], and
//! [`crate::api::projection`], with nothing to stop them drifting apart.

/// Maximum canvas or output raster dimension, per axis, in pixels.
pub const MAX_DIMENSION: u32 = 32_768;
/// Maximum canvas allocation (width times height), in pixels.
pub const MAX_CANVAS_PIXELS: u64 = 64_000_000;
/// Maximum single-output allocation (width times height), in pixels.
pub const MAX_OUTPUT_PIXELS: u64 = 32_000_000;
