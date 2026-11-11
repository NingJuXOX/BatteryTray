use crate::error::{BatteryTrayError, Result};
use image::codecs::ico::IcoEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, ImageEncoder, ImageReader, Rgba, RgbaImage};
use std::fs;
use std::path::Path;

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

/// Returns the battery color using the original 60/30 percent thresholds.
pub fn percent_color(percent: Option<u8>) -> [u8; 3] {
    match percent {
        None => COLOR_GRAY,
        Some(value) if value >= 60 => COLOR_GREEN,
        Some(value) if value >= 30 => COLOR_YELLOW,
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

/// Validates format, size and decodability of a custom PNG/ICO/BMP image.
pub fn validate_image_file(path: &Path) -> Result<()> {
    let extension = path
        .extension()
        .and_then(|item| item.to_str())
        .unwrap_or("")
        .to_lowercase();
    if !matches!(extension.as_str(), "png" | "ico" | "bmp") {
        return Err(BatteryTrayError::Invalid(
            "图标必须是 PNG / ICO / BMP".to_owned(),
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
    // Bound width, height and total allocation before the decoder runs so a
    // decompression bomb cannot exhaust memory.
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_PIXELS.saturating_mul(4).saturating_add(1024 * 1024));
    let mut reader = ImageReader::new(std::io::Cursor::new(&bytes)).with_guessed_format()?;
    reader.limits(limits);
    let image = reader.decode()?;
    check_dimensions(image.width(), image.height())?;
    Ok(image.to_rgba8())
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
            let body_y = glyph_top + 0.42 * glyph_height;
            coverage.rounded_rect(
                cx,
                body_y,
                0.78 * glyph_height,
                0.40 * glyph_height,
                0.16 * glyph_height,
                false,
            );
            for sign in [-1.0, 1.0] {
                coverage.rounded_rect(
                    cx + sign * 0.36 * glyph_height,
                    glyph_top + 0.62 * glyph_height,
                    0.28 * glyph_height,
                    0.54 * glyph_height,
                    0.13 * glyph_height,
                    false,
                );
            }
            coverage.rounded_rect(
                cx - 0.24 * glyph_height,
                body_y,
                0.15 * glyph_height,
                0.05 * glyph_height,
                0.02,
                true,
            );
            coverage.rounded_rect(
                cx - 0.24 * glyph_height,
                body_y,
                0.05 * glyph_height,
                0.15 * glyph_height,
                0.02,
                true,
            );
            for (dx, dy) in [(0.19, -0.08), (0.31, 0.05)] {
                coverage.rounded_rect(
                    cx + dx * glyph_height,
                    body_y + dy * glyph_height,
                    0.10 * glyph_height,
                    0.10 * glyph_height,
                    0.05 * glyph_height,
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

/// Renders a custom image with the same battery bar; missing images use `device`.
pub fn custom_icon_rgba(
    path: &Path,
    percent: Option<u8>,
    size: u32,
    charging: bool,
    light_taskbar: bool,
) -> Result<RgbaImage> {
    let source = load_image(path)?;
    let size = size.clamp(8, 256);
    let available_height = (size as f64 * 0.76).max(4.0) as u32;
    let available_width = (size as f64 * 0.86).max(4.0) as u32;
    let ratio = (f64::from(available_width) / f64::from(source.width()))
        .min(f64::from(available_height) / f64::from(source.height()));
    let width = ((source.width() as f64 * ratio).round() as u32).max(1);
    let height = ((source.height() as f64 * ratio).round() as u32).max(1);
    let resized = DynamicImage::ImageRgba8(source)
        .resize_exact(width, height, FilterType::Lanczos3)
        .to_rgba8();
    let mut canvas = RgbaImage::from_pixel(size, size, Rgba([0, 0, 0, 0]));
    image::imageops::overlay(
        &mut canvas,
        &resized,
        ((size - width) / 2) as i64,
        ((available_height - height) / 2) as i64,
    );
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
        assert_eq!(percent_color(Some(60)), COLOR_GREEN);
        assert_eq!(percent_color(Some(30)), COLOR_YELLOW);
        assert_eq!(percent_color(Some(29)), COLOR_RED);
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
}
