//! `rlm.view_image` host side, ported from ultron/rlm/view-image.ts over utils/image-process.ts,
//! utils/image-resize-core.ts and utils/mime.ts: content-sniffed type, conversion of BMP to PNG, auto-resize to
//! the model's limits (Lanczos3, then the smaller of PNG and JPEG under the byte budget, then 3/4 steps), and the
//! kernel's short description. Photon (WASM) is replaced by the `image` crate, so resized or converted bytes differ
//! from TypeScript's; dimensions, type choice and limits do not.

use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader};
use serde_json::{Map, Value, json};
use std::io::Cursor;

pub const VIEW_IMAGE_REQUEST: &str = "rlm.view_image";
pub const MAX_IMAGES_PER_CELL: usize = 8;
pub const MAX_VIEW_IMAGE_SOURCE_BYTES: u64 = 64 * 1024 * 1024;
const LOW_DETAIL_MAX_SIDE: u32 = 512;
const PNG_SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];

/// A model's image resize limits (`inputLimits.images.resize`); unset fields take the defaults.
#[derive(Clone, Copy, Debug, Default)]
pub struct ResizeLimits {
    pub max_width: Option<u32>,
    pub max_height: Option<u32>,
    pub max_bytes: Option<f64>,
    pub jpeg_quality: Option<u8>,
}

/// The model a cell's images go to.
#[derive(Clone, Debug)]
pub struct ImageModel {
    pub provider: String,
    pub id: String,
    pub accepts_images: bool,
    pub resize: Option<ResizeLimits>,
}

/// Pi's `images.autoResize` setting (default on) and the current model.
#[derive(Clone, Debug)]
pub struct ViewImageOptions {
    pub auto_resize: bool,
    pub model: Option<ImageModel>,
}

impl Default for ViewImageOptions {
    fn default() -> Self {
        ViewImageOptions {
            auto_resize: true,
            model: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CellImage {
    pub mime_type: &'static str,
    pub data: Vec<u8>,
}

/// The images one cell attached, in call order.
#[derive(Default)]
pub struct CellImages {
    pub images: Vec<CellImage>,
    pub options: ViewImageOptions,
}

fn u32_be(b: &[u8], at: usize) -> u64 {
    (0..4).fold(0, |n, i| (n << 8) | u64::from(*b.get(at + i).unwrap_or(&0)))
}

fn u32_le(b: &[u8], at: usize) -> u64 {
    (0..4)
        .rev()
        .fold(0, |n, i| (n << 8) | u64::from(*b.get(at + i).unwrap_or(&0)))
}

fn u16_le(b: &[u8], at: usize) -> u64 {
    u64::from(*b.get(at).unwrap_or(&0)) | (u64::from(*b.get(at + 1).unwrap_or(&0)) << 8)
}

fn ascii_at(b: &[u8], at: usize, text: &str) -> bool {
    b.get(at..at + text.len()) == Some(text.as_bytes())
}

fn animated_png(b: &[u8]) -> bool {
    let mut offset = PNG_SIGNATURE.len();
    while offset + 8 <= b.len() {
        if ascii_at(b, offset + 4, "acTL") {
            return true;
        }
        if ascii_at(b, offset + 4, "IDAT") {
            return false;
        }
        let next = offset as u64 + 12 + u32_be(b, offset);
        if next > b.len() as u64 {
            return false;
        }
        offset = next as usize;
    }
    false
}

fn bmp(b: &[u8]) -> bool {
    if b.len() < 26 {
        return false;
    }
    let (size, pixels, dib) = (u32_le(b, 2), u32_le(b, 10), u32_le(b, 14));
    if (size != 0 && size < 26) || pixels < 14 + dib || (size != 0 && pixels >= size) {
        return false;
    }
    let (planes, bits) = match dib {
        12 => (u16_le(b, 22), u16_le(b, 24)),
        40..=124 if b.len() >= 30 => (u16_le(b, 26), u16_le(b, 28)),
        _ => return false,
    };
    planes == 1 && [1, 4, 8, 16, 24, 32].contains(&bits)
}

/// The image type by content (utils/mime.ts detectSupportedImageMimeType).
pub fn detect_image_mime_type(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(&[0xff, 0xd8, 0xff]) {
        return (b.get(3) != Some(&0xf7)).then_some("image/jpeg");
    }
    if b.starts_with(&PNG_SIGNATURE) {
        let png = b.len() >= 16 && u32_be(b, 8) == 13 && ascii_at(b, 12, "IHDR");
        return (png && !animated_png(b)).then_some("image/png");
    }
    if ascii_at(b, 0, "GIF87a") || ascii_at(b, 0, "GIF89a") {
        return Some("image/gif");
    }
    if ascii_at(b, 0, "RIFF") && ascii_at(b, 8, "WEBP") {
        return Some("image/webp");
    }
    (ascii_at(b, 0, "BM") && bmp(b)).then_some("image/bmp")
}

/// Header dimensions for the inline types (png, jpeg, gif, webp); None for others or unreadable headers.
fn dimensions(bytes: &[u8], mime: &str) -> Option<(u32, u32)> {
    let format = match mime {
        "image/png" => ImageFormat::Png,
        "image/jpeg" => ImageFormat::Jpeg,
        "image/gif" => ImageFormat::Gif,
        "image/webp" => ImageFormat::WebP,
        _ => return None,
    };
    ImageReader::with_format(Cursor::new(bytes), format)
        .into_dimensions()
        .ok()
}

/// Decode with the EXIF orientation applied (utils/exif-orientation.ts).
fn decode(bytes: &[u8]) -> Option<DynamicImage> {
    let mut decoder = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_decoder()
        .ok()?;
    let orientation = decoder.orientation().ok();
    let mut image = DynamicImage::from_decoder(decoder).ok()?;
    if let Some(orientation) = orientation {
        image.apply_orientation(orientation);
    }
    Some(image)
}

fn encode_png(image: &DynamicImage) -> Option<Vec<u8>> {
    let mut out = Cursor::new(Vec::new());
    image.to_rgba8().write_to(&mut out, ImageFormat::Png).ok()?;
    Some(out.into_inner())
}

fn encode_jpeg(image: &DynamicImage, quality: u8) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    image
        .to_rgb8()
        .write_with_encoder(JpegEncoder::new_with_quality(&mut out, quality))
        .ok()?;
    Some(out)
}

fn base64_len(bytes: usize) -> f64 {
    bytes.div_ceil(3) as f64 * 4.0
}

/// JS `Math.round` of a non-negative number.
fn js_round(value: f64) -> u32 {
    (value + 0.5).floor() as u32
}

/// resizeImageInProcess: None when the image cannot be decoded or brought under `max_bytes`.
fn resize(bytes: &[u8], mime: &'static str, limits: ResizeLimits) -> Option<CellImage> {
    let max_width = limits.max_width.unwrap_or(2000);
    let max_height = limits.max_height.unwrap_or(2000);
    let max_bytes = limits.max_bytes.unwrap_or(4.5 * 1024.0 * 1024.0);
    let quality = limits.jpeg_quality.unwrap_or(80);
    let image = decode(bytes)?;
    let (width, height) = (image.width(), image.height());
    if width <= max_width && height <= max_height && base64_len(bytes.len()) < max_bytes {
        return Some(CellImage {
            mime_type: mime,
            data: bytes.to_vec(),
        });
    }
    let (mut w, mut h) = (width, height);
    if w > max_width {
        h = js_round(f64::from(h) * f64::from(max_width) / f64::from(w));
        w = max_width;
    }
    if h > max_height {
        w = js_round(f64::from(w) * f64::from(max_height) / f64::from(h));
        h = max_height;
    }
    let mut qualities = vec![quality];
    for q in [85, 70, 55, 40] {
        if !qualities.contains(&q) {
            qualities.push(q);
        }
    }
    loop {
        let resized = image.resize_exact(w, h, FilterType::Lanczos3);
        let png = encode_png(&resized)?;
        if base64_len(png.len()) < max_bytes {
            return Some(CellImage {
                mime_type: "image/png",
                data: png,
            });
        }
        for &q in &qualities {
            let jpeg = encode_jpeg(&resized, q)?;
            if base64_len(jpeg.len()) < max_bytes {
                return Some(CellImage {
                    mime_type: "image/jpeg",
                    data: jpeg,
                });
            }
        }
        if w == 1 && h == 1 {
            return None;
        }
        let next = |side: u32| {
            if side == 1 {
                1
            } else {
                ((f64::from(side) * 0.75).floor() as u32).max(1)
            }
        };
        let (nw, nh) = (next(w), next(h));
        if (nw, nh) == (w, h) {
            return None;
        }
        (w, h) = (nw, nh);
    }
}

/// `n / unit` with one decimal, as JS `toFixed(1)` rounds it (half up; the quotient is exact).
fn tenths(n: u64, unit: u64) -> String {
    let t = (n * 20 + unit) / (2 * unit);
    format!("{}.{}", t / 10, t % 10)
}

fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{} KB", tenths(bytes, 1024))
    } else {
        format!("{} MB", tenths(bytes, 1024 * 1024))
    }
}

fn format_name(mime: &str) -> String {
    mime.split('/').nth(1).unwrap_or(mime).to_uppercase()
}

/// Pi's read tool note, said once per cell when the current model takes no image input.
pub fn non_vision_note(model: Option<&ImageModel>) -> Option<String> {
    let model = model.filter(|model| !model.accepts_images)?;
    Some(format!(
        "[view_image: the current model ({}/{}) does not accept image input, so attached images are omitted from its request. Inspect images with Python instead (for example PIL for size and pixel colors).]",
        model.provider, model.id
    ))
}

impl CellImages {
    pub fn new(options: ViewImageOptions) -> Self {
        CellImages {
            images: Vec::new(),
            options,
        }
    }

    fn limits(&self, detail: Option<&str>) -> ResizeLimits {
        let base = self.options.model.as_ref().and_then(|model| model.resize);
        if detail != Some("low") {
            return base.unwrap_or_default();
        }
        let base = base.unwrap_or_default();
        ResizeLimits {
            max_width: Some(base.max_width.unwrap_or(LOW_DETAIL_MAX_SIDE).min(LOW_DETAIL_MAX_SIDE)),
            max_height: Some(base.max_height.unwrap_or(LOW_DETAIL_MAX_SIDE).min(LOW_DETAIL_MAX_SIDE)),
            ..base
        }
    }

    /// Load, normalize and keep one image; the kernel's reply is `{description, note?}`.
    pub fn attach(&mut self, payload: &Map<String, Value>) -> Result<Value, String> {
        if self.images.len() >= MAX_IMAGES_PER_CELL {
            return Err(format!(
                "view_image: at most {MAX_IMAGES_PER_CELL} images per cell; view the rest in a later cell"
            ));
        }
        let path = match payload.get("path") {
            Some(Value::String(path)) if !path.is_empty() => path,
            _ => return Err("view_image: path must be a non-empty string".into()),
        };
        let size = std::fs::metadata(path)
            .map_err(|error| match error.kind() {
                std::io::ErrorKind::NotFound => format!("ENOENT: no such file or directory, stat '{path}'"),
                _ => error.to_string(),
            })?
            .len();
        if size > MAX_VIEW_IMAGE_SOURCE_BYTES {
            return Err(format!(
                "view_image: {} is over the {} limit",
                format_bytes(size),
                format_bytes(MAX_VIEW_IMAGE_SOURCE_BYTES)
            ));
        }
        let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
        let mime = detect_image_mime_type(&bytes)
            .ok_or("view_image: not a supported image (png, jpeg, gif or webp, detected by content)")?;
        let original = dimensions(&bytes, mime);
        let detail = payload.get("detail").and_then(Value::as_str);
        // normalizeImage: BMP becomes PNG first.
        let normalized = if mime == "image/bmp" {
            let png = decode(&bytes)
                .and_then(|image| encode_png(&image))
                .ok_or("view_image: Image omitted: could not be converted to a supported inline image format.")?;
            CellImage {
                mime_type: "image/png",
                data: png,
            }
        } else {
            CellImage {
                mime_type: mime,
                data: bytes,
            }
        };
        let processed = if self.options.auto_resize {
            resize(&normalized.data, normalized.mime_type, self.limits(detail))
                .ok_or("view_image: Image omitted: could not be resized below the inline image size limit.")?
        } else {
            normalized
        };
        let shown = dimensions(&processed.data, processed.mime_type);
        let mut description = format!(
            "image {}: {}{}, {}",
            self.images.len() + 1,
            shown.map_or(String::new(), |(w, h)| format!("{w}x{h} ")),
            format_name(processed.mime_type),
            format_bytes(processed.data.len() as u64)
        );
        match (original, shown) {
            (Some(original), Some(shown)) if original != shown => {
                description.push_str(&format!(" (downscaled from {}x{})", original.0, original.1));
            }
            _ if processed.mime_type != mime => {
                description.push_str(&format!(" (converted from {})", format_name(mime)));
            }
            _ => {}
        }
        self.images.push(processed);
        Ok(match non_vision_note(self.options.model.as_ref()) {
            Some(note) => json!({ "description": description, "note": note }),
            None => json!({ "description": description }),
        })
    }

    /// The tool result text for this cell: the non-vision note follows the text when images are attached.
    pub fn text(&self, text: &str) -> String {
        match non_vision_note(self.options.model.as_ref()) {
            Some(note) if !self.images.is_empty() => format!("{text}\n{note}"),
            _ => text.to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    /// Literal outputs of the TS formatBytes (`toFixed(1)` rounds the exact quotient half up).
    #[test]
    fn host_view_image_formats_bytes_like_js_to_fixed() {
        let got: Vec<String> = [1023, 1024, 1280, 1382, 1587, 2611, 1_310_720, 67_108_864]
            .into_iter()
            .map(super::format_bytes)
            .collect();
        assert_eq!(
            got,
            [
                "1023 B", "1.0 KB", "1.3 KB", "1.3 KB", "1.5 KB", "2.5 KB", "1.3 MB", "64.0 MB"
            ]
        );
    }
}
