use crate::error::{BatteryTrayError, Result};
use image::codecs::ico::IcoEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, ImageEncoder, ImageReader, Rgba, RgbaImage};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// Maximum custom image file size (20 MiB).
pub const MAX_FILE_BYTES: u64 = 20 * 1024 * 1024;
/// Maximum image width or height.
pub const MAX_DIMENSION: u32 = 4096;
/// Maximum decoded pixel count.
pub const MAX_PIXELS: u64 = 4_000_000;
/// High-battery color.
pub const COLOR_GREEN: [u8; 3] = [34, 197, 94];
/// Medium-battery color.
pub const COLOR_YELLOW: [u8; 3] = [234, 179, 8];
/// Low-battery color.
pub const COLOR_RED: [u8; 3] = [239, 68, 68];
/// Unknown-battery color.
pub const COLOR_GRAY: [u8; 3] = [128, 128, 128];

/// Returns the battery color for one percentage.
///
/// Green above 60 %, yellow from 20 % up to 60 %, red below 20 %. The tray bar
/// and the panel percentage share this mapping so both read the same at a
/// glance.
pub fn percent_color(percent: Option<u8>) -> [u8; 3] {
    match percent {
        None => COLOR_GRAY,
        Some(value) if value > 60 => COLOR_GREEN,
        Some(value) if value >= 20 => COLOR_YELLOW,
        Some(_) => COLOR_RED,
    }
}

fn check_dimensions(width: u32, height: u32) -> Result<()> {
    if width == 0 || height == 0 {
        return Err(BatteryTrayError::Invalid("图片尺寸无效".to_owned()));
    }
    if width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err(BatteryTrayError::Invalid(format!(
            "图片单边不能超过 {MAX_DIMENSION} 像素"
        )));
    }
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(BatteryTrayError::Invalid(format!(
            "图片像素总数不能超过 {MAX_PIXELS}"
        )));
    }
    Ok(())
}

/// Validates format, size and decodability of a custom PNG/ICO/BMP/SVG image.
pub fn validate_image_file(path: &Path) -> Result<()> {
    let extension = path
        .extension()
        .and_then(|item| item.to_str())
        .unwrap_or("")
        .to_lowercase();
    if !matches!(extension.as_str(), "png" | "ico" | "bmp" | "svg") {
        return Err(BatteryTrayError::Invalid(
            "图标必须是 PNG / ICO / BMP / SVG".to_owned(),
        ));
    }
    // One validation path: `load_image` also enforces size and decode limits,
    // so a file replaced after this check cannot cause a huge allocation.
    load_image(path).map(|_| ())
}

fn load_image(path: &Path) -> Result<RgbaImage> {
    let metadata = fs::metadata(path)?;
    if metadata.len() == 0 {
        return Err(BatteryTrayError::Invalid("图片文件为空".to_owned()));
    }
    if metadata.len() > MAX_FILE_BYTES {
        return Err(BatteryTrayError::Invalid(format!(
            "图片文件不能超过 {} MiB",
            MAX_FILE_BYTES / (1024 * 1024)
        )));
    }
    let bytes = fs::read(path)?;
    let is_svg = path
        .extension()
        .and_then(|item| item.to_str())
        .is_some_and(|item| item.eq_ignore_ascii_case("svg"));
    if is_svg {
        return load_svg(&bytes);
    }
    load_raster(&bytes)
}

fn load_raster(bytes: &[u8]) -> Result<RgbaImage> {
    // Bound width, height and total allocation before the decoder runs so a
    // decompression bomb cannot exhaust memory.
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_PIXELS.saturating_mul(4).saturating_add(1024 * 1024));
    let mut reader = ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?;
    reader.limits(limits);
    let image = reader.decode()?;
    check_dimensions(image.width(), image.height())?;
    Ok(image.to_rgba8())
}

/// Rasterizes an SVG document into straight-alpha RGBA pixels.
///
/// `usvg::Tree::from_data` also transparently decompresses gzip-compressed
/// SVGZ payloads, so a `.svg` file holding compressed data works as well.
/// The document is parsed with `usvg` and drawn by `resvg`. External file
/// references are deliberately disabled, so a custom icon stays self-contained
/// and cannot read arbitrary files from the user's disk.
///
/// Rasterizer features are trimmed in `Cargo.toml` to keep the binary small:
/// no text shaping and no raster image decoding, so `<text>` and `<image>`
/// elements are not drawn. Geometry, gradients and clip paths all render.
fn load_svg(bytes: &[u8]) -> Result<RgbaImage> {
    let options = resvg::usvg::Options {
        image_href_resolver: resvg::usvg::ImageHrefResolver {
            resolve_data: resvg::usvg::ImageHrefResolver::default_data_resolver(),
            resolve_string: Box::new(|_, _| None),
        },
        ..Default::default()
    };
    let tree = resvg::usvg::Tree::from_data(bytes, &options)
        .map_err(|error| BatteryTrayError::Invalid(format!("SVG 无法解析：{error}")))?;

    // The SVG's intrinsic size can be fractional or huge. Rasterize the long
    // edge at 512 px (ample for tray icons up to 256 with 2x headroom), then
    // shrink further if MAX_PIXELS would be exceeded. A uniform scale keeps the
    // aspect ratio intact; `custom_icon_rgba` downsizes the result afterwards.
    let source = tree.size();
    let source_width = source.width();
    let source_height = source.height();
    if !source_width.is_finite()
        || !source_height.is_finite()
        || source_width <= 0.0
        || source_height <= 0.0
    {
        return Err(BatteryTrayError::Invalid("SVG 尺寸无效".to_owned()));
    }
    let long_side = f64::from(source_width.max(source_height));
    let short_side = f64::from(source_width.min(source_height));
    let target_long = 512.0_f64.min(f64::from(MAX_DIMENSION));
    let area_at_target = target_long * target_long * (short_side / long_side);
    let fit = (MAX_PIXELS as f64 / area_at_target).sqrt().min(1.0);
    let scale = target_long * fit / long_side;
    let width = (f64::from(source_width) * scale).floor().max(1.0) as u32;
    let height = (f64::from(source_height) * scale).floor().max(1.0) as u32;
    check_dimensions(width, height)?;

    let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height)
        .ok_or_else(|| BatteryTrayError::Invalid("SVG 无法生成位图".to_owned()))?;
    let transform = resvg::tiny_skia::Transform::from_scale(scale as f32, scale as f32);
    resvg::render(&tree, transform, &mut pixmap.as_mut());

    // tiny-skia stores premultiplied RGBA; image::RgbaImage expects straight
    // alpha, so undo the premultiplication the same way tiny-skia's PNG writer
    // does.
    let mut rgba = pixmap.take();
    for pixel in rgba.chunks_exact_mut(4) {
        let alpha = pixel[3];
        if alpha != 255 {
            let a = f64::from(alpha) / 255.0;
            for channel in &mut pixel[..3] {
                *channel = (f64::from(*channel) / a + 0.5) as u8;
            }
        }
    }
    RgbaImage::from_raw(width, height, rgba)
        .ok_or_else(|| BatteryTrayError::Invalid("SVG 无法生成位图".to_owned()))
}

#[derive(Clone)]
struct Coverage {
    size: usize,
    scale: usize,
    values: Vec<u8>,
}

impl Coverage {
    fn new(size: usize, scale: usize) -> Self {
        let samples = size * scale;
        Self {
            size,
            scale,
            values: vec![0; samples * samples],
        }
    }

    fn set_span(&mut self, row: i32, x0: f64, x1: f64, value: u8) {
        let samples = self.size * self.scale;
        if row < 0 || row >= samples as i32 {
            return;
        }
        let start = ((x0 * self.scale as f64 - 0.5).ceil() as i32).clamp(0, samples as i32);
        let end = ((x1 * self.scale as f64 - 0.5).ceil() as i32).clamp(0, samples as i32);
        if end <= start {
            return;
        }
        let offset = row as usize * samples;
        for x in start..end {
            self.values[offset + x as usize] = value;
        }
    }

    fn rounded_rect(
        &mut self,
        cx: f64,
        cy: f64,
        width: f64,
        height: f64,
        radius: f64,
        erase: bool,
    ) {
        if width <= 0.0 || height <= 0.0 {
            return;
        }
        let left = cx - width / 2.0;
        let right = cx + width / 2.0;
        let top = cy - height / 2.0;
        let bottom = cy + height / 2.0;
        let radius = radius.max(0.0).min(width / 2.0).min(height / 2.0);
        let start = ((top * self.scale as f64 - 0.5).ceil() as i32).max(0);
        let end =
            ((bottom * self.scale as f64 - 0.5).ceil() as i32).min((self.size * self.scale) as i32);
        for row in start..end {
            let y = (row as f64 + 0.5) / self.scale as f64;
            let inset = if y < top + radius {
                let dy = top + radius - y;
                radius - (radius * radius - dy * dy).max(0.0).sqrt()
            } else if y > bottom - radius {
                let dy = y - (bottom - radius);
                radius - (radius * radius - dy * dy).max(0.0).sqrt()
            } else {
                0.0
            };
            self.set_span(row, left + inset, right - inset, u8::from(!erase));
        }
    }

    fn upper_ring(&mut self, cx: f64, cy: f64, outer: f64, inner: f64) {
        let start = (((cy - outer) * self.scale as f64 - 0.5).ceil() as i32).max(0);
        let end =
            ((cy * self.scale as f64 - 0.5).ceil() as i32).min((self.size * self.scale) as i32);
        for row in start..end {
            let y = (row as f64 + 0.5) / self.scale as f64;
            let dy = cy - y;
            if !(0.0..=outer).contains(&dy) {
                continue;
            }
            let outer_half = (outer * outer - dy * dy).max(0.0).sqrt();
            if dy < inner {
                let inner_half = (inner * inner - dy * dy).max(0.0).sqrt();
                self.set_span(row, cx - outer_half, cx - inner_half, 1);
                self.set_span(row, cx + inner_half, cx + outer_half, 1);
            } else {
                self.set_span(row, cx - outer_half, cx + outer_half, 1);
            }
        }
    }

    fn polygon(&mut self, points: &[(f64, f64)]) {
        if points.len() < 3 {
            return;
        }
        let min_y = points
            .iter()
            .map(|point| point.1)
            .fold(f64::INFINITY, f64::min);
        let max_y = points
            .iter()
            .map(|point| point.1)
            .fold(f64::NEG_INFINITY, f64::max);
        let start = ((min_y * self.scale as f64 - 0.5).ceil() as i32).max(0);
        let end =
            ((max_y * self.scale as f64 - 0.5).ceil() as i32).min((self.size * self.scale) as i32);
        for row in start..end {
            let y = (row as f64 + 0.5) / self.scale as f64;
            let mut crossings = Vec::new();
            for index in 0..points.len() {
                let (x1, y1) = points[index];
                let (x2, y2) = points[(index + 1) % points.len()];
                if (y1 > y) != (y2 > y) {
                    crossings.push(x1 + (y - y1) * (x2 - x1) / (y2 - y1));
                }
            }
            crossings.sort_by(f64::total_cmp);
            for pair in crossings.chunks_exact(2) {
                self.set_span(row, pair[0], pair[1], 1);
            }
        }
    }

    fn alpha(&self, x: usize, y: usize) -> f64 {
        let mut count = 0_u32;
        for sy in 0..self.scale {
            for sx in 0..self.scale {
                count += u32::from(
                    self.values
                        [(y * self.scale + sy) * self.size * self.scale + x * self.scale + sx],
                );
            }
        }
        f64::from(count) / (self.scale * self.scale) as f64
    }
}

fn overlay(canvas: &mut RgbaImage, coverage: &Coverage, color: [u8; 3], opacity: f64) {
    for y in 0..canvas.height() as usize {
        for x in 0..canvas.width() as usize {
            let source_alpha = (coverage.alpha(x, y) * opacity).clamp(0.0, 1.0);
            if source_alpha <= 0.0 {
                continue;
            }
            let pixel = canvas.get_pixel_mut(x as u32, y as u32);
            let destination_alpha = f64::from(pixel[3]) / 255.0;
            let output_alpha = source_alpha + destination_alpha * (1.0 - source_alpha);
            if output_alpha <= 0.0 {
                continue;
            }
            let source_weight = source_alpha / output_alpha;
            let destination_weight = destination_alpha * (1.0 - source_alpha) / output_alpha;
            for channel in 0..3 {
                pixel[channel] = (f64::from(color[channel]) * source_weight
                    + f64::from(pixel[channel]) * destination_weight)
                    .round() as u8;
            }
            pixel[3] = (output_alpha * 255.0).round() as u8;
        }
    }
}

fn draw_shape(coverage: &mut Coverage, kind: &str, size: usize, glyph_top: f64, glyph_height: f64) {
    let cx = size as f64 / 2.0;
    match kind {
        "headphone" => {
            coverage.upper_ring(
                cx,
                glyph_top + 0.54 * glyph_height,
                0.46 * glyph_height,
                0.34 * glyph_height,
            );
            for sign in [-1.0, 1.0] {
                coverage.rounded_rect(
                    cx + sign * 0.36 * glyph_height,
                    glyph_top + 0.72 * glyph_height,
                    0.30 * glyph_height,
                    0.44 * glyph_height,
                    0.15 * glyph_height,
                    false,
                );
            }
        }
        "keyboard" => {
            let body_w = 0.96 * glyph_height;
            let body_h = 0.60 * glyph_height;
            let body_y = glyph_top + 0.47 * glyph_height;
            coverage.rounded_rect(
                cx,
                body_y,
                body_w,
                body_h,
                (0.08 * glyph_height).max(0.7),
                false,
            );
            for row in 0..3 {
                for column in 0..5 {
                    coverage.rounded_rect(
                        cx - body_w * 0.34 + column as f64 * body_w * 0.17,
                        body_y - body_h * 0.28 + row as f64 * body_h * 0.26,
                        body_w * 0.10,
                        body_h * 0.10,
                        0.2,
                        true,
                    );
                }
            }
        }
        "gamepad" => {
            // Silhouette: one wide rounded body with a shallow stadium notch cut
            // up from the bottom centre, which leaves two stubby legs. The body
            // is slightly wider than tall so the shape reads as a pad and not as
            // a phone; the notch keeps the legs from merging into a slab.
            let unit = glyph_height;
            let body_top = glyph_top + 0.105 * unit;
            let body_h = 0.805 * unit;
            coverage.rounded_rect(
                cx,
                body_top + body_h / 2.0,
                1.075 * unit,
                body_h,
                0.230 * unit,
                false,
            );
            // Notch: erase a stadium whose top edge sits inside the body and
            // whose sides run past the bottom edge, so the roof of the opening is
            // a semicircle in the bottom centre of the pad rather than a squared
            // slot.
            let notch_depth = 0.168 * unit;
            let notch_top = body_top + body_h - notch_depth;
            let notch_h = body_h + notch_depth;
            let notch_w = 0.390 * unit;
            coverage.rounded_rect(
                cx,
                notch_top + notch_h / 2.0,
                notch_w,
                notch_h,
                0.195 * unit,
                true,
            );
            // Chunky plus-shaped d-pad on the left.
            let dpad_x = cx - 0.215 * unit;
            let dpad_y = glyph_top + 0.437 * unit;
            coverage.rounded_rect(
                dpad_x,
                dpad_y,
                0.250 * unit,
                0.096 * unit,
                0.048 * unit,
                true,
            );
            coverage.rounded_rect(
                dpad_x,
                dpad_y,
                0.096 * unit,
                0.250 * unit,
                0.048 * unit,
                true,
            );
            // Four face buttons in a diamond on the right.
            let face_x = cx + 0.245 * unit;
            let face_y = glyph_top + 0.437 * unit;
            for (dx, dy) in [(0.0, -0.106), (0.106, 0.0), (0.0, 0.106), (-0.106, 0.0)] {
                coverage.rounded_rect(
                    face_x + dx * unit,
                    face_y + dy * unit,
                    0.104 * unit,
                    0.104 * unit,
                    0.052 * unit,
                    true,
                );
            }
        }
        _ => {
            let body_w = 0.62 * glyph_height;
            coverage.rounded_rect(
                cx,
                glyph_top + 0.48 * glyph_height,
                body_w,
                0.96 * glyph_height,
                0.27 * glyph_height,
                false,
            );
            coverage.rounded_rect(
                cx,
                glyph_top + 0.21 * glyph_height,
                (0.13 * glyph_height).max(0.9),
                (0.26 * glyph_height).max(1.6),
                0.45,
                true,
            );
            coverage.rounded_rect(
                cx,
                glyph_top + 0.40 * glyph_height,
                body_w * 0.92,
                (0.055 * glyph_height).max(0.55),
                0.25,
                true,
            );
        }
    }
}

fn battery_layers(
    size: usize,
    percent: Option<u8>,
    charging: bool,
    light: bool,
) -> (Coverage, Coverage, Option<Coverage>) {
    let scale = 4;
    let bar_height = (size as f64 * 0.16).max(2.2);
    let bar_bottom = size as f64 - (size as f64 * 0.02).max(0.4);
    let bar_top = bar_bottom - bar_height;
    let left = (size as f64 * 0.07).max(0.6);
    let mut right = size as f64 - left;
    let bolt = if charging {
        let height = (bar_height * 1.75).max(4.2);
        let width = height * 0.58;
        let bolt_left = right - width;
        let top = bar_bottom - height;
        let mut mask = Coverage::new(size, scale);
        let base = [
            (0.64, 0.0),
            (0.16, 0.58),
            (0.46, 0.58),
            (0.38, 1.0),
            (0.86, 0.42),
            (0.55, 0.42),
        ];
        let points: Vec<_> = base
            .iter()
            .map(|(x, y)| (bolt_left + x * width, top + y * height))
            .collect();
        mask.polygon(&points);
        right = right.min(bolt_left + width * 0.35);
        Some(mask)
    } else {
        None
    };
    let mut track = Coverage::new(size, scale);
    track.rounded_rect(
        (left + right) / 2.0,
        (bar_top + bar_bottom) / 2.0,
        right - left,
        bar_height,
        bar_height / 2.0,
        false,
    );
    let mut fill = Coverage::new(size, scale);
    if let Some(percent) = percent {
        let width = ((right - left) * f64::from(percent) / 100.0)
            .max(bar_height * 0.92)
            .min(right - left);
        fill.rounded_rect(
            left + width / 2.0,
            (bar_top + bar_bottom) / 2.0,
            width,
            bar_height,
            (bar_height / 2.0).min(width / 2.0),
            false,
        );
    }
    let _theme = light;
    (track, fill, bolt)
}

/// Renders a built-in device silhouette and battery bar to RGBA pixels.
pub fn device_icon_rgba(
    kind: &str,
    percent: Option<u8>,
    size: u32,
    charging: bool,
    light_taskbar: bool,
) -> RgbaImage {
    let size = size.clamp(8, 256) as usize;
    let scale = 4;
    let mut shape = Coverage::new(size, scale);
    let top = (size as f64 * 0.02).max(0.3);
    let bar_height = (size as f64 * 0.16).max(2.2);
    let glyph_height = (size as f64 - bar_height - top - 2.0).max(4.0);
    draw_shape(&mut shape, kind, size, top, glyph_height);
    let (track, fill, bolt) = battery_layers(size, percent, charging, light_taskbar);
    let glyph = if percent.is_some() {
        if light_taskbar {
            [56, 60, 68]
        } else {
            [226, 230, 236]
        }
    } else if light_taskbar {
        [150, 154, 160]
    } else {
        [128, 132, 138]
    };
    let track_color = if light_taskbar {
        [24, 26, 30]
    } else {
        [255, 255, 255]
    };
    let mut canvas = RgbaImage::from_pixel(size as u32, size as u32, Rgba([0, 0, 0, 0]));
    overlay(&mut canvas, &track, track_color, 0.34);
    if percent.is_some() {
        overlay(&mut canvas, &fill, percent_color(percent), 1.0);
    }
    overlay(&mut canvas, &shape, glyph, 1.0);
    if let Some(bolt) = bolt {
        overlay(&mut canvas, &bolt, glyph, 1.0);
    }
    canvas
}

/// Renders only the built-in device silhouette, without the battery bar.
///
/// Used by the compact panel cards: the shape is the exact same one drawn for
/// the tray icon, so a mouse/headphone/... reads consistently in both places.
pub fn device_glyph_rgba(kind: &str, size: u32, color: [u8; 3]) -> RgbaImage {
    let size = size.clamp(8, 256) as usize;
    let scale = 4;
    let mut shape = Coverage::new(size, scale);
    let top = (size as f64 * 0.04).max(0.3);
    let glyph_height = (size as f64 * 0.88).max(4.0);
    draw_shape(&mut shape, kind, size, top, glyph_height);
    let mut canvas = RgbaImage::from_pixel(size as u32, size as u32, Rgba([0, 0, 0, 0]));
    overlay(&mut canvas, &shape, color, 1.0);
    canvas
}

/// A custom image already decoded and scaled for one icon size.
struct ScaledIcon {
    signature: (u128, u64),
    image: Arc<RgbaImage>,
    offset_x: i64,
    offset_y: i64,
}

static SCALED_ICONS: OnceLock<Mutex<HashMap<(PathBuf, u32), ScaledIcon>>> = OnceLock::new();

/// Upper bound on cached renders; 32 devices at two icon sizes stay well below.
const SCALED_ICON_LIMIT: usize = 64;

fn scaled_icons() -> &'static Mutex<HashMap<(PathBuf, u32), ScaledIcon>> {
    SCALED_ICONS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Decodes and scales the custom image for `size`, using a per-file cache.
///
/// The notification area rebuilds an icon whenever the battery percentage
/// changes, which for a custom image used to reread the file and rerun the SVG
/// rasterizer every minute. Only the scaling depends on the file and the icon
/// size; the battery bar on top is what actually changes.
fn scaled_icon(path: &Path, size: u32) -> Result<(Arc<RgbaImage>, i64, i64)> {
    let key = (path.to_path_buf(), size);
    let signature = crate::config::file_signature(path);
    if let Ok(cache) = scaled_icons().lock() {
        if let Some(entry) = cache.get(&key).filter(|entry| entry.signature == signature) {
            return Ok((Arc::clone(&entry.image), entry.offset_x, entry.offset_y));
        }
    }
    let source = load_image(path)?;
    let available_height = (size as f64 * 0.76).max(4.0) as u32;
    let available_width = (size as f64 * 0.86).max(4.0) as u32;
    let ratio = (f64::from(available_width) / f64::from(source.width()))
        .min(f64::from(available_height) / f64::from(source.height()));
    let width = ((source.width() as f64 * ratio).round() as u32).max(1);
    let height = ((source.height() as f64 * ratio).round() as u32).max(1);
    let image = Arc::new(
        DynamicImage::ImageRgba8(source)
            .resize_exact(width, height, FilterType::Lanczos3)
            .to_rgba8(),
    );
    let offset_x = ((size - width) / 2) as i64;
    let offset_y = ((available_height - height) / 2) as i64;
    let mut cache = match scaled_icons().lock() {
        Ok(cache) => cache,
        Err(poisoned) => poisoned.into_inner(),
    };
    if cache.len() >= SCALED_ICON_LIMIT {
        cache.clear();
    }
    cache.insert(
        key,
        ScaledIcon {
            signature,
            image: Arc::clone(&image),
            offset_x,
            offset_y,
        },
    );
    Ok((image, offset_x, offset_y))
}

/// Renders a custom image with the same battery bar; missing images use `device`.
pub fn custom_icon_rgba(
    path: &Path,
    percent: Option<u8>,
    size: u32,
    charging: bool,
    light_taskbar: bool,
) -> Result<RgbaImage> {
    let size = size.clamp(8, 256);
    let (resized, offset_x, offset_y) = scaled_icon(path, size)?;
    let mut canvas = RgbaImage::from_pixel(size, size, Rgba([0, 0, 0, 0]));
    image::imageops::overlay(&mut canvas, resized.as_ref(), offset_x, offset_y);
    let (track, fill, bolt) = battery_layers(size as usize, percent, charging, light_taskbar);
    let track_color = if light_taskbar {
        [24, 26, 30]
    } else {
        [255, 255, 255]
    };
    let glyph = if light_taskbar {
        [56, 60, 68]
    } else {
        [226, 230, 236]
    };
    overlay(&mut canvas, &track, track_color, 0.34);
    if percent.is_some() {
        overlay(&mut canvas, &fill, percent_color(percent), 1.0);
    }
    if let Some(bolt) = bolt {
        overlay(&mut canvas, &bolt, glyph, 1.0);
    }
    Ok(canvas)
}

/// Encodes RGBA pixels as an in-memory ICO accepted by native Windows GUI.
pub fn icon_bytes(image: &RgbaImage) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    IcoEncoder::new(&mut bytes).write_image(
        image.as_raw(),
        image.width(),
        image.height(),
        image::ColorType::Rgba8.into(),
    )?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thresholds_and_shapes_match_contract() {
        assert_eq!(percent_color(Some(100)), COLOR_GREEN);
        assert_eq!(percent_color(Some(61)), COLOR_GREEN);
        assert_eq!(percent_color(Some(60)), COLOR_YELLOW);
        assert_eq!(percent_color(Some(20)), COLOR_YELLOW);
        assert_eq!(percent_color(Some(19)), COLOR_RED);
        assert_eq!(percent_color(Some(1)), COLOR_RED);
        assert_eq!(percent_color(None), COLOR_GRAY);
        let mouse = device_icon_rgba("mouse", Some(70), 16, false, false);
        let keyboard = device_icon_rgba("keyboard", Some(70), 16, false, false);
        assert_ne!(mouse, keyboard);
        assert_eq!(mouse.get_pixel(0, 0)[3], 0);
        assert!(mouse.pixels().any(|pixel| pixel.0 == [34, 197, 94, 255]));
        let unknown = device_icon_rgba("mouse", None, 16, false, false);
        assert!(!unknown.pixels().any(|pixel| pixel.0 == [34, 197, 94, 255]));
    }

    #[test]
    fn custom_image_is_validated_and_encoded() {
        let temporary = tempfile::tempdir();
        assert!(temporary.is_ok());
        let Some(directory) = temporary.ok() else {
            return;
        };
        let path = directory.path().join("icon.png");
        let source = RgbaImage::from_pixel(8, 8, Rgba([220, 30, 40, 255]));
        let mut encoded = Vec::new();
        let encoder = image::codecs::png::PngEncoder::new(&mut encoded);
        assert!(encoder
            .write_image(source.as_raw(), 8, 8, image::ColorType::Rgba8.into())
            .is_ok());
        assert!(std::fs::write(&path, encoded).is_ok());
        assert!(validate_image_file(&path).is_ok());
        let rendered = custom_icon_rgba(&path, Some(70), 16, false, false);
        assert!(rendered.is_ok());
        let rendered = rendered.unwrap_or_else(|_| RgbaImage::new(0, 0));
        assert!(rendered.pixels().any(|pixel| pixel.0 == [220, 30, 40, 255]));
        assert!(icon_bytes(&rendered).is_ok());
    }

    const RED_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16"><rect width="16" height="16" fill="#dc1e28"/></svg>"##;

    #[test]
    fn svg_icon_is_validated_rasterized_and_encoded() {
        let Some(directory) = tempfile::tempdir().ok() else {
            return;
        };
        let path = directory.path().join("icon.svg");
        assert!(std::fs::write(&path, RED_SVG).is_ok());
        assert!(validate_image_file(&path).is_ok());
        let rendered = custom_icon_rgba(&path, Some(70), 16, false, false);
        assert!(rendered.is_ok());
        let rendered = rendered.unwrap_or_else(|_| RgbaImage::new(0, 0));
        assert!(rendered.pixels().any(|pixel| pixel.0 == [220, 30, 40, 255]));
        assert!(icon_bytes(&rendered).is_ok());
    }

    #[test]
    fn cached_render_is_reused_and_refreshed_after_the_file_changes() {
        let Some(directory) = tempfile::tempdir().ok() else {
            return;
        };
        let path = directory.path().join("icon.png");
        let write = |color: [u8; 4], width: u32| {
            let source = RgbaImage::from_pixel(width, 8, Rgba(color));
            let mut encoded = Vec::new();
            let encoder = image::codecs::png::PngEncoder::new(&mut encoded);
            assert!(encoder
                .write_image(source.as_raw(), width, 8, image::ColorType::Rgba8.into())
                .is_ok());
            assert!(std::fs::write(&path, encoded).is_ok());
        };
        write([220, 30, 40, 255], 8);
        let first = custom_icon_rgba(&path, Some(70), 32, false, false);
        assert!(first.is_ok(), "first render failed: {first:?}");
        // An unchanged file must reuse the decoded image: rendered pixels stay
        // identical because only the battery bar depends on the percentage.
        let again = custom_icon_rgba(&path, Some(20), 32, false, false);
        assert!(again.is_ok(), "cached render failed: {again:?}");
        let (Some(first), Some(again)) = (first.ok(), again.ok()) else {
            return;
        };
        assert_eq!(first.dimensions(), again.dimensions());
        // Replacing the file must invalidate the cache, otherwise editing an
        // icon in the settings window would show the previous picture.
        write([30, 200, 60, 255], 9);
        let replaced = custom_icon_rgba(&path, Some(70), 32, false, false);
        assert!(replaced.is_ok(), "render after edit failed: {replaced:?}");
        let replaced = replaced.unwrap_or_else(|_| RgbaImage::new(0, 0));
        assert!(
            replaced.pixels().any(|pixel| pixel.0 == [30, 200, 60, 255]),
            "cache kept the replaced icon"
        );
    }

    #[test]
    fn malformed_svg_is_rejected() {
        let temporary = tempfile::tempdir();
        let Some(directory) = temporary.ok() else {
            return;
        };
        let path = directory.path().join("broken.svg");
        assert!(std::fs::write(&path, b"<svg><rect").is_ok());
        assert!(validate_image_file(&path).is_err());
    }

    #[test]
    fn unsupported_extension_is_rejected() {
        let temporary = tempfile::tempdir();
        let Some(directory) = temporary.ok() else {
            return;
        };
        let path = directory.path().join("icon.gif");
        assert!(std::fs::write(&path, RED_SVG).is_ok());
        let message = match validate_image_file(&path) {
            Err(error) => error.to_string(),
            Ok(()) => String::new(),
        };
        assert!(message.contains("PNG / ICO / BMP / SVG"));
    }

    #[test]
    fn huge_svg_is_clamped_to_decode_limits() {
        let huge = r##"<svg xmlns="http://www.w3.org/2000/svg" width="100000" height="50000"><rect width="100000" height="50000" fill="#dc1e28"/></svg>"##;
        let image = load_svg(huge.as_bytes());
        assert!(image.is_ok());
        let image = image.unwrap_or_else(|_| RgbaImage::new(0, 0));
        assert!(image.width() > 0 && image.height() > 0);
        assert!(image.width() <= MAX_DIMENSION && image.height() <= MAX_DIMENSION);
        assert!(u64::from(image.width()) * u64::from(image.height()) <= MAX_PIXELS);
    }
}
