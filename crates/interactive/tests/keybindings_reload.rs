//! `/reload` re-reads `<agentDir>/keybindings.json` (`KeybindingsManager.reload()`): the app's keys and the
//! editor's follow the file.

use std::rc::Rc;

use interactive::app::{App, AppOptions};
use interactive::history::PromptHistoryStore;
use interactive::lane::{LaneClient, LaneEvent, ModelInfo, SessionOpened};
use tui::keybindings::{KeybindingsManager, Platform};
use tui::qol_keys::native_app_keybindings;
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
}

fn app() -> App {
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
        Box::new(Quiet),
    );
    app.apply(LaneEvent::Opened(SessionOpened {
        session_id: "sess".into(),
        session_file: None,
        name: None,
        messages: Vec::new(),
    }));
    app
}

fn typed(app: &mut App, text: &str) {
    for c in text.chars() {
        app.handle_input(&c.to_string());
    }
}

#[test]
fn interactive_keybindings_reload_applies_the_file_to_app_and_editor() {
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

    let mut app = app();
    app.apply(LaneEvent::Keybindings(user));
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
