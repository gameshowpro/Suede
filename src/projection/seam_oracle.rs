//! D3 reference weights, deliberately test-only until geometry-model integration.
//!
//! These are shared *source coverage* polygons, never destination pins. Public
//! pinning still selects a fixed source rectangle. The supported topology,
//! the boundary convention and the pairwise seam rule shared with production
//! are documented on the items below and on `layout::Evaluator`.
//!
//! The oracle takes arbitrary convex quads, but it implements the seam rule
//! on the axis-aligned BOUNDING BOX of each intersection polygon and of each
//! source. It is therefore required to agree with production exactly on
//! axis-aligned RECTANGLES — which is all a configured layout ever contains,
//! and what the per-pixel comparison tests check — while on skewed quads it
//! is only a well-defined reference of its own.

type CanvasPoint = [f64; 2];
type SourceQuad = [CanvasPoint; 4];
type Polygon = Vec<CanvasPoint>;

// Normalize by the larger canvas dimension before all geometric operations.
const LENGTH_EPS: f64 = 1e-10;
const AREA_EPS: f64 = 1e-12;
const MAX_PARTICIPANTS: usize = 8;

#[derive(Debug)]
pub(super) struct Oracle {
    scale: f64,
    canvas: CanvasPoint,
    sources: Vec<Polygon>,
    visible: Vec<Polygon>,
    stacked: bool,
    /// Every seam, built once from `sources` — see [`seam_pairs`].
    pairs: Vec<SeamPair>,
}

fn sub(a: CanvasPoint, b: CanvasPoint) -> CanvasPoint {
    [a[0] - b[0], a[1] - b[1]]
}

fn cross(a: CanvasPoint, b: CanvasPoint) -> f64 {
    a[0] * b[1] - a[1] * b[0]
}

fn area(poly: &[CanvasPoint]) -> f64 {
    (0..poly.len())
        .map(|i| cross(poly[i], poly[(i + 1) % poly.len()]))
        .sum::<f64>()
        * 0.5
}

fn edges(poly: &[CanvasPoint]) -> impl Iterator<Item = (CanvasPoint, CanvasPoint)> + '_ {
    (0..poly.len()).map(|i| (poly[i], poly[(i + 1) % poly.len()]))
}

/// Convex clipping retains every intersection vertex (up to eight for quads).
fn intersection(subject: &[CanvasPoint], clip: &[CanvasPoint]) -> Polygon {
    let mut result = subject.to_vec();
    for (a, b) in edges(clip) {
        let input = std::mem::take(&mut result);
        for (p, q) in edges(&input) {
            let dp = cross(sub(b, a), sub(p, a));
            let dq = cross(sub(b, a), sub(q, a));
            if dp >= 0.0 {
                result.push(p);
            }
            if (dp >= 0.0) != (dq >= 0.0) {
                let t = dp / (dp - dq);
                result.push([p[0] + t * (q[0] - p[0]), p[1] + t * (q[1] - p[1])]);
            }
        }
    }
    result
}

/// Shared nonzero edge segments connect a layout; a shared vertex does not.
fn shared_edge(a: &[CanvasPoint], b: &[CanvasPoint]) -> bool {
    edges(a).any(|(p, q)| {
        let d = sub(q, p);
        let len = d[0].hypot(d[1]);
        if len <= LENGTH_EPS {
            return false;
        }
        let u = [d[0] / len, d[1] / len];
        edges(b).any(|(r, s)| {
            let r = sub(r, p);
            let s = sub(s, p);
            if cross(u, r).abs() > LENGTH_EPS || cross(u, s).abs() > LENGTH_EPS {
                return false;
            }
            let t0 = r[0] * u[0] + r[1] * u[1];
            let t1 = s[0] * u[0] + s[1] * u[1];
            t0.max(t1).min(len) - t0.min(t1).max(0.0) > LENGTH_EPS
        })
    })
}

/// Minimum inward perpendicular edge distance, or None outside a convex quad.
fn inward_distance(poly: &[CanvasPoint], p: CanvasPoint) -> Option<f64> {
    let mut distance = f64::INFINITY;
    for (a, b) in edges(poly) {
        let d = sub(b, a);
        let signed = cross(d, sub(p, a)) / d[0].hypot(d[1]);
        // Do not expand coverage with an epsilon: that would introduce weight
        // dependencies outside the polygons used by affected().
        if signed < 0.0 {
            return None;
        }
        distance = distance.min(signed.max(0.0));
    }
    Some(distance)
}

/// The projected extent `[min, max]` of `poly`'s vertices onto unit
/// direction `dir`.
fn project(poly: &[CanvasPoint], dir: CanvasPoint) -> (f64, f64) {
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for v in poly {
        let t = v[0] * dir[0] + v[1] * dir[1];
        lo = lo.min(t);
        hi = hi.max(t);
    }
    (lo, hi)
}

/// A polygon's axis-aligned bounding box as `(low corner, high corner)`.
fn bounds(poly: &[CanvasPoint]) -> (CanvasPoint, CanvasPoint) {
    let (lo_x, hi_x) = project(poly, [1.0, 0.0]);
    let (lo_y, hi_y) = project(poly, [0.0, 1.0]);
    ([lo_x, lo_y], [hi_x, hi_y])
}

/// One ordered pair of overlapping sources `(r, q)`, reduced to the single
/// ramp `q` imposes on `r` — the seam rule stated once on
/// `layout::Evaluator`, applied here to the axis-aligned BOUNDING BOX of the
/// intersection polygon and to each source's own bounding box.
#[derive(Clone, Copy, Debug)]
struct SeamPair {
    /// The attenuated source `r`.
    source: usize,
    /// The seam axis: `0` for x, `1` for y.
    axis: usize,
    /// The intersection box's closed extent along the OTHER axis, outside
    /// which the pair imposes no ramp.
    cross: (f64, f64),
    /// Ramp denominators measured from `r`'s low and high bounding-box
    /// edges across the seam axis; `0.0` means no ramp from that edge.
    low: f64,
    high: f64,
}

/// Every ordered pair of `sources` that overlaps in positive area, with the
/// ramp it imposes — see the seam-rule doc comment on `layout::Evaluator`.
///
/// This is the one place the oracle is deliberately weaker than its
/// production counterpart: a seam between two arbitrary convex quads is
/// measured on bounding boxes, so for anything but axis-aligned rectangles
/// the answer is a reasonable definition rather than the same definition.
/// Agreement with `layout::Evaluator` is required — and tested per pixel —
/// on RECTANGLES, which is what a configured layout ever contains; the
/// skewed-quad cases below exist to show the oracle stays well defined, not
/// to pin production's behavior on them.
///
/// `sources` are the unclipped source quads (not the canvas-clipped
/// `visible` set): production computes its pairs from unclipped configured
/// source rectangles too, so canvas clipping only decides which portion of
/// the canvas is queried, never which seams attenuate.
fn seam_pairs(sources: &[Polygon]) -> Vec<SeamPair> {
    let mut pairs = Vec::new();
    for (i, r) in sources.iter().enumerate() {
        for (j, q) in sources.iter().enumerate() {
            if i == j {
                continue;
            }
            let (overlap_lo, overlap_hi) = bounds(&intersection(r, q));
            // Positive extent both ways, so quads that merely touch — or
            // miss — form no seam. An empty intersection projects to an
            // inverted box and fails this too.
            if !(overlap_hi[0] > overlap_lo[0] && overlap_hi[1] > overlap_lo[1]) {
                continue;
            }
            let axis = usize::from(overlap_hi[0] - overlap_lo[0] >= overlap_hi[1] - overlap_lo[1]);
            let other = 1 - axis;
            let extent = overlap_hi[axis] - overlap_lo[axis];
            let (r_lo, r_hi) = bounds(r);
            let (q_lo, q_hi) = bounds(q);
            let inside_low = q_lo[axis] < r_lo[axis] && r_lo[axis] < q_hi[axis];
            let inside_high = q_lo[axis] < r_hi[axis] && r_hi[axis] < q_hi[axis];
            let (low, high) = match (inside_low, inside_high) {
                // `q` spans `r`: `r` is the pair's inner slice and ramps
                // from both of its edges over half its own extent, which is
                // half of `extent` because the overlap then covers all of
                // `r` along this axis.
                (true, true) => (extent * 0.5, extent * 0.5),
                (true, false) => (extent, 0.0),
                (false, true) => (0.0, extent),
                (false, false) => continue,
            };
            pairs.push(SeamPair {
                source: i,
                axis,
                cross: (overlap_lo[other], overlap_hi[other]),
                low,
                high,
            });
        }
    }
    pairs
}

/// One edge's contribution to a ramp: `clamp(distance / denominator, 0, 1)`,
/// or `1` when no pair ramps from that edge.
fn edge_ramp(distance: f64, denominator: f64) -> f64 {
    if denominator > 0.0 {
        (distance / denominator).clamp(0.0, 1.0)
    } else {
        1.0
    }
}

impl Oracle {
    pub(super) fn new(canvas: CanvasPoint, quads: &[SourceQuad]) -> Result<Self, String> {
        if !canvas.iter().all(|v| v.is_finite() && *v > 0.0) {
            return Err("invalid_canvas: dimensions must be positive and finite".into());
        }
        if quads.is_empty() || quads.len() > MAX_PARTICIPANTS {
            return Err("participant_limit: expected 1..=8 configured participants".into());
        }
        let scale = canvas[0].max(canvas[1]);
        let canvas = [canvas[0] / scale, canvas[1] / scale];
        let clip = [[0.0, 0.0], [canvas[0], 0.0], canvas, [0.0, canvas[1]]];
        let mut sources = Vec::new();
        let mut visible = Vec::new();
        for (index, quad) in quads.iter().enumerate() {
            let quad: Polygon = quad.iter().map(|p| [p[0] / scale, p[1] / scale]).collect();
            if !quad
                .iter()
                .flatten()
                .all(|v| v.is_finite() && v.abs() <= 16.0)
            {
                return Err(format!(
                    "invalid_source[{index}]: coordinates must be finite within 16 canvas spans"
                ));
            }
            if area(&quad) <= AREA_EPS
                || (0..4).any(|i| {
                    cross(
                        sub(quad[(i + 1) % 4], quad[i]),
                        sub(quad[(i + 2) % 4], quad[(i + 1) % 4]),
                    ) <= AREA_EPS
                })
            {
                return Err(format!("invalid_source[{index}]: expected a strictly convex, clockwise quad in y-down coordinates"));
            }
            let clipped = intersection(&quad, &clip);
            if area(&clipped) <= AREA_EPS {
                return Err(format!(
                    "empty_source[{index}]: no supported positive-area canvas coverage"
                ));
            }
            sources.push(quad);
            visible.push(clipped);
        }

        let count = sources.len();
        let mut graph = vec![vec![false; count]; count];
        let mut duplicate_pairs = 0;
        for i in 0..count {
            for j in i + 1..count {
                // Use original areas: canvas clipping must not turn a seam
                // into a duplicate or change weights inside the canvas.
                if area(&intersection(&sources[i], &sources[j]))
                    >= 0.8 * area(&sources[i]).min(area(&sources[j]))
                {
                    duplicate_pairs += 1;
                }
                let connected = area(&intersection(&visible[i], &visible[j])) > AREA_EPS
                    || shared_edge(&visible[i], &visible[j]);
                graph[i][j] = connected;
                graph[j][i] = connected;
            }
        }
        if duplicate_pairs > 0 && duplicate_pairs != count * (count - 1) / 2 {
            return Err("mixed_stack_topology: near-total overlaps must form one all-pairs stack; mixing stacks with ordinary seams is unsupported".into());
        }
        let mut reached = vec![false; count];
        reached[0] = true;
        for _ in 0..count {
            for i in 0..count {
                if reached[i] {
                    for (j, linked) in graph[i].iter().enumerate() {
                        reached[j] |= linked;
                    }
                }
            }
        }
        if reached.iter().any(|v| !v) {
            return Err("disconnected_sources: require positive-area overlap or a shared edge segment within the canvas; gaps and point contacts do not connect".into());
        }
        let pairs = seam_pairs(&sources);
        Ok(Self {
            scale,
            canvas,
            sources,
            visible,
            stacked: duplicate_pairs > 0,
            pairs,
        })
    }

    /// The seam rule (see `layout::Evaluator`'s doc comment, which states it
    /// once for both production and this independent oracle): the smallest
    /// ramp any x-axis seam of `sources[index]` imposes at `p`, times the
    /// smallest any y-axis seam imposes, and exactly `1` when no seam
    /// attenuates it there. `None` outside the polygon.
    fn raw_weight(&self, index: usize, p: CanvasPoint) -> Option<f64> {
        let poly = &self.sources[index];
        inward_distance(poly, p)?;
        let (lo, hi) = bounds(poly);
        let mut ramps = [1.0_f64; 2];
        for pair in self.pairs.iter().filter(|pair| pair.source == index) {
            let across = p[1 - pair.axis];
            if across < pair.cross.0 || across > pair.cross.1 {
                continue;
            }
            let along = p[pair.axis];
            let ramp = edge_ramp(along - lo[pair.axis], pair.low)
                .min(edge_ramp(hi[pair.axis] - along, pair.high));
            ramps[pair.axis] = ramps[pair.axis].min(ramp);
        }
        Some(ramps[0] * ramps[1])
    }

    /// Closed source boundaries. At a covered point where all distances are
    /// zero, split equally among covering participants. Outside canvas: zero.
    pub(super) fn weights(&self, point: CanvasPoint) -> Result<Vec<f64>, String> {
        if !point.iter().all(|v| v.is_finite()) {
            return Err("invalid_sample: point must be finite".into());
        }
        let p = [point[0] / self.scale, point[1] / self.scale];
        let mut result = vec![0.0; self.sources.len()];
        if p[0] < 0.0 || p[1] < 0.0 || p[0] > self.canvas[0] || p[1] > self.canvas[1] {
            return Ok(result);
        }
        let raw: Vec<_> = (0..self.sources.len())
            .map(|i| self.raw_weight(i, p))
            .collect();
        let count = raw.iter().filter(|v| v.is_some()).count();
        let total: f64 = raw.iter().flatten().sum();
        for (weight, raw) in result.iter_mut().zip(raw) {
            if let Some(raw) = raw {
                *weight = if self.stacked {
                    1.0
                } else if total > 0.0 {
                    raw / total
                } else {
                    1.0 / count as f64
                };
            }
        }
        Ok(result)
    }

    /// Conservative source-weight dependency set for a stable roster/canvas.
    /// A changed global physical-coverage maximum is supplied separately; this
    /// source-weight oracle does not infer physical footprints from pin edits.
    fn affected(&self, next: &Self, coverage_max_changed: bool) -> Vec<usize> {
        assert_eq!(self.sources.len(), next.sources.len());
        assert_eq!((self.scale, self.canvas), (next.scale, next.canvas));
        if coverage_max_changed || self.stacked != next.stacked {
            return (0..self.sources.len()).collect();
        }
        let changed: Vec<_> = (0..self.sources.len())
            .filter(|&i| self.sources[i] != next.sources[i])
            .collect();
        (0..self.sources.len())
            .filter(|&i| {
                changed.contains(&i)
                    || changed.iter().any(|&j| {
                        // Include edge contact because the oracle specifies boundary values.
                        [&self.visible[i], &next.visible[i]].into_iter().any(|a| {
                            [&self.visible[j], &next.visible[j]]
                                .into_iter()
                                .any(|b| !intersection(a, b).is_empty() || shared_edge(a, b))
                        })
                    })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CanvasRect, ProjectionConfig, Rect};
    use crate::projection::blend::{canvas_plan, Participant, Slicing};
    use crate::projection::layout::{Evaluator, LayoutParticipant, LayoutSpec};

    fn rect(x: f64, y: f64, w: f64, h: f64) -> SourceQuad {
        [[x, y], [x + w, y], [x + w, y + h], [x, y + h]]
    }

    fn close(actual: &[f64], expected: &[f64]) {
        assert_eq!(actual.len(), expected.len());
        for (a, b) in actual.iter().zip(expected) {
            assert!((a - b).abs() <= 1e-6, "{actual:?} != {expected:?}");
        }
    }

    fn check_sum(oracle: &Oracle) {
        for y in 0..=80 {
            for x in 0..=80 {
                let sample = [
                    oracle.canvas[0] * oracle.scale * x as f64 / 80.0,
                    oracle.canvas[1] * oracle.scale * y as f64 / 80.0,
                ];
                // Test the represented query point. Converting a separately
                // normalized boundary point to canvas units and back can put
                // it on the opposite side of a strict closed half-plane.
                let p = [sample[0] / oracle.scale, sample[1] / oracle.scale];
                let covered = oracle
                    .sources
                    .iter()
                    .filter(|q| inward_distance(q, p).is_some())
                    .count();
                let w = oracle.weights(sample).unwrap();
                assert!(w.iter().all(|v| v.is_finite() && (0.0..=1.0).contains(v)));
                let expected = if oracle.stacked {
                    covered as f64
                } else {
                    f64::from(covered > 0)
                };
                close(&[w.iter().sum()], &[expected]);
            }
        }
    }

    #[test]
    fn ordinary_strip_and_unequal_sources_have_known_weights() {
        let oracle = Oracle::new(
            [1900.0, 1000.0],
            &[
                rect(0.0, 0.0, 1000.0, 1000.0),
                rect(900.0, 0.0, 1000.0, 1000.0),
            ],
        )
        .unwrap();
        close(&oracle.weights([925.0, 500.0]).unwrap(), &[0.75, 0.25]);
        close(&oracle.weights([900.0, 500.0]).unwrap(), &[1.0, 0.0]);
        // The pair's only seam is vertical, so its ramp is a function of x
        // alone and the same ratio holds right up to the common top edge.
        close(&oracle.weights([925.0, 10.0]).unwrap(), &[0.75, 0.25]);
        check_sum(&oracle);
        let unequal = Oracle::new(
            [1900.0, 1000.0],
            &[
                rect(0.0, 0.0, 1000.0, 1000.0),
                rect(900.0, 200.0, 1000.0, 600.0),
            ],
        )
        .unwrap();
        // The pair's intersection is 100 units wide and 600 tall, so its
        // seam axis is x and the short source's height is not a seam at all:
        // both sources ramp across the 100-unit overlap only, 25% into it.
        // (Round 2's per-edge rule saw the tall source straddling the short
        // one's top and bottom edges and attenuated it to 0.25 * 0.5 * 0.5
        // here — a gradient running along a vertical seam, which is exactly
        // what the pairwise rule exists to remove.)
        close(&unequal.weights([925.0, 500.0]).unwrap(), &[0.75, 0.25]);
        // Above the short source the pair does not cover the row at all, so
        // its ramp does not apply and the tall source keeps the whole pixel.
        close(&unequal.weights([950.0, 100.0]).unwrap(), &[1.0, 0.0]);
        check_sum(&unequal);
    }

    #[test]
    fn skewed_pair_has_shared_distances_and_eight_vertex_intersection() {
        let diamond = [[1.0, 0.0], [2.0, 1.0], [1.0, 2.0], [0.0, 1.0]];
        let square = rect(0.25, 0.25, 1.5, 1.5);
        assert_eq!(intersection(&diamond, &square).len(), 8);
        // This pair is near-total; use a shifted diamond for an ordinary seam.
        let shifted = diamond.map(|p| [p[0] + 0.75, p[1]]);
        let oracle = Oracle::new([3.0, 2.0], &[rect(0.0, 0.0, 1.5, 2.0), shifted]).unwrap();
        // The intersection is a triangle spanning x in [0.75, 1.5] and y in
        // [0.25, 1.75], so its bounding box is 0.75 wide and 1.5 tall and
        // the seam axis is x. The square's right edge is 0.25 inside that
        // 0.75-wide overlap (ramp 1/3) and the diamond's left vertex is 0.5
        // inside it (ramp 2/3); the two already sum to 1. This is the
        // bounding-box approximation the module doc calls out: on skewed
        // quads the oracle is a definition of its own, not a claim about
        // production, which only ever sees rectangles.
        close(
            &oracle.weights([1.25, 1.0]).unwrap(),
            &[1.0 / 3.0, 2.0 / 3.0],
        );
        check_sum(&oracle);
    }

    #[test]
    fn cross_and_triple_overlaps_normalize_before_gamma() {
        let cross = [
            rect(0.0, 0.0, 2.0, 2.0),
            rect(1.0, 0.0, 2.0, 2.0),
            rect(0.0, 1.0, 2.0, 2.0),
            rect(1.0, 1.0, 2.0, 2.0),
        ];
        let oracle = Oracle::new([3.0, 3.0], &cross).unwrap();
        close(&oracle.weights([1.5, 1.5]).unwrap(), &[0.25; 4]);
        // A 2x2 grid is exactly separable: with `a` the left column's share
        // in x and `b` the top row's share in y, the four raw weights are
        // `a*b`, `(1-a)*b`, `a*(1-b)`, `(1-a)*(1-b)` and already sum to 1,
        // so normalization is the identity. Here a = b = 0.75.
        let (a, b) = (0.75, 0.75);
        close(
            &oracle.weights([1.25, 1.25]).unwrap(),
            &[a * b, (1.0 - a) * b, a * (1.0 - b), (1.0 - a) * (1.0 - b)],
        );
        check_sum(&oracle);
        let triple = Oracle::new([3.0, 3.0], &cross[..3]).unwrap();
        let weights = triple.weights([1.5, 1.5]).unwrap();
        // An L of three squares, at the center of the corner where all
        // three overlap. The top-left square has a vertical seam with the
        // top-right one and a horizontal seam with the bottom-left one, so
        // it carries both ramps: 0.5 * 0.5. The top-right and bottom-left
        // squares overlap each other in a perfect square, whose seam axis
        // the rule breaks towards y, so the top-right square carries a
        // vertical ramp as well (0.5 * 0.5) and the bottom-left one carries
        // only its horizontal ramp (0.5). The three raw weights still sum
        // to exactly 1, so normalization is the identity — but the corner
        // is not shared equally, as it would be if the diagonal pair had no
        // seam at all. An L of three projectors has no symmetric answer
        // here: whichever axis that diagonal pair picks, one of the two
        // ends its image inside the other's with a hard edge.
        close(&weights, &[0.25, 0.25, 0.5]);
        let signals: Vec<_> = weights.iter().map(|w| w.powf(1.0 / 2.2)).collect();
        assert!(signals.iter().sum::<f64>() > 1.0);
        close(&[signals.iter().map(|r| r.powf(2.2)).sum()], &[1.0]);
        check_sum(&triple);
    }

    #[test]
    fn duplicates_use_original_polygon_area_and_reject_ambiguous_mixtures() {
        let a = rect(0.0, 0.0, 10.0, 10.0);
        // Exact threshold is inclusive, matching the legacy 4/5 rule.
        let stack = Oracle::new([12.0, 10.0], &[a, rect(2.0, 0.0, 10.0, 10.0)]).unwrap();
        close(&stack.weights([5.0, 5.0]).unwrap(), &[1.0, 1.0]);
        check_sum(&stack);
        let seam = Oracle::new([13.0, 10.0], &[a, rect(2.01, 0.0, 10.0, 10.0)]).unwrap();
        close(&[seam.weights([5.0, 5.0]).unwrap().iter().sum()], &[1.0]);
        let error = Oracle::new([20.0, 10.0], &[a, a, rect(9.0, 0.0, 10.0, 10.0)]).unwrap_err();
        assert!(error.contains("mixed_stack_topology"));
        // Nontransitive A~B~C duplicate chains must not become implicit groups.
        assert!(Oracle::new(
            [14.0, 10.0],
            &[a, rect(2.0, 0.0, 10.0, 10.0), rect(4.0, 0.0, 10.0, 10.0)]
        )
        .unwrap_err()
        .contains("mixed_stack_topology"));
        let diamond = [[1.0, 0.0], [2.0, 1.0], [1.0, 2.0], [0.0, 1.0]];
        assert!(
            Oracle::new([2.0, 2.0], &[diamond, diamond])
                .unwrap()
                .stacked
        );
    }

    #[test]
    fn edge_contacts_connect_point_contacts_and_gaps_do_not() {
        let a = rect(0.0, 0.0, 1.0, 1.0);
        let touching = Oracle::new([2.0, 1.0], &[a, rect(1.0, 0.0, 1.0, 1.0)]).unwrap();
        close(&touching.weights([1.0, 0.5]).unwrap(), &[0.5, 0.5]);
        close(&touching.weights([0.999, 0.5]).unwrap(), &[1.0, 0.0]);
        check_sum(&touching);
        for b in [rect(1.0, 1.0, 1.0, 1.0), rect(1.01, 0.0, 1.0, 1.0)] {
            assert!(Oracle::new([3.0, 3.0], &[a, b])
                .unwrap_err()
                .contains("disconnected_sources"));
        }
    }

    #[test]
    fn clipping_does_not_reweight_or_create_duplicates() {
        let sources = [rect(-1.0, 0.0, 2.0, 2.0), rect(0.75, 0.0, 2.0, 2.0)];
        let clipped = Oracle::new([1.0, 2.0], &sources).unwrap();
        let wide = Oracle::new([3.0, 2.0], &sources).unwrap();
        close(&clipped.weights([0.8, 1.0]).unwrap(), &[0.8, 0.2]);
        close(
            &clipped.weights([0.8, 1.0]).unwrap(),
            &wide.weights([0.8, 1.0]).unwrap(),
        );
        close(&clipped.weights([-0.1, 1.0]).unwrap(), &[0.0, 0.0]);
        check_sum(&clipped);
        // Clipping both to a small common canvas would otherwise mark a stack.
        let overlap_only = [rect(-9.0, 0.0, 10.0, 10.0), rect(0.0, 0.0, 10.0, 10.0)];
        assert!(!Oracle::new([1.0, 10.0], &overlap_only).unwrap().stacked);
    }

    #[test]
    fn invalid_geometry_is_rejected_with_identifiable_errors() {
        let good = rect(0.0, 0.0, 1.0, 1.0);
        for bad in [
            rect(0.0, 0.0, 0.0, 1.0),
            [[0.0, 0.0], [1.0, 1.0], [1.0, 0.0], [0.0, 1.0]],
            [[0.0, 0.0], [1.0, 0.0], [0.2, 0.2], [0.0, 1.0]],
            rect(f64::NAN, 0.0, 1.0, 1.0),
            rect(100.0, 0.0, 1.0, 1.0),
            rect(0.0, 0.0, 1e-13, 1.0),
        ] {
            assert!(Oracle::new([2.0, 2.0], &[bad])
                .unwrap_err()
                .contains("invalid_source[0]"));
        }
        assert!(Oracle::new([2.0, 2.0], &[rect(3.0, 3.0, 1.0, 1.0)])
            .unwrap_err()
            .contains("empty_source"));
        assert!(Oracle::new([0.0, 2.0], &[good]).is_err());
        assert!(Oracle::new([2.0, 2.0], &[]).is_err());
        assert!(Oracle::new([2.0, 2.0], &[good; 9]).is_err());
        assert!(Oracle::new([2.0, 2.0], &[good])
            .unwrap()
            .weights([f64::INFINITY, 0.0])
            .is_err());
    }

    #[test]
    fn permutation_and_uniform_scale_preserve_weights() {
        let sources = [
            rect(0.0, 0.0, 2.0, 2.0),
            rect(1.0, 0.0, 2.0, 2.0),
            rect(0.0, 1.0, 2.0, 2.0),
        ];
        let oracle = Oracle::new([3.0, 3.0], &sources).unwrap();
        let expected = oracle.weights([1.25, 1.25]).unwrap();
        let reversed = Oracle::new([3.0, 3.0], &[sources[2], sources[1], sources[0]]).unwrap();
        close(
            &reversed.weights([1.25, 1.25]).unwrap(),
            &expected.iter().copied().rev().collect::<Vec<_>>(),
        );
        for scale in [0.001, 1000.0] {
            let scaled = sources.map(|q| q.map(|p| [p[0] * scale, p[1] * scale]));
            close(
                &Oracle::new([3.0 * scale, 3.0 * scale], &scaled)
                    .unwrap()
                    .weights([1.25 * scale, 1.25 * scale])
                    .unwrap(),
                &expected,
            );
        }
    }

    #[test]
    fn affected_set_matches_full_rebuild_and_global_coverage_invalidates_all() {
        let old_sources = [
            rect(0.0, 0.0, 2.0, 2.0),
            rect(1.5, 0.0, 2.0, 2.0),
            rect(3.0, 0.0, 2.0, 2.0),
        ];
        let mut new_sources = old_sources;
        new_sources[0] = rect(0.0, 0.0, 2.1, 2.0);
        let old = Oracle::new([5.0, 2.0], &old_sources).unwrap();
        let new = Oracle::new([5.0, 2.0], &new_sources).unwrap();
        assert_eq!(old.affected(&old, false), Vec::<usize>::new());
        assert_eq!(old.affected(&new, false), vec![0, 1]);
        assert_eq!(old.affected(&new, true), vec![0, 1, 2]);
        for y in 0..20 {
            for x in 0..50 {
                let p = [(x as f64 + 0.5) / 10.0, (y as f64 + 0.5) / 10.0];
                let mut incremental = old.weights(p).unwrap();
                let full = new.weights(p).unwrap();
                for i in old.affected(&new, false) {
                    incremental[i] = full[i];
                }
                assert_eq!(incremental, full);
            }
        }
        // No connected/display state or destination pin is an oracle input.
        // Keeping a configured but absent middle projector preserves its weights.
        close(&old.weights([1.75, 1.0]).unwrap(), &[0.5, 0.5, 0.0]);
    }

    #[test]
    fn fixed_point_gain_error_is_bounded_across_every_weight_and_gamma() {
        // Two overlapping sources — A = [0, 2] x [0, 2], B = [-1, 1] x [0, 2]
        // — overlap in a band 1 wide and 2 tall, so the seam axis is x and
        // the overlap depth is exactly 1. A ramps from its left edge (the
        // only one of its two that lies inside B) as exactly `x`, and B from
        // its right edge as exactly `1 - x`, so `layout::Evaluator`'s own
        // weight for A at canvas-unit x = w is exactly w. That passes w
        // through the actual production evaluator — not a synthetic ramp —
        // for the fixed-point rounding/gamma contract this checks. (The
        // sources are twice as tall as the canvas so that the overlap is
        // unambiguously a VERTICAL seam: a square overlap is the rule's tie
        // case, which it breaks towards a horizontal seam.) (The lift half of the
        // fixed-point contract is exhaustively covered by
        // `layout`'s own `footprint_maximum_is_canvas_clipped_and_does_not_follow_pins`.)
        let spec = LayoutSpec {
            aspect: 1.0,
            blend: true,
            participants: vec![
                LayoutParticipant {
                    output: "A".into(),
                    source: CanvasRect {
                        x: 0.0,
                        y: 0.0,
                        width: 2.0,
                        height: 2.0,
                    },
                    raster_footprint: CanvasRect {
                        x: 0.0,
                        y: 0.0,
                        width: 2.0,
                        height: 2.0,
                    },
                },
                LayoutParticipant {
                    output: "B".into(),
                    source: CanvasRect {
                        x: -1.0,
                        y: 0.0,
                        width: 2.0,
                        height: 2.0,
                    },
                    raster_footprint: CanvasRect {
                        x: -1.0,
                        y: 0.0,
                        width: 2.0,
                        height: 2.0,
                    },
                },
            ],
        };
        let evaluator = Evaluator::new(&spec, 1000, 1000).unwrap();
        for index in 0..=1000 {
            let w = index as f64 / 1000.0;
            for gamma in [1.0, 2.2, 3.0] {
                for edge in [0.0, 0.5, 1.0] {
                    let expected = edge * w.powf(1.0 / gamma);
                    let (gain, offset) = evaluator.transfer(0, w * 1000.0, 500.0, gamma, 0.0, edge);
                    assert_eq!(offset, 0, "no black lift is configured");
                    assert!(
                        (f64::from(gain) / 256.0 - expected).abs() <= 1.0 / 512.0 + 1e-9,
                        "w={w} gamma={gamma} edge={edge}: gain {gain}"
                    );
                    for input in [0_u16, 1, 127, 128, 254, 255] {
                        let actual = ((gain * input) >> 8).min(255);
                        let ideal = expected * f64::from(input);
                        assert!((f64::from(actual) - ideal).abs() <= 2.0);
                    }
                }
            }
        }
    }

    #[test]
    fn production_and_oracle_agree_at_a_common_top_border() {
        // Once the deleted legacy pairwise `RampSpec` rule always looked at
        // every edge, so an ordinary two-projector overlap's ratio would
        // shift near the top/bottom border where a third, uninvolved edge
        // happened to be closer — the discrepancy this oracle was built to
        // expose (finding A5). Production now uses `layout::Evaluator`'s
        // active-edge rule everywhere, including for this legacy integer
        // layout (synthesized into a `LayoutSpec`), which is the same rule
        // this oracle implements: they must agree here too, not just away
        // from every border.
        let participants: Vec<_> = [0, 900]
            .into_iter()
            .enumerate()
            .map(|(i, x)| Participant {
                name: format!("output-{i}"),
                rect: Rect {
                    x,
                    y: 0,
                    width: 1000,
                    height: 1000,
                },
                connected: true,
            })
            .collect();
        let config = ProjectionConfig {
            blend: true,
            gamma: 1.0,
            ..ProjectionConfig::default()
        };
        let plan = canvas_plan(&participants, Some(&config), Slicing::Always).unwrap();
        let evaluator = super::super::layout::Evaluator::new(
            plan.layout.as_ref().unwrap(),
            plan.canvas_width,
            plan.canvas_height,
        )
        .unwrap();
        let oracle = Oracle::new(
            [1900.0, 1000.0],
            &[
                rect(0.0, 0.0, 1000.0, 1000.0),
                rect(900.0, 0.0, 1000.0, 1000.0),
            ],
        )
        .unwrap();
        let weights = oracle.weights([925.0, 10.0]).unwrap();
        close(&weights, &[0.75, 0.25]);
        let production = evaluator.transfer(0, 925.0, 10.0, 1.0, 0.0, 1.0);
        assert_eq!(production, (192, 0));
        assert_eq!((weights[0] * 256.0).round() as u16, 192);
    }

    #[test]
    fn boundaries_do_not_expand_source_or_canvas_coverage() {
        let oracle = Oracle::new([1.0, 1.0], &[rect(0.25, 0.25, 0.5, 0.5)]).unwrap();
        for p in [[0.25, 0.25], [0.75, 0.75], [0.25, 0.5]] {
            close(&oracle.weights(p).unwrap(), &[1.0]);
        }
        close(
            &oracle.weights([0.25 - LENGTH_EPS / 2.0, 0.5]).unwrap(),
            &[0.0],
        );
        close(
            &oracle.weights([0.25 + LENGTH_EPS / 2.0, 0.5]).unwrap(),
            &[1.0],
        );
        let clipped = Oracle::new(
            [2.0, 1.0],
            &[rect(-1.0, -1.0, 2.0, 3.0), rect(1.0, -1.0, 2.0, 3.0)],
        )
        .unwrap();
        close(&clipped.weights([1.0, 0.0]).unwrap(), &[0.5, 0.5]);
        close(&clipped.weights([1.0, 1.0]).unwrap(), &[0.5, 0.5]);
        close(
            &clipped.weights([-LENGTH_EPS / 2.0, 0.5]).unwrap(),
            &[0.0, 0.0],
        );
    }

    #[test]
    fn all_pairs_stacks_and_unequal_clipped_threshold_are_explicit() {
        let quad = [[0.0, 0.0], [2.0, 0.25], [1.75, 2.0], [0.25, 1.75]];
        let stack = Oracle::new([2.0, 2.0], &[quad; 4]).unwrap();
        close(&stack.weights([1.0, 1.0]).unwrap(), &[1.0; 4]);
        check_sum(&stack);
        let a = rect(-8.0, 0.0, 10.0, 10.0);
        for (x, expected_stack) in [(0.399999, true), (0.400001, false)] {
            // Smaller area is 20, intersection is (2-x)*10. Both sources
            // are clipped, but the duplicate threshold uses original areas.
            let oracle = Oracle::new([1.0, 10.0], &[a, rect(x, 0.0, 2.0, 10.0)]).unwrap();
            assert_eq!(oracle.stacked, expected_stack);
        }
    }

    #[test]
    fn thin_skewed_sources_keep_boundary_dependencies() {
        let a = [[0.0, 0.0], [2000.0, 0.001], [2000.0, 0.011], [0.0, 0.01]];
        let b = rect(1900.0, 0.0, 2100.0, 0.02);
        let old = Oracle::new([4000.0, 0.02], &[a, b]).unwrap();
        let mut edited = a;
        edited[1][0] = 2100.0;
        let next = Oracle::new([4000.0, 0.02], &[edited, b]).unwrap();
        let affected = old.affected(&next, false);
        assert_eq!(affected, vec![0, 1]);
        for p in a
            .into_iter()
            .chain(edited)
            .chain([[2000.0, 0.005], [0.0, 0.0], [2100.0, 0.001]])
        {
            let mut incremental = old.weights(p).unwrap();
            let full = next.weights(p).unwrap();
            for &i in &affected {
                incremental[i] = full[i];
            }
            assert_eq!(incremental, full);
        }
        let diamond = [[1.0, 0.0], [2.0, 1.0], [1.0, 2.0], [0.0, 1.0]];
        let square = rect(0.25, 0.25, 1.5, 1.5);
        let clipped = Oracle::new([2.0, 2.0], &[diamond, square]).unwrap();
        // Eight-vertex near-total intersection: the stack exception supplies
        // full intensity at every shared vertex, including boundaries.
        assert!(clipped.stacked);
        for p in intersection(&diamond, &square) {
            close(&clipped.weights(p).unwrap(), &[1.0, 1.0]);
        }
    }

    #[test]
    fn arbitrary_triple_matches_the_oracle_not_the_deleted_pairwise_sum() {
        // The deleted legacy pairwise `RampSpec` rule summed each pair's
        // ramp independently, so a three-way overlap's coefficients summed
        // to 229/256 (about 0.895), not 1 (`seam_oracle.rs`'s own former
        // documentation of that gap). Production now uses the same
        // minimum-active-distance rule this oracle does, through
        // `layout::Evaluator`, so both agree — and both actually sum to 1.
        let participants: Vec<_> = [0, 5, 8]
            .into_iter()
            .enumerate()
            .map(|(i, x)| Participant {
                name: format!("output-{i}"),
                rect: Rect {
                    x,
                    y: 0,
                    width: 10,
                    height: 10,
                },
                connected: true,
            })
            .collect();
        let config = ProjectionConfig {
            blend: true,
            gamma: 1.0,
            ..ProjectionConfig::default()
        };
        let plan = canvas_plan(&participants, Some(&config), Slicing::Always).unwrap();
        let evaluator = super::super::layout::Evaluator::new(
            plan.layout.as_ref().unwrap(),
            plan.canvas_width,
            plan.canvas_height,
        )
        .unwrap();
        let production: Vec<u16> = (0..3)
            .map(|i| evaluator.transfer(i, 8.5, 5.5, 1.0, 0.0, 1.0).0)
            .collect();
        let oracle = Oracle::new(
            [18.0, 10.0],
            &[
                rect(0.0, 0.0, 10.0, 10.0),
                rect(5.0, 0.0, 10.0, 10.0),
                rect(8.0, 0.0, 10.0, 10.0),
            ],
        )
        .unwrap();
        let expected = oracle.weights([8.5, 5.5]).unwrap();
        // Raw weights at x = 8.5. Every pair here is a vertical seam, so
        // each source takes the SMALLEST ramp any of its seams asks for.
        // The first source ramps from its right edge over its widest
        // overlap, the 5-unit one with the middle source: 1.5/5 = 0.3. The
        // middle source ramps 3.5 into 5 from its left and 6.5 into 7 from
        // its right, and the smaller of those is 0.7. The last ramps 0.5
        // into 7 from its left: 1/14. Those sum to 1.0714, and normalizing
        // gives the triple below.
        let raw = [0.3, 0.7, 0.5 / 7.0];
        let total: f64 = raw.iter().sum();
        close(&expected, &raw.map(|w| w / total));
        let sum: u32 = production.iter().map(|&g| u32::from(g)).sum();
        assert!((sum as i64 - 256).abs() <= 2, "sum was {sum}, not ~256");
        for i in 0..3 {
            assert!(
                (f64::from(production[i]) / 256.0 - expected[i]).abs() <= 1.0 / 256.0 + 1e-9,
                "index {i}: production {} oracle {}",
                production[i],
                expected[i]
            );
        }
    }

    // --- restored per-pixel production-vs-oracle comparison (finding A5) --
    //
    // Commit 7fccd1d replaced the original per-pixel comparison
    // (`production_rectangles_match_independent_d3_oracle`) with a
    // sum-to-one test that is true by construction — `weight = own / total`
    // sums to 1 regardless of whether `own` is computed correctly, so it
    // could not have caught a seam-rule bug. This restores the genuine
    // comparison, now that both production (`layout::Evaluator`) and this
    // oracle implement the identical active-edge rule stated once on
    // `layout::Evaluator`'s doc comment.

    /// Build the same layout as both a production `LayoutSpec` (pixel rects
    /// normalized by the canvas width, per `layout::canvas_plan_with_correction`'s
    /// convention) and an oracle `Oracle`, then check every covered canvas
    /// pixel's weight agrees within old test's tolerance: `1/512` gain
    /// quantization plus a small epsilon.
    fn compare_production_and_oracle(rects: &[(f64, f64, f64, f64)], width: i32, height: i32) {
        let columns: Vec<i32> = (0..width).collect();
        let rows: Vec<i32> = (0..height).collect();
        compare_production_and_oracle_at(rects, width, height, &columns, &rows);
    }

    /// As [`compare_production_and_oracle`], but only at the listed canvas
    /// columns and rows. A wall-sized canvas has too many pixels to sweep
    /// exhaustively in a debug build, so its test names the bands that
    /// matter and samples the rest.
    fn compare_production_and_oracle_at(
        rects: &[(f64, f64, f64, f64)],
        width: i32,
        height: i32,
        columns: &[i32],
        rows: &[i32],
    ) {
        let aspect = f64::from(width) / f64::from(height);
        let participants: Vec<_> = rects
            .iter()
            .enumerate()
            .map(|(i, &(x, y, w, h))| {
                let source = CanvasRect {
                    x: x / f64::from(width),
                    y: y / f64::from(width),
                    width: w / f64::from(width),
                    height: h / f64::from(width),
                };
                LayoutParticipant {
                    output: i.to_string(),
                    source,
                    raster_footprint: source,
                }
            })
            .collect();
        let spec = LayoutSpec {
            aspect,
            blend: true,
            participants,
        };
        let evaluator = Evaluator::new(&spec, width, height).unwrap();
        let quads: Vec<SourceQuad> = rects.iter().map(|&(x, y, w, h)| rect(x, y, w, h)).collect();
        let oracle = Oracle::new([f64::from(width), f64::from(height)], &quads).unwrap();
        for &gy in rows {
            for &gx in columns {
                let (cx, cy) = (f64::from(gx) + 0.5, f64::from(gy) + 0.5);
                let expected = oracle.weights([cx, cy]).unwrap();
                for (i, expected) in expected.iter().enumerate() {
                    let (gain, lift) = evaluator.transfer(i, cx, cy, 1.0, 0.0, 1.0);
                    assert_eq!(lift, 0, "no black lift is configured");
                    let actual = f64::from(gain) / 256.0;
                    assert!(
                        (actual - expected).abs() <= 1.0 / 512.0 + 1e-9,
                        "index {i} at ({cx}, {cy}): production {actual} oracle {expected}"
                    );
                }
            }
        }
    }

    #[test]
    fn production_matches_oracle_for_a_two_strip() {
        compare_production_and_oracle(
            &[(0.0, 0.0, 60.0, 100.0), (40.0, 0.0, 60.0, 100.0)],
            100,
            100,
        );
    }

    #[test]
    fn production_matches_oracle_for_a_2x2_grid_with_20_percent_overlaps() {
        let rects = [
            (0.0, 0.0, 60.0, 60.0),
            (40.0, 0.0, 60.0, 60.0),
            (0.0, 40.0, 60.0, 60.0),
            (40.0, 40.0, 60.0, 60.0),
        ];
        compare_production_and_oracle(&rects, 100, 100);

        // The four-way corner explicitly, not just swept over by the loop
        // above: every source carries an equal quarter there.
        let participants: Vec<_> = rects
            .iter()
            .enumerate()
            .map(|(i, &(x, y, w, h))| {
                let source = CanvasRect {
                    x: x / 100.0,
                    y: y / 100.0,
                    width: w / 100.0,
                    height: h / 100.0,
                };
                LayoutParticipant {
                    output: i.to_string(),
                    source,
                    raster_footprint: source,
                }
            })
            .collect();
        let spec = LayoutSpec {
            aspect: 1.0,
            blend: true,
            participants,
        };
        let evaluator = Evaluator::new(&spec, 100, 100).unwrap();
        let quads: Vec<SourceQuad> = rects.iter().map(|&(x, y, w, h)| rect(x, y, w, h)).collect();
        let oracle = Oracle::new([100.0, 100.0], &quads).unwrap();
        let corner = [50.0, 50.0];
        close(&oracle.weights(corner).unwrap(), &[0.25; 4]);
        for i in 0..4 {
            let (gain, _) = evaluator.transfer(i, corner[0], corner[1], 1.0, 0.0, 1.0);
            assert!(
                (f64::from(gain) / 256.0 - 0.25).abs() <= 1.0 / 512.0 + 1e-9,
                "index {i} at the corner: gain {gain}"
            );
        }

        // The product property itself, stated independently of either
        // implementation: at every pixel each source's weight is the product
        // of the same two 1-D ramps, one per axis, and the four already sum
        // to 1 without normalization.
        for gy in 0..100 {
            for gx in 0..100 {
                let (cx, cy) = (f64::from(gx) + 0.5, f64::from(gy) + 0.5);
                let a = ((60.0 - cx) / 20.0).clamp(0.0, 1.0);
                let b = ((60.0 - cy) / 20.0).clamp(0.0, 1.0);
                close(
                    &oracle.weights([cx, cy]).unwrap(),
                    &[a * b, (1.0 - a) * b, a * (1.0 - b), (1.0 - a) * (1.0 - b)],
                );
            }
        }
    }

    /// The configuration of a real four-projector wall (brain, September
    /// 2026): a 3864x2300 canvas at aspect 1.68, columns overlapping by 225
    /// pixels and rows that touch with a 0.07-pixel sliver. That sliver is
    /// what made the old minimum-distance rule paint a bright triangle at
    /// the center of the wall, so it is the case both implementations most
    /// need to agree on. Rows around the row boundary and columns across the
    /// horizontal seam are swept densely; the rest of the canvas — far too
    /// many pixels to sweep exhaustively in a debug build — is sampled at a
    /// coprime stride so the samples do not line up with any edge.
    #[test]
    fn production_matches_oracle_for_brains_four_projector_wall() {
        const SCALE: f64 = 3864.0;
        let (w, h) = (0.5291176764326357, 0.2976286929933576);
        let (column_x, row_y) = (0.47088232356736426, 0.29760940224473764);
        let rects: Vec<(f64, f64, f64, f64)> =
            [(0.0, 0.0), (column_x, 0.0), (0.0, row_y), (column_x, row_y)]
                .into_iter()
                .map(|(x, y)| (x * SCALE, y * SCALE, w * SCALE, h * SCALE))
                .collect();
        let mut rows: Vec<i32> = (1130..1180).collect();
        rows.extend((0..2300).step_by(53));
        let mut columns: Vec<i32> = (1810..1830).collect();
        columns.extend((0..3864).step_by(61));
        compare_production_and_oracle_at(&rects, 3864, 2300, &columns, &rows);
    }

    /// The same wall one percent out of true, which is what round 3 is
    /// about: the right column sits 1% of the canvas width lower than the
    /// left, and the bottom row 1% further right than the top. Every pair
    /// then straddles its neighbor in BOTH axes, so it is the layout that
    /// most exercises the choice of seam axis and the cross-range, and the
    /// one where the two implementations are most likely to disagree.
    /// Sampled like brain's wall above: dense bands across every seam and
    /// every crop edge, a coprime stride elsewhere.
    #[test]
    fn production_matches_oracle_for_a_misaligned_2x2_grid() {
        const SCALE: f64 = 3864.0;
        let (w, h) = (0.53, 0.30);
        let rects: Vec<(f64, f64, f64, f64)> =
            [(0.0, 0.0), (0.47, 0.01), (0.01, 0.26), (0.47, 0.26)]
                .into_iter()
                .map(|(x, y)| (x * SCALE, y * SCALE, w * SCALE, h * SCALE))
                .collect();
        // Column overlap [0.47, 0.54], row overlap [0.26, 0.31], plus every
        // slice's own top/bottom and left/right crop edge.
        let mut columns: Vec<i32> = Vec::new();
        for edge in [0.0, 0.01, 0.47, 0.53, 0.54, 1.0] {
            let center = (edge * SCALE) as i32;
            columns.extend((center - 6).max(0)..(center + 6).min(3864));
        }
        columns.extend((0..3864).step_by(61));
        let mut rows: Vec<i32> = Vec::new();
        for edge in [0.0, 0.01, 0.26, 0.30, 0.31, 0.56] {
            let center = (edge * SCALE) as i32;
            rows.extend((center - 6).max(0)..(center + 6).min(2300));
        }
        rows.extend((0..2300).step_by(53));
        compare_production_and_oracle_at(&rects, 3864, 2300, &columns, &rows);
    }

    #[test]
    fn production_matches_oracle_for_unequal_heights_with_partial_overlap() {
        compare_production_and_oracle(
            &[(0.0, 0.0, 60.0, 100.0), (40.0, 20.0, 60.0, 60.0)],
            100,
            100,
        );
    }

    #[test]
    fn production_matches_oracle_for_a_three_way_overlap() {
        compare_production_and_oracle(
            &[
                (0.0, 0.0, 10.0, 10.0),
                (5.0, 0.0, 10.0, 10.0),
                (8.0, 0.0, 10.0, 10.0),
            ],
            18,
            10,
        );
    }

    #[test]
    fn production_matches_oracle_for_a_nested_layout() {
        // B sits entirely inside A: near-total overlap (`>= 80%` of the
        // smaller original rectangle), so both `Evaluator` and `Oracle`
        // independently classify this an all-pairs stack — full weight for
        // both, not a distance ratio — and must still agree pixel for pixel.
        compare_production_and_oracle(
            &[(0.0, 0.0, 100.0, 100.0), (20.0, 20.0, 30.0, 30.0)],
            100,
            100,
        );
    }

    #[test]
    fn production_matches_oracle_for_a_cross_shaped_overlap() {
        let rects = [
            (0.0, 0.0, 60.0, 60.0),
            (30.0, 0.0, 60.0, 60.0),
            (0.0, 30.0, 60.0, 60.0),
            (30.0, 30.0, 60.0, 60.0),
        ];
        compare_production_and_oracle(&rects, 90, 90);
    }
}
