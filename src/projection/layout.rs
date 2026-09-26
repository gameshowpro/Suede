//! Configured source weights and physical rectangle coverage, independent of
//! connected presenters. No per-pixel allocation or destination-pin inputs.

use serde::{Deserialize, Serialize};

use crate::model::{CanvasConfig, CanvasRect};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LayoutParticipant {
    pub output: String,
    /// This output's slice: the canvas region it samples, in normalized
    /// canvas units — the same value as `geometry.slice`.
    pub slice: CanvasRect,
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
    own_slice: CanvasRect,
    aspect: f64,
    blend: bool,
    stacked: bool,
    slices: Vec<CanvasRect>,
    footprints: Vec<CanvasRect>,
    maximum: u32,
    dynamic: bool,
}

/// **The seam rule**, stated once here and implemented identically in
/// production ([`Evaluator::raw_weight`] below) and in the independent test
/// oracle (`seam_oracle::Oracle`, which applies it to the bounding box of
/// the intersection polygon so that it can also take arbitrary convex
/// quads; the two are required to agree on rectangles):
///
/// **A seam blends across itself and nothing else.** A horizontal seam
/// fades vertically, a vertical seam fades horizontally, and no overlap
/// with one neighbor may bend the gradient of a seam with another.
///
/// The rule is therefore stated per ORDERED PAIR of sources `(r, q)` whose
/// source rectangles intersect in positive area. Let `I` be that
/// intersection.
///
/// 1. **Seam axis.** `x` when `I.width < I.height`, otherwise `y`: the
///    axis the overlap is thinnest across is the one the seam runs
///    perpendicular to. At a grid corner, where a column overlap and a row
///    overlap meet, this picks the smaller of the two, which coincides with
///    one of the two straight seams meeting there; a square corner may pick
///    either and the two answers agree in a full grid.
/// 2. **The ramp `q` imposes on `r`**, a function of the coordinate along
///    the seam axis only:
///    - if exactly one of `r`'s two edges across that axis lies STRICTLY
///      inside `q`'s extent along it, `r` ramps from that edge:
///      `clamp(distance from the point to that edge / I's extent along the
///      axis, 0, 1)`;
///    - if `q` spans `r` along the axis (BOTH of `r`'s edges are strictly
///      inside `q`'s extent), `r` is the inner slice of the pair: it ramps
///      from both of its edges over HALF its own extent, so its ramp peaks
///      at 1 at its center and falls to 0 at each of its own edges. That is
///      what lets a slice nested inside a larger one fade in and out
///      instead of appearing with a hard edge;
///    - otherwise (`r` spans `q`, or the edges only touch) the pair imposes
///      no ramp on `r` at all: 1.
/// 3. **Cross-range.** The pair's ramp applies only where `q` really covers
///    that row or column, i.e. within `I`'s extent along the OTHER axis
///    (closed, matching [`contains`]); outside it the pair contributes 1.
/// 4. **Combining.** `r`'s RAW weight is `(min over its x-axis pairs'
///    ramps) * (min over its y-axis pairs' ramps)`, each min defaulting to
///    1 when `r` has no pair on that axis. Taking the MINIMUM per axis —
///    not the product — is what stops a diagonal grid neighbor, whose seam
///    axis is the same as an adjacent neighbor's, from squaring a ramp that
///    is already being applied: it repeats it instead.
///
/// The final weight of a covering source is its raw weight divided by the
/// sum of the raw weights of every source covering the point. `!blend`, or a
/// detected all-pairs stack, gives every covering source weight 1 instead.
///
/// Two strips overlapping by 20% and a grid overlapping in both axes are
/// exact: their raw weights already sum to 1 before normalization, so
/// normalization is the identity and every seam carries precisely its 1-D
/// ramp.
///
/// **Why pairwise.** The previous rule was per EDGE: each edge a neighbor
/// straddled carried a ramp across that straddle's depth. On a regular grid
/// that is the same answer, but it reads a neighbor's offset in the wrong
/// axis as a seam. A wall whose right column sat 1% of the canvas width
/// lower than its left column had its top-left slice's bottom edge
/// straddled by the top-RIGHT slice over nearly the slices' whole height,
/// so a "seam" ramp ran down the full height of a column seam: at 25% into
/// the column overlap the split ran 253/3 at the top of the wall to 98/158
/// at the bottom where the geometry asks for a constant 192/64, and the row
/// seam ran 249/7 to 90/166 along its length. Pairing the intersection with
/// its own axis confines each neighbor's influence to the seam it actually
/// forms. The round-2 fix this replaces — dividing each edge's ramp by that
/// overlap's own depth, rather than taking the minimum distance to any
/// active edge — is retained in spirit: a sub-pixel sliver where two rows
/// merely touch still produces a ramp clamped to 1 at every sampled pixel
/// center, so it changes nothing at all.
///
/// A source no pair attenuates at the point is not attenuated: its raw
/// weight is 1, which is the physical answer — nothing overlaps it there, so
/// it has nothing to give away. This is not the old `Some(1.0)` sentinel of
/// review finding A7, which stood in for a *distance* and so invented blend
/// ratios for nested layouts; under this rule a nested source gets the
/// inner-slice ramp of point 2 instead.
///
/// **What this rule does NOT smooth.** Where a slice's own crop ends
/// against the middle of a neighbor — a short slice's top edge inside a
/// tall neighbor, on a pair whose seam axis is the other one — its weight
/// stops at whatever its seam ramp is there rather than fading to zero. The
/// composite stays continuous (the covering weights always sum to 1), but
/// the SPLIT steps at that line, and so does that projector's own image.
/// That is a property of the installation, not of the rule: a projector
/// whose image ends in the middle of another's has a hard edge there
/// whatever weight it is given. The tests assert those steps explicitly
/// rather than hiding them.
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
    slices: Vec<CanvasRect>,
    stacked: bool,
    maximum: u32,
    width: f64,
    vertical_density: f64,
    rows: usize,
    cols: usize,
    /// Per-row ramp denominator for every slice's left and right edge,
    /// indexed `row * slices.len() + source`: the largest denominator any
    /// x-axis pair whose cross-range covers that row asks for from that
    /// edge, or `0.0` for no ramp. The minimum of several ramps measured
    /// from the SAME edge is the ramp over the largest of their
    /// denominators, so one number per edge per row is the exact minimum
    /// over that row's pairs — see [`Evaluator`]'s rule, point 4.
    /// Precomputed once per [`Evaluator`] from [`seam_pairs`] so
    /// `raw_weight` is a table lookup, never a scan over every other slice.
    row_left: Vec<f64>,
    row_right: Vec<f64>,
    /// Per-column denominators for the top and bottom edges, symmetric to
    /// the rows above and indexed `col * slices.len() + source`.
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

/// One ordered pair of overlapping slices `(r, q)`, reduced to the single
/// ramp `q` imposes on `r` — points 1 to 3 of the rule on [`Evaluator`].
/// Built once per [`Evaluator`] by [`seam_pairs`].
#[derive(Clone, Copy, Debug, PartialEq)]
struct SeamPair {
    /// The attenuated slice `r`'s index.
    source: usize,
    /// The seam axis: x (the intersection is narrower than it is tall) or y.
    axis_x: bool,
    /// The intersection's closed extent along the OTHER axis — the rows (for
    /// an x-axis pair) or columns (for a y-axis pair) where `q` actually
    /// covers `r`. The ramp applies only there.
    cross: (f64, f64),
    /// Ramp denominator measured from `r`'s LOW edge across the seam axis
    /// (left for x, top for y) and from its HIGH edge (right, bottom).
    /// `0.0` means this pair asks for no ramp from that edge.
    low: f64,
    high: f64,
}

/// Every ordered pair whose sources overlap in positive area, with the ramp
/// each one imposes — see the seam-rule doc comment on [`Evaluator`]. Pairs
/// that impose nothing (`r` spans `q` along the seam axis) are omitted.
fn seam_pairs(sources: &[CanvasRect]) -> Vec<SeamPair> {
    let mut pairs = Vec::new();
    for (i, r) in sources.iter().enumerate() {
        for (j, q) in sources.iter().enumerate() {
            if i == j {
                continue;
            }
            let (x0, x1) = (r.x.max(q.x), right(r).min(right(q)));
            let (y0, y1) = (r.y.max(q.y), bottom(r).min(bottom(q)));
            // Positive area only: rectangles that merely touch — brain's
            // two rows, before the 0.07-pixel sliver — form no seam at all.
            if !(x1 > x0 && y1 > y0) {
                continue;
            }
            let axis_x = x1 - x0 < y1 - y0;
            let (extent, cross) = if axis_x {
                (x1 - x0, (y0, y1))
            } else {
                (y1 - y0, (x0, x1))
            };
            let (r_low, r_high, q_low, q_high) = if axis_x {
                (r.x, right(r), q.x, right(q))
            } else {
                (r.y, bottom(r), q.y, bottom(q))
            };
            // Strictly inside: an edge a neighbor merely reaches is not a
            // seam, matching the positive-area test above.
            let inside_low = q_low < r_low && r_low < q_high;
            let inside_high = q_low < r_high && r_high < q_high;
            let (low, high) = match (inside_low, inside_high) {
                // `q` spans `r`: `r` is the pair's inner slice, and `extent`
                // is then `r`'s own extent along the axis, so half of it is
                // the "half its own extent" the rule asks for.
                (true, true) => (extent * 0.5, extent * 0.5),
                (true, false) => (extent, 0.0),
                (false, true) => (0.0, extent),
                (false, false) => continue,
            };
            pairs.push(SeamPair {
                source: i,
                axis_x,
                cross,
                low,
                high,
            });
        }
    }
    pairs
}

/// Collapse [`seam_pairs`] into one ramp denominator per source, per edge,
/// per row (for the x-axis pairs) or column (for the y-axis pairs), taking
/// the largest denominator asked of each edge — which is exactly the minimum
/// of those pairs' ramps, since they all measure their distance from the
/// same edge. `0.0` records an edge with no ramp.
fn build_edge_tables(
    pairs: &[SeamPair],
    count: usize,
    cols: usize,
    rows: usize,
    width: f64,
    vertical_density: f64,
) -> (Vec<f64>, Vec<f64>, Vec<f64>, Vec<f64>) {
    let mut row_left = vec![0.0_f64; rows * count];
    let mut row_right = vec![0.0_f64; rows * count];
    let mut col_top = vec![0.0_f64; cols * count];
    let mut col_bottom = vec![0.0_f64; cols * count];
    for row in 0..rows {
        let y = (row as f64 + 0.5) / vertical_density;
        for pair in pairs.iter().filter(|p| p.axis_x) {
            if y < pair.cross.0 || y > pair.cross.1 {
                continue;
            }
            let index = row * count + pair.source;
            row_left[index] = row_left[index].max(pair.low);
            row_right[index] = row_right[index].max(pair.high);
        }
    }
    for col in 0..cols {
        let x = (col as f64 + 0.5) / width;
        for pair in pairs.iter().filter(|p| !p.axis_x) {
            if x < pair.cross.0 || x > pair.cross.1 {
                continue;
            }
            let index = col * count + pair.source;
            col_top[index] = col_top[index].max(pair.low);
            col_bottom[index] = col_bottom[index].max(pair.high);
        }
    }
    (row_left, row_right, col_top, col_bottom)
}

/// How wide a seam-boundary marker line is, in pixels of the output that
/// draws it: the source rectangle scaled to that output's resolution, before
/// any warp. [`Evaluator::marker_widths`] converts this to canvas pixels.
pub(crate) const MARKER_OUTPUT_PIXELS: f64 = 2.0;

/// One marker line, as the half-open canvas-pixel band `[from, to)` it
/// occupies along the axis it is measured across: a vertical line (constant
/// canvas x) for an x-axis marker, a horizontal one for a y-axis marker.
/// `tag` is one of [`super::blend::MARKER_ORANGE`] /
/// [`super::blend::MARKER_BLUE`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct MarkerBand {
    pub tag: u8,
    pub from: f64,
    pub to: f64,
}

impl MarkerBand {
    fn contains(&self, along: f64) -> bool {
        along >= self.from && along < self.to
    }
}

/// The two markers one active ramp contributes, given the ramp's own extent
/// `[low, high]` in canvas pixels, the band `width` in canvas pixels and the
/// colors at each of its ends.
///
/// **Both bands lie INSIDE the ramp**, `[low, low + width)` and
/// `[high - width, high)`. That is what makes the aid work rather than merely draw: the two
/// sources meeting at a seam name the same two boundaries — one source's
/// ramp start is the other's ramp end — so insetting both of them the same
/// way is the only rule under which a correctly aligned pair's orange and
/// blue land on exactly the same canvas pixels and sum to white. Insetting
/// outward instead would put one source's blue line beyond its own rectangle
/// (the blue boundary IS that rectangle's edge), where it has no pixels to
/// draw it with, and would leave the pair reading as adjacent orange and
/// blue stripes rather than one white one.
fn ramp_bands(low: f64, high: f64, width: f64, low_tag: u8, high_tag: u8) -> [MarkerBand; 2] {
    [
        MarkerBand {
            tag: low_tag,
            from: low,
            to: low + width,
        },
        MarkerBand {
            tag: high_tag,
            from: high - width,
            to: high,
        },
    ]
}

/// The tag of the band containing `along`, preferring orange — see the
/// precedence paragraph on [`Evaluator::marker_at`].
fn band_hit(bands: &[Option<MarkerBand>; 4], along: f64) -> u8 {
    let mut tag = super::blend::MARKER_NONE;
    for band in bands.iter().flatten() {
        if band.contains(along) {
            if band.tag == super::blend::MARKER_ORANGE {
                return super::blend::MARKER_ORANGE;
            }
            tag = band.tag;
        }
    }
    tag
}

/// One edge's contribution to a raw weight: `clamp(distance / denominator,
/// 0, 1)`, or `1` when no pair ramps from that edge (`denominator == 0.0`).
fn edge_ramp(distance: f64, denominator: f64) -> f64 {
    if denominator > 0.0 {
        (distance / denominator).clamp(0.0, 1.0)
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
        let slices: Vec<_> = spec.participants.iter().map(|p| p.slice).collect();
        // Not `validate_slices`: connectivity is a requirement of
        // shared-canvas *calibration*, decided once at the model/API layer
        // (`crate::model::geometry::validate_slices`, still strict, gates
        // what a user can configure there). By the time a `LayoutSpec`
        // reaches here — legacy integer-position layouts included, see
        // `blend::canvas_plan_with_warp_activation` — disconnection is a
        // fact about the installation, not a schema error: a two-group wall
        // with a deliberate gap simply has two groups that never blend with
        // each other, and rejecting it here would have quietly cost every
        // legacy layout its canvas plan the moment `Evaluator` started being
        // the sole blend-weight rule.
        let stacked = crate::model::geometry::validate_slices_allow_disconnected(&canvas, &slices)?;
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
        let pairs = seam_pairs(&slices);
        let (row_left, row_right, col_top, col_bottom) = build_edge_tables(
            &pairs,
            slices.len(),
            cols,
            rows,
            width as f64,
            vertical_density,
        );
        Ok(Self {
            spec: spec.clone(),
            slices,
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

    /// The seam rule (see the doc comment on [`Evaluator`]): `index`'s
    /// smallest x-axis pair ramp times its smallest y-axis pair ramp at
    /// canvas-unit point `(x, y)`, or `1` when no pair attenuates it there;
    /// `None` outside `index`'s own source rectangle. `row`/`col` are the
    /// precomputed-table indices for this query point — callers derive them
    /// once and reuse them across every source index, which is what keeps
    /// the per-pixel cost proportional to the number of sources.
    fn raw_weight(&self, index: usize, x: f64, y: f64, row: usize, col: usize) -> Option<f64> {
        let r = &self.slices[index];
        if x < r.x || x > right(r) || y < r.y || y > bottom(r) {
            return None;
        }
        let count = self.slices.len();
        let (row_base, col_base) = (row * count + index, col * count + index);
        let across = edge_ramp(x - r.x, self.row_left[row_base])
            .min(edge_ramp(right(r) - x, self.row_right[row_base]));
        let down = edge_ramp(y - r.y, self.col_top[col_base])
            .min(edge_ramp(bottom(r) - y, self.col_bottom[col_base]));
        Some(across * down)
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
        let slice = &self.spec.participants[index].slice;
        LocalKey {
            own_slice: *slice,
            aspect: self.spec.aspect,
            blend: self.spec.blend,
            stacked: self.stacked,
            slices: if self.spec.blend && !self.stacked {
                self.spec
                    .participants
                    .iter()
                    .filter(|p| intersects(slice, &p.slice))
                    .map(|p| p.slice)
                    .collect()
            } else {
                Vec::new()
            },
            footprints: if dynamic || lift > 0.0 {
                self.spec
                    .participants
                    .iter()
                    .filter(|p| intersects(slice, &p.raster_footprint))
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
            for i in 0..self.slices.len() {
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

    /// The canvas-pixel widths `[x, y]` of source `index`'s marker lines when
    /// it is shown on an output of `size`: [`MARKER_OUTPUT_PIXELS`] of that
    /// output's pre-warp raster, per axis. The evaluator knows nothing about
    /// outputs, so table builders compute this once per output and pass it
    /// to [`Self::marker_at`].
    pub fn marker_widths(&self, index: usize, size: (u32, u32)) -> [f64; 2] {
        let r = &self.slices[index];
        [
            MARKER_OUTPUT_PIXELS * r.width * self.width / f64::from(size.0),
            MARKER_OUTPUT_PIXELS * r.height * self.vertical_density / f64::from(size.1),
        ]
    }

    /// The seam-boundary marker lines source `index` draws across canvas row
    /// `row`, each `width` canvas pixels wide: vertical lines, banded in
    /// canvas-pixel x. At most four — two
    /// from each of this source's own left and right edges — in an array
    /// rather than a `Vec` because [`Self::marker_at`] calls this once per
    /// destination pixel and must not allocate.
    ///
    /// Derived from nothing but [`Self::row_left`] / [`Self::row_right`],
    /// the same precomputed denominators [`Self::raw_weight`] ramps with, so
    /// a marked boundary is *by construction* the boundary the render
    /// actually uses on that row. That is why this reads the compacted
    /// per-row table rather than rescanning [`seam_pairs`]: where more than
    /// one neighbor ramps a row from the same edge, the table has already
    /// collapsed them to the one denominator that wins, and the aid draws
    /// the merged line the picture really has instead of one line per
    /// neighbor. A calibration aid can never disagree with what it is
    /// calibrating; being approximate about a rare multi-neighbor row is the
    /// cheaper half of that trade.
    ///
    /// A sub-pixel ramp denominator — brain's 0.07-pixel row sliver, say —
    /// still counts as a ramp here, exactly as it does in the tables, so its
    /// two bands land within one band width of each other and overpaint. That is
    /// honest: the configuration really does say those rectangles overlap,
    /// and the aid reports the geometry rather than second-guessing it.
    pub(crate) fn row_bands(
        &self,
        index: usize,
        row: usize,
        width: f64,
    ) -> [Option<MarkerBand>; 4] {
        let r = &self.slices[index];
        self.bands(
            self.row_left.get(row * self.slices.len() + index).copied(),
            self.row_right.get(row * self.slices.len() + index).copied(),
            r.x * self.width,
            right(r) * self.width,
            self.width,
            width,
        )
    }

    /// The horizontal marker lines source `index` draws down canvas column
    /// `col`, banded in canvas-pixel y — [`Self::row_bands`] in the other
    /// axis, from [`Self::col_top`] / [`Self::col_bottom`].
    pub(crate) fn column_bands(
        &self,
        index: usize,
        col: usize,
        width: f64,
    ) -> [Option<MarkerBand>; 4] {
        let r = &self.slices[index];
        self.bands(
            self.col_top.get(col * self.slices.len() + index).copied(),
            self.col_bottom
                .get(col * self.slices.len() + index)
                .copied(),
            r.y * self.vertical_density,
            bottom(r) * self.vertical_density,
            self.vertical_density,
            width,
        )
    }

    /// Shared body of [`Self::row_bands`] and [`Self::column_bands`].
    ///
    /// `low`/`high` are the ramp denominators from this source's own low and
    /// high edge in canvas *units*; `low_edge`/`high_edge` are those two
    /// edges in canvas *pixels*, and `density` converts between them.
    /// `width` is each band's width in canvas pixels.
    ///
    /// The ramp from the low edge runs `[low_edge, low_edge + low]`: its low
    /// end is where this source's weight is zero (blue, the outside edge of
    /// the overlap) and its high end is where the gradient starts (orange).
    /// The ramp from the high edge runs `[high_edge - high, high_edge]` with
    /// the colors the other way round. Both are the rule's own definitions,
    /// not a second opinion about where a seam is.
    fn bands(
        &self,
        low: Option<f64>,
        high: Option<f64>,
        low_edge: f64,
        high_edge: f64,
        density: f64,
        width: f64,
    ) -> [Option<MarkerBand>; 4] {
        let mut out = [None; 4];
        // No ramp means no seam to line up against, and a layout that is not
        // blending has no gradient for the orange line to mark the start of.
        // Suppressing it there also keeps this method's dependencies inside
        // what `Evaluator::key` already tracks: `LocalKey` records the
        // neighbor rectangles a marker position depends on only while
        // `blend && !stacked`, so drawing markers outside that could leave a
        // stale table after a neighbor moved.
        if !self.spec.blend || self.stacked {
            return out;
        }
        if let Some(depth) = low.filter(|d| *d > 0.0) {
            let [blue, orange] = ramp_bands(
                low_edge,
                low_edge + depth * density,
                width,
                super::blend::MARKER_BLUE,
                super::blend::MARKER_ORANGE,
            );
            out[0] = Some(blue);
            out[1] = Some(orange);
        }
        if let Some(depth) = high.filter(|d| *d > 0.0) {
            let [orange, blue] = ramp_bands(
                high_edge - depth * density,
                high_edge,
                width,
                super::blend::MARKER_ORANGE,
                super::blend::MARKER_BLUE,
            );
            out[2] = Some(orange);
            out[3] = Some(blue);
        }
        out
    }

    /// The seam-boundary marker tag source `index` paints at canvas-pixel
    /// point `(cx, cy)`: [`super::blend::MARKER_NONE`],
    /// [`super::blend::MARKER_ORANGE`] or [`super::blend::MARKER_BLUE`].
    /// `widths` are the `[x, y]` band widths from [`Self::marker_widths`].
    ///
    /// This is a pure geometry query — it says where the lines are, not what
    /// they do to a pixel. The override itself is applied as the very last
    /// step of both render paths (see [`super::blend::MARKER_NONE`]'s doc),
    /// because the blue line sits exactly where this source's own blend
    /// weight is zero and so would be multiplied away by the transfer it is
    /// meant to be drawn over.
    ///
    /// Nothing is marked outside the canvas or outside `index`'s own source
    /// rectangle — the same closed test [`Self::raw_weight`] applies — so a
    /// marker can never light a destination pixel the transfer would have
    /// left black for want of anything to show there.
    ///
    /// **Precedence.** Orange beats blue wherever both fall on one pixel
    /// (only possible where a ramp is under two band widths deep, or where a
    /// source ramps from both of its edges and the two meet at its center):
    /// orange marks the boundary an operator is actively adjusting, blue
    /// merely records where this output has already faded out. Across axes,
    /// a vertical and a horizontal marker crossing at a grid corner
    /// overpaint in a small patch; that is expected, and which color
    /// wins there follows the same rule.
    pub fn marker_at(&self, index: usize, cx: f64, cy: f64, widths: [f64; 2]) -> u8 {
        if !self.spec.blend || self.stacked {
            return super::blend::MARKER_NONE;
        }
        let (x, y) = (cx / self.width, cy / self.vertical_density);
        if !(0.0..=1.0).contains(&x) || !(0.0..=1.0 / self.spec.aspect).contains(&y) {
            return super::blend::MARKER_NONE;
        }
        let r = &self.slices[index];
        if x < r.x || x > right(r) || y < r.y || y > bottom(r) {
            return super::blend::MARKER_NONE;
        }
        let (row, col) = self.cell(cx, cy);
        let across = band_hit(&self.row_bands(index, row, widths[0]), cx);
        if across == super::blend::MARKER_ORANGE {
            return across;
        }
        let down = band_hit(&self.column_bands(index, col, widths[1]), cy);
        if down != super::blend::MARKER_NONE {
            return down;
        }
        across
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
/// evaluates every pair at the query point and takes the minimum ramp per
/// axis directly, exactly as an unprecomputed implementation would, with no
/// per-edge envelope and no row/column tables. Used only to prove that the
/// tables built in `Evaluator::new` agree with it everywhere, never for
/// production.
fn naive_raw_weight(sources: &[CanvasRect], index: usize, x: f64, y: f64) -> Option<f64> {
    let r = &sources[index];
    if x < r.x || x > right(r) || y < r.y || y > bottom(r) {
        return None;
    }
    let (mut across, mut down) = (1.0_f64, 1.0_f64);
    for pair in seam_pairs(sources).iter().filter(|p| p.source == index) {
        let (along, cross) = if pair.axis_x { (x, y) } else { (y, x) };
        if cross < pair.cross.0 || cross > pair.cross.1 {
            continue;
        }
        let (low_distance, high_distance) = if pair.axis_x {
            (along - r.x, right(r) - along)
        } else {
            (along - r.y, bottom(r) - along)
        };
        let ramp = edge_ramp(low_distance, pair.low).min(edge_ramp(high_distance, pair.high));
        if pair.axis_x {
            across = across.min(ramp);
        } else {
            down = down.min(ramp);
        }
    }
    Some(across * down)
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
            slice: geometry.slice,
            raster_footprint: geometry.raster_footprint,
        });
        if attached.is_none() {
            continue;
        }
        let source = canonical_slice(desired, canvas, config).unwrap_or_else(|| {
            geometry
                .slice
                .pixel_rect(canvas)
                .expect("canvas already validated above")
        });
        slices.push(super::blend::SliceSpec {
            output: output.clone(),
            slice: crate::model::Rect {
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

/// Recover exact integer metadata only when the floating slice equals the
/// canonical simple-layout conversion, with its original canvas dimensions.
/// This is an equality proof, not tolerance-based snapping of a user edit.
fn canonical_slice(
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
    (selected.geometry.as_ref()?.slice == canonical).then_some(px)
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
                    slice: *r,
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
        input.participants[0].slice.width = 0.42;
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
        input.participants[0].slice.x = 0.1;
        input.participants[0].slice.width = 0.9;
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
            // The misaligned grid's proportions on a square canvas: every
            // pair overlaps in both axes, so this is what exercises the
            // cross-ranges and the diagonal pairs that repeat a neighbor's
            // ramp.
            vec![
                rect(0.0, 0.0, 0.53, 0.30),
                rect(0.47, 0.01, 0.53, 0.30),
                rect(0.01, 0.26, 0.53, 0.30),
                rect(0.47, 0.26, 0.53, 0.30),
            ],
            // And the inner-slice branch.
            nested_rects(),
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
                            naive_raw_weight(&evaluator.slices, i, px, py),
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
                    slice: rect(x, y, width, height),
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

    /// One scan line across a layout, with the lines where it is allowed —
    /// and required — to step. See
    /// [`irregular_layouts_sum_to_one_and_step_only_where_a_crop_ends`].
    struct ScanCase {
        name: &'static str,
        rects: Vec<CanvasRect>,
        /// Scan down a column (`true`) or across a row (`false`).
        vertical: bool,
        /// The scan line's own fixed coordinate, in canvas units.
        at: f64,
        /// Canvas-unit coordinates along the scan where some slice's own
        /// crop ends in the middle of a neighbor, so the SPLIT between the
        /// covering slices steps there — see the "What this rule does NOT
        /// smooth" paragraph on [`Evaluator`]. Each one must really step.
        steps: &'static [f64],
    }

    /// Properties 5 and 6: irregular three-way overlaps, a nested layout and
    /// unequal-height partial overlaps all sum to 1, and are continuous
    /// everywhere — no step larger than 2/256 between adjacent pixels along
    /// a scan line that crosses every edge in the layout — EXCEPT where a
    /// slice's own crop ends inside a neighbor. Those lines are listed per
    /// case and asserted to be genuine steps (over 8/256), never quietly
    /// tolerated: a projector whose image simply stops in the middle of
    /// another's has a hard edge there under any weighting, and the rule
    /// does not pretend otherwise. The composite still sums to 1 across
    /// such a line, which this test also checks.
    ///
    /// The canvas is 4000x4000, not the 100x100 used elsewhere in this
    /// module, because "2/256 between adjacent pixels" bounds a SLOPE, and a
    /// slope is only a continuity statement once the pixel pitch is fine
    /// enough to resolve it. A perfectly correct linear ramp across a
    /// 25-pixel seam steps by 10/256 per pixel by construction, and no rule
    /// can do better. The steepest gradient in these layouts is the
    /// "nested span" case, where the middle source's seams with both of its
    /// neighbors are narrow, so the crossover between them happens over a
    /// short stretch. Halving the canvas halves every jump reported here,
    /// which is what continuity means.
    #[test]
    fn irregular_layouts_sum_to_one_and_step_only_where_a_crop_ends() {
        const SPAN: usize = 4000;
        /// The scan line's fixed coordinate, halfway across the canvas.
        const MIDDLE: f64 = 0.5;
        let cases = [
            // Three strips with unequal overlaps, seams at every x edge.
            ScanCase {
                name: "three-way",
                rects: vec![
                    rect(0.0, 0.0, 0.5, 1.0),
                    rect(0.25, 0.0, 0.55, 1.0),
                    rect(0.4, 0.0, 0.6, 1.0),
                ],
                vertical: false,
                at: MIDDLE,
                steps: &[],
            },
            // A short middle source nested in the vertical span of two tall
            // neighbors. Both of its seams are vertical, so a horizontal
            // scan crosses only ramps.
            ScanCase {
                name: "nested span",
                rects: vec![
                    rect(0.0, 0.0, 0.45, 1.0),
                    rect(0.3, 0.25, 0.4, 0.5),
                    rect(0.55, 0.0, 0.45, 1.0),
                ],
                vertical: false,
                at: MIDDLE,
                steps: &[],
            },
            // Unequal heights with a partial overlap, scanned horizontally.
            ScanCase {
                name: "unequal heights",
                rects: vec![rect(0.0, 0.0, 0.6, 1.0), rect(0.4, 0.2, 0.6, 0.6)],
                vertical: false,
                at: MIDDLE,
                steps: &[],
            },
            // The same layout scanned down its own vertical midline, which
            // is inside the column overlap: the short source's top and
            // bottom edges are where its crop ends inside its tall
            // neighbor, and the split steps at both.
            ScanCase {
                name: "unequal heights, vertical scan",
                rects: vec![rect(0.0, 0.0, 0.6, 1.0), rect(0.4, 0.2, 0.6, 0.6)],
                vertical: true,
                at: MIDDLE,
                steps: &[0.2, 0.8],
            },
            // A three-way overlap whose middle source is short: scanned
            // across, every crossing is a ramp.
            ScanCase {
                name: "three-way, short middle",
                rects: vec![
                    rect(0.0, 0.0, 0.5, 1.0),
                    rect(0.25, 0.15, 0.55, 0.7),
                    rect(0.4, 0.0, 0.6, 1.0),
                ],
                vertical: false,
                at: MIDDLE,
                steps: &[],
            },
            // The same three-way overlap scanned down the middle source's
            // left seam: it steps where that source's crop begins and ends.
            ScanCase {
                name: "three-way, short middle, vertical scan",
                rects: vec![
                    rect(0.0, 0.0, 0.5, 1.0),
                    rect(0.25, 0.15, 0.55, 0.7),
                    rect(0.4, 0.0, 0.6, 1.0),
                ],
                vertical: true,
                at: 0.45,
                steps: &[0.15, 0.85],
            },
            // A slice nested inside another across the seam axis, scanned
            // through its interior: its ramp peaks at its center and falls
            // to zero at each of its own left and right edges, so nothing
            // steps.
            ScanCase {
                name: "nested",
                rects: nested_rects(),
                vertical: false,
                at: 0.35,
                steps: &[],
            },
            // The same layout scanned down the nested slice's center,
            // crossing the wide slice's top and bottom edges: that is where
            // the wide slice's own crop ends inside the tall one.
            ScanCase {
                name: "nested, vertical scan",
                rects: nested_rects(),
                vertical: true,
                at: 0.4,
                steps: &[0.2, 0.5],
            },
        ];
        for case in cases {
            let input = spec(&case.rects);
            let evaluator = Evaluator::new(&input, SPAN as i32, SPAN as i32).unwrap();
            let fixed = case.at * SPAN as f64;
            let expected_steps: Vec<usize> = case
                .steps
                .iter()
                .map(|s| (s * SPAN as f64).round() as usize)
                .collect();
            let mut seen_steps = vec![false; expected_steps.len()];
            let mut previous: Option<[i64; 8]> = None;
            for step in 0..SPAN {
                let center = step as f64 + 0.5;
                let (cx, cy) = if case.vertical {
                    (fixed, center)
                } else {
                    (center, fixed)
                };
                let covered = case
                    .rects
                    .iter()
                    .filter(|r| contains(r, cx / SPAN as f64, cy / SPAN as f64))
                    .count();
                let sum = gain_sum(&evaluator, case.rects.len(), cx, cy);
                let expected = if covered > 0 { 256 } else { 0 };
                assert!(
                    (sum - expected).abs() <= case.rects.len() as i64,
                    "{}: gains at ({cx}, {cy}) summed to {sum}, not {expected}",
                    case.name
                );
                let mut gains = [0i64; 8];
                for (i, gain) in gains.iter_mut().enumerate().take(case.rects.len()) {
                    *gain = i64::from(evaluator.transfer(i, cx, cy, 1.0, 0.0, 1.0).0);
                }
                // Where the scan leaves the covered area altogether there is
                // nothing to be continuous with: the canvas is simply dark
                // beyond the last slice.
                if covered == 0 {
                    previous = None;
                    continue;
                }
                if let Some(previous) = previous {
                    let jump = (0..case.rects.len())
                        .map(|i| (gains[i] - previous[i]).abs())
                        .max()
                        .unwrap_or(0);
                    match expected_steps.iter().position(|s| *s == step) {
                        Some(which) => {
                            assert!(
                                jump > 8,
                                "{}: the crop end at {} should step, but the largest change \
                                 at ({cx}, {cy}) was {jump}/256",
                                case.name,
                                case.steps[which]
                            );
                            seen_steps[which] = true;
                        }
                        None => {
                            for i in 0..case.rects.len() {
                                assert!(
                                    (gains[i] - previous[i]).abs() <= 2,
                                    "{}: source {i} jumped from {} to {} at ({cx}, {cy})",
                                    case.name,
                                    previous[i],
                                    gains[i]
                                );
                            }
                        }
                    }
                }
                previous = Some(gains);
            }
            assert!(
                seen_steps.iter().all(|s| *s),
                "{}: not every listed crop end was reached by the scan",
                case.name
            );
        }
    }

    /// A narrow, tall slice (source 1) nested inside a wide, short one
    /// (source 0) ACROSS the seam axis: source 0 spans it in x, which is the
    /// axis their intersection is thinnest across, so source 1 is the
    /// inner slice of the pair and ramps from both of its own edges.
    ///
    /// A slice nested inside another in BOTH axes cannot be tested here,
    /// and not for want of trying: a pair whose intersection is the whole of
    /// the smaller rectangle is a near-total overlap, and
    /// `model::geometry::validate_sources` accepts near-total pairs only
    /// when EVERY pair in the layout is near-total — a deliberate stack,
    /// where every source runs at full brightness and no seam rule applies.
    /// A layout mixing one fully nested pair with any ordinary seam is
    /// rejected outright (`mixed_stack_topology`). Nesting across one axis
    /// is therefore the only nesting a blended layout can contain, and it is
    /// what the inner-slice branch of the rule exists for.
    fn nested_rects() -> Vec<CanvasRect> {
        vec![rect(0.0, 0.2, 1.0, 0.3), rect(0.3, 0.0, 0.2, 0.9)]
    }

    /// The nested slice ramps from both of its own left and right edges
    /// over half its width, so it fades in and out across its host instead
    /// of appearing with a hard vertical edge, peaks at an even half-and-half
    /// split at its center, and the pair sums to 1 at every point.
    #[test]
    fn a_nested_slice_ramps_from_both_of_its_own_edges() {
        const SPAN: i32 = 4000;
        let rects = nested_rects();
        let input = spec(&rects);
        let evaluator = Evaluator::new(&input, SPAN, SPAN).unwrap();
        assert!(
            !evaluator.stacked,
            "nesting across one axis only must stay an ordinary seam layout"
        );
        // Along a row inside the overlap band, at a quarter, a half and
        // three quarters of the way across the nested slice: the tent ramp
        // is 0.5, 1 and 0.5, against the host's flat 1.
        let cy = 0.35 * f64::from(SPAN);
        for (x, expected) in [(0.35, 0.5), (0.4, 1.0), (0.45, 0.5)] {
            let cx = x * f64::from(SPAN);
            let host = f64::from(evaluator.transfer(0, cx, cy, 1.0, 0.0, 1.0).0);
            let inner = f64::from(evaluator.transfer(1, cx, cy, 1.0, 0.0, 1.0).0);
            let share = expected / (1.0 + expected);
            assert!(
                (inner / 256.0 - share).abs() <= 1.0 / 256.0,
                "nested slice at x={x} had share {}, expected {share}",
                inner / 256.0
            );
            assert!(
                (host + inner - 256.0).abs() <= 2.0,
                "nested pair at x={x} summed to {}",
                host + inner
            );
        }
        // And it really does reach zero at its own edges, where the host
        // takes the whole pixel back.
        for x in [0.3, 0.5] {
            let cx = x * f64::from(SPAN);
            assert_eq!(evaluator.transfer(1, cx, cy, 1.0, 0.0, 1.0).0, 0);
            assert_eq!(evaluator.transfer(0, cx, cy, 1.0, 0.0, 1.0).0, 256);
        }
    }

    /// Every marker band a source draws across one canvas row, in order.
    fn row_marker_list(
        evaluator: &Evaluator,
        index: usize,
        row: usize,
        width: f64,
    ) -> Vec<MarkerBand> {
        evaluator
            .row_bands(index, row, width)
            .into_iter()
            .flatten()
            .collect()
    }

    /// A marker line is two pixels of the output that draws it, not two
    /// canvas pixels. On brain, a 0.5277 x 0.2967 source of a 900-pixel
    /// canvas shown at 1920x1080 has about 4.04 output pixels per canvas
    /// pixel, so each line is just under half a canvas pixel wide.
    #[test]
    fn marker_widths_are_two_pixels_of_the_scaled_output() {
        let evaluator = Evaluator::new(&spec(&[rect(0.0, 0.0, 0.5277, 0.2967)]), 900, 900).unwrap();
        let [x, y] = evaluator.marker_widths(0, (1920, 1080));
        assert!((x - 2.0 * 0.5277 * 900.0 / 1920.0).abs() < 1e-12, "x {x}");
        assert!((y - 2.0 * 0.2967 * 900.0 / 1080.0).abs() < 1e-12, "y {y}");
        assert!((x - 0.4947).abs() < 1e-4 && (y - 0.4945).abs() < 1e-4);
        // A 60x100-pixel source shown at 30x50 has two canvas pixels per
        // output pixel in each axis, so its lines are four canvas pixels.
        let rects = [rect(0.0, 0.0, 0.6, 1.0), rect(0.4, 0.0, 0.6, 1.0)];
        let evaluator = Evaluator::new(&spec(&rects), 100, 100).unwrap();
        assert_eq!(evaluator.marker_widths(0, (30, 50)), [4.0, 4.0]);
        assert_eq!(evaluator.marker_widths(1, (120, 200)), [1.0, 1.0]);
    }

    /// Two strips overlapping by 20% of a 100-pixel canvas, so the overlap is
    /// the band `x = 40..60`. Hand-computed, from the rule and nothing else:
    ///
    /// - source 0 spans `0..60` and ramps from its RIGHT edge over a
    ///   denominator of 0.2 canvas units, so its ramp runs `40..60`. Orange
    ///   (gradient start) is at 40, blue (weight zero) at its own edge, 60.
    /// - source 1 spans `40..100` and ramps from its LEFT edge over the same
    ///   denominator, so its ramp runs `40..60` too. Blue is at its own edge,
    ///   40, and orange at 60.
    ///
    /// Both are shown at 30x50, so a line is four canvas pixels (two output
    /// pixels) lying INSIDE the overlap at each boundary, `40..44` and
    /// `56..60`. That is what puts the two sources' lines on the same pixels
    /// rather than either side of them. See [`ramp_bands`].
    #[test]
    fn two_strips_mark_both_ends_of_the_overlap_they_actually_blend_across() {
        use crate::projection::blend::{MARKER_BLUE, MARKER_NONE, MARKER_ORANGE};
        let rects = [rect(0.0, 0.0, 0.6, 1.0), rect(0.4, 0.0, 0.6, 1.0)];
        let evaluator = Evaluator::new(&spec(&rects), 100, 100).unwrap();
        let widths = evaluator.marker_widths(0, (30, 50));
        assert_eq!(widths, evaluator.marker_widths(1, (30, 50)));
        let band = |tag, from, to| MarkerBand { tag, from, to };
        // A vertical seam is the same at every row, the very first and last
        // included: these are the seam rule's own per-row tables.
        for row in [0, 1, 50, 98, 99] {
            assert_eq!(
                row_marker_list(&evaluator, 0, row, widths[0]),
                vec![
                    band(MARKER_ORANGE, 40.0, 44.0),
                    band(MARKER_BLUE, 56.0, 60.0)
                ],
                "source 0, row {row}"
            );
            assert_eq!(
                row_marker_list(&evaluator, 1, row, widths[0]),
                vec![
                    band(MARKER_BLUE, 40.0, 44.0),
                    band(MARKER_ORANGE, 56.0, 60.0)
                ],
                "source 1, row {row}"
            );
            // A vertical seam draws no horizontal lines.
            assert_eq!(evaluator.column_bands(0, row, widths[1]), [None; 4]);
            assert_eq!(evaluator.column_bands(1, row, widths[1]), [None; 4]);
        }
        // And the per-pixel query agrees, pixel for pixel, along a row.
        for x in 0..100 {
            let cx = f64::from(x) + 0.5;
            let expected = |low, high, outside| match x {
                40..=43 => low,
                56..=59 => high,
                _ => outside,
            };
            assert_eq!(
                evaluator.marker_at(0, cx, 50.0, widths),
                expected(MARKER_ORANGE, MARKER_BLUE, MARKER_NONE),
                "source 0 at x={x}"
            );
            // Source 1 covers nothing left of x = 40, so nothing is marked
            // there however close the band comes.
            assert_eq!(
                evaluator.marker_at(1, cx, 50.0, widths),
                expected(MARKER_BLUE, MARKER_ORANGE, MARKER_NONE),
                "source 1 at x={x}"
            );
        }
    }

    /// With the two strips shown at different scales, each line is two
    /// pixels of its own output: source 0 at 30x50 draws four-canvas-pixel
    /// lines and source 1 at 60x100 draws two-canvas-pixel ones. Both stay
    /// anchored at the same boundaries, inside the overlap.
    #[test]
    fn unequal_scales_give_each_output_its_own_two_pixel_lines() {
        use crate::projection::blend::{MARKER_BLUE, MARKER_ORANGE};
        let rects = [rect(0.0, 0.0, 0.6, 1.0), rect(0.4, 0.0, 0.6, 1.0)];
        let evaluator = Evaluator::new(&spec(&rects), 100, 100).unwrap();
        let band = |tag, from, to| MarkerBand { tag, from, to };
        let coarse = evaluator.marker_widths(0, (30, 50));
        let fine = evaluator.marker_widths(1, (60, 100));
        assert_eq!((coarse[0], fine[0]), (4.0, 2.0));
        assert_eq!(
            row_marker_list(&evaluator, 0, 50, coarse[0]),
            vec![
                band(MARKER_ORANGE, 40.0, 44.0),
                band(MARKER_BLUE, 56.0, 60.0)
            ]
        );
        assert_eq!(
            row_marker_list(&evaluator, 1, 50, fine[0]),
            vec![
                band(MARKER_BLUE, 40.0, 42.0),
                band(MARKER_ORANGE, 58.0, 60.0)
            ]
        );
    }

    /// The design intent, asserted rather than assumed: at each of the two
    /// boundaries of a correctly aligned overlap, one output paints orange
    /// and the other paints blue over the very same canvas pixels, and the
    /// two sum to white in light at each configured gamma — the wall adds
    /// light, not signal, so the check decodes each channel first. A projector that is out of
    /// line instead shows its orange beside its neighbor's blue, and the
    /// operator sees a colored fringe where a white line should be.
    #[test]
    fn a_lined_up_pair_sums_its_two_marker_colors_to_white() {
        use crate::projection::blend::{MarkerPalette, MARKER_BLUE, MARKER_NONE, MARKER_ORANGE};
        let rects = [rect(0.0, 0.0, 0.6, 1.0), rect(0.4, 0.0, 0.6, 1.0)];
        let evaluator = Evaluator::new(&spec(&rects), 100, 100).unwrap();
        let widths = evaluator.marker_widths(0, (30, 50));
        let mut coincidences = 0;
        for x in 0..100 {
            let cx = f64::from(x) + 0.5;
            let near = evaluator.marker_at(0, cx, 50.0, widths);
            let far = evaluator.marker_at(1, cx, 50.0, widths);
            if near == MARKER_NONE && far == MARKER_NONE {
                continue;
            }
            // Never the same color on one pixel: one output is starting its
            // gradient exactly where the other has finished fading out.
            assert_ne!(near, far, "both outputs painted the same color at x={x}");
            assert!(near == MARKER_ORANGE || near == MARKER_BLUE);
            assert!(far == MARKER_ORANGE || far == MARKER_BLUE);
            for gamma in [1.8, 2.2, 2.4] {
                let palette = MarkerPalette::for_gamma(gamma);
                let a = palette.color(near).expect("a palette color");
                let b = palette.color(far).expect("a palette color");
                let light = |v: u8| (f64::from(v) / 255.0).powf(gamma);
                for i in 0..3 {
                    let sum = light(a[i]) + light(b[i]);
                    assert!(
                        (sum - 1.0).abs() < 0.01,
                        "x={x} gamma {gamma} channel {i} summed to {sum} in light"
                    );
                }
            }
            coincidences += 1;
        }
        // Eight canvas pixels: a four-pixel line at each end of the overlap.
        assert_eq!(coincidences, 8);
    }

    /// The inner-slice branch of the rule ramps a nested source from BOTH of
    /// its own edges over half its own extent, so it gets two complete
    /// marker pairs — blue at each of its edges, orange at each end of the
    /// tent — where an ordinary seam gets one. No special case produces
    /// this: the same two tables, read the same way.
    ///
    /// [`nested_rects`]'s inner slice spans `x = 0.3..0.5` and is spanned in
    /// x by its host, so each of its ramps is half its own width, 0.1 canvas
    /// units. On a 4000-pixel canvas that is `1200..1600` and `1600..2000`.
    /// Shown at 400 pixels wide, its 800 canvas pixels make each line four
    /// canvas pixels, and the two orange lines meet at the tent's peak as
    /// one line twice that width — which is what a source fading in and out
    /// looks like, not a defect.
    #[test]
    fn a_nested_slice_produces_two_marker_pairs_not_one() {
        use crate::projection::blend::{MARKER_BLUE, MARKER_ORANGE};
        const SPAN: i32 = 4000;
        let evaluator = Evaluator::new(&spec(&nested_rects()), SPAN, SPAN).unwrap();
        let widths = evaluator.marker_widths(1, (400, 1800));
        assert_eq!(widths, [4.0, 4.0]);
        // A row inside the band where the two rectangles really overlap.
        let row = 1400usize;
        let band = |tag, from, to| MarkerBand { tag, from, to };
        assert_eq!(
            row_marker_list(&evaluator, 1, row, widths[0]),
            vec![
                band(MARKER_BLUE, 1200.0, 1204.0),
                band(MARKER_ORANGE, 1596.0, 1600.0),
                band(MARKER_ORANGE, 1600.0, 1604.0),
                band(MARKER_BLUE, 1996.0, 2000.0),
            ]
        );
        // Two pairs: two blues at the slice's own edges, where its weight is
        // zero, and two oranges meeting at its center, where it peaks.
        assert_eq!(
            row_marker_list(&evaluator, 1, row, widths[0])
                .iter()
                .filter(|b| b.tag == MARKER_BLUE)
                .count(),
            2
        );
        let cy = row as f64 + 0.5;
        for x in [1200, 1203, 1996, 1999] {
            assert_eq!(
                evaluator.marker_at(1, f64::from(x) + 0.5, cy, widths),
                MARKER_BLUE
            );
        }
        for x in [1596, 1599, 1600, 1603] {
            assert_eq!(
                evaluator.marker_at(1, f64::from(x) + 0.5, cy, widths),
                MARKER_ORANGE
            );
        }
        // The host spans the inner slice, so no pair ramps it in x and it
        // draws no vertical line of its own — the rule's "r spans q imposes
        // nothing" case, reported faithfully.
        assert_eq!(evaluator.row_bands(0, row, widths[0]), [None; 4]);
    }

    /// Markers are geometry, not decoration: nothing is marked outside the
    /// source's own rectangle, and a layout that is not blending — `blend`
    /// off, or a detected stack, where every covering source runs at full
    /// weight and there is no gradient for a line to mark the start of — has
    /// no markers at all.
    #[test]
    fn markers_stay_inside_their_own_source_and_need_a_blend_to_exist() {
        use crate::projection::blend::MARKER_NONE;
        let rects = [rect(0.0, 0.0, 0.6, 1.0), rect(0.4, 0.0, 0.6, 1.0)];
        let evaluator = Evaluator::new(&spec(&rects), 100, 100).unwrap();
        let widths = evaluator.marker_widths(0, (30, 50));
        for (index, outside) in [(0usize, 70.0_f64), (1, 20.0)] {
            assert_eq!(
                evaluator.marker_at(index, outside, 50.0, widths),
                MARKER_NONE
            );
        }
        let mut unblended = spec(&rects);
        unblended.blend = false;
        let unblended = Evaluator::new(&unblended, 100, 100).unwrap();
        for x in 0..100 {
            for index in 0..2 {
                assert_eq!(
                    unblended.marker_at(index, f64::from(x) + 0.5, 50.0, widths),
                    MARKER_NONE
                );
            }
        }
        assert_eq!(unblended.row_bands(0, 50, widths[0]), [None; 4]);
        let stacked = Evaluator::new(&spec(&[rect(0.0, 0.0, 1.0, 1.0); 3]), 100, 100).unwrap();
        assert!(stacked.stacked);
        assert_eq!(stacked.marker_at(0, 50.0, 50.0, widths), MARKER_NONE);
    }

    /// A 2x2 wall that is slightly out of true, on brain's canvas: the
    /// right column sits 1% of the canvas width lower than the left column,
    /// and the bottom row 1% further right than the top row. Every seam is
    /// still straight — only the slices meeting along it are offset.
    fn misaligned_grid() -> LayoutSpec {
        let (width, height) = (0.53, 0.30);
        let outputs = [
            ("TL", 0.0, 0.0),
            ("TR", 0.47, 0.01),
            ("BL", 0.01, 0.26),
            ("BR", 0.47, 0.26),
        ];
        LayoutSpec {
            aspect: 1.68,
            blend: true,
            participants: outputs
                .into_iter()
                .map(|(output, x, y)| LayoutParticipant {
                    output: output.into(),
                    slice: rect(x, y, width, height),
                    raster_footprint: rect(x, y, width, height),
                })
                .collect(),
        }
    }

    /// Every canvas-pixel center along one seam of [`misaligned_grid`],
    /// between `from` and `to` in canvas units. `across` is the fixed
    /// canvas-unit coordinate ACROSS the seam; `column_seam` says whether
    /// the seam runs down the rows (a vertical seam between two columns) or
    /// along the columns (a horizontal seam between two rows).
    ///
    /// Canvas units are normalized by the canvas WIDTH in both axes, so one
    /// pixel is `1/3864` of a unit either way; the vertical axis simply
    /// stops at row 2300.
    fn misaligned_seam_points(
        column_seam: bool,
        across: f64,
        from: f64,
        to: f64,
    ) -> Vec<(f64, f64)> {
        const WIDTH: f64 = 3864.0;
        let steps = if column_seam { 2300 } else { 3864 };
        (0..steps)
            .map(|s| f64::from(s) + 0.5)
            .filter(|along| (from..to).contains(&(along / WIDTH)))
            .map(|along| {
                if column_seam {
                    (across * WIDTH, along)
                } else {
                    (along, across * WIDTH)
                }
            })
            .collect()
    }

    /// Round 3's acceptance test. On [`misaligned_grid`] every seam carries
    /// the ratio its own geometry asks for, unchanged along its whole
    /// length: a quarter of the way into an overlap is 192/64 at every
    /// point of that seam where only its own pair covers.
    ///
    /// Under the round-2 per-edge rule the two seams that involve the
    /// offset slices ran a gradient along their own length instead. At 25%
    /// into the TL/TR column overlap the split ran 253/3 at the top of the
    /// wall to 98/158 near the bottom of those slices, and at 25% into the
    /// TL/BL row overlap it ran 249/7 at the left to 90/166 at the right:
    /// the 1% offset made each slice straddle its neighbor's edge in the
    /// WRONG axis by nearly a whole slice, and that straddle became a ramp
    /// running the length of the seam. The two seams between slices that
    /// are NOT offset from each other (BL/BR and TR/BR) were already
    /// constant, and still are.
    ///
    /// Each seam is checked over the stretch where only its own pair
    /// covers. In the corner where all four slices meet, the left column's
    /// row overlap is 0.04 canvas units deep and the right column's 0.05:
    /// two different vertical ramps multiply the two columns' shares there,
    /// so the column ratio in the corner is not 3:1 and cannot be under any
    /// separable rule. That is a property of the misalignment, four slices
    /// wide and one corner in size, not a gradient running down a seam.
    #[test]
    fn a_misaligned_grid_blends_each_seam_across_itself_only() {
        let evaluator = Evaluator::new(&misaligned_grid(), 3864, 2300).unwrap();
        let index = |name: &str| evaluator.index(name).unwrap();
        // (name, near slice, far slice, a column seam?, a quarter of the
        // way into the overlap, and the stretch of the seam over which only
        // this pair covers).
        let seams = [
            ("TL/TR column seam", "TL", "TR", true, 0.485, 0.01, 0.26),
            ("BL/BR column seam", "BL", "BR", true, 0.4875, 0.31, 0.56),
            ("TL/BL row seam", "TL", "BL", false, 0.27, 0.01, 0.47),
            ("TR/BR row seam", "TR", "BR", false, 0.2725, 0.54, 1.0),
        ];
        for (name, near, far, column_seam, across, from, to) in seams {
            let points = misaligned_seam_points(column_seam, across, from, to);
            assert!(
                points.len() > 100,
                "{name}: only {} sampled points",
                points.len()
            );
            for (cx, cy) in points {
                let gains = (
                    evaluator.transfer(index(near), cx, cy, 1.0, 0.0, 1.0).0,
                    evaluator.transfer(index(far), cx, cy, 1.0, 0.0, 1.0).0,
                );
                assert_eq!(
                    gains,
                    (192, 64),
                    "{name}: {near}/{far} at ({cx}, {cy}) is not the quarter-way ratio"
                );
            }
        }
    }
}
