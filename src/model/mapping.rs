//! Per-output mapping telemetry: whether the chain from the canvas to one
//! output is verified to scale by a whole number, 1:1 included.
//!
//! The verdict is a fact about Suede's own chain, derived from three kinds of
//! input, each recorded when it changes and never on a statistics interval:
//!
//! - what the slicer installed for each output ([`SlicerMappingFacts`],
//!   reported with its `applied` control event, or once at startup by the
//!   CPU renderer, which has no live controller);
//! - how it obtains the canvas ([`CaptureFacts`], reported when a capture is
//!   (re)established);
//! - what the daemon knows that the slicer does not ([`MappingContext`]: the
//!   compositor's scale and transform per output, whether sway tiles the
//!   outputs itself, and the active app), plus the browser scale factor each
//!   app was launched with.
//!
//! [`MappingInputs::derive`] turns those into one [`OutputMapping`] per
//! output. It is pure, so the verdict and every `unverified` string are
//! testable without a slicer.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::{SamplingMode, Transform};

/// How close a value must be to a whole number, or to 1, to count as one.
const TOLERANCE: f64 = 1e-9;

/// Who applies an output's sway transform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum TransformOwner {
    /// The slicer composes it into its own raster (GPU renderer at output
    /// scale 1), so its exact copy permutes texels instead of resampling.
    Slicer,
    /// The compositor rotates or mirrors a logical buffer itself: the CPU
    /// renderer, a scaled output, and the tiled path. Suede cannot read back
    /// that blit.
    Compositor,
}

/// What the slicer installed for one output, as it reports it.
///
/// Describes the mapping the renderer actually performs, which on the CPU
/// renderer is not always the configured one: CPU content is a byte copy of
/// the whole-pixel slice rectangle (`cropHonored: false` when the configured
/// crop differs from it).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlicerMappingFacts {
    /// Output pixels per canvas pixel, horizontally, in the output's
    /// as-viewed orientation.
    pub ratio_x: f64,
    /// Output pixels per canvas pixel, vertically.
    pub ratio_y: f64,
    /// The source rectangle starts on a whole canvas pixel.
    pub whole_pixel_origin: bool,
    /// No corner-pin or center correction.
    pub identity_geometry: bool,
    /// The sampling path actually taken.
    pub sampling_path: SamplingMode,
    /// Whether this renderer composes output transforms into its own raster
    /// at all (the GPU renderer does; the CPU renderer never does).
    pub owns_transforms: bool,
    /// The transform the slicer applies itself for this output; `normal`
    /// when it applies none (a scaled output, the CPU renderer, or an output
    /// without a transform).
    pub owned_transform: Transform,
    /// Whether the renderer realizes the configured source rectangle.
    pub crop_honored: bool,
}

/// How the slicer obtains the canvas it maps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum CaptureKind {
    /// wlr-screencopy into a GPU-imported dmabuf (GPU renderer).
    Dmabuf,
    /// wlr-screencopy into shared memory (CPU renderer).
    Shm,
    /// An offscreen producer's shared Vulkan image.
    Shared,
    /// Nothing is captured: the slicer draws a calibration pattern itself,
    /// so no browser is in the chain.
    Pattern,
}

/// The capture the running slicer established, reported each time it is
/// (re)established with different facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CaptureFacts {
    pub kind: CaptureKind,
    /// Pixel format: a DRM fourcc such as `XR24` for a screencopy capture,
    /// the Vulkan format name for a shared image. Absent for a pattern.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// DRM format modifier of a dmabuf capture, as a hexadecimal string
    /// (`0x...`): modifiers do not fit a JSON number exactly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modifier: Option<String>,
    /// Whether presenting this format changes pixel values (a 10-bit
    /// source presented as 8 bits, for example). Every 8-bit RGB capture
    /// format is a lossless channel reorder and reports `false`.
    pub converted: bool,
}

/// Whether the chain from the canvas to one output is verified to scale by a
/// whole number, and every fact the verdict was made from.
///
/// Computed once whenever one of its inputs changes (a slicer apply, a
/// capture being established, an output's scale or transform, an app
/// launch), never per statistics interval. Presentation (zero-copy scanout
/// or a composited frame) is deliberately not part of it; that stays in
/// `lastInterval.outputs[].zeroCopyPresented`.
///
/// What it does **not** claim: anything inside the browser (CSS or canvas
/// scaling, video decode scaling, color management), the compositor's blit
/// when it composites the frame, the DRM color pipeline, the display's own
/// scaler and mode handling, and the pixel values Suede changes on purpose
/// (seam blending, black lift, gamma). It is about which canvas pixel lands
/// on which output pixel, and whether the capture format survives intact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct OutputMapping {
    /// Output pixels per canvas pixel, horizontally (1 at 1:1, 2 at a 2x
    /// upscale).
    pub ratio_x: f64,
    /// Output pixels per canvas pixel, vertically.
    pub ratio_y: f64,
    /// Both ratios are whole numbers of at least 1, the source origin is a
    /// whole canvas pixel, and there is no corner or center correction.
    pub whole_number: bool,
    /// The sampling path actually taken: `exact` for the integer copy, the
    /// kernel otherwise, `tiled` when sway tiles the outputs itself and no
    /// slicer runs.
    pub sampling_path: SamplingMode,
    pub transform_owner: TransformOwner,
    /// The output's sway transform, when the compositor reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transform: Option<Transform>,
    /// The compositor scale in effect on the output, when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_scale: Option<f64>,
    /// The device scale factor the active browser was launched with: `1`
    /// for a `chromium-kiosk` launch that forces it, null when unknown (any
    /// other launcher, an overriding `extraArgs` value Suede cannot parse,
    /// no launch yet) and while the slicer draws a pattern, which no browser
    /// is in the chain of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_scale_factor: Option<f64>,
    /// Every check passed: the exact copy at 1:1, or `nearest` at a whole
    /// ratio above 1, at output scale 1, from an unconverted capture, with
    /// any transform applied by the slicer, and a browser scale factor of
    /// 1 whenever a browser is in the chain.
    pub verified: bool,
    /// Each failing check, in operator language. Empty exactly when
    /// `verified`.
    #[serde(default)]
    pub unverified: Vec<String>,
    /// The desired-state (working-copy) generation these facts describe:
    /// the slicer's `configGeneration.applied`, or the reconciled generation
    /// on the tiled path. A client compares it with the working-copy
    /// generation it holds and shows nothing while the two differ. Null when
    /// the slicer's generation is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
}

/// What the compositor reports for one output.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct OutputContext {
    pub scale: Option<f64>,
    pub transform: Option<Transform>,
}

/// The daemon-side facts, set once per reconciliation pass and only acted on
/// when they differ from the previous pass's.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MappingContext {
    /// Every enabled, connected output sway reports, by name.
    pub outputs: BTreeMap<String, OutputContext>,
    /// `Some(generation)` when sway tiles the active app's window across
    /// `outputs` with no slicer (an appliance with `allow_overlaps = false`,
    /// or any layout without a canvas), for the working-copy generation that
    /// pass reconciled. Every output in `outputs` then gets the tiled
    /// derivation.
    pub tiled: Option<u64>,
    /// The app desired state activates, whose launch decides the browser
    /// scale factor.
    pub active_app: Option<String>,
}

/// Everything the verdict is derived from. Equality is what decides whether
/// a new event recomputes anything.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MappingInputs {
    /// Per output, from the current slicer session's last apply. Cleared
    /// whenever that session ends.
    pub slicer: BTreeMap<String, SlicerMappingFacts>,
    /// The current slicer session's capture. Cleared with `slicer`.
    pub capture: Option<CaptureFacts>,
    pub context: MappingContext,
    /// The browser scale factor each app was most recently launched with.
    pub launches: BTreeMap<String, Option<f64>>,
}

impl MappingInputs {
    /// The active app's browser scale factor, `None` when unknown.
    pub fn browser_scale_factor(&self) -> Option<f64> {
        self.context
            .active_app
            .as_ref()
            .and_then(|app| self.launches.get(app).copied().flatten())
    }

    /// One mapping per output. `applied_generation` is the slicer's
    /// `configGeneration.applied`; the tiled path carries its own.
    pub fn derive(&self, applied_generation: Option<u64>) -> BTreeMap<String, OutputMapping> {
        let browser = self.browser_scale_factor();
        if let Some(generation) = self.context.tiled {
            return self
                .context
                .outputs
                .iter()
                .map(|(name, output)| {
                    (
                        name.clone(),
                        tiled_mapping(output, browser, Some(generation)),
                    )
                })
                .collect();
        }
        self.slicer
            .iter()
            .map(|(name, facts)| {
                (
                    name.clone(),
                    slicer_mapping(
                        facts,
                        self.capture.as_ref(),
                        self.context.outputs.get(name),
                        browser,
                        applied_generation,
                    ),
                )
            })
            .collect()
    }
}

fn near(value: f64, target: f64) -> bool {
    (value - target).abs() <= TOLERANCE
}

/// A whole number of output pixels per canvas pixel: 1, 2, 3, ...
fn whole_ratio(ratio: f64) -> bool {
    ratio.is_finite() && ratio >= 1.0 - TOLERANCE && near(ratio, ratio.round())
}

/// A number as an operator reads it: `2`, `1.5`, `0.333`.
fn number(value: f64) -> String {
    if near(value, value.round()) {
        return format!("{}", value.round());
    }
    let text = format!("{value:.3}");
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// `2×`, or `2× by 3×` when the axes differ.
fn ratio_text(x: f64, y: f64) -> String {
    if near(x, y) {
        format!("{}×", number(x))
    } else {
        format!("{}× by {}×", number(x), number(y))
    }
}

fn sampling_name(mode: SamplingMode) -> &'static str {
    match mode {
        SamplingMode::Exact => "exact",
        SamplingMode::Nearest => "nearest",
        SamplingMode::Bilinear => "bilinear",
        SamplingMode::Bicubic => "bicubic",
        SamplingMode::Lanczos => "lanczos",
        SamplingMode::Easu => "easu",
        SamplingMode::Area => "area",
        SamplingMode::Tiled => "tiled",
    }
}

/// The output-scale, transform and browser checks every path shares.
/// `browser_in_chain` is false while the slicer draws a pattern.
fn shared_checks(
    output: Option<&OutputContext>,
    owner: TransformOwner,
    browser: Option<f64>,
    browser_in_chain: bool,
    unverified: &mut Vec<String>,
) {
    match output.and_then(|output| output.scale) {
        None => unverified.push("output scale unknown".to_string()),
        Some(scale) if !near(scale, 1.0) => {
            unverified.push(format!("output scale {}", number(scale)))
        }
        Some(_) => {}
    }
    match output.and_then(|output| output.transform) {
        None => unverified.push("output transform unknown".to_string()),
        Some(transform) if !transform.is_normal() && owner == TransformOwner::Compositor => {
            unverified.push(
                if transform.is_mirror() {
                    "flipped by the compositor"
                } else {
                    "rotated by the compositor"
                }
                .to_string(),
            )
        }
        Some(_) => {}
    }
    if browser_in_chain {
        match browser {
            None => unverified.push("browser scale factor unknown".to_string()),
            Some(factor) if !near(factor, 1.0) => {
                unverified.push(format!("browser scale factor {}", number(factor)))
            }
            Some(_) => {}
        }
    }
}

/// The verdict for one slicer output.
pub fn slicer_mapping(
    facts: &SlicerMappingFacts,
    capture: Option<&CaptureFacts>,
    output: Option<&OutputContext>,
    browser: Option<f64>,
    generation: Option<u64>,
) -> OutputMapping {
    let mut unverified = Vec::new();
    let (x, y) = (facts.ratio_x, facts.ratio_y);
    let whole_ratios = whole_ratio(x) && whole_ratio(y);
    if !whole_ratios {
        unverified.push(format!(
            "scaling {} is not a whole number",
            ratio_text(x, y)
        ));
    }
    // Unreachable from a valid document since slice units: the planner
    // always hands the slicer whole-pixel crops (`effective_slice_px`).
    // Kept so a slicer that ever reports otherwise is not called verified.
    if !facts.whole_pixel_origin {
        unverified.push("fractional crop origin".to_string());
    }
    if !facts.identity_geometry {
        unverified.push("corner or center correction".to_string());
    }
    let whole_number = whole_ratios && facts.whole_pixel_origin && facts.identity_geometry;
    // The sampling check only means something once the geometry is whole:
    // a fractional mapping already says why it is not verified.
    if whole_number {
        let unit = near(x, 1.0) && near(y, 1.0);
        let sampling = facts.sampling_path;
        if unit && sampling != SamplingMode::Exact {
            unverified.push(format!(
                "{} sampling at 1× instead of the exact copy",
                sampling_name(sampling)
            ));
        } else if !unit && sampling != SamplingMode::Nearest {
            unverified.push(format!(
                "{} filter at {} (choose nearest for pixel replication)",
                sampling_name(sampling),
                ratio_text(x, y)
            ));
        }
    }
    if !facts.crop_honored {
        unverified.push(
            "the CPU renderer copies the whole-pixel slice, not the configured crop".to_string(),
        );
    }
    match capture {
        None => unverified.push("capture format unknown".to_string()),
        Some(capture) if capture.converted => {
            unverified.push("capture format converted".to_string())
        }
        Some(_) => {}
    }
    let transform = output.and_then(|output| output.transform);
    let owner = match transform {
        Some(transform) if facts.owns_transforms && facts.owned_transform == transform => {
            TransformOwner::Slicer
        }
        None if facts.owns_transforms && !facts.owned_transform.is_normal() => {
            TransformOwner::Slicer
        }
        _ => TransformOwner::Compositor,
    };
    let browser_in_chain = capture.is_none_or(|capture| capture.kind != CaptureKind::Pattern);
    shared_checks(output, owner, browser, browser_in_chain, &mut unverified);
    OutputMapping {
        ratio_x: x,
        ratio_y: y,
        whole_number,
        sampling_path: facts.sampling_path,
        transform_owner: owner,
        transform,
        output_scale: output.and_then(|output| output.scale),
        browser_scale_factor: browser.filter(|_| browser_in_chain),
        verified: unverified.is_empty(),
        unverified,
        generation,
    }
}

/// The verdict for one output sway tiles itself, with no slicer: one texel
/// per pixel by construction at output scale 1 and sway's integer
/// positions, so only the scale, the transform (applied by the compositor)
/// and the browser are left to check.
pub fn tiled_mapping(
    output: &OutputContext,
    browser: Option<f64>,
    generation: Option<u64>,
) -> OutputMapping {
    let mut unverified = Vec::new();
    shared_checks(
        Some(output),
        TransformOwner::Compositor,
        browser,
        true,
        &mut unverified,
    );
    OutputMapping {
        ratio_x: 1.0,
        ratio_y: 1.0,
        whole_number: true,
        sampling_path: SamplingMode::Tiled,
        transform_owner: TransformOwner::Compositor,
        transform: output.transform,
        output_scale: output.scale,
        browser_scale_factor: browser,
        verified: unverified.is_empty(),
        unverified,
        generation,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exact_facts() -> SlicerMappingFacts {
        SlicerMappingFacts {
            ratio_x: 1.0,
            ratio_y: 1.0,
            whole_pixel_origin: true,
            identity_geometry: true,
            sampling_path: SamplingMode::Exact,
            owns_transforms: true,
            owned_transform: Transform::Normal,
            crop_honored: true,
        }
    }

    fn dmabuf() -> CaptureFacts {
        CaptureFacts {
            kind: CaptureKind::Dmabuf,
            format: Some("XR24".into()),
            modifier: Some("0x0".into()),
            converted: false,
        }
    }

    fn unit_output() -> OutputContext {
        OutputContext {
            scale: Some(1.0),
            transform: Some(Transform::Normal),
        }
    }

    fn verdict(
        facts: &SlicerMappingFacts,
        capture: Option<&CaptureFacts>,
        output: Option<&OutputContext>,
        browser: Option<f64>,
    ) -> OutputMapping {
        slicer_mapping(facts, capture, output, browser, Some(7))
    }

    #[test]
    fn the_exact_copy_at_one_to_one_is_verified() {
        let mapping = verdict(
            &exact_facts(),
            Some(&dmabuf()),
            Some(&unit_output()),
            Some(1.0),
        );
        assert!(mapping.verified, "{:?}", mapping.unverified);
        assert!(mapping.unverified.is_empty());
        assert!(mapping.whole_number);
        assert_eq!(mapping.transform_owner, TransformOwner::Slicer);
        assert_eq!(mapping.generation, Some(7));
    }

    #[test]
    fn nearest_at_a_whole_ratio_above_one_is_verified() {
        let facts = SlicerMappingFacts {
            ratio_x: 2.0,
            ratio_y: 2.0,
            sampling_path: SamplingMode::Nearest,
            ..exact_facts()
        };
        let mapping = verdict(&facts, Some(&dmabuf()), Some(&unit_output()), Some(1.0));
        assert!(mapping.verified, "{:?}", mapping.unverified);
    }

    /// Each clause failing on its own names exactly itself.
    #[test]
    fn each_failing_clause_is_named_in_isolation() {
        let capture = dmabuf();
        let output = unit_output();
        let cases: Vec<(
            SlicerMappingFacts,
            CaptureFacts,
            OutputContext,
            Option<f64>,
            &str,
        )> = vec![
            (
                SlicerMappingFacts {
                    ratio_x: 1.5,
                    ratio_y: 1.5,
                    sampling_path: SamplingMode::Nearest,
                    ..exact_facts()
                },
                capture.clone(),
                output,
                Some(1.0),
                "scaling 1.5× is not a whole number",
            ),
            (
                SlicerMappingFacts {
                    ratio_x: 2.0,
                    ratio_y: 0.5,
                    sampling_path: SamplingMode::Nearest,
                    ..exact_facts()
                },
                capture.clone(),
                output,
                Some(1.0),
                "scaling 2× by 0.5× is not a whole number",
            ),
            (
                SlicerMappingFacts {
                    whole_pixel_origin: false,
                    sampling_path: SamplingMode::Exact,
                    ..exact_facts()
                },
                capture.clone(),
                output,
                Some(1.0),
                "fractional crop origin",
            ),
            (
                SlicerMappingFacts {
                    identity_geometry: false,
                    ..exact_facts()
                },
                capture.clone(),
                output,
                Some(1.0),
                "corner or center correction",
            ),
            (
                SlicerMappingFacts {
                    ratio_x: 2.0,
                    ratio_y: 2.0,
                    sampling_path: SamplingMode::Bilinear,
                    ..exact_facts()
                },
                capture.clone(),
                output,
                Some(1.0),
                "bilinear filter at 2× (choose nearest for pixel replication)",
            ),
            (
                SlicerMappingFacts {
                    sampling_path: SamplingMode::Bilinear,
                    ..exact_facts()
                },
                capture.clone(),
                output,
                Some(1.0),
                "bilinear sampling at 1× instead of the exact copy",
            ),
            (
                SlicerMappingFacts {
                    crop_honored: false,
                    ..exact_facts()
                },
                capture.clone(),
                output,
                Some(1.0),
                "the CPU renderer copies the whole-pixel slice, not the configured crop",
            ),
            (
                exact_facts(),
                CaptureFacts {
                    converted: true,
                    ..capture.clone()
                },
                output,
                Some(1.0),
                "capture format converted",
            ),
            (
                exact_facts(),
                capture.clone(),
                OutputContext {
                    scale: Some(1.5),
                    ..output
                },
                Some(1.0),
                "output scale 1.5",
            ),
            (
                SlicerMappingFacts {
                    owns_transforms: false,
                    ..exact_facts()
                },
                capture.clone(),
                OutputContext {
                    transform: Some(Transform::Rotate180),
                    ..output
                },
                Some(1.0),
                "rotated by the compositor",
            ),
            (
                SlicerMappingFacts {
                    owns_transforms: false,
                    ..exact_facts()
                },
                capture.clone(),
                OutputContext {
                    transform: Some(Transform::Flipped),
                    ..output
                },
                Some(1.0),
                "flipped by the compositor",
            ),
            (
                exact_facts(),
                capture.clone(),
                output,
                None,
                "browser scale factor unknown",
            ),
            (
                exact_facts(),
                capture.clone(),
                output,
                Some(2.0),
                "browser scale factor 2",
            ),
            (
                exact_facts(),
                capture.clone(),
                OutputContext {
                    scale: None,
                    ..output
                },
                Some(1.0),
                "output scale unknown",
            ),
            (
                exact_facts(),
                capture.clone(),
                OutputContext {
                    transform: None,
                    ..output
                },
                Some(1.0),
                "output transform unknown",
            ),
        ];
        for (facts, capture, output, browser, expected) in cases {
            let mapping = verdict(&facts, Some(&capture), Some(&output), browser);
            assert!(!mapping.verified, "{expected}");
            assert_eq!(mapping.unverified, vec![expected.to_string()]);
        }
        let mapping = verdict(&exact_facts(), None, Some(&output), Some(1.0));
        assert_eq!(
            mapping.unverified,
            vec!["capture format unknown".to_string()]
        );
    }

    #[test]
    fn a_rotation_the_slicer_owns_is_verified() {
        let facts = SlicerMappingFacts {
            owned_transform: Transform::Rotate180,
            ..exact_facts()
        };
        let output = OutputContext {
            transform: Some(Transform::Rotate180),
            ..unit_output()
        };
        let mapping = verdict(&facts, Some(&dmabuf()), Some(&output), Some(1.0));
        assert!(mapping.verified, "{:?}", mapping.unverified);
        assert_eq!(mapping.transform_owner, TransformOwner::Slicer);
        assert_eq!(mapping.transform, Some(Transform::Rotate180));
    }

    /// A GPU slicer on a scaled output leaves the rotation to the
    /// compositor: the spec carries `normal`, sway reports 180.
    #[test]
    fn a_scaled_rotated_output_names_both_the_scale_and_the_compositor() {
        let output = OutputContext {
            scale: Some(2.0),
            transform: Some(Transform::Rotate180),
        };
        let mapping = verdict(&exact_facts(), Some(&dmabuf()), Some(&output), Some(1.0));
        assert_eq!(mapping.transform_owner, TransformOwner::Compositor);
        assert_eq!(
            mapping.unverified,
            vec![
                "output scale 2".to_string(),
                "rotated by the compositor".to_string()
            ]
        );
    }

    #[test]
    fn a_pattern_has_no_browser_in_the_chain() {
        let pattern = CaptureFacts {
            kind: CaptureKind::Pattern,
            format: None,
            modifier: None,
            converted: false,
        };
        let mapping = verdict(&exact_facts(), Some(&pattern), Some(&unit_output()), None);
        assert!(mapping.verified, "{:?}", mapping.unverified);
        assert_eq!(mapping.browser_scale_factor, None);
    }

    #[test]
    fn the_tiled_derivation_checks_scale_transform_and_browser() {
        let mapping = tiled_mapping(&unit_output(), Some(1.0), Some(3));
        assert!(mapping.verified);
        assert_eq!(mapping.sampling_path, SamplingMode::Tiled);
        assert_eq!(mapping.transform_owner, TransformOwner::Compositor);
        assert_eq!(mapping.generation, Some(3));

        let scaled = OutputContext {
            scale: Some(1.25),
            ..unit_output()
        };
        assert_eq!(
            tiled_mapping(&scaled, Some(1.0), None).unverified,
            vec!["output scale 1.25".to_string()]
        );
        let rotated = OutputContext {
            transform: Some(Transform::Rotate90),
            ..unit_output()
        };
        assert_eq!(
            tiled_mapping(&rotated, Some(1.0), None).unverified,
            vec!["rotated by the compositor".to_string()]
        );
        assert_eq!(
            tiled_mapping(&unit_output(), None, None).unverified,
            vec!["browser scale factor unknown".to_string()]
        );
    }

    #[test]
    fn derive_uses_the_tiled_path_only_when_sway_tiles() {
        let mut inputs = MappingInputs::default();
        inputs.context.outputs.insert("DP-1".into(), unit_output());
        inputs.context.active_app = Some("show".into());
        inputs.launches.insert("show".into(), Some(1.0));
        assert!(
            inputs.derive(Some(4)).is_empty(),
            "no slicer facts, not tiled"
        );

        inputs.slicer.insert("DP-1".into(), exact_facts());
        inputs.capture = Some(dmabuf());
        let derived = inputs.derive(Some(4));
        assert!(derived["DP-1"].verified);
        assert_eq!(derived["DP-1"].generation, Some(4));

        inputs.slicer.clear();
        inputs.capture = None;
        inputs.context.tiled = Some(9);
        let derived = inputs.derive(Some(4));
        assert_eq!(derived["DP-1"].sampling_path, SamplingMode::Tiled);
        assert_eq!(derived["DP-1"].generation, Some(9));
    }

    #[test]
    fn the_browser_factor_follows_the_active_app() {
        let mut inputs = MappingInputs::default();
        inputs.launches.insert("a".into(), Some(1.0));
        inputs.launches.insert("b".into(), None);
        assert_eq!(inputs.browser_scale_factor(), None);
        inputs.context.active_app = Some("a".into());
        assert_eq!(inputs.browser_scale_factor(), Some(1.0));
        inputs.context.active_app = Some("b".into());
        assert_eq!(inputs.browser_scale_factor(), None);
    }

    #[test]
    fn numbers_read_as_an_operator_would_write_them() {
        assert_eq!(number(2.0), "2");
        assert_eq!(number(1.5), "1.5");
        assert_eq!(number(1.0 / 3.0), "0.333");
        assert_eq!(ratio_text(2.0, 2.0), "2×");
        assert_eq!(ratio_text(2.0, 3.0), "2× by 3×");
    }
}
