//! The client TUI's fullscreen transcript viewport (`TuiAltScreen` under experimental/client-tui.ts, the layout of
//! chat-viewport.ts and the options of tui-renderer.ts): scroll keys before the focused component, prompt jumps to
//! user messages (OSC 133 zones), wheel and mouse selection with copy-on-select through the app's clipboard service
//! and its "Copied!" flash, the jump-to-end indicator, mouse input kept away from overlays that do not read it, the
//! run loop's mouse tracking, and the corrected tuiMode status. The viewport itself is replayed against the
//! TypeScript in crates/tui/tests/fullscreen_scroll.rs; these are literal expectations of the integration.

use std::rc::Rc;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use interactive::app::{App, AppOptions};
use interactive::history::PromptHistoryStore;
use interactive::lane::{LaneClient, LaneEvent, ModelInfo, SessionOpened};
use interactive::qol::QolSettings;
use interactive::run::{UiEvent, paint_loop};
use serde_json::{Value, json};
use tui::clipboard::{Clipboard, ClipboardService};
use tui::keybindings::{KeybindingsManager, Platform};
use tui::qol_keys::native_app_keybindings;
use tui::theme::{ColorMode, builtin_theme_json, create_theme};
use tui::utils::strip_terminal_sequences;

struct Quiet(Arc<Mutex<Vec<String>>>);

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
    fn save_setting(&self, key: &str, value: Value) {
        self.0.lock().unwrap().push(format!("{key}={value}"));
    }
}

fn keys() -> KeybindingsManager {
    KeybindingsManager::new(
        native_app_keybindings(Platform::new("linux", false)),
        Vec::new(),
        Default::default(),
    )
}

fn messages(turns: usize) -> Vec<Value> {
    (1..=turns)
        .flat_map(|i| {
            [
                json!({"role": "user", "content": format!("question {i}")}),
                json!({"role": "assistant", "stopReason": "stop", "content": [{"type": "text", "text": format!("answer {i}")}]}),
            ]
        })
        .collect()
}

/// A fullscreen app (`ULTRON_TUI_PARITY` settings: no scrollback split) on a session of `turns` turns.
fn app(turns: usize) -> (App, Arc<Mutex<Vec<String>>>) {
    app_with(turns, QolSettings::parity())
}

fn app_with(turns: usize, qol: QolSettings) -> (App, Arc<Mutex<Vec<String>>>) {
    let theme = Rc::new(create_theme(&builtin_theme_json("dark").unwrap(), ColorMode::Truecolor, None).unwrap());
    let model = ModelInfo {
        provider: "anthropic".into(),
        id: "a".into(),
        name: "Model a".into(),
        reasoning: false,
        context_window: 1000,
        thinking_levels: Vec::new(),
    };
    let saved = Arc::new(Mutex::new(Vec::new()));
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
        Box::new(Quiet(Arc::clone(&saved))),
    );
    app.apply(LaneEvent::Opened(SessionOpened {
        session_id: "sess".into(),
        session_file: Some("/s/cur".into()),
        name: None,
        messages: messages(turns),
    }));
    (app, saved)
}

const W: usize = 60;
const H: usize = 16;

fn screen(app: &mut App) -> Vec<String> {
    app.render(W, H)
        .iter()
        .map(|l| strip_terminal_sequences(l).trim_end().to_string())
        .collect()
}

/// The message whose OSC 133 zone starts on the top row (user and assistant messages are zones): row 0 is its
/// padding, row 1 its text (the scrollbar column cut off).
fn top_message(app: &mut App) -> String {
    let s = screen(app);
    let clean = |l: &str| l.trim_end_matches(['│', '┃', ' ']).trim().to_string();
    assert_eq!(clean(&s[0]), "", "{s:#?}");
    clean(&s[1])
}

/// The screen row showing `text`.
fn row_of(app: &mut App, text: &str) -> usize {
    let s = screen(app);
    s.iter()
        .position(|l| l.contains(text))
        .unwrap_or_else(|| panic!("{text}: {s:#?}"))
}

#[test]
fn interactive_fullscreen_page_keys_scroll_the_transcript_before_the_editor() {
    let (mut app, _) = app(12);
    let following = screen(&mut app);
    assert!(following.iter().any(|l| l.contains("answer 12")), "{following:#?}");
    assert!(app.viewport().is_following_output());
    let top = app.viewport().viewport_top();
    let height = app.viewport().view.viewport_height();
    // PageUp: a page less four rows of overlap; the editor does not see the key.
    app.handle_input("\x1b[5~");
    screen(&mut app);
    assert_eq!(app.viewport().viewport_top(), top - (height - 4));
    assert!(!app.viewport().is_following_output());
    // Home and End scroll the viewport in fullscreen (they shadow the editor's line start/end).
    app.handle_input("a");
    app.handle_input("\x1b[H");
    assert_eq!(app.viewport().viewport_top(), 0);
    assert_eq!(app.editor_text(), "a");
    app.handle_input("\x1b[F");
    screen(&mut app);
    assert!(app.viewport().is_following_output());
    assert_eq!(app.viewport().viewport_top(), top);
}

#[test]
fn interactive_fullscreen_prompt_jumps_land_on_user_messages() {
    let (mut app, _) = app(12);
    screen(&mut app);
    // Ctrl+Up: the nearest message zone start above the viewport top becomes the top row.
    app.handle_input("\x1b[1;5A");
    assert_eq!(top_message(&mut app), "answer 10");
    app.handle_input("\x1b[1;5A");
    assert_eq!(top_message(&mut app), "question 10");
    // Ctrl+Shift+Up too; Ctrl+Down goes back.
    app.handle_input("\x1b[1;6A");
    assert_eq!(top_message(&mut app), "answer 9");
    app.handle_input("\x1b[1;5B");
    assert_eq!(top_message(&mut app), "question 10");
    app.handle_input("\x1b[1;6B");
    assert_eq!(top_message(&mut app), "answer 10");
}

#[test]
fn interactive_fullscreen_jump_indicator_shows_while_scrolled_up_and_returns_to_the_end() {
    let (mut app, _) = app(12);
    screen(&mut app);
    app.handle_input("\x1b[<64;5;3M");
    let s = screen(&mut app);
    let row = s
        .iter()
        .position(|l| l.contains("↓ Jump to latest message · End"))
        .unwrap_or_else(|| panic!("{s:#?}"));
    let col = s[row].find('↓').unwrap();
    // A click on the label follows the end again.
    app.handle_input(&format!("\x1b[<0;{};{}M", col + 1, row + 1));
    let s = screen(&mut app);
    assert!(app.viewport().is_following_output());
    assert!(!s.iter().any(|l| l.contains("Jump to latest message")), "{s:#?}");
}

struct Recording(Arc<Mutex<Vec<String>>>);

impl Clipboard for Recording {
    fn get_text(&mut self) -> Result<String, String> {
        Ok(String::new())
    }
    fn set_text(&mut self, text: &str) -> Result<(), String> {
        self.0.lock().unwrap().push(text.to_string());
        Ok(())
    }
    fn get_image_png(&mut self) -> Result<Vec<u8>, String> {
        Err("none".into())
    }
}

/// Poll the app's services until `done` (hang guard only).
fn poll_until(app: &mut App, done: impl Fn(&mut App) -> bool) {
    let guard = Instant::now() + Duration::from_secs(120);
    while !done(app) {
        assert!(Instant::now() < guard, "timed out");
        app.poll_services();
        std::thread::yield_now();
    }
}

#[test]
fn interactive_fullscreen_drag_selection_copies_through_the_clipboard_service_and_flashes() {
    let (mut app, _) = app(12);
    let copies = Arc::new(Mutex::new(Vec::new()));
    app.set_clipboard(ClipboardService::spawn(Recording(Arc::clone(&copies))));
    screen(&mut app);
    let row = row_of(&mut app, "answer 12") + 1;
    // Press at the start of "answer 12", drag over "answer", release: the selection is copied.
    let col = screen(&mut app)[row - 1].find("answer").unwrap() + 1;
    app.handle_input(&format!("\x1b[<0;{col};{row}M"));
    app.handle_input(&format!("\x1b[<32;{};{row}M", col + 5));
    app.handle_input(&format!("\x1b[<0;{};{row}m", col + 5));
    poll_until(&mut app, |a| screen(a)[0].contains("Copied!"));
    assert_eq!(*copies.lock().unwrap(), ["answer"]);
    // The flash sits at the top right and expires after a second.
    let top = screen(&mut app)[0].clone();
    assert!(top.ends_with(" Copied!"), "{top:?}");
    assert!(app.poll_viewport(Instant::now() + Duration::from_secs(2)));
    assert!(!screen(&mut app)[0].contains("Copied!"));
}

/// The clipboard failed: its error flashes (five seconds) instead of "Copied!".
#[test]
fn interactive_fullscreen_copy_failure_flashes_the_clipboard_error() {
    struct Failing;
    impl Clipboard for Failing {
        fn get_text(&mut self) -> Result<String, String> {
            Err("x".into())
        }
        fn set_text(&mut self, _: &str) -> Result<(), String> {
            Err("Clipboard unavailable".into())
        }
        fn get_image_png(&mut self) -> Result<Vec<u8>, String> {
            Err("x".into())
        }
    }
    let (mut app, _) = app(3);
    app.set_clipboard(ClipboardService::spawn(Failing));
    screen(&mut app);
    let row = row_of(&mut app, "answer 3") + 1;
    app.handle_input(&format!("\x1b[<0;2;{row}M"));
    app.handle_input(&format!("\x1b[<32;6;{row}M"));
    app.handle_input(&format!("\x1b[<0;6;{row}m"));
    poll_until(&mut app, |a| screen(a)[0].contains("Clipboard unavailable"));
    assert!(!app.poll_viewport(Instant::now() + Duration::from_secs(2)));
    assert!(app.poll_viewport(Instant::now() + Duration::from_secs(6)));
}

/// Without a clipboard service the selection goes out as OSC 52 with the frames.
#[test]
fn interactive_fullscreen_copy_without_a_clipboard_writes_osc52() {
    let (mut app, _) = app(3);
    screen(&mut app);
    let row = row_of(&mut app, "answer 3") + 1;
    let col = screen(&mut app)[row - 1].find("answer").unwrap() + 1;
    app.handle_input(&format!("\x1b[<0;{col};{row}M"));
    app.handle_input(&format!("\x1b[<0;{};{row}m", col + 7));
    let osc52: Vec<&String> = app
        .terminal_writes
        .iter()
        .filter(|w| w.starts_with("\x1b]52;"))
        .collect();
    assert_eq!(
        osc52,
        ["\x1b]52;c;YW5zd2VyIDM="]
            .map(|s| format!("{s}\x07"))
            .iter()
            .collect::<Vec<_>>()
    );
    assert!(screen(&mut app)[0].ends_with(" Copied!"));
}

/// A full-frame overlay owns the keys and the mouse (the viewport defers to it); focus reports stay the
/// viewport's and never reach the overlay as keys.
#[test]
fn interactive_fullscreen_overlays_take_the_mouse_and_focus_reports_stay_with_the_viewport() {
    let mut qol = QolSettings::parity();
    qol.overlays = true;
    let (mut app, _) = app_with(12, qol);
    screen(&mut app);
    app.handle_input("\x1ba");
    app.apply(LaneEvent::Agents(Vec::new()));
    let hub = screen(&mut app);
    assert!(hub.iter().any(|l| l.contains("No agents in this session")), "{hub:#?}");
    app.handle_input("\x1b[<64;5;5M");
    app.handle_input("\x1b[<0;5;5M");
    app.handle_input("\x1b[O");
    app.handle_input("\x1b[I");
    assert_eq!(screen(&mut app), hub);
    // The wheel went to the hub, not the transcript behind it.
    assert!(app.viewport().is_following_output());
    app.handle_input("\x1b");
    assert!(!screen(&mut app).iter().any(|l| l.contains("No agents in this session")));
}

#[test]
fn interactive_fullscreen_paint_loop_captures_and_releases_the_mouse() {
    let (mut app, _) = app(2);
    app.set_mouse_tracking(Some(tui::fullscreen::ENABLE_BUTTON_MOTION_MOUSE));
    let (tx, rx) = mpsc::channel();
    tx.send(UiEvent::Input("\x1b[<64;3;3M".into())).unwrap();
    tx.send(UiEvent::InputClosed).unwrap();
    let mut writes = Vec::new();
    paint_loop(
        &mut app,
        &rx,
        &mut |d| {
            writes.push(d.to_string());
            Ok(())
        },
        &|| (60, 16),
        None,
    )
    .unwrap();
    assert_eq!(
        writes[0],
        "\x1b[?1049h\x1b[?7l\x1b[?1000h\x1b[?1002h\x1b[?1004h\x1b[?1006h\x1b[2J\x1b[H\x1b[?25l"
    );
    let release = writes
        .iter()
        .position(|w| w == &tui::screen::AltScreen::release_mouse())
        .unwrap();
    assert!(writes[release + 1].contains("\x1b[?1049l"), "{writes:?}");
    // The wheel report scrolled the transcript; nothing reached the editor.
    assert_eq!(app.editor_text(), "");
}

#[test]
fn interactive_tui_mode_change_says_when_it_applies() {
    let (mut app, saved) = app(1);
    app.handle_input("/settings");
    app.handle_input("\r");
    app.apply(LaneEvent::Settings {
        settings: json!({}),
        env_overrides: Default::default(),
        themes: vec!["dark".into()],
        terminal_theme: "dark".into(),
    });
    for c in "tui mode".chars() {
        app.handle_input(&c.to_string());
    }
    let s = screen(&mut app).join("\n");
    let row = s
        .lines()
        .find(|l| l.contains("TUI mode"))
        .unwrap_or_else(|| panic!("{s}"))
        .to_string();
    // The selector shows the configured mode (default fullscreen, like the TS native client).
    assert!(row.contains("fullscreen") && !row.contains("regular"), "{row}");
    app.handle_input("\r");
    let s = screen(&mut app).join("\n");
    assert_eq!(*saved.lock().unwrap(), ["tuiMode=\"regular\""]);
    let words = s.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        words.contains("Saved tuiMode: regular. It takes effect when Ultron restarts."),
        "{s}"
    );
    let row = s.lines().find(|l| l.contains("TUI mode")).unwrap().to_string();
    assert!(row.contains("regular"), "{row}");
}

fn entry(id: &str, parent: Option<&str>, role: &str, text: &str) -> Value {
    let message = if role == "user" {
        json!({"role": "user", "content": text})
    } else {
        json!({"role": "assistant", "content": [{"type": "text", "text": text}], "stopReason": "stop"})
    };
    json!({"type": "message", "id": id, "parentId": parent, "message": message})
}

/// client-tui-chat.ts `#setWorking`: a navigation operation shows `Navigating... (escape to cancel)` with the
/// spinner until the session answers.
#[test]
fn interactive_tree_navigation_shows_the_navigating_indicator_until_answered() {
    let (mut app, _) = app(1);
    app.handle_input("/tree");
    app.handle_input("\r");
    app.apply(LaneEvent::Tree {
        tree: json!([{"entry": entry("u1", None, "user", "first ask"), "children": [
            {"entry": entry("a1", Some("u1"), "assistant", "One."), "children": [
                {"entry": entry("u2", Some("a1"), "user", "second ask"), "children": []}]}]}]),
        leaf: Some("u2".into()),
    });
    app.handle_input("\x1b[A");
    app.handle_input("\r");
    // No summary.
    app.handle_input("\r");
    let s = screen(&mut app);
    let row = s
        .iter()
        .find(|l| l.contains("Navigating... (escape to cancel)"))
        .unwrap_or_else(|| panic!("{s:#?}"))
        .clone();
    assert!(app.animating(), "the spinner animates");
    app.tick();
    let next = screen(&mut app);
    assert!(
        next.iter()
            .any(|l| l.contains("Navigating... (escape to cancel)") && *l != row),
        "{next:#?}"
    );
    app.apply(LaneEvent::Navigated(Ok(None)));
    let s = screen(&mut app);
    assert!(!s.iter().any(|l| l.contains("Navigating...")), "{s:#?}");
    assert!(!app.animating());
}

/// The scrollback split leaves prompt jumps to the terminal (TS regular mode handles no prompt keys either): the
/// rows it writes to scrollback keep their OSC 133 prompt marks, and Ctrl+Up is not the viewport's.
#[test]
fn interactive_split_mode_keeps_prompt_marks_in_scrollback_for_terminal_jumps() {
    let mut qol = QolSettings::parity();
    qol.viewport_split = true;
    let (mut app, _) = app_with(2, qol);
    let plan = app.frame_plan(W, H);
    let history = plan.history.expect("settled rows go to scrollback");
    let marked: Vec<String> = history
        .rows
        .iter()
        .filter(|r| r.starts_with("\x1b]133;A\x07"))
        .map(|r| strip_terminal_sequences(r).trim().to_string())
        .collect();
    assert_eq!(marked.len(), 4, "{marked:?}");
    app.handle_input("\x1b[1;5A");
    assert!(app.viewport().is_following_output());
    // Home is the editor's again (line start), not the viewport's.
    for c in ["a", "b", "\x1b[H", "X"] {
        app.handle_input(c);
    }
    assert_eq!(app.editor_text(), "Xab");
}

/// A clipboard holding text only, counting text reads.
struct TextClipboard(Arc<Mutex<usize>>, &'static str);

/// A clipboard holding an image and text.
struct ImageAndText;

impl Clipboard for ImageAndText {
    fn get_text(&mut self) -> Result<String, String> {
        Ok("the text".into())
    }
    fn set_text(&mut self, _: &str) -> Result<(), String> {
        Ok(())
    }
    fn get_image_png(&mut self) -> Result<Vec<u8>, String> {
        Ok(b"\x89PNG\r\n\x1a\n".to_vec())
    }
}

impl Clipboard for TextClipboard {
    fn get_text(&mut self) -> Result<String, String> {
        *self.0.lock().unwrap() += 1;
        Ok(self.1.into())
    }
    fn set_text(&mut self, _: &str) -> Result<(), String> {
        Ok(())
    }
    fn get_image_png(&mut self) -> Result<Vec<u8>, String> {
        Err("none".into())
    }
}

/// interactive-mode.ts `handleRightClickPaste` on Windows: a right-button press in the transcript pastes the
/// clipboard's text into the editor as a bracketed paste; its release and other platforms (the default) do not.
/// Clipboard reads answer in order, so a right-click paste and a Ctrl+V each land once.
#[test]
fn interactive_fullscreen_right_click_pastes_the_clipboard_text_when_enabled() {
    let (mut on, _) = app(3);
    let reads = Arc::new(Mutex::new(0));
    on.set_clipboard(ClipboardService::spawn(TextClipboard(
        Arc::clone(&reads),
        "pasted text",
    )));
    on.set_right_click_paste(true);
    screen(&mut on);
    let row = row_of(&mut on, "answer 3") + 1;
    on.handle_input(&format!("\x1b[<2;3;{row}M"));
    on.handle_input(&format!("\x1b[<2;3;{row}m"));
    poll_until(&mut on, |a| !a.editor_text().is_empty());
    assert_eq!(on.editor_text(), "pasted text");
    // A right-click paste, then Ctrl+V: both answers land, in order.
    on.handle_input(&format!("\x1b[<2;3;{row}M"));
    on.handle_input("\x16");
    poll_until(&mut on, |a| a.editor_text().len() > "pasted text".len() * 2);
    assert_eq!(on.editor_text(), "pasted textpasted textpasted text");
    assert_eq!(*reads.lock().unwrap(), 3);
    // It is a bracketed paste (the editor folds a long one into a marker), not typed text as Ctrl+V inserts.
    let (mut long, _) = app(3);
    let text = "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12";
    long.set_clipboard(ClipboardService::spawn(TextClipboard(Arc::new(Mutex::new(0)), text)));
    long.set_right_click_paste(true);
    screen(&mut long);
    long.handle_input(&format!("\x1b[<2;3;{row}M"));
    poll_until(&mut long, |a| !a.editor_text().is_empty());
    assert_eq!(long.editor_text(), "[paste #1 +12 lines]");
    // Ctrl+V first, then a right click: the Ctrl+V answer is typed text, the right click's a bracketed paste.
    let (mut both, _) = app(3);
    both.set_clipboard(ClipboardService::spawn(TextClipboard(Arc::new(Mutex::new(0)), text)));
    both.set_right_click_paste(true);
    screen(&mut both);
    both.handle_input("\x16");
    both.handle_input(&format!("\x1b[<2;3;{row}M"));
    poll_until(&mut both, |a| a.editor_text().ends_with(']'));
    assert_eq!(both.editor_text(), format!("{text}[paste #1 +12 lines]"));
    // A right click pastes the text even when the clipboard also holds an image (it reads text only).
    let (mut image, _) = app(3);
    image.set_clipboard(ClipboardService::spawn(ImageAndText));
    image.set_right_click_paste(true);
    screen(&mut image);
    image.handle_input(&format!("\x1b[<2;3;{row}M"));
    image.handle_input("\x16");
    poll_until(&mut image, |a| {
        a.editor_text().contains("ultron-clipboard-") || a.editor_text().contains("[Image")
    });
    assert!(image.editor_text().starts_with("the text"), "{}", image.editor_text());

    // Off (not Windows, or VS Code's terminal): the press reads nothing; only the following Ctrl+V pastes.
    let (mut off, _) = app(3);
    let reads = Arc::new(Mutex::new(0));
    off.set_clipboard(ClipboardService::spawn(TextClipboard(
        Arc::clone(&reads),
        "pasted text",
    )));
    screen(&mut off);
    off.handle_input(&format!("\x1b[<2;3;{row}M"));
    off.handle_input("\x16");
    poll_until(&mut off, |a| !a.editor_text().is_empty());
    assert_eq!(off.editor_text(), "pasted text");
    assert_eq!(*reads.lock().unwrap(), 1);
}
