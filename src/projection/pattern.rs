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
        Some(TestPattern::Identify) => identify(&mut rgb, width, height, &spec.rect, &spec.output),
        None => {}
    }
    rgb
}

// --- output identification -----------------------------------------------

/// The connector's name, as large as this output will carry.
///
/// Sized for someone standing at the projector rather than sitting at the
/// screen: the answer they need is which cable to move, and the two displays
/// they are choosing between may be metres apart and differently lit. So the
/// name is scaled to the output rather than set at a fixed size, and the
/// background is keyed to the name so neighbours never look alike even when
/// the text is too far away to read.
fn identify(rgb: &mut [u8], width: u32, height: u32, rect: &Rect, output: &str) {
    let ground = name_colour(output);
    for pixel in rgb.chunks_exact_mut(3) {
        pixel.copy_from_slice(&ground);
    }

    // A border in the same hue but bright: it states where this output ends,
    // which is the other half of the question when two of them overlap.
    let edge = name_colour_bright(output);
    let thickness = (width.min(height) / 60).clamp(2, 12) as i32;
    for y in 0..height as i32 {
        for x in 0..width as i32 {
            if x < thickness
                || y < thickness
                || x >= width as i32 - thickness
                || y >= height as i32 - thickness
            {
                put(rgb, width, x, y, edge);
            }
        }
    }

    // The largest scale that leaves a margin, so a long name on a narrow
    // output shrinks to fit rather than running off the side.
    let name = output.to_ascii_uppercase();
    let usable = (width as f64 * 0.82) as i32;
    let by_width = (usable / (6 * name.len().max(1) as i32)).max(1);
    let by_height = ((height as f64 * 0.42) as i32 / 7).max(1);
    let scale = by_width.min(by_height);

    let title_w = text_width(&name, scale);
    let title_x = (width as i32 - title_w) / 2;
    let title_y = (height as i32 - 7 * scale) / 2 - height as i32 / 12;
    text(rgb, width, height, title_x, title_y, scale, &name);

    // Underneath, what the daemon thinks this output is: its size and where
    // it sits on the canvas. Ordering the cables is the point, and that is
    // the line which says whether the order came out right.
    let detail = format!("{}X{} AT {},{}", width, height, rect.x, rect.y);
    let small = (scale / 4).clamp(1, 6);
    let detail_w = text_width(&detail, small);
    text(
        rgb,
        width,
        height,
        (width as i32 - detail_w) / 2,
        title_y + 7 * scale + 8 * small,
        small,
        &detail,
    );
}

/// A dark ground unique to a connector name.
///
/// Hashed rather than taken from a list: the names are whatever the hardware
/// offers, and a list would eventually meet a machine whose outputs all fell
/// off the end of it and came out the same colour.
fn name_colour(output: &str) -> [u8; 3] {
    hsl(name_hue(output), 0.55, 0.22)
}

fn name_colour_bright(output: &str) -> [u8; 3] {
    hsl(name_hue(output), 0.75, 0.55)
}

fn name_hue(output: &str) -> f64 {
    // FNV-1a, for a spread that separates names differing by one character -
    // which is exactly what DP-5 and DP-6 are.
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in output.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    (hash % 360) as f64
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
    // The shadow exists to keep small text legible over the grid's tile
    // colours. Offsetting it by a whole scale unit is right at scale 1 and
    // absurd at scale 50, where it stops reading as a shadow and starts
    // reading as a second, misaligned copy of the letter.
    let shadow = (scale / 6).max(1);
    for (offset, colour) in [(shadow, [0, 0, 0]), (0, [255, 255, 255])] {
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
        ',' => [
            "     ", "     ", "     ", "     ", "  ## ", "  #  ", " #   ",
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

    /// Two connectors must never come out looking the same, or the pattern
    /// answers the wrong question: a wall of identical rectangles tells you
    /// nothing about which cable to move.
    #[test]
    fn every_output_gets_its_own_ground() {
        let names = [
            "DP-1", "DP-2", "DP-5", "DP-6", "DP-7", "DP-8", "HDMI-A-1", "HDMI-A-2", "eDP-1",
            "DVI-D-1",
        ];
        let mut seen = std::collections::HashMap::new();
        for name in names {
            let colour = name_colour(name);
            if let Some(other) = seen.insert(colour, name) {
                panic!("{name} and {other} share a ground colour {colour:?}");
            }
        }
        // Adjacent numbers are the pair most likely to be confused, so they
        // are the pair worth being furthest apart.
        let five = name_hue("DP-5");
        let six = name_hue("DP-6");
        assert!(
            (five - six).abs() > 20.0,
            "DP-5 and DP-6 hues are only {} apart",
            (five - six).abs()
        );
    }

    /// A long name on a narrow output must shrink rather than run off it.
    #[test]
    fn the_name_always_fits_the_output() {
        for (width, height, name) in [
            (1920u32, 1080u32, "DP-1"),
            (1280, 720, "HDMI-A-2"),
            (640, 480, "DP-1-1"),
            (3840, 2160, "DVI-D-1"),
            // Absurd, but the arithmetic should still hold.
            (800, 600, "DISPLAYPORT-EXTENDED-7"),
        ] {
            let rgb = render(
                width,
                height,
                &OverlaySpec {
                    output: name.into(),
                    gamma: 2.2,
                    black_lift: 0.0,
                    rect: Rect {
                        x: 0,
                        y: 0,
                        width: width as i32,
                        height: height as i32,
                    },
                    pattern: Some(TestPattern::Identify),
                    ramps: Vec::new(),
                },
            );
            assert_eq!(rgb.len(), width as usize * height as usize * 3);

            // White is only laid down by the text, so its extent is the
            // text's. Nothing may touch the outer edge.
            let mut min_x = width as i32;
            let mut max_x = -1i32;
            for y in 0..height as i32 {
                for x in 0..width as i32 {
                    if pixel(&rgb, width, x, y) == [255, 255, 255] {
                        min_x = min_x.min(x);
                        max_x = max_x.max(x);
                    }
                }
            }
            assert!(max_x >= 0, "{name} at {width}x{height} drew no text at all");
            assert!(
                min_x > 0 && max_x < width as i32 - 1,
                "{name} at {width}x{height} ran to the edge ({min_x}..{max_x})"
            );
        }
    }

    /// The name has to be legible across a room, which means most of the
    /// output's height, not a corner of it.
    #[test]
    fn the_name_is_drawn_large() {
        let rgb = render(
            1920,
            1080,
            &spec(
                TestPattern::Identify,
                Rect {
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1080,
                },
            ),
        );
        let mut top = 1080i32;
        let mut bottom = -1i32;
        for y in 0..1080 {
            for x in 0..1920 {
                if pixel(&rgb, 1920, x, y) == [255, 255, 255] {
                    top = top.min(y);
                    bottom = bottom.max(y);
                }
            }
        }
        let tall = bottom - top;
        assert!(
            tall > 1080 / 5,
            "the name is only {tall}px tall on a 1080p output"
        );
    }

    /// Not a test: writes each pattern out as a PPM so it can be looked at.
    ///
    ///     SUEDE_WRITE_PATTERN=/tmp/p cargo test -- --ignored write_patterns
    ///
    /// These are pictures, and some of their faults are only faults to an
    /// eye. This caught a drop shadow that was correct at small sizes and
    /// became a second misaligned copy of every letter at large ones, and a
    /// comma the font did not have and silently dropped - neither of which
    /// any assertion here was going to notice.
    #[test]
    #[ignore]
    fn write_patterns() {
        let Ok(dir) = std::env::var("SUEDE_WRITE_PATTERN") else {
            return;
        };
        let cases = [
            (TestPattern::Identify, "DP-5", 1920u32, 1200u32),
            (TestPattern::Identify, "DP-6", 1920, 1200),
            (TestPattern::Identify, "HDMI-A-1", 1280, 720),
            (TestPattern::Grid, "DP-5", 1920, 1200),
            (TestPattern::Gamma, "DP-5", 1920, 1200),
        ];
        for (pattern, name, w, h) in cases {
            let rgb = render(
                w,
                h,
                &OverlaySpec {
                    output: name.into(),
                    gamma: 2.2,
                    black_lift: 0.0,
                    rect: Rect {
                        x: 0,
                        y: 0,
                        width: w as i32,
                        height: h as i32,
                    },
                    pattern: Some(pattern),
                    ramps: Vec::new(),
                },
            );
            let mut out = format!("P6 {w} {h} 255 ").into_bytes();
            out.extend_from_slice(&rgb);
            let file = format!("{dir}-{pattern:?}-{name}.ppm").to_lowercase();
            std::fs::write(&file, out).unwrap();
            eprintln!("wrote {file}");
        }
    }
}
