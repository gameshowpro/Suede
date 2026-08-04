//! Built-in test patterns, rendered on the CPU into the overlay buffer.
//!
//! Adapted from the operator's proven SVG bench pattern: 100 px colour tiles,
//! a white diagonal cross with a small black centre cross per tile, a corner
//! triangle, and pixel coordinates — plus patterns specific to edge blending
//! (white for ramps, black for lift, a gamma-measurement chart).
//!
//! Everything is drawn in *global* layout coordinates derived from the
//! output's rectangle, so a feature at global x=1800 lands on the same spot
//! of both projectors sharing a seam: when the projectors are physically
//! aligned, the patterns superimpose exactly. That is what makes the grid an
//! alignment tool rather than just a picture.

use crate::model::{Rect, TestPattern};

use super::blend::OverlaySpec;

const TILE: i32 = 100;

/// Render the pattern as RGB, three bytes per pixel, row-major.
pub fn render(width: u32, height: u32, spec: &OverlaySpec) -> Vec<u8> {
    let mut rgb = vec![0u8; width as usize * height as usize * 3];
    match spec.pattern {
        Some(TestPattern::Grid) => grid(&mut rgb, width, height, &spec.rect, &spec.output),
        Some(TestPattern::White) => rgb.fill(255),
        // Black is already black — and deliberately unmarked: any lit pixel
        // would corrupt the black-lift comparison it exists for.
        Some(TestPattern::Black) => {}
        Some(TestPattern::Gamma) => gamma_chart(&mut rgb, width, height, spec.gamma),
        None => {}
    }
    rgb
}

// --- the tile grid --------------------------------------------------------

fn grid(rgb: &mut [u8], width: u32, height: u32, rect: &Rect, output: &str) {
    for y in 0..height as i32 {
        for x in 0..width as i32 {
            let gx = x + rect.x;
            let gy = y + rect.y;
            let u = gx.rem_euclid(TILE);
            let v = gy.rem_euclid(TILE);

            let mut pixel = tile_color(gx.div_euclid(TILE) as i64, gy.div_euclid(TILE) as i64);

            // White diagonal cross, corner to corner.
            let on_down = (v - u).abs() <= 1;
            let on_up = (v - (TILE - 1 - u)).abs() <= 1;
            if on_down || on_up {
                pixel = [255, 255, 255];
                // Small black centre cross on top of the white diagonals.
                if (37..=62).contains(&u) && (37..=62).contains(&v) {
                    pixel = [0, 0, 0];
                }
            }

            // Corner triangle marking the tile origin.
            if u < 10 && (v as f64) < 8.66 * (1.0 - u as f64 / 10.0) {
                pixel = [255, 255, 255];
            }

            put(rgb, width, x, y, pixel);
        }
    }

    // Tile annotations: global coordinates top corners, output name at the
    // bottom. Text is screen truth — a photo of the projection says exactly which
    // output and which pixels are in frame.
    let first_tx = rect.x.div_euclid(TILE);
    let first_ty = rect.y.div_euclid(TILE);
    let last_tx = (rect.x + width as i32 - 1).div_euclid(TILE);
    let last_ty = (rect.y + height as i32 - 1).div_euclid(TILE);
    for ty in first_ty..=last_ty {
        for tx in first_tx..=last_tx {
            let origin_x = tx * TILE - rect.x;
            let origin_y = ty * TILE - rect.y;
            let label_x = (tx * TILE).to_string();
            let label_y = (ty * TILE).to_string();
            text(rgb, width, height, origin_x + 5, origin_y + 5, 1, &label_x);
            let w = text_width(&label_y, 1);
            text(
                rgb,
                width,
                height,
                origin_x + TILE - 5 - w,
                origin_y + 14,
                1,
                &label_y,
            );
            let w = text_width(output, 1);
            text(
                rgb,
                width,
                height,
                origin_x + TILE - 5 - w,
                origin_y + TILE - 12,
                1,
                output,
            );
        }
    }
}

/// Tile colours in the spirit of the reference pattern: hue families down the
/// rows, light-to-dark variants across the columns, a grey every tenth.
fn tile_color(tx: i64, ty: i64) -> [u8; 3] {
    let col = tx.rem_euclid(10) as usize;
    let row = ty.rem_euclid(18) as usize;
    if col == 9 {
        return if row % 2 == 0 {
            [66, 66, 66]
        } else {
            [158, 158, 158]
        };
    }
    let hue = row as f64 * 20.0;
    let lightness = [0.50, 0.62, 0.72, 0.82, 0.90, 0.42, 0.34, 0.27, 0.20][col];
    hsl(hue, 0.72, lightness)
}

fn hsl(hue: f64, saturation: f64, lightness: f64) -> [u8; 3] {
    let c = (1.0 - (2.0 * lightness - 1.0).abs()) * saturation;
    let h = hue / 60.0;
    let x = c * (1.0 - (h % 2.0 - 1.0).abs());
    let (r, g, b) = match h as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = lightness - c / 2.0;
    [
        ((r + m) * 255.0).round() as u8,
        ((g + m) * 255.0).round() as u8,
        ((b + m) * 255.0).round() as u8,
    ]
}

// --- the gamma chart ------------------------------------------------------

/// Candidate patches beside a stripe field averaging to half light.
///
/// Alternating single-pixel black and white rows emit 50% luminance no
/// matter what the display's transfer curve is. A solid patch of signal
/// `(1/2)^(1/γ)` emits 50% *only* when the display's gamma is γ. Stand back,
/// squint, and the candidate that melts into its stripes names the
/// projector's gamma — the number the `gamma` setting wants.
fn gamma_chart(rgb: &mut [u8], width: u32, height: u32, configured: f64) {
    const CANDIDATES: [f64; 6] = [1.6, 1.8, 2.0, 2.2, 2.4, 2.6];
    const BLOCK_W: i32 = 200;
    const BLOCK_H: i32 = 220;
    const GAP: i32 = 24;

    let total = CANDIDATES.len() as i32 * (BLOCK_W + GAP) - GAP;
    let left = (width as i32 - total) / 2;
    let top = (height as i32 - BLOCK_H) / 2;

    for (index, gamma) in CANDIDATES.iter().enumerate() {
        let x0 = left + index as i32 * (BLOCK_W + GAP);
        let solid = (255.0 * 0.5f64.powf(1.0 / gamma)).round() as u8;

        for y in top..top + BLOCK_H - 40 {
            for x in x0..x0 + BLOCK_W {
                if x < 0 || y < 0 || x >= width as i32 || y >= height as i32 {
                    continue;
                }
                // Stripes surround the solid centre on both sides, so the
                // comparison is local rather than across the block edge.
                let inner = x >= x0 + BLOCK_W / 4 && x < x0 + 3 * BLOCK_W / 4;
                let value = if inner {
                    solid
                } else if y % 2 == 0 {
                    255
                } else {
                    0
                };
                put(rgb, width, x, y, [value, value, value]);
            }
        }

        let label = format!("{gamma:.1}");
        let w = text_width(&label, 2);
        text(
            rgb,
            width,
            height,
            x0 + (BLOCK_W - w) / 2,
            top + BLOCK_H - 24,
            2,
            &label,
        );
        // Mark the currently configured value so the operator can see what
        // the daemon believes while comparing it against reality.
        if (gamma - configured).abs() < 0.05 {
            for x in x0..x0 + BLOCK_W {
                for dy in 0..3 {
                    let y = top + BLOCK_H + 2 + dy;
                    if x >= 0 && y >= 0 && x < width as i32 && y < height as i32 {
                        put(rgb, width, x, y, [255, 255, 255]);
                    }
                }
            }
        }
    }
}

// --- text -----------------------------------------------------------------

fn put(rgb: &mut [u8], width: u32, x: i32, y: i32, pixel: [u8; 3]) {
    let offset = (y as usize * width as usize + x as usize) * 3;
    rgb[offset..offset + 3].copy_from_slice(&pixel);
}

pub fn text_width(message: &str, scale: i32) -> i32 {
    message.len() as i32 * 6 * scale
}

/// Blit `message` in the 5×7 font, white with a black drop shadow so it
/// survives any tile colour underneath.
pub fn text(rgb: &mut [u8], width: u32, height: u32, x: i32, y: i32, scale: i32, message: &str) {
    for (offset, colour) in [(scale.max(1), [0, 0, 0]), (0, [255, 255, 255])] {
        let mut pen_x = x + offset;
        let pen_y = y + offset;
        for c in message.chars() {
            let rows = glyph(c.to_ascii_uppercase());
            for (row_index, row) in rows.iter().enumerate() {
                for (col_index, cell) in row.chars().enumerate() {
                    if cell != '#' {
                        continue;
                    }
                    for sy in 0..scale {
                        for sx in 0..scale {
                            let px = pen_x + col_index as i32 * scale + sx;
                            let py = pen_y + row_index as i32 * scale + sy;
                            if px >= 0 && py >= 0 && px < width as i32 && py < height as i32 {
                                put(rgb, width, px, py, colour);
                            }
                        }
                    }
                }
            }
            pen_x += 6 * scale;
        }
    }
}

/// A 5×7 font covering what output names and coordinates need. Drawn as
/// strings so a review can literally read the glyphs.
fn glyph(c: char) -> [&'static str; 7] {
    match c {
        '0' => [
            " ### ", "#   #", "#  ##", "# # #", "##  #", "#   #", " ### ",
        ],
        '1' => [
            "  #  ", " ##  ", "  #  ", "  #  ", "  #  ", "  #  ", " ### ",
        ],
        '2' => [
            " ### ", "#   #", "    #", "   # ", "  #  ", " #   ", "#####",
        ],
        '3' => [
            " ### ", "#   #", "    #", "  ## ", "    #", "#   #", " ### ",
        ],
        '4' => [
            "   # ", "  ## ", " # # ", "#  # ", "#####", "   # ", "   # ",
        ],
        '5' => [
            "#####", "#    ", "#### ", "    #", "    #", "#   #", " ### ",
        ],
        '6' => [
            " ### ", "#    ", "#    ", "#### ", "#   #", "#   #", " ### ",
        ],
        '7' => [
            "#####", "    #", "   # ", "  #  ", "  #  ", "  #  ", "  #  ",
        ],
        '8' => [
            " ### ", "#   #", "#   #", " ### ", "#   #", "#   #", " ### ",
        ],
        '9' => [
            " ### ", "#   #", "#   #", " ####", "    #", "    #", " ### ",
        ],
        '-' => [
            "     ", "     ", "     ", "#####", "     ", "     ", "     ",
        ],
        '.' => [
            "     ", "     ", "     ", "     ", "     ", " ##  ", " ##  ",
        ],
        'A' => [
            " ### ", "#   #", "#   #", "#####", "#   #", "#   #", "#   #",
        ],
        'B' => [
            "#### ", "#   #", "#   #", "#### ", "#   #", "#   #", "#### ",
        ],
        'C' => [
            " ### ", "#   #", "#    ", "#    ", "#    ", "#   #", " ### ",
        ],
        'D' => [
            "#### ", "#   #", "#   #", "#   #", "#   #", "#   #", "#### ",
        ],
        'E' => [
            "#####", "#    ", "#    ", "#### ", "#    ", "#    ", "#####",
        ],
        'F' => [
            "#####", "#    ", "#    ", "#### ", "#    ", "#    ", "#    ",
        ],
        'G' => [
            " ### ", "#   #", "#    ", "# ###", "#   #", "#   #", " ### ",
        ],
        'H' => [
            "#   #", "#   #", "#   #", "#####", "#   #", "#   #", "#   #",
        ],
        'I' => [
            " ### ", "  #  ", "  #  ", "  #  ", "  #  ", "  #  ", " ### ",
        ],
        'J' => [
            "    #", "    #", "    #", "    #", "    #", "#   #", " ### ",
        ],
        'K' => [
            "#   #", "#  # ", "# #  ", "##   ", "# #  ", "#  # ", "#   #",
        ],
        'L' => [
            "#    ", "#    ", "#    ", "#    ", "#    ", "#    ", "#####",
        ],
        'M' => [
            "#   #", "## ##", "# # #", "# # #", "#   #", "#   #", "#   #",
        ],
        'N' => [
            "#   #", "##  #", "# # #", "#  ##", "#   #", "#   #", "#   #",
        ],
        'O' => [
            " ### ", "#   #", "#   #", "#   #", "#   #", "#   #", " ### ",
        ],
        'P' => [
            "#### ", "#   #", "#   #", "#### ", "#    ", "#    ", "#    ",
        ],
        'Q' => [
            " ### ", "#   #", "#   #", "#   #", "# # #", "#  # ", " ## #",
        ],
        'R' => [
            "#### ", "#   #", "#   #", "#### ", "# #  ", "#  # ", "#   #",
        ],
        'S' => [
            " ####", "#    ", "#    ", " ### ", "    #", "    #", "#### ",
        ],
        'T' => [
            "#####", "  #  ", "  #  ", "  #  ", "  #  ", "  #  ", "  #  ",
        ],
        'U' => [
            "#   #", "#   #", "#   #", "#   #", "#   #", "#   #", " ### ",
        ],
        'V' => [
            "#   #", "#   #", "#   #", "#   #", "#   #", " # # ", "  #  ",
        ],
        'W' => [
            "#   #", "#   #", "#   #", "# # #", "# # #", "## ##", "#   #",
        ],
        'X' => [
            "#   #", "#   #", " # # ", "  #  ", " # # ", "#   #", "#   #",
        ],
        'Y' => [
            "#   #", "#   #", " # # ", "  #  ", "  #  ", "  #  ", "  #  ",
        ],
        'Z' => [
            "#####", "    #", "   # ", "  #  ", " #   ", "#    ", "#####",
        ],
        _ => [
            "     ", "     ", "     ", "     ", "     ", "     ", "     ",
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TestPattern;

    fn spec(pattern: TestPattern, rect: Rect) -> OverlaySpec {
        OverlaySpec {
            output: "DP-1".into(),
            gamma: 2.2,
            black_lift: 0.0,
            rect,
            pattern: Some(pattern),
            ramps: Vec::new(),
        }
    }

    fn pixel(rgb: &[u8], width: u32, x: i32, y: i32) -> [u8; 3] {
        let offset = (y as usize * width as usize + x as usize) * 3;
        [rgb[offset], rgb[offset + 1], rgb[offset + 2]]
    }

    #[test]
    fn the_grid_is_continuous_across_a_seam() {
        // The whole point: both projectors draw the same global pixel the
        // same way, so aligned projectors superimpose the pattern exactly.
        let left = render(
            1920,
            1080,
            &spec(
                TestPattern::Grid,
                Rect {
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1080,
                },
            ),
        );
        let right = render(
            1920,
            1080,
            &spec(
                TestPattern::Grid,
                Rect {
                    x: 1760,
                    y: 0,
                    width: 1920,
                    height: 1080,
                },
            ),
        );
        // Compare a band of the shared region, skipping the per-tile text
        // (whose absolute placement is identical anyway, but the output name
        // differs between projectors only if names differ — here they match).
        for global_x in 1760..1920 {
            for global_y in (300..400).step_by(7) {
                assert_eq!(
                    pixel(&left, 1920, global_x, global_y),
                    pixel(&right, 1920, global_x - 1760, global_y),
                    "global pixel ({global_x},{global_y}) differs between outputs"
                );
            }
        }
    }

    #[test]
    fn white_and_black_are_what_they_say() {
        let white = render(
            64,
            32,
            &spec(
                TestPattern::White,
                Rect {
                    x: 0,
                    y: 0,
                    width: 64,
                    height: 32,
                },
            ),
        );
        assert!(white.iter().all(|&v| v == 255));
        let black = render(
            64,
            32,
            &spec(
                TestPattern::Black,
                Rect {
                    x: 0,
                    y: 0,
                    width: 64,
                    height: 32,
                },
            ),
        );
        assert!(
            black.iter().all(|&v| v == 0),
            "black must be unmarked, pure zero"
        );
    }

    #[test]
    fn the_gamma_chart_patches_follow_the_formula() {
        let rgb = render(
            1920,
            1080,
            &spec(
                TestPattern::Gamma,
                Rect {
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1080,
                },
            ),
        );
        // The 2.2 candidate is the fourth block; sample its solid centre.
        let block_x = (1920 - (6 * 224 - 24)) / 2 + 3 * 224 + 100;
        let value = pixel(&rgb, 1920, block_x, 540)[0];
        let expected = (255.0 * 0.5f64.powf(1.0 / 2.2)).round() as u8;
        assert_eq!(value, expected, "solid patch must be 0.5^(1/gamma)");
        // And its surround alternates full black and full white by row.
        let stripe_x = block_x - 90;
        let a = pixel(&rgb, 1920, stripe_x, 540)[0];
        let b = pixel(&rgb, 1920, stripe_x, 541)[0];
        assert_eq!((a.min(b), a.max(b)), (0, 255));
    }

    #[test]
    fn tile_colours_are_stable_and_distinct() {
        // Neighbouring tiles must differ (the grid must be visible), and the
        // palette must be deterministic (the fingerprint must be stable).
        assert_eq!(tile_color(0, 0), tile_color(0, 0));
        assert_ne!(tile_color(0, 0), tile_color(1, 0));
        assert_ne!(tile_color(0, 0), tile_color(0, 1));
    }

    #[test]
    fn text_renders_something_where_asked() {
        let mut rgb = vec![0u8; 100 * 20 * 3];
        text(&mut rgb, 100, 20, 2, 2, 1, "DP-1");
        assert!(rgb.contains(&255), "glyphs must produce pixels");
    }
}
