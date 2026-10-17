//! The TUI's debug environment variables and the Termux height rule (packages/tui terminal.ts:149-163 and 469-478,
//! tui-main-screen.ts:321-358 and 569-596; tui-render.test.ts "skips full re-render on height changes in Termux").
//! Expected strings are the TypeScript's formats; JS reference values were printed with node (`toISOString`,
//! `JSON.stringify(lines, null, 2)`, `toString(36)`).

use std::path::Path;

use tui::render_debug::{LocalStamp, RenderDebug, iso_time, render_dump_name, write_log_path};
use tui::viewport::{FramePlan, ViewportRenderer};

const R: &str = "\x1b[0m\x1b]8;;\x07";

fn plan(rows: &[&str]) -> FramePlan {
    FramePlan {
        history: None,
        viewport: rows.iter().map(|s| s.to_string()).collect(),
    }
}

fn traced() -> ViewportRenderer {
    let mut r = ViewportRenderer::new();
    r.trace = true;
    r
}

#[test]
fn tui_render_debug_write_log_path_names_a_file_in_a_directory() {
    let at = LocalStamp {
        year: 2026,
        month: 3,
        day: 7,
        hour: 9,
        minute: 5,
        second: 2,
    };
    assert_eq!(
        write_log_path("/logs", true, at, 4242),
        Some(Path::new("/logs/tui-2026-03-07_09-05-02-4242.log").to_path_buf())
    );
    // Not an existing directory: the value is the file.
    assert_eq!(
        write_log_path("/logs/out.log", false, at, 1),
        Some(Path::new("/logs/out.log").to_path_buf())
    );
    assert_eq!(write_log_path("", true, at, 1), None);
}

#[test]
fn tui_render_debug_iso_time_matches_date_to_iso_string() {
    assert_eq!(iso_time(0), "1970-01-01T00:00:00.000Z");
    assert_eq!(iso_time(1_729_000_000_123), "2024-10-15T13:46:40.123Z");
    assert_eq!(iso_time(1_792_224_000_999), "2026-10-17T08:00:00.999Z");
    assert_eq!(iso_time(-1), "1969-12-31T23:59:59.999Z");
    assert_eq!(render_dump_name(17, 123_456_789), "render-17-21i3v9.log");
}

#[test]
fn tui_render_debug_redraw_reasons_follow_the_main_screen_wording() {
    let mut r = traced();
    r.clear_on_shrink = true;
    r.render(&plan(&["a", "b", "c"]), 20, 5);
    r.render(&plan(&["a", "b", "x"]), 20, 5);
    r.render(&plan(&["a", "b", "x"]), 18, 5);
    r.render(&plan(&["a", "b", "x"]), 18, 7);
    r.render(&plan(&["a"]), 18, 7);
    r.invalidate();
    r.render(&plan(&["a"]), 18, 7);
    let (redraws, _) = r.take_traces();
    assert_eq!(
        redraws,
        [
            "fullRender: first render (prev=0, new=3, height=5)",
            "fullRender: terminal width changed (20 -> 18) (prev=3, new=3, height=5)",
            "fullRender: terminal height changed (5 -> 7) (prev=3, new=3, height=7)",
            "fullRender: clearOnShrink (maxLinesRendered=3) (prev=3, new=1, height=7)",
            "fullRender: terminal width changed (-1 -> 18) (prev=1, new=1, height=7)",
        ]
    );
    // Off unless traced.
    let mut quiet = ViewportRenderer::new();
    quiet.render(&plan(&["a"]), 20, 5);
    quiet.render(&plan(&["b"]), 10, 5);
    assert_eq!(quiet.take_traces(), (Vec::new(), Vec::new()));
}

/// tui-render.test.ts "skips full re-render on height changes in Termux" (and its non-Termux twin): heights 15, 8,
/// 14, 11 after a first paint at 10.
#[test]
fn tui_render_debug_termux_height_change_is_diffed_not_repainted() {
    let rows: Vec<String> = (0..20).map(|i| format!("Line {i}")).collect();
    let rows: Vec<&str> = rows.iter().map(String::as_str).collect();
    for termux in [true, false] {
        let mut r = traced();
        r.termux = termux;
        r.render(&plan(&rows), 40, 10);
        let frames: Vec<String> = [15, 8, 14, 11]
            .into_iter()
            .map(|height| r.render(&plan(&rows), 40, height).bytes)
            .collect();
        // A full paint goes back to the viewport's top and clears everything below it before writing.
        let repainted = |frame: &str| {
            let rest = frame.strip_prefix("\x1b[?2026h").unwrap_or(frame);
            let rest = match rest.strip_prefix("\x1b[") {
                Some(tail)
                    if tail
                        .find('A')
                        .is_some_and(|a| tail[..a].bytes().all(|b| b.is_ascii_digit())) =>
                {
                    &tail[tail.find('A').unwrap() + 1..]
                }
                _ => rest,
            };
            rest.starts_with("\r\x1b[J")
        };
        let (redraws, _) = r.take_traces();
        if termux {
            assert_eq!(redraws, ["fullRender: first render (prev=0, new=10, height=10)"]);
            assert!(!frames.iter().any(|f| repainted(f)), "no full repaint: {frames:?}");
            assert!(!frames.iter().any(|f| f.contains("\x1b[2J") || f.contains("\x1b[3J")));
            // The rows shown at the new height are rewritten where they changed.
            assert!(frames[0].contains(&format!("Line 5{R}")) && frames[0].contains(&format!("Line 19{R}")));
        } else {
            assert_eq!(redraws.len(), 5, "{redraws:?}");
            assert_eq!(
                redraws[1],
                "fullRender: terminal height changed (10 -> 15) (prev=10, new=15, height=15)"
            );
            assert!(frames.iter().all(|f| repainted(f)), "{frames:?}");
        }
    }
}

#[test]
fn tui_render_debug_dump_lists_a_differential_render() {
    let mut r = traced();
    r.render(&plan(&["a", "b"]), 10, 5);
    let frame = r.render(&plan(&["a", "c"]), 10, 5);
    // A frame without changes dumps nothing (TS returns before the dump).
    r.render(&plan(&["a", "c"]), 10, 5);
    let (_, dumps) = r.take_traces();
    assert_eq!(dumps.len(), 1);
    let written = frame.bytes.encode_utf16().count();
    let expected = [
        "firstChanged: 1".to_string(),
        "viewportTop: 0".into(),
        "cursorRow: 1".into(),
        "height: 5".into(),
        "lineDiff: 0".into(),
        "hardwareCursorRow: 1".into(),
        "renderEnd: 1".into(),
        "finalCursorRow: 1".into(),
        "cursorPos: null".into(),
        "newLines.length: 2".into(),
        "previousLines.length: 2".into(),
        String::new(),
        "=== newLines ===".into(),
        "[\n  \"a\\u001b[0m\\u001b]8;;\\u0007\",\n  \"c\\u001b[0m\\u001b]8;;\\u0007\"\n]".into(),
        String::new(),
        "=== previousLines ===".into(),
        "[\n  \"a\\u001b[0m\\u001b]8;;\\u0007\",\n  \"b\\u001b[0m\\u001b]8;;\\u0007\"\n]".into(),
        String::new(),
        "=== buffer ===".into(),
        format!("[{written} chars written in bounded chunks]"),
    ]
    .join("\n");
    assert_eq!(dumps[0].text(), expected);
    // A change above the cursor moves up to it.
    let mut r = traced();
    r.render(&plan(&["a", "b", "c"]), 10, 5);
    r.render(&plan(&["x", "b", "c"]), 10, 5);
    let (_, dumps) = r.take_traces();
    assert_eq!(
        (
            dumps[0].first_changed,
            dumps[0].line_diff,
            dumps[0].render_end,
            dumps[0].final_cursor_row
        ),
        (0, -2, 0, 0)
    );
}

#[test]
fn tui_render_debug_files_are_written_by_the_background_writer() {
    let temp = tempfile_dir();
    let (logs, dumps) = (temp.join("agent"), temp.join("dumps"));
    let write_log = temp.join("write.log");
    let env = |name: &str| match name {
        "PI_TUI_DEBUG_REDRAW" | "PI_TUI_DEBUG" => Some("1".to_string()),
        _ => None,
    };
    let debug = RenderDebug::from_env(
        &env,
        Some(write_log.clone()),
        &logs,
        &dumps,
        Box::new(|| 1_729_000_000_123),
    )
    .expect("on");
    assert!(debug.traces_renders());
    debug.wrote("\x1b[?2026h");
    debug.wrote("é\r\n");
    debug.redraw("fullRender: first render (prev=0, new=1, height=5)");
    debug.redraw("fullRender: clearOnShrink (maxLinesRendered=3) (prev=3, new=1, height=5)");
    debug.dump("first");
    debug.dump("second");
    debug.finish();
    assert_eq!(std::fs::read_to_string(&write_log).unwrap(), "\x1b[?2026hé\r\n");
    assert_eq!(
        std::fs::read_to_string(logs.join("pi-tui-debug.log")).unwrap(),
        "[2024-10-15T13:46:40.123Z] fullRender: first render (prev=0, new=1, height=5)\n\
         [2024-10-15T13:46:40.123Z] fullRender: clearOnShrink (maxLinesRendered=3) (prev=3, new=1, height=5)\n"
    );
    let mut names: Vec<String> = std::fs::read_dir(&dumps)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names.len(), 2, "one file per render: {names:?}");
    for name in &names {
        assert!(
            name.starts_with("render-1729000000123-") && name.ends_with(".log"),
            "{name}"
        );
    }
    let mut texts: Vec<String> = names
        .iter()
        .map(|n| std::fs::read_to_string(dumps.join(n)).unwrap())
        .collect();
    texts.sort();
    assert_eq!(texts, ["first", "second"]);
    // Nothing is on unless asked for.
    let none = |_: &str| None;
    assert!(RenderDebug::from_env(&none, None, &logs, &dumps, Box::new(|| 0)).is_none());
    let write_only = RenderDebug::from_env(&none, Some(write_log), &logs, &dumps, Box::new(|| 0)).unwrap();
    assert!(!write_only.traces_renders());
    let _ = std::fs::remove_dir_all(&temp);
}

/// A private temp directory (the tui crate has no tempfile dev-dependency).
fn tempfile_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tui-render-debug-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// terminal.ts `enableWindowsVTInput`: `ENABLE_VIRTUAL_TERMINAL_INPUT` (0x0200) is added to the console input mode,
/// keeping its other flags (the call itself is `cfg(windows)`).
#[test]
fn tui_terminal_windows_vt_input_mode_adds_the_flag() {
    assert_eq!(tui::terminal::ENABLE_VIRTUAL_TERMINAL_INPUT, 0x0200);
    assert_eq!(tui::terminal::vt_input_mode(0x0007), 0x0207);
    assert_eq!(tui::terminal::vt_input_mode(0x0207), 0x0207);
    assert_eq!(tui::terminal::vt_input_mode(0), 0x0200);
}
