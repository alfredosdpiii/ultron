//! The client TUI while its Radius connection is recovered (client-tui.ts `#handleConnectionState`,
//! `#handleAttachmentState` and `handleInput` while `#busy`), against a recording `LaneClient`: the recovery status
//! shows, typed input and Enter wait, Ctrl+C still clears and a second one exits, Ctrl+D exits on an empty editor,
//! and the end of the recovery clears the status and takes input again.

use std::cell::RefCell;
use std::rc::Rc;

use interactive::app::{App, AppOptions};
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
    fn steer(&self, text: &str) -> String {
        self.push(format!("steer {text}"));
        "s1".into()
    }
    fn follow_up(&self, text: &str) -> String {
        self.push(format!("follow_up {text}"));
        "f1".into()
    }
    fn cancel_queued(&self, id: &str) {
        self.push(format!("cancel {id}"));
    }
    fn abort(&self) {
        self.push("abort".into());
    }
    fn new_session(&self) {
        self.push("new".into());
    }
    fn resume(&self, path: &str) {
        self.push(format!("resume {path}"));
    }
    fn list_sessions(&self) {
        self.push("list".into());
    }
    fn set_name(&self, name: &str) {
        self.push(format!("name {name}"));
    }
    fn select_model(&self, provider: &str, id: &str) {
        self.push(format!("model {provider}/{id}"));
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
        provider: "anthropic".into(),
        id: "m".into(),
        name: "M".into(),
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

fn screen(app: &mut App) -> String {
    app.render(80, 30)
        .iter()
        .map(|l| tui::utils::strip_terminal_sequences(l).trim_end().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn recovery(busy: bool, status: &str) -> LaneEvent {
    LaneEvent::Recovery {
        busy,
        status: status.into(),
    }
}

#[test]
fn interactive_radius_recovery_waits_for_the_connection() {
    let (mut app, calls) = app();
    typed(&mut app, "draft");
    app.apply(recovery(true, "Radius disconnected; retrying…"));
    assert!(screen(&mut app).contains("Radius disconnected; retrying…"));
    // Typing and Enter wait: the editor keeps its draft, nothing is sent.
    typed(&mut app, "more");
    app.handle_input("\r");
    assert!(calls.0.borrow().is_empty());
    let shown = screen(&mut app);
    assert!(shown.contains("draft") && !shown.contains("draftmore"), "{shown}");
    app.apply(recovery(true, "Reconnecting to Radius…"));
    let shown = screen(&mut app);
    assert!(
        shown.contains("Reconnecting to Radius…") && !shown.contains("retrying"),
        "{shown}"
    );
    // Ctrl+D waits while the editor has text; Ctrl+C clears it, then Ctrl+D exits.
    app.handle_input("\x04");
    assert!(!app.exit);
    app.handle_input("\x03");
    assert!(!screen(&mut app).contains("draft"));
    app.handle_input("\x04");
    assert!(app.exit);
}

#[test]
fn interactive_radius_recovery_ends_with_input_back() {
    let (mut app, calls) = app();
    app.apply(recovery(true, "Reattaching Session…"));
    assert!(screen(&mut app).contains("Reattaching Session…"));
    // A second Ctrl+C within the window exits, as it does outside recovery.
    let (mut other, _) = self::app();
    other.apply(recovery(true, "Reattaching Session…"));
    other.handle_input("\x03");
    assert!(!other.exit);
    other.handle_input("\x03");
    assert!(other.exit);
    // Attached again: the status clears and input reaches the session.
    app.apply(recovery(false, ""));
    assert!(!screen(&mut app).contains("Reattaching Session…"));
    typed(&mut app, "hello");
    app.handle_input("\r");
    assert_eq!(calls.0.borrow().clone(), vec!["prompt hello".to_string()]);
}
