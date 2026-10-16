//! `@file` arguments (cli/file-processor.ts `processFileArguments`): each file's text inline as
//! `<file name="<abs>">…</file>`, and images attached to the prompt (content-sniffed, converted and resized as
//! utils/image-process.ts does, through crates/host's `view_image` pipeline) with a `<file>` reference and its
//! processing hints. Paths resolve like the read tool's (`resolveReadPath`: `~`, a stray `@`, unicode spaces, the
//! macOS screenshot variants).

use base64::Engine;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// The prompt text and image attachments of the `@file` arguments.
#[derive(Debug, Default, PartialEq)]
pub struct ProcessedFiles {
    pub text: String,
    /// `ImageContent`: `{type: "image", mimeType, data}` (base64).
    pub images: Vec<Value>,
}

/// `normalizePath(input, {normalizeUnicodeSpaces, stripAtPrefix})`: unicode spaces become spaces, one leading `@`
/// goes, `~` expands, `file://` URLs become paths.
fn normalize_input(input: &str, home: &Path) -> PathBuf {
    let spaced: String = input
        .chars()
        .map(|c| match c {
            '\u{00A0}' | '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
            c => c,
        })
        .collect();
    let stripped = spaced.strip_prefix('@').unwrap_or(&spaced);
    config::paths::expand_tilde(stripped, home)
}

/// `resolveToCwd`: absolute paths normalized, relative ones against `cwd` (Node's `path.resolve`).
pub fn resolve_to_cwd(input: &str, cwd: &Path, home: &Path) -> PathBuf {
    let normalized = normalize_input(input, home);
    config::paths::normalize(&cwd.join(normalized))
}

/// `tryMacOSScreenshotPath`: ` AM.` / ` PM.` (any case) with a narrow no-break space before the AM/PM.
fn am_pm_variant(path: &str) -> String {
    let chars: Vec<char> = path.chars().collect();
    let mut out = String::with_capacity(path.len());
    let mut i = 0;
    while i < chars.len() {
        let window = chars.get(i..i + 4);
        if let Some([' ', a, m, '.']) = window
            && matches!(a.to_ascii_uppercase(), 'A' | 'P')
            && m.eq_ignore_ascii_case(&'m')
        {
            out.push('\u{202F}');
            out.push(*a);
            out.push(*m);
            out.push('.');
            i += 4;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// `resolveReadPath`: the resolved path, else the first macOS variant that exists (AM/PM narrow space, curly
/// apostrophe), else the resolved path. (The NFD variant needs Unicode normalization tables the Rust build does
/// not carry; macOS APFS lookups are normalization-insensitive anyway.)
pub fn resolve_read_path(input: &str, cwd: &Path, home: &Path) -> PathBuf {
    let resolved = resolve_to_cwd(input, cwd, home);
    if resolved.exists() {
        return resolved;
    }
    let text = resolved.display().to_string();
    for variant in [am_pm_variant(&text), text.replace('\'', "\u{2019}")] {
        if variant != text && Path::new(&variant).exists() {
            return PathBuf::from(variant);
        }
    }
    resolved
}

/// Header dimensions of an inline image type.
fn dimensions(bytes: &[u8], mime: &str) -> Option<(u32, u32)> {
    let format = match mime {
        "image/png" => image::ImageFormat::Png,
        "image/jpeg" => image::ImageFormat::Jpeg,
        "image/gif" => image::ImageFormat::Gif,
        "image/webp" => image::ImageFormat::WebP,
        "image/bmp" => image::ImageFormat::Bmp,
        _ => return None,
    };
    image::ImageReader::with_format(std::io::Cursor::new(bytes), format)
        .into_dimensions()
        .ok()
}

/// JS `x.toFixed(2)` for a positive ratio (ties away from zero).
fn to_fixed_2(x: f64) -> String {
    format!("{:.2}", (x * 100.0).round() / 100.0)
}

/// `processImage(bytes, mime, {autoResizeImages: true})`: the attachment and its hints, or the omission message.
fn process_image(path: &Path, bytes: &[u8], mime: &str) -> Result<(Value, Vec<String>), String> {
    let mut images = host::view_image::CellImages::new(host::view_image::ViewImageOptions::default());
    let mut payload = serde_json::Map::new();
    payload.insert("path".into(), path.display().to_string().into());
    if let Err(error) = images.attach(&payload) {
        return Err(if error.contains("converted") {
            "[Image omitted: could not be converted to a supported inline image format.]".into()
        } else {
            "[Image omitted: could not be resized below the inline image size limit.]".into()
        });
    }
    let processed = images.images.pop().ok_or("[Image omitted: could not be read.]")?;
    let mut hints = Vec::new();
    // `conversionHint`: only an image that was not an inline type (BMP) was converted.
    if mime == "image/bmp" {
        hints.push(format!("[Image converted from {mime} to {}.]", processed.mime_type));
    }
    // `formatDimensionNote`: only when the image was re-encoded to fit.
    let normalized_unchanged = processed.data == bytes || (mime == "image/bmp" && processed.mime_type == "image/png");
    let original = dimensions(bytes, mime);
    let shown = dimensions(&processed.data, processed.mime_type);
    if let (Some((ow, oh)), Some((w, h))) = (original, shown)
        && (!normalized_unchanged || (ow, oh) != (w, h))
    {
        hints.push(format!(
            "[Image: original {ow}x{oh}, displayed at {w}x{h}. Multiply coordinates by {} to map to original image.]",
            to_fixed_2(f64::from(ow) / f64::from(w))
        ));
    }
    let data = base64::engine::general_purpose::STANDARD.encode(&processed.data);
    Ok((
        json!({"type": "image", "mimeType": processed.mime_type, "data": data}),
        hints,
    ))
}

/// `processFileArguments`: `Err` is the error line (`File not found: <abs>`, `Could not read file <abs>: <why>`)
/// the run ends with.
pub fn process_file_arguments(file_args: &[String], cwd: &Path, home: &Path) -> Result<ProcessedFiles, String> {
    let mut out = ProcessedFiles::default();
    for arg in file_args {
        let path = resolve_read_path(arg, cwd, home);
        let name = path.display().to_string();
        let metadata = std::fs::metadata(&path).map_err(|_| format!("File not found: {name}"))?;
        if metadata.len() == 0 {
            continue;
        }
        let bytes = std::fs::read(&path).map_err(|e| format!("Could not read file {name}: {e}"))?;
        match host::view_image::detect_image_mime_type(&bytes) {
            Some(mime) => match process_image(&path, &bytes, mime) {
                Ok((image, hints)) => {
                    out.images.push(image);
                    out.text
                        .push_str(&format!("<file name=\"{name}\">{}</file>\n", hints.join("\n")));
                }
                Err(message) => out.text.push_str(&format!("<file name=\"{name}\">{message}</file>\n")),
            },
            None => {
                let text = String::from_utf8_lossy(&bytes);
                let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
                out.text.push_str(&format!("<file name=\"{name}\">\n{text}\n</file>\n"));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_file_args_am_pm_variant_uses_a_narrow_space() {
        assert_eq!(am_pm_variant("/s/Shot 1.02.03 PM.png"), "/s/Shot 1.02.03\u{202F}PM.png");
        assert_eq!(am_pm_variant("/s/x am.png"), "/s/x\u{202F}am.png");
        assert_eq!(am_pm_variant("/s/ AMx"), "/s/ AMx");
    }

    #[test]
    fn cli_file_args_resolve_like_resolve_to_cwd() {
        let (cwd, home) = (Path::new("/w/p"), Path::new("/h"));
        assert_eq!(resolve_to_cwd("a/../b.txt", cwd, home), PathBuf::from("/w/p/b.txt"));
        assert_eq!(resolve_to_cwd("@x.txt", cwd, home), PathBuf::from("/w/p/x.txt"));
        assert_eq!(resolve_to_cwd("~/n.md", cwd, home), PathBuf::from("/h/n.md"));
        assert_eq!(resolve_to_cwd("/abs//q.md", cwd, home), PathBuf::from("/abs/q.md"));
        assert_eq!(
            resolve_to_cwd("my\u{00A0}file.txt", cwd, home),
            PathBuf::from("/w/p/my file.txt")
        );
        assert_eq!(to_fixed_2(9.0 / 8.0), "1.13");
        assert_eq!(to_fixed_2(4000.0 / 2000.0), "2.00");
    }
}
