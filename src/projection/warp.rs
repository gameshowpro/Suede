//! Pure, bounded projective corner pinning.
//!
//! Coordinates in this module use a top-left origin and describe pixel
//! boundaries.  `corners` are output pixel boundaries in TL, TR, BR, BL
//! order; the source is the unit square and is remapped around `center`
//! before the homography is applied.
//! An optional source rectangle supplies canvas placement and density without
//! changing local pin geometry. Capture y inversion belongs to the caller.

// Frozen bounds for the Phase 1 pixel-coordinate mapping: positive area and
// convex turns exceed EPS * max(width, height)^2; elimination pivots exceed
// EPS times the remaining coefficient scale. Denominators use the solver's
// h22=1 normalization (and its unscaled inverse).
const EPS: f64 = 1.0e-12;
const DENOM_EPS: f64 = 1.0e-7;
const MAX_DIMENSION: u32 = 32768;
const MAX_CORNER_SCALE: f64 = 16.0;
const MAX_F32_CORNER_ERROR: f64 = 0.125; // source pixels, after center unmapping

/// Internal slicer geometry, in output pixel-boundary coordinates.
/// This is not the persisted geometry schema.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Geometry {
    pub corners: [[f64; 2]; 4],
    #[serde(default = "neutral_center")]
    pub center: [f64; 2],
}

fn neutral_center() -> [f64; 2] {
    [0.5, 0.5]
}

impl Geometry {
    pub fn warp(&self, width: u32, height: u32) -> Result<Option<Warp>, String> {
        let warp = Warp::new(self.corners, self.center, width, height)?;
        Ok((!warp.is_identity()).then_some(warp))
    }
}

#[derive(Clone, Debug)]
pub struct Warp {
    corners: [[f64; 2]; 4],
    forward: [[f64; 3]; 3],
    inverse: [[f64; 3]; 3],
    center: [f64; 2],
    output_width: u32,
    output_height: u32,
    edges: [[f64; 3]; 4],
    bounds: [f64; 4], // min x, min y, max x, max y
    identity: bool,
    source_rect: Option<[f64; 4]>,
}

impl Warp {
    /// Build a warp from output pixel-boundary corner pins.
    pub fn new(
        corners: [[f64; 2]; 4],
        center: [f64; 2],
        width: u32,
        height: u32,
    ) -> Result<Self, String> {
        if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
            return Err("warp dimensions must be in 1..=32768".into());
        }
        if !center
            .iter()
            .all(|v| v.is_finite() && (0.01..=0.99).contains(v))
        {
            return Err("warp centers must be finite and in 0.01..=0.99".into());
        }
        if !corners.iter().flatten().all(|v| v.is_finite()) {
            return Err("warp corners must be finite".into());
        }
        let limit = MAX_CORNER_SCALE * (width as f64).max(height as f64);
        if corners.iter().flatten().any(|v| v.abs() > limit) {
            return Err("warp corners exceed bounded output coordinate range".into());
        }
        let area = signed_area(&corners);
        let scale = (width as f64).max(height as f64).max(1.0);
        if area <= EPS * scale * scale {
            return Err("warp corners must form a positive-area quad".into());
        }
        for i in 0..4 {
            let a = corners[i];
            let b = corners[(i + 1) % 4];
            let c = corners[(i + 2) % 4];
            if cross(sub(b, a), sub(c, b)) <= EPS * scale * scale {
                return Err("warp corners must be strictly convex and ordered TL,TR,BR,BL".into());
            }
        }

        let remapped = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
        let forward = solve_homography(remapped, corners)?;
        // A linear denominator cannot change sign over a square unless it
        // does so at a corner.  This rejects projective poles in the source.
        check_denominator(&forward, &remapped, "source")?;
        let inverse = invert3(forward).ok_or_else(|| "warp homography is singular".to_string())?;
        if inverse
            .iter()
            .flatten()
            .any(|v| !v.is_finite() || !(*v as f32).is_finite())
        {
            return Err("warp homography cannot be represented by finite f32 rows".into());
        }
        let output_rect = [
            [0.0, 0.0],
            [width as f64, 0.0],
            [width as f64, height as f64],
            [0.0, height as f64],
        ];
        check_denominator(&inverse, &output_rect, "output")?;
        check_f32_denominator(&inverse, &output_rect)?;
        check_f32_corners(&inverse, &corners, center, width, height)?;
        let mut edges = [[0.0; 3]; 4];
        for i in 0..4 {
            let a = corners[i];
            let b = corners[(i + 1) % 4];
            edges[i] = [a[1] - b[1], b[0] - a[0], a[0] * b[1] - b[0] * a[1]];
        }
        let bounds = corners.iter().fold(
            [
                f64::INFINITY,
                f64::INFINITY,
                f64::NEG_INFINITY,
                f64::NEG_INFINITY,
            ],
            |mut r, p| {
                r[0] = r[0].min(p[0]);
                r[1] = r[1].min(p[1]);
                r[2] = r[2].max(p[0]);
                r[3] = r[3].max(p[1]);
                r
            },
        );

        let identity = center == [0.5, 0.5]
            && corners
                == [
                    [0.0, 0.0],
                    [width as f64, 0.0],
                    [width as f64, height as f64],
                    [0.0, height as f64],
                ];
        Ok(Self {
            corners,
            forward,
            inverse,
            center,
            output_width: width,
            output_height: height,
            edges,
            bounds,
            identity,
            source_rect: None,
        })
    }

    /// Set absolute canvas pixel-boundary x, y, width, and height. This does
    /// not change output-local geometry or the exact local identity flag.
    pub fn with_source_rect(mut self, rect: [f64; 4]) -> Result<Self, String> {
        let limit = MAX_CORNER_SCALE * MAX_DIMENSION as f64;
        if !rect.iter().all(|v| v.is_finite() && v.abs() <= limit)
            || rect[2] <= 0.0
            || rect[3] <= 0.0
            || rect[2] as f32 <= 0.0
            || rect[3] as f32 <= 0.0
            || (rect[0] + rect[2]).abs() > limit
            || (rect[1] + rect[3]).abs() > limit
        {
            return Err("warp source rectangle must have finite positive extents within bounded canvas coordinates".into());
        }
        // Check the actual f32 mapping, including the new scale and offset.
        // A valid local map can lose precision when sampling a larger source.
        let rows = self.inverse.map(|r| r.map(|v| v as f32));
        let expected = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
        for (p, expected) in self.corners.iter().zip(expected) {
            let q = rows.map(|r| r[0] * p[0] as f32 + r[1] * p[1] as f32 + r[2]);
            for axis in 0..2 {
                let s = q[axis] / q[2];
                let c = self.center[axis] as f32;
                let unit = if s <= c {
                    s / (2.0 * c)
                } else {
                    0.5 + (s - c) / (2.0 * (1.0 - c))
                };
                let mapped = rect[axis] as f32 + unit * rect[axis + 2] as f32;
                let exact = rect[axis] + expected[axis] * rect[axis + 2];
                if !mapped.is_finite() || (mapped as f64 - exact).abs() > MAX_F32_CORNER_ERROR {
                    return Err("warp f32 mapping loses canvas source corner precision".into());
                }
            }
        }
        self.source_rect = Some(rect);
        Ok(self)
    }

    pub fn source_rect(&self) -> Option<[f64; 4]> {
        self.source_rect
    }

    /// Unclamped absolute canvas coordinates, before capture y inversion.
    /// Existing callers without a source rectangle retain unit pixel density.
    pub fn canvas_at(&self, x: f64, y: f64, fallback_origin: [f64; 2]) -> Option<[f64; 2]> {
        let local = self.source_at(x, y)?;
        let rect = self.source_rect.unwrap_or([
            fallback_origin[0],
            fallback_origin[1],
            self.output_width as f64,
            self.output_height as f64,
        ]);
        let point = [
            rect[0] + local[0] / self.output_width as f64 * rect[2],
            rect[1] + local[1] / self.output_height as f64 * rect[3],
        ];
        point.iter().all(|v| v.is_finite()).then_some(point)
    }

    /// Canvas sampling position with source-border texel-center clamping.
    /// Subpixel source extents sample their midpoint. Canvas clipping and
    /// capture y inversion still belong to the caller.
    pub fn clamped_canvas_at(&self, x: f64, y: f64, fallback_origin: [f64; 2]) -> Option<[f64; 2]> {
        let mut point = self.canvas_at(x, y, fallback_origin)?;
        let rect = self.source_rect.unwrap_or([
            fallback_origin[0],
            fallback_origin[1],
            self.output_width as f64,
            self.output_height as f64,
        ]);
        for axis in 0..2 {
            let inset = (rect[axis + 2] * 0.5).min(0.5);
            point[axis] = point[axis].clamp(
                rect[axis] + inset,
                rect[axis] + (rect[axis + 2] - inset).max(inset),
            );
        }
        Some(point)
    }

    /// Construct the exact neutral mapping for an output of `width`×`height`.
    pub fn identity(width: u32, height: u32) -> Self {
        Self::new(
            [
                [0.0, 0.0],
                [width as f64, 0.0],
                [width as f64, height as f64],
                [0.0, height as f64],
            ],
            [0.5, 0.5],
            width,
            height,
        )
        .expect("identity dimensions must be in 1..=32768")
    }

    /// Inverse rows mapping output pixel boundaries to remapped unit coords.
    pub fn inverse_rows(&self) -> [[f32; 4]; 3] {
        [
            [
                self.inverse[0][0] as f32,
                self.inverse[0][1] as f32,
                self.inverse[0][2] as f32,
                0.0,
            ],
            [
                self.inverse[1][0] as f32,
                self.inverse[1][1] as f32,
                self.inverse[1][2] as f32,
                0.0,
            ],
            [
                self.inverse[2][0] as f32,
                self.inverse[2][1] as f32,
                self.inverse[2][2] as f32,
                0.0,
            ],
        ]
    }

    pub fn center(&self) -> [f32; 2] {
        [self.center[0] as f32, self.center[1] as f32]
    }

    /// Raster dimensions used to convert the inverse map to source pixels.
    pub fn output_size(&self) -> [u32; 2] {
        [self.output_width, self.output_height]
    }

    /// Exact local identity, without a tolerance that could hide a pin edit.
    /// This alone does not establish integer-exact canvas sampling: the caller
    /// must also establish an in-bounds integer source origin and unit density
    /// using actual source/output dimensions, including rounded canvas height.
    pub fn is_identity(&self) -> bool {
        self.identity
    }

    /// Map a local source unit coordinate to output pixel boundaries.
    /// Applies the center remaps before the homography; no canvas crop or
    /// capture y inversion is involved. Coordinates may be extrapolated, but
    /// a nonfinite result or a point at a projective pole returns `None`.
    pub fn destination_at(&self, u: f64, v: f64) -> Option<[f64; 2]> {
        let q = mat_point(
            self.forward,
            [map_center(u, self.center[0]), map_center(v, self.center[1])],
        );
        if !q.iter().all(|v| v.is_finite()) || q[2].abs() <= DENOM_EPS {
            return None;
        }
        let p = [q[0] / q[2], q[1] / q[2]];
        p.iter().all(|v| v.is_finite()).then_some(p)
    }

    /// Return unclamped local source pixel coordinates for an output boundary.
    pub fn source_at(&self, x: f64, y: f64) -> Option<[f64; 2]> {
        if !x.is_finite() || !y.is_finite() {
            return None;
        }
        let q = mat_point(self.inverse, [x, y]);
        if !q.iter().all(|v| v.is_finite()) || q[2].abs() <= DENOM_EPS {
            return None;
        }
        let r = q[0] / q[2];
        let s = q[1] / q[2];
        if !r.is_finite() || !s.is_finite() {
            return None;
        }
        let source = [
            unmap_center(r, self.center[0]) * self.output_width as f64,
            unmap_center(s, self.center[1]) * self.output_height as f64,
        ];
        source.iter().all(|v| v.is_finite()).then_some(source)
    }

    /// Area of the destination quad intersected with output pixel `[x,x+1]×[y,y+1]`.
    pub fn coverage(&self, x: u32, y: u32) -> f64 {
        let px = x as f64;
        let py = y as f64;
        let max_x = px + 1.0;
        let max_y = py + 1.0;
        if self.bounds[2] <= px
            || self.bounds[0] >= max_x
            || self.bounds[3] <= py
            || self.bounds[1] >= max_y
        {
            return 0.0;
        }
        let pixel = [[px, py], [max_x, py], [max_x, max_y], [px, max_y]];
        if self
            .edges
            .iter()
            .any(|e| edge_max(e, px, py, max_x, max_y) < -EPS)
        {
            return 0.0;
        }
        if self
            .edges
            .iter()
            .all(|e| edge_min(e, px, py, max_x, max_y) >= -EPS)
        {
            return 1.0;
        }
        let clipped = clip_polygon(&self.corners, &pixel);
        (signed_area(&clipped).abs()).clamp(0.0, 1.0)
    }
}

fn map_center(t: f64, c: f64) -> f64 {
    if t <= 0.5 {
        2.0 * c * t
    } else {
        c + 2.0 * (1.0 - c) * (t - 0.5)
    }
}

fn unmap_center(s: f64, c: f64) -> f64 {
    if s <= c {
        s / (2.0 * c)
    } else {
        0.5 + (s - c) / (2.0 * (1.0 - c))
    }
}

fn signed_area(p: &[[f64; 2]]) -> f64 {
    p.iter()
        .enumerate()
        .map(|(i, a)| {
            let b = p[(i + 1) % p.len()];
            a[0] * b[1] - b[0] * a[1]
        })
        .sum::<f64>()
        * 0.5
}
fn sub(a: [f64; 2], b: [f64; 2]) -> [f64; 2] {
    [a[0] - b[0], a[1] - b[1]]
}
fn cross(a: [f64; 2], b: [f64; 2]) -> f64 {
    a[0] * b[1] - a[1] * b[0]
}
fn mat_point(m: [[f64; 3]; 3], p: [f64; 2]) -> [f64; 3] {
    [
        m[0][0] * p[0] + m[0][1] * p[1] + m[0][2],
        m[1][0] * p[0] + m[1][1] * p[1] + m[1][2],
        m[2][0] * p[0] + m[2][1] * p[1] + m[2][2],
    ]
}

fn check_denominator(m: &[[f64; 3]; 3], points: &[[f64; 2]], name: &str) -> Result<(), String> {
    let mut sign = 0.0;
    for p in points {
        let d = m[2][0] * p[0] + m[2][1] * p[1] + m[2][2];
        if !d.is_finite() || d.abs() <= DENOM_EPS {
            return Err(format!("warp has a pole in {name} domain"));
        }
        if sign == 0.0 {
            sign = d.signum();
        } else if d.signum() != sign {
            return Err(format!("warp has a pole in {name} domain"));
        }
    }
    Ok(())
}

fn check_f32_denominator(m: &[[f64; 3]; 3], points: &[[f64; 2]]) -> Result<(), String> {
    let q = m.map(|r| r.map(|v| v as f32));
    let mut sign = 0.0;
    for p in points {
        let d = q[2][0] * p[0] as f32 + q[2][1] * p[1] as f32 + q[2][2];
        let original = m[2][0] * p[0] + m[2][1] * p[1] + m[2][2];
        if !d.is_finite()
            || (d as f64).abs() <= DENOM_EPS
            || d.signum() as f64 != original.signum()
            || (sign != 0.0 && d.signum() != sign)
        {
            return Err(
                "warp f32 mapping has a pole or unstable denominator in output domain".into(),
            );
        }
        sign = d.signum();
    }
    Ok(())
}

fn check_f32_corners(
    inverse: &[[f64; 3]; 3],
    corners: &[[f64; 2]; 4],
    center: [f64; 2],
    width: u32,
    height: u32,
) -> Result<(), String> {
    let rows = inverse.map(|r| r.map(|v| v as f32));
    let expected = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
    for (p, expected) in corners.iter().zip(expected) {
        let q = rows.map(|r| r[0] * p[0] as f32 + r[1] * p[1] as f32 + r[2]);
        for axis in 0..2 {
            // Match the shader's f32 arithmetic, including its center remap.
            let s = q[axis] / q[2];
            let c = center[axis] as f32;
            let local = if s <= c {
                s / (2.0 * c)
            } else {
                0.5 + (s - c) / (2.0 * (1.0 - c))
            };
            let error = (local as f64 - expected[axis]).abs() * [width as f64, height as f64][axis];
            if !error.is_finite() || error > MAX_F32_CORNER_ERROR {
                return Err("warp f32 mapping loses source corner precision".into());
            }
        }
    }
    Ok(())
}

fn edge_value(e: [f64; 3], x: f64, y: f64) -> f64 {
    e[0] * x + e[1] * y + e[2]
}
fn edge_min(e: &[f64; 3], x0: f64, y0: f64, x1: f64, y1: f64) -> f64 {
    [
        edge_value(*e, x0, y0),
        edge_value(*e, x1, y0),
        edge_value(*e, x1, y1),
        edge_value(*e, x0, y1),
    ]
    .iter()
    .copied()
    .fold(f64::INFINITY, f64::min)
}
fn edge_max(e: &[f64; 3], x0: f64, y0: f64, x1: f64, y1: f64) -> f64 {
    [
        edge_value(*e, x0, y0),
        edge_value(*e, x1, y0),
        edge_value(*e, x1, y1),
        edge_value(*e, x0, y1),
    ]
    .iter()
    .copied()
    .fold(f64::NEG_INFINITY, f64::max)
}

fn solve_homography(src: [[f64; 2]; 4], dst: [[f64; 2]; 4]) -> Result<[[f64; 3]; 3], String> {
    let mut a = [[0.0; 9]; 8];
    for (i, (s, d)) in src.iter().zip(dst).enumerate() {
        let (u, v) = (s[0], s[1]);
        let (x, y) = (d[0], d[1]);
        let r = 2 * i;
        a[r] = [u, v, 1.0, 0.0, 0.0, 0.0, -u * x, -v * x, x];
        a[r + 1] = [0.0, 0.0, 0.0, u, v, 1.0, -u * y, -v * y, y];
    }
    for col in 0..8 {
        let mut pivot = col;
        for r in col + 1..8 {
            if a[r][col].abs() > a[pivot][col].abs() {
                pivot = r;
            }
        }
        let scale = a
            .iter()
            .skip(col)
            .map(|r| r.iter().take(8).map(|v| v.abs()).fold(0.0, f64::max))
            .fold(0.0, f64::max);
        if a[pivot][col].abs() <= EPS * scale.max(1.0) {
            return Err("warp homography is ill-conditioned".into());
        }
        a.swap(col, pivot);
        let d = a[col][col];
        for value in a[col][col..9].iter_mut() {
            *value /= d;
        }
        let pivot_row = a[col];
        for (r, row) in a.iter_mut().enumerate() {
            if r != col {
                let f = row[col];
                for (value, pivot_value) in row[col..9].iter_mut().zip(pivot_row[col..9].iter()) {
                    *value -= f * pivot_value;
                }
            }
        }
    }
    Ok([
        [a[0][8], a[1][8], a[2][8]],
        [a[3][8], a[4][8], a[5][8]],
        [a[6][8], a[7][8], 1.0],
    ])
}

fn invert3(m: [[f64; 3]; 3]) -> Option<[[f64; 3]; 3]> {
    let d = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
        - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
    if !d.is_finite() || d.abs() <= EPS {
        return None;
    }
    let c = [
        [
            m[1][1] * m[2][2] - m[1][2] * m[2][1],
            m[0][2] * m[2][1] - m[0][1] * m[2][2],
            m[0][1] * m[1][2] - m[0][2] * m[1][1],
        ],
        [
            m[1][2] * m[2][0] - m[1][0] * m[2][2],
            m[0][0] * m[2][2] - m[0][2] * m[2][0],
            m[0][2] * m[1][0] - m[0][0] * m[1][2],
        ],
        [
            m[1][0] * m[2][1] - m[1][1] * m[2][0],
            m[0][1] * m[2][0] - m[0][0] * m[2][1],
            m[0][0] * m[1][1] - m[0][1] * m[1][0],
        ],
    ];
    Some(c.map(|r| r.map(|v| v / d)))
}

fn clip_polygon(subject: &[[f64; 2]; 4], clip: &[[f64; 2]; 4]) -> Vec<[f64; 2]> {
    let mut out = subject.to_vec();
    for i in 0..4 {
        let a = clip[i];
        let b = clip[(i + 1) % 4];
        let input = out;
        out = Vec::new();
        if input.is_empty() {
            break;
        }
        let mut prev = *input.last().unwrap();
        let mut prev_in = cross(sub(b, a), sub(prev, a)) >= -EPS;
        for cur in input {
            let cur_in = cross(sub(b, a), sub(cur, a)) >= -EPS;
            if cur_in != prev_in {
                let d = sub(cur, prev);
                let t = cross(sub(a, prev), sub(b, a)) / cross(d, sub(b, a));
                out.push([prev[0] + t * d[0], prev[1] + t * d[1]]);
            }
            if cur_in {
                out.push(cur);
            }
            prev = cur;
            prev_in = cur_in;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identity_and_center_inverse() {
        let w = Warp::identity(100, 80);
        assert!(w.is_identity());
        assert_eq!(w.source_at(50.0, 40.0), Some([50.0, 40.0]));
        assert_eq!(w.center(), [0.5, 0.5]);
    }
    #[test]
    fn nonneutral_centers_put_midpoint_on_requested_lines() {
        let w = Warp::new(
            [[0.0, 0.0], [100.0, 0.0], [100.0, 100.0], [0.0, 100.0]],
            [0.3, 0.7],
            100,
            100,
        )
        .unwrap();
        let p = w.source_at(30.0, 70.0).unwrap();
        assert!((p[0] - 50.0).abs() < 1e-9 && (p[1] - 50.0).abs() < 1e-9);
    }
    #[test]
    fn f32_inverse_stays_subpixel_accurate() {
        let w = Warp::new(
            [
                [100.0, 100.0],
                [1000.0, 0.0],
                [1000.0, 1000.0],
                [0.0, 1000.0],
            ],
            [0.5, 0.5],
            1000,
            1000,
        )
        .unwrap();
        let r = w.inverse_rows();
        for p in [[0.0, 0.0], [500.5, 300.25], [999.0, 999.0]] {
            let q = [
                r[0][0] * p[0] as f32 + r[0][1] * p[1] as f32 + r[0][2],
                r[1][0] * p[0] as f32 + r[1][1] * p[1] as f32 + r[1][2],
                r[2][0] * p[0] as f32 + r[2][1] * p[1] as f32 + r[2][2],
            ];
            let got = [q[0] / q[2], q[1] / q[2]];
            let exact = w.source_at(p[0], p[1]).unwrap();
            assert!(
                (got[0] as f64 * 1000.0 - exact[0]).abs() < 0.001
                    && (got[1] as f64 * 1000.0 - exact[1]).abs() < 0.001
            );
        }
    }
    #[test]
    fn partial_edge_area() {
        let w = Warp::new(
            [[0.5, 0.0], [2.0, 0.0], [2.0, 1.0], [0.5, 1.0]],
            [0.5, 0.5],
            2,
            1,
        )
        .unwrap();
        assert!((w.coverage(0, 0) - 0.5).abs() < 1e-9);
    }
    #[test]
    fn rejects_bad_inputs() {
        assert!(Warp::new(
            [[0.0, 0.0], [1.0, 1.0], [1.0, 0.0], [0.0, 1.0]],
            [0.5, 0.5],
            10,
            10
        )
        .is_err());
        assert!(Warp::new(
            [[f64::NAN, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
            [0.5, 0.5],
            10,
            10
        )
        .is_err());
        assert!(Warp::new(
            [[0.0, 0.0], [32769.0, 0.0], [32769.0, 10.0], [0.0, 10.0]],
            [0.5, 0.5],
            32769,
            10
        )
        .is_err());
        assert!(Warp::new(
            [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
            [0.0, 0.5],
            10,
            10
        )
        .is_err());
    }
    #[test]
    fn clipped_area_sums_to_quad_area() {
        let w = Warp::new(
            [[0.25, 0.25], [2.25, 0.25], [2.25, 1.25], [0.25, 1.25]],
            [0.5, 0.5],
            3,
            2,
        )
        .unwrap();
        let mut sum = 0.0;
        for y in 0..2 {
            for x in 0..3 {
                sum += w.coverage(x, y);
            }
        }
        assert!((sum - 2.0).abs() < 1e-9);
    }
    #[test]
    fn d1_corner_roundtrip() {
        let w = Warp::new(
            [
                [100.0, 100.0],
                [1000.0, 0.0],
                [1000.0, 1000.0],
                [0.0, 1000.0],
            ],
            [0.5, 0.5],
            1000,
            1000,
        )
        .unwrap();
        for (p, s) in [
            ([100.0, 100.0], [0.0, 0.0]),
            ([1000.0, 0.0], [1000.0, 0.0]),
            ([1000.0, 1000.0], [1000.0, 1000.0]),
            ([0.0, 1000.0], [0.0, 1000.0]),
        ] {
            let got = w.source_at(p[0], p[1]).unwrap();
            assert!((got[0] - s[0]).abs() < 1e-8 && (got[1] - s[1]).abs() < 1e-8);
        }
    }

    fn assert_point(actual: Option<[f64; 2]>, expected: [f64; 2]) {
        let actual = actual.expect("finite mapping");
        for axis in 0..2 {
            assert!(
                (actual[axis] - expected[axis]).abs() < 1e-8,
                "{actual:?} != {expected:?}"
            );
        }
    }

    #[test]
    fn identity_preserves_pixel_centers_including_first_and_last_rows() {
        for (width, height) in [(1, 1), (7, 3), (1920, 1200), (32768, 32768)] {
            let warp = Warp::identity(width, height);
            for y in [0, height / 2, height - 1] {
                for x in [0, width / 2, width - 1] {
                    let pixel = [x as f64 + 0.5, y as f64 + 0.5];
                    assert_point(warp.source_at(pixel[0], pixel[1]), pixel);
                    assert_point(
                        warp.destination_at(pixel[0] / width as f64, pixel[1] / height as f64),
                        pixel,
                    );
                    assert_eq!(warp.coverage(x, y), 1.0);
                }
            }
        }
    }

    #[test]
    fn source_rectangle_changes_canvas_mapping_without_changing_local_geometry() {
        let original = Warp::identity(17, 9);
        assert_eq!(original.source_rect(), None);
        assert_point(original.canvas_at(0.5, 8.5, [3.0, 2.0]), [3.5, 10.5]);
        let rect = [3.25, -2.125, 25.5, 13.5];
        let mapped = original.with_source_rect(rect).unwrap();
        assert_eq!(mapped.source_rect(), Some(rect));
        assert!(mapped.is_identity());
        assert_point(mapped.source_at(0.5, 8.5), [0.5, 8.5]);
        assert_point(mapped.canvas_at(0.5, 8.5, [99.0, 99.0]), [4.0, 10.625]);
        assert_point(mapped.destination_at(0.5, 0.5), [8.5, 4.5]);
        assert_eq!(mapped.coverage(0, 0), 1.0);
        // The caller keeps this exact rect and selects general sampling.
        let near = Warp::identity(17, 9)
            .with_source_rect([1e-10, 0.0, 17.0, 9.0])
            .unwrap();
        assert_eq!(near.source_rect().unwrap()[0], 1e-10);
    }

    #[test]
    fn source_border_clamp_uses_canvas_pixels_and_subpixel_midpoints() {
        let warp = Warp::identity(8, 4)
            .with_source_rect([2.25, 3.125, 16.0, 2.0])
            .unwrap();
        assert_point(warp.clamped_canvas_at(-1.0, -1.0, [0.0; 2]), [2.75, 3.625]);
        assert_point(warp.clamped_canvas_at(9.0, 5.0, [0.0; 2]), [17.75, 4.625]);
        let tiny = Warp::identity(8, 4)
            .with_source_rect([0.1, 3.125, 0.3, 0.25])
            .unwrap();
        for point in [[0.0, 0.0], [4.0, 2.0], [8.0, 4.0]] {
            assert_point(
                tiny.clamped_canvas_at(point[0], point[1], [0.0; 2]),
                [0.25, 3.25],
            );
        }
    }

    #[test]
    fn source_rectangle_rejects_unbounded_and_unrepresentable_inputs() {
        for rect in [
            [f64::NAN, 0.0, 8.0, 4.0],
            [0.0, 0.0, f64::INFINITY, 4.0],
            [0.0, 0.0, 0.0, 4.0],
            [0.0, 0.0, 8.0, -1.0],
            [0.0, 0.0, 1e-300, 4.0],
            [524288.0, 0.0, 8.0, 4.0],
            [-524289.0, 0.0, 8.0, 4.0],
        ] {
            assert!(
                Warp::identity(8, 4).with_source_rect(rect).is_err(),
                "{rect:?}"
            );
        }
        // This narrow translated destination retains local pixel precision,
        // but magnifying the source makes f32 cancellation unacceptable.
        let warp = Warp::new(
            [[10.0, 0.0], [10.003, 0.0], [10.003, 4.0], [10.0, 4.0]],
            [0.5, 0.5],
            8,
            4,
        )
        .unwrap();
        assert!(warp.with_source_rect([0.0, 0.0, 32768.0, 4.0]).is_err());
    }

    #[test]
    #[cfg(feature = "projection")]
    fn d1_moves_the_picture_and_overlap_without_changing_source_selection() {
        use crate::model::Rect;
        let a_source = Rect {
            x: 0,
            y: 0,
            width: 1000,
            height: 1000,
        };
        let b_source = Rect { x: 900, ..a_source };
        let a = Warp::new(
            [
                [100.0, 100.0],
                [1000.0, 0.0],
                [1000.0, 1000.0],
                [0.0, 1000.0],
            ],
            [0.5, 0.5],
            1000,
            1000,
        )
        .unwrap();
        let b = Warp::identity(1000, 1000);
        for (u, v) in [
            (0.0, 0.0),
            (1.0, 0.0),
            (1.0, 1.0),
            (0.0, 1.0),
            (0.5, 0.5),
            (0.9, 0.0),
            (0.9, 1.0),
            (0.95, 0.5),
        ] {
            // Independent, published D1 H; not a second call to the solver.
            let d = 10.0 - u - v;
            let destination = [
                1000.0 * (8.0 * u - v + 1.0) / d,
                1000.0 * (-u + 8.0 * v + 1.0) / d,
            ];
            assert_point(a.destination_at(u, v), destination);
            assert_point(
                a.source_at(destination[0], destination[1]),
                [1000.0 * u, 1000.0 * v],
            );
        }
        let destination = a.destination_at(0.95, 0.5).unwrap();
        let local = a.source_at(destination[0], destination[1]).unwrap();
        // A left-to-right ramp over local[0] in [900, 1000] would sit
        // exactly half-way (gain 128/256) here; check that arithmetic
        // directly (`blend::RampSpec`/`transfer_at` were retired in favor of
        // `layout::Evaluator`, the single blend-weight rule) to keep this
        // fixed-point precision check on `source_at`'s round trip.
        let transmitted = (1.0 - (local[0] - 900.0) / 100.0).clamp(0.0, 1.0);
        assert_eq!((transmitted * 256.0).round() as u16, 128);
        let b_local = b.source_at(50.0, 500.0).unwrap();
        assert_point(
            Some([b_source.x as f64 + b_local[0], b_local[1]]),
            [950.0, 500.0],
        );
        assert_eq!(a.coverage(0, 0), 0.0);
        assert!(a.source_at(0.0, 0.0).unwrap().iter().all(|v| *v < 0.0));
    }

    #[test]
    fn center_remaps_roundtrip_on_both_sides_of_each_break() {
        for center in [[0.01, 0.99], [0.3, 0.7], [0.5, 0.5], [0.99, 0.01]] {
            let warp = Warp::new(
                [[10.0, 6.0], [310.0, 0.0], [300.0, 190.0], [0.0, 180.0]],
                center,
                320,
                200,
            )
            .unwrap();
            for u in [0.0, 0.25, 0.499, 0.5, 0.501, 0.75, 1.0] {
                for v in [0.0, 0.25, 0.499, 0.5, 0.501, 0.75, 1.0] {
                    let p = warp.destination_at(u, v).unwrap();
                    assert_point(warp.source_at(p[0], p[1]), [u * 320.0, v * 200.0]);
                }
            }
        }
        // Center lines stay straight, but a diagonal can kink at a midline.
        let warp = Warp::new(
            [[0.0, 0.0], [100.0, 0.0], [100.0, 100.0], [0.0, 100.0]],
            [0.3, 0.7],
            100,
            100,
        )
        .unwrap();
        for v in [0.0, 0.25, 0.5, 0.75, 1.0] {
            assert!((warp.destination_at(0.5, v).unwrap()[0] - 30.0).abs() < 1e-8);
        }
        assert_point(warp.destination_at(0.25, 0.25), [15.0, 35.0]);
        assert_point(warp.destination_at(0.5, 0.5), [30.0, 70.0]);
        assert_point(warp.destination_at(0.75, 0.75), [65.0, 85.0]);
    }

    #[test]
    fn exact_local_identity_never_snaps_nearby_edits_or_other_transforms() {
        let corners = [[0.0, 0.0], [100.0, 0.0], [100.0, 80.0], [0.0, 80.0]];
        assert!(Warp::new(corners, [0.5, 0.5], 100, 80)
            .unwrap()
            .is_identity());
        let mut near = corners;
        near[0][0] = 1e-10;
        for (pins, center) in [
            (near, [0.5, 0.5]),
            (corners, [0.5 + 1e-10, 0.5]),
            (corners.map(|p| [p[0] + 0.25, p[1]]), [0.5, 0.5]),
            (corners.map(|p| [p[0] + 1.0, p[1]]), [0.5, 0.5]),
            (corners.map(|p| [p[0] * 0.75, p[1]]), [0.5, 0.5]),
            ([corners[1], corners[2], corners[3], corners[0]], [0.5, 0.5]),
        ] {
            assert!(!Warp::new(pins, center, 100, 80).unwrap().is_identity());
        }
        assert!(
            Warp::new(
                [corners[1], corners[0], corners[3], corners[2]],
                [0.5, 0.5],
                100,
                80
            )
            .is_err(),
            "reflection reverses winding"
        );
    }

    #[test]
    fn invalid_geometry_and_numeric_limits_are_rejected() {
        let rect = [[0.0, 0.0], [100.0, 0.0], [100.0, 100.0], [0.0, 100.0]];
        for pins in [
            [rect[0], rect[0], rect[2], rect[3]],             // duplicate
            [[0.0, 0.0], [1.0, 0.0], [2.0, 0.0], [3.0, 0.0]], // zero area
            [rect[0], rect[1], [25.0, 25.0], rect[3]],        // concave
            [rect[0], rect[3], rect[2], rect[1]],             // reversed
            rect.map(|p| [p[0], p[1] * 1e-14]),               // scale-aware area bound
            [[f64::INFINITY, 0.0], rect[1], rect[2], rect[3]],
            [[-1600.01, 0.0], rect[1], rect[2], rect[3]],
            // Convex trapezoid, but its inverse pole crosses the raster.
            [[40.0, 40.0], [60.0, 40.0], [90.0, 60.0], [10.0, 60.0]],
        ] {
            assert!(Warp::new(pins, [0.5, 0.5], 100, 100).is_err(), "{pins:?}");
        }
        for center in [
            [f64::NAN, 0.5],
            [0.5, f64::INFINITY],
            [0.009, 0.5],
            [0.5, 0.991],
        ] {
            assert!(Warp::new(rect, center, 100, 100).is_err());
        }
        for (width, height) in [(0, 100), (100, 0), (32769, 100), (100, 32769)] {
            assert!(Warp::new(rect, [0.5, 0.5], width, height).is_err());
        }
        let warp = Warp::identity(100, 100);
        assert_eq!(warp.source_at(f64::NAN, 0.0), None);
        assert_eq!(warp.source_at(0.0, f64::INFINITY), None);
        assert_eq!(warp.destination_at(f64::INFINITY, 0.5), None);
        assert_eq!(warp.destination_at(0.5, f64::NAN), None);
    }

    #[test]
    fn rounded_gpu_mapping_must_retain_denominator_sign_and_corner_precision() {
        let poles = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [-2.0, 0.0, 1.0]];
        assert!(check_f32_denominator(&poles, &[[0.0, 0.0], [1.0, 0.0]]).is_err());
        // Finite f32 coefficients can still lose a narrow translated picture.
        let pins = [
            [1600.0, 0.0],
            [1600.00001, 0.0],
            [1600.00001, 100.0],
            [1600.0, 100.0],
        ];
        assert!(Warp::new(pins, [0.5, 0.5], 1000, 100).is_err());
    }

    #[test]
    fn coverage_handles_corners_shallow_edges_and_raster_clipping() {
        let slanted = Warp::new(
            [[0.0, 0.0], [4.0, 1.0], [4.0, 3.0], [0.0, 2.0]],
            [0.5, 0.5],
            4,
            3,
        )
        .unwrap();
        // The top edge is y=x/4; exact area under it in each unit column.
        for (x, expected) in [0.875, 0.625, 0.375, 0.125].into_iter().enumerate() {
            assert!((slanted.coverage(x as u32, 0) - expected).abs() < 1e-9);
        }
        let corner = Warp::new(
            [[0.5, 0.5], [2.0, 0.5], [2.0, 2.0], [0.5, 2.0]],
            [0.5, 0.5],
            3,
            3,
        )
        .unwrap();
        assert_eq!(corner.coverage(0, 0), 0.25);
        assert_eq!(corner.coverage(1, 1), 1.0);
        assert_eq!(corner.coverage(2, 2), 0.0);
        let overscan = Warp::new(
            [[-1.0, -1.0], [1.5, -1.0], [1.5, 3.0], [-1.0, 3.0]],
            [0.5, 0.5],
            2,
            2,
        )
        .unwrap();
        let area: f64 = (0..2)
            .flat_map(|y| (0..2).map(move |x| (x, y)))
            .map(|(x, y)| overscan.coverage(x, y))
            .sum();
        assert_eq!(area, 3.0);
    }
}
