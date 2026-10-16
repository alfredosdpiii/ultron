//! Clipboard image reads on Linux: replays fixtures/clipboard-image.json (utils/clipboard-image.ts
//! `readClipboardImage` over fake `wl-paste`/`xclip`, recorded by fixtures/generate-clipboard-image.mjs) through
//! `tui::clipboard_image` with the same fake tools on an injected PATH, comparing the image and every tool call.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;

use serde_json::Value;
use tui::clipboard::tool_run;
use tui::clipboard_image::{ImageRead, is_wayland_session, read_clipboard_image};

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

#[test]
fn tui_clipboard_image_reads_like_ts() {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/clipboard-image.json")).unwrap();
    for (i, case) in fixture["cases"].as_array().unwrap().iter().enumerate() {
        let tmp = TempDir::new(i);
        let (bin, scenario) = (tmp.0.join("bin"), tmp.0.join(format!("s{i}")));
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&scenario).unwrap();
        for tool in ["wl-paste", "xclip"] {
            let path = bin.join(tool);
            std::fs::write(&path, fake(tool, &scenario.display().to_string())).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let files = &fixture["scenarios"][case["scenario"].as_str().unwrap()];
        for (name, text) in files.as_object().unwrap() {
            std::fs::write(scenario.join(format!("{name}.out")), latin1(text.as_str().unwrap())).unwrap();
        }
        std::fs::write(scenario.join("log"), "").unwrap();
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
        assert_eq!(got, want, "{case}");
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
