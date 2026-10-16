//! The terminal pieces of the client TUI's live settings and side pane: `copyToClipboard` (ported
//! packages/coding-agent test/clipboard.test.ts, literal expectations, over a recording [`CopyIo`]), `SplitView`
//! (fixtures/split-view-parity.json from `node crates/tui/fixtures/generate-split-view.mjs`), a live theme swap,
//! the editor's padding and the split viewport's clear-on-shrink.

use std::path::{Path, PathBuf};

use serde_json::Value;
use tui::clipboard_copy::{CopyEnv, CopyIo, copy_to_clipboard};

/// Records every command and answers from a script: `fail` lists the commands that fail.
#[derive(Default)]
struct Io {
    calls: Vec<(String, Vec<String>, Option<String>)>,
    /// `None`: no native writer; `Some(ok)`: a writer that succeeds or rejects.
    native: Option<bool>,
    native_calls: usize,
    fail: Vec<&'static str>,
    wslpath: Option<&'static str>,
    temp: Vec<(PathBuf, String)>,
    removed: Vec<PathBuf>,
}

impl CopyIo for Io {
    fn native_set_text(&mut self, _text: &str) -> Option<Result<(), String>> {
        self.native_calls += 1;
        self.native
            .map(|ok| if ok { Ok(()) } else { Err("native failed".into()) })
    }
    fn run(&mut self, command: &str, args: &[String], input: Option<&str>, _timeout_ms: u64) -> Option<Vec<u8>> {
        self.calls
            .push((command.into(), args.to_vec(), input.map(str::to_string)));
        if self.fail.contains(&command) {
            return None;
        }
        if command == "wslpath" {
            return Some(self.wslpath.unwrap_or("").as_bytes().to_vec());
        }
        Some(Vec::new())
    }
    fn write_temp(&mut self, text: &str) -> Result<PathBuf, String> {
        let path = PathBuf::from(format!("/virtual/pi-wsl-clip-{}.txt", self.temp.len()));
        self.temp.push((path.clone(), text.into()));
        Ok(path)
    }
    fn remove(&mut self, path: &Path) {
        self.removed.push(path.to_path_buf());
    }
}

impl Io {
    fn names(&self) -> Vec<&str> {
        self.calls.iter().map(|(c, _, _)| c.as_str()).collect()
    }
}

fn copy(platform: &str, vars: &[(&str, &str)], io: &mut Io, text: &str) -> Result<Option<String>, String> {
    let owned: Vec<(String, String)> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let var = move |name: &str| owned.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
    let env = CopyEnv {
        platform,
        var: &var,
        proc_version: Some("Linux version 6.9 (gcc)".into()),
    };
    copy_to_clipboard(text, &env, io)
}

fn native() -> Io {
    Io {
        native: Some(true),
        ..Io::default()
    }
}

#[test]
fn tui_copy_local_native_success_skips_osc52_and_commands() {
    let mut io = native();
    assert_eq!(copy("darwin", &[], &mut io, "hello"), Ok(None));
    assert_eq!(io.native_calls, 1);
    assert!(io.calls.is_empty());
}

#[test]
fn tui_copy_linux_skips_the_native_writer_and_uses_xclip() {
    let mut io = native();
    assert_eq!(copy("linux", &[("DISPLAY", ":0")], &mut io, "hello"), Ok(None));
    assert_eq!(io.native_calls, 0);
    assert_eq!(
        io.calls,
        [(
            "xclip".to_string(),
            vec!["-selection".to_string(), "clipboard".to_string()],
            Some("hello".to_string())
        )]
    );
}

#[test]
fn tui_copy_remote_native_success_still_emits_osc52() {
    let mut io = native();
    assert_eq!(
        copy("darwin", &[("SSH_CONNECTION", "client server")], &mut io, "hello"),
        Ok(Some("\x1b]52;c;aGVsbG8=\x07".into()))
    );
    assert!(io.calls.is_empty());
}

#[test]
fn tui_copy_rejected_or_missing_native_falls_back_to_pbcopy() {
    let mut io = Io {
        native: Some(false),
        ..Io::default()
    };
    assert_eq!(copy("darwin", &[], &mut io, "hello"), Ok(None));
    assert_eq!(io.calls, [("pbcopy".to_string(), vec![], Some("hello".to_string()))]);
    let mut io = Io::default();
    assert_eq!(copy("darwin", &[], &mut io, "hello"), Ok(None));
    assert_eq!(io.calls.len(), 1);
}

#[test]
fn tui_copy_tries_xclip_and_xsel_after_wl_copy_fails() {
    let mut io = Io {
        fail: vec!["wl-copy", "xclip"],
        ..Io::default()
    };
    assert_eq!(
        copy(
            "linux",
            &[("WAYLAND_DISPLAY", "wayland-0"), ("DISPLAY", ":0")],
            &mut io,
            "hello"
        ),
        Ok(None)
    );
    assert_eq!(io.names(), ["wl-copy", "xclip", "xsel"]);
}

#[test]
fn tui_copy_local_linux_failure_is_reported_not_osc52() {
    let mut io = Io {
        fail: vec!["xclip", "xsel"],
        ..Io::default()
    };
    assert_eq!(
        copy("linux", &[("DISPLAY", ":0")], &mut io, "hello"),
        Err("Clipboard unavailable: install `xclip` or `xsel`, or check X11 access".into())
    );
    assert_eq!(io.names(), ["xclip", "xsel"]);
    let mut io = Io {
        fail: vec!["wl-copy", "xclip", "xsel"],
        ..Io::default()
    };
    assert_eq!(
        copy(
            "linux",
            &[("WAYLAND_DISPLAY", "wayland-0"), ("DISPLAY", ":0")],
            &mut io,
            "hello"
        ),
        Err("Clipboard unavailable: install `wl-clipboard` (`wl-copy`) or check Wayland access".into())
    );
    let mut io = Io {
        fail: vec!["termux-clipboard-set"],
        ..Io::default()
    };
    assert_eq!(
        copy("linux", &[("TERMUX_VERSION", "1")], &mut io, "hello"),
        Err("Clipboard unavailable: install the Termux:API app and `termux-api` package".into())
    );
}

#[test]
fn tui_copy_display_less_linux_falls_back_to_osc52() {
    let mut io = Io::default();
    assert_eq!(
        copy("linux", &[], &mut io, "hello"),
        Ok(Some("\x1b]52;c;aGVsbG8=\x07".into()))
    );
    assert!(io.calls.is_empty());
}

#[test]
fn tui_copy_wsl_without_display_uses_powershell_through_a_temp_file() {
    let mut io = Io {
        wslpath: Some("\\\\wsl.localhost\\Ubuntu\\tmp\\clip.txt\n"),
        ..Io::default()
    };
    assert_eq!(
        copy("linux", &[("WSL_DISTRO_NAME", "Ubuntu")], &mut io, "héllo"),
        Ok(None)
    );
    assert_eq!(io.names(), ["wslpath", "powershell.exe"]);
    assert_eq!(io.temp[0].1, "héllo");
    assert_eq!(io.removed, [io.temp[0].0.clone()]);
    let script = &io.calls[1].1[2];
    assert!(script.contains("Set-Clipboard"), "{script}");
    assert!(
        script.contains("'\\\\wsl.localhost\\Ubuntu\\tmp\\clip.txt'"),
        "{script}"
    );
}

#[test]
fn tui_copy_wsl_interop_unavailable_falls_back_to_osc52() {
    let mut io = Io {
        fail: vec!["wslpath"],
        ..Io::default()
    };
    assert!(
        copy("linux", &[("WSL_DISTRO_NAME", "Ubuntu")], &mut io, "hello")
            .unwrap()
            .is_some()
    );
    assert_eq!(io.names(), ["wslpath"]);
}

#[test]
fn tui_copy_wsl_windows_terminal_prefers_osc52_once() {
    let mut io = Io::default();
    let vars = [("WSL_DISTRO_NAME", "Ubuntu"), ("WT_SESSION", "s")];
    assert!(copy("linux", &vars, &mut io, "hello").unwrap().is_some());
    assert!(io.calls.is_empty());
    let mut io = Io::default();
    let vars = [
        ("WSL_DISTRO_NAME", "Ubuntu"),
        ("WT_SESSION", "s"),
        ("SSH_CONNECTION", "c s"),
    ];
    assert_eq!(
        copy("linux", &vars, &mut io, "hello"),
        Ok(Some("\x1b]52;c;aGVsbG8=\x07".into()))
    );
    // An oversized payload goes through PowerShell instead.
    let mut io = Io {
        wslpath: Some("C:\\clip.txt"),
        ..Io::default()
    };
    let vars = [("WSL_DISTRO_NAME", "Ubuntu"), ("WT_SESSION", "s")];
    assert_eq!(copy("linux", &vars, &mut io, &"x".repeat(80_000)), Ok(None));
    assert_eq!(io.names(), ["wslpath", "powershell.exe"]);
}

#[test]
fn tui_copy_wsl_with_a_display_prefers_linux_tools() {
    let mut io = Io::default();
    let vars = [("WSL_DISTRO_NAME", "Ubuntu"), ("WAYLAND_DISPLAY", "wayland-0")];
    assert_eq!(copy("linux", &vars, &mut io, "hello"), Ok(None));
    assert_eq!(io.names(), ["wl-copy"]);
}

#[test]
fn tui_copy_remote_failure_uses_osc52_unless_oversized() {
    let mut io = Io {
        native: Some(false),
        fail: vec!["pbcopy"],
        ..Io::default()
    };
    assert!(
        copy("darwin", &[("SSH_CONNECTION", "c s")], &mut io, "hello")
            .unwrap()
            .is_some()
    );
    let mut io = Io {
        native: Some(false),
        fail: vec!["pbcopy"],
        ..Io::default()
    };
    assert_eq!(
        copy("darwin", &[("SSH_CONNECTION", "c s")], &mut io, &"x".repeat(80_000)),
        Err("Clipboard unavailable: text exceeds the OSC 52 size limit".into())
    );
}

#[test]
fn tui_copy_detects_wsl_from_proc_version() {
    let var = |_: &str| None;
    let env = CopyEnv {
        platform: "linux",
        var: &var,
        proc_version: Some("Linux version 5.15.90.1-microsoft-standard-WSL2".into()),
    };
    assert!(tui::clipboard_copy::is_wsl(&env));
}

#[test]
fn tui_copy_clipboard_command_needs_the_injected_path() {
    // Without a PATH nothing is run (launchers come only from the injected environment).
    assert_eq!(
        tui::clipboard_copy::run_clipboard_command("sh", &[], None, 1000, None),
        None
    );
    let empty = std::env::temp_dir().join(format!("p-tui-empty-path-{}", std::process::id()));
    std::fs::create_dir_all(&empty).unwrap();
    assert_eq!(
        tui::clipboard_copy::run_clipboard_command("sh", &[], None, 1000, Some(&empty.display().to_string())),
        None
    );
    let _ = std::fs::remove_dir_all(empty);
}

#[test]
fn tui_split_view_matches_typescript() {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/split-view-parity.json")).unwrap();
    let lines = |v: &Value| -> Vec<String> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|l| l.as_str().unwrap().into())
            .collect()
    };
    let cases = fixture["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 32);
    for case in cases {
        let width = case["width"].as_u64().unwrap() as usize;
        let open = case["open"].as_bool().unwrap();
        let (main_width, side_width) =
            tui::split_view::split_widths(width, open, 44usize.max((width as f64 * 0.4).round() as usize));
        assert_eq!(
            (main_width as u64, side_width as u64),
            (case["mainWidth"].as_u64().unwrap(), case["sideWidth"].as_u64().unwrap()),
            "{case}"
        );
        let main = lines(&fixture["mains"][case["main"].as_u64().unwrap() as usize]);
        let side = lines(&fixture["sides"][case["side"].as_u64().unwrap() as usize]);
        let got = tui::split_view::render_split(&main, &side, main_width, side_width, "\x1b[90m│\x1b[39m");
        assert_eq!(got, lines(&case["lines"]), "{width} open={open}");
    }
}

#[test]
fn tui_split_view_keeps_the_editor_cursor_marker() {
    let main = vec![format!("ab{}c", tui::renderer::CURSOR_MARKER)];
    let got = tui::split_view::render_split(&main, &["x".into()], 10, 5, "│");
    assert!(got[0].contains(tui::renderer::CURSOR_MARKER), "{:?}", got[0]);
}

#[test]
fn tui_theme_adopt_recolors_every_holder() {
    use tui::theme::{ColorMode, builtin_theme_json, create_theme};
    let dark =
        std::rc::Rc::new(create_theme(&builtin_theme_json("dark").unwrap(), ColorMode::Truecolor, None).unwrap());
    let light = create_theme(&builtin_theme_json("light").unwrap(), ColorMode::Truecolor, None).unwrap();
    let held = std::rc::Rc::clone(&dark);
    let before = held.fg("accent", "x").unwrap();
    dark.adopt(&light);
    assert_eq!(held.fg("accent", "x").unwrap(), light.fg("accent", "x").unwrap());
    assert_ne!(held.fg("accent", "x").unwrap(), before);
    assert_eq!(held.thinking_border("high", "x"), light.thinking_border("high", "x"));
    // The name stays the holder's.
    assert_eq!(held.name.as_deref(), Some("dark"));
}

#[test]
fn tui_editor_padding_x_moves_the_text() {
    let keys = tui::keybindings::KeybindingsManager::new(
        tui::keybindings::app_keybindings(tui::keybindings::Platform::new("linux", false)),
        Vec::new(),
        Default::default(),
    );
    let mut editor = tui::editor::Editor::new(keys, Default::default(), Box::new(str::to_string), 0);
    editor.set_text("hi");
    let plain = editor.render(20);
    editor.set_padding_x(3);
    assert_eq!(editor.padding_x(), 3);
    let padded = editor.render(20);
    let row = |lines: &[String]| strip(&lines[1].replace(tui::renderer::CURSOR_MARKER, ""));
    assert!(row(&plain).starts_with("hi"), "{:?}", row(&plain));
    assert!(row(&padded).starts_with("   hi"), "{:?}", row(&padded));
}

#[test]
fn tui_viewport_clear_on_shrink_repaints_the_shrunk_viewport() {
    use tui::viewport::{FramePlan, ViewportRenderer};
    let plan = |n: usize| FramePlan {
        history: None,
        viewport: (0..n).map(|i| format!("row {i}")).collect(),
    };
    let shrink = |clear: bool| {
        let mut r = ViewportRenderer::new();
        r.clear_on_shrink = clear;
        r.render(&plan(5), 40, 20);
        let before = r.rows_written;
        let frame = r.render(&plan(2), 40, 20);
        (r.rows_written - before, frame.bytes)
    };
    // Diffed: the unchanged rows stay, the rest is cleared below.
    let (written, _) = shrink(false);
    assert_eq!(written, 0);
    // Clear-on-shrink: the viewport is painted whole from its top.
    let (written, bytes) = shrink(true);
    assert_eq!(written, 2);
    assert!(bytes.contains("\x1b[4A\r\x1b[J"), "{bytes:?}");
}

/// Drop CSI sequences.
fn strip(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}
