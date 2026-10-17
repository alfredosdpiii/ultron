//! Terminal input reaching the fullscreen app the way the TypeScript native client gets it: the Kitty keyboard
//! protocol negotiated by the run loop (packages/tui/src/terminal.ts) with Kitty key releases kept from the
//! components (tui.ts `handleInput`), Windows Terminal's raw BS as Ctrl+Backspace (keys.ts), the mouse routed to
//! components (tui-alt-screen.ts `handleMouseEvent`: editor.ts, select-list.ts, settings-list.ts, input.ts,
//! tool-execution.ts and assistant-message.ts `MouseRegion`s), and the hardware cursor and clear-on-shrink env
//! fallbacks (settings-manager.ts). Literal expectations from the TypeScript sources.

use std::rc::Rc;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use interactive::app::{App, AppOptions};
use interactive::history::PromptHistoryStore;
use interactive::lane::{LaneClient, LaneEvent, ModelInfo, SessionOpened};
use interactive::qol::QolSettings;
use interactive::run::{UiEvent, paint_loop_with};
use serde_json::{Value, json};
use tui::keybindings::{KeybindingsManager, Platform};
use tui::keyboard_protocol::KeyboardProtocol;
use tui::keys::KeyContext;
use tui::qol_keys::native_app_keybindings;
use tui::theme::{ColorMode, builtin_theme_json, create_theme};
use tui::utils::strip_terminal_sequences;

const W: usize = 80;
const H: usize = 40;

struct Recorder(Arc<Mutex<Vec<String>>>);

impl LaneClient for Recorder {
    fn prompt(&self, text: &str) {
        self.0.lock().unwrap().push(format!("prompt {text}"));
    }
    fn steer(&self, _: &str) -> String {
        String::new()
    }
    fn follow_up(&self, text: &str) -> String {
        self.0.lock().unwrap().push(format!("follow-up {text}"));
        String::new()
    }
    fn cancel_queued(&self, _: &str) {}
    fn abort(&self) {}
    fn new_session(&self) {}
    fn resume(&self, _: &str) {}
    fn list_sessions(&self) {}
    fn set_name(&self, _: &str) {}
    fn select_model(&self, _: &str, _: &str) {}
    fn set_thinking(&self, level: &str) {
        self.0.lock().unwrap().push(format!("thinking {level}"));
    }
    fn save_setting(&self, key: &str, value: Value) {
        self.0.lock().unwrap().push(format!("{key}={value}"));
    }
}

type User = Vec<(String, Option<Vec<String>>)>;

fn keys(ctx: KeyContext, user: &User) -> KeybindingsManager {
    KeybindingsManager::new(native_app_keybindings(Platform::new("linux", false)), user.clone(), ctx)
}

fn app_with(ctx: KeyContext, messages: Vec<Value>) -> (App, Arc<Mutex<Vec<String>>>) {
    app_with_keys(ctx, Vec::new(), messages)
}

fn app_with_keys(ctx: KeyContext, user: User, messages: Vec<Value>) -> (App, Arc<Mutex<Vec<String>>>) {
    let theme = Rc::new(create_theme(&builtin_theme_json("dark").unwrap(), ColorMode::Truecolor, None).unwrap());
    let model = ModelInfo {
        provider: "anthropic".into(),
        id: "a".into(),
        name: "Model a".into(),
        reasoning: true,
        context_window: 1000,
        thinking_levels: Vec::new(),
    };
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut app = App::new(
        AppOptions {
            theme,
            keys: keys(ctx, &user),
            editor_keys: keys(ctx, &user),
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
            qol: QolSettings::parity(),
        },
        Box::new(Recorder(Arc::clone(&calls))),
    );
    app.apply(LaneEvent::Opened(SessionOpened {
        session_id: "sess".into(),
        session_file: Some("/s/cur".into()),
        name: None,
        messages,
    }));
    (app, calls)
}

fn app() -> (App, Arc<Mutex<Vec<String>>>) {
    app_with(KeyContext::default(), Vec::new())
}

fn screen(app: &mut App) -> Vec<String> {
    app.render(W, H)
        .iter()
        .map(|l| strip_terminal_sequences(l).trim_end().to_string())
        .collect()
}

/// The screen cell (column, row) where `text` starts, on the last row showing it.
fn cell_of(app: &mut App, text: &str) -> (usize, usize) {
    let s = screen(app);
    let row = s
        .iter()
        .rposition(|l| l.contains(text))
        .unwrap_or_else(|| panic!("{text}: {s:#?}"));
    let byte = s[row].find(text).unwrap();
    (tui::utils::visible_width(&s[row][..byte]), row)
}

/// A left press and release on one cell (SGR, 1-based).
fn click(app: &mut App, (x, y): (usize, usize)) {
    app.handle_input(&format!("\x1b[<0;{};{}M", x + 1, y + 1));
    app.handle_input(&format!("\x1b[<0;{};{}m", x + 1, y + 1));
}

fn typed(app: &mut App, text: &str) {
    for c in text.chars() {
        app.handle_input(&c.to_string());
    }
}

/// The run loop over `inputs` (then stdin closes): what it wrote, with `keyboard` queried at terminal start.
fn run(app: &mut App, inputs: &[&str], keyboard: &mut KeyboardProtocol) -> String {
    let (tx, rx) = mpsc::channel();
    for input in inputs {
        tx.send(UiEvent::Input((*input).to_string())).unwrap();
    }
    tx.send(UiEvent::InputClosed).unwrap();
    let mut out = String::new();
    paint_loop_with(
        app,
        &rx,
        &mut |data| {
            out.push_str(data);
            Ok(())
        },
        &|| (W as u16, H as u16),
        None,
        keyboard,
    )
    .unwrap();
    out
}

// ---- keyboard protocol ----

/// A Kitty flags reply turns Kitty matching on for the app, the editor and its bindings (ESC CR is Shift+Enter,
/// a new line), and never reaches the editor; key releases are dropped (`isKeyRelease`).
#[test]
fn interactive_kitty_reply_switches_the_app_to_kitty_input() {
    let (mut app, _) = app();
    let mut keyboard = KeyboardProtocol::new();
    keyboard.query();
    let out = run(
        &mut app,
        &["\x1b[?7u", "h", "\x1b[104;1:3u", "\x1b\r", "i"],
        &mut keyboard,
    );
    assert!(app.kitty_protocol_active());
    assert_eq!(app.editor_text(), "h\ni");
    assert!(!out.contains("\x1b[>4;2m"), "{out:?}");
    assert!(keyboard.kitty_active());
}

/// Device attributes before any flags reply: the modifyOtherKeys fallback is written, Kitty matching stays off.
#[test]
fn interactive_device_attributes_reply_enables_modify_other_keys() {
    let (mut app, _) = app();
    let mut keyboard = KeyboardProtocol::new();
    keyboard.query();
    let out = run(&mut app, &["\x1b[?62;4;52c", "o", "k"], &mut keyboard);
    assert!(!app.kitty_protocol_active());
    assert_eq!(app.editor_text(), "ok");
    assert_eq!(out.matches("\x1b[>4;2m").count(), 1, "{out:?}");
    assert!(keyboard.modify_other_keys_active());
    // Stopping the terminal resets it.
    assert_eq!(keyboard.stop(), "\x1b[<u\x1b[>4;0m");
}

/// A reply fragment the stdin buffer flushed alone is held, then completed by the next read.
#[test]
fn interactive_split_kitty_reply_is_held_and_completed() {
    let (mut app, _) = app();
    let mut keyboard = KeyboardProtocol::new();
    keyboard.query();
    run(&mut app, &["\x1b[?7", "u", "x"], &mut keyboard);
    assert!(app.kitty_protocol_active());
    assert_eq!(app.editor_text(), "x");
}

// ---- Windows Terminal ----

/// keys.ts `matchesRawBackspace`: in Windows Terminal (WT_SESSION, not over SSH) a raw BS is Ctrl+Backspace;
/// elsewhere it is Backspace. With Ctrl+Backspace bound to deleteWordBackward, the BS deletes the word only there.
#[test]
fn interactive_windows_terminal_raw_bs_is_ctrl_backspace() {
    let user: User = vec![(
        "tui.editor.deleteWordBackward".into(),
        Some(vec!["ctrl+backspace".into()]),
    )];
    let wt = KeyContext {
        kitty_active: false,
        windows_terminal: true,
    };
    let (mut wt_app, _) = app_with_keys(wt, user.clone(), Vec::new());
    typed(&mut wt_app, "foo bar");
    wt_app.handle_input("\x08");
    assert_eq!(wt_app.editor_text(), "foo ");
    let (mut app, _) = app_with_keys(KeyContext::default(), user, Vec::new());
    typed(&mut app, "foo bar");
    app.handle_input("\x08");
    assert_eq!(app.editor_text(), "foo ba");
}

// ---- mouse ----

/// editor.ts `handleMouse`: a left click (press and release on one cell) on a text row moves the cursor there.
#[test]
fn interactive_mouse_click_in_the_editor_moves_the_cursor() {
    let (mut app, _) = app();
    typed(&mut app, "hello world");
    let (x, y) = cell_of(&mut app, "world");
    click(&mut app, (x, y));
    typed(&mut app, "X");
    assert_eq!(app.editor_text(), "hello Xworld");
    // A drag over the editor is text selection, not a cursor move.
    let (x, y) = cell_of(&mut app, "hello");
    app.handle_input(&format!("\x1b[<0;{};{}M", x + 1, y + 1));
    app.handle_input(&format!("\x1b[<32;{};{}M", x + 4, y + 1));
    app.handle_input(&format!("\x1b[<0;{};{}m", x + 4, y + 1));
    typed(&mut app, "Y");
    assert_eq!(app.editor_text(), "hello XYworld");
}

/// editor.ts `handleMouse` on the autocomplete rows: a click picks the clicked row (select-list.ts).
#[test]
fn interactive_mouse_click_on_an_autocomplete_row_picks_it() {
    let (mut app, _) = app();
    typed(&mut app, "/se");
    app.fire_autocomplete();
    let s = screen(&mut app).join("\n");
    assert!(s.contains("settings") && s.contains("session"), "{s}");
    let (x, y) = cell_of(&mut app, "session        Show");
    click(&mut app, (x, y));
    assert_eq!(app.editor_text(), "/session ", "{s}");
}

/// select-list.ts `handleMouse`: a press selects the row, the click confirms it.
#[test]
fn interactive_mouse_click_on_a_selection_row_confirms_it() {
    let (mut app, calls) = app();
    typed(&mut app, "/thinking");
    app.handle_input("\r");
    let s = screen(&mut app).join("\n");
    assert!(s.contains("Select thinking level:"), "{s}");
    let (x, y) = cell_of(&mut app, "high");
    click(&mut app, (x, y));
    let s = screen(&mut app).join("\n");
    assert!(s.contains("Thinking level: high."), "{s}");
    assert!(calls.lock().unwrap().contains(&"thinking high".to_string()));
}

fn open_settings(app: &mut App) {
    typed(app, "/settings");
    app.handle_input("\r");
    app.apply(LaneEvent::Settings {
        settings: json!({}),
        env_overrides: Default::default(),
        themes: vec!["dark".into()],
        terminal_theme: "dark".into(),
    });
}

/// settings-list.ts `handleMouse`: a click on a row selects it and activates it (cycles its value, saved).
#[test]
fn interactive_mouse_click_on_a_settings_row_changes_it() {
    let (mut app, calls) = app();
    open_settings(&mut app);
    typed(&mut app, "tui mode");
    let (x, y) = cell_of(&mut app, "TUI mode");
    click(&mut app, (x, y));
    assert_eq!(*calls.lock().unwrap(), ["tuiMode=\"regular\""]);
    let s = screen(&mut app).join("\n");
    assert!(
        s.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .contains("Saved tuiMode: regular."),
        "{s}"
    );
}

/// input.ts `handleMouse` through the settings search row: a press puts the cursor under the pointer.
#[test]
fn interactive_mouse_press_in_the_settings_search_moves_its_cursor() {
    let (mut app, _) = app();
    open_settings(&mut app);
    typed(&mut app, "tui");
    let (x, y) = cell_of(&mut app, "> tui");
    // Column 3 is after "> t".
    click(&mut app, (x + 3, y));
    typed(&mut app, "X");
    let s = screen(&mut app).join("\n");
    assert!(s.contains("> tXui"), "{s}");
}

fn rlm_turn(output: &str) -> Vec<Value> {
    vec![
        json!({"role": "user", "content": "run it"}),
        json!({"role": "assistant", "stopReason": "toolUse", "content": [
            {"type": "toolCall", "id": "c1", "name": "rlm", "arguments": {"code": "print(1)"}}
        ]}),
        json!({"role": "toolResult", "toolCallId": "c1", "toolName": "rlm", "isError": false,
            "content": [{"type": "text", "text": output}]}),
    ]
}

/// tool-execution.ts `createResultRegion`: a left click on a tool's result toggles that tool's expansion; the
/// transcript's Ctrl+O sets every tool again.
#[test]
fn interactive_mouse_click_on_a_tool_result_toggles_its_expansion() {
    let output = (1..=15).map(|i| format!("out-{i:02}")).collect::<Vec<_>>().join("\n");
    let (mut app, _) = app_with(KeyContext::default(), rlm_turn(&output));
    let s = screen(&mut app).join("\n");
    assert!(s.contains("out-15") && !s.contains("out-01"), "{s}");
    let at = cell_of(&mut app, "out-15");
    click(&mut app, at);
    let s = screen(&mut app).join("\n");
    assert!(s.contains("out-01") && s.contains("out-15"), "{s}");
    // A click again collapses it.
    let at = cell_of(&mut app, "out-15");
    click(&mut app, at);
    assert!(!screen(&mut app).join("\n").contains("out-01"));
    // A click on the user message toggles nothing.
    let at = cell_of(&mut app, "run it");
    click(&mut app, at);
    assert!(!screen(&mut app).join("\n").contains("out-01"));
}

/// assistant-message.ts: a left click on a thinking run shows or hides that run (`thinkingVisibilityOverrides`);
/// Ctrl+T's `setHideThinkingBlock` clears the overrides.
#[test]
fn interactive_mouse_click_on_a_thinking_block_toggles_it() {
    let messages = vec![
        json!({"role": "user", "content": "why"}),
        json!({"role": "assistant", "stopReason": "stop", "content": [
            {"type": "thinking", "thinking": "deep thoughts"},
            {"type": "text", "text": "because"}
        ]}),
    ];
    let (mut app, _) = app_with(KeyContext::default(), messages);
    app.set_hide_thinking(true);
    let s = screen(&mut app).join("\n");
    assert!(s.contains("Thinking...") && !s.contains("deep thoughts"), "{s}");
    let at = cell_of(&mut app, "Thinking...");
    click(&mut app, at);
    let s = screen(&mut app).join("\n");
    assert!(s.contains("deep thoughts") && !s.contains("Thinking..."), "{s}");
    let at = cell_of(&mut app, "deep thoughts");
    click(&mut app, at);
    assert!(screen(&mut app).join("\n").contains("Thinking..."));
    // Clicking the answer text toggles nothing.
    let at = cell_of(&mut app, "because");
    click(&mut app, at);
    assert!(screen(&mut app).join("\n").contains("Thinking..."));
}

/// A press on a component that moves before the release is no click (`mousePressMoved`): the autocomplete row
/// pressed stays selected but is not picked.
#[test]
fn interactive_mouse_press_moved_off_its_cell_is_no_click() {
    let (mut app, _) = app();
    typed(&mut app, "/se");
    app.fire_autocomplete();
    let (x, y) = cell_of(&mut app, "session        Show");
    app.handle_input(&format!("\x1b[<0;{};{}M", x + 1, y + 1));
    app.handle_input(&format!("\x1b[<32;{};{}M", x + 3, y + 1));
    app.handle_input(&format!("\x1b[<0;{};{}m", x + 3, y + 1));
    assert_eq!(app.editor_text(), "/se");
}

// ---- settings env fallbacks ----

/// settings-manager.ts `getShowHardwareCursor` (`??` falls back to `PI_HARDWARE_CURSOR=1`) and `getClearOnShrink`
/// (a present setting, else `PI_CLEAR_ON_SHRINK=1`).
#[test]
fn interactive_hardware_cursor_and_clear_on_shrink_fall_back_to_env() {
    let env = |k: &str| match k {
        "PI_HARDWARE_CURSOR" | "PI_CLEAR_ON_SHRINK" => Some("1".to_string()),
        _ => None,
    };
    let (mut app, _) = app();
    app.apply_startup_settings(&json!({}), &env);
    assert!(app.show_hardware_cursor && app.clear_on_shrink);
    app.apply_startup_settings(&json!({}), &|_| None);
    assert!(!app.show_hardware_cursor && !app.clear_on_shrink);
    app.apply_startup_settings(
        &json!({"showHardwareCursor": false, "terminal": {"clearOnShrink": false}}),
        &env,
    );
    assert!(!app.show_hardware_cursor && !app.clear_on_shrink);
    // `??` treats null as unset; `!== undefined` does not.
    app.apply_startup_settings(
        &json!({"showHardwareCursor": null, "terminal": {"clearOnShrink": null}}),
        &env,
    );
    assert!(app.show_hardware_cursor && !app.clear_on_shrink);
    app.apply_startup_settings(
        &json!({"showHardwareCursor": true, "terminal": {"clearOnShrink": true}}),
        &|_| None,
    );
    assert!(app.show_hardware_cursor && app.clear_on_shrink);
    // Other env values are not "1".
    app.apply_startup_settings(&json!({}), &|_| Some("true".to_string()));
    assert!(!app.show_hardware_cursor && !app.clear_on_shrink);
}
