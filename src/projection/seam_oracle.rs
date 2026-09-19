//! D3 reference weights, deliberately test-only until geometry-model integration.
//!
//! These are shared *source coverage* polygons, never destination pins. Public
//! pinning still selects a fixed source rectangle. See research/warp/D3.md for
//! the supported topology, boundary convention, and comparison with current ramps.

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
        Ok(Self {
            scale,
            canvas,
            sources,
            visible,
            stacked: duplicate_pairs > 0,
        })
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
        let distances: Vec<_> = self.sources.iter().map(|q| inward_distance(q, p)).collect();
        let count = distances.iter().filter(|v| v.is_some()).count();
        let total: f64 = distances.iter().flatten().sum();
        for (weight, distance) in result.iter_mut().zip(distances) {
            if let Some(distance) = distance {
                *weight = if self.stacked {
                    1.0
                } else if total > 0.0 {
                    distance / total
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
    use crate::model::{ProjectionConfig, Rect};
    use crate::projection::blend::{
        canvas_plan, transfer_at, FadeTo, Participant, RampSpec, Slicing,
    };

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
        // All-edge distances differ from axis ramps near a common top edge.
        close(&oracle.weights([925.0, 10.0]).unwrap(), &[0.5, 0.5]);
        check_sum(&oracle);
        let unequal = Oracle::new(
            [1900.0, 1000.0],
            &[
                rect(0.0, 0.0, 1000.0, 1000.0),
                rect(900.0, 200.0, 1000.0, 600.0),
            ],
        )
        .unwrap();
        close(&unequal.weights([925.0, 500.0]).unwrap(), &[0.75, 0.25]);
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
        let d = 0.5_f64 / 2.0_f64.sqrt();
        close(
            &oracle.weights([1.25, 1.0]).unwrap(),
            &[0.25 / (0.25 + d), d / (0.25 + d)],
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
        close(
            &oracle.weights([1.25, 1.25]).unwrap(),
            &[0.5, 1.0 / 6.0, 1.0 / 6.0, 1.0 / 6.0],
        );
        check_sum(&oracle);
        let triple = Oracle::new([3.0, 3.0], &cross[..3]).unwrap();
        let weights = triple.weights([1.5, 1.5]).unwrap();
        close(&weights, &[1.0 / 3.0; 3]);
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
    fn fixed_transfer_coefficient_error_is_bounded_separately() {
        // A unit left-facing ramp passes w through the production transfer
        // evaluator, so this checks the actual rounding/gamma/border contract.
        let ramp = RampSpec {
            rect: Rect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            fade_to: FadeTo::Left,
        };
        for index in 0..=1000 {
            let w = index as f64 / 1000.0;
            for gamma in [1.0, 2.2, 3.0] {
                for lift in [0.0, 0.17, 0.5, 1.0] {
                    for edge in [0.0, 0.5, 1.0] {
                        let r = w.powf(1.0 / gamma);
                        let a = edge * (1.0 - lift) * r;
                        let b = edge * lift;
                        let (gain, offset) =
                            transfer_at(std::slice::from_ref(&ramp), gamma, lift, w, 0.5, edge);
                        assert!((f64::from(gain) / 256.0 - a).abs() <= 1.0 / 512.0 + 1e-15);
                        assert!((f64::from(offset) / 255.0 - b).abs() <= 1.0 / 510.0 + 1e-15);
                        for input in [0_u16, 1, 127, 128, 254, 255] {
                            let actual = (((gain * input) >> 8) + u16::from(offset)).min(255);
                            let ideal = (a * f64::from(input) + 255.0 * b).min(255.0);
                            assert!((f64::from(actual) - ideal).abs() <= 2.0);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn distance_rule_difference_from_current_rectangular_evaluator_is_explicit() {
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
        let oracle = Oracle::new(
            [1900.0, 1000.0],
            &[
                rect(0.0, 0.0, 1000.0, 1000.0),
                rect(900.0, 0.0, 1000.0, 1000.0),
            ],
        )
        .unwrap();
        // At a common top border, minimum-distance weights differ materially.
        let weights = oracle.weights([925.0, 10.0]).unwrap();
        close(&weights, &[0.5, 0.5]);
        let legacy = transfer_at(&plan.slices[0].ramps, 1.0, 0.0, 925.0, 10.0, 1.0);
        assert_eq!(legacy, (192, 0));
        assert_eq!((weights[0] * 256.0).round() as u16, 128);
        // Record current arithmetic with nonzero lift; this is a comparison, not
        // an old-configuration migration requirement.
        assert_eq!(
            transfer_at(&plan.slices[0].ramps, 1.0, 0.2, 925.0, 10.0, 1.0),
            (154, 51)
        );
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
    fn arbitrary_triple_improves_the_current_pairwise_sum() {
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
        let coefficients: Vec<_> = plan
            .slices
            .iter()
            .map(|s| transfer_at(&s.ramps, 1.0, 0.0, 8.5 - f64::from(s.source.x), 5.5, 1.0).0)
            .collect();
        assert_eq!(coefficients, vec![58, 166, 5]);
        let oracle = Oracle::new(
            [18.0, 10.0],
            &[
                rect(0.0, 0.0, 10.0, 10.0),
                rect(5.0, 0.0, 10.0, 10.0),
                rect(8.0, 0.0, 10.0, 10.0),
            ],
        )
        .unwrap();
        close(
            &oracle.weights([8.5, 5.5]).unwrap(),
            &[3.0 / 11.0, 7.0 / 11.0, 1.0 / 11.0],
        );
    }
}
