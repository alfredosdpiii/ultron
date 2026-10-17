//! Extension UI follows the session's theme. TypeScript initializes the theme in the worker (session-worker.ts:1644
//! `initTheme(settingsManager.getTheme())`) because its extensions render ANSI in-process (tool renderers,
//! `ctx.ui.theme`); Rust extensions are processes with declarative UI (crates/extensions/src/ui.rs: no render
//! code), and the TUI draws their dialogs, statuses and widgets with its own theme, the settings theme. Here: an
//! extension's input and editor dialogs and its selection under a light theme use the light theme's colors.

use std::rc::Rc;

use interactive::app::{App, AppOptions};
use interactive::history::PromptHistoryStore;
use interactive::lane::{LaneClient, LaneEvent, ModelInfo, SessionOpened};
use serde_json::{Value, json};
use tui::keybindings::{KeybindingsManager, Platform};
use tui::qol_keys::native_app_keybindings;
use tui::theme::{ColorMode, Theme, builtin_theme_json, create_theme};

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
    fn extension_ui_respond(&self, _: &str, _: Value) {}
}

fn theme(name: &str) -> Rc<Theme> {
    Rc::new(create_theme(&builtin_theme_json(name).unwrap(), ColorMode::Truecolor, None).unwrap())
}

fn app(theme: Rc<Theme>) -> App {
    let keys = || {
        KeybindingsManager::new(
            native_app_keybindings(Platform::new("linux", false)),
            Vec::new(),
            Default::default(),
        )
    };
    let model = ModelInfo {
        provider: "anthropic".into(),
        id: "a".into(),
        name: "Model a".into(),
        reasoning: false,
        context_window: 1000,
        thinking_levels: Vec::new(),
    };
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
            qol: interactive::qol::QolSettings::parity(),
        },
        Box::new(Quiet),
    );
    app.apply(LaneEvent::Opened(SessionOpened {
        session_id: "s".into(),
        session_file: None,
        name: None,
        messages: Vec::new(),
    }));
    app
}

/// The first rendered row containing `text`.
fn row_with(app: &mut App, text: &str) -> String {
    app.render(80, 30)
        .into_iter()
        .find(|l| tui::utils::strip_terminal_sequences(l).contains(text))
        .unwrap_or_else(|| panic!("{text}"))
}

#[test]
fn interactive_extension_dialogs_are_drawn_in_the_session_theme() {
    for name in ["light", "dark"] {
        let t = theme(name);
        let accent = t.fg("accent", "Name?").unwrap();
        let mut app = app(Rc::clone(&t));
        app.apply(LaneEvent::ExtensionUi(
            json!({"type": "extension_ui_request", "id": "p1", "method": "input", "title": "Name?"}),
        ));
        assert!(
            row_with(&mut app, "Name?").contains(&accent),
            "{name}: the input's title"
        );
        app.handle_input("\x1b");
        app.apply(LaneEvent::ExtensionUi(
            json!({"type": "extension_ui_request", "id": "e1", "method": "editor", "title": "Edit it"}),
        ));
        assert!(
            row_with(&mut app, "Edit it").contains(&t.fg("accent", "Edit it").unwrap()),
            "{name}: the editor's title"
        );
        app.handle_input("\x1b");
        app.apply(LaneEvent::ExtensionUi(
            json!({"type": "extension_ui_request", "id": "d1", "method": "select",
            "title": "Pick", "options": ["one", "two"]}),
        ));
        let selected = row_with(&mut app, "one");
        let other = theme(if name == "light" { "dark" } else { "light" });
        assert_ne!(t.fg("accent", "x").unwrap(), other.fg("accent", "x").unwrap());
        let accent_code = t.fg("accent", "").unwrap();
        let code = accent_code.trim_end_matches("\x1b[39m");
        assert!(selected.contains(code), "{name}: the selected option {selected:?}");
    }
}
