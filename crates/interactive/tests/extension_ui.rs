//! `/mcp` in the TUI (client-tui.ts `#serveExtensionUI` and slash-commands-provider.ts `directExtensionCommand`): the
//! command runs in the session's worker (`pi.legacy-extension-commands.run`); its dialogs arrive as Pi's
//! `extension_ui_request` and are answered one at a time (`select` in the selector, `confirm` as Yes/No, `input` in a
//! text prompt, Esc cancels), and `notify` shows in the status line.

use std::cell::RefCell;
use std::rc::Rc;

use interactive::app::{App, AppOptions};
use interactive::history::PromptHistoryStore;
use interactive::lane::{LaneClient, LaneEvent, ModelInfo, SessionOpened};
use serde_json::{Value, json};
use tui::keybindings::{KeybindingsManager, Platform};
use tui::qol_keys::native_app_keybindings;
use tui::theme::{ColorMode, builtin_theme_json, create_theme};

#[derive(Default)]
struct Calls(RefCell<Vec<String>>);

struct Recorder(Rc<Calls>);

impl LaneClient for Recorder {
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
    fn extension_command(&self, name: &str, args: &str) {
        self.0.0.borrow_mut().push(format!("run {name} [{args}]"));
    }
    fn extension_ui_respond(&self, id: &str, answer: Value) {
        self.0.0.borrow_mut().push(format!("respond {id} {answer}"));
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
        id: "a".into(),
        name: "Model a".into(),
        reasoning: true,
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

fn submit(app: &mut App, text: &str) {
    for c in text.chars() {
        app.handle_input(&c.to_string());
    }
    app.handle_input("\r");
}

fn screen(app: &mut App) -> String {
    app.render(100, 30)
        .iter()
        .map(|l| tui::utils::strip_terminal_sequences(l).trim_end().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn calls(c: &Calls) -> Vec<String> {
    c.0.borrow().clone()
}

fn request(value: Value) -> LaneEvent {
    LaneEvent::ExtensionUi(value)
}

#[test]
fn interactive_mcp_runs_in_the_worker_and_its_dialogs_are_answered_in_order() {
    let (mut app, recorded) = app();
    submit(&mut app, "/mcp");
    submit(&mut app, "/mcp login docs");
    assert_eq!(calls(&recorded), ["run mcp []", "run mcp [login docs]"]);
    // Two dialogs at once: the second waits for the first.
    app.apply(request(
        json!({"type": "extension_ui_request", "id": "d1", "method": "select",
        "title": "MCP servers", "options": ["docs \u{2014} not connected", "web \u{2014} connected"]}),
    ));
    app.apply(request(
        json!({"type": "extension_ui_request", "id": "d2", "method": "confirm",
        "title": "Sure?", "message": "really"}),
    ));
    let shown = screen(&mut app);
    assert!(shown.contains("MCP servers"), "{shown}");
    assert!(shown.contains("web \u{2014} connected"), "{shown}");
    app.handle_input("\x1b[B");
    app.handle_input("\r");
    assert!(screen(&mut app).contains("Sure?: really"));
    app.handle_input("\r");
    assert_eq!(
        calls(&recorded)[2..],
        [
            "respond d1 {\"value\":\"web \u{2014} connected\"}".to_string(),
            "respond d2 {\"confirmed\":true}".to_string(),
        ]
    );
}

#[test]
fn interactive_mcp_input_dialogs_answer_or_cancel_and_notices_show_in_the_status_line() {
    let (mut app, recorded) = app();
    app.apply(request(
        json!({"type": "extension_ui_request", "id": "p1", "method": "input",
        "title": "MCP sign-in for docs: paste the redirect URL, or wait for the browser.",
        "placeholder": "http://127.0.0.1/callback?code=..."}),
    ));
    assert!(screen(&mut app).contains("MCP sign-in for docs"));
    for c in "http://x/cb?code=1".chars() {
        app.handle_input(&c.to_string());
    }
    app.handle_input("\r");
    app.apply(request(
        json!({"type": "extension_ui_request", "id": "p2", "method": "input", "title": "Again"}),
    ));
    app.handle_input("\x1b");
    app.apply(request(
        json!({"type": "extension_ui_request", "id": "n1", "method": "notify",
        "message": "Signed in to \"docs\" (3 tools).", "notifyType": "info"}),
    ));
    assert_eq!(
        calls(&recorded),
        [
            "respond p1 {\"value\":\"http://x/cb?code=1\"}",
            "respond p2 {\"cancelled\":true}",
        ]
    );
    assert!(screen(&mut app).contains("Signed in to \"docs\" (3 tools)."));
}

#[test]
fn interactive_extension_command_names_the_command() {
    let (mut app, recorded) = app();
    submit(&mut app, "/extension mcp status");
    submit(&mut app, "/extension");
    assert_eq!(calls(&recorded), ["run mcp [status]"]);
    assert!(screen(&mut app).contains("Usage: /extension <name> [args]"));
}
