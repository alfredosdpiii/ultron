//! Extension shortcuts in the TUI (interactive-mode.ts `setupExtensionShortcuts`, custom-editor.ts `handleInput`):
//! an opened session is asked for its shortcuts under the effective keybindings, a key matching one runs it in the
//! session before any built-in binding takes the key, other keys reach the editor, `/hotkeys` lists them under
//! **Extensions**, and the conflict warnings show as `[Extension issues]`. Literal expectations from the TypeScript
//! source (`hotkeys` markdown, `formatDiagnostics`).

use std::cell::RefCell;
use std::rc::Rc;

use interactive::app::{App, AppOptions};
use interactive::extension_shortcuts::{ExtensionShortcut, ShortcutDiagnostic, hotkeys_section, issues, matching};
use interactive::history::PromptHistoryStore;
use interactive::lane::{LaneClient, LaneEvent, ModelInfo, SessionOpened};
use tui::keybindings::{KeybindingsManager, Platform};
use tui::qol_keys::native_app_keybindings;
use tui::theme::{ColorMode, builtin_theme_json, create_theme};

#[derive(Default)]
struct Calls(RefCell<Vec<String>>);

struct Recorder(Rc<Calls>);

impl Recorder {
    fn push(&self, call: String) {
        self.0.0.borrow_mut().push(call);
    }
}

impl LaneClient for Recorder {
    fn prompt(&self, text: &str) {
        self.push(format!("prompt {text}"));
    }
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
    fn extension_command(&self, name: &str, args: &str) {
        self.push(format!("extension {name}|{args}"));
    }
    fn extension_completions(&self, name: &str, prefix: &str) {
        self.push(format!("complete {name}|{prefix}"));
    }
    fn extension_shortcuts(&self, keybindings: Vec<(String, Vec<String>)>) {
        let interrupt = keybindings
            .iter()
            .find(|(a, _)| a == "app.interrupt")
            .map(|(_, k)| k.join(","));
        self.push(format!("shortcuts app.interrupt={}", interrupt.unwrap_or_default()));
    }
    fn extension_shortcut(&self, key: &str, keybindings: Vec<(String, Vec<String>)>) {
        self.push(format!("shortcut {key} ({})", keybindings.len() > 10));
    }
}

fn app() -> (App, Rc<Calls>) {
    let theme = Rc::new(create_theme(&builtin_theme_json("dark").unwrap(), ColorMode::Truecolor, None).unwrap());
    let keys = || {
        KeybindingsManager::new(
            native_app_keybindings(Platform::new("linux", false)),
            Vec::new(),
            Default::default(),
        )
    };
    let model = ModelInfo {
        provider: "p".into(),
        id: "m".into(),
        name: "m".into(),
        reasoning: false,
        context_window: 1000,
        thinking_levels: Vec::new(),
    };
    let calls = Rc::new(Calls::default());
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
            thinking_level: "medium".into(),
            splash: false,
            history: PromptHistoryStore::new(None, "/p"),
            fd_path: None,
            highlight: Rc::new(|code: &str| code.split('\n').map(str::to_string).collect()),
            qol: interactive::qol::QolSettings::parity(),
        },
        Box::new(Recorder(calls.clone())),
    );
    app.apply(LaneEvent::Opened(SessionOpened {
        session_id: "sess".into(),
        session_file: None,
        name: None,
        messages: Vec::new(),
    }));
    (app, calls)
}

fn typed(app: &mut App, text: &str) {
    for c in text.chars() {
        app.handle_input(&c.to_string());
    }
}

fn submit(app: &mut App, text: &str) {
    typed(app, text);
    app.handle_input("\r");
}

fn screen(app: &mut App) -> String {
    app.render(100, 30)
        .iter()
        .map(|l| tui::utils::strip_terminal_sequences(l).trim_end().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn shortcut(key: &str, description: Option<&str>) -> ExtensionShortcut {
    ExtensionShortcut {
        key: key.into(),
        description: description.map(str::to_string),
        extension_path: "/x/ext/extension.json".into(),
    }
}

fn shortcut_calls(calls: &Calls) -> Vec<String> {
    calls
        .0
        .borrow()
        .iter()
        .filter(|c| c.starts_with("shortcut"))
        .cloned()
        .collect()
}

#[test]
fn interactive_extension_shortcuts_are_asked_for_when_a_session_opens() {
    let (_, calls) = app();
    assert_eq!(shortcut_calls(&calls), ["shortcuts app.interrupt=escape"]);
}

#[test]
fn interactive_extension_shortcuts_run_before_builtin_keys_and_leave_other_keys_to_the_editor() {
    let (mut app, calls) = app();
    calls.0.borrow_mut().clear();
    // Before the session answered nothing is bound: Alt+H is the editor's.
    app.handle_input("\x1bh");
    assert!(shortcut_calls(&calls).is_empty());
    app.apply(LaneEvent::ExtensionShortcuts {
        shortcuts: vec![shortcut("alt+h", Some("Say hello")), shortcut("Ctrl+V", None)],
        diagnostics: Vec::new(),
    });
    app.handle_input("\x1bh");
    // Ctrl+V is the clipboard paste built-in; the extension's registration takes it (a warning said so).
    app.handle_input("\x16");
    typed(&mut app, "hi");
    assert_eq!(
        shortcut_calls(&calls),
        ["shortcut alt+h (true)", "shortcut Ctrl+V (true)"]
    );
    assert!(screen(&mut app).contains("hi"));
    // A new session's shortcuts replace them; until it answers none are bound.
    app.apply(LaneEvent::Opened(SessionOpened {
        session_id: "other".into(),
        session_file: None,
        name: None,
        messages: Vec::new(),
    }));
    app.handle_input("\x1bh");
    assert_eq!(shortcut_calls(&calls).len(), 3);
    assert_eq!(shortcut_calls(&calls)[2], "shortcuts app.interrupt=escape");
}

#[test]
fn interactive_extension_shortcuts_match_keys_case_insensitively() {
    let shortcuts = [shortcut("CTRL+SHIFT+X", None), shortcut("alt+h", None)];
    assert_eq!(matching(&shortcuts, "\x1bh").map(|s| s.key.as_str()), Some("alt+h"));
    // Ctrl+Shift+X in the Kitty keyboard protocol.
    assert_eq!(
        matching(&shortcuts, "\x1b[120;6u").map(|s| s.key.as_str()),
        Some("CTRL+SHIFT+X")
    );
    assert_eq!(matching(&shortcuts, "h"), None);
}

#[test]
fn interactive_extension_shortcuts_are_listed_by_hotkeys() {
    assert_eq!(hotkeys_section(&[]), "");
    assert_eq!(
        hotkeys_section(&[shortcut("ctrl+shift+h", Some("Say hello")), shortcut("alt+z", None)]),
        "\n**Extensions**\n| Key | Action |\n|-----|--------|\n| `Ctrl+Shift+H` | Say hello |\n| `Alt+Z` | /x/ext/extension.json |\n"
    );
    let (mut app, _) = app();
    app.apply(LaneEvent::ExtensionShortcuts {
        shortcuts: vec![shortcut("alt+h", Some("Say hello"))],
        diagnostics: Vec::new(),
    });
    submit(&mut app, "/hotkeys");
    let shown = screen(&mut app);
    let extensions = shown.find("Extensions").expect("an Extensions section");
    assert!(
        shown[extensions..].contains("Alt+H") && shown[extensions..].contains("Say hello"),
        "{shown}"
    );
}

#[test]
fn interactive_extension_shortcut_conflicts_show_as_extension_issues() {
    let warnings = vec![ShortcutDiagnostic {
        message: "Extension shortcut 'ctrl+c' from /x/ext/extension.json conflicts with built-in shortcut. Skipping."
            .into(),
        path: "/x/ext/extension.json".into(),
        error: false,
    }];
    assert_eq!(issues(&[]), None);
    assert_eq!(
        issues(&warnings).unwrap(),
        "[Extension issues]\n  /x/ext/extension.json\n    Extension shortcut 'ctrl+c' from /x/ext/extension.json conflicts with built-in shortcut. Skipping."
    );
    let (mut app, _) = app();
    app.apply(LaneEvent::ExtensionShortcuts {
        shortcuts: Vec::new(),
        diagnostics: warnings,
    });
    let shown = screen(&mut app);
    assert!(
        shown.contains("[Extension issues]") && shown.contains("conflicts with built-in shortcut"),
        "{shown}"
    );
}

/// `/reload` re-reads `<agentDir>/keybindings.json` (`KeybindingsManager.reload()`): the app's keys, the editor's,
/// and the extension shortcut conflicts (resolved again under the effective bindings) all follow the file.
#[test]
fn interactive_keybindings_reload_applies_the_file_to_app_editor_and_shortcut_conflicts() {
    let dir = std::env::temp_dir().join(format!("interactive-keybindings-reload-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = tui::keybindings_file::keybindings_path(&dir);
    // Legacy names migrate (`exit`, `cursorLineStart`, `interrupt`).
    std::fs::write(
        &path,
        r#"{"exit": "ctrl+q", "cursorLineStart": "f6", "interrupt": ["ctrl+x"]}"#,
    )
    .unwrap();
    let defs = tui::keybindings::app_keybindings(Platform::new("linux", false));
    let user = tui::keybindings_file::load_user_bindings(&path, &defs);
    let _ = std::fs::remove_dir_all(&dir);

    let (mut app, calls) = app();
    calls.0.borrow_mut().clear();
    app.apply(LaneEvent::Keybindings(user));
    assert_eq!(*calls.0.borrow(), ["shortcuts app.interrupt=ctrl+x"]);
    typed(&mut app, "abc");
    app.handle_input("\x1b[17~");
    typed(&mut app, "X");
    assert_eq!(app.editor_text(), "Xabc");
    // Ctrl+A is no longer cursorLineStart: the cursor stays after "X".
    app.handle_input("\x01");
    typed(&mut app, "Y");
    assert_eq!(app.editor_text(), "XYabc");
    // Ctrl+E (End belongs to the fullscreen viewport).
    app.handle_input("\x05");
    for _ in 0..5 {
        app.handle_input("\x7f");
    }
    assert_eq!(app.editor_text(), "");
    app.handle_input("\x04");
    assert!(!app.exit, "ctrl+d is no longer app.exit");
    app.handle_input("\x11");
    assert!(app.exit, "ctrl+q is app.exit");
}
