//! Pure edge-blend math: seams from layout, gamma-shaped ramps from seams.
//!
//! Everything here is deterministic geometry and arithmetic, deliberately free
//! of Wayland, processes, and IO, for the same reason the output planner is
//! pure: the correctness rules — ramps exactly spanning the shared region,
//! luminance summing to one across a seam — are testable on any machine.
//! The future warp client (corner pinning) reuses this module unchanged; a
//! homography changes where a ramp is *drawn*, not what its values are.

use serde::{Deserialize, Serialize};

use crate::model::{ProjectionConfig, Rect, TestPattern};

/// An output taking part in seam derivation: its place in the global layout.
#[derive(Debug, Clone, PartialEq)]
pub struct Participant {
    pub name: String,
    pub rect: Rect,
    /// Whether a display is currently attached to this output.
    ///
    /// Absent outputs still take full part in the plan: they shape the
    /// canvas and every ramp, because the *configuration* describes the
    /// installation and a dark projector does not change where the light
    /// from its neighbours lands. Only presentation is skipped — the region
    /// simply goes unshown. So unplugging one projector never alters what
    /// the others display, and plugging it back in needs no re-authoring.
    pub connected: bool,
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
    /// The configured black-level compensation, carried so a change to it
    /// repaints the overlay. It is not *applied* here: these overlays run
    /// only where nothing overlaps, so there is no raised black floor to
    /// match. The canvas path resolves it per pixel through [`Coverage`].
    #[serde(default)]
    pub black_lift: f64,
    /// This output's rectangle in the global layout, so patterns can draw in
    /// global coordinates and continue exactly across a seam.
    #[serde(default)]
    pub rect: Rect,
    /// Test pattern to draw instead of showing the content through.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<TestPattern>,
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

/// One projector's slice of the canvas: which region it shows, and how its
/// seam edges fade. `source` is in canvas coordinates; `ramps` are in
/// slice-local coordinates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SliceSpec {
    pub output: String,
    pub source: Rect,
    pub ramps: Vec<RampSpec>,
}

/// Everything the slicer process needs: capture this, cut it up like that.
/// Serialized to the `suede slice` subcommand verbatim.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlicerSpec {
    /// Output to capture — the headless canvas.
    pub source: String,
    pub canvas_width: i32,
    pub canvas_height: i32,
    pub gamma: f64,
    pub black_lift: f64,
    /// Render this instead of capturing, for alignment and calibration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<TestPattern>,
    pub slices: Vec<SliceSpec>,
}

/// Everything the reconciler derives from the configured layout.
#[derive(Debug, Clone, PartialEq)]
pub struct CanvasPlan {
    pub canvas_width: i32,
    pub canvas_height: i32,
    /// Where each output goes in *sway's* layout: a plain edge-to-edge
    /// tiling, row-major by the configured layout. Sway never sees overlaps.
    pub sway_positions: Vec<(String, i32, i32)>,
    /// The slices, ready for a [`SlicerSpec`] once the canvas output exists.
    pub slices: Vec<SliceSpec>,
}

/// Derive the canvas from the configured layout.
///
/// **The layout is the projection configuration.** Each participant's
/// rectangle is where its beam lands in canvas space; wherever two
/// rectangles intersect, both projectors show that region of the canvas, and
/// with `blend` on each fades its own copy toward the neighbour. Every seam
/// carries its own width because every seam *is* its own intersection —
/// a top row may overlap differently from a bottom row, grids included.
///
/// The plan is a function of the *configuration alone*. Whether a display is
/// currently attached decides only which slices get presented — never the
/// canvas size and never a ramp. That is what keeps a failure local: pull the
/// cable on one projector and the others carry on showing exactly the pixels
/// they showed a moment earlier, with the dead projector's region simply
/// unlit. Deriving any of the geometry from what happens to be plugged in
/// would make one loose connector reflow the whole installation.
///
/// Returns `None` when nothing overlaps: sway can tile non-overlapping
/// layouts natively, and the direct path costs nothing per frame.
pub fn canvas_plan(
    participants: &[Participant],
    config: Option<&ProjectionConfig>,
) -> Option<CanvasPlan> {
    if participants.len() < 2 {
        return None;
    }
    let any_overlap = participants.iter().enumerate().any(|(i, a)| {
        participants[i + 1..]
            .iter()
            .any(|b| intersect(&a.rect, &b.rect).is_some())
    });
    if !any_overlap {
        return None;
    }

    // Normalise so the canvas starts at 0,0 wherever the user drew it.
    let min_x = participants.iter().map(|p| p.rect.x).min().unwrap_or(0);
    let min_y = participants.iter().map(|p| p.rect.y).min().unwrap_or(0);
    let rects: Vec<(String, Rect)> = participants
        .iter()
        .map(|p| {
            (
                p.name.clone(),
                Rect {
                    x: p.rect.x - min_x,
                    y: p.rect.y - min_y,
                    width: p.rect.width,
                    height: p.rect.height,
                },
            )
        })
        .collect();

    let canvas_width = rects.iter().map(|(_, r)| r.x + r.width).max().unwrap_or(0);
    let canvas_height = rects.iter().map(|(_, r)| r.y + r.height).max().unwrap_or(0);

    // Ramps from pairwise intersections, when blending is on.
    let blend = config.is_some_and(|p| p.blend);
    let mut ramps: Vec<Vec<RampSpec>> = vec![Vec::new(); rects.len()];
    if blend {
        for i in 0..rects.len() {
            for j in (i + 1)..rects.len() {
                let (a, b) = (&rects[i].1, &rects[j].1);
                let Some(seam) = intersect(a, b) else {
                    continue;
                };
                // A near-total overlap is a deliberate duplicate (stacked
                // projectors, a mirror) — both show it at full strength.
                if area(&seam) * 5 >= area(a).min(area(b)) * 4 {
                    continue;
                }
                // Edge adjacency: the seam spans (most of) the shorter
                // output along the axis perpendicular to the fade. In a
                // grid, a corner already receives the product of each
                // output's horizontal and vertical ramps, which sums to
                // constant luminance by construction — a diagonal pair must
                // not add a third.
                let horizontal = seam.height * 2 >= a.height.min(b.height);
                let vertical = seam.width * 2 >= a.width.min(b.width);
                let center_dx = (a.x * 2 + a.width) - (b.x * 2 + b.width);
                let center_dy = (a.y * 2 + a.height) - (b.y * 2 + b.height);
                let axis = match (horizontal, vertical) {
                    (true, false) => Axis::X,
                    (false, true) => Axis::Y,
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
                    let origin = &rects[index].1;
                    ramps[index].push(RampSpec {
                        rect: Rect {
                            x: seam.x - origin.x,
                            y: seam.y - origin.y,
                            width: seam.width,
                            height: seam.height,
                        },
                        fade_to,
                    });
                }
            }
        }
    }

    // Only connected outputs can be presented on. Everything above — the
    // canvas size and every ramp — was computed from the full configured
    // layout, so a disconnected output leaves its region unshown without
    // altering a single pixel of its neighbours'.
    let slices: Vec<SliceSpec> = rects
        .iter()
        .zip(ramps)
        .zip(participants)
        .filter(|((_, _), participant)| participant.connected)
        .map(|(((name, rect), ramps), _)| SliceSpec {
            output: name.clone(),
            source: *rect,
            ramps,
        })
        .collect();

    // Sway's layout: row-major order of the configured layout, tiled edge to
    // edge in one row. Purely internal — presenters cover every output.
    let mut order: Vec<usize> = (0..rects.len())
        .filter(|&i| participants[i].connected)
        .collect();
    order.sort_by_key(|&i| (rects[i].1.y, rects[i].1.x));
    let mut sway_positions = Vec::new();
    let mut x = 0;
    for index in order {
        sway_positions.push((rects[index].0.clone(), x, 0));
        x += rects[index].1.width;
    }

    Some(CanvasPlan {
        canvas_width,
        canvas_height,
        sway_positions,
        slices,
    })
}

/// How many projectors light each point of the canvas.
///
/// Projector black is additive and cannot be subtracted: a region lit by `n`
/// projectors sits at `n` times one projector's black floor, so the only way
/// to make an installation *look* even is to raise everywhere else to meet
/// its worst spot. That makes the shortfall a function of coverage, and
/// coverage a function of the whole layout — which is why this is derived
/// from every slice's rectangle rather than from one projector's seams.
///
/// A plain two-projector blend has coverage 1 and 2 only, and reduces to the
/// original rule exactly: lift outside the seam, none inside it. A 2×2 grid
/// has three regimes — 1, 2 and 4 — and needs all three.
#[derive(Debug, Clone, PartialEq)]
pub struct Coverage {
    rects: Vec<Rect>,
    max: u32,
}

fn covering(rects: &[Rect], x: f64, y: f64) -> u32 {
    rects
        .iter()
        .filter(|r| {
            x >= r.x as f64
                && x < (r.x + r.width) as f64
                && y >= r.y as f64
                && y < (r.y + r.height) as f64
        })
        .count() as u32
}

impl Coverage {
    /// Every projector's rectangle, in one shared coordinate space.
    pub fn new(rects: impl IntoIterator<Item = Rect>) -> Self {
        let rects: Vec<Rect> = rects
            .into_iter()
            .filter(|r| r.width > 0 && r.height > 0)
            .collect();
        // Coverage only changes where a rectangle begins or ends, so the
        // true maximum is found by sampling the centre of every cell of the
        // grid those edges induce — a few dozen probes, not megapixels.
        let mut xs: Vec<i32> = rects.iter().flat_map(|r| [r.x, r.x + r.width]).collect();
        let mut ys: Vec<i32> = rects.iter().flat_map(|r| [r.y, r.y + r.height]).collect();
        xs.sort_unstable();
        xs.dedup();
        ys.sort_unstable();
        ys.dedup();
        let mut max = 0;
        for xw in xs.windows(2) {
            for yw in ys.windows(2) {
                let x = f64::from(xw[0] + xw[1]) / 2.0;
                let y = f64::from(yw[0] + yw[1]) / 2.0;
                max = max.max(covering(&rects, x, y));
            }
        }
        Self { rects, max }
    }

    /// The highest coverage anywhere — the black floor everything must match.
    pub fn max(&self) -> u32 {
        self.max
    }

    /// How many projectors light this point.
    pub fn at(&self, x: f64, y: f64) -> u32 {
        covering(&self.rects, x, y)
    }

    /// The lift *one* projector applies at this point.
    ///
    /// `black_lift` is calibrated as the lift that adds one projector's worth
    /// of black, so a point short by `max − n` projectors needs that much
    /// added — divided by `n`, because every projector lighting the point
    /// applies the overlay and their contributions add just as their black
    /// does. At full coverage the shortfall is zero and nothing is applied.
    pub fn lift(&self, black_lift: f64, x: f64, y: f64) -> f64 {
        let n = self.at(x, y);
        if n == 0 || n >= self.max {
            return 0.0;
        }
        (black_lift.clamp(0.0, 1.0) * f64::from(self.max - n) / f64::from(n)).clamp(0.0, 1.0)
    }
}

/// The signal transfer for one pixel of a slice, as fixed-point `(a, b)`
/// where `out = (a·in) >> 8 + b`.
///
/// Combines the gamma-shaped ramps (inside seams, multiplied where they
/// overlap at grid corners) with the black-level rescale, so the slicer
/// applies one table in one pass.
///
/// `lift` is the lift *resolved for this pixel* — [`Coverage::lift`] — not
/// the configured `blackLift`. The two effects are independent: in a grid, a
/// two-projector seam is both inside a ramp and short of the four-projector
/// centre's black, so it needs a ramp *and* a lift. Only in the plain
/// two-projector case do they never coincide.
pub fn pixel_transfer(ramps: &[RampSpec], gamma: f64, lift: f64, x: i32, y: i32) -> (u16, u8) {
    let transmitted = ramps_attenuation(ramps, x as f64 + 0.5, y as f64 + 0.5)
        .map_or(1.0, |t| t.clamp(0.0, 1.0).powf(1.0 / gamma));
    let lift = lift.clamp(0.0, 1.0);
    (
        ((1.0 - lift) * transmitted * 256.0).round() as u16,
        (lift * 255.0).round() as u8,
    )
}

enum Axis {
    X,
    Y,
}

/// One pattern overlay per output, for bench alignment when no canvas runs.
///
/// Ramps live in the slicer now — the canvas is where seams exist. These
/// overlays only carry test patterns, drawn in layout coordinates so two
/// physically-aligned projectors superimpose them.
pub fn overlay_specs(participants: &[Participant], config: &ProjectionConfig) -> Vec<OverlaySpec> {
    if config.test_pattern.is_none() {
        return Vec::new();
    }
    let mut specs: Vec<OverlaySpec> = participants
        .iter()
        .filter(|participant| participant.connected)
        .map(|participant| OverlaySpec {
            output: participant.name.clone(),
            gamma: config.gamma,
            black_lift: config.black_lift,
            rect: participant.rect,
            pattern: config.test_pattern,
            ramps: Vec::new(),
        })
        .collect();
    specs.sort_by(|a, b| a.output.cmp(&b.output));
    specs
}

/// Combined light attenuation of every ramp covering the pixel centre
/// `(x, y)`, or `None` when no ramp covers it at all.
///
/// The distinction matters: a covered pixel with attenuation 1.0 is *inside*
/// a seam (no lift there — the doubled projector black is the lift), while an
/// uncovered pixel is outside every seam and receives the black-lift.
fn attenuation_at(spec: &OverlaySpec, x: f64, y: f64) -> Option<f64> {
    ramps_attenuation(&spec.ramps, x, y)
}

fn ramps_attenuation(ramps: &[RampSpec], x: f64, y: f64) -> Option<f64> {
    let mut covered = false;
    let mut transmitted = 1.0;
    for ramp in ramps {
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
/// Seam pixels are black at the ramp alpha; everything else is transparent.
///
/// These overlays are the *no-canvas* path, which the reconciler runs only
/// when nothing in the layout overlaps. Every pixel is therefore lit by
/// exactly one projector, no region sits at a doubled black floor, and the
/// black-level lift is zero by construction — see [`Coverage`], which is
/// what resolves it wherever seams do exist.
pub fn pixel_map(width: u32, height: u32, spec: &OverlaySpec) -> Vec<u8> {
    match spec.pattern {
        None => transparent_map(width, height, spec),
        Some(_) => pattern_map(width, height, spec),
    }
}

/// The normal overlay: content shows through except in the seams.
fn transparent_map(width: u32, height: u32, spec: &OverlaySpec) -> Vec<u8> {
    let mut pixels = vec![0u8; width as usize * height as usize * 4];
    for y in 0..height {
        for x in 0..width {
            let offset = ((y * width + x) * 4) as usize;
            // Black at the ramp alpha inside a seam; fully transparent
            // outside, where there is nothing to compensate for.
            if let Some(t) = attenuation_at(spec, x as f64 + 0.5, y as f64 + 0.5) {
                pixels[offset + 3] = ramp_alpha(t, spec.gamma);
            }
        }
    }
    pixels
}

/// A test pattern: fully opaque, with the ramps applied to the pattern itself
/// exactly as they would be to real content — so what the operator aligns
/// with is what content will experience. As above, this path has no seams and
/// so no lift; the canvas path patterns through [`pixel_transfer`] instead,
/// which carries both.
fn pattern_map(width: u32, height: u32, spec: &OverlaySpec) -> Vec<u8> {
    let rgb = super::pattern::render(width, height, spec);
    let mut pixels = vec![0u8; width as usize * height as usize * 4];
    for y in 0..height {
        for x in 0..width {
            let index = (y * width + x) as usize;
            let source = [rgb[index * 3], rgb[index * 3 + 1], rgb[index * 3 + 2]];
            // In a seam: scale the signal so the *light* is attenuated by t,
            // exactly as the alpha ramp does to content.
            let shaped: [f64; 3] = match attenuation_at(spec, x as f64 + 0.5, y as f64 + 0.5) {
                Some(t) => {
                    let scale = t.powf(1.0 / spec.gamma);
                    [0, 1, 2].map(|c| source[c] as f64 * scale)
                }
                None => [0, 1, 2].map(|c| source[c] as f64),
            };
            let offset = index * 4;
            // Opaque and premultiplied: BGRA from the shaped RGB.
            pixels[offset] = shaped[2].round().clamp(0.0, 255.0) as u8;
            pixels[offset + 1] = shaped[1].round().clamp(0.0, 255.0) as u8;
            pixels[offset + 2] = shaped[0].round().clamp(0.0, 255.0) as u8;
            pixels[offset + 3] = 255;
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
            connected: true,
        }
    }

    /// The same output, configured but with nothing plugged into it.
    fn absent(name: &str, x: i32, y: i32, width: i32, height: i32) -> Participant {
        Participant {
            connected: false,
            ..participant(name, x, y, width, height)
        }
    }

    fn blending() -> ProjectionConfig {
        ProjectionConfig::default()
    }

    // --- the canvas plan: the layout IS the configuration -----------------

    #[test]
    fn an_overlapping_pair_becomes_a_canvas_with_mirrored_ramps() {
        let plan = canvas_plan(
            &[
                participant("DP-3", 0, 0, 1920, 1080),
                participant("DP-1", 1760, 0, 1920, 1080),
            ],
            Some(&blending()),
        )
        .expect("overlap must produce a plan");

        assert_eq!((plan.canvas_width, plan.canvas_height), (3680, 1080));
        // Sway sees a plain tiling, never the overlap.
        assert_eq!(
            plan.sway_positions,
            vec![("DP-3".into(), 0, 0), ("DP-1".into(), 1920, 0)]
        );

        let left = &plan.slices[0];
        assert_eq!(left.output, "DP-3");
        assert_eq!(
            left.source,
            Rect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080
            }
        );
        assert_eq!(left.ramps.len(), 1);
        assert_eq!(left.ramps[0].fade_to, FadeTo::Right);
        assert_eq!(
            left.ramps[0].rect,
            Rect {
                x: 1760,
                y: 0,
                width: 160,
                height: 1080
            }
        );

        let right = &plan.slices[1];
        assert_eq!(right.source.x, 1760);
        assert_eq!(right.ramps[0].fade_to, FadeTo::Left);
        assert_eq!(right.ramps[0].rect.width, 160);
    }

    #[test]
    fn losing_a_projector_changes_nothing_the_others_show() {
        // The requirement, stated as a test: take a working three-projector
        // installation, unplug the middle one, and every pixel the survivors
        // were showing must still be the same pixel of the same canvas.
        let configured = [
            participant("DP-3", 0, 0, 1920, 1080),
            participant("DP-1", 1760, 0, 1920, 1080),
            participant("DP-2", 3520, 0, 1920, 1080),
        ];
        let whole = canvas_plan(&configured, Some(&blending())).expect("plan");

        let mut degraded = configured;
        degraded[1].connected = false;
        let degraded = canvas_plan(&degraded, Some(&blending())).expect("plan");

        // The canvas is unchanged, so the app renders exactly as before and
        // is never reloaded at a different size.
        assert_eq!(
            (degraded.canvas_width, degraded.canvas_height),
            (whole.canvas_width, whole.canvas_height)
        );
        assert_eq!(
            (degraded.canvas_width, degraded.canvas_height),
            (5440, 1080)
        );

        // The survivors keep their source rectangles and their ramps — the
        // seam toward the dark projector still fades, so nothing brightens.
        for name in ["DP-3", "DP-2"] {
            let before = whole.slices.iter().find(|s| s.output == name).unwrap();
            let after = degraded.slices.iter().find(|s| s.output == name).unwrap();
            assert_eq!(before, after, "{name} must be untouched");
        }
        // The absent output is simply not presented.
        assert!(!degraded.slices.iter().any(|s| s.output == "DP-1"));
        assert!(!degraded.sway_positions.iter().any(|(n, _, _)| n == "DP-1"));
    }

    #[test]
    fn an_output_that_was_never_plugged_in_still_shapes_the_canvas() {
        // Configuring a rig from the desk before the projectors arrive: the
        // canvas is the full installation, and the one display present shows
        // its own region of it, already blended for the neighbour to come.
        let plan = canvas_plan(
            &[
                participant("DP-3", 0, 0, 1920, 1080),
                absent("DP-1", 1760, 0, 1920, 1080),
            ],
            Some(&blending()),
        )
        .expect("a configured overlap is a plan even with nothing attached");

        assert_eq!((plan.canvas_width, plan.canvas_height), (3680, 1080));
        assert_eq!(plan.slices.len(), 1);
        assert_eq!(plan.slices[0].output, "DP-3");
        assert_eq!(plan.slices[0].ramps.len(), 1);
        assert_eq!(plan.slices[0].ramps[0].fade_to, FadeTo::Right);
        assert_eq!(plan.sway_positions, vec![("DP-3".into(), 0, 0)]);
    }

    #[test]
    fn a_test_pattern_is_only_sent_to_attached_displays() {
        let config = ProjectionConfig {
            test_pattern: Some(TestPattern::Grid),
            ..ProjectionConfig::default()
        };
        let specs = overlay_specs(
            &[
                participant("DP-3", 0, 0, 1920, 1080),
                absent("DP-1", 1920, 0, 1920, 1080),
            ],
            &config,
        );
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].output, "DP-3");
    }

    #[test]
    fn every_seam_carries_its_own_overlap() {
        // A row of three where the rigger got 160 px on one seam and 100 on
        // the other. No single number can describe this; the layout can.
        let plan = canvas_plan(
            &[
                participant("A", 0, 0, 1920, 1080),
                participant("B", 1760, 0, 1920, 1080),
                participant("C", 3580, 0, 1920, 1080),
            ],
            Some(&blending()),
        )
        .unwrap();

        assert_eq!(plan.canvas_width, 5500);
        let middle = &plan.slices[1];
        assert_eq!(middle.ramps.len(), 2);
        let widths: Vec<i32> = middle.ramps.iter().map(|r| r.rect.width).collect();
        assert!(widths.contains(&160) && widths.contains(&100), "{widths:?}");
    }

    #[test]
    fn rows_can_overlap_differently_from_columns() {
        // The 2x2 the redesign asked for: the top pair overlaps 160 in x,
        // the rows overlap 90 in y. Corners come out as the product of the
        // horizontal and vertical ramps; no diagonal ramp is generated.
        let plan = canvas_plan(
            &[
                participant("A", 0, 0, 1920, 1080),
                participant("B", 1760, 0, 1920, 1080),
                participant("C", 0, 990, 1920, 1080),
                participant("D", 1760, 990, 1920, 1080),
            ],
            Some(&blending()),
        )
        .unwrap();

        assert_eq!((plan.canvas_width, plan.canvas_height), (3680, 2070));
        for slice in &plan.slices {
            assert_eq!(
                slice.ramps.len(),
                2,
                "{} should fade on exactly two edges",
                slice.output
            );
        }
        let a = &plan.slices[0];
        let horizontal = a.ramps.iter().find(|r| r.fade_to == FadeTo::Right).unwrap();
        let vertical = a
            .ramps
            .iter()
            .find(|r| r.fade_to == FadeTo::Bottom)
            .unwrap();
        assert_eq!(horizontal.rect.width, 160);
        assert_eq!(vertical.rect.height, 90);
    }

    #[test]
    fn a_layout_without_overlaps_needs_no_canvas() {
        // Sway tiles non-overlapping layouts natively; the slicer would be
        // pure overhead.
        assert!(canvas_plan(
            &[
                participant("DP-3", 0, 0, 1920, 1080),
                participant("DP-1", 1920, 0, 1920, 1080),
            ],
            Some(&blending()),
        )
        .is_none());
        assert!(canvas_plan(&[participant("DP-1", 0, 0, 1920, 1080)], None).is_none());
    }

    #[test]
    fn the_canvas_normalises_wherever_the_layout_was_drawn() {
        // An operator who drew the layout starting at 500,300 still gets a
        // canvas anchored at zero.
        let plan = canvas_plan(
            &[
                participant("L", 500, 300, 1920, 1080),
                participant("R", 2260, 300, 1920, 1080),
            ],
            Some(&blending()),
        )
        .unwrap();
        assert_eq!((plan.canvas_width, plan.canvas_height), (3680, 1080));
        assert_eq!(plan.slices[0].source.x, 0);
        assert_eq!(plan.slices[0].source.y, 0);
    }

    #[test]
    fn a_full_mirror_is_duplicated_but_never_ramped() {
        // Two projectors stacked for brightness, or a confidence monitor:
        // both show the region at full strength.
        let plan = canvas_plan(
            &[
                participant("MAIN", 0, 0, 1920, 1080),
                participant("STACKED", 0, 0, 1920, 1080),
            ],
            Some(&blending()),
        )
        .unwrap();
        assert!(plan.slices.iter().all(|slice| slice.ramps.is_empty()));
        assert_eq!(plan.slices[0].source, plan.slices[1].source);
    }

    #[test]
    fn blend_off_still_slices_but_does_not_fade() {
        // Physically overlapping beams need the duplication even unblended.
        let plan = canvas_plan(
            &[
                participant("L", 0, 0, 1920, 1080),
                participant("R", 1760, 0, 1920, 1080),
            ],
            Some(&ProjectionConfig {
                blend: false,
                ..Default::default()
            }),
        )
        .unwrap();
        assert!(plan.slices.iter().all(|slice| slice.ramps.is_empty()));

        // And with no projection section at all, the same.
        let plan = canvas_plan(
            &[
                participant("L", 0, 0, 1920, 1080),
                participant("R", 1760, 0, 1920, 1080),
            ],
            None,
        )
        .unwrap();
        assert!(plan.slices.iter().all(|slice| slice.ramps.is_empty()));
    }

    // --- the per-pixel transfer -------------------------------------------

    #[test]
    fn seam_light_sums_to_one_through_the_transfer() {
        let plan = canvas_plan(
            &[
                participant("L", 0, 0, 1920, 1080),
                participant("R", 1760, 0, 1920, 1080),
            ],
            Some(&blending()),
        )
        .unwrap();
        let gamma = 2.2;
        let left = &plan.slices[0];
        let right = &plan.slices[1];
        for offset in 0..160 {
            let (a_left, _) = pixel_transfer(&left.ramps, gamma, 0.0, 1760 + offset, 500);
            let (a_right, _) = pixel_transfer(&right.ramps, gamma, 0.0, offset, 500);
            let light = (a_left as f64 / 256.0).powf(gamma) + (a_right as f64 / 256.0).powf(gamma);
            assert!(
                (light - 1.0).abs() < 0.03,
                "column {offset}: light sums to {light}"
            );
        }
        // Outside the seam: identity.
        assert_eq!(pixel_transfer(&left.ramps, gamma, 0.0, 800, 500), (256, 0));
    }

    /// The lift a slice applies at a global canvas point, as the slicer
    /// resolves it: coverage over the whole layout, then the local pixel.
    fn lift_at(plan: &CanvasPlan, slice: &SliceSpec, black_lift: f64, gx: i32, gy: i32) -> u8 {
        let coverage = Coverage::new(plan.slices.iter().map(|s| s.source));
        let lift = coverage.lift(black_lift, f64::from(gx) + 0.5, f64::from(gy) + 0.5);
        pixel_transfer(
            &slice.ramps,
            2.2,
            lift,
            gx - slice.source.x,
            gy - slice.source.y,
        )
        .1
    }

    #[test]
    fn black_lift_applies_outside_seams_only() {
        // Two projectors: coverage is 1 or 2, so the general rule collapses
        // to the original one — full lift outside, none inside.
        let plan = canvas_plan(
            &[
                participant("L", 0, 0, 1920, 1080),
                participant("R", 1760, 0, 1920, 1080),
            ],
            Some(&blending()),
        )
        .unwrap();
        let left = &plan.slices[0];
        let coverage = Coverage::new(plan.slices.iter().map(|s| s.source));
        assert_eq!(coverage.max(), 2);

        // Outside: out = lift + (1-lift)*in.
        let lift = coverage.lift(0.1, 800.5, 500.5);
        let (a, b) = pixel_transfer(&left.ramps, 2.2, lift, 800, 500);
        assert_eq!(b, 26, "lift offset should be 0.1*255");
        assert_eq!(a, 230, "multiplier should be (1-0.1)*256");
        // Inside the seam: no lift, the doubled projector black is the lift.
        assert_eq!(lift_at(&plan, left, 0.1, 1840, 500), 0);
    }

    #[test]
    fn a_grid_lifts_every_region_by_its_own_shortfall() {
        // A 2×2 with 160px overlaps both ways. Three black floors exist —
        // one, two and four projectors — and only the four-way centre is
        // already at the worst of them.
        let plan = canvas_plan(
            &[
                participant("TL", 0, 0, 1920, 1080),
                participant("TR", 1760, 0, 1920, 1080),
                participant("BL", 0, 920, 1920, 1080),
                participant("BR", 1760, 920, 1920, 1080),
            ],
            Some(&blending()),
        )
        .unwrap();
        let coverage = Coverage::new(plan.slices.iter().map(|s| s.source));
        assert_eq!(coverage.max(), 4, "the centre is lit by all four");
        assert_eq!(coverage.at(500.5, 500.5), 1);
        assert_eq!(coverage.at(1840.5, 500.5), 2);
        assert_eq!(coverage.at(1840.5, 1000.5), 4);

        // L·(N−n)/n with L = 0.05: a lone projector makes up three floors on
        // its own, each of a seam's pair makes up one, the centre none.
        assert!((coverage.lift(0.05, 500.5, 500.5) - 0.15).abs() < 1e-9);
        assert!((coverage.lift(0.05, 1840.5, 500.5) - 0.05).abs() < 1e-9);
        assert_eq!(coverage.lift(0.05, 1840.5, 1000.5), 0.0);

        // And the centre is the one region the old binary rule got wrong: it
        // sits inside ramps, so it must still receive no lift while the
        // two-way seams around it — also inside ramps — now do.
        let tl = &plan.slices[0];
        assert_eq!(lift_at(&plan, tl, 0.05, 1840, 1000), 0, "four-way centre");
        assert_eq!(lift_at(&plan, tl, 0.05, 1840, 500), 13, "two-way seam");
        assert_eq!(lift_at(&plan, tl, 0.05, 500, 500), 38, "single projector");
    }

    #[test]
    fn total_black_is_even_across_a_grid() {
        // The point of the exercise: every region emits the same black.
        // In units of one projector's black, a point lit by n projectors
        // emits n, and each of them adds its lift on top.
        let plan = canvas_plan(
            &[
                participant("TL", 0, 0, 1920, 1080),
                participant("TR", 1760, 0, 1920, 1080),
                participant("BL", 0, 920, 1920, 1080),
                participant("BR", 1760, 920, 1920, 1080),
            ],
            Some(&blending()),
        )
        .unwrap();
        let coverage = Coverage::new(plan.slices.iter().map(|s| s.source));
        let emitted = |x: f64, y: f64| {
            let n = f64::from(coverage.at(x, y));
            // Each projector emits its own black plus the lift it applies.
            n * (1.0 + coverage.lift(0.05, x, y) / 0.05)
        };
        let centre = emitted(1840.5, 1000.5);
        for (x, y, label) in [
            (500.5, 500.5, "single"),
            (1840.5, 500.5, "vertical seam"),
            (500.5, 1000.5, "horizontal seam"),
        ] {
            let here = emitted(x, y);
            assert!(
                (here - centre).abs() < 1e-9,
                "{label} emits {here}, centre emits {centre}"
            );
        }
    }

    #[test]
    fn corner_regions_multiply_their_ramps() {
        let plan = canvas_plan(
            &[
                participant("A", 0, 0, 1920, 1080),
                participant("B", 1760, 0, 1920, 1080),
                participant("C", 0, 990, 1920, 1080),
                participant("D", 1760, 990, 1920, 1080),
            ],
            Some(&blending()),
        )
        .unwrap();
        // Slice A, mid-corner: both ramps at half strength -> product 0.25
        // of the light, signal multiplier 0.25^(1/gamma).
        let a = &plan.slices[0];
        let (mid, _) = pixel_transfer(&a.ramps, 2.0, 0.0, 1840, 1035);
        let expected = (0.25f64).powf(0.5);
        assert!(
            ((mid as f64 / 256.0) - expected).abs() < 0.02,
            "corner multiplier was {mid}"
        );
    }

    // --- pattern overlays --------------------------------------------------

    #[test]
    fn overlays_exist_only_for_test_patterns() {
        let installation = [
            participant("DP-3", 0, 0, 1920, 1080),
            participant("DP-1", 1920, 0, 1920, 1080),
        ];
        assert!(overlay_specs(&installation, &blending()).is_empty());
        let specs = overlay_specs(
            &installation,
            &ProjectionConfig {
                test_pattern: Some(TestPattern::Grid),
                ..Default::default()
            },
        );
        assert_eq!(specs.len(), 2);
        assert!(specs.iter().all(|spec| spec.ramps.is_empty()));
    }

    // --- the overlay pixel paths (unchanged math, kept honest) -------------

    fn ramp_only_spec(gamma: f64, width: i32) -> OverlaySpec {
        OverlaySpec {
            output: "DP-1".into(),
            gamma,
            black_lift: 0.0,
            rect: Rect::default(),
            pattern: None,
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
        let mid = map[50] as f64 / 255.0;
        assert!((mid - 0.27).abs() < 0.02, "alpha at midpoint was {mid}");
        assert!(map[0] <= 1);
        assert!(map[99] >= 220);
    }

    #[test]
    fn a_pattern_overlay_is_opaque() {
        let spec = OverlaySpec {
            output: "DP-1".into(),
            gamma: 2.2,
            black_lift: 0.0,
            rect: Rect {
                x: 0,
                y: 0,
                width: 100,
                height: 1,
            },
            pattern: Some(TestPattern::White),
            ramps: Vec::new(),
        };
        let pixels = pixel_map(100, 1, &spec);
        assert!((0..100).all(|x| pixels[x * 4 + 3] == 255));
    }

    #[test]
    fn the_slicer_spec_serialises_stably() {
        // The spec crosses a process boundary as JSON; field names are ABI.
        let spec = SlicerSpec {
            source: "HEADLESS-1".into(),
            canvas_width: 3680,
            canvas_height: 1080,
            gamma: 2.2,
            black_lift: 0.04,
            pattern: None,
            slices: vec![SliceSpec {
                output: "DP-3".into(),
                source: Rect {
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1080,
                },
                ramps: vec![RampSpec {
                    rect: Rect {
                        x: 1760,
                        y: 0,
                        width: 160,
                        height: 1080,
                    },
                    fade_to: FadeTo::Right,
                }],
            }],
        };
        let json = serde_json::to_string(&spec).unwrap();
        assert!(json.contains(r#""canvasWidth":3680"#), "{json}");
        assert!(json.contains(r#""fadeTo":"right""#), "{json}");
        let back: SlicerSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(back, spec);
    }
}
