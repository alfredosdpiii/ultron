//! The TUI debug environment variables through the run loop (`terminal_debug` as the product sets it up, then
//! `paint_loop` on the split viewport): `PI_TUI_WRITE_LOG` naming a directory gets every byte the loop wrote in
//! `tui-<local time>-<pid>.log`, `PI_TUI_DEBUG_REDRAW=1` logs each full redraw to `<agent dir>/pi-tui-debug.log`,
//! `PI_TUI_DEBUG=1` dumps each differential render; `TERMUX_VERSION` turns on the Termux height rule. Everything is
//! written into the test's private directory.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use interactive::app::{App, AppOptions};
use interactive::history::PromptHistoryStore;
use interactive::lane::{LaneClient, LaneEvent, ModelInfo, SessionOpened};
use interactive::qol::QolSettings;
use interactive::run::{UiEvent, paint_loop, terminal_debug};
use serde_json::{Value, json};
use tui::keybindings::{KeybindingsManager, Platform};
use tui::qol_keys::native_app_keybindings;
use tui::render_debug::LocalStamp;
use tui::theme::{ColorMode, builtin_theme_json, create_theme};

struct Quiet;

impl LaneClient for Quiet {
    fn prompt(&self, _: &str) {}
    fn steer(&self, _: &str) -> String {
        String::new()
    }
    fn follow_up(&self, _: &str) -> String {
        String::new()
    }
    fn cancel_queued(&self, _: &str) {}
    fn abort(&self) {}
    fn new_session(&self) {}
    fn resume(&self, _: &str) {}
    fn list_sessions(&self) {}
    fn set_name(&self, _: &str) {}
    fn select_model(&self, _: &str, _: &str) {}
    fn save_setting(&self, _: &str, _: Value) {}
}

fn keys() -> KeybindingsManager {
    KeybindingsManager::new(
        native_app_keybindings(Platform::new("linux", false)),
        Vec::new(),
        Default::default(),
    )
}

fn split_app() -> App {
    let theme = Rc::new(create_theme(&builtin_theme_json("dark").unwrap(), ColorMode::Truecolor, None).unwrap());
    let model = ModelInfo {
        provider: "anthropic".into(),
        id: "a".into(),
        name: "Model a".into(),
        reasoning: false,
        context_window: 1000,
        thinking_levels: Vec::new(),
    };
    let mut qol = QolSettings::parity();
    qol.viewport_split = true;
    let mut app = App::new(
        AppOptions {
            theme,
            keys: keys(),
            editor_keys: keys(),
            version: "1.2.3".into(),
            cwd: "/p".into(),
            home: None,
            server_id: "server".into(),
            models: vec![model.clone()],
            model,
            thinking_level: "off".into(),
            splash: false,
            history: PromptHistoryStore::new(None, "/p"),
            fd_path: None,
            highlight: Rc::new(|code: &str| code.split('\n').map(str::to_string).collect()),
            qol,
        },
        Box::new(Quiet),
    );
    app.apply(LaneEvent::Opened(SessionOpened {
        session_id: "sess".into(),
        session_file: Some("/s/cur".into()),
        name: None,
        messages: vec![json!({"role": "user", "content": "question"})],
    }));
    app
}

const STAMP: LocalStamp = LocalStamp {
    year: 2026,
    month: 10,
    day: 17,
    hour: 9,
    minute: 5,
    second: 3,
};

/// Run the loop for three painted frames: the first at 60x16, then a width change, then a typed key at the same
/// size; every write is recorded.
fn run_three_frames(app: &mut App, sizes: [(u16, u16); 3]) -> String {
    let (tx, rx) = mpsc::channel();
    let mut writes = String::new();
    let frames = Cell::new(0);
    let calls = Cell::new(0);
    paint_loop(
        app,
        &rx,
        &mut |d| {
            writes.push_str(d);
            if d.ends_with("\x1b[?2026l") {
                frames.set(frames.get() + 1);
                let next = match frames.get() {
                    1 => UiEvent::Input("x".into()),
                    2 => UiEvent::Input("y".into()),
                    _ => UiEvent::InputClosed,
                };
                let _ = tx.send(next);
            }
            Ok(())
        },
        &|| {
            calls.set(calls.get() + 1);
            sizes[(calls.get() - 1).min(2)]
        },
        None,
    )
    .unwrap();
    writes
}

#[test]
fn interactive_terminal_debug_env_logs_writes_redraws_and_renders() {
    let temp = tempfile_dir();
    let (write_dir, agent, dumps) = (temp.join("writes"), temp.join("agent"), temp.join("dumps"));
    std::fs::create_dir_all(&write_dir).unwrap();
    let vars: HashMap<&str, String> = HashMap::from([
        ("PI_TUI_WRITE_LOG", write_dir.display().to_string()),
        ("PI_TUI_DEBUG_REDRAW", "1".to_string()),
        ("PI_TUI_DEBUG", "1".to_string()),
    ]);
    let var = |name: &str| vars.get(name).cloned();
    let (debug, termux) = terminal_debug(&var, &agent, &dumps, STAMP, 4242, Box::new(|| 1_792_224_000_999));
    assert!(!termux);
    let debug = debug.expect("debug outputs on");
    let mut app = split_app();
    app.set_terminal_debug(Some(Arc::clone(&debug)), termux);
    let writes = run_three_frames(&mut app, [(60, 16), (50, 16), (50, 16)]);
    drop(app);
    debug.finish();

    let log = write_dir.join("tui-2026-10-17_09-05-03-4242.log");
    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        writes,
        "every byte the loop wrote"
    );
    let redraws = std::fs::read_to_string(agent.join("pi-tui-debug.log")).unwrap();
    let lines: Vec<&str> = redraws.lines().collect();
    assert_eq!(lines.len(), 2, "{redraws}");
    assert!(
        lines[0].starts_with("[2026-10-17T08:00:00.999Z] fullRender: first render (prev=0, new="),
        "{redraws}"
    );
    assert!(
        lines[1].starts_with("[2026-10-17T08:00:00.999Z] fullRender: terminal width changed (60 -> 50) (prev="),
        "{redraws}"
    );
    let dumped: Vec<_> = std::fs::read_dir(&dumps).unwrap().map(|e| e.unwrap().path()).collect();
    assert_eq!(dumped.len(), 1, "the typed key's differential render: {dumped:?}");
    let name = dumped[0].file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        name.starts_with("render-1792224000999-") && name.ends_with(".log"),
        "{name}"
    );
    let dump = std::fs::read_to_string(&dumped[0]).unwrap();
    assert!(dump.starts_with("firstChanged: "), "{dump}");
    assert!(
        dump.contains("\nheight: 16\n") && dump.contains("\n=== newLines ===\n"),
        "{dump}"
    );
    let _ = std::fs::remove_dir_all(&temp);
}

#[test]
fn interactive_terminal_debug_is_off_without_the_variables_and_reads_termux() {
    let none = |_: &str| None;
    let temp = tempfile_dir();
    let (debug, termux) = terminal_debug(&none, &temp, &temp, STAMP, 1, Box::new(|| 0));
    assert!(debug.is_none() && !termux);
    let termux_var = |name: &str| (name == "TERMUX_VERSION").then(|| "0.118.0".to_string());
    assert!(terminal_debug(&termux_var, &temp, &temp, STAMP, 1, Box::new(|| 0)).1);
    let empty = |name: &str| (name == "TERMUX_VERSION").then(String::new);
    assert!(!terminal_debug(&empty, &temp, &temp, STAMP, 1, Box::new(|| 0)).1);
    // A write log that is not an existing directory is the file itself.
    let file = temp.join("tui.log");
    let named = |name: &str| (name == "PI_TUI_WRITE_LOG").then(|| file.display().to_string());
    let (debug, _) = terminal_debug(&named, &temp, &temp, STAMP, 1, Box::new(|| 0));
    let debug = debug.unwrap();
    assert_eq!(debug.write_log.as_deref(), Some(file.as_path()));
    assert!(!debug.traces_renders());
    // Termux through the loop: a height change alone is diffed, never painted whole.
    let redraw = |name: &str| (name == "PI_TUI_DEBUG_REDRAW" || name == "TERMUX_VERSION").then(|| "1".to_string());
    let agent = temp.join("agent");
    let (debug, termux) = terminal_debug(&redraw, &agent, &temp, STAMP, 1, Box::new(|| 0));
    let debug = debug.unwrap();
    let mut app = split_app();
    app.set_terminal_debug(Some(Arc::clone(&debug)), termux);
    run_three_frames(&mut app, [(60, 16), (60, 10), (60, 10)]);
    debug.finish();
    let redraws = std::fs::read_to_string(agent.join("pi-tui-debug.log")).unwrap();
    assert_eq!(redraws.lines().count(), 1, "only the first render: {redraws}");
    let _ = std::fs::remove_dir_all(&temp);
}

fn tempfile_dir() -> std::path::PathBuf {
    static N: Mutex<u32> = Mutex::new(0);
    let n = {
        let mut n = N.lock().unwrap();
        *n += 1;
        *n
    };
    let dir = std::env::temp_dir().join(format!("interactive-terminal-debug-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
