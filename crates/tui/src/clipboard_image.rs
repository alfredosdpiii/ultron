//! Reading an image from the clipboard (utils/clipboard-image.ts `readClipboardImage`). On Linux: `wl-paste` only in
//! a Wayland session (`WAYLAND_DISPLAY`, or `XDG_SESSION_TYPE=wayland`) or under WSL, then `xclip`; under WSL with
//! no Linux image, the Windows clipboard through `powershell.exe` (a PNG saved to a temp file named by `wslpath`);
//! when every tool failed, the native clipboard. On macOS and Windows: the native clipboard only. Each tool lists
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
//! The native clipboard is arboard here (the native platform helper there): its RGBA image is encoded as a PNG. Every
//! reader is injected ([`ImageSources`]), so tests never touch a real clipboard or run PowerShell.

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

/// An RGBA image (the native clipboard's) as a PNG.
pub fn rgba_to_png(width: u32, height: u32, rgba: Vec<u8>) -> Option<Vec<u8>> {
    let image = ::image::RgbaImage::from_raw(width, height, rgba)?;
    let mut png = Vec::new();
    image
        .write_to(&mut std::io::Cursor::new(&mut png), ::image::ImageFormat::Png)
        .ok()?;
    Some(png)
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

/// The PowerShell script that saves the Windows clipboard's image to `win_path` as a PNG (`psScript`).
pub fn powershell_script(win_path: &str) -> String {
    let quoted = win_path.replace('\'', "''");
    [
        "Add-Type -AssemblyName System.Windows.Forms".to_string(),
        "Add-Type -AssemblyName System.Drawing".to_string(),
        format!("$path = '{quoted}'"),
        "$img = [System.Windows.Forms.Clipboard]::GetImage()".to_string(),
        "if ($img) { $img.Save($path, [System.Drawing.Imaging.ImageFormat]::Png); Write-Output 'ok' } else { Write-Output 'empty' }".to_string(),
    ]
    .join("; ")
}

/// `readClipboardImageViaPowerShell`: the Windows clipboard's image under WSL, saved by PowerShell to `temp_file`
/// (named for Windows by `wslpath -w`), read back and removed. `None` when there is none or any step failed.
fn via_powershell(run: &mut RunTool<'_>, temp_file: &std::path::Path) -> Option<ImageRead> {
    let image = powershell_image(run, temp_file);
    let _ = std::fs::remove_file(temp_file);
    image
}

fn powershell_image(run: &mut RunTool<'_>, temp_file: &std::path::Path) -> Option<ImageRead> {
    {
        let win_path = run("wslpath", &["-w", &temp_file.to_string_lossy()])?;
        let win_path = String::from_utf8_lossy(&win_path).trim().to_string();
        if win_path.is_empty() {
            return None;
        }
        let output = run(
            "powershell.exe",
            &["-NoProfile", "-Command", &powershell_script(&win_path)],
        )?;
        if String::from_utf8_lossy(&output).trim() != "ok" {
            return None;
        }
        let bytes = std::fs::read(temp_file).ok().filter(|b| !b.is_empty())?;
        Some(ImageRead::Image {
            bytes,
            mime_type: "image/png".to_string(),
        })
    }
}

/// The readers [`read_clipboard_image`] uses: clipboard tools, the native clipboard, and where the WSL PowerShell
/// fallback saves its PNG.
pub struct ImageSources<'a, 'b> {
    /// `process.platform`: `linux`, `darwin` or `win32`.
    pub platform: &'a str,
    pub var: &'a dyn Fn(&str) -> Option<String>,
    /// `/proc/version`, for `isWSL`.
    pub proc_version: Option<&'a str>,
    pub run: &'a mut RunTool<'b>,
    /// `readClipboardImageViaNativeClipboard`: an image, no image, or unavailable (`Failed`).
    pub native: &'a mut dyn FnMut() -> ImageRead,
    /// A fresh temp file path for the PowerShell fallback (`pi-wsl-clip-<id>.png` in the temp dir).
    pub temp_file: &'a dyn Fn() -> std::path::PathBuf,
}

/// `readClipboardImage`: `Image` (converted to PNG unless it is a supported type) or why there is none.
pub fn read_clipboard_image(sources: ImageSources<'_, '_>) -> ImageRead {
    let ImageSources {
        platform,
        var,
        proc_version,
        run,
        native,
        temp_file,
    } = sources;
    if var("TERMUX_VERSION").is_some_and(|v| !v.is_empty()) {
        return ImageRead::NoImage;
    }
    let mut image = ImageRead::Failed;
    if platform == "linux" {
        let wsl = is_wsl(var, proc_version);
        if is_wayland_session(var) || wsl {
            image = via_wl_paste(run);
        }
        if image == ImageRead::Failed {
            image = via_xclip(run);
        }
        // Linux's empty/unavailable distinction stays when Windows has no image either.
        if wsl && !matches!(image, ImageRead::Image { .. }) {
            image = via_powershell(run, &temp_file()).unwrap_or(image);
        }
        if image == ImageRead::Failed {
            image = native();
        }
    } else {
        image = native();
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
