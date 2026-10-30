//! Pure geometry types used by the projection configuration.
//!
//! Canvas coordinates are isotropic: the canvas occupies `[0, 1]` by
//! `[0, 1/aspect]`.  A render of `W × H` pixels therefore uses `W` pixels per
//! x unit and `aspect * H` pixels per y unit.  `H` is rounded from
//! `W / aspect`; the small density difference caused by that rounding is
//! intentional and is preserved by [`CanvasRect::pixel_rect`].
//!
//! A slice is stored in one of two [`SliceUnits`]: fractions of the canvas
//! in those isotropic units (`percent`), or whole canvas pixels (`pixels`).
//! Whatever is stored, the slicer always crops whole canvas pixels, and
//! [`effective_slice_px`] is the one place those pixels are derived.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::limits::{MAX_CANVAS_PIXELS, MAX_DIMENSION, MAX_OUTPUT_PIXELS};

const MAX_SLICE_SPAN: f64 = 16.0;
const TOPOLOGY_EPS: f64 = 1.0e-12;
const SHARED_EDGE_EPS: f64 = 1.0e-10;

/// The selected projection pipeline, declared from most to least
/// restrictive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "lowercase")]
pub enum ProjectionMode {
    /// Strictly one-to-one: every enabled output shows exactly its own
    /// raster's worth of canvas pixels, at whole-pixel canvas coordinates,
    /// at output scale 1, without correction. Needs the canvas slicer, so it
    /// is unavailable on a tiled appliance (`allow_overlaps = false`).
    /// See [`crate::model::exact`].
    Exact,
    /// Shared rectangular content selection and scaling, without correction.
    #[default]
    Simple,
    /// Configured canvas slice rectangles and destination corner pins.
    Warp,
}

impl ProjectionMode {
    /// Whether the mode renders through the shared canvas even when the
    /// document names none, so a canvas is required. Simple uses one only
    /// when it is configured.
    pub fn requires_canvas(self) -> bool {
        match self {
            Self::Exact | Self::Warp => true,
            Self::Simple => false,
        }
    }

    /// Whether the slicer applies the retained destination correction
    /// (corner pins and center remap) and presents a unit-scale raster.
    pub fn applies_correction(self) -> bool {
        match self {
            Self::Warp => true,
            Self::Exact | Self::Simple => false,
        }
    }

    /// The mode's lowercase wire name, for messages.
    pub fn name(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Simple => "simple",
            Self::Warp => "warp",
        }
    }
}

/// How a slicer output actually sampled its captured source.
///
/// Distinct from [`ProjectionMode`]: a Simple-mode crop that happens to sit
/// on an integer pixel boundary samples `Exact` just like an identity Warp
/// output would, while a fractional crop or corner pin in either mode needs
/// `Bilinear`. This is a report of the sampling path taken, not of which
/// pipeline requested it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum SamplingMode {
    /// Pixel-for-pixel; no interpolation was needed.
    Exact,
    /// Nearest-texel sampling.
    Nearest,
    /// Bilinear interpolation, because the source and destination rasters
    /// did not line up 1:1.
    Bilinear,
    /// Catmull-Rom bicubic interpolation.
    Bicubic,
    /// Lanczos-3 windowed sinc.
    Lanczos,
    /// Edge-adaptive upscaling (FSR 1.0 EASU).
    Easu,
    /// Box (area) averaging, applied automatically when an axis minifies by
    /// more than 2x.
    Area,
    /// No slicer: sway tiles one fullscreen window across the outputs, one
    /// texel per pixel at output scale 1. Reported only as a per-output
    /// `mapping.samplingPath`, never as a slicer's `samplingMode`.
    Tiled,
}

/// The unit every output's `geometry.slice`, `geometry.rasterFootprint` and
/// `arrangeOffset` is stored in: `projection.sliceUnits`, one per
/// installation, so a resolution change treats every slice alike.
///
/// Whichever is stored, the slicer always crops whole canvas pixels; see
/// [`effective_slice_px`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "lowercase")]
pub enum SliceUnits {
    /// Fractions of the canvas, in the isotropic canvas units: `x` and
    /// `width` over the canvas width, `y` and `height` over the same unit
    /// (the canvas occupies `[0, 1]` by `[0, 1/aspect]`). These fractions are
    /// the truth; the pixels are derived at use by rounding the edges, so a
    /// resolution change rescales every slice and repeated changes never
    /// drift. What every document without `sliceUnits` stores.
    #[default]
    Percent,
    /// Whole canvas pixels from the canvas's top-left corner. These integers
    /// are the truth: a resolution change leaves them alone, and a slice that
    /// then extends past the canvas is reported, not moved. Exact mode
    /// always stores pixels.
    Pixels,
}

impl SliceUnits {
    /// The unit's wire name, for messages.
    pub fn name(self) -> &'static str {
        match self {
            Self::Percent => "percent",
            Self::Pixels => "pixels",
        }
    }

    /// The serialization skip for `projection.sliceUnits`: anything but
    /// `pixels` is left out, so a percent document stays exactly as an older
    /// build wrote it.
    pub fn omitted(units: &Option<Self>) -> bool {
        !matches!(units, Some(Self::Pixels))
    }
}

/// A rectangle of whole canvas pixels: what the slicer crops.
///
/// `x` and `y` may be negative, and the rectangle may extend past the
/// canvas: a slice is allowed to overhang the canvas as long as part of it
/// lies on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PixelRect {
    pub x: i64,
    pub y: i64,
    pub width: i64,
    pub height: i64,
}

impl PixelRect {
    pub fn right(self) -> i64 {
        self.x + self.width
    }

    pub fn bottom(self) -> i64 {
        self.y + self.height
    }

    /// The rectangle as the slicer's `[x, y, width, height]` source rect.
    pub fn source(self) -> [f64; 4] {
        [
            self.x as f64,
            self.y as f64,
            self.width as f64,
            self.height as f64,
        ]
    }

    /// The rectangle as stored in [`SliceUnits::Pixels`].
    pub fn stored(self) -> CanvasRect {
        let [x, y, width, height] = self.source();
        CanvasRect {
            x,
            y,
            width,
            height,
        }
    }

    /// The same rectangle in isotropic canvas units (what
    /// [`SliceUnits::Percent`] stores), by exact division.
    pub fn to_canvas_units(self, canvas: &CanvasConfig) -> Result<CanvasRect, String> {
        to_canvas_units(self.stored(), SliceUnits::Pixels, canvas)
    }
}

/// Canvas pixels per isotropic canvas unit on each axis: the canvas width
/// horizontally and `aspect * height` vertically, exactly as
/// [`CanvasRect::pixel_rect`] has them.
pub fn canvas_densities(canvas: &CanvasConfig) -> Result<(f64, f64), String> {
    let (width, height) = canvas.dimensions()?;
    Ok((f64::from(width), canvas.aspect * f64::from(height)))
}

/// Round half up: the one rounding rule for slice edges (`1919.5` is
/// `1920`, `-0.5` is `0`).
pub fn round_half_up(value: f64) -> f64 {
    (value + 0.5).floor() + 0.0
}

/// The whole canvas pixels a stored slice stands for: the one place pixels
/// are derived from a stored slice.
///
/// * [`SliceUnits::Pixels`]: the stored integers themselves (validation
///   rejects anything else; a stray fraction is rounded here like an edge).
/// * [`SliceUnits::Percent`]: the edges are rounded, not the sizes:
///   `left = round(x * W)`, `right = round((x + width) * W)`,
///   `width = right - left`, and likewise on `y` with the vertical density,
///   rounding half up. Two slices sharing an edge therefore share the
///   rounded edge, and an overlap is the same whole number of pixels seen
///   from either side.
///
/// Fails on an invalid canvas in percent, and on a rectangle whose edges are
/// not finite or are far outside any canvas.
pub fn effective_slice_px(
    slice: CanvasRect,
    units: SliceUnits,
    canvas: &CanvasConfig,
) -> Result<PixelRect, String> {
    let (x_density, y_density) = match units {
        SliceUnits::Percent => canvas_densities(canvas)?,
        SliceUnits::Pixels => (1.0, 1.0),
    };
    let edge = |value: f64| -> Result<i64, String> {
        let rounded = round_half_up(value);
        if rounded.is_finite() && rounded.abs() <= f64::from(i32::MAX) {
            Ok(rounded as i64)
        } else {
            Err("geometry.slice is outside the representable pixel range".into())
        }
    };
    let left = edge(slice.x * x_density)?;
    let top = edge(slice.y * y_density)?;
    let right = edge(slice.right() * x_density)?;
    let bottom = edge(slice.bottom() * y_density)?;
    Ok(PixelRect {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    })
}

/// A stored rectangle in isotropic canvas units, without rounding: the
/// fractions themselves in [`SliceUnits::Percent`], the pixels divided by
/// the canvas densities in [`SliceUnits::Pixels`]. For the checks and the
/// physical footprint, which are not crops.
pub fn to_canvas_units(
    rect: CanvasRect,
    units: SliceUnits,
    canvas: &CanvasConfig,
) -> Result<CanvasRect, String> {
    match units {
        SliceUnits::Percent => Ok(rect),
        SliceUnits::Pixels => {
            let (x_density, y_density) = canvas_densities(canvas)?;
            Ok(CanvasRect {
                x: rect.x / x_density,
                y: rect.y / y_density,
                width: rect.width / x_density,
                height: rect.height / y_density,
            })
        }
    }
}

/// Convert a stored rectangle from one unit to the other on `canvas`:
/// percent to pixels rounds the edges ([`effective_slice_px`]); pixels to
/// percent is exact division. Switching units is the only time a stored
/// value is rounded.
pub fn convert_rect(
    rect: CanvasRect,
    from: SliceUnits,
    to: SliceUnits,
    canvas: &CanvasConfig,
) -> Result<CanvasRect, String> {
    match (from, to) {
        (SliceUnits::Percent, SliceUnits::Percent) | (SliceUnits::Pixels, SliceUnits::Pixels) => {
            Ok(rect)
        }
        (SliceUnits::Percent, SliceUnits::Pixels) => {
            Ok(effective_slice_px(rect, from, canvas)?.stored())
        }
        (SliceUnits::Pixels, SliceUnits::Percent) => to_canvas_units(rect, from, canvas),
    }
}

/// Convert a stored offset (`arrangeOffset`) from one unit to the other on
/// `canvas`, by the same rules as [`convert_rect`]: rounded to whole pixels
/// one way, divided exactly the other.
pub fn convert_offset(
    [x, y]: [f64; 2],
    from: SliceUnits,
    to: SliceUnits,
    canvas: &CanvasConfig,
) -> Result<[f64; 2], String> {
    let (x_density, y_density) = canvas_densities(canvas)?;
    Ok(match (from, to) {
        (SliceUnits::Percent, SliceUnits::Percent) | (SliceUnits::Pixels, SliceUnits::Pixels) => {
            [x, y]
        }
        (SliceUnits::Percent, SliceUnits::Pixels) => {
            [round_half_up(x * x_density), round_half_up(y * y_density)]
        }
        (SliceUnits::Pixels, SliceUnits::Percent) => [x / x_density, y / y_density],
    })
}

/// The first component of a rectangle stored in pixels that is not a whole
/// number, as `(name, value)`.
pub fn first_fractional(rect: CanvasRect) -> Option<(&'static str, f64)> {
    [
        ("x", rect.x),
        ("y", rect.y),
        ("width", rect.width),
        ("height", rect.height),
    ]
    .into_iter()
    .find(|(_, value)| value.fract() != 0.0)
}

/// A canvas rectangle in isotropic canvas units.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CanvasRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl CanvasRect {
    pub fn right(self) -> f64 {
        self.x + self.width
    }

    pub fn bottom(self) -> f64 {
        self.y + self.height
    }

    pub fn area(self) -> f64 {
        self.width * self.height
    }

    pub fn corners(self) -> [[f64; 2]; 4] {
        [
            [self.x, self.y],
            [self.right(), self.y],
            [self.right(), self.bottom()],
            [self.x, self.bottom()],
        ]
    }

    /// Convert this canvas rectangle to continuous pixel-boundary coordinates.
    /// The result is `[x, y, width, height]`, not the bottom-right corner.
    ///
    /// Not the crop: the slicer crops [`effective_slice_px`], the rounded
    /// edges. This is the unrounded measurement, for density arithmetic and
    /// for judging how far a stored fraction is from whole pixels.
    ///
    /// Fails with `canvas`'s own [`CanvasConfig::dimensions`] error when the
    /// canvas itself is not valid; every caller either already validated the
    /// canvas or is in a position to propagate this rather than trust it.
    pub fn pixel_rect(self, canvas: &CanvasConfig) -> Result<[f64; 4], String> {
        let (width, height) = canvas.dimensions()?;
        let x_density = f64::from(width);
        let y_density = canvas.aspect * f64::from(height);
        Ok([
            self.x * x_density,
            self.y * y_density,
            self.width * x_density,
            self.height * y_density,
        ])
    }

    fn finite_positive(self) -> bool {
        self.x.is_finite()
            && self.y.is_finite()
            && self.width.is_finite()
            && self.height.is_finite()
            && self.width > 0.0
            && self.height > 0.0
            && self.right().is_finite()
            && self.bottom().is_finite()
    }
}

/// Canvas aspect and the operator-selected render width.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CanvasConfig {
    pub aspect: f64,
    pub render_width: u32,
}

impl Default for CanvasConfig {
    fn default() -> Self {
        Self {
            aspect: 1.0,
            render_width: 1,
        }
    }
}

impl CanvasConfig {
    /// Return the actual raster dimensions `(width, height)`.
    pub fn dimensions(&self) -> Result<(u32, u32), String> {
        if !(self.aspect.is_finite() && self.aspect > 0.0) {
            return Err("canvas aspect must be a finite positive number".into());
        }
        if self.render_width == 0 {
            return Err("canvas renderWidth must be greater than zero".into());
        }
        let height = (f64::from(self.render_width) / self.aspect)
            .round()
            .max(1.0);
        if !height.is_finite() || height < 1.0 || height > f64::from(u32::MAX) {
            return Err("canvas height is outside the supported range".into());
        }
        let height = height as u32;
        if self.render_width > MAX_DIMENSION || height > MAX_DIMENSION {
            return Err("canvas dimensions must be at most 32768 pixels per axis".into());
        }
        let pixels = u64::from(self.render_width) * u64::from(height);
        if pixels > MAX_CANVAS_PIXELS {
            return Err("canvas allocation exceeds the 64MP limit".into());
        }
        Ok((self.render_width, height))
    }

    pub fn bounds(&self) -> Result<CanvasRect, String> {
        self.dimensions()?;
        Ok(CanvasRect {
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 1.0 / self.aspect,
        })
    }
}

/// Persisted shared content selection and independent destination correction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OutputGeometry {
    /// The slice: the canvas region this output samples, used by every
    /// mode, in the installation's `projection.sliceUnits` (fractions of the
    /// canvas in `percent`, whole canvas pixels in `pixels`). The whole-pixel
    /// crop the slicer uses is derived from this rectangle
    /// ([`effective_slice_px`]), never stored as a second editable
    /// representation.
    ///
    /// Serialized as `slice`; `source` is still accepted on input (an alpha
    /// rename) so saved documents and older clients' writes keep parsing,
    /// but every response and every re-serialization emits `slice`.
    #[serde(alias = "source")]
    pub slice: CanvasRect,
    /// Output-local normalized destination pins in TL, TR, BR, BL order.
    /// Normalized to the as-viewed (upright, logical) raster: the picture on
    /// the mounted display after any output transform. For 90/270 transforms
    /// that raster is `mode.height` by `mode.width`.
    pub corners: [[f64; 2]; 4],
    #[serde(default = "neutral_center")]
    pub center: [f64; 2],
    /// Physical light-field coverage, independent of the slice and
    /// destination pins, in the same `projection.sliceUnits` as the slice
    /// (it need not be whole pixels: it is a measurement, not a crop). Until
    /// a calibrated footprint is supplied, conversion copies the slice
    /// rectangle into this field.
    pub raster_footprint: CanvasRect,
}

fn neutral_center() -> [f64; 2] {
    [0.5, 0.5]
}

impl OutputGeometry {
    pub fn slice_quad(&self) -> [[f64; 2]; 4] {
        self.slice.corners()
    }

    /// Validate the geometry that is independent of the selected pipeline,
    /// with the slice stored in `units`.
    pub fn validate_numbers(
        &self,
        canvas: Option<&CanvasConfig>,
        units: SliceUnits,
    ) -> Result<(), String> {
        if !self.slice.finite_positive() {
            return Err("geometry.slice must contain finite positive dimensions".into());
        }
        if units == SliceUnits::Pixels {
            if let Some((name, value)) = first_fractional(self.slice) {
                return Err(format!(
                    "geometry.slice must be whole canvas pixels while projection.sliceUnits is \
                     \"pixels\"; {name} is {value}"
                ));
            }
        }
        if !self.raster_footprint.finite_positive() {
            return Err("geometry.rasterFootprint must contain finite positive dimensions".into());
        }
        if !self.corners.iter().flatten().all(|value| value.is_finite()) {
            return Err("geometry.corners must contain finite numbers".into());
        }
        if !self
            .center
            .iter()
            .all(|value| value.is_finite() && (0.01..=0.99).contains(value))
        {
            return Err("geometry.center values must be finite and in 0.01..=0.99".into());
        }
        // Even without an output mode, validate the pin topology against a
        // representative raster so a retained simple-mode entry cannot hide
        // duplicate, concave, singular, or pole-crossing pins.
        let unit_corners = self.corners.map(|[x, y]| [x * 1000.0, y * 1000.0]);
        crate::warp_math::Warp::new(unit_corners, self.center, 1000, 1000)
            .map(|_| ())
            .map_err(|error| format!("geometry corners are invalid: {error}"))?;
        if let Some(canvas) = canvas {
            validate_rect_bounds(
                to_canvas_units(self.slice, units, canvas)?,
                canvas,
                "geometry.slice",
            )?;
            // The crop the slicer will actually use must have pixels in it.
            let pixels = effective_slice_px(self.slice, units, canvas)?;
            if pixels.width < 1 || pixels.height < 1 {
                return Err(format!(
                    "geometry.slice rounds to {}×{} canvas pixels; a slice needs at least one \
                     whole pixel on each axis",
                    pixels.width, pixels.height
                ));
            }
            validate_rect_extent(
                to_canvas_units(self.raster_footprint, units, canvas)?,
                canvas,
                "geometry.rasterFootprint",
            )?;
        }
        Ok(())
    }

    /// Validate the destination pins against one output raster. `width` and
    /// `height` are the as-viewed (logical) raster dimensions, already
    /// swapped for 90/270 transforms. The pins are checked against the
    /// whole-pixel crop the slicer will use.
    pub fn validate_for_output(
        &self,
        canvas: &CanvasConfig,
        units: SliceUnits,
        width: u32,
        height: u32,
    ) -> Result<(), String> {
        self.validate_numbers(Some(canvas), units)?;
        if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
            return Err("output dimensions must be in 1..=32768".into());
        }
        if u64::from(width) * u64::from(height) > MAX_OUTPUT_PIXELS {
            return Err("output allocation exceeds the 32MP limit".into());
        }
        let corners = self
            .corners
            .map(|[x, y]| [x * f64::from(width), y * f64::from(height)]);
        let source_rect = effective_slice_px(self.slice, units, canvas)?.source();
        crate::warp_math::Warp::new(corners, self.center, width, height)
            .and_then(|warp| warp.with_source_rect(source_rect))
            .map(|_| ())
            .map_err(|error| format!("geometry corners are invalid: {error}"))
    }
}

fn validate_rect_bounds(
    rect: CanvasRect,
    canvas: &CanvasConfig,
    label: &str,
) -> Result<(), String> {
    validate_rect_extent(rect, canvas, label)?;
    let bounds = canvas.bounds()?;
    let span = bounds.width.max(bounds.height);
    let left = rect.x.max(bounds.x);
    let top = rect.y.max(bounds.y);
    let right = rect.right().min(bounds.right());
    let bottom = rect.bottom().min(bounds.bottom());
    if right - left <= TOPOLOGY_EPS * span || bottom - top <= TOPOLOGY_EPS * span {
        return Err(format!(
            "{label} has no positive visible area on the canvas"
        ));
    }
    Ok(())
}

fn validate_rect_extent(
    rect: CanvasRect,
    canvas: &CanvasConfig,
    label: &str,
) -> Result<(), String> {
    if !rect.finite_positive() {
        return Err(format!("{label} must contain finite positive dimensions"));
    }
    let bounds = canvas.bounds()?;
    let span = bounds.width.max(bounds.height);
    let limit = MAX_SLICE_SPAN * span;
    if rect.x.abs() > limit
        || rect.y.abs() > limit
        || rect.right().abs() > limit
        || rect.bottom().abs() > limit
    {
        return Err(format!("{label} exceeds the ±16 canvas-span bound"));
    }
    Ok(())
}

/// Validate the topology of public rectangular slice regions.
///
/// The boolean is `true` only for a pure near-total stack (every pair covers
/// at least 80% of the smaller original rectangle).  The graph otherwise
/// requires positive-area clipped overlap or a shared edge segment; point
/// contact and gaps do not connect regions.
///
/// Requires every region to be reachable from every other (see
/// [`validate_slices_allow_disconnected`] for the one caller — a legacy
/// integer-position layout — that must accept a gap).
pub fn validate_slices(canvas: &CanvasConfig, slices: &[CanvasRect]) -> Result<bool, String> {
    validate_slices_inner(canvas, slices, true)
}

/// As [`validate_slices`], but a slice graph with more than one connected
/// component is not an error: disconnected groups simply never blend with
/// each other. Every other check — bounds, minimum area, and the
/// near-total-stack-vs-ordinary-seam consistency rule — is unchanged and
/// still applies within and across groups.
///
/// This exists for legacy integer-position layouts (`blend`'s synthesized
/// `LayoutSpec` for a `SlicerSpec` with no configured canvas): connectivity
/// is a requirement of shared-canvas calibration, which those layouts never
/// had, not of the legacy positioning scheme itself. A wall with a
/// deliberate gap between two output groups produced a canvas plan before
/// canvas calibration existed and must keep doing so.
pub(crate) fn validate_slices_allow_disconnected(
    canvas: &CanvasConfig,
    slices: &[CanvasRect],
) -> Result<bool, String> {
    validate_slices_inner(canvas, slices, false)
}

fn validate_slices_inner(
    canvas: &CanvasConfig,
    slices: &[CanvasRect],
    require_connected: bool,
) -> Result<bool, String> {
    if slices.is_empty() || slices.len() > 8 {
        return Err("slices must contain between 1 and 8 regions".into());
    }
    let bounds = canvas.bounds()?;
    let span = bounds.width.max(bounds.height);
    for (index, slice) in slices.iter().copied().enumerate() {
        validate_rect_bounds(slice, canvas, &format!("slice[{index}]"))?;
        if slice.width * slice.height <= TOPOLOGY_EPS * span * span {
            return Err(format!("slice[{index}] area is too small"));
        }
    }

    let mut all_stack = true;
    for i in 0..slices.len() {
        for j in (i + 1)..slices.len() {
            let a = slices[i];
            let b = slices[j];
            let intersection = clipped_intersection(a, b, bounds);
            let overlap = intersection.width.max(0.0) * intersection.height.max(0.0);
            let shared_edge = shared_edge_length(a, b, bounds);
            // Duplicate classification deliberately uses the original
            // rectangles. Canvas clipping only controls visible topology.
            let original = raw_intersection(a, b);
            let original_overlap = original.width.max(0.0) * original.height.max(0.0);
            let near_total = original_overlap >= 0.8 * a.area().min(b.area());
            all_stack &= near_total;
            if !near_total
                && (overlap > TOPOLOGY_EPS * span * span || shared_edge > SHARED_EDGE_EPS * span)
            {
                // Keep checking all pairs so callers receive a stable result,
                // but remember that a mixed stack/seam layout is forbidden.
                all_stack = false;
            }
        }
    }
    if require_connected {
        let linked = |a: CanvasRect, b: CanvasRect| {
            let intersection = clipped_intersection(a, b, bounds);
            let overlap = intersection.width.max(0.0) * intersection.height.max(0.0);
            overlap > TOPOLOGY_EPS * span * span
                || shared_edge_length(a, b, bounds) > SHARED_EDGE_EPS * span
        };
        let mut connected = vec![false; slices.len()];
        connected[0] = true;
        let mut stack = vec![0usize];
        while let Some(current) = stack.pop() {
            for j in 0..slices.len() {
                if connected[j] || j == current {
                    continue;
                }
                if linked(slices[current], slices[j]) {
                    connected[j] = true;
                    stack.push(j);
                }
            }
        }
        if connected.iter().any(|value| !value) {
            return Err(
                "slices are disconnected: gaps and point contacts do not connect regions".into(),
            );
        }
    }

    let mut any_near_total = false;
    for i in 0..slices.len() {
        for j in (i + 1)..slices.len() {
            let overlap = raw_intersection(slices[i], slices[j]);
            let overlap = overlap.width.max(0.0) * overlap.height.max(0.0);
            if overlap >= 0.8 * slices[i].area().min(slices[j].area()) {
                any_near_total = true;
            } else if any_near_total {
                return Err(
                    "mixed_stack_topology: slice pairs must all be near-total or ordinary".into(),
                );
            }
        }
    }
    if any_near_total && !all_stack {
        return Err("mixed_stack_topology: slice pairs must all be near-total or ordinary".into());
    }
    Ok(any_near_total)
}

fn clipped_intersection(a: CanvasRect, b: CanvasRect, bounds: CanvasRect) -> CanvasRect {
    let left = a.x.max(b.x).max(bounds.x);
    let top = a.y.max(b.y).max(bounds.y);
    let right = a.right().min(b.right()).min(bounds.right());
    let bottom = a.bottom().min(b.bottom()).min(bounds.bottom());
    CanvasRect {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    }
}

fn raw_intersection(a: CanvasRect, b: CanvasRect) -> CanvasRect {
    CanvasRect {
        x: a.x.max(b.x),
        y: a.y.max(b.y),
        width: a.right().min(b.right()) - a.x.max(b.x),
        height: a.bottom().min(b.bottom()) - a.y.max(b.y),
    }
}

fn shared_edge_length(a: CanvasRect, b: CanvasRect, bounds: CanvasRect) -> f64 {
    let clip_x0 = bounds.x;
    let clip_x1 = bounds.right();
    let clip_y0 = bounds.y;
    let clip_y1 = bounds.bottom();
    let horizontal = if (a.bottom() - b.y).abs() <= SHARED_EDGE_EPS
        || (b.bottom() - a.y).abs() <= SHARED_EDGE_EPS
    {
        (a.right().min(b.right()).min(clip_x1) - a.x.max(b.x).max(clip_x0)).max(0.0)
    } else {
        0.0
    };
    let vertical = if (a.right() - b.x).abs() <= SHARED_EDGE_EPS
        || (b.right() - a.x).abs() <= SHARED_EDGE_EPS
    {
        (a.bottom().min(b.bottom()).min(clip_y1) - a.y.max(b.y).max(clip_y0)).max(0.0)
    } else {
        0.0
    };
    horizontal.max(vertical)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, width: f64, height: f64) -> CanvasRect {
        CanvasRect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn dimensions_round_height_and_preserve_density() {
        let canvas = CanvasConfig {
            aspect: 1.9,
            render_width: 1900,
        };
        assert_eq!(canvas.dimensions().unwrap(), (1900, 1000));
        assert_eq!(
            rect(0.0, 0.0, 0.5, 0.5).pixel_rect(&canvas).unwrap(),
            [0.0, 0.0, 950.0, 950.0]
        );
    }

    /// A8: `pixel_rect` used to `expect()` a valid canvas and panic on an
    /// invalid one; it must return `Err` instead.
    #[test]
    fn pixel_rect_reports_an_invalid_canvas_instead_of_panicking() {
        let invalid = CanvasConfig {
            aspect: 0.0,
            render_width: 100,
        };
        assert!(rect(0.0, 0.0, 1.0, 1.0).pixel_rect(&invalid).is_err());
    }

    #[test]
    fn slice_topology_distinguishes_stack_and_gap() {
        let canvas = CanvasConfig {
            aspect: 1.0,
            render_width: 100,
        };
        assert!(!validate_slices(
            &canvas,
            &[rect(0.0, 0.0, 1.0, 1.0), rect(0.5, 0.0, 1.0, 1.0)]
        )
        .unwrap());
        assert!(validate_slices(
            &canvas,
            &[rect(0.0, 0.0, 1.0, 1.0), rect(1.1, 0.0, 1.0, 1.0)]
        )
        .is_err());
        assert!(validate_slices(
            &canvas,
            &[rect(0.0, 0.0, 1.0, 1.0), rect(0.1, 0.0, 1.0, 1.0)]
        )
        .unwrap());
    }

    #[test]
    fn slice_topology_rejects_two_disjoint_chains() {
        let canvas = CanvasConfig {
            aspect: 1.0,
            render_width: 100,
        };
        let slices = [
            rect(0.0, 0.0, 0.2, 1.0),
            rect(0.2, 0.0, 0.2, 1.0),
            rect(0.6, 0.0, 0.2, 1.0),
            rect(0.8, 0.0, 0.2, 1.0),
        ];
        let error = validate_slices(&canvas, &slices).unwrap_err();
        assert!(error.contains("disconnected"), "{error}");
    }

    #[test]
    fn geometry_uses_camel_case_and_defaults_neutral_center() {
        let geometry: OutputGeometry = serde_json::from_str(
            r#"{
                "slice":{"x":0,"y":0,"width":1,"height":1},
                "corners":[[0,0],[1,0],[1,1],[0,1]],
                "rasterFootprint":{"x":0,"y":0,"width":1,"height":1}
            }"#,
        )
        .unwrap();
        assert_eq!(geometry.center, [0.5, 0.5]);
        let value = serde_json::to_value(geometry).unwrap();
        assert!(value.get("rasterFootprint").is_some());
        assert!(value.get("raster_footprint").is_none());
    }

    /// The alpha rename's back-compat contract: a document (or an old
    /// client's write) naming the field `source` still parses, and every
    /// re-serialization emits the new name, `slice`, never the alias.
    #[test]
    fn geometry_slice_accepts_the_source_alias_and_serializes_as_slice() {
        let geometry: OutputGeometry = serde_json::from_str(
            r#"{
                "source":{"x":0.25,"y":0.5,"width":1,"height":1},
                "corners":[[0,0],[1,0],[1,1],[0,1]],
                "rasterFootprint":{"x":0,"y":0,"width":1,"height":1}
            }"#,
        )
        .unwrap();
        assert_eq!(geometry.slice, rect(0.25, 0.5, 1.0, 1.0));
        let value = serde_json::to_value(&geometry).unwrap();
        assert_eq!(
            value.get("slice"),
            Some(&serde_json::json!({"x":0.25,"y":0.5,"width":1.0,"height":1.0}))
        );
        assert!(value.get("source").is_none());

        // Round-trips through the canonical name too, unsurprisingly.
        let round_tripped: OutputGeometry = serde_json::from_value(value).unwrap();
        assert_eq!(round_tripped, geometry);
    }
}
