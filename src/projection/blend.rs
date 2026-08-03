//! Pure edge-blend math: seams from layout, gamma-shaped ramps from seams.
//!
//! Everything here is deterministic geometry and arithmetic, deliberately free
//! of Wayland, processes, and IO, for the same reason the output planner is
//! pure: the correctness rules — ramps exactly spanning the shared region,
//! luminance summing to one across a seam — are testable on any machine.
//! The future warp client (corner pinning) reuses this module unchanged; a
//! homography changes where a ramp is *drawn*, not what its values are.

use serde::{Deserialize, Serialize};

use crate::model::Rect;

/// An output taking part in seam derivation: its place in the global layout.
#[derive(Debug, Clone, PartialEq)]
pub struct Participant {
    pub name: String,
    pub rect: Rect,
}

/// The side of a ramp at which attenuation reaches zero — the side where the
/// neighbouring projector has fully taken over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FadeTo {
    Left,
    Right,
    Top,
    Bottom,
}

/// One gradient this output must draw, in output-local coordinates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RampSpec {
    pub rect: Rect,
    pub fade_to: FadeTo,
}

/// Everything one overlay process needs. Serialized to the `suede blend`
/// subcommand verbatim — this struct *is* the daemon↔overlay contract.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverlaySpec {
    pub output: String,
    /// Shapes the ramps' fall-off; see [`crate::model::ProjectionConfig`].
    pub gamma: f64,
    /// Signal lift applied *outside* the seams, compensating for the doubled
    /// projector black inside them. `0.0` is off.
    #[serde(default)]
    pub black_lift: f64,
    pub ramps: Vec<RampSpec>,
}

fn intersect(a: &Rect, b: &Rect) -> Option<Rect> {
    let x = a.x.max(b.x);
    let y = a.y.max(b.y);
    let right = (a.x + a.width).min(b.x + b.width);
    let bottom = (a.y + a.height).min(b.y + b.height);
    (right > x && bottom > y).then_some(Rect {
        x,
        y,
        width: right - x,
        height: bottom - y,
    })
}

fn area(rect: &Rect) -> i64 {
    rect.width as i64 * rect.height as i64
}

/// Derive one overlay spec per output that has any seam.
///
/// A seam is a pairwise intersection of two participants' rectangles, with
/// two structural exclusions:
///
/// - **Mirrors.** An intersection covering most of either output is a
///   mirrored display (the sway same-position trick — a confidence monitor
///   showing a projector's picture), not a projection seam; fading it would
///   black out both screens.
/// - **Diagonals.** In a 2×2 grid the corner region already receives the
///   product of each output's horizontal and vertical ramps, which sums to
///   constant luminance by construction — a third ramp from the diagonal
///   pair would darken it twice.
pub fn overlay_specs(
    participants: &[Participant],
    gamma: f64,
    black_lift: f64,
) -> Vec<OverlaySpec> {
    let mut ramps: Vec<(usize, RampSpec)> = Vec::new();

    for i in 0..participants.len() {
        for j in (i + 1)..participants.len() {
            let (a, b) = (&participants[i], &participants[j]);
            let Some(seam) = intersect(&a.rect, &b.rect) else {
                continue;
            };

            // A mirror, not a seam.
            if area(&seam) * 5 >= area(&a.rect).min(area(&b.rect)) * 4 {
                continue;
            }

            // Edge adjacency: the seam spans (most of) the shorter output
            // along the axis perpendicular to the fade.
            let horizontal = seam.height * 2 >= a.rect.height.min(b.rect.height);
            let vertical = seam.width * 2 >= a.rect.width.min(b.rect.width);
            let center_dx = (a.rect.x * 2 + a.rect.width) - (b.rect.x * 2 + b.rect.width);
            let center_dy = (a.rect.y * 2 + a.rect.height) - (b.rect.y * 2 + b.rect.height);

            let axis = match (horizontal, vertical) {
                (true, false) => Axis::X,
                (false, true) => Axis::Y,
                // Both qualify (heavily overlapped): the larger centre offset
                // decides. Neither: a diagonal neighbour — no ramp.
                (true, true) => {
                    if center_dx.abs() >= center_dy.abs() {
                        Axis::X
                    } else {
                        Axis::Y
                    }
                }
                (false, false) => continue,
            };

            let (fade_a, fade_b) = match axis {
                Axis::X if center_dx <= 0 => (FadeTo::Right, FadeTo::Left),
                Axis::X => (FadeTo::Left, FadeTo::Right),
                Axis::Y if center_dy <= 0 => (FadeTo::Bottom, FadeTo::Top),
                Axis::Y => (FadeTo::Top, FadeTo::Bottom),
            };

            for (index, fade_to) in [(i, fade_a), (j, fade_b)] {
                let origin = &participants[index].rect;
                ramps.push((
                    index,
                    RampSpec {
                        rect: Rect {
                            x: seam.x - origin.x,
                            y: seam.y - origin.y,
                            width: seam.width,
                            height: seam.height,
                        },
                        fade_to,
                    },
                ));
            }
        }
    }

    let mut specs: Vec<OverlaySpec> = Vec::new();
    for (index, participant) in participants.iter().enumerate() {
        let mine: Vec<RampSpec> = ramps
            .iter()
            .filter(|(owner, _)| *owner == index)
            .map(|(_, ramp)| ramp.clone())
            .collect();
        if !mine.is_empty() {
            specs.push(OverlaySpec {
                output: participant.name.clone(),
                gamma,
                black_lift,
                ramps: mine,
            });
        }
    }
    specs.sort_by(|a, b| a.output.cmp(&b.output));
    specs
}

enum Axis {
    X,
    Y,
}

/// Combined light attenuation of every ramp covering the pixel centre
/// `(x, y)`, or `None` when no ramp covers it at all.
///
/// The distinction matters: a covered pixel with attenuation 1.0 is *inside*
/// a seam (no lift there — the doubled projector black is the lift), while an
/// uncovered pixel is outside every seam and receives the black-lift.
fn attenuation_at(spec: &OverlaySpec, x: f64, y: f64) -> Option<f64> {
    let mut covered = false;
    let mut transmitted = 1.0;
    for ramp in &spec.ramps {
        let inside = x >= ramp.rect.x as f64
            && x < (ramp.rect.x + ramp.rect.width) as f64
            && y >= ramp.rect.y as f64
            && y < (ramp.rect.y + ramp.rect.height) as f64;
        if !inside {
            continue;
        }
        covered = true;
        let along = match ramp.fade_to {
            FadeTo::Right => 1.0 - (x - ramp.rect.x as f64) / ramp.rect.width as f64,
            FadeTo::Left => (x - ramp.rect.x as f64) / ramp.rect.width as f64,
            FadeTo::Bottom => 1.0 - (y - ramp.rect.y as f64) / ramp.rect.height as f64,
            FadeTo::Top => (y - ramp.rect.y as f64) / ramp.rect.height as f64,
        };
        // Where ramps overlap (grid corners), attenuations multiply in
        // light, which is what makes a 2×2 corner sum to constant luminance.
        transmitted *= along.clamp(0.0, 1.0);
    }
    covered.then_some(transmitted)
}

/// The ramp alpha for a covered pixel.
///
/// The overlay is black; compositing `content·(1−α)` happens in signal space,
/// and the display then raises the signal to `gamma`. To attenuate *light* by
/// `t`, the alpha must therefore be `1 − t^(1/gamma)` — the mathematically
/// required shaping that a naive signal-space gradient gets wrong.
fn ramp_alpha(transmitted: f64, gamma: f64) -> u8 {
    let alpha = 1.0 - transmitted.powf(1.0 / gamma);
    (alpha * 255.0).round().clamp(0.0, 255.0) as u8
}

/// Ramp alpha per pixel, row-major; 0 outside every seam. The seam math in
/// isolation, kept for tests and for the future warp shader.
pub fn alpha_map(width: u32, height: u32, spec: &OverlaySpec) -> Vec<u8> {
    let mut map = vec![0u8; width as usize * height as usize];
    for y in 0..height {
        for x in 0..width {
            if let Some(t) = attenuation_at(spec, x as f64 + 0.5, y as f64 + 0.5) {
                map[(y * width + x) as usize] = ramp_alpha(t, spec.gamma);
            }
        }
    }
    map
}

/// The complete overlay image: premultiplied BGRA bytes, row-major.
///
/// Seam pixels are black at the ramp alpha. Everything else is white at
/// `blackLift` alpha, which composites to `out = lift + (1−lift)·content` —
/// the standard black-level rescale — matching the un-doubled regions to the
/// seams' doubled projector black. Lift zero leaves them fully transparent.
pub fn pixel_map(width: u32, height: u32, spec: &OverlaySpec) -> Vec<u8> {
    let lift = (spec.black_lift.clamp(0.0, 1.0) * 255.0).round() as u8;
    let mut pixels = vec![0u8; width as usize * height as usize * 4];
    for y in 0..height {
        for x in 0..width {
            let offset = ((y * width + x) * 4) as usize;
            match attenuation_at(spec, x as f64 + 0.5, y as f64 + 0.5) {
                // Black: only the alpha byte is nonzero.
                Some(t) => pixels[offset + 3] = ramp_alpha(t, spec.gamma),
                // White at the lift alpha; premultiplied, so B=G=R=A.
                None => {
                    pixels[offset] = lift;
                    pixels[offset + 1] = lift;
                    pixels[offset + 2] = lift;
                    pixels[offset + 3] = lift;
                }
            }
        }
    }
    pixels
}

#[cfg(test)]
mod tests {
    use super::*;

    fn participant(name: &str, x: i32, y: i32, width: i32, height: i32) -> Participant {
        Participant {
            name: name.to_string(),
            rect: Rect {
                x,
                y,
                width,
                height,
            },
        }
    }

    fn specs_for(participants: &[Participant]) -> Vec<OverlaySpec> {
        overlay_specs(participants, 2.2, 0.0)
    }

    #[test]
    fn two_overlapping_outputs_get_mirrored_ramps() {
        // 160 px of shared strip: the classic two-projector soft edge.
        let specs = specs_for(&[
            participant("DP-1", 0, 0, 1920, 1080),
            participant("DP-2", 1760, 0, 1920, 1080),
        ]);
        assert_eq!(specs.len(), 2);

        let left = &specs[0];
        assert_eq!(left.output, "DP-1");
        assert_eq!(left.ramps.len(), 1);
        // The seam sits at the right edge of DP-1, in its own coordinates.
        assert_eq!(
            left.ramps[0].rect,
            Rect {
                x: 1760,
                y: 0,
                width: 160,
                height: 1080
            }
        );
        assert_eq!(left.ramps[0].fade_to, FadeTo::Right);

        let right = &specs[1];
        assert_eq!(
            right.ramps[0].rect,
            Rect {
                x: 0,
                y: 0,
                width: 160,
                height: 1080
            }
        );
        assert_eq!(right.ramps[0].fade_to, FadeTo::Left);
    }

    #[test]
    fn a_row_of_four_has_three_seams() {
        let specs = specs_for(&[
            participant("DP-1", 0, 0, 1920, 1200),
            participant("DP-2", 1760, 0, 1920, 1200),
            participant("DP-3", 3520, 0, 1920, 1200),
            participant("DP-4", 5280, 0, 1920, 1200),
        ]);
        assert_eq!(specs.len(), 4);
        // The middle projectors blend on both sides; the ends on one.
        assert_eq!(specs[0].ramps.len(), 1);
        assert_eq!(specs[1].ramps.len(), 2);
        assert_eq!(specs[2].ramps.len(), 2);
        assert_eq!(specs[3].ramps.len(), 1);
    }

    #[test]
    fn separated_outputs_produce_nothing() {
        let specs = specs_for(&[
            participant("DP-1", 0, 0, 1920, 1080),
            participant("DP-2", 1920, 0, 1920, 1080),
        ]);
        assert!(specs.is_empty(), "adjacent-but-not-overlapping has no seam");
    }

    #[test]
    fn a_mirrored_output_is_not_a_seam() {
        // The sway same-position trick: a confidence monitor showing the
        // projector's picture. Fading "the overlap" would black out both.
        let specs = specs_for(&[
            participant("PROJECTOR", 0, 0, 1920, 1080),
            participant("MONITOR", 0, 0, 1920, 1080),
        ]);
        assert!(specs.is_empty(), "a full mirror must produce no ramps");

        // Still a mirror when the monitor is a different, contained size.
        let specs = specs_for(&[
            participant("PROJECTOR", 0, 0, 1920, 1080),
            participant("MONITOR", 0, 0, 1280, 1024),
        ]);
        assert!(specs.is_empty(), "a contained mirror must produce no ramps");
    }

    #[test]
    fn an_operator_monitor_beside_the_wall_is_untouched() {
        // Two projectors overlap; the monitor sits beyond them, no overlap.
        let specs = specs_for(&[
            participant("DP-1", 0, 0, 1920, 1080),
            participant("DP-2", 1760, 0, 1920, 1080),
            participant("HDMI-A-1", 3680, 0, 1920, 1080),
        ]);
        assert_eq!(specs.len(), 2);
        assert!(specs.iter().all(|spec| spec.output != "HDMI-A-1"));
    }

    #[test]
    fn vertical_stacks_fade_up_and_down() {
        let specs = specs_for(&[
            participant("TOP", 0, 0, 1920, 1080),
            participant("BOT", 0, 960, 1920, 1080),
        ]);
        // Sorted by name: specs[0] is BOT, specs[1] is TOP. The upper
        // projector hands over toward its bottom edge, and vice versa.
        assert_eq!(specs[1].ramps[0].fade_to, FadeTo::Bottom);
        assert_eq!(specs[0].ramps[0].fade_to, FadeTo::Top);
        assert_eq!(specs[0].ramps[0].rect.height, 120);
    }

    #[test]
    fn a_2x2_grid_gets_two_ramps_per_output_and_no_diagonals() {
        // Each output overlaps its horizontal neighbour, vertical neighbour,
        // and (in the middle) its diagonal. The diagonal pair must not add a
        // third ramp: the corner is already the product of the other two.
        let specs = specs_for(&[
            participant("A", 0, 0, 1920, 1080),
            participant("B", 1760, 0, 1920, 1080),
            participant("C", 0, 960, 1920, 1080),
            participant("D", 1760, 960, 1920, 1080),
        ]);
        assert_eq!(specs.len(), 4);
        for spec in &specs {
            assert_eq!(
                spec.ramps.len(),
                2,
                "{} should blend on exactly two edges",
                spec.output
            );
        }
    }

    #[test]
    fn the_wall_shares_one_gamma_and_lift() {
        let specs = overlay_specs(
            &[
                participant("DP-1", 0, 0, 1920, 1080),
                participant("DP-2", 1760, 0, 1920, 1080),
            ],
            2.4,
            0.05,
        );
        for spec in &specs {
            assert_eq!(spec.gamma, 2.4);
            assert_eq!(spec.black_lift, 0.05);
        }
    }

    fn ramp_only_spec(gamma: f64, width: i32) -> OverlaySpec {
        OverlaySpec {
            output: "DP-1".into(),
            gamma,
            black_lift: 0.0,
            ramps: vec![RampSpec {
                rect: Rect {
                    x: 0,
                    y: 0,
                    width,
                    height: 1,
                },
                fade_to: FadeTo::Right,
            }],
        }
    }

    #[test]
    fn gamma_shapes_the_alpha() {
        let map = alpha_map(100, 1, &ramp_only_spec(2.2, 100));
        // Midway across the ramp, half the light must remain: the *signal*
        // multiplier is 0.5^(1/2.2) ≈ 0.73, so alpha ≈ 0.27 — noticeably
        // less than the 0.5 a signal-space gradient would apply.
        let mid = map[50] as f64 / 255.0;
        assert!((mid - 0.27).abs() < 0.02, "alpha at midpoint was {mid}");
        // The interior edge leaves the picture essentially untouched (the
        // first pixel centre sits half a pixel into the ramp, so ≤1/255).
        assert!(map[0] <= 1, "interior edge should be clear, was {}", map[0]);
        // The far edge is almost fully handed over. Not 255: gamma shaping is
        // steepest near zero (0.5% of the light still needs 9% of the signal),
        // and the last pixel centre sits half a pixel shy of the true edge —
        // where the neighbour's mirrored ramp makes up exactly the remainder.
        assert!(
            map[99] >= 220,
            "far edge should be near-opaque, was {}",
            map[99]
        );
    }

    #[test]
    fn luminance_sums_to_one_across_a_seam() {
        // The mathematical point of the whole feature: at every column of the
        // seam, what the left projector still shows plus what the right one
        // shows adds to the full picture, within 8-bit quantisation.
        let gamma = 2.2;
        let specs = overlay_specs(
            &[
                participant("L", 0, 0, 1920, 1080),
                participant("R", 1760, 0, 1920, 1080),
            ],
            gamma,
            0.0,
        );
        let left = alpha_map(1920, 1, &specs[0]);
        let right = alpha_map(1920, 1, &specs[1]);
        for offset in 0..160u32 {
            let a = 1.0 - left[(1760 + offset) as usize] as f64 / 255.0;
            let b = 1.0 - right[offset as usize] as f64 / 255.0;
            let luminance = a.powf(gamma) + b.powf(gamma);
            assert!(
                (luminance - 1.0).abs() < 0.02,
                "column {offset}: luminance sums to {luminance}"
            );
        }
    }

    #[test]
    fn gamma_one_is_a_plain_linear_gradient() {
        let map = alpha_map(256, 1, &ramp_only_spec(1.0, 256));
        let quarter = map[64] as f64 / 255.0;
        assert!((quarter - 0.25).abs() < 0.01);
    }

    #[test]
    fn overlapping_ramps_multiply_in_light() {
        // A grid corner: horizontal and vertical ramps cover the same pixels.
        let spec = OverlaySpec {
            output: "A".into(),
            gamma: 2.0,
            black_lift: 0.0,
            ramps: vec![
                RampSpec {
                    rect: Rect {
                        x: 0,
                        y: 0,
                        width: 10,
                        height: 10,
                    },
                    fade_to: FadeTo::Right,
                },
                RampSpec {
                    rect: Rect {
                        x: 0,
                        y: 0,
                        width: 10,
                        height: 10,
                    },
                    fade_to: FadeTo::Bottom,
                },
            ],
        };
        let map = alpha_map(10, 10, &spec);
        // At (5,5) each ramp transmits 0.45 of the light; together 0.2025.
        let alpha = map[5 * 10 + 5] as f64 / 255.0;
        let transmitted = (1.0 - alpha).powf(2.0);
        assert!(
            (transmitted - 0.2025).abs() < 0.02,
            "transmitted {transmitted}"
        );
    }

    #[test]
    fn black_lift_paints_white_outside_the_seam_only() {
        let spec = OverlaySpec {
            output: "DP-1".into(),
            gamma: 2.2,
            black_lift: 0.1,
            ramps: vec![RampSpec {
                rect: Rect {
                    x: 0,
                    y: 0,
                    width: 4,
                    height: 1,
                },
                fade_to: FadeTo::Left,
            }],
        };
        let pixels = pixel_map(8, 1, &spec);

        // Inside the seam: black — colour bytes zero, only alpha set. The
        // doubled projector black is the lift there; adding more would glow.
        assert_eq!(&pixels[0..3], &[0, 0, 0], "seam pixels must stay black");
        // Outside: premultiplied white at the lift alpha. Compositing gives
        // out = 0.9·content + 0.1·white — the black-level rescale.
        let lift = (0.1f64 * 255.0).round() as u8;
        assert_eq!(&pixels[6 * 4..6 * 4 + 4], &[lift, lift, lift, lift]);
    }

    #[test]
    fn zero_lift_leaves_the_rest_of_the_screen_alone() {
        let pixels = pixel_map(8, 1, &ramp_only_spec(2.2, 4));
        assert_eq!(
            &pixels[6 * 4..6 * 4 + 4],
            &[0, 0, 0, 0],
            "no lift: fully transparent outside the seam"
        );
    }

    #[test]
    fn the_spec_serialises_stably_for_the_subcommand() {
        // The spec crosses a process boundary as JSON; field names are ABI.
        let spec = OverlaySpec {
            output: "DP-1".into(),
            gamma: 2.2,
            black_lift: 0.04,
            ramps: vec![RampSpec {
                rect: Rect {
                    x: 1760,
                    y: 0,
                    width: 160,
                    height: 1080,
                },
                fade_to: FadeTo::Right,
            }],
        };
        let json = serde_json::to_string(&spec).unwrap();
        assert!(json.contains(r#""fadeTo":"right""#), "{json}");
        assert!(json.contains(r#""blackLift":0.04"#), "{json}");
        let back: OverlaySpec = serde_json::from_str(&json).unwrap();
        assert_eq!(back, spec);
    }
}
