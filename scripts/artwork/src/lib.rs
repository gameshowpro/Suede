//! Deterministic raster and browser artwork generation for Suede.

use image::{imageops::FilterType, ImageBuffer, ImageFormat, Rgba, RgbaImage};
use resvg::{tiny_skia, usvg};
use serde_json::json;
use std::{
    error::Error,
    fs,
    io::{self, Cursor},
    path::Path,
};

const CHARCOAL: [u8; 3] = [19, 21, 26];
const ALPHA_CROP_THRESHOLD: u8 = 8;

/// A generated file and its media type, suitable for embedding in a site or package.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Asset {
    pub name: String,
    pub mime: &'static str,
}

/// Generate the complete web artwork suite from an `artwork/` source directory.
pub fn generate_web(source_dir: &Path, out_dir: &Path) -> Result<Vec<Asset>, Box<dyn Error>> {
    generate_web_with_paths(source_dir, out_dir, WebPaths::site())
}

/// Generate the web suite and documentation display assets.
pub fn generate_docs(source_dir: &Path, out_dir: &Path) -> Result<Vec<Asset>, Box<dyn Error>> {
    let mut assets = generate_web_with_paths(source_dir, out_dir, WebPaths::docs())?;
    // Find and retain the soft-shadow bounds once; every target derives from this same frame.
    let native = cropped_visible(&load_native(source_dir)?);
    for (name, size) in [("icon-docs", 144), ("icon-readme", 384)] {
        let image = fit_transparent(&native, size, size)?;
        write_page_image(out_dir, name, &image, &mut assets)?;
    }
    Ok(assets)
}

#[derive(Clone, Copy)]
struct WebPaths {
    start_url: &'static str,
    scope: &'static str,
    id: &'static str,
    manifest_prefix: &'static str,
    browser_prefix: &'static str,
}

impl WebPaths {
    fn site() -> Self {
        Self {
            start_url: "/",
            scope: "/",
            id: "/",
            manifest_prefix: "",
            browser_prefix: "/assets/artwork/",
        }
    }

    fn docs() -> Self {
        Self {
            start_url: "../../",
            scope: "../../",
            id: "../../",
            manifest_prefix: "",
            browser_prefix: "",
        }
    }
}

fn generate_web_with_paths(
    source_dir: &Path,
    out_dir: &Path,
    paths: WebPaths,
) -> Result<Vec<Asset>, Box<dyn Error>> {
    fs::create_dir_all(out_dir)?;
    let native = cropped_visible(&load_native(source_dir)?);
    let favicon_tree = load_svg_tree(&source_dir.join("suede-small.svg"))?;
    let mut assets = Vec::new();

    // Header artwork is framed from the visible alpha bounds, while pixels in the
    // retained rectangle keep their original alpha, including the soft shadow.
    let header = fit_transparent(&native, 144, 144)?;
    write_page_image(out_dir, "icon-header", &header, &mut assets)?;

    copy_asset(
        source_dir,
        out_dir,
        "suede-small.svg",
        "favicon.svg",
        "image/svg+xml",
        &mut assets,
    )?;
    for size in [16, 24, 32, 48, 64, 96, 128, 256] {
        let icon = render_svg(&favicon_tree, size, size)?;
        write_png_if_changed(&out_dir.join(format!("favicon-{size}x{size}.png")), &icon)?;
        assets.push(asset(format!("favicon-{size}x{size}.png"), "image/png"));
    }
    let mut ico_frames = Vec::new();
    for size in [16, 24, 32, 48, 64, 128, 256] {
        ico_frames.push((size, render_svg(&favicon_tree, size, size)?));
    }
    write_if_changed(&out_dir.join("favicon.ico"), &encode_ico(&ico_frames)?)?;
    assets.push(asset("favicon.ico", "image/vnd.microsoft.icon"));

    copy_asset(
        source_dir,
        out_dir,
        "suede-mono.svg",
        "safari-pinned-tab.svg",
        "image/svg+xml",
        &mut assets,
    )?;

    for size in [57, 60, 72, 76, 114, 120, 144, 152, 167, 180] {
        let icon = fit_opaque(&native, size, size, 0.80)?;
        let name = format!("apple-touch-icon-{size}x{size}.png");
        write_png_if_changed(&out_dir.join(&name), &icon)?;
        assets.push(asset(name, "image/png"));
    }
    let apple_default = fit_opaque(&native, 180, 180, 0.80)?;
    write_png_if_changed(&out_dir.join("apple-touch-icon.png"), &apple_default)?;
    assets.push(asset("apple-touch-icon.png", "image/png"));

    for size in [192, 256, 384, 512, 1024] {
        let icon = fit_transparent(&native, size, size)?;
        let name = format!("android-chrome-{size}x{size}.png");
        write_png_if_changed(&out_dir.join(&name), &icon)?;
        assets.push(asset(name, "image/png"));
    }
    for size in [192, 512, 1024] {
        let icon = fit_maskable(&native, size)?;
        let name = format!("android-chrome-maskable-{size}x{size}.png");
        write_png_if_changed(&out_dir.join(&name), &icon)?;
        assets.push(asset(name, "image/png"));
    }

    for size in [70, 144, 150, 310] {
        let image = fit_opaque(&native, size, size, 0.78)?;
        let name = format!("mstile-{size}x{size}.png");
        write_png_if_changed(&out_dir.join(&name), &image)?;
        assets.push(asset(name, "image/png"));
    }
    let wide = fit_opaque(&native, 310, 150, 0.78)?;
    write_png_if_changed(&out_dir.join("mstile-310x150.png"), &wide)?;
    assets.push(asset("mstile-310x150.png", "image/png"));

    let manifest = web_manifest(paths);
    write_if_changed(&out_dir.join("site.webmanifest"), manifest.as_bytes())?;
    assets.push(asset("site.webmanifest", "application/manifest+json"));
    let browserconfig = browserconfig(paths);
    write_if_changed(&out_dir.join("browserconfig.xml"), browserconfig.as_bytes())?;
    assets.push(asset("browserconfig.xml", "application/xml"));

    let social = social_card(&native)?;
    write_png_if_changed(&out_dir.join("social-card.png"), &social)?;
    assets.push(asset("social-card.png", "image/png"));
    Ok(assets)
}

fn asset(name: impl Into<String>, mime: &'static str) -> Asset {
    Asset {
        name: name.into(),
        mime,
    }
}

fn load_native(source_dir: &Path) -> Result<RgbaImage, Box<dyn Error>> {
    let path = source_dir.join("suede.png");
    let image = image::open(&path).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("could not read {}: {error}", path.display()),
        )
    })?;
    Ok(image.into_rgba8())
}

fn load_svg_tree(path: &Path) -> Result<usvg::Tree, Box<dyn Error>> {
    let data = fs::read(path)?;
    Ok(usvg::Tree::from_data(&data, &usvg::Options::default())?)
}

fn render_svg(tree: &usvg::Tree, width: u32, height: u32) -> Result<RgbaImage, Box<dyn Error>> {
    let source_width = tree.size().width();
    let source_height = tree.size().height();
    let scale = (width as f32 / source_width).min(height as f32 / source_height);
    let mut pixmap = tiny_skia::Pixmap::new(width, height)
        .ok_or_else(|| io::Error::other("could not allocate SVG raster buffer"))?;
    let x = (width as f32 - source_width * scale) / 2.0;
    let y = (height as f32 - source_height * scale) / 2.0;
    let transform = tiny_skia::Transform::from_scale(scale, scale).post_translate(x, y);
    resvg::render(tree, transform, &mut pixmap.as_mut());
    let mut rgba = RgbaImage::from_raw(width, height, pixmap.data().to_vec())
        .ok_or_else(|| io::Error::other("SVG renderer returned an invalid buffer"))?;
    // tiny-skia stores premultiplied alpha; PNG stores straight alpha.
    for pixel in rgba.pixels_mut() {
        let alpha = pixel[3] as u16;
        if alpha > 0 && alpha < 255 {
            for channel in 0..3 {
                pixel[channel] = ((pixel[channel] as u16 * 255 + alpha / 2) / alpha).min(255) as u8;
            }
        }
    }
    Ok(rgba)
}

fn visible_bounds(image: &RgbaImage, threshold: u8) -> Option<(u32, u32, u32, u32)> {
    let mut min_x = image.width();
    let mut min_y = image.height();
    let mut max_x = 0;
    let mut max_y = 0;
    let mut found = false;
    for (x, y, pixel) in image.enumerate_pixels() {
        if pixel[3] >= threshold {
            found = true;
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
        }
    }
    found.then_some((min_x, min_y, max_x + 1, max_y + 1))
}

fn cropped_visible(image: &RgbaImage) -> RgbaImage {
    let Some((x0, y0, x1, y1)) = visible_bounds(image, ALPHA_CROP_THRESHOLD) else {
        return image.clone();
    };
    // Retain low-alpha shadow pixels around the alpha>=8 framing bounds.
    let padding = (((x1 - x0).max(y1 - y0) as f32 * 0.08).ceil() as u32).max(2);
    let left = x0.saturating_sub(padding);
    let top = y0.saturating_sub(padding);
    let right = x1.saturating_add(padding).min(image.width());
    let bottom = y1.saturating_add(padding).min(image.height());
    image::imageops::crop_imm(image, left, top, right - left, bottom - top).to_image()
}

fn resize_premultiplied(image: &RgbaImage, width: u32, height: u32) -> RgbaImage {
    let premultiplied = ImageBuffer::from_fn(image.width(), image.height(), |x, y| {
        let pixel = image.get_pixel(x, y);
        let alpha = pixel[3] as u16;
        Rgba([
            pixel[0] as u16 * alpha,
            pixel[1] as u16 * alpha,
            pixel[2] as u16 * alpha,
            alpha * 255,
        ])
    });
    let resized = image::imageops::resize(&premultiplied, width, height, FilterType::Lanczos3);
    RgbaImage::from_fn(width, height, |x, y| {
        let pixel = resized.get_pixel(x, y);
        let alpha = pixel[3].min(65_025);
        if alpha == 0 {
            return Rgba([0, 0, 0, 0]);
        }
        let unpremultiply = |channel: u16| {
            ((channel as u32 * 255 + alpha as u32 / 2) / alpha as u32).min(255) as u8
        };
        let alpha8 = ((alpha as u32 * 255 + 32_512) / 65_025).min(255) as u8;
        Rgba([
            unpremultiply(pixel[0]),
            unpremultiply(pixel[1]),
            unpremultiply(pixel[2]),
            alpha8,
        ])
    })
}

fn resize_contained(image: &RgbaImage, width: u32, height: u32) -> RgbaImage {
    let factor = (width as f64 / image.width() as f64).min(height as f64 / image.height() as f64);
    let factor = factor.min(1.0);
    let resized_width = (image.width() as f64 * factor).round().max(1.0) as u32;
    let resized_height = (image.height() as f64 * factor).round().max(1.0) as u32;
    resize_premultiplied(image, resized_width, resized_height)
}

fn fit_transparent(
    image: &RgbaImage,
    width: u32,
    height: u32,
) -> Result<RgbaImage, Box<dyn Error>> {
    let margin = 0.90;
    let resized = resize_contained(
        image,
        ((width as f32 * margin).round() as u32).max(1),
        ((height as f32 * margin).round() as u32).max(1),
    );
    let mut output = RgbaImage::new(width, height);
    image::imageops::overlay(
        &mut output,
        &resized,
        ((width - resized.width()) / 2) as i64,
        ((height - resized.height()) / 2) as i64,
    );
    Ok(output)
}

fn fit_opaque(
    image: &RgbaImage,
    width: u32,
    height: u32,
    occupancy: f32,
) -> Result<RgbaImage, Box<dyn Error>> {
    let resized = resize_contained(
        image,
        (width as f32 * occupancy).round() as u32,
        (height as f32 * occupancy).round() as u32,
    );
    let mut output = solid_image(width, height, CHARCOAL);
    composite_centered(&mut output, &resized);
    Ok(output)
}

fn fit_maskable(image: &RgbaImage, size: u32) -> Result<RgbaImage, Box<dyn Error>> {
    // Keep all four corners of the artwork rectangle within radius 0.37 * size.
    let box_side = ((size as f32 * 0.37 * 2.0_f32.sqrt()).floor() as u32).max(1);
    let resized = resize_contained(image, box_side, box_side);
    let mut output = solid_image(size, size, CHARCOAL);
    composite_centered(&mut output, &resized);
    Ok(output)
}

fn social_card(image: &RgbaImage) -> Result<RgbaImage, Box<dyn Error>> {
    let resized = resize_contained(image, 512, 512);
    let mut output = solid_image(1200, 630, CHARCOAL);
    composite_centered(&mut output, &resized);
    Ok(output)
}

fn solid_image(width: u32, height: u32, color: [u8; 3]) -> RgbaImage {
    RgbaImage::from_pixel(width, height, Rgba([color[0], color[1], color[2], 255]))
}

fn composite_centered(output: &mut RgbaImage, foreground: &RgbaImage) {
    let x = (output.width() - foreground.width()) / 2;
    let y = (output.height() - foreground.height()) / 2;
    image::imageops::overlay(output, foreground, x as i64, y as i64);
    for pixel in output.pixels_mut() {
        pixel[3] = 255;
    }
}

fn write_png_if_changed(path: &Path, image: &RgbaImage) -> Result<(), Box<dyn Error>> {
    let mut bytes = Cursor::new(Vec::new());
    image.write_to(&mut bytes, ImageFormat::Png)?;
    write_if_changed(path, &bytes.into_inner())
}

/// Page images favor AVIF; PNG remains available for fallback and README clients.
fn write_page_image(
    out_dir: &Path,
    name: &str,
    image: &RgbaImage,
    assets: &mut Vec<Asset>,
) -> Result<(), Box<dyn Error>> {
    let png_name = format!("{name}.png");
    write_png_if_changed(&out_dir.join(&png_name), image)?;
    assets.push(asset(png_name, "image/png"));

    let pixels: Vec<_> = image
        .pixels()
        .map(|p| ravif::RGBA8::new(p[0], p[1], p[2], p[3]))
        .collect();
    // ravif uses 4:4:4 color, avoiding subsampling at the blue/white boundaries.
    // Preserve the alpha channel at maximum quality for the soft shadow.
    let encoded = ravif::Encoder::new()
        .with_quality(80.0)
        .with_alpha_quality(100.0)
        .with_speed(6)
        .encode_rgba(ravif::Img::new(
            pixels.as_slice(),
            image.width() as usize,
            image.height() as usize,
        ))?;
    let avif_name = format!("{name}.avif");
    write_if_changed(&out_dir.join(&avif_name), &encoded.avif_file)?;
    assets.push(asset(avif_name, "image/avif"));
    Ok(())
}

fn write_if_changed(path: &Path, bytes: &[u8]) -> Result<(), Box<dyn Error>> {
    if fs::read(path).ok().as_deref() == Some(bytes) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, bytes)?;
    Ok(())
}

fn copy_asset(
    source_dir: &Path,
    out_dir: &Path,
    source_name: &str,
    name: &str,
    mime: &'static str,
    assets: &mut Vec<Asset>,
) -> Result<(), Box<dyn Error>> {
    write_if_changed(
        &out_dir.join(name),
        &fs::read(source_dir.join(source_name))?,
    )?;
    assets.push(asset(name, mime));
    Ok(())
}

fn encode_ico(frames: &[(u32, RgbaImage)]) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut pngs = Vec::with_capacity(frames.len());
    for (_, frame) in frames {
        let mut cursor = Cursor::new(Vec::new());
        frame.write_to(&mut cursor, ImageFormat::Png)?;
        pngs.push(cursor.into_inner());
    }
    let directory_bytes = 6 + frames.len() * 16;
    let mut offset = directory_bytes as u32;
    let mut output = Vec::with_capacity(directory_bytes + pngs.iter().map(Vec::len).sum::<usize>());
    output.extend_from_slice(&0u16.to_le_bytes());
    output.extend_from_slice(&1u16.to_le_bytes());
    output.extend_from_slice(&(frames.len() as u16).to_le_bytes());
    for ((size, _), png) in frames.iter().zip(&pngs) {
        output.push(if *size == 256 { 0 } else { *size as u8 });
        output.push(if *size == 256 { 0 } else { *size as u8 });
        output.push(0);
        output.push(0);
        output.extend_from_slice(&1u16.to_le_bytes());
        output.extend_from_slice(&32u16.to_le_bytes());
        output.extend_from_slice(&(png.len() as u32).to_le_bytes());
        output.extend_from_slice(&offset.to_le_bytes());
        offset += png.len() as u32;
    }
    for png in pngs {
        output.extend_from_slice(&png);
    }
    Ok(output)
}

fn web_manifest(paths: WebPaths) -> String {
    let icons = [192, 256, 384, 512, 1024]
        .into_iter()
        .map(|size| json!({
            "src": format!("{}android-chrome-{size}x{size}.png", paths.manifest_prefix),
            "sizes": format!("{size}x{size}"),
            "type": "image/png",
            "purpose": "any"
        }))
        .chain([192, 512, 1024].into_iter().map(|size| json!({
            "src": format!("{}android-chrome-maskable-{size}x{size}.png", paths.manifest_prefix),
            "sizes": format!("{size}x{size}"),
            "type": "image/png",
            "purpose": "maskable"
        })))
        .collect::<Vec<_>>();
    let manifest = json!({
        "name": "Suede",
        "short_name": "Suede",
        "id": paths.id,
        "start_url": paths.start_url,
        "scope": paths.scope,
        "display": "standalone",
        "background_color": format!("#{:02x}{:02x}{:02x}", CHARCOAL[0], CHARCOAL[1], CHARCOAL[2]),
        "theme_color": format!("#{:02x}{:02x}{:02x}", CHARCOAL[0], CHARCOAL[1], CHARCOAL[2]),
        "icons": icons
    });
    format!(
        "{}\n",
        serde_json::to_string_pretty(&manifest).expect("manifest is JSON serializable")
    )
}

fn browserconfig(paths: WebPaths) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<browserconfig>\n  <msapplication>\n    <tile>\n      <square70x70logo src=\"{}mstile-70x70.png\" />\n      <square150x150logo src=\"{}mstile-150x150.png\" />\n      <square310x310logo src=\"{}mstile-310x310.png\" />\n      <wide310x150logo src=\"{}mstile-310x150.png\" />\n      <TileColor>#{:02x}{:02x}{:02x}</TileColor>\n    </tile>\n  </msapplication>\n</browserconfig>\n",
        paths.browser_prefix, paths.browser_prefix, paths.browser_prefix, paths.browser_prefix,
        CHARCOAL[0], CHARCOAL[1], CHARCOAL[2]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::PathBuf, time::SystemTime};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "suede-artwork-{label}-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn fixtures(path: &Path) {
        fs::create_dir_all(path).unwrap();
        let mut native = RgbaImage::new(512, 512);
        for y in 64..448 {
            for x in 64..448 {
                let dx = (130i32 - x as i32).max(0).max(x as i32 - 382).max(0);
                let dy = (110i32 - y as i32).max(0).max(y as i32 - 402).max(0);
                let distance = dx.max(dy);
                if distance == 0 {
                    native.put_pixel(x, y, Rgba([180, 40, 90, 255]));
                } else if distance <= 20 {
                    let alpha = (7.0 - distance as f32 / 8.0).max(1.0) as u8;
                    native.put_pixel(x, y, Rgba([10, 12, 18, alpha]));
                }
            }
        }
        // A transparent speck below the framing threshold must not pull the crop.
        native.put_pixel(4, 6, Rgba([255, 255, 255, 7]));
        native.save(path.join("suede.png")).unwrap();
        fs::write(path.join("suede-small.svg"), br##"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><rect x="1" y="1" width="8" height="8" fill="#d34"/></svg>"##).unwrap();
        fs::write(
            path.join("suede-mono.svg"),
            b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>",
        )
        .unwrap();
    }

    #[test]
    fn outputs_have_expected_dimensions_and_maskable_safe_area() {
        let dirs = TempDir::new("web-suite");
        let source = dirs.0.join("source");
        let output = dirs.0.join("output");
        fixtures(&source);
        let assets = generate_web(&source, &output).unwrap();
        let header = image::open(output.join("icon-header.png"))
            .unwrap()
            .into_rgba8();
        assert_eq!((header.width(), header.height()), (144, 144));
        assert!(header.pixels().any(|p| p[3] == 0));
        assert!(header.pixels().any(|p| p[3] == 255));

        // Threshold-8 framing ignores the fixture speck and the padding preserves its soft shadow.
        let visible = cropped_visible(&image::open(source.join("suede.png")).unwrap().into_rgba8());
        assert!(visible.width() < 512 && visible.height() < 512);
        assert!(visible.pixels().any(|pixel| (1..8).contains(&pixel[3])));
        assert_eq!(visible.get_pixel(4, 114)[3], 4, "outer shadow was clipped");
        assert!(!assets.iter().any(|asset| asset.name == "suede-small.svg"));

        for size in [16, 24, 32, 48, 64, 96, 128, 256] {
            let icon = image::open(output.join(format!("favicon-{size}x{size}.png"))).unwrap();
            assert_eq!((icon.width(), icon.height()), (size, size));
        }
        for size in [57, 60, 72, 76, 114, 120, 144, 152, 167, 180] {
            let icon =
                image::open(output.join(format!("apple-touch-icon-{size}x{size}.png"))).unwrap();
            assert_eq!((icon.width(), icon.height()), (size, size));
        }
        for size in [192, 256, 384, 512, 1024] {
            let icon =
                image::open(output.join(format!("android-chrome-{size}x{size}.png"))).unwrap();
            assert_eq!((icon.width(), icon.height()), (size, size));
        }
        for size in [192, 512, 1024] {
            let icon =
                image::open(output.join(format!("android-chrome-maskable-{size}x{size}.png")))
                    .unwrap()
                    .into_rgba8();
            assert_eq!((icon.width(), icon.height()), (size, size));
            assert_eq!(
                icon.get_pixel(0, 0).0,
                [CHARCOAL[0], CHARCOAL[1], CHARCOAL[2], 255]
            );
            assert!(icon.pixels().all(|pixel| pixel[3] == 255));
            let mut min_x = size;
            let mut min_y = size;
            let mut max_x = 0;
            let mut max_y = 0;
            for (x, y, pixel) in icon.enumerate_pixels() {
                if pixel.0[..3] != CHARCOAL {
                    min_x = min_x.min(x);
                    min_y = min_y.min(y);
                    max_x = max_x.max(x);
                    max_y = max_y.max(y);
                }
            }
            let center = size as f64 / 2.0;
            let edge_x = (min_x as f64 - center)
                .abs()
                .max((max_x as f64 + 1.0 - center).abs());
            let edge_y = (min_y as f64 - center)
                .abs()
                .max((max_y as f64 + 1.0 - center).abs());
            assert!(edge_x.hypot(edge_y) <= 0.37 * size as f64 + 1.0);
            assert!((min_x + max_x + 1).abs_diff(size) <= 2);
            assert!((min_y + max_y + 1).abs_diff(size) <= 2);
        }
        for (name, width, height) in [
            ("mstile-70x70.png", 70, 70),
            ("mstile-144x144.png", 144, 144),
            ("mstile-150x150.png", 150, 150),
            ("mstile-310x310.png", 310, 310),
            ("mstile-310x150.png", 310, 150),
            ("social-card.png", 1200, 630),
        ] {
            let image = image::open(output.join(name)).unwrap();
            assert_eq!((image.width(), image.height()), (width, height));
        }
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(output.join("site.webmanifest")).unwrap()).unwrap();
        for icon in manifest["icons"].as_array().unwrap() {
            assert!(output.join(icon["src"].as_str().unwrap()).exists());
        }
        assert!(output.join("browserconfig.xml").exists());
        let ico = fs::read(output.join("favicon.ico")).unwrap();
        assert_eq!(u16::from_le_bytes([ico[4], ico[5]]), 7);
        for (index, size) in [16u8, 24, 32, 48, 64, 128, 0].into_iter().enumerate() {
            assert_eq!(ico[6 + index * 16], size);
            assert_eq!(ico[7 + index * 16], size);
        }
        let browser = fs::read_to_string(output.join("browserconfig.xml")).unwrap();
        assert!(!browser.contains("square144x144logo"));
    }

    #[test]
    fn docs_paths_are_relative_and_include_shared_readme_icon() {
        let dirs = TempDir::new("docs");
        let source = dirs.0.join("source");
        let output = dirs.0.join("output");
        fixtures(&source);
        generate_docs(&source, &output).unwrap();
        assert!(output.join("icon-readme.png").exists());
        let manifest = fs::read_to_string(output.join("site.webmanifest")).unwrap();
        assert!(manifest.contains("\"start_url\": \"../../\""));
        assert!(manifest.contains("\"scope\": \"../../\""));
        let browser = fs::read_to_string(output.join("browserconfig.xml")).unwrap();
        assert!(browser.contains("mstile-70x70.png"));
        assert!(!browser.contains("/assets/"));
        let docs_icon = image::open(output.join("icon-docs.png")).unwrap();
        let readme_icon = image::open(output.join("icon-readme.png")).unwrap();
        assert_eq!((docs_icon.width(), docs_icon.height()), (144, 144));
        assert_eq!((readme_icon.width(), readme_icon.height()), (384, 384));
    }

    #[test]
    fn resizing_premultiplies_alpha_and_never_upscales() {
        let mut edge = RgbaImage::new(40, 40);
        for y in 10..30 {
            for x in 10..30 {
                edge.put_pixel(x, y, Rgba([240, 30, 45, 255]));
            }
        }
        for y in 8..32 {
            for x in 8..32 {
                if edge.get_pixel(x, y)[3] == 0 {
                    edge.put_pixel(x, y, Rgba([240, 30, 45, 4]));
                }
            }
        }
        let output = fit_transparent(&edge, 96, 96).unwrap();
        // Ignore the 1-7 alpha shadow when measuring the main artwork bounds.
        let bounds = visible_bounds(&output, 8).unwrap();
        assert!(bounds.2 - bounds.0 <= 40);
        assert!(bounds.3 - bounds.1 <= 40);
        assert!(output.pixels().any(|pixel| pixel[3] > 0 && pixel[3] < 8));
        let reduced = resize_premultiplied(&edge, 12, 12);
        for pixel in reduced.pixels().filter(|pixel| pixel[3] > 0) {
            assert!(pixel[0] >= 230, "dark fringe in {pixel:?}");
        }
    }
}
