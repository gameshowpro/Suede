//! Pure geometry types used by the projection configuration.
//!
//! Canvas coordinates are isotropic: the canvas occupies `[0, 1]` by
//! `[0, 1/aspect]`.  A render of `W × H` pixels therefore uses `W` pixels per
//! x unit and `aspect * H` pixels per y unit.  `H` is rounded from
//! `W / aspect`; the small density difference caused by that rounding is
//! intentional and is preserved by [`CanvasRect::pixel_rect`].

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::limits::{MAX_CANVAS_PIXELS, MAX_DIMENSION, MAX_OUTPUT_PIXELS};

const MAX_SOURCE_SPAN: f64 = 16.0;
const TOPOLOGY_EPS: f64 = 1.0e-12;
const SHARED_EDGE_EPS: f64 = 1.0e-10;

/// The selected projection pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "lowercase")]
pub enum ProjectionMode {
    /// Shared rectangular content selection and scaling, without correction.
    #[default]
    Simple,
    /// Configured canvas source rectangles and destination corner pins.
    Warp,
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
    /// Bilinear interpolation, because the source and destination rasters
    /// did not line up 1:1.
    Bilinear,
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
    /// Canonical normalized crop, used by both Simple and Warp. Pixel crop
    /// origins and content enlargement are derived from this rectangle, never
    /// stored as a second editable representation.
    pub source: CanvasRect,
    /// Output-local normalized destination pins in TL, TR, BR, BL order.
    pub corners: [[f64; 2]; 4],
    #[serde(default = "neutral_center")]
    pub center: [f64; 2],
    /// Physical light-field coverage, independent of source and destination
    /// pins. Until a calibrated footprint is supplied, conversion copies the
    /// source rectangle into this field.
    pub raster_footprint: CanvasRect,
}

fn neutral_center() -> [f64; 2] {
    [0.5, 0.5]
}

impl OutputGeometry {
    pub fn source_quad(&self) -> [[f64; 2]; 4] {
        self.source.corners()
    }

    /// Validate the geometry that is independent of the selected pipeline.
    pub fn validate_numbers(&self, canvas: Option<&CanvasConfig>) -> Result<(), String> {
        if !self.source.finite_positive() {
            return Err("geometry.source must contain finite positive dimensions".into());
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
            validate_rect_bounds(self.source, canvas, "geometry.source")?;
            validate_rect_extent(self.raster_footprint, canvas, "geometry.rasterFootprint")?;
        }
        Ok(())
    }

    /// Validate the destination pins against one output raster.
    pub fn validate_for_output(
        &self,
        canvas: &CanvasConfig,
        width: u32,
        height: u32,
    ) -> Result<(), String> {
        self.validate_numbers(Some(canvas))?;
        if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
            return Err("output dimensions must be in 1..=32768".into());
        }
        if u64::from(width) * u64::from(height) > MAX_OUTPUT_PIXELS {
            return Err("output allocation exceeds the 32MP limit".into());
        }
        let corners = self
            .corners
            .map(|[x, y]| [x * f64::from(width), y * f64::from(height)]);
        let source_rect = self.source.pixel_rect(canvas)?;
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
    let limit = MAX_SOURCE_SPAN * span;
    if rect.x.abs() > limit
        || rect.y.abs() > limit
        || rect.right().abs() > limit
        || rect.bottom().abs() > limit
    {
        return Err(format!("{label} exceeds the ±16 canvas-span bound"));
    }
    Ok(())
}

/// Validate the topology of public rectangular source regions.
///
/// The boolean is `true` only for a pure near-total stack (every pair covers
/// at least 80% of the smaller original rectangle).  The graph otherwise
/// requires positive-area clipped overlap or a shared edge segment; point
/// contact and gaps do not connect regions.
///
/// Requires every region to be reachable from every other (see
/// [`validate_sources_allow_disconnected`] for the one caller — a legacy
/// integer-position layout — that must accept a gap).
pub fn validate_sources(canvas: &CanvasConfig, sources: &[CanvasRect]) -> Result<bool, String> {
    validate_sources_inner(canvas, sources, true)
}

/// As [`validate_sources`], but a source graph with more than one connected
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
pub(crate) fn validate_sources_allow_disconnected(
    canvas: &CanvasConfig,
    sources: &[CanvasRect],
) -> Result<bool, String> {
    validate_sources_inner(canvas, sources, false)
}

fn validate_sources_inner(
    canvas: &CanvasConfig,
    sources: &[CanvasRect],
    require_connected: bool,
) -> Result<bool, String> {
    if sources.is_empty() || sources.len() > 8 {
        return Err("sources must contain between 1 and 8 regions".into());
    }
    let bounds = canvas.bounds()?;
    let span = bounds.width.max(bounds.height);
    for (index, source) in sources.iter().copied().enumerate() {
        validate_rect_bounds(source, canvas, &format!("source[{index}]"))?;
        if source.width * source.height <= TOPOLOGY_EPS * span * span {
            return Err(format!("source[{index}] area is too small"));
        }
    }

    let mut all_stack = true;
    for i in 0..sources.len() {
        for j in (i + 1)..sources.len() {
            let a = sources[i];
            let b = sources[j];
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
        let mut connected = vec![false; sources.len()];
        connected[0] = true;
        let mut stack = vec![0usize];
        while let Some(current) = stack.pop() {
            for j in 0..sources.len() {
                if connected[j] || j == current {
                    continue;
                }
                if linked(sources[current], sources[j]) {
                    connected[j] = true;
                    stack.push(j);
                }
            }
        }
        if connected.iter().any(|value| !value) {
            return Err(
                "sources are disconnected: gaps and point contacts do not connect regions".into(),
            );
        }
    }

    let mut any_near_total = false;
    for i in 0..sources.len() {
        for j in (i + 1)..sources.len() {
            let overlap = raw_intersection(sources[i], sources[j]);
            let overlap = overlap.width.max(0.0) * overlap.height.max(0.0);
            if overlap >= 0.8 * sources[i].area().min(sources[j].area()) {
                any_near_total = true;
            } else if any_near_total {
                return Err(
                    "mixed_stack_topology: source pairs must all be near-total or ordinary".into(),
                );
            }
        }
    }
    if any_near_total && !all_stack {
        return Err("mixed_stack_topology: source pairs must all be near-total or ordinary".into());
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
    fn source_topology_distinguishes_stack_and_gap() {
        let canvas = CanvasConfig {
            aspect: 1.0,
            render_width: 100,
        };
        assert!(!validate_sources(
            &canvas,
            &[rect(0.0, 0.0, 1.0, 1.0), rect(0.5, 0.0, 1.0, 1.0)]
        )
        .unwrap());
        assert!(validate_sources(
            &canvas,
            &[rect(0.0, 0.0, 1.0, 1.0), rect(1.1, 0.0, 1.0, 1.0)]
        )
        .is_err());
        assert!(validate_sources(
            &canvas,
            &[rect(0.0, 0.0, 1.0, 1.0), rect(0.1, 0.0, 1.0, 1.0)]
        )
        .unwrap());
    }

    #[test]
    fn source_topology_rejects_two_disjoint_chains() {
        let canvas = CanvasConfig {
            aspect: 1.0,
            render_width: 100,
        };
        let sources = [
            rect(0.0, 0.0, 0.2, 1.0),
            rect(0.2, 0.0, 0.2, 1.0),
            rect(0.6, 0.0, 0.2, 1.0),
            rect(0.8, 0.0, 0.2, 1.0),
        ];
        let error = validate_sources(&canvas, &sources).unwrap_err();
        assert!(error.contains("disconnected"), "{error}");
    }

    #[test]
    fn geometry_uses_camel_case_and_defaults_neutral_center() {
        let geometry: OutputGeometry = serde_json::from_str(
            r#"{
                "source":{"x":0,"y":0,"width":1,"height":1},
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
}
