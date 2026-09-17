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
        // The animated counter belongs to the slicer, which draws it per
        // frame from `sync_rects` rather than through this function at all.
        // Reaching here means the tiled path asked for it — see
        // `sync_unavailable`.
        Some(TestPattern::Sync) => sync_unavailable(&mut rgb, width, height),
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

// --- sync counter ---------------------------------------------------------

/// Bit per segment of a seven-segment digit, `A` (the top bar) in bit 0
/// through `G` (the middle bar) in bit 6 — the ordering `segment_rect`
/// below and [`DIGIT_SEGMENTS`] both use.
///
/// ```text
///   AAA      bit 0  A  top
///  F   B     bit 1  B  top right
///  F   B     bit 2  C  bottom right
///   GGG      bit 3  D  bottom
///  E   C     bit 4  E  bottom left
///  E   C     bit 5  F  top left
///   DDD      bit 6  G  middle
/// ```
const DIGIT_SEGMENTS: [u8; 10] = [
    0b0111111, // 0: A B C D E F
    0b0000110, // 1: B C
    0b1011011, // 2: A B D E G
    0b1001111, // 3: A B C D G
    0b1100110, // 4: B C F G
    0b1101101, // 5: A C D F G
    0b1111101, // 6: A C D E F G
    0b0000111, // 7: A B C
    0b1111111, // 8: all
    0b1101111, // 9: A B C D F G
];

/// The most glyphs [`sync_rects`] will draw for a name or an id, so the rect
/// count has a ceiling the GPU path's fixed-size buffer can be sized against
/// — see [`MAX_SYNC_RECTS`]. A connector name is never this long; a snapshot
/// id would need a century of frames to be.
const SYNC_TEXT_LIMIT: usize = 12;

/// The ceiling [`sync_rects`] guarantees it stays under, whatever the size,
/// the counter value, the output name or the snapshot id: 14 digit segments,
/// 16 strip cells of at most four rects each, and two runs of
/// [`SYNC_TEXT_LIMIT`] glyphs whose 5x7 cells merge into at most three
/// horizontal runs per row. Rounded up, and asserted by
/// `sync_rects_stay_under_the_advertised_ceiling`, so `gpu.rs` can allocate
/// one fixed buffer per output and never grow it.
pub const MAX_SYNC_RECTS: usize = 14 + 16 * 4 + 2 * SYNC_TEXT_LIMIT * 7 * 3;

/// One filled white rectangle of the sync pattern, in output-local pixels
/// and half-open: a pixel is lit when `x0 <= x < x1 && y0 <= y < y1`.
///
/// `u32` and half-open specifically because this is what the fragment shader
/// receives verbatim, four `uint`s per rect (see `gpu.rs`'s
/// `set_sync_shapes`), and because every rect has already been clipped to
/// the output — a negative coordinate could not survive that, and pretending
/// one might would only invite an `as` cast at the boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncRect {
    pub x0: u32,
    pub y0: u32,
    pub x1: u32,
    pub y1: u32,
}

impl SyncRect {
    /// Whether `(x, y)` is inside — the CPU path's half of what the shader's
    /// four comparisons do.
    pub fn contains(&self, x: u32, y: u32) -> bool {
        x >= self.x0 && x < self.x1 && y >= self.y0 && y < self.y1
    }
}

/// Rectangles that belong to one feature of the pattern, with the box that
/// encloses them.
///
/// The grouping exists for the GPU path: a fragment shader that tested every
/// rect against every pixel would do several hundred comparisons for a
/// picture that is almost entirely black. Testing the four features'
/// bounding boxes first means a pixel outside all of them costs four tests,
/// and a pixel inside one only pays for that feature's own rects. The CPU
/// path ignores the grouping and walks the rects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncGroup {
    pub bounds: SyncRect,
    pub rects: Vec<SyncRect>,
}

/// The sync pattern's geometry for one output and one frame: everything the
/// renderers need, as white rectangles on black, and nothing about how they
/// are drawn.
///
/// Pure, and the single source of both renderers' pixels — the CPU path
/// rasterises this list into an shm buffer and the GPU path uploads it to a
/// fragment shader — because the measurement this pattern exists for is only
/// meaningful if switching renderers cannot change what the camera sees.
///
/// What is drawn, and why each part is there:
///
/// - **Two seven-segment digits of `frame % 100`**, 90 % of the output's
///   height and centred. Big because the camera is looking at a projected
///   image from across a room, and seven-segment because segments are
///   rectangles: the same shapes the shader already tests, with no font
///   rasterisation on either path to disagree about.
/// - **A sixteen-bit binary strip of `frame & 0xffff`**, most significant
///   bit at the top, filled for one and hollow for zero. Two consecutive
///   frames differ in the low bits whatever the digits are doing, so the
///   strip still reads when a projector's colour wheel has smeared the
///   digits across the exposure — and it disambiguates the wrap from 99 to
///   00.
/// - **The output's name**, top left, so a photograph of two projectors
///   needs no notes about which is which.
/// - **The snapshot id**, bottom left and small, so a frame in a photograph
///   can be matched to the ten-second `GET /projection/stats` interval that
///   reported its straddles.
///
/// `frame` is the slicer's present-cycle counter; every output in one cycle
/// is drawn with the same value, so any difference a camera sees is the
/// presentation path's, not the pattern's.
pub fn sync_rects(
    width: u32,
    height: u32,
    frame: u32,
    output: &str,
    snapshot: u64,
) -> Vec<SyncGroup> {
    let (w, h) = (i64::from(width), i64::from(height));
    if w <= 0 || h <= 0 {
        return Vec::new();
    }
    let inset = (w / 100).max(2);

    // The digits, as large as 90 % of the height allows, narrowed if the
    // output is too thin to carry that (`11/10` is the block's width as a
    // multiple of one digit's height: two half-height-wide digits plus a
    // tenth-height gap).
    let digit_h = ((h * 9) / 10).min((w * 9 / 10) * 10 / 11).max(1);
    let digit_w = (digit_h / 2).max(1);
    let gap = (digit_h / 10).max(1);
    let block_w = 2 * digit_w + gap;
    let block_x = (w - block_w) / 2;
    let block_y = (h - digit_h) / 2;
    let thickness = (digit_h / 8).max(1);

    let mut groups = Vec::new();

    let value = frame % 100;
    let mut digits = Vec::new();
    for (index, digit) in [(value / 10) as usize, (value % 10) as usize]
        .into_iter()
        .enumerate()
    {
        let x = block_x + index as i64 * (digit_w + gap);
        let lit = DIGIT_SEGMENTS[digit];
        for segment in 0..7 {
            if lit & (1 << segment) == 0 {
                continue;
            }
            let (x0, y0, x1, y1) = segment_rect(segment, x, block_y, digit_w, digit_h, thickness);
            push_rect(&mut digits, x0, y0, x1, y1, width, height);
        }
    }
    push_group(&mut groups, digits);

    // The binary strip, down the margin to the right of the digits. Squeezed
    // rather than moved when that margin is narrow: a square output still
    // gets a readable-from-nearby strip, and clipping keeps it on screen
    // whatever happens.
    let pitch = (digit_h / 16).max(2);
    let margin = (w - (block_x + block_w) - 2 * inset).max(2);
    let cell = pitch.min(margin).max(1);
    let strip_x = w - inset - cell;
    let strip_y = block_y + (digit_h - pitch * 16) / 2;
    let border = (cell / 5).max(1);
    let mut strip = Vec::new();
    for bit in 0..16i64 {
        // Most significant bit at the top, so the strip reads the way the
        // number is written.
        let set = (frame >> (15 - bit as u32)) & 1 == 1;
        let (x0, y0) = (strip_x, strip_y + bit * pitch);
        let (x1, y1) = (x0 + cell, y0 + cell);
        if set {
            push_rect(&mut strip, x0, y0, x1, y1, width, height);
        } else {
            // Hollow, as four thin bars: a shader that only knows how to
            // test filled rectangles can still draw an outline.
            push_rect(&mut strip, x0, y0, x1, y0 + border, width, height);
            push_rect(&mut strip, x0, y1 - border, x1, y1, width, height);
            push_rect(
                &mut strip,
                x0,
                y0 + border,
                x0 + border,
                y1 - border,
                width,
                height,
            );
            push_rect(
                &mut strip,
                x1 - border,
                y0 + border,
                x1,
                y1 - border,
                width,
                height,
            );
        }
    }
    push_group(&mut groups, strip);

    // The output name, top left, as large as the margin beside the digits
    // will carry — so it never runs into the counter it labels.
    let name: String = output
        .to_ascii_uppercase()
        .chars()
        .take(SYNC_TEXT_LIMIT)
        .collect();
    if !name.is_empty() {
        // Two places it could go, and the bigger wins: beside the digits in
        // the left margin (which is where the room is on a wide output), or
        // above them in the top margin (which is where it is on a tall one).
        // Both start at the same corner, so this is a choice of constraint
        // rather than of position.
        let cell = 6 * name.len() as i64;
        let beside = ((h / 16) / 7).min(((block_x - 2 * inset).max(1)) / cell);
        let above =
            ((h / 16).min((block_y - 2 * inset).max(1)) / 7).min((w - 2 * inset).max(1) / cell);
        let scale = beside.max(above).max(1);
        let mut rects = Vec::new();
        text_rects(&name, inset, inset, scale, width, height, &mut rects);
        push_group(&mut groups, rects);
    }

    // The snapshot id, bottom left and deliberately small: it is read off a
    // photograph afterwards, never from the room.
    let label: String = format!("S{snapshot}")
        .chars()
        .take(SYNC_TEXT_LIMIT)
        .collect();
    let small = (h / 200).max(1);
    let mut id = Vec::new();
    text_rects(
        &label,
        inset,
        h - inset - 7 * small,
        small,
        width,
        height,
        &mut id,
    );
    push_group(&mut groups, id);

    groups
}

/// One seven-segment bar as a rectangle, `segment` indexed as
/// [`DIGIT_SEGMENTS`] documents.
fn segment_rect(segment: u32, x: i64, y: i64, w: i64, h: i64, t: i64) -> (i64, i64, i64, i64) {
    // The middle bar straddles the digit's centre line, which is what makes
    // the two halves the same height.
    let mid0 = y + h / 2 - t / 2;
    let mid1 = mid0 + t;
    match segment {
        0 => (x + t, y, x + w - t, y + t),         // A, top
        1 => (x + w - t, y + t, x + w, mid0),      // B, top right
        2 => (x + w - t, mid1, x + w, y + h - t),  // C, bottom right
        3 => (x + t, y + h - t, x + w - t, y + h), // D, bottom
        4 => (x, mid1, x + t, y + h - t),          // E, bottom left
        5 => (x, y + t, x + t, mid0),              // F, top left
        _ => (x + t, mid0, x + w - t, mid1),       // G, middle
    }
}

/// Clip a rectangle to the output and keep it if anything is left. Every
/// rect [`sync_rects`] returns goes through here, which is what lets
/// [`SyncRect`] be unsigned and lets both renderers index without bounds
/// checks of their own.
fn push_rect(out: &mut Vec<SyncRect>, x0: i64, y0: i64, x1: i64, y1: i64, width: u32, height: u32) {
    let x0 = x0.clamp(0, i64::from(width));
    let y0 = y0.clamp(0, i64::from(height));
    let x1 = x1.clamp(0, i64::from(width));
    let y1 = y1.clamp(0, i64::from(height));
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    out.push(SyncRect {
        x0: x0 as u32,
        y0: y0 as u32,
        x1: x1 as u32,
        y1: y1 as u32,
    });
}

/// Turn a feature's rectangles into a [`SyncGroup`] with the box enclosing
/// them. A feature that clipped away entirely contributes no group at all,
/// so the shader never walks an empty one.
fn push_group(groups: &mut Vec<SyncGroup>, rects: Vec<SyncRect>) {
    let Some(first) = rects.first().copied() else {
        return;
    };
    let mut bounds = first;
    for rect in &rects[1..] {
        bounds.x0 = bounds.x0.min(rect.x0);
        bounds.y0 = bounds.y0.min(rect.y0);
        bounds.x1 = bounds.x1.max(rect.x1);
        bounds.y1 = bounds.y1.max(rect.y1);
    }
    groups.push(SyncGroup { bounds, rects });
}

/// `message` in the 5x7 font of [`glyph`], as rectangles rather than pixels:
/// each row of each glyph becomes one rect per *run* of lit cells, so an `E`
/// costs seven rects instead of twenty-three set pixels. The merge is what
/// keeps a twelve-character line inside [`MAX_SYNC_RECTS`].
fn text_rects(
    message: &str,
    x: i64,
    y: i64,
    scale: i64,
    width: u32,
    height: u32,
    out: &mut Vec<SyncRect>,
) {
    let mut pen = x;
    for c in message.chars() {
        let rows = glyph(c.to_ascii_uppercase());
        for (row_index, row) in rows.iter().enumerate() {
            let mut run: Option<usize> = None;
            // The trailing space closes a run that reaches the last column.
            for (column, cell) in row.chars().chain(std::iter::once(' ')).enumerate() {
                if cell == '#' {
                    run.get_or_insert(column);
                } else if let Some(start) = run.take() {
                    let y0 = y + row_index as i64 * scale;
                    push_rect(
                        out,
                        pen + start as i64 * scale,
                        y0,
                        pen + column as i64 * scale,
                        y0 + scale,
                        width,
                        height,
                    );
                }
            }
        }
        pen += 6 * scale;
    }
}

/// The tiled path's stand-in for [`TestPattern::Sync`].
///
/// `suede blend` overlays are painted once per configure and never again
/// (see the `overlay` module), so a counter drawn here would
/// freeze on whatever number it started at — which looks exactly like two
/// projectors locked in step, the one answer this pattern must never give
/// by accident. It shows two dashes and says what to change instead.
fn sync_unavailable(rgb: &mut [u8], width: u32, height: u32) {
    let (w, h) = (i64::from(width), i64::from(height));
    let digit_h = ((h * 9) / 10).min((w * 9 / 10) * 10 / 11).max(1);
    let digit_w = (digit_h / 2).max(1);
    let gap = (digit_h / 10).max(1);
    let block_x = (w - (2 * digit_w + gap)) / 2;
    let block_y = (h - digit_h) / 2;
    let thickness = (digit_h / 8).max(1);
    let mut dashes = Vec::new();
    for index in 0..2 {
        let (x0, y0, x1, y1) = segment_rect(
            6,
            block_x + index * (digit_w + gap),
            block_y,
            digit_w,
            digit_h,
            thickness,
        );
        push_rect(&mut dashes, x0, y0, x1, y1, width, height);
    }
    for rect in &dashes {
        for y in rect.y0..rect.y1 {
            for x in rect.x0..rect.x1 {
                put(rgb, width, x as i32, y as i32, [255, 255, 255]);
            }
        }
    }
    let message = "SYNC NEEDS THE SLICER";
    let scale = ((width as i32 * 4 / 5) / (6 * message.len() as i32)).max(1);
    let text_x = (width as i32 - text_width(message, scale)) / 2;
    let text_y = (block_y + digit_h * 3 / 4) as i32;
    text(rgb, width, height, text_x, text_y, scale, message);
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

    // --- sync counter -----------------------------------------------------

    /// Where the ones digit of a `1920x1080` sync frame sits, worked out here
    /// from the same rules `sync_rects` documents rather than read back out
    /// of it — so a change to the layout has to be a deliberate one.
    fn ones_digit_box(width: u32, height: u32) -> (i64, i64, i64, i64, i64) {
        let (w, h) = (i64::from(width), i64::from(height));
        let digit_h = ((h * 9) / 10).min((w * 9 / 10) * 10 / 11).max(1);
        let digit_w = (digit_h / 2).max(1);
        let gap = (digit_h / 10).max(1);
        let block_x = (w - (2 * digit_w + gap)) / 2;
        let block_y = (h - digit_h) / 2;
        let thickness = (digit_h / 8).max(1);
        (
            block_x + digit_w + gap,
            block_y,
            digit_w,
            digit_h,
            thickness,
        )
    }

    fn lit(groups: &[SyncGroup], x: i64, y: i64) -> bool {
        if x < 0 || y < 0 {
            return false;
        }
        groups
            .iter()
            .any(|group| group.rects.iter().any(|r| r.contains(x as u32, y as u32)))
    }

    #[test]
    fn every_digit_lights_exactly_the_seven_segment_set_it_should() {
        // The classic truth table, written out here independently of
        // `DIGIT_SEGMENTS` and in reading order A..G, so this test is a
        // check on that constant and not a copy of it.
        let expected: [&str; 10] = [
            "ABCDEF",  // 0
            "BC",      // 1
            "ABDEG",   // 2
            "ABCDG",   // 3
            "BCFG",    // 4
            "ACDFG",   // 5
            "ACDEFG",  // 6
            "ABC",     // 7
            "ABCDEFG", // 8
            "ABCDFG",  // 9
        ];
        let (width, height) = (1920u32, 1080u32);
        let (x, y, w, h, t) = ones_digit_box(width, height);
        // One point per segment, at the middle of where that bar must lie.
        let probes = [
            ('A', x + w / 2, y + t / 2),
            ('B', x + w - t / 2, y + h / 4),
            ('C', x + w - t / 2, y + 3 * h / 4),
            ('D', x + w / 2, y + h - t / 2),
            ('E', x + t / 2, y + 3 * h / 4),
            ('F', x + t / 2, y + h / 4),
            ('G', x + w / 2, y + h / 2),
        ];
        for (digit, set) in expected.iter().enumerate() {
            let groups = sync_rects(width, height, digit as u32, "DP-1", 0);
            for (segment, px, py) in probes {
                assert_eq!(
                    lit(&groups, px, py),
                    set.contains(segment),
                    "digit {digit}, segment {segment} at ({px},{py})"
                );
            }
        }
    }

    #[test]
    fn the_binary_strip_reads_most_significant_bit_first() {
        let (width, height) = (1920u32, 1080u32);
        // Alternating bits, so a reversed strip fails as loudly as a
        // rotated one, plus a value whose top bit is set.
        for frame in [0xaaaau32, 0x5555, 0x8001, 0x0000, 0xffff] {
            let groups = sync_rects(width, height, frame, "DP-1", 0);
            let (w, h) = (i64::from(width), i64::from(height));
            let inset = (w / 100).max(2);
            let digit_h = ((h * 9) / 10).min((w * 9 / 10) * 10 / 11).max(1);
            let digit_w = (digit_h / 2).max(1);
            let gap = (digit_h / 10).max(1);
            let block_x = (w - (2 * digit_w + gap)) / 2;
            let block_y = (h - digit_h) / 2;
            let pitch = (digit_h / 16).max(2);
            let margin = (w - (block_x + 2 * digit_w + gap) - 2 * inset).max(2);
            let cell = pitch.min(margin).max(1);
            let strip_x = w - inset - cell;
            let strip_y = block_y + (digit_h - pitch * 16) / 2;
            for bit in 0..16i64 {
                // The centre of a cell is lit for a one and hollow for a
                // zero — the one sample that tells filled from outlined.
                let centre = (strip_x + cell / 2, strip_y + bit * pitch + cell / 2);
                let set = (frame >> (15 - bit as u32)) & 1 == 1;
                assert_eq!(
                    lit(&groups, centre.0, centre.1),
                    set,
                    "frame {frame:#06x}, strip row {bit}"
                );
            }
        }
    }

    #[test]
    fn the_counter_wraps_at_a_hundred_and_ignores_the_high_bits() {
        // 42 and 142 must be the same two digits, and differ only in the
        // strip — which is exactly what makes the strip worth drawing.
        let a = sync_rects(1920, 1080, 42, "DP-1", 7);
        let b = sync_rects(1920, 1080, 142, "DP-1", 7);
        assert_eq!(a[0], b[0], "the digits");
        assert_ne!(a[1], b[1], "the strip");
    }

    #[test]
    fn the_same_arguments_always_produce_the_same_rectangles() {
        let once = sync_rects(1920, 1080, 37, "HDMI-A-1", 1234);
        let twice = sync_rects(1920, 1080, 37, "HDMI-A-1", 1234);
        assert_eq!(once, twice);
        // And the output name is the only thing that varies between two
        // presenters in the same cycle: the counter itself must not.
        let other = sync_rects(1920, 1080, 37, "DP-2", 1234);
        assert_eq!(once[0], other[0], "the digits");
        assert_eq!(once[1], other[1], "the strip");
    }

    #[test]
    fn every_rectangle_lies_inside_the_output() {
        // Including sizes far outside anything a projector does, because
        // the renderers index their buffers with these numbers unchecked.
        for &(width, height) in &[
            (1920u32, 1080u32),
            (1280, 720),
            (3840, 2160),
            (1080, 1920),
            (64, 64),
            (17, 9),
            (1, 1),
        ] {
            for frame in [0u32, 1, 99, 100, 65535, u32::MAX] {
                for output in ["DP-1", "", "A-VERY-LONG-CONNECTOR-NAME"] {
                    let groups = sync_rects(width, height, frame, output, u64::MAX);
                    for group in &groups {
                        assert!(!group.rects.is_empty(), "{width}x{height}: empty group");
                        for rect in &group.rects {
                            assert!(rect.x0 < rect.x1 && rect.y0 < rect.y1, "{rect:?}");
                            assert!(
                                rect.x1 <= width && rect.y1 <= height,
                                "{rect:?} escapes {width}x{height}"
                            );
                            assert!(
                                rect.x0 >= group.bounds.x0
                                    && rect.y0 >= group.bounds.y0
                                    && rect.x1 <= group.bounds.x1
                                    && rect.y1 <= group.bounds.y1,
                                "{rect:?} escapes its group bounds {:?}",
                                group.bounds
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn sync_rects_stay_under_the_advertised_ceiling() {
        // `gpu.rs` allocates one fixed buffer per output from
        // `MAX_SYNC_RECTS`, so this is the constant's only guarantee.
        let mut worst = 0usize;
        for &(width, height) in &[(1920u32, 1080u32), (3840, 2160), (1280, 800)] {
            for frame in [0u32, 0x8888, 0xaaaa, u32::MAX, 88] {
                // `M` and `W` are the font's busiest glyphs — three runs in
                // a five-cell row — so a name of them is the worst case.
                for output in ["MWMWMWMWMWMWMWMW", "DP-1"] {
                    let count: usize = sync_rects(width, height, frame, output, u64::MAX)
                        .iter()
                        .map(|group| group.rects.len())
                        .sum();
                    worst = worst.max(count);
                    assert!(
                        count <= MAX_SYNC_RECTS,
                        "{count} rects at {width}x{height} exceeds {MAX_SYNC_RECTS}"
                    );
                }
            }
        }
        assert!(
            worst > 100,
            "the sweep never got near the ceiling ({worst})"
        );
    }

    #[test]
    fn the_tiled_fallback_says_so_instead_of_freezing_a_counter() {
        let rgb = render(
            1920,
            1080,
            &spec(
                TestPattern::Sync,
                Rect {
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1080,
                },
            ),
        );
        // The middle bar of the left dash is lit...
        let (x, y, w, h, t) = ones_digit_box(1920, 1080);
        let gap = (h / 10).max(1);
        let left = x - w - gap;
        assert_eq!(
            pixel(&rgb, 1920, (left + w / 2) as i32, (y + h / 2) as i32),
            [255, 255, 255]
        );
        // ...and the top bar, which every digit except 1 and 4 would light,
        // is not: this is a dash, not a frozen number.
        assert_eq!(
            pixel(&rgb, 1920, (left + w / 2) as i32, (y + t / 2) as i32),
            [0, 0, 0]
        );
    }

    /// Not a test: the sync counter's frames as PPMs, for the same reason
    /// `write_patterns` exists — the layout is a picture, and whether the
    /// name runs into the digits is not something an assertion sees.
    ///
    ///     SUEDE_WRITE_PATTERN=/tmp/p cargo test -- --ignored write_sync_frames
    #[test]
    #[ignore]
    fn write_sync_frames() {
        let Ok(dir) = std::env::var("SUEDE_WRITE_PATTERN") else {
            return;
        };
        for (name, w, h, frame) in [
            ("DP-1", 1920u32, 1080u32, 42u32),
            ("HDMI-A-1", 1920, 1080, 7),
            ("DP-2", 1280, 800, 99),
            ("DP-3", 1080, 1920, 3),
        ] {
            let mut rgb = vec![0u8; w as usize * h as usize * 3];
            for group in sync_rects(w, h, frame, name, 123_456) {
                for rect in group.rects {
                    for y in rect.y0..rect.y1 {
                        for x in rect.x0..rect.x1 {
                            put(&mut rgb, w, x as i32, y as i32, [255, 255, 255]);
                        }
                    }
                }
            }
            let mut out = format!("P6 {w} {h} 255 ").into_bytes();
            out.extend_from_slice(&rgb);
            let file = format!("{dir}-sync-{name}-{frame}.ppm").to_lowercase();
            std::fs::write(&file, out).unwrap();
            eprintln!("wrote {file}");
        }
    }
}
