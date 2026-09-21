//! Configured source weights and physical rectangle coverage, independent of
//! connected presenters. No per-pixel allocation or destination-pin inputs.

use serde::{Deserialize, Serialize};

use crate::model::{CanvasConfig, CanvasRect};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LayoutParticipant {
    pub output: String,
    pub source: CanvasRect,
    pub raster_footprint: CanvasRect,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LayoutSpec {
    pub aspect: f64,
    pub blend: bool,
    pub participants: Vec<LayoutParticipant>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LocalKey {
    own_source: CanvasRect,
    aspect: f64,
    blend: bool,
    stacked: bool,
    sources: Vec<CanvasRect>,
    footprints: Vec<CanvasRect>,
    maximum: u32,
    dynamic: bool,
}

pub(crate) struct Evaluator {
    spec: LayoutSpec,
    sources: Vec<CanvasRect>,
    stacked: bool,
    maximum: u32,
    width: f64,
    vertical_density: f64,
}

fn right(r: &CanvasRect) -> f64 {
    r.x + r.width
}
fn bottom(r: &CanvasRect) -> f64 {
    r.y + r.height
}
fn intersects(a: &CanvasRect, b: &CanvasRect) -> bool {
    a.x <= right(b) && b.x <= right(a) && a.y <= bottom(b) && b.y <= bottom(a)
}
fn distance(index: usize, sources: &[CanvasRect], x: f64, y: f64) -> Option<f64> {
    let r = &sources[index];
    if x < r.x || x > right(r) || y < r.y || y > bottom(r) {
        return None;
    }
    // Only edges that overlap with another slice are active seams. Exterior
    // boundaries with no overlapping neighbor must not attenuate the blend.
    let has_left = sources
        .iter()
        .enumerate()
        .any(|(j, q)| j != index && q.x < r.x && r.x < right(q) && q.y <= y && y <= bottom(q));
    let has_right = sources.iter().enumerate().any(|(j, q)| {
        j != index && q.x < right(r) && right(r) < right(q) && q.y <= y && y <= bottom(q)
    });
    let has_top = sources
        .iter()
        .enumerate()
        .any(|(j, q)| j != index && q.y < r.y && r.y < bottom(q) && q.x <= x && x <= right(q));
    let has_bottom = sources.iter().enumerate().any(|(j, q)| {
        j != index && q.y < bottom(r) && bottom(r) < bottom(q) && q.x <= x && x <= right(q)
    });

    let mut min_d = f64::INFINITY;
    if has_left {
        min_d = min_d.min(x - r.x);
    }
    if has_right {
        min_d = min_d.min(right(r) - x);
    }
    if has_top {
        min_d = min_d.min(y - r.y);
    }
    if has_bottom {
        min_d = min_d.min(bottom(r) - y);
    }

    if min_d.is_infinite() {
        Some(1.0)
    } else {
        Some(min_d.max(0.0))
    }
}
fn contains(r: &CanvasRect, x: f64, y: f64) -> bool {
    x >= r.x && x < right(r) && y >= r.y && y < bottom(r)
}

impl Evaluator {
    pub fn new(spec: &LayoutSpec, width: i32, height: i32) -> Result<Self, String> {
        if width <= 0 || height <= 0 {
            return Err("invalid canvas dimensions".into());
        }
        let canvas = CanvasConfig {
            aspect: spec.aspect,
            render_width: width as u32,
            scale: 1.0,
        };
        let dimensions = canvas.dimensions()?;
        if dimensions != (width as u32, height as u32) {
            return Err("layout aspect and actual canvas dimensions disagree".into());
        }
        let sources: Vec<_> = spec.participants.iter().map(|p| p.source).collect();
        let stacked = crate::model::geometry::validate_sources(&canvas, &sources)?;
        let mut names = std::collections::HashSet::new();
        for p in &spec.participants {
            if p.output.is_empty() || !names.insert(&p.output) {
                return Err("layout output identities must be unique and nonempty".into());
            }
            let r = &p.raster_footprint;
            let span = (1.0 / spec.aspect).max(1.0);
            if ![r.x, r.y, r.width, r.height, right(r), bottom(r)]
                .iter()
                .all(|v| v.is_finite() && v.abs() <= 16.0 * span)
                || r.width <= 0.0
                || r.height <= 0.0
            {
                return Err(format!("invalid raster footprint for {}", p.output));
            }
        }
        // Rectangle coverage changes only at footprint edges. Clip the
        // arrangement to the canvas; spill outside it cannot raise N inside.
        let mut xs = vec![0.0, 1.0];
        let mut ys = vec![0.0, 1.0 / spec.aspect];
        for p in &spec.participants {
            xs.extend([
                p.raster_footprint.x.clamp(0.0, 1.0),
                right(&p.raster_footprint).clamp(0.0, 1.0),
            ]);
            ys.extend([
                p.raster_footprint.y.clamp(0.0, 1.0 / spec.aspect),
                bottom(&p.raster_footprint).clamp(0.0, 1.0 / spec.aspect),
            ]);
        }
        xs.sort_by(f64::total_cmp);
        xs.dedup();
        ys.sort_by(f64::total_cmp);
        ys.dedup();
        let mut maximum = 0;
        for x in xs.windows(2) {
            for y in ys.windows(2) {
                let n = spec
                    .participants
                    .iter()
                    .filter(|p| {
                        contains(
                            &p.raster_footprint,
                            (x[0] + x[1]) * 0.5,
                            (y[0] + y[1]) * 0.5,
                        )
                    })
                    .count() as u32;
                maximum = maximum.max(n);
            }
        }
        Ok(Self {
            spec: spec.clone(),
            sources,
            stacked,
            maximum,
            width: width as f64,
            vertical_density: height as f64 * spec.aspect,
        })
    }

    pub fn index(&self, output: &str) -> Result<usize, String> {
        self.spec
            .participants
            .iter()
            .position(|p| p.output == output)
            .ok_or_else(|| format!("presenter {output} is absent from configured layout"))
    }

    pub fn key(&self, index: usize, lift: f64, dynamic: bool) -> LocalKey {
        let source = &self.spec.participants[index].source;
        LocalKey {
            own_source: *source,
            aspect: self.spec.aspect,
            blend: self.spec.blend,
            stacked: self.stacked,
            sources: if self.spec.blend && !self.stacked {
                self.spec
                    .participants
                    .iter()
                    .filter(|p| intersects(source, &p.source))
                    .map(|p| p.source)
                    .collect()
            } else {
                Vec::new()
            },
            footprints: if dynamic || lift > 0.0 {
                self.spec
                    .participants
                    .iter()
                    .filter(|p| intersects(source, &p.raster_footprint))
                    .map(|p| p.raster_footprint)
                    .collect()
            } else {
                Vec::new()
            },
            maximum: if dynamic || lift > 0.0 {
                self.maximum
            } else {
                0
            },
            dynamic,
        }
    }

    /// Maximum physical raster coverage for the complete configured layout.
    /// This is a shared uniform for every dynamic output in one generation.
    pub fn maximum(&self) -> u32 {
        self.maximum
    }

    pub fn transfer(
        &self,
        index: usize,
        cx: f64,
        cy: f64,
        gamma: f64,
        level: f64,
        edge: f64,
    ) -> (u16, u8) {
        let (x, y) = (cx / self.width, cy / self.vertical_density);
        if !(0.0..=1.0).contains(&x) || !(0.0..=1.0 / self.spec.aspect).contains(&y) {
            return (0, 0);
        }
        let Some(own) = distance(index, &self.sources, x, y) else {
            return (0, 0);
        };
        let weight = if !self.spec.blend || self.stacked {
            1.0
        } else {
            let mut count = 0;
            let mut total = 0.0;
            for i in 0..self.sources.len() {
                if let Some(d) = distance(i, &self.sources, x, y) {
                    count += 1;
                    total += d;
                }
            }
            if total > 0.0 {
                own / total
            } else {
                1.0 / f64::from(count)
            }
        };
        let n = self
            .spec
            .participants
            .iter()
            .filter(|p| contains(&p.raster_footprint, x, y))
            .count() as u32;
        let lift = if n > 0 {
            (level * f64::from(self.maximum.saturating_sub(n)) / f64::from(n)).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let ramp = weight.powf(1.0 / gamma);
        (
            (edge * (1.0 - lift) * ramp * 256.0).round() as u16,
            (edge * lift * 255.0).round() as u8,
        )
    }

    /// Dynamic shape entry containing ramp, border coverage and physical
    /// coverage count. The adaptive level is intentionally absent: it is a
    /// shared runtime uniform and changing it must not rebuild this table.
    pub fn dynamic_shape(&self, index: usize, cx: f64, cy: f64, gamma: f64, edge: f64) -> u32 {
        let (x, y) = (cx / self.width, cy / self.vertical_density);
        if !(0.0..=1.0).contains(&x) || !(0.0..=1.0 / self.spec.aspect).contains(&y) {
            return crate::projection::blend::pack_dynamic_shape(0.0, 0.0, 0);
        }
        let Some(own) = distance(index, &self.sources, x, y) else {
            return crate::projection::blend::pack_dynamic_shape(0.0, 0.0, 0);
        };
        let weight = if !self.spec.blend || self.stacked {
            1.0
        } else {
            let mut count = 0;
            let mut total = 0.0;
            for i in 0..self.sources.len() {
                if let Some(d) = distance(i, &self.sources, x, y) {
                    count += 1;
                    total += d;
                }
            }
            if total > 0.0 {
                own / total
            } else {
                1.0 / f64::from(count)
            }
        };
        let n = self
            .spec
            .participants
            .iter()
            .filter(|p| contains(&p.raster_footprint, x, y))
            .count() as u32;
        crate::projection::blend::pack_dynamic_shape(weight.powf(1.0 / gamma), edge, n)
    }
}

/// Whether the complete canvas mapping has the integer translation fast path.
pub(crate) fn exact_source(source: [f64; 4], size: (u32, u32), canvas: (i32, i32)) -> bool {
    source.iter().all(|v| v.is_finite())
        && source[0] >= 0.0
        && source[1] >= 0.0
        && source[0].fract() == 0.0
        && source[1].fract() == 0.0
        && source[2] == size.0 as f64
        && source[3] == size.1 as f64
        && source[0] + source[2] <= canvas.0 as f64
        && source[1] + source[3] <= canvas.1 as f64
}

/// Plan the retained warp configuration without consulting connected displays
/// for source geometry, canvas size, or physical coverage.
pub fn canvas_plan(
    desired: &crate::model::DesiredState,
    observed: &[crate::model::Output],
) -> Result<super::blend::CanvasPlan, String> {
    canvas_plan_with_correction(desired, observed, true)
}

/// Plan the common canvas/source mapping for either presentation mode.
///
/// Source rectangles and physical footprints belong to the installation, not
/// to corner correction.  Simple therefore uses this same plan with no
/// destination geometry, while Warp adds the retained correction below.
pub fn canvas_plan_with_correction(
    desired: &crate::model::DesiredState,
    observed: &[crate::model::Output],
    apply_correction: bool,
) -> Result<super::blend::CanvasPlan, String> {
    let projection = desired
        .projection
        .as_ref()
        .ok_or("projection configuration missing")?;
    let canvas = projection.canvas.as_ref().ok_or("warp canvas missing")?;
    let (width, height) = canvas.dimensions()?;
    let mut layout = LayoutSpec {
        aspect: canvas.aspect,
        blend: projection.blend,
        participants: Vec::new(),
    };
    let mut slices = Vec::new();
    let mut positions = Vec::new();
    for config in desired.outputs.iter().filter(|o| o.enable) {
        let mode = config.effective_mode().ok_or("warp output mode missing")?;
        let (destination_width, destination_height) =
            config.presentation_dimensions(mode, apply_correction)?;
        let geometry = config
            .geometry
            .as_ref()
            .ok_or("warp output geometry missing")?;
        geometry.validate_for_output(canvas, mode.width as u32, mode.height as u32)?;
        let attached = observed.iter().find(|o| config.r#match.matches(o));
        let output = attached
            .map(|o| o.name.clone())
            .unwrap_or_else(|| config.r#match.key());
        layout.participants.push(LayoutParticipant {
            output: output.clone(),
            source: geometry.source,
            raster_footprint: geometry.raster_footprint,
        });
        if attached.is_none() {
            continue;
        }
        let source = canonical_source(desired, canvas, config)
            .unwrap_or_else(|| geometry.source.pixel_rect(canvas));
        slices.push(super::blend::SliceSpec {
            output: output.clone(),
            source: crate::model::Rect {
                x: source[0].floor() as i32,
                y: source[1].floor() as i32,
                width: destination_width,
                height: destination_height,
            },
            source_rect: Some(source),
            geometry: apply_correction.then_some(super::warp::Geometry {
                corners: geometry
                    .corners
                    .map(|p| [p[0] * f64::from(mode.width), p[1] * f64::from(mode.height)]),
                center: geometry.center,
            }),
            ramps: Vec::new(),
        });
        positions.push((config.r#match.key(), output, destination_width));
    }
    Evaluator::new(&layout, width as i32, height as i32)?;
    // Stable output identity and raster sizes, never destination/source pins.
    positions.sort_by(|a, b| a.0.cmp(&b.0));
    let mut x: i32 = 0;
    let mut sway_positions = Vec::new();
    for (_, name, w) in positions {
        sway_positions.push((name, x, 0));
        x = x.checked_add(w).ok_or("output tiling overflow")?;
    }
    Ok(super::blend::CanvasPlan {
        canvas_width: width as i32,
        canvas_height: height as i32,
        layout: Some(layout),
        adaptive_lift: projection.black_lift.adaptive_settings(),
        coverage_rects: Vec::new(),
        slices,
        sway_positions,
    })
}

/// Recover exact integer metadata only when the floating source equals the
/// canonical simple-layout conversion, with its original canvas dimensions.
/// This is an equality proof, not tolerance-based snapping of a user edit.
fn canonical_source(
    desired: &crate::model::DesiredState,
    canvas: &CanvasConfig,
    selected: &crate::model::OutputConfig,
) -> Option<[f64; 4]> {
    let rects: Option<Vec<_>> = desired
        .outputs
        .iter()
        .filter(|o| o.enable)
        .map(|o| {
            let m = o.effective_mode()?;
            let p = o.position?;
            Some((
                i64::from(p.x),
                i64::from(p.y),
                i64::from(m.width),
                i64::from(m.height),
            ))
        })
        .collect();
    let rects = rects?;
    let x = rects.iter().map(|r| r.0).min()?;
    let y = rects.iter().map(|r| r.1).min()?;
    let w = rects.iter().map(|r| r.0 + r.2).max()? - x;
    let h = rects.iter().map(|r| r.1 + r.3).max()? - y;
    if w <= 0
        || h <= 0
        || canvas.dimensions().ok()? != (w as u32, h as u32)
        || canvas.aspect != w as f64 / h as f64
    {
        return None;
    }
    let p = selected.position?;
    let m = selected.effective_mode()?;
    let px = [
        (i64::from(p.x) - x) as f64,
        (i64::from(p.y) - y) as f64,
        f64::from(m.width),
        f64::from(m.height),
    ];
    let canonical = CanvasRect {
        x: px[0] / w as f64,
        y: px[1] / w as f64,
        width: px[2] / w as f64,
        height: px[3] / w as f64,
    };
    (selected.geometry.as_ref()?.source == canonical).then_some(px)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, w: f64, h: f64) -> CanvasRect {
        CanvasRect {
            x,
            y,
            width: w,
            height: h,
        }
    }
    fn spec(rects: &[CanvasRect]) -> LayoutSpec {
        LayoutSpec {
            aspect: 1.0,
            blend: true,
            participants: rects
                .iter()
                .enumerate()
                .map(|(i, r)| LayoutParticipant {
                    output: i.to_string(),
                    source: *r,
                    raster_footprint: *r,
                })
                .collect(),
        }
    }

    #[test]
    fn production_rectangles_normalize_and_sum_to_one() {
        let cases = [
            vec![rect(0.0, 0.0, 0.6, 1.0), rect(0.4, 0.0, 0.6, 1.0)],
            vec![
                rect(0.0, 0.0, 0.6, 0.6),
                rect(0.4, 0.0, 0.6, 0.6),
                rect(0.0, 0.4, 0.6, 0.6),
                rect(0.4, 0.4, 0.6, 0.6),
            ],
            vec![
                rect(0.0, 0.0, 0.6, 0.6),
                rect(0.4, 0.0, 0.6, 0.6),
                rect(0.0, 0.4, 0.6, 0.6),
            ],
            vec![rect(-0.1, 0.0, 0.7, 1.0), rect(0.5, 0.0, 0.7, 1.0)],
            vec![rect(0.0, 0.0, 1.0, 1.0); 3],
        ];
        for rects in cases {
            let input = spec(&rects);
            let production = Evaluator::new(&input, 100, 100).unwrap();
            for y in 0..100 {
                for x in 0..100 {
                    let (cx, cy) = (x as f64 + 0.5, y as f64 + 0.5);
                    let mut sum = 0.0;
                    for i in 0..rects.len() {
                        let (gain, lift) = production.transfer(i, cx, cy, 1.0, 0.0, 1.0);
                        assert_eq!(lift, 0);
                        sum += f64::from(gain) / 256.0;
                    }
                    let covered = rects
                        .iter()
                        .filter(|r| {
                            let (px, py) = (cx / 100.0, cy / 100.0);
                            px >= r.x && px <= right(r) && py >= r.y && py <= bottom(r)
                        })
                        .count();
                    let expected = if production.stacked {
                        covered as f64
                    } else if covered > 0 {
                        1.0
                    } else {
                        0.0
                    };
                    assert!(
                        (sum - expected).abs() <= (rects.len() as f64) * (1.0 / 256.0) + 1e-6,
                        "sum at ({cx}, {cy}) was {sum}, expected {expected}"
                    );
                }
            }
        }
    }

    #[test]
    fn exterior_boundaries_do_not_plateau_or_distort_seams() {
        // 2x2 grid with 20% overlaps (seams at [0.4..0.6] in x and y)
        let rects = [
            rect(0.0, 0.0, 0.6, 0.6), // 0: Top-Left
            rect(0.4, 0.0, 0.6, 0.6), // 1: Top-Right
            rect(0.0, 0.4, 0.6, 0.6), // 2: Bottom-Left
            rect(0.4, 0.4, 0.6, 0.6), // 3: Bottom-Right
        ];
        let input = spec(&rects);
        let production = Evaluator::new(&input, 100, 100).unwrap();

        // Across the vertical seam (y in [40..60]):
        // Compare the interior (x = 80, middle of TR/BR) with the outer edge (x = 99.5, bottom-right corner)
        for y in 41..59 {
            let cy = y as f64 + 0.5;
            let (tr_mid, _) = production.transfer(1, 80.0, cy, 1.0, 0.0, 1.0);
            let (br_mid, _) = production.transfer(3, 80.0, cy, 1.0, 0.0, 1.0);
            let (tr_edge, _) = production.transfer(1, 99.5, cy, 1.0, 0.0, 1.0);
            let (br_edge, _) = production.transfer(3, 99.5, cy, 1.0, 0.0, 1.0);

            // The edge must not freeze at 50% / 128: it must follow the same ramp as the interior
            assert_eq!(tr_edge, tr_mid, "TR mismatch at y={cy}");
            assert_eq!(br_edge, br_mid, "BR mismatch at y={cy}");
        }

        // At y = 45 (25% into the seam from the top), TR should be 75% (192) and BR should be 25% (64)
        // at the very outer edge x = 99.5
        let (tr, _) = production.transfer(1, 99.5, 45.0, 1.0, 0.0, 1.0);
        let (br, _) = production.transfer(3, 99.5, 45.0, 1.0, 0.0, 1.0);
        assert_eq!(tr, 192);
        assert_eq!(br, 64);
    }

    #[test]
    fn ordinary_strip_maintains_ratio_up_to_borders() {
        // Two horizontal displays with 20% overlap at x in [0.4..0.6]
        let rects = [rect(0.0, 0.0, 0.6, 1.0), rect(0.4, 0.0, 0.6, 1.0)];
        let input = spec(&rects);
        let production = Evaluator::new(&input, 100, 100).unwrap();

        // At x = 45 (25% into seam), output 0 should be 75% (192) and output 1 should be 25% (64)
        // Check near top edge (y = 0.5), middle (y = 50.0), and near bottom edge (y = 99.5)
        for cy in [0.5, 10.0, 50.0, 90.0, 99.5] {
            let (a, _) = production.transfer(0, 45.0, cy, 1.0, 0.0, 1.0);
            let (b, _) = production.transfer(1, 45.0, cy, 1.0, 0.0, 1.0);
            assert_eq!(a, 192, "output 0 at cy={cy} was not 192");
            assert_eq!(b, 64, "output 1 at cy={cy} was not 64");
        }
    }

    #[test]
    fn footprint_maximum_is_canvas_clipped_and_does_not_follow_pins() {
        let mut input = spec(&[rect(0.0, 0.0, 0.6, 1.0), rect(0.4, 0.0, 0.6, 1.0)]);
        let a = Evaluator::new(&input, 100, 100).unwrap();
        assert_eq!(a.maximum, 2);
        assert_eq!(a.transfer(0, 10.0, 50.0, 1.0, 0.2, 1.0), (205, 51));
        assert_eq!(a.transfer(0, 50.0, 50.0, 1.0, 0.2, 1.0), (128, 0));
        input.participants[1].raster_footprint = rect(2.0, 0.0, 1.0, 1.0);
        let b = Evaluator::new(&input, 100, 100).unwrap();
        assert_eq!(b.maximum, 1);
        assert_eq!(b.transfer(0, 10.0, 50.0, 1.0, 0.2, 1.0), (256, 0));
        assert_ne!(a.key(0, 0.2, false), b.key(0, 0.2, false));
        assert_eq!(a.key(0, 0.0, false), b.key(0, 0.0, false));
    }

    #[test]
    fn only_intersecting_source_dependencies_change_without_global_lift() {
        let mut input = spec(&[
            rect(0.0, 0.0, 0.4, 1.0),
            rect(0.3, 0.0, 0.4, 1.0),
            rect(0.6, 0.0, 0.4, 1.0),
        ]);
        let before = Evaluator::new(&input, 100, 100).unwrap();
        input.participants[0].source.width = 0.42;
        let after = Evaluator::new(&input, 100, 100).unwrap();
        assert_ne!(before.key(0, 0.1, false), after.key(0, 0.1, false));
        assert_ne!(before.key(1, 0.1, false), after.key(1, 0.1, false));
        assert_eq!(before.key(2, 0.1, false), after.key(2, 0.1, false));
        for y in 0..100 {
            for x in 0..100 {
                assert_eq!(
                    before.transfer(2, x as f64 + 0.5, y as f64 + 0.5, 2.2, 0.1, 1.0),
                    after.transfer(2, x as f64 + 0.5, y as f64 + 0.5, 2.2, 0.1, 1.0)
                );
            }
        }
    }

    #[test]
    fn full_mapping_exactness_uses_actual_dimensions_without_snapping() {
        assert!(exact_source(
            [10.0, 20.0, 100.0, 80.0],
            (100, 80),
            (200, 200)
        ));
        assert!(!exact_source(
            [10.0000000001, 20.0, 100.0, 80.0],
            (100, 80),
            (200, 200)
        ));
        assert!(!exact_source(
            [10.0, 20.0, 100.0, 79.999],
            (100, 80),
            (200, 200)
        ));
        assert!(!exact_source(
            [-1.0, 20.0, 100.0, 80.0],
            (100, 80),
            (200, 200)
        ));
    }

    #[test]
    fn owner_source_bounds_remain_a_dependency_with_blend_and_lift_disabled() {
        let mut input = spec(&[rect(0.0, 0.0, 1.0, 1.0)]);
        input.blend = false;
        let before = Evaluator::new(&input, 100, 100).unwrap();
        input.participants[0].source.x = 0.1;
        input.participants[0].source.width = 0.9;
        let after = Evaluator::new(&input, 100, 100).unwrap();
        assert_ne!(before.key(0, 0.0, false), after.key(0, 0.0, false));
        assert_eq!(before.transfer(0, 5.0, 50.0, 1.0, 0.0, 1.0), (256, 0));
        assert_eq!(after.transfer(0, 5.0, 50.0, 1.0, 0.0, 1.0), (0, 0));
    }

    #[test]
    fn dynamic_keys_ignore_level_but_include_mode() {
        let input = spec(&[rect(0.0, 0.0, 0.6, 1.0), rect(0.4, 0.0, 0.6, 1.0)]);
        let evaluator = Evaluator::new(&input, 100, 100).unwrap();
        assert_eq!(evaluator.key(0, 0.0, true), evaluator.key(0, 0.5, true));
        assert_ne!(evaluator.key(0, 0.0, true), evaluator.key(0, 0.0, false));
    }
}
