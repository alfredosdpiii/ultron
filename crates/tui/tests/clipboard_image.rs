//! Clipboard image reads on Linux: replays fixtures/clipboard-image.json (utils/clipboard-image.ts
//! `readClipboardImage` over fake `wl-paste`/`xclip`, recorded by fixtures/generate-clipboard-image.mjs) through
//! `tui::clipboard_image` with the same fake tools on an injected PATH, comparing the image and every tool call.
//! PNG, JPEG, WebP and GIF images pass through byte for byte; a BMP, TIFF (first page of a multi-page one) or ICO
//! (the entry Photon's decoder picks) is converted to PNG (Photon there, the `image` crate here), compared by its
//! decoded RGBA pixels since the two PNG encoders compress differently.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;

use serde_json::Value;
use tui::clipboard::{Clipboard, ClipboardImage, SystemClipboard, tool_run};
use tui::clipboard_image::{ImageRead, extension_for_image_mime_type, is_wayland_session, read_clipboard_image};

/// generate-clipboard-image.mjs `FAKE`, with the scenario directory written in (the test cannot set its own
/// environment for the child).
fn fake(tool: &str, scenario: &str) -> String {
    format!(
        "#!/bin/sh\nCLIP_SCENARIO='{scenario}'\nkey=$(printf '%s_' \"$@\" | tr '/' '-')\nprintf '%s %s\\n' \"{tool}\" \"$key\" >> \"$CLIP_SCENARIO/log\"\nf=\"$CLIP_SCENARIO/{tool}.$key.out\"\n[ -f \"$f\" ] || exit 1\ncat \"$f\"\n"
    )
}

/// A private temp dir, removed on drop (tui has no tempfile dependency).
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(i: usize) -> Self {
        let dir = std::env::temp_dir().join(format!("tui-clipboard-image-{}-{i}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The fixture's latin1 strings as bytes.
fn latin1(text: &str) -> Vec<u8> {
    text.chars().map(|c| c as u32 as u8).collect()
}

/// A PNG's size, color type and pixels as RGBA8.
fn rgba(png: &[u8]) -> (u32, u32, String, Vec<u8>) {
    let image = image::load_from_memory_with_format(png, image::ImageFormat::Png).unwrap();
    let color = format!("{:?}", image.color());
    let rgba = image.to_rgba8();
    (rgba.width(), rgba.height(), color, rgba.into_raw())
}

/// The fake tools of [`fake`] in `tmp/bin`, answering from `tmp/s<i>` with the files of `scenario`.
fn fake_tools(tmp: &TempDir, i: usize, files: &Value) -> (std::path::PathBuf, std::path::PathBuf) {
    let (bin, scenario) = (tmp.0.join("bin"), tmp.0.join(format!("s{i}")));
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&scenario).unwrap();
    for tool in ["wl-paste", "xclip"] {
        let path = bin.join(tool);
        std::fs::write(&path, fake(tool, &scenario.display().to_string())).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    for (name, text) in files.as_object().unwrap() {
        std::fs::write(scenario.join(format!("{name}.out")), latin1(text.as_str().unwrap())).unwrap();
    }
    std::fs::write(scenario.join("log"), "").unwrap();
    (bin, scenario)
}

#[test]
fn tui_clipboard_image_reads_like_ts() {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/clipboard-image.json")).unwrap();
    assert_eq!(fixture["cases"].as_array().unwrap().len(), 32);
    for (i, case) in fixture["cases"].as_array().unwrap().iter().enumerate() {
        let tmp = TempDir::new(i);
        let (bin, scenario) = fake_tools(&tmp, i, &fixture["scenarios"][case["scenario"].as_str().unwrap()]);
        let env: HashMap<String, String> = case["env"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
            .collect();
        // The fake tools, then the system's (`tr`, `cat` for the scripts themselves run from /usr/bin).
        let path = format!("{}:/usr/bin:/bin", bin.display());
        let mut run = |program: &str, args: &[&str]| tool_run(Some(&path), program, args);
        let got = read_clipboard_image(&|k| env.get(k).cloned(), None, &mut run);
        let want = match &case["image"] {
            Value::Null => None,
            image => Some((
                latin1(image["bytes"].as_str().unwrap()),
                image["mimeType"].as_str().unwrap().to_string(),
            )),
        };
        let got = match got {
            ImageRead::Image { bytes, mime_type } => Some((bytes, mime_type)),
            ImageRead::NoImage | ImageRead::Failed => None,
        };
        let name = case["scenario"].as_str().unwrap();
        let converted = ["-bmp", "-tiff", "-ico"].iter().any(|kind| name.contains(kind)) && name != "x11-tiff-and-png";
        match (&got, &want) {
            (Some((got_bytes, got_type)), Some((want_bytes, want_type))) if converted => {
                assert_eq!(got_type, want_type, "{case}");
                assert_eq!(rgba(got_bytes), rgba(want_bytes), "{case}");
                assert!(got_bytes.starts_with(b"\x89PNG\r\n\x1a\n"), "{case}");
            }
            _ => assert_eq!(got, want, "{case}"),
        }
        assert_eq!(
            std::fs::read_to_string(scenario.join("log")).unwrap(),
            case["calls"].as_str().unwrap(),
            "{case}"
        );
    }
}

/// `isWaylandSession`: `WAYLAND_DISPLAY` (non-empty) or `XDG_SESSION_TYPE` exactly `wayland`.
#[test]
fn tui_wayland_session_reads_xdg_session_type() {
    let env = |pairs: &'static [(&'static str, &'static str)]| {
        move |k: &str| pairs.iter().find(|(name, _)| *name == k).map(|(_, v)| v.to_string())
    };
    assert!(is_wayland_session(&env(&[("XDG_SESSION_TYPE", "wayland")])));
    assert!(is_wayland_session(&env(&[("WAYLAND_DISPLAY", "wayland-1")])));
    assert!(!is_wayland_session(&env(&[("XDG_SESSION_TYPE", "x11")])));
    assert!(!is_wayland_session(&env(&[("XDG_SESSION_TYPE", "Wayland")])));
    assert!(!is_wayland_session(&env(&[("WAYLAND_DISPLAY", "")])));
    assert!(!is_wayland_session(&env(&[])));
}

/// `extensionForImageMimeType`: the pasted file's extension, by the base type, case-insensitively.
#[test]
fn tui_clipboard_image_extension_for_mime_type_like_ts() {
    let cases = [
        ("image/png", Some("png")),
        ("image/jpeg", Some("jpg")),
        ("IMAGE/JPEG; q=1", Some("jpg")),
        ("image/webp", Some("webp")),
        ("image/gif", Some("gif")),
        ("image/bmp", None),
        ("image/jpg", None),
        ("text/plain", None),
    ];
    for (mime_type, extension) in cases {
        assert_eq!(extension_for_image_mime_type(mime_type), extension, "{mime_type}");
    }
}

/// The system clipboard keeps a JPEG (and its type) instead of refusing every non-PNG image, and converts a BMP;
/// its tools come only from the injected PATH.
#[test]
fn tui_system_clipboard_reads_typed_images_from_the_injected_path() {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/clipboard-image.json")).unwrap();
    let tmp = TempDir::new(1000);
    let cases = [
        ("x11-jpeg", "image/jpeg"),
        ("x11-bmp", "image/png"),
        ("x11-tiff", "image/png"),
        ("wayland-tiff-pages", ""),
        ("x11-ico-sizes", "image/png"),
        ("x11-tiff-truncated", ""),
        ("x11-undecodable", ""),
    ];
    for (i, (scenario, mime_type)) in cases.into_iter().enumerate() {
        let (bin, _) = fake_tools(&tmp, 1000 + i, &fixture["scenarios"][scenario]);
        let path = format!("{}:/usr/bin:/bin", bin.display());
        let mut clipboard = SystemClipboard::with_env(Some(path), vec![("DISPLAY".into(), ":0".into())]);
        match clipboard.get_image() {
            Ok(ClipboardImage { bytes, mime_type: got }) => {
                assert_eq!(got, mime_type, "{scenario}");
                assert_eq!(clipboard.get_image_png().is_ok(), got == "image/png", "{scenario}");
                if scenario == "x11-jpeg" {
                    assert_eq!(bytes, b"\xff\xd8\xff\xe0jpeg");
                }
            }
            Err(error) => assert_eq!(
                (mime_type, error.as_str()),
                ("", "no image on the clipboard"),
                "{scenario}"
            ),
        }
    }
    // No PATH: no tool runs, so there is no image.
    let mut clipboard = SystemClipboard::with_env(None, vec![("DISPLAY".into(), ":0".into())]);
    assert!(clipboard.get_image().is_err());
}
