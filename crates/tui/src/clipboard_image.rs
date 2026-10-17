//! Reading an image from the clipboard on Linux (utils/clipboard-image.ts `readClipboardImage`): `wl-paste` only in
//! a Wayland session (`WAYLAND_DISPLAY`, or `XDG_SESSION_TYPE=wayland`) or under WSL, then `xclip`. Each tool lists
//! the clipboard's types first and asks for the preferred image type (PNG, JPEG, WebP, GIF, then any `image/*`).
//! A tool that ran but found no image ends the read: an empty Wayland clipboard never falls through to a stale X11
//! one. Only a tool that failed (missing, non-zero exit) passes the read on.
//!
//! A PNG, JPEG, WebP or GIF image is returned as read (its type names the pasted file's extension); any other image
//! type (a BMP from a Windows DIB, say) is decoded and re-encoded as an RGBA PNG (`convertToPng`, Photon there, the
//! `image` crate here). An image that cannot be decoded is no image.
//!
//! The `image` crate here decodes BMP, GIF, ICO, JPEG, PNG, TIFF and WebP, like Photon (which wraps the same crate):
//! a multi-page TIFF converts its first page (an undecodable first page is no image), an ICO its deepest entry,
//! the largest among equals, the last among ties.
//!
//! Not ported: the WSL PowerShell fallback and the native clipboard module.

/// The image types tried, in order of preference (`SUPPORTED_IMAGE_MIME_TYPES`).
pub const SUPPORTED_IMAGE_MIME_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/webp", "image/gif"];

/// What one reader produced: an image, no image (the read ends), or a failure (the next reader runs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImageRead {
    Image { bytes: Vec<u8>, mime_type: String },
    NoImage,
    Failed,
}

/// Runs a clipboard tool: its stdout when it exited 0 (possibly empty), None when it failed (`runClipboardCommand`).
pub type RunTool<'a> = dyn FnMut(&str, &[&str]) -> Option<Vec<u8>> + 'a;

/// `isWaylandSession`.
pub fn is_wayland_session(var: &dyn Fn(&str) -> Option<String>) -> bool {
    var("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty()) || var("XDG_SESSION_TYPE").as_deref() == Some("wayland")
}

/// `isWSL`: `WSL_DISTRO_NAME` or `WSLENV`, else `/proc/version` naming Microsoft or WSL.
pub fn is_wsl(var: &dyn Fn(&str) -> Option<String>, proc_version: Option<&str>) -> bool {
    let set = |name: &str| var(name).is_some_and(|v| !v.is_empty());
    if set("WSL_DISTRO_NAME") || set("WSLENV") {
        return true;
    }
    proc_version.is_some_and(|release| {
        let release = release.to_lowercase();
        release.contains("microsoft") || release.contains("wsl")
    })
}

fn base_mime_type(mime_type: &str) -> String {
    mime_type.split(';').next().unwrap_or(mime_type).trim().to_lowercase()
}

/// `extensionForImageMimeType`: the pasted file's extension for a supported image type.
pub fn extension_for_image_mime_type(mime_type: &str) -> Option<&'static str> {
    match base_mime_type(mime_type).as_str() {
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpg"),
        "image/webp" => Some("webp"),
        "image/gif" => Some("gif"),
        _ => None,
    }
}

/// `isSupportedImageMimeType`.
fn is_supported_image_mime_type(mime_type: &str) -> bool {
    SUPPORTED_IMAGE_MIME_TYPES.contains(&base_mime_type(mime_type).as_str())
}

/// `convertToPng`: decode any image the codecs know and encode it as an RGBA PNG (Photon's `get_bytes`).
pub fn convert_to_png(bytes: &[u8]) -> Option<Vec<u8>> {
    let image = ::image::load_from_memory(bytes).ok()?.to_rgba8();
    let mut png = Vec::new();
    image
        .write_to(&mut std::io::Cursor::new(&mut png), ::image::ImageFormat::Png)
        .ok()?;
    Some(png)
}

/// `selectPreferredImageMimeType`.
pub fn preferred_image_type(types: &[String]) -> Option<String> {
    let normalized: Vec<(String, String)> = types
        .iter()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .map(|t| (t.to_string(), base_mime_type(t)))
        .collect();
    for preferred in SUPPORTED_IMAGE_MIME_TYPES {
        if let Some((raw, _)) = normalized.iter().find(|(_, base)| base == preferred) {
            return Some(raw.clone());
        }
    }
    normalized
        .iter()
        .find(|(_, base)| base.starts_with("image/"))
        .map(|(raw, _)| raw.clone())
}

fn lines(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .split('\n')
        .map(|l| l.trim_end_matches('\r').trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// `readClipboardImageViaWlPaste`.
fn via_wl_paste(run: &mut RunTool<'_>) -> ImageRead {
    let Some(list) = run("wl-paste", &["--list-types"]) else {
        return ImageRead::Failed;
    };
    let Some(selected) = preferred_image_type(&lines(&list)) else {
        return ImageRead::NoImage;
    };
    match run("wl-paste", &["--type", &selected, "--no-newline"]) {
        None => ImageRead::Failed,
        Some(data) if data.is_empty() => ImageRead::NoImage,
        Some(bytes) => ImageRead::Image {
            bytes,
            mime_type: base_mime_type(&selected),
        },
    }
}

/// `readClipboardImageViaXclip`.
fn via_xclip(run: &mut RunTool<'_>) -> ImageRead {
    let targets = run("xclip", &["-selection", "clipboard", "-t", "TARGETS", "-o"]);
    let preferred = preferred_image_type(&targets.as_deref().map(lines).unwrap_or_default());
    if targets.is_some() && preferred.is_none() {
        return ImageRead::NoImage;
    }
    let mut tries: Vec<String> = preferred.into_iter().collect();
    for t in SUPPORTED_IMAGE_MIME_TYPES {
        if !tries.iter().any(|x| x == t) {
            tries.push(t.to_string());
        }
    }
    for mime_type in tries {
        if let Some(bytes) =
            run("xclip", &["-selection", "clipboard", "-t", &mime_type, "-o"]).filter(|d| !d.is_empty())
        {
            return ImageRead::Image {
                bytes,
                mime_type: base_mime_type(&mime_type),
            };
        }
    }
    ImageRead::Failed
}

/// `readClipboardImage` on Linux, without the PowerShell and native fallbacks: `Image` (converted to PNG unless it
/// is a supported type) or why there is none.
pub fn read_clipboard_image(
    var: &dyn Fn(&str) -> Option<String>,
    proc_version: Option<&str>,
    run: &mut RunTool<'_>,
) -> ImageRead {
    if var("TERMUX_VERSION").is_some_and(|v| !v.is_empty()) {
        return ImageRead::NoImage;
    }
    let mut image = ImageRead::Failed;
    if is_wayland_session(var) || is_wsl(var, proc_version) {
        image = via_wl_paste(run);
    }
    if image == ImageRead::Failed {
        image = via_xclip(run);
    }
    match image {
        ImageRead::Image { bytes, mime_type } if !is_supported_image_mime_type(&mime_type) => {
            match convert_to_png(&bytes) {
                Some(bytes) => ImageRead::Image {
                    bytes,
                    mime_type: "image/png".to_string(),
                },
                None => ImageRead::NoImage,
            }
        }
        image => image,
    }
}
