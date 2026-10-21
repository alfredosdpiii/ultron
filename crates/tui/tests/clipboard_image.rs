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
use tui::clipboard_image::{
    ImageRead, ImageSources, extension_for_image_mime_type, is_wayland_session, read_clipboard_image,
};

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
    // A script just written can fail to start with ETXTBSY while another test thread's child, forked during the
    // write, still holds the write descriptor (it closes at that child's exec): under load the product's first tool
    // call then failed. Start each fake once until it runs; the log is reset below.
    for tool in ["wl-paste", "xclip"] {
        let path = bin.join(tool);
        let guard = std::time::Instant::now() + std::time::Duration::from_secs(120);
        loop {
            match std::process::Command::new(&path).arg("probe").output() {
                Err(e) if e.raw_os_error() == Some(26) && std::time::Instant::now() < guard => std::thread::yield_now(),
                Err(e) => panic!("{tool}: {e}"),
                Ok(_) => break,
            }
        }
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
        // Every recorded case ends before the native clipboard and (with a Linux image under WSL) before PowerShell.
        let got = read_clipboard_image(ImageSources {
            platform: "linux",
            var: &|k| env.get(k).cloned(),
            proc_version: None,
            run: &mut run,
            native: &mut || panic!("native clipboard read"),
            temp_file: &|| panic!("PowerShell fallback"),
        });
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

/// How the fake Windows side of [`wsl`] answers.
#[derive(Clone, Copy)]
enum Windows {
    /// `wslpath` fails.
    NoWslpath,
    /// `wslpath` prints only blanks.
    BlankWslpath,
    /// PowerShell fails.
    NoPowershell,
    /// PowerShell prints this and writes these bytes to the Windows path's file (None: writes nothing).
    Prints(&'static str, Option<&'static [u8]>),
}

/// One WSL read (`WSL_DISTRO_NAME` set) with `linux` answering the Linux tools (program and arguments joined by
/// spaces) and `windows` the `wslpath`/`powershell.exe` calls: the image, every call in order, and whether the temp
/// file is still there afterwards. `wslpath -w <file>` maps the file to `C:\Users\it's\<name>`, so the script must
/// quote the path's apostrophe.
fn wsl(linux: &[(&str, &[u8])], windows: Windows) -> (ImageRead, Vec<String>, bool) {
    let tmp = TempDir::new(3000 + linux.len() * 10 + windows_index(windows));
    let temp_file = tmp.0.join("pi-wsl-clip-1.png");
    let calls = std::cell::RefCell::new(Vec::new());
    let mut run = |program: &str, args: &[&str]| -> Option<Vec<u8>> {
        calls.borrow_mut().push(format!("{program} {}", args.join(" ")));
        match program {
            "wslpath" => {
                assert_eq!(args, ["-w", temp_file.to_str().unwrap()]);
                match windows {
                    Windows::NoWslpath => None,
                    Windows::BlankWslpath => Some(b"  \r\n".to_vec()),
                    _ => Some(b"C:\\Users\\it's\\pi-wsl-clip-1.png\r\n".to_vec()),
                }
            }
            "powershell.exe" => match windows {
                Windows::Prints(output, bytes) => {
                    if let Some(bytes) = bytes {
                        std::fs::write(&temp_file, bytes).unwrap();
                    }
                    Some(output.as_bytes().to_vec())
                }
                _ => None,
            },
            _ => {
                let key = format!("{program} {}", args.join(" "));
                linux.iter().find(|(k, _)| *k == key).map(|(_, out)| out.to_vec())
            }
        }
    };
    let got = read_clipboard_image(ImageSources {
        platform: "linux",
        var: &|k| (k == "WSL_DISTRO_NAME").then(|| "Ubuntu".to_string()),
        proc_version: None,
        run: &mut run,
        native: &mut || panic!("native clipboard read"),
        temp_file: &|| temp_file.clone(),
    });
    let left = temp_file.exists();
    (got, calls.into_inner(), left)
}

fn windows_index(windows: Windows) -> usize {
    match windows {
        Windows::NoWslpath => 0,
        Windows::BlankWslpath => 1,
        Windows::NoPowershell => 2,
        Windows::Prints(output, bytes) => 3 + output.len() + bytes.map_or(0, <[u8]>::len),
    }
}

/// `readClipboardImageViaPowerShell` (utils/clipboard-image.ts): under WSL with no Linux image, the Windows
/// clipboard through `wslpath -w` and `powershell.exe -NoProfile -Command <script>`, whose `$path` is the Windows path
/// with `'` doubled; `ok` and a non-empty file are a PNG, anything else no image (an empty Linux clipboard stays
/// no image); the temp file is always removed. A Linux image never asks Windows.
#[test]
fn tui_clipboard_image_wsl_powershell_fallback() {
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nwindows-png";
    let script = "Add-Type -AssemblyName System.Windows.Forms; Add-Type -AssemblyName System.Drawing; \
                  $path = 'C:\\Users\\it''s\\pi-wsl-clip-1.png'; $img = [System.Windows.Forms.Clipboard]::GetImage(); \
                  if ($img) { $img.Save($path, [System.Drawing.Imaging.ImageFormat]::Png); Write-Output 'ok' } else \
                  { Write-Output 'empty' }";
    assert_eq!(
        tui::clipboard_image::powershell_script("C:\\Users\\it's\\pi-wsl-clip-1.png"),
        script
    );
    let text_only: &[(&str, &[u8])] = &[("wl-paste --list-types", b"text/plain\n")];
    let calls = |extra: &[&str]| -> Vec<String> {
        let mut c = vec!["wl-paste --list-types".to_string()];
        c.extend(extra.iter().map(|s| s.to_string()));
        c
    };
    let wslpath = "wslpath -w <file>";
    let powershell = format!("powershell.exe -NoProfile -Command {script}");
    let normalize = |(got, calls, left): (ImageRead, Vec<String>, bool)| {
        let calls: Vec<String> = calls
            .into_iter()
            .map(|c| {
                if c.starts_with("wslpath -w ") {
                    wslpath.to_string()
                } else {
                    c
                }
            })
            .collect();
        (got, calls, left)
    };
    let png = ImageRead::Image {
        bytes: PNG.to_vec(),
        mime_type: "image/png".into(),
    };
    // The Windows clipboard's image, as a PNG; the temp file is gone.
    assert_eq!(
        normalize(wsl(text_only, Windows::Prints("ok\r\n", Some(PNG)))),
        (png.clone(), calls(&[wslpath, &powershell]), false)
    );
    // Whatever PowerShell saved is taken as a PNG, unconverted (TS sets the type, never checks the bytes).
    assert_eq!(
        normalize(wsl(text_only, Windows::Prints("ok", Some(b"BM-not-converted")))).0,
        ImageRead::Image {
            bytes: b"BM-not-converted".to_vec(),
            mime_type: "image/png".into()
        }
    );
    // No image on Windows either: an empty answer, an empty file, other output, a failure.
    for windows in [
        Windows::Prints("empty", None),
        Windows::Prints("ok", Some(b"")),
        Windows::Prints("ok then more", Some(PNG)),
        Windows::NoPowershell,
    ] {
        assert_eq!(
            normalize(wsl(text_only, windows)),
            (ImageRead::NoImage, calls(&[wslpath, &powershell]), false)
        );
    }
    // No Windows path: PowerShell never runs.
    for windows in [Windows::NoWslpath, Windows::BlankWslpath] {
        assert_eq!(
            normalize(wsl(text_only, windows)),
            (ImageRead::NoImage, calls(&[wslpath]), false)
        );
    }
    // A Linux image (Wayland, or X11 after wl-paste failed) never asks Windows.
    let wayland: &[(&str, &[u8])] = &[
        ("wl-paste --list-types", b"image/png\n"),
        ("wl-paste --type image/png --no-newline", b"\x89PNG\r\n\x1a\nlinux"),
    ];
    assert_eq!(
        normalize(wsl(wayland, Windows::Prints("ok", Some(PNG)))),
        (
            ImageRead::Image {
                bytes: b"\x89PNG\r\n\x1a\nlinux".to_vec(),
                mime_type: "image/png".into()
            },
            calls(&["wl-paste --type image/png --no-newline"]),
            false
        )
    );
    let x11: &[(&str, &[u8])] = &[
        ("xclip -selection clipboard -t TARGETS -o", b"image/jpeg\n"),
        ("xclip -selection clipboard -t image/jpeg -o", b"\xff\xd8\xff\xe0x11"),
    ];
    let (got, got_calls, _) = normalize(wsl(x11, Windows::Prints("ok", Some(PNG))));
    assert_eq!(
        got,
        ImageRead::Image {
            bytes: b"\xff\xd8\xff\xe0x11".to_vec(),
            mime_type: "image/jpeg".into()
        }
    );
    assert_eq!(
        got_calls,
        calls(&[
            "xclip -selection clipboard -t TARGETS -o",
            "xclip -selection clipboard -t image/jpeg -o"
        ])
    );
    // X11 ran and had no image: Windows is asked next, and its image wins.
    let x11_text: &[(&str, &[u8])] = &[("xclip -selection clipboard -t TARGETS -o", b"UTF8_STRING\n")];
    assert_eq!(
        normalize(wsl(x11_text, Windows::Prints("ok", Some(PNG)))),
        (
            png,
            calls(&["xclip -selection clipboard -t TARGETS -o", wslpath, &powershell]),
            false
        )
    );
}

/// A fake clipboard tool runner.
type Tools = dyn Fn(&str, &[&str]) -> Option<Vec<u8>>;

/// The native clipboard (`readClipboardImageViaNativeClipboard`): the only reader off Linux, and on Linux only after
/// every tool failed (an empty clipboard, or a Windows "no image" under WSL, never reaches it). Its image is
/// converted unless supported; an unavailable native clipboard is no image.
#[test]
fn tui_clipboard_image_native_fallback_order() {
    let image = || ImageRead::Image {
        bytes: b"\x89PNG\r\n\x1a\nnative".to_vec(),
        mime_type: "image/png".to_string(),
    };
    let read = |platform: &str, vars: &[(&str, &str)], tools: &Tools, native: ImageRead| {
        let calls = std::cell::RefCell::new(Vec::<String>::new());
        let natives = std::cell::Cell::new(0);
        let var = |k: &str| vars.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string());
        let mut run = |program: &str, args: &[&str]| {
            calls.borrow_mut().push(format!("{program} {}", args.join(" ")));
            tools(program, args)
        };
        let mut native_read = || {
            natives.set(natives.get() + 1);
            native.clone()
        };
        let got = read_clipboard_image(ImageSources {
            platform,
            var: &var,
            proc_version: None,
            run: &mut run,
            native: &mut native_read,
            temp_file: &|| std::path::PathBuf::from("/nonexistent/pi-wsl-clip-x.png"),
        });
        (got, calls.into_inner(), natives.get())
    };
    let none = |_: &str, _: &[&str]| None;
    // macOS and Windows: the native clipboard only, no tool runs.
    for platform in ["darwin", "win32"] {
        assert_eq!(
            read(platform, &[("DISPLAY", ":0")], &none, image()),
            (image(), vec![], 1)
        );
        assert_eq!(
            read(platform, &[], &none, ImageRead::NoImage),
            (ImageRead::NoImage, vec![], 1)
        );
        assert_eq!(
            read(platform, &[], &none, ImageRead::Failed),
            (ImageRead::Failed, vec![], 1)
        );
    }
    // Termux: nothing at all.
    assert_eq!(
        read("darwin", &[("TERMUX_VERSION", "1")], &none, image()),
        (ImageRead::NoImage, vec![], 0)
    );
    // Linux with every tool missing: the native clipboard last.
    let (got, calls, natives) = read("linux", &[("DISPLAY", ":0")], &none, image());
    assert_eq!((got, natives), (image(), 1));
    assert_eq!(calls[0], "xclip -selection clipboard -t TARGETS -o");
    // Linux, xclip ran and found no image: the native clipboard is not asked.
    let targets =
        |program: &str, args: &[&str]| (program == "xclip" && args[3] == "TARGETS").then(|| b"UTF8_STRING\n".to_vec());
    let (got, calls, natives) = read("linux", &[("DISPLAY", ":0")], &targets, image());
    assert_eq!((got, calls.len(), natives), (ImageRead::NoImage, 1, 0));
    // WSL with every tool failing (wslpath too): the native clipboard after PowerShell.
    let (got, calls, natives) = read("linux", &[("WSL_DISTRO_NAME", "Ubuntu")], &none, image());
    assert_eq!((got, natives), (image(), 1));
    assert_eq!(calls.first().map(String::as_str), Some("wl-paste --list-types"));
    assert_eq!(
        calls.last().map(String::as_str),
        Some("wslpath -w /nonexistent/pi-wsl-clip-x.png")
    );
    // A native image of another type is converted to PNG; an undecodable one is no image.
    let bmp = ImageRead::Image {
        bytes: b"not an image".to_vec(),
        mime_type: "image/bmp".to_string(),
    };
    assert_eq!(read("win32", &[], &none, bmp).0, ImageRead::NoImage);
}

/// The native reader's RGBA image becomes a PNG of the same pixels.
#[test]
fn tui_clipboard_image_rgba_to_png_round_trips() {
    let pixels = vec![255, 0, 0, 255, 0, 255, 0, 128, 0, 0, 255, 0, 9, 9, 9, 9];
    let png = tui::clipboard_image::rgba_to_png(2, 2, pixels.clone()).unwrap();
    assert_eq!(rgba(&png), (2, 2, "Rgba8".to_string(), pixels));
    // A buffer that does not match the size is no PNG.
    assert_eq!(tui::clipboard_image::rgba_to_png(3, 2, vec![0; 16]), None);
}
