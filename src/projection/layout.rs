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

/// **The seam rule**, stated once here and implemented identically in
/// production ([`Evaluator::raw_weight`] below) and in the independent test
/// oracle (`seam_oracle::Oracle`, generalized to arbitrary convex quads):
///
/// For a source `r` and each of its four edges, that edge is ACTIVE at the
/// queried row (for the left and right edges) or column (for the top and
/// bottom edges) when some other source `q` STRADDLES it there: `q` reaches
/// past the edge on both sides — `q.y < r.y && r.y < bottom(q)` for `r`'s
/// top edge — while covering the queried column (for a top/bottom edge) or
/// row (for a left/right edge). "Straddles" is strict on the axis across the
/// edge, so a neighbor that merely touches the edge never activates it, and
/// closed on the axis along the edge.
///
/// An active edge carries an overlap DEPTH: the largest distance that any
/// straddling neighbor extends inward past that edge at that row or column,
/// clamped to the source's own extent across the edge. For `r`'s top edge
/// that is `max over straddling q of (bottom(q) - r.y)`, capped at
/// `r.height`. Each active edge contributes the ramp
/// `clamp(distance_from_the_point_to_that_edge / depth, 0, 1)`, and the
/// source's RAW weight is the PRODUCT of its active edges' ramps — exactly
/// `1` when no edge of it is active at that point.
///
/// The final weight of a covering source is its raw weight divided by the
/// sum of the raw weights of every source covering the point. `!blend`, or a
/// detected all-pairs stack, gives every covering source weight 1 instead.
///
/// Normalizing each edge by its own overlap depth, and multiplying the ramps
/// instead of taking the minimum distance, is what makes the rule separable.
/// Two strips overlapping by 20% already sum to exactly 1 before
/// normalization, and so does a grid overlapping in both axes, where every
/// source's raw weight is the product of the same two independent 1-D ramps.
/// The previous "minimum distance to an active edge" rule was not separable,
/// and that cost a real four-projector wall: its two rows touched with a
/// 0.07-pixel overlap, which made the bottom row's top edge active with a
/// distance that dominated the minimum for the first 112 rows below the row
/// boundary. The horizontal seam froze at 50/50 there instead of following
/// its gamma ramp, painting a visible bright triangle at the center of the
/// wall. Raising a threshold would not have fixed it: with a genuine
/// vertical overlap band the same minimum still compressed the horizontal
/// gradient inside the band, giving a left share of 1/3 where the geometry
/// asks for 1/4. Dividing by the depth instead makes a sub-pixel sliver
/// produce a ramp that is already clamped to 1 at every sampled pixel
/// center, so it changes nothing at all.
///
/// A source with no active edge at the point is not attenuated: its raw
/// weight is 1, which is the physical answer — nothing overlaps it there, so
/// it has nothing to give away. This is not the old `Some(1.0)` sentinel of
/// review finding A7, which stood in for a *distance* and so invented blend
/// ratios for nested layouts; under this rule a nested source still gets a
/// real ramp, because a neighbor that contains it straddles all four of its
/// edges and supplies four real depths.
///
/// **Boundary convention.** Both [`Evaluator::raw_weight`] (which decides
/// whether a point is covered at all, and evaluates the active-edge ramps)
/// and [`contains`] (used for physical raster-footprint coverage counting)
/// treat a source rectangle as CLOSED: all four edges included. This matches
/// the independent oracle's documented "closed source boundaries"
/// convention, so two rectangles that only touch — no overlap — still split
/// a shared edge 50/50 rather than leaving it undefined. The alternative
/// (half-open, as [`super::blend::Coverage::at`] uses for raw
/// pixel-rectangle *coverage counting*, where avoiding a double count at a
/// shared boundary matters more than symmetry) would leave that shared line
/// owned by neither side here, silently zeroing the blend along it. Being
/// consistent removes a second convention to reason about, at the cost of a
/// physical footprint that exactly touches a neighbor's being double-counted
/// on the one shared line — a measure-zero edge case for a coverage count.
pub(crate) struct Evaluator {
    spec: LayoutSpec,
    sources: Vec<CanvasRect>,
    stacked: bool,
    maximum: u32,
    width: f64,
    vertical_density: f64,
    rows: usize,
    cols: usize,
    /// Per-row overlap depth for every source's left and right edge, indexed
    /// `row * sources.len() + source`. `0.0` means the edge is not active on
    /// that row; an active edge always has a strictly positive depth, because
    /// straddling is strict. Precomputed once per [`Evaluator`] so
    /// `raw_weight` is a table lookup, never a scan over every other source.
    row_left: Vec<f64>,
    row_right: Vec<f64>,
    /// Per-column depths for the top and bottom edges, symmetric to the rows
    /// above and indexed `col * sources.len() + source`.
    col_top: Vec<f64>,
    col_bottom: Vec<f64>,
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
/// Closed rectangle containment — see the seam-rule doc comment above for
/// why this matches [`Evaluator::raw_weight`]'s convention rather than a
/// half-open one.
fn contains(r: &CanvasRect, x: f64, y: f64) -> bool {
    x >= r.x && x <= right(r) && y >= r.y && y <= bottom(r)
}

/// Precompute, once per row and once per column, each source's active-edge
/// overlap DEPTH for its left/right (per row) and top/bottom (per column)
/// edges — see the seam-rule doc comment on [`Evaluator`]. `0.0` records an
/// inactive edge. Left/right depths depend only on the row because the
/// straddle test's "along the edge" axis is Y for a vertical edge;
/// top/bottom depends only on the column for the symmetric reason.
///
/// Every depth is capped at the source's own extent across that edge, so a
/// neighbor reaching clear through the source cannot make its ramp shallower
/// than the source itself.
fn build_edge_depths(
    sources: &[CanvasRect],
    cols: usize,
    rows: usize,
    width: f64,
    vertical_density: f64,
) -> (Vec<f64>, Vec<f64>, Vec<f64>, Vec<f64>) {
    let count = sources.len();
    let mut row_left = vec![0.0; rows * count];
    let mut row_right = vec![0.0; rows * count];
    let mut col_top = vec![0.0; cols * count];
    let mut col_bottom = vec![0.0; cols * count];
    for row in 0..rows {
        let y = (row as f64 + 0.5) / vertical_density;
        for (i, r) in sources.iter().enumerate() {
            let (mut left, mut right_depth) = (0.0_f64, 0.0_f64);
            for (j, q) in sources.iter().enumerate() {
                if i == j || !(q.y <= y && y <= bottom(q)) {
                    continue;
                }
                if q.x < r.x && r.x < right(q) {
                    left = left.max((right(q) - r.x).min(r.width));
                }
                if q.x < right(r) && right(r) < right(q) {
                    right_depth = right_depth.max((right(r) - q.x).min(r.width));
                }
            }
            row_left[row * count + i] = left;
            row_right[row * count + i] = right_depth;
        }
    }
    for col in 0..cols {
        let x = (col as f64 + 0.5) / width;
        for (i, r) in sources.iter().enumerate() {
            let (mut top, mut bottom_depth) = (0.0_f64, 0.0_f64);
            for (j, q) in sources.iter().enumerate() {
                if i == j || !(q.x <= x && x <= right(q)) {
                    continue;
                }
                if q.y < r.y && r.y < bottom(q) {
                    top = top.max((bottom(q) - r.y).min(r.height));
                }
                if q.y < bottom(r) && bottom(r) < bottom(q) {
                    bottom_depth = bottom_depth.max((bottom(r) - q.y).min(r.height));
                }
            }
            col_top[col * count + i] = top;
            col_bottom[col * count + i] = bottom_depth;
        }
    }
    (row_left, row_right, col_top, col_bottom)
}

/// One edge's contribution to a raw weight: `clamp(distance / depth, 0, 1)`,
/// or `1` when the edge is not active (`depth == 0.0`).
fn edge_ramp(distance: f64, depth: f64) -> f64 {
    if depth > 0.0 {
        (distance / depth).clamp(0.0, 1.0)
    } else {
        1.0
    }
}

impl Evaluator {
    pub fn new(spec: &LayoutSpec, width: i32, height: i32) -> Result<Self, String> {
        if width <= 0 || height <= 0 {
            return Err("invalid canvas dimensions".into());
        }
        let canvas = CanvasConfig {
            aspect: spec.aspect,
            render_width: width as u32,
        };
        let dimensions = canvas.dimensions()?;
        if dimensions != (width as u32, height as u32) {
            return Err("layout aspect and actual canvas dimensions disagree".into());
        }
        let sources: Vec<_> = spec.participants.iter().map(|p| p.source).collect();
        // Not `validate_sources`: connectivity is a requirement of
        // shared-canvas *calibration*, decided once at the model/API layer
        // (`crate::model::geometry::validate_sources`, still strict, gates
        // what a user can configure there). By the time a `LayoutSpec`
        // reaches here — legacy integer-position layouts included, see
        // `blend::canvas_plan_with_warp_activation` — disconnection is a
        // fact about the installation, not a schema error: a two-group wall
        // with a deliberate gap simply has two groups that never blend with
        // each other, and rejecting it here would have quietly cost every
        // legacy layout its canvas plan the moment `Evaluator` started being
        // the sole blend-weight rule.
        let stacked =
            crate::model::geometry::validate_sources_allow_disconnected(&canvas, &sources)?;
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
        let (cols, rows) = (width as usize, height as usize);
        let vertical_density = height as f64 * spec.aspect;
        let (row_left, row_right, col_top, col_bottom) =
            build_edge_depths(&sources, cols, rows, width as f64, vertical_density);
        Ok(Self {
            spec: spec.clone(),
            sources,
            stacked,
            maximum,
            width: width as f64,
            vertical_density,
            rows,
            cols,
            row_left,
            row_right,
            col_top,
            col_bottom,
        })
    }

    /// The seam rule (see the doc comment on [`Evaluator`]): the product of
    /// `index`'s active-edge ramps at canvas-unit point `(x, y)`, or `1` when
    /// no edge of it is active there; `None` outside `index`'s own source
    /// rectangle. `row`/`col` are the precomputed-table indices for this
    /// query point — callers derive them once and reuse them across every
    /// source index.
    fn raw_weight(&self, index: usize, x: f64, y: f64, row: usize, col: usize) -> Option<f64> {
        let r = &self.sources[index];
        if x < r.x || x > right(r) || y < r.y || y > bottom(r) {
            return None;
        }
        let count = self.sources.len();
        let (row_base, col_base) = (row * count + index, col * count + index);
        Some(
            edge_ramp(x - r.x, self.row_left[row_base])
                * edge_ramp(right(r) - x, self.row_right[row_base])
                * edge_ramp(y - r.y, self.col_top[col_base])
                * edge_ramp(bottom(r) - y, self.col_bottom[col_base]),
        )
    }

    /// Row/column table indices for a canvas-pixel point, clamped into the
    /// precomputed tables' bounds (a warped sample can land fractionally
    /// anywhere within the canvas, including exactly on its far edge).
    fn cell(&self, cx: f64, cy: f64) -> (usize, usize) {
        let row = (cy.floor() as i64).clamp(0, self.rows as i64 - 1) as usize;
        let col = (cx.floor() as i64).clamp(0, self.cols as i64 - 1) as usize;
        (row, col)
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

    /// Blend weight and physical footprint coverage count at canvas-pixel
    /// point `(cx, cy)` for `index`, or `None` outside the canvas or outside
    /// `index`'s own source rectangle. Shared by [`Self::transfer`] and
    /// [`Self::dynamic_shape`] so the two never compute it differently — see
    /// the seam-rule doc comment on [`Evaluator`].
    fn weight_and_coverage(&self, index: usize, cx: f64, cy: f64) -> Option<(f64, u32)> {
        let (x, y) = (cx / self.width, cy / self.vertical_density);
        if !(0.0..=1.0).contains(&x) || !(0.0..=1.0 / self.spec.aspect).contains(&y) {
            return None;
        }
        let (row, col) = self.cell(cx, cy);
        let own = self.raw_weight(index, x, y, row, col)?;
        let weight = if !self.spec.blend || self.stacked {
            1.0
        } else {
            let mut count = 0;
            let mut total = 0.0;
            for i in 0..self.sources.len() {
                if let Some(d) = self.raw_weight(i, x, y, row, col) {
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
        Some((weight, n))
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
        let Some((weight, n)) = self.weight_and_coverage(index, cx, cy) else {
            return (0, 0);
        };
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
        let Some((weight, n)) = self.weight_and_coverage(index, cx, cy) else {
            return crate::projection::blend::pack_dynamic_shape(0.0, 0.0, 0);
        };
        crate::projection::blend::pack_dynamic_shape(weight.powf(1.0 / gamma), edge, n)
    }
}

#[cfg(test)]
/// A deliberately naive, unoptimized reference for [`Evaluator::raw_weight`]:
/// scans every other source per edge, exactly as an unprecomputed
/// implementation would. Used only to prove that the per-row/per-column
/// depth tables built in `Evaluator::new` agree with it everywhere, never
/// for production.
fn naive_raw_weight(sources: &[CanvasRect], index: usize, x: f64, y: f64) -> Option<f64> {
    let r = &sources[index];
    if x < r.x || x > right(r) || y < r.y || y > bottom(r) {
        return None;
    }
    let (mut left, mut right_depth) = (0.0_f64, 0.0_f64);
    let (mut top, mut bottom_depth) = (0.0_f64, 0.0_f64);
    for (j, q) in sources.iter().enumerate() {
        if j == index {
            continue;
        }
        if q.y <= y && y <= bottom(q) {
            if q.x < r.x && r.x < right(q) {
                left = left.max((right(q) - r.x).min(r.width));
            }
            if q.x < right(r) && right(r) < right(q) {
                right_depth = right_depth.max((right(r) - q.x).min(r.width));
            }
        }
        if q.x <= x && x <= right(q) {
            if q.y < r.y && r.y < bottom(q) {
                top = top.max((bottom(q) - r.y).min(r.height));
            }
            if q.y < bottom(r) && bottom(r) < bottom(q) {
                bottom_depth = bottom_depth.max((bottom(r) - q.y).min(r.height));
            }
        }
    }
    Some(
        edge_ramp(x - r.x, left)
            * edge_ramp(right(r) - x, right_depth)
            * edge_ramp(y - r.y, top)
            * edge_ramp(bottom(r) - y, bottom_depth),
    )
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
        let source = canonical_source(desired, canvas, config).unwrap_or_else(|| {
            geometry
                .source
                .pixel_rect(canvas)
                .expect("canvas already validated above")
        });
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

    /// The precomputed per-row/per-column `Evaluator::raw_weight` must
    /// agree, pixel for pixel, with the unoptimized reference that scans
    /// every other source on every call — proving the depth-table
    /// precomputation in `Evaluator::new` changed performance, not the answer.
    #[test]
    fn precomputed_raw_weight_matches_the_naive_per_pixel_reference() {
        let cases = [
            vec![rect(0.0, 0.0, 0.6, 1.0), rect(0.4, 0.0, 0.6, 1.0)],
            vec![
                rect(0.0, 0.0, 0.6, 0.6),
                rect(0.4, 0.0, 0.6, 0.6),
                rect(0.0, 0.4, 0.6, 0.6),
                rect(0.4, 0.4, 0.6, 0.6),
            ],
            vec![
                rect(0.0, 0.0, 0.4, 1.0),
                rect(0.3, 0.0, 0.4, 1.0),
                rect(0.6, 0.0, 0.4, 1.0),
            ],
        ];
        for rects in cases {
            let input = spec(&rects);
            let evaluator = Evaluator::new(&input, 64, 64).unwrap();
            for y in 0..64 {
                for x in 0..64 {
                    let (cx, cy) = (x as f64 + 0.5, y as f64 + 0.5);
                    let (px, py) = (cx / evaluator.width, cy / evaluator.vertical_density);
                    let (row, col) = evaluator.cell(cx, cy);
                    for i in 0..rects.len() {
                        assert_eq!(
                            evaluator.raw_weight(i, px, py, row, col),
                            naive_raw_weight(&evaluator.sources, i, px, py),
                            "source {i} at ({x}, {y})"
                        );
                    }
                }
            }
        }
    }

    /// Two rectangles that only share the line `x = 0.5` never straddle each
    /// other, so neither has an active edge anywhere: both raw weights are a
    /// flat 1, both sources run at full brightness right up to the shared
    /// line, and the line itself — covered by both — splits 50/50.
    #[test]
    fn a_touching_pair_has_no_active_edge_and_is_never_attenuated() {
        let rects = [rect(0.0, 0.0, 0.5, 1.0), rect(0.5, 0.0, 0.5, 1.0)];
        let input = spec(&rects);
        let evaluator = Evaluator::new(&input, 100, 100).unwrap();
        for cx in [0.5_f64, 5.0, 25.0, 49.5] {
            let (row, col) = evaluator.cell(cx, 50.0);
            let raw = evaluator
                .raw_weight(0, cx / 100.0, 0.5, row, col)
                .expect("covered");
            assert_eq!(raw, 1.0, "raw weight at cx={cx} was {raw}, not 1");
            assert_eq!(evaluator.transfer(0, cx, 50.0, 1.0, 0.0, 1.0).0, 256);
        }
        assert_eq!(evaluator.transfer(0, 50.0, 50.0, 1.0, 0.0, 1.0).0, 128);
        assert_eq!(evaluator.transfer(1, 50.0, 50.0, 1.0, 0.0, 1.0).0, 128);
    }

    /// Property 1: two strips with a 20% overlap give exactly the 1-D ramp
    /// at every row, and normalization is the identity — the raw weights
    /// already sum to 1 before any division.
    #[test]
    fn two_strips_give_the_exact_one_dimensional_ramp_at_every_row() {
        let rects = [rect(0.0, 0.0, 0.6, 1.0), rect(0.4, 0.0, 0.6, 1.0)];
        let input = spec(&rects);
        let evaluator = Evaluator::new(&input, 100, 100).unwrap();
        for y in 0..100 {
            let cy = y as f64 + 0.5;
            for x in 0..100 {
                let cx = x as f64 + 0.5;
                let (row, col) = evaluator.cell(cx, cy);
                let (px, py) = (cx / 100.0, cy / 100.0);
                let raws: Vec<_> = (0..2)
                    .filter_map(|i| evaluator.raw_weight(i, px, py, row, col))
                    .collect();
                // Normalization is the identity: the raw weights of the
                // covering sources already sum to exactly 1.
                let total: f64 = raws.iter().sum();
                assert!(
                    (total - 1.0).abs() < 1e-12,
                    "raw weights at ({cx}, {cy}) summed to {total}, not 1"
                );
                // The 1-D ramp: 1 left of the seam, (0.6 - x)/0.2 inside it,
                // 0 right of it, for source 0; the complement for source 1.
                let expected = ((0.6 - px) / 0.2).clamp(0.0, 1.0);
                let actual = evaluator
                    .raw_weight(0, px, py, row, col)
                    .unwrap_or(0.0 * expected);
                if px <= 0.6 {
                    assert!(
                        (actual - expected).abs() < 1e-12,
                        "source 0 at ({cx}, {cy}) was {actual}, expected {expected}"
                    );
                }
            }
        }
        // And the 75/25 split a quarter of the way into the seam survives
        // the fixed-point encoding, at the top row and the bottom row alike.
        for cy in [0.5, 50.0, 99.5] {
            assert_eq!(evaluator.transfer(0, 45.0, cy, 1.0, 0.0, 1.0).0, 192);
            assert_eq!(evaluator.transfer(1, 45.0, cy, 1.0, 0.0, 1.0).0, 64);
        }
    }

    /// Property 2: a 2x2 grid with 20% overlaps in both axes gives every
    /// source the product of the same two independent 1-D ramps, everywhere,
    /// and those raw products sum to exactly 1 before normalization.
    #[test]
    fn a_grid_gives_the_product_of_two_one_dimensional_ramps_everywhere() {
        let rects = [
            rect(0.0, 0.0, 0.6, 0.6), // 0: top-left
            rect(0.4, 0.0, 0.6, 0.6), // 1: top-right
            rect(0.0, 0.4, 0.6, 0.6), // 2: bottom-left
            rect(0.4, 0.4, 0.6, 0.6), // 3: bottom-right
        ];
        let input = spec(&rects);
        let evaluator = Evaluator::new(&input, 100, 100).unwrap();
        for y in 0..100 {
            let cy = y as f64 + 0.5;
            for x in 0..100 {
                let cx = x as f64 + 0.5;
                let (px, py) = (cx / 100.0, cy / 100.0);
                let (row, col) = evaluator.cell(cx, cy);
                // The two independent 1-D ramps: `a` is the left column's
                // share in x, `b` is the top row's share in y.
                let a = ((0.6 - px) / 0.2).clamp(0.0, 1.0);
                let b = ((0.6 - py) / 0.2).clamp(0.0, 1.0);
                let expected = [a * b, (1.0 - a) * b, a * (1.0 - b), (1.0 - a) * (1.0 - b)];
                let mut total = 0.0;
                for (i, expected) in expected.iter().enumerate() {
                    let Some(actual) = evaluator.raw_weight(i, px, py, row, col) else {
                        assert_eq!(*expected, 0.0, "source {i} at ({cx}, {cy}) is not covered");
                        continue;
                    };
                    assert!(
                        (actual - expected).abs() < 1e-12,
                        "source {i} at ({cx}, {cy}) was {actual}, expected {expected}"
                    );
                    total += actual;
                }
                assert!(
                    (total - 1.0).abs() < 1e-12,
                    "raw weights at ({cx}, {cy}) summed to {total}, not 1"
                );
            }
        }
    }

    /// Brain's four-projector wall, exactly as the daemon holds it: canvas
    /// aspect 1.68 at 3864 px wide, columns overlapping by 225 px and rows
    /// touching with a 0.07-pixel sliver.
    fn brain_layout() -> LayoutSpec {
        let (width, height) = (0.5291176764326357, 0.2976286929933576);
        let (right_x, bottom_y) = (0.47088232356736426, 0.29760940224473764);
        let (footprint_w, footprint_h) = (0.4968944099378882, 0.2795031055900621);
        let (footprint_x, footprint_y) = (0.4554865424430642, 0.23809523809523808);
        let outputs = [
            ("DP-1", 0.0, 0.0, 0.0, 0.0),
            ("DP-2", right_x, 0.0, footprint_x, 0.0),
            ("DP-3", 0.0, bottom_y, 0.0, footprint_y),
            ("DP-4", right_x, bottom_y, footprint_x, footprint_y),
        ];
        LayoutSpec {
            aspect: 1.68,
            blend: true,
            participants: outputs
                .into_iter()
                .map(|(output, x, y, fx, fy)| LayoutParticipant {
                    output: output.into(),
                    source: rect(x, y, width, height),
                    raster_footprint: rect(fx, fy, footprint_w, footprint_h),
                })
                .collect(),
        }
    }

    /// Property 3: on brain's real configuration the horizontal seam between
    /// the bottom two slices (DP-3 and DP-4) is the same at EVERY row inside
    /// them, including the very first row below the row boundary. The
    /// 0.07-pixel sliver where the two rows touch is a real straddle, but its
    /// overlap depth is 0.07 px, so its ramp is already clamped to 1 at every
    /// sampled pixel center and cannot shape the horizontal seam.
    ///
    /// Under the old minimum-distance rule this failed: the first 112 rows
    /// below the boundary were a flat 128/128 instead of the gamma ramp,
    /// which is the bright triangle seen at the center of the wall.
    #[test]
    fn brain_horizontal_seam_has_no_wedge_below_the_row_boundary() {
        let layout = brain_layout();
        let evaluator = Evaluator::new(&layout, 3864, 2300).unwrap();
        let bottom_index = evaluator.index("DP-3").unwrap();
        let other_index = evaluator.index("DP-4").unwrap();
        // The first canvas row whose center lies inside the bottom slices.
        let first = (0.29760940224473764_f64 * 3864.0).ceil() as i32;
        assert_eq!(first, 1150);
        let sample_columns: Vec<f64> = (0..=10)
            .map(|k| (0.47088232356736426 + 0.05823535286527144 * f64::from(k) / 10.0) * 3864.0)
            .collect();
        // Row 1950 is 800 rows below the boundary, far from any sliver.
        let reference: Vec<_> = sample_columns
            .iter()
            .map(|&cx| {
                (
                    evaluator.transfer(bottom_index, cx, 1950.5, 2.2, 0.0, 1.0),
                    evaluator.transfer(other_index, cx, 1950.5, 2.2, 0.0, 1.0),
                )
            })
            .collect();
        for row in first..2300 {
            let cy = f64::from(row) + 0.5;
            for (column, expected) in sample_columns.iter().zip(&reference) {
                let actual = (
                    evaluator.transfer(bottom_index, *column, cy, 2.2, 0.0, 1.0),
                    evaluator.transfer(other_index, *column, cy, 2.2, 0.0, 1.0),
                );
                assert_eq!(
                    actual, *expected,
                    "row {row} at column {column} differs from row 1950"
                );
            }
        }
        // And the ramp really is a ramp, not a plateau. Ten percent into the
        // 225-pixel seam the split is 230/26 (90/10 of 256) on the very first
        // row below the boundary, where the old rule gave a flat 128/128.
        let ten_percent = (0.47088232356736426 + 0.05823535286527144 * 0.1) * 3864.0;
        let (left, _) = evaluator.transfer(bottom_index, ten_percent, 1150.5, 1.0, 0.0, 1.0);
        let (right_gain, _) = evaluator.transfer(other_index, ten_percent, 1150.5, 1.0, 0.0, 1.0);
        assert_eq!((left, right_gain), (230, 26));
    }

    /// Property 4: a sub-pixel overlap sliver is indistinguishable, at every
    /// sampled pixel center, from rectangles that exactly touch. This is the
    /// general statement of what brain's 0.07-pixel row overlap does.
    #[test]
    fn a_sub_pixel_sliver_weighs_the_same_as_exactly_touching_rectangles() {
        let sliver = 0.07 / 100.0;
        let overlapping = spec(&[
            rect(0.0, 0.0, 0.6, 0.5 + sliver),
            rect(0.4, 0.0, 0.6, 0.5 + sliver),
            rect(0.0, 0.5, 0.6, 0.5),
            rect(0.4, 0.5, 0.6, 0.5),
        ]);
        let touching = spec(&[
            rect(0.0, 0.0, 0.6, 0.5),
            rect(0.4, 0.0, 0.6, 0.5),
            rect(0.0, 0.5, 0.6, 0.5),
            rect(0.4, 0.5, 0.6, 0.5),
        ]);
        let a = Evaluator::new(&overlapping, 100, 100).unwrap();
        let b = Evaluator::new(&touching, 100, 100).unwrap();
        for y in 0..100 {
            for x in 0..100 {
                let (cx, cy) = (x as f64 + 0.5, y as f64 + 0.5);
                for i in 0..4 {
                    assert_eq!(
                        a.transfer(i, cx, cy, 2.2, 0.0, 1.0),
                        b.transfer(i, cx, cy, 2.2, 0.0, 1.0),
                        "source {i} at ({cx}, {cy})"
                    );
                }
            }
        }
    }

    /// Sum the gains of every source at one canvas pixel.
    fn gain_sum(evaluator: &Evaluator, count: usize, cx: f64, cy: f64) -> i64 {
        (0..count)
            .map(|i| i64::from(evaluator.transfer(i, cx, cy, 1.0, 0.0, 1.0).0))
            .sum()
    }

    /// Properties 5 and 6: irregular three-way overlaps, a nested layout and
    /// unequal-height partial overlaps all sum to 1 and stay continuous —
    /// no step larger than 2/256 between adjacent pixels along a scan line
    /// that crosses every edge in the layout.
    ///
    /// The canvas is 4000x4000, not the 100x100 used elsewhere in this
    /// module, because "2/256 between adjacent pixels" bounds a SLOPE, and a
    /// slope is only a continuity statement once the pixel pitch is fine
    /// enough to resolve it. A perfectly correct linear ramp across a
    /// 25-pixel seam steps by 10/256 per pixel by construction, and no rule
    /// can do better. The steepest gradient in these four layouts is the
    /// "nested span" case, where the middle source is attenuated to a
    /// quarter by its straddled top and bottom edges while its tall
    /// neighbors are not attenuated vertically at all, so the horizontal
    /// crossover between them happens over a short stretch: about 17 weight
    /// units per canvas unit, which needs roughly 2200 pixels of canvas
    /// before it falls under 2/256 per pixel. Halving the canvas halves
    /// every jump reported here, which is what continuity means.
    #[test]
    fn irregular_layouts_sum_to_one_and_have_no_discontinuity() {
        const SPAN: usize = 4000;
        /// The scan line, halfway down the canvas; `NAN` selects the
        /// symmetric scan straight down the canvas's vertical midline.
        const SCAN: f64 = SPAN as f64 / 2.0 + 0.5;
        let cases: [(&str, Vec<CanvasRect>, f64); 4] = [
            // Three strips with unequal overlaps, seams at every x edge.
            (
                "three-way",
                vec![
                    rect(0.0, 0.0, 0.5, 1.0),
                    rect(0.25, 0.0, 0.55, 1.0),
                    rect(0.4, 0.0, 0.6, 1.0),
                ],
                SCAN,
            ),
            // A short middle source nested in the vertical span of two tall
            // neighbors: its top and bottom edges are both straddled.
            (
                "nested span",
                vec![
                    rect(0.0, 0.0, 0.45, 1.0),
                    rect(0.3, 0.25, 0.4, 0.5),
                    rect(0.55, 0.0, 0.45, 1.0),
                ],
                SCAN,
            ),
            // Unequal heights with a partial overlap, scanned horizontally.
            (
                "unequal heights",
                vec![rect(0.0, 0.0, 0.6, 1.0), rect(0.4, 0.2, 0.6, 0.6)],
                SCAN,
            ),
            // The same layout scanned across its own vertical midline.
            (
                "unequal heights, vertical scan",
                vec![rect(0.0, 0.0, 0.6, 1.0), rect(0.4, 0.2, 0.6, 0.6)],
                f64::NAN,
            ),
        ];
        for (name, rects, scan) in cases {
            let input = spec(&rects);
            let evaluator = Evaluator::new(&input, SPAN as i32, SPAN as i32).unwrap();
            let vertical = scan.is_nan();
            let mut previous: Option<[i64; 8]> = None;
            for step in 0..SPAN {
                let center = step as f64 + 0.5;
                let (cx, cy) = if vertical {
                    (SPAN as f64 / 2.0 + 0.5, center)
                } else {
                    (center, scan)
                };
                let covered = rects
                    .iter()
                    .filter(|r| contains(r, cx / SPAN as f64, cy / SPAN as f64))
                    .count();
                let sum = gain_sum(&evaluator, rects.len(), cx, cy);
                let expected = if covered > 0 { 256 } else { 0 };
                assert!(
                    (sum - expected).abs() <= rects.len() as i64,
                    "{name}: gains at ({cx}, {cy}) summed to {sum}, not {expected}"
                );
                let mut gains = [0i64; 8];
                for (i, gain) in gains.iter_mut().enumerate().take(rects.len()) {
                    *gain = i64::from(evaluator.transfer(i, cx, cy, 1.0, 0.0, 1.0).0);
                }
                if let Some(previous) = previous {
                    for i in 0..rects.len() {
                        assert!(
                            (gains[i] - previous[i]).abs() <= 2,
                            "{name}: source {i} jumped from {} to {} at ({cx}, {cy})",
                            previous[i],
                            gains[i]
                        );
                    }
                }
                previous = Some(gains);
            }
        }
    }
}
