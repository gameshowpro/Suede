//! The resampling kernels the GPU blend shader applies to an output that
//! does not take the exact copy, and the per-output choice between them.
//!
//! An output that maps its canvas pixels 1:1 (the integer copy in
//! `blend.frag`, [`super::warp_update::SamplingPath::Exact`]) ignores all of
//! this. Every other output samples through its warp with a [`Kernel`]:
//! the installation's configured [`ResampleFilter`], unless an axis
//! minifies by more than [`AREA_THRESHOLD`], in which case the box (area)
//! filter replaces it automatically. The canvas is an imported dmabuf, so
//! mipmaps are not available; a box of `texelFetch`es is the minification
//! filter instead.
//!
//! The kernels, as the shaders implement them (`blend.frag` and
//! `blend_markers.frag`, which [`Kernel::flags`] selects between):
//!
//! - `bilinear`: one `textureLod` through the LINEAR sampler, unchanged
//!   from before filters were selectable.
//! - `nearest`: one `texelFetch` of the texel holding the same source
//!   position bilinear samples at.
//! - `bicubic`: Catmull-Rom (`a = -0.5`) over the 4×4 texels around the
//!   source position, read with `texelFetch`. (Four or nine LINEAR taps can
//!   express a cubic B-spline, but not Catmull-Rom's negative lobes without
//!   the hardware's coarse sub-texel weight quantization; sixteen fetches
//!   keep the weights in full `f32`.)
//! - `lanczos`: Lanczos-3 over the 6×6 texels around the source position,
//!   weights normalized to sum to one.
//! - `area`: the integer average of a [`AreaBox`] of texels centered on the
//!   source position, rounded to nearest. Chosen automatically, never
//!   configured.
//!
//! Bicubic and Lanczos results are clamped to `0..=255` before rounding
//! (both kernels overshoot at hard edges). Every kernel but bilinear reads
//! only texels that intersect the output's source rectangle, clamping a
//! tap that falls outside it to the nearest edge texel, as `texel_bounds`
//! describes. Rounding back to a byte and everything after it (the
//! transfer table, coverage, black lift) are unchanged: the tables are
//! destination-side and never see the kernel.
//!
//! `easu` is accepted by the configuration but not implemented by the
//! shaders yet; an output configured with it samples, and reports,
//! bilinear, with a note saying so ([`note`]).

use super::warp::Warp;
use crate::model::{ResampleFilter, SamplingMode};

/// An axis whose minification (canvas pixels per output pixel) exceeds this
/// switches the whole output to the area filter.
pub const AREA_THRESHOLD: f64 = 2.0;

/// The largest box the area filter averages per axis. Minification past
/// this skips texels, and the output's sampling note says so.
pub const AREA_MAX_TAPS: u8 = 8;

/// How far a ratio may sit from a whole number and still count as it, so
/// a 2× minification computed through a homography never reads as 2.0000001.
const RATIO_EPSILON: f64 = 1.0e-6;

/// The kernel `blend.frag` resamples a non-exact output with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kernel {
    Bilinear,
    Nearest,
    Bicubic,
    Lanczos,
    Area(AreaBox),
}

/// The texel box the area filter averages, per canvas axis.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AreaBox {
    /// Texels averaged along canvas x, `1..=AREA_MAX_TAPS`.
    pub width: u8,
    /// Texels averaged along canvas y, `1..=AREA_MAX_TAPS`.
    pub height: u8,
    /// Whether either axis wanted more than [`AREA_MAX_TAPS`].
    pub capped: bool,
}

/// `flags` bits 4..7: the kernel code. Must match the `FILTER_*` constants
/// in both blend shaders; `shader_filter_codes_match_the_kernel_flags`
/// holds them to it.
const KERNEL_SHIFT: u32 = 4;
/// `flags` bits 8..11 and 12..15: the area box's width and height, minus
/// one. Zero for every other kernel.
const AREA_WIDTH_SHIFT: u32 = 8;
const AREA_HEIGHT_SHIFT: u32 = 12;

/// Every `flags` bit [`Kernel::flags`] may set.
pub const KERNEL_FLAGS_MASK: u32 = 0xfff0;

pub(crate) const CODE_BILINEAR: u32 = 0;
pub(crate) const CODE_NEAREST: u32 = 1;
pub(crate) const CODE_BICUBIC: u32 = 2;
pub(crate) const CODE_LANCZOS: u32 = 3;
pub(crate) const CODE_AREA: u32 = 4;

impl Kernel {
    /// The kernel for an output configured with `filter` whose source
    /// minifies by `minification` (canvas pixels per output pixel, per
    /// canvas axis; see [`minification`]): the area filter past
    /// [`AREA_THRESHOLD`] on either axis, otherwise the configured filter.
    /// `easu` is not implemented by the shaders and falls back to bilinear.
    pub fn select(filter: ResampleFilter, minification: [f64; 2]) -> Self {
        if let Some(area) = area_box(minification) {
            return Self::Area(area);
        }
        match filter {
            ResampleFilter::Bilinear | ResampleFilter::Easu => Self::Bilinear,
            ResampleFilter::Nearest => Self::Nearest,
            ResampleFilter::Bicubic => Self::Bicubic,
            ResampleFilter::Lanczos => Self::Lanczos,
        }
    }

    /// What the control status reports for this kernel.
    pub fn mode(self) -> SamplingMode {
        match self {
            Self::Bilinear => SamplingMode::Bilinear,
            Self::Nearest => SamplingMode::Nearest,
            Self::Bicubic => SamplingMode::Bicubic,
            Self::Lanczos => SamplingMode::Lanczos,
            Self::Area(_) => SamplingMode::Area,
        }
    }

    /// The bits of the shaders' `pc.flags` that select this kernel: the code
    /// in bits 4..7 and, for the area filter, its box in bits 8..15. Only
    /// meaningful on the warp path; the exact copy ignores them. Bilinear
    /// is all zeros, so a push-constant block that never heard of filters
    /// samples exactly as before.
    pub fn flags(self) -> u32 {
        let code = match self {
            Self::Bilinear => CODE_BILINEAR,
            Self::Nearest => CODE_NEAREST,
            Self::Bicubic => CODE_BICUBIC,
            Self::Lanczos => CODE_LANCZOS,
            Self::Area(_) => CODE_AREA,
        };
        let area = match self {
            Self::Area(area) => {
                (u32::from(area.width - 1) << AREA_WIDTH_SHIFT)
                    | (u32::from(area.height - 1) << AREA_HEIGHT_SHIFT)
            }
            _ => 0,
        };
        (code << KERNEL_SHIFT) | area
    }
}

/// The area filter's box for an output minifying by `minification`, or
/// `None` when neither axis minifies by more than [`AREA_THRESHOLD`]. Each
/// axis averages `ceil(m)` texels (at least one, at most
/// [`AREA_MAX_TAPS`]); an axis that does not minify past the threshold
/// still gets its own `ceil(m)`, so a 2.5× by 1.5× output averages 3×2.
pub fn area_box(minification: [f64; 2]) -> Option<AreaBox> {
    if !minification
        .iter()
        .any(|&m| m > AREA_THRESHOLD + RATIO_EPSILON)
    {
        return None;
    }
    let taps = |m: f64| {
        // NaN becomes one tap: `f64::max` returns the non-NaN operand.
        let wanted = (m - RATIO_EPSILON).ceil().max(1.0);
        (
            wanted.min(f64::from(AREA_MAX_TAPS)) as u8,
            wanted > f64::from(AREA_MAX_TAPS),
        )
    };
    let (width, capped_x) = taps(minification[0]);
    let (height, capped_y) = taps(minification[1]);
    Some(AreaBox {
        width,
        height,
        capped: capped_x || capped_y,
    })
}

/// The largest minification `warp` asks of its source on each canvas axis:
/// canvas pixels per output pixel, `[x, y]`, along the canvas axes (so an
/// output rotated by 90 still reports canvas x first).
///
/// Measured as the length of the gradient of each canvas coordinate with
/// respect to output pixels, the scale factor along that canvas axis
/// whatever the warp's rotation, and maximized over the part of the output
/// the warp actually covers: a grid of output pixel centers inside the
/// destination quad, plus a grid of points on the quad itself (which
/// catches a quad smaller than one grid cell). An identity warp with a
/// source rectangle gives exactly `source_rect.zw / logical size`.
pub fn minification(warp: &Warp) -> [f64; 2] {
    let [width, height] = warp.output_size().map(f64::from);
    let covered = |p: [f64; 2]| {
        (0.0..=width).contains(&p[0])
            && (0.0..=height).contains(&p[1])
            && warp.source_at(p[0], p[1]).is_some_and(|s| {
                (-RATIO_EPSILON..=width + RATIO_EPSILON).contains(&s[0])
                    && (-RATIO_EPSILON..=height + RATIO_EPSILON).contains(&s[1])
            })
    };
    let lerp = |a: f64, b: f64, i: usize, n: usize| a + (b - a) * i as f64 / (n - 1) as f64;
    let mut points: Vec<[f64; 2]> = Vec::new();
    for j in 0..9 {
        for i in 0..9 {
            points.push([
                lerp(0.5, (width - 0.5).max(0.5), i, 9),
                lerp(0.5, (height - 0.5).max(0.5), j, 9),
            ]);
        }
    }
    for j in 0..5 {
        for i in 0..5 {
            if let Some(p) = warp.destination_at(lerp(0.01, 0.99, i, 5), lerp(0.01, 0.99, j, 5)) {
                points.push(p);
            }
        }
    }
    let gradient = |p: [f64; 2]| -> Option<[f64; 2]> {
        const H: f64 = 0.25;
        let at = |x: f64, y: f64| warp.canvas_at(x, y, [0.0, 0.0]);
        let (x0, x1) = (at(p[0] - H, p[1])?, at(p[0] + H, p[1])?);
        let (y0, y1) = (at(p[0], p[1] - H)?, at(p[0], p[1] + H)?);
        let axis = |a: usize| ((x1[a] - x0[a]) / (2.0 * H)).hypot((y1[a] - y0[a]) / (2.0 * H));
        let m = [axis(0), axis(1)];
        m.iter().all(|v| v.is_finite()).then_some(m)
    };
    let fold = |points: &mut dyn Iterator<Item = [f64; 2]>| {
        points
            .filter_map(gradient)
            .fold(None, |best: Option<[f64; 2]>, m| {
                Some(best.map_or(m, |b| [b[0].max(m[0]), b[1].max(m[1])]))
            })
    };
    fold(&mut points.iter().copied().filter(|&p| covered(p)))
        .or_else(|| fold(&mut points.iter().copied()))
        .unwrap_or([1.0, 1.0])
}

/// What an output's sampling report adds to its `samplingMode`, if
/// anything: that the area box was capped, or that `easu` was configured
/// but the shader sampled bilinear.
pub fn note(filter: ResampleFilter, kernel: Kernel) -> Option<String> {
    match kernel {
        Kernel::Area(area) if area.capped => Some(format!(
            "area filter capped at {AREA_MAX_TAPS}x{AREA_MAX_TAPS} texels; \
             minification past {AREA_MAX_TAPS}x skips texels"
        )),
        Kernel::Bilinear if filter == ResampleFilter::Easu => {
            Some("easu is not implemented yet; this output samples bilinear".into())
        }
        _ => None,
    }
}

/// The kernel the CPU renderer actually runs for `kernel`: `nearest`,
/// `bilinear` and `bicubic` are implemented there; `lanczos` and the
/// automatic `area` filter are GPU-only and fall back to bilinear.
pub fn cpu_kernel(kernel: Kernel) -> Kernel {
    match kernel {
        Kernel::Lanczos | Kernel::Area(_) => Kernel::Bilinear,
        other => other,
    }
}

/// What the CPU renderer reports for an output the GPU would resample with
/// `kernel` (configured as `filter`): the kernel it actually runs
/// ([`cpu_kernel`]) and, when that is not the one asked for, a note in the
/// same register as [`note`]'s `easu` fallback, so the daemon does not
/// pretend parity it does not have.
pub fn cpu_sampling(filter: ResampleFilter, kernel: Kernel) -> (SamplingMode, Option<String>) {
    let mode = cpu_kernel(kernel).mode();
    let note = match kernel {
        Kernel::Area(_) => Some(
            "area filtering (automatic past 2x minification) is GPU-only; \
             this output samples bilinear on the CPU renderer"
                .to_string(),
        ),
        _ => match filter {
            ResampleFilter::Lanczos => Some("lanczos"),
            ResampleFilter::Easu => Some("easu"),
            _ => None,
        }
        .map(|name| {
            format!("{name} is GPU-only; this output samples bilinear on the CPU renderer")
        }),
    };
    (mode, note)
}

/// The inclusive texel index range, `[[x0, y0], [x1, y1]]`, every kernel
/// but bilinear clamps its taps to: the texels that intersect the source
/// rectangle `rect` (canvas pixel-boundary x, y, width, height), inside a
/// `canvas`-sized texture. Unflipped canvas coordinates, before capture y
/// inversion; the shaders compute the same range from `pc.source_rect`.
#[cfg(test)]
pub(crate) fn texel_bounds(rect: [f64; 4], canvas: [u32; 2]) -> [[i64; 2]; 2] {
    let mut bounds = [[0; 2]; 2];
    for axis in 0..2 {
        let last = i64::from(canvas[axis]) - 1;
        let lo = (rect[axis].floor() as i64).clamp(0, last);
        let hi = ((rect[axis] + rect[axis + 2]).ceil() as i64 - 1).clamp(lo, last);
        bounds[0][axis] = lo;
        bounds[1][axis] = hi;
    }
    bounds
}

/// A CPU reference for one output pixel of `kernel`, in `f64`, for tests
/// that hold the shaders to the definitions in the module doc. `source` is
/// the clamped, unflipped canvas position the shader computes (the warp's
/// [`Warp::clamped_canvas_at`]); `texel` reads the canvas at an unflipped
/// texel index inside `bounds` (see [`texel_bounds`]), so it is where a
/// test applies capture y inversion.
///
/// Bilinear here clamps its two taps per axis to `bounds` too, where the
/// hardware clamps only to the texture; the two agree whenever the source
/// rectangle is whole-pixel, because the clamped positions then give an
/// outside tap zero weight.
#[cfg(test)]
pub(crate) fn reference_sample(
    kernel: Kernel,
    texel: &dyn Fn([i64; 2]) -> [u8; 3],
    source: [f64; 2],
    bounds: [[i64; 2]; 2],
) -> [u8; 3] {
    let fetch = |i: [i64; 2]| {
        texel([
            i[0].clamp(bounds[0][0], bounds[1][0]),
            i[1].clamp(bounds[0][1], bounds[1][1]),
        ])
    };
    let separable = |first: i64, taps: i64, weight: &dyn Fn(f64) -> f64| {
        let f = [source[0] - 0.5, source[1] - 0.5];
        let base = [f[0].floor(), f[1].floor()];
        let t = [f[0] - base[0], f[1] - base[1]];
        let base = [base[0] as i64, base[1] as i64];
        let w = |axis: usize, k: i64| weight(k as f64 - t[axis]);
        let sum = |axis: usize| (first..first + taps).map(|k| w(axis, k)).sum::<f64>();
        let norm = sum(0) * sum(1);
        let mut out = [0u8; 3];
        for (c, out) in out.iter_mut().enumerate() {
            let mut acc = 0.0;
            for j in first..first + taps {
                for i in first..first + taps {
                    acc += w(0, i) * w(1, j) * f64::from(fetch([base[0] + i, base[1] + j])[c]);
                }
            }
            *out = ((acc / norm).clamp(0.0, 255.0) + 0.5).floor() as u8;
        }
        out
    };
    match kernel {
        Kernel::Nearest => fetch([source[0].floor() as i64, source[1].floor() as i64]),
        Kernel::Bilinear => separable(0, 2, &|x| (1.0 - x.abs()).max(0.0)),
        Kernel::Bicubic => separable(-1, 4, &catmull_rom),
        Kernel::Lanczos => separable(-2, 6, &lanczos3),
        Kernel::Area(area) => {
            let n = [i64::from(area.width), i64::from(area.height)];
            let start = [
                (source[0] - n[0] as f64 * 0.5 + 0.5).floor() as i64,
                (source[1] - n[1] as f64 * 0.5 + 0.5).floor() as i64,
            ];
            let count = (n[0] * n[1]) as u32;
            let mut sum = [0u32; 3];
            for j in 0..n[1] {
                for i in 0..n[0] {
                    let t = fetch([start[0] + i, start[1] + j]);
                    for c in 0..3 {
                        sum[c] += u32::from(t[c]);
                    }
                }
            }
            sum.map(|s| ((s + count / 2) / count) as u8)
        }
    }
}

/// Catmull-Rom (`a = -0.5`) at distance `x` from a texel center. The one
/// copy of the weights: the CPU renderer's bicubic and the test reference
/// both use it.
pub(crate) fn catmull_rom(x: f64) -> f64 {
    let x = x.abs();
    if x < 1.0 {
        (1.5 * x - 2.5) * x * x + 1.0
    } else if x < 2.0 {
        ((-0.5 * x + 2.5) * x - 4.0) * x + 2.0
    } else {
        0.0
    }
}

/// Lanczos-3 at distance `x` from a texel center.
#[cfg(test)]
fn lanczos3(x: f64) -> f64 {
    let x = x.abs();
    if x < 1.0e-5 {
        1.0
    } else if x >= 3.0 {
        0.0
    } else {
        let px = std::f64::consts::PI * x;
        3.0 * px.sin() * (px / 3.0).sin() / (px * px)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Transform;

    #[test]
    fn area_kicks_in_past_two_times_minification_per_axis() {
        // Not at exactly 2x, on either axis, nor at any magnification.
        for m in [[2.0, 2.0], [2.0, 1.0], [1.0, 2.0], [1.0, 1.0], [0.5, 0.25]] {
            assert_eq!(area_box(m), None, "{m:?}");
        }
        // A homography's 2x may land a hair above 2; it still is not area.
        assert_eq!(area_box([2.0 + 1.0e-9, 2.0]), None);
        let area = |width, height, capped| {
            Some(AreaBox {
                width,
                height,
                capped,
            })
        };
        // At 2.5x, on each axis on its own and on both.
        assert_eq!(area_box([2.5, 1.0]), area(3, 1, false));
        assert_eq!(area_box([1.0, 2.5]), area(1, 3, false));
        assert_eq!(area_box([2.5, 2.5]), area(3, 3, false));
        // The other axis gets its own ceil(m), at least one tap.
        assert_eq!(area_box([2.5, 1.5]), area(3, 2, false));
        assert_eq!(area_box([2.5, 0.5]), area(3, 1, false));
        // Whole ratios are not rounded up by float noise.
        assert_eq!(area_box([3.0 + 1.0e-9, 4.0 - 1.0e-9]), area(3, 4, false));
        // Capped at 8 taps, and the cap is recorded.
        assert_eq!(area_box([8.0, 3.0]), area(8, 3, false));
        assert_eq!(area_box([11.3, 3.0]), area(8, 3, true));
        assert_eq!(area_box([3.0, f64::NAN]), area(3, 1, false));
    }

    #[test]
    fn select_honors_the_filter_unless_area_takes_over() {
        use ResampleFilter::*;
        for (filter, kernel) in [
            (Bilinear, Kernel::Bilinear),
            (Nearest, Kernel::Nearest),
            (Bicubic, Kernel::Bicubic),
            (Lanczos, Kernel::Lanczos),
            (Easu, Kernel::Bilinear),
        ] {
            assert_eq!(Kernel::select(filter, [0.5, 0.5]), kernel, "{filter:?}");
            assert_eq!(Kernel::select(filter, [2.0, 1.0]), kernel, "{filter:?}");
            assert_eq!(
                Kernel::select(filter, [2.5, 1.0]),
                Kernel::Area(AreaBox {
                    width: 3,
                    height: 1,
                    capped: false
                }),
                "{filter:?}"
            );
        }
        assert_eq!(
            Kernel::select(Nearest, [1.0, 1.0]).mode(),
            SamplingMode::Nearest
        );
        assert_eq!(
            Kernel::select(Lanczos, [3.0, 3.0]).mode(),
            SamplingMode::Area
        );
    }

    #[test]
    fn notes_name_the_cap_and_the_easu_fallback_only() {
        assert_eq!(note(ResampleFilter::Bilinear, Kernel::Bilinear), None);
        assert_eq!(note(ResampleFilter::Nearest, Kernel::Nearest), None);
        assert!(note(ResampleFilter::Easu, Kernel::Bilinear)
            .unwrap()
            .contains("easu is not implemented"));
        let capped = Kernel::select(ResampleFilter::Easu, [12.0, 1.0]);
        assert!(note(ResampleFilter::Easu, capped)
            .unwrap()
            .contains("capped at 8x8"));
        let uncapped = Kernel::select(ResampleFilter::Easu, [3.0, 1.0]);
        assert_eq!(note(ResampleFilter::Easu, uncapped), None);
    }

    /// The CPU renderer implements `nearest` and `bicubic`; the GPU-only
    /// filters report `bilinear` with a note.
    #[test]
    fn cpu_gpu_only_filters_report_bilinear_with_note() {
        let report = |filter, minification| {
            let kernel = Kernel::select(filter, minification);
            assert_eq!(
                cpu_kernel(kernel).mode(),
                cpu_sampling(filter, kernel).0,
                "{filter:?}"
            );
            cpu_sampling(filter, kernel)
        };
        let unity = [1.0, 1.0];
        assert_eq!(
            report(ResampleFilter::Bilinear, unity),
            (SamplingMode::Bilinear, None)
        );
        assert_eq!(
            report(ResampleFilter::Nearest, unity),
            (SamplingMode::Nearest, None)
        );
        assert_eq!(
            report(ResampleFilter::Bicubic, unity),
            (SamplingMode::Bicubic, None)
        );
        for (filter, name) in [
            (ResampleFilter::Lanczos, "lanczos"),
            (ResampleFilter::Easu, "easu"),
        ] {
            let (mode, note) = report(filter, unity);
            assert_eq!(mode, SamplingMode::Bilinear);
            let note = note.expect("a GPU-only filter carries a note");
            assert!(note.starts_with(&format!("{name} is GPU-only")), "{note}");
            assert!(note.contains("bilinear"), "{note}");
        }
        // The automatic box is GPU-only too, whatever filter is configured.
        for filter in [ResampleFilter::Nearest, ResampleFilter::Bicubic] {
            let (mode, note) = report(filter, [3.0, 3.0]);
            assert_eq!(mode, SamplingMode::Bilinear, "{filter:?}");
            assert!(note.unwrap().contains("area filtering"), "{filter:?}");
        }
    }

    #[test]
    fn kernel_flags_use_bits_four_to_fifteen_and_bilinear_is_zero() {
        assert_eq!(Kernel::Bilinear.flags(), 0);
        assert_eq!(Kernel::Nearest.flags(), 1 << 4);
        assert_eq!(Kernel::Bicubic.flags(), 2 << 4);
        assert_eq!(Kernel::Lanczos.flags(), 3 << 4);
        let area = Kernel::Area(AreaBox {
            width: 3,
            height: 8,
            capped: true,
        });
        assert_eq!(area.flags(), (4 << 4) | (2 << 8) | (7 << 12));
        for kernel in [Kernel::Nearest, Kernel::Bicubic, Kernel::Lanczos, area] {
            assert_eq!(kernel.flags() & !KERNEL_FLAGS_MASK, 0, "{kernel:?}");
            assert_eq!(
                kernel.flags() & 0xf,
                0,
                "{kernel:?} leaves the transform bits"
            );
        }
    }

    /// Both blend shaders decode the kernel code and the area box from the
    /// same bits [`Kernel::flags`] writes.
    #[test]
    fn shader_filter_codes_match_the_kernel_flags() {
        for shader in [
            include_str!("shaders/blend.frag"),
            include_str!("shaders/blend_markers.frag"),
        ] {
            for (name, code) in [
                ("BILINEAR", CODE_BILINEAR),
                ("NEAREST", CODE_NEAREST),
                ("BICUBIC", CODE_BICUBIC),
                ("LANCZOS", CODE_LANCZOS),
                ("AREA", CODE_AREA),
            ] {
                let line = format!("const uint FILTER_{name} = {code}u;");
                assert!(shader.contains(&line), "missing `{line}`");
            }
            assert!(shader.contains("uint resample = (pc.flags >> 4u) & 0xfu;"));
            assert!(shader.contains("uint nx = ((pc.flags >> 8u) & 0xfu) + 1u;"));
            assert!(shader.contains("uint ny = ((pc.flags >> 12u) & 0xfu) + 1u;"));
        }
    }

    fn identity(size: [u32; 2], rect: [f64; 4]) -> Warp {
        Warp::identity(size[0], size[1])
            .with_source_rect(rect)
            .unwrap()
    }

    fn close(a: [f64; 2], b: [f64; 2]) -> bool {
        (a[0] - b[0]).abs() < 1.0e-9 && (a[1] - b[1]).abs() < 1.0e-9
    }

    #[test]
    fn minification_of_a_plain_scale_is_source_over_output_per_axis() {
        for (size, rect, expected) in [
            ([17, 9], [2.0, 2.0, 42.5, 22.5], [2.5, 2.5]),
            ([17, 9], [2.0, 2.0, 42.5, 9.0], [2.5, 1.0]),
            ([34, 18], [3.0, 2.0, 17.0, 9.0], [0.5, 0.5]),
            ([1920, 1080], [0.0, 0.0, 3840.0, 2160.0], [2.0, 2.0]),
            ([1920, 1080], [0.5, 0.0, 1920.0, 1080.0], [1.0, 1.0]),
        ] {
            let m = minification(&identity(size, rect));
            assert!(close(m, expected), "{size:?} {rect:?}: {m:?}");
        }
        // Exactly 2x through the full warp math is still not area.
        let m = minification(&identity([1920, 1080], [0.0, 0.0, 3840.0, 2160.0]));
        assert_eq!(area_box(m), None);
    }

    #[test]
    fn minification_follows_canvas_axes_under_an_axis_swapping_transform() {
        // A 20x10 logical raster sampling 50x10 canvas pixels: 2.5x along
        // canvas x, 1x along canvas y, on a physical 10x20 raster.
        for transform in [Transform::Rotate90, Transform::Flipped270] {
            let warp = Warp::for_output(
                [[0.0, 0.0], [20.0, 0.0], [20.0, 10.0], [0.0, 10.0]],
                [0.5, 0.5],
                20,
                10,
                transform,
            )
            .unwrap()
            .with_source_rect([0.0, 0.0, 50.0, 10.0])
            .unwrap();
            assert_eq!(warp.output_size(), [10, 20]);
            let m = minification(&warp);
            assert!(close(m, [2.5, 1.0]), "{transform:?}: {m:?}");
        }
    }

    #[test]
    fn minification_of_a_keystone_is_its_largest_over_the_covered_quad() {
        // The top edge is squeezed to 60% of the width: a homography then
        // minifies by 1/0.6 there on both axes (perspective foreshortens
        // vertically too, plus a little from the slanted sides), and by
        // about one at the full-width bottom edge.
        let warp = Warp::new(
            [[20.0, 0.0], [80.0, 0.0], [100.0, 100.0], [0.0, 100.0]],
            [0.5, 0.5],
            100,
            100,
        )
        .unwrap()
        .with_source_rect([0.0, 0.0, 100.0, 100.0])
        .unwrap();
        let m = minification(&warp);
        let top = 100.0 / 60.0;
        assert!(
            (m[0] - top).abs() < 0.05 && (m[1] - top).abs() < 0.05,
            "{m:?}"
        );
        assert_eq!(area_box(m), None);
        let denser = Warp::new(
            [[20.0, 0.0], [80.0, 0.0], [100.0, 100.0], [0.0, 100.0]],
            [0.5, 0.5],
            100,
            100,
        )
        .unwrap()
        .with_source_rect([0.0, 0.0, 150.0, 100.0])
        .unwrap();
        let m = minification(&denser);
        assert!((m[0] - 1.5 * top).abs() < 0.08, "{m:?}");
        assert_eq!(area_box(m).map(|a| [a.width, a.height]), Some([3, 2]));
        // Pins far outside the raster do not count the uncovered part: an
        // overscanned quad at unit scale stays at about one.
        let overscan = Warp::new(
            [
                [-50.0, -50.0],
                [150.0, -50.0],
                [150.0, 150.0],
                [-50.0, 150.0],
            ],
            [0.5, 0.5],
            100,
            100,
        )
        .unwrap()
        .with_source_rect([0.0, 0.0, 200.0, 200.0])
        .unwrap();
        assert!(close(minification(&overscan), [1.0, 1.0]));
    }

    fn canvas(size: [u32; 2]) -> impl Fn([i64; 2]) -> [u8; 3] {
        move |[x, y]: [i64; 2]| {
            assert!(x >= 0 && y >= 0 && x < i64::from(size[0]) && y < i64::from(size[1]));
            [
                (x * 10 + 1) as u8,
                (y * 16 + 3) as u8,
                (x * 5 + y * 7) as u8,
            ]
        }
    }

    #[test]
    fn nearest_replicates_whole_number_upscales() {
        let size = [8, 4];
        let texel = canvas(size);
        let bounds = texel_bounds([0.0, 0.0, 8.0, 4.0], size);
        for scale in [2u32, 3] {
            let warp = identity([8 * scale, 4 * scale], [0.0, 0.0, 8.0, 4.0]);
            for y in 0..4 * scale {
                for x in 0..8 * scale {
                    let s = warp
                        .clamped_canvas_at(f64::from(x) + 0.5, f64::from(y) + 0.5, [0.0; 2])
                        .unwrap();
                    assert_eq!(
                        reference_sample(Kernel::Nearest, &texel, s, bounds),
                        texel([i64::from(x / scale), i64::from(y / scale)]),
                        "{scale}x ({x}, {y})"
                    );
                }
            }
        }
    }

    #[test]
    fn interpolating_kernels_hit_texel_centers_and_keep_flat_fields_flat() {
        let size = [12, 10];
        let texel = canvas(size);
        let bounds = texel_bounds([0.0, 0.0, 12.0, 10.0], size);
        for kernel in [Kernel::Bilinear, Kernel::Bicubic, Kernel::Lanczos] {
            for (x, y) in [(0, 0), (5, 4), (11, 9)] {
                let s = [f64::from(x) + 0.5, f64::from(y) + 0.5];
                assert_eq!(
                    reference_sample(kernel, &texel, s, bounds),
                    texel([i64::from(x), i64::from(y)]),
                    "{kernel:?} at ({x}, {y})"
                );
            }
            let flat = |_: [i64; 2]| [37, 200, 255];
            for s in [[3.1, 2.7], [6.0, 5.0], [0.5, 9.5], [7.25, 1.9]] {
                assert_eq!(
                    reference_sample(kernel, &flat, s, bounds),
                    [37, 200, 255],
                    "{kernel:?} at {s:?}"
                );
            }
        }
    }

    #[test]
    fn bicubic_reproduces_a_linear_ramp_and_lanczos_stays_close() {
        let ramp = |[x, _]: [i64; 2]| [(x * 20) as u8, 0, 0];
        let bounds = [[0, 0], [12, 4]];
        for s in [[4.1f64, 2.5], [5.75, 2.5], [6.3, 1.2]] {
            let expected = ((s[0] - 0.5) * 20.0 + 0.5).floor() as u8;
            assert_eq!(
                reference_sample(Kernel::Bicubic, &ramp, s, bounds)[0],
                expected
            );
            let lanczos = reference_sample(Kernel::Lanczos, &ramp, s, bounds)[0];
            assert!(
                lanczos.abs_diff(expected) <= 1,
                "{s:?}: {lanczos} vs {expected}"
            );
        }
    }

    #[test]
    fn overshoot_is_clamped_and_taps_stay_inside_the_source_texels() {
        // A hard 0/255 edge rings under both cubic and Lanczos.
        let edge = |[x, _]: [i64; 2]| if x < 4 { [0; 3] } else { [255; 3] };
        let bounds = [[0, 0], [7, 3]];
        for kernel in [Kernel::Bicubic, Kernel::Lanczos] {
            // Just past the edge the negative lobe overshoots above 255 on
            // the bright side and below 0 on the dark side; both clamp.
            assert_eq!(reference_sample(kernel, &edge, [4.6, 1.5], bounds)[0], 255);
            assert_eq!(reference_sample(kernel, &edge, [3.4, 1.5], bounds)[0], 0);
            // Past the widest kernel's reach of the edge, flat again.
            assert_eq!(reference_sample(kernel, &edge, [7.0, 1.5], bounds)[0], 255);
        }
        // A source rectangle of texels 2..=5: taps clamp to its edges, so the
        // outside texels (marked 255) are never read.
        let marked = |[x, _]: [i64; 2]| {
            if (2..=5).contains(&x) {
                [10; 3]
            } else {
                [255; 3]
            }
        };
        let bounds = texel_bounds([2.0, 0.0, 4.0, 4.0], [8, 4]);
        assert_eq!(bounds, [[2, 0], [5, 3]]);
        for kernel in [
            Kernel::Nearest,
            Kernel::Bicubic,
            Kernel::Lanczos,
            Kernel::select(ResampleFilter::Bilinear, [3.0, 3.0]),
        ] {
            for s in [[2.5, 0.5], [5.5, 3.5], [3.9, 2.1]] {
                assert_eq!(
                    reference_sample(kernel, &marked, s, bounds),
                    [10; 3],
                    "{kernel:?} at {s:?}"
                );
            }
        }
    }

    #[test]
    fn texel_bounds_cover_every_texel_the_rect_touches() {
        assert_eq!(
            texel_bounds([3.0, 2.0, 17.0, 9.0], [48, 28]),
            [[3, 2], [19, 10]]
        );
        assert_eq!(
            texel_bounds([10.3, 0.7, 10.4, 2.0], [48, 28]),
            [[10, 0], [20, 2]]
        );
        assert_eq!(
            texel_bounds([5.25, 3.125, 0.25, 0.375], [48, 28]),
            [[5, 3], [5, 3]]
        );
        // Clipped to the canvas on both sides.
        assert_eq!(
            texel_bounds([-3.25, -2.0, 60.0, 40.0], [48, 28]),
            [[0, 0], [47, 27]]
        );
    }

    #[test]
    fn area_averages_the_block_under_each_output_pixel() {
        let size = [12, 9];
        let texel = canvas(size);
        let bounds = texel_bounds([0.0, 0.0, 12.0, 9.0], size);
        // 3x minification: each output pixel is the rounded mean of its own
        // 3x3 block, computed here independently.
        let warp = identity([4, 3], [0.0, 0.0, 12.0, 9.0]);
        let kernel = Kernel::select(ResampleFilter::Nearest, minification(&warp));
        assert_eq!(
            kernel,
            Kernel::Area(AreaBox {
                width: 3,
                height: 3,
                capped: false
            })
        );
        for y in 0..3 {
            for x in 0..4 {
                let s = warp
                    .clamped_canvas_at(f64::from(x) + 0.5, f64::from(y) + 0.5, [0.0; 2])
                    .unwrap();
                let mut sum = [0u32; 3];
                for j in 0..3 {
                    for i in 0..3 {
                        let t = texel([i64::from(x * 3 + i), i64::from(y * 3 + j)]);
                        for c in 0..3 {
                            sum[c] += u32::from(t[c]);
                        }
                    }
                }
                let expected = sum.map(|s| (f64::from(s) / 9.0).round() as u8);
                assert_eq!(reference_sample(kernel, &texel, s, bounds), expected);
            }
        }
    }
}
