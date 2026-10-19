//! The file-restoring rewind in the TUI (Rust-only feature): `/tree`'s Shift+R and the Esc-Esc rewind selector's
//! `r` ask for a confirmation, then send `restore_files` for the entry; the selector's second choice also rewinds
//! the conversation once the files were restored (and not when the restore failed). The tree shows the restore key
//! in its help only when some entry has a checkpoint.

use std::cell::RefCell;
use std::rc::Rc;

use interactive::app::{App, AppOptions};
use interactive::history::PromptHistoryStore;
use interactive::lane::{ForkMessage, LaneClient, LaneEvent, ModelInfo, SessionOpened};
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
    fn new_session(&self) {}
    fn resume(&self, _: &str) {}
    fn list_sessions(&self) {}
    fn set_name(&self, _: &str) {}
    fn select_model(&self, _: &str, _: &str) {}
    fn abort(&self) {
        self.0.0.borrow_mut().push("abort".into());
    }
    fn fork_messages(&self) {
        self.0.0.borrow_mut().push("fork_messages".into());
    }
    fn fork(&self, entry_id: &str) {
        self.0.0.borrow_mut().push(format!("fork {entry_id}"));
    }
    fn tree(&self) {
        self.0.0.borrow_mut().push("tree".into());
    }
    fn restore_files(&self, entry_id: &str) {
        self.0.0.borrow_mut().push(format!("restore_files {entry_id}"));
    }
}

fn model() -> ModelInfo {
    ModelInfo {
        provider: "anthropic".into(),
        id: "a".into(),
        name: "Model a".into(),
        reasoning: true,
        context_window: 1000,
        thinking_levels: Vec::new(),
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
            models: vec![model()],
            model: model(),
            thinking_level: "medium".into(),
            splash: false,
            history: PromptHistoryStore::new(None, "/p"),
            fd_path: None,
            highlight: Rc::new(|code: &str| code.split('\n').map(str::to_string).collect()),
            qol: interactive::qol::QolSettings::default(),
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

fn screen(app: &mut App) -> String {
    app.render(100, 40)
        .iter()
        .map(|l| tui::utils::strip_terminal_sequences(l).trim_end().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn calls(c: &Calls) -> Vec<String> {
    c.0.borrow().clone()
}

fn entry(id: &str, parent: Option<&str>, role: &str, text: &str) -> Value {
    let message = if role == "user" {
        json!({"role": "user", "content": text})
    } else {
        json!({"role": "assistant", "content": [{"type": "text", "text": text}], "stopReason": "stop"})
    };
    json!({"type": "message", "id": id, "parentId": parent, "message": message})
}

fn tree(checkpoint: bool) -> Value {
    let mut first = json!({"entry": entry("u1", None, "user", "first ask"), "children": [
        {"entry": entry("a1", Some("u1"), "assistant", "One."), "children": [
            {"entry": entry("u2", Some("a1"), "user", "second ask"), "children": [
                {"entry": entry("a2", Some("u2"), "assistant", "Two."), "children": []}]}]}]});
    if checkpoint {
        first["checkpoint"] = json!(true);
    }
    json!([first])
}

#[test]
fn interactive_restore_files_from_the_tree_after_confirming() {
    let (mut app, c) = app();
    app.handle_input("/");
    for ch in "tree".chars() {
        app.handle_input(&ch.to_string());
    }
    app.handle_input("\r");
    // Without checkpoints the help is TypeScript's.
    app.apply(LaneEvent::Tree {
        tree: tree(false),
        leaf: Some("a2".into()),
    });
    assert!(!screen(&mut app).contains("restore files"));
    app.apply(LaneEvent::Tree {
        tree: tree(true),
        leaf: Some("a2".into()),
    });
    let s = screen(&mut app);
    assert!(s.contains("shift+r restore files"), "{s}");
    for _ in 0..3 {
        app.handle_input("\x1b[A");
    }
    assert!(screen(&mut app).contains("› • user: first ask"));
    app.handle_input("R");
    let s = screen(&mut app);
    assert!(s.contains("Restore files to this point?"), "{s}");
    assert!(s.contains("saved as a checkpoint first"), "{s}");
    assert!(!s.contains("rewind the conversation"), "{s}");
    // Cancel sends nothing.
    app.handle_input("\x1b[B");
    app.handle_input("\r");
    assert!(!calls(&c).iter().any(|call| call.starts_with("restore_files")));
    // Again, confirmed.
    app.handle_input("/");
    for ch in "tree".chars() {
        app.handle_input(&ch.to_string());
    }
    app.handle_input("\r");
    app.apply(LaneEvent::Tree {
        tree: tree(true),
        leaf: Some("a2".into()),
    });
    app.handle_input("\x1b[A");
    app.handle_input("R");
    app.handle_input("\r");
    assert_eq!(calls(&c).last().unwrap(), "restore_files u2");
    app.apply(LaneEvent::FilesRestored(Ok("Restored files to checkpoint abc".into())));
    assert!(screen(&mut app).contains("Restored files to checkpoint abc"));
    app.apply(LaneEvent::FilesRestored(Err("No checkpoint".into())));
    assert!(screen(&mut app).contains("Error: No checkpoint"));
    // Only a restore: no fork followed.
    assert!(!calls(&c).iter().any(|call| call.starts_with("fork ")));
}

fn rewind_selector(app: &mut App) {
    app.handle_input("\x1b");
    app.handle_input("\x1b");
    app.apply(LaneEvent::ForkMessages(vec![
        ForkMessage {
            entry_id: "u1".into(),
            text: "one".into(),
        },
        ForkMessage {
            entry_id: "u2".into(),
            text: "two".into(),
        },
    ]));
}

#[test]
fn interactive_restore_files_from_the_rewind_selector_and_rewind_after_success() {
    let (mut app, c) = app();
    for (role, text) in [
        ("user", "one"),
        ("assistant", "One."),
        ("user", "two"),
        ("assistant", "Two."),
    ] {
        let message = if role == "user" {
            json!({"role": "user", "content": text})
        } else {
            json!({"role": "assistant", "content": [{"type": "text", "text": text}], "stopReason": "stop"})
        };
        app.apply(LaneEvent::Message(message));
    }
    rewind_selector(&mut app);
    let s = screen(&mut app);
    assert!(s.contains("r restore files"), "{s}");
    // After the newest reply the files are already there.
    app.handle_input("r");
    assert!(screen(&mut app).contains("Files are already at this point"));
    // The second user turn: files before it, and the conversation rewound there once they are restored.
    rewind_selector(&mut app);
    app.handle_input("\x1b[D");
    app.handle_input("r");
    let s = screen(&mut app);
    assert!(s.contains("Restore files to before this turn?"), "{s}");
    app.handle_input("\x1b[B");
    app.handle_input("\r");
    assert_eq!(calls(&c).last().unwrap(), "restore_files u2");
    app.apply(LaneEvent::FilesRestored(Ok("Restored files to checkpoint abc".into())));
    assert_eq!(calls(&c).last().unwrap(), "fork u2");
    // A failed restore does not rewind the conversation.
    rewind_selector(&mut app);
    app.handle_input("\x1b[D");
    app.handle_input("r");
    app.handle_input("\x1b[B");
    app.handle_input("\r");
    let before = calls(&c).len();
    app.apply(LaneEvent::FilesRestored(Err("Restoring would overwrite files".into())));
    assert_eq!(calls(&c).len(), before);
    assert!(screen(&mut app).contains("Error: Restoring would overwrite files"));
    // "Restore files" alone does not rewind either.
    rewind_selector(&mut app);
    app.handle_input("\x1b[D");
    app.handle_input("r");
    app.handle_input("\r");
    assert_eq!(calls(&c).last().unwrap(), "restore_files u2");
    app.apply(LaneEvent::FilesRestored(Ok("done".into())));
    assert_eq!(calls(&c).last().unwrap(), "restore_files u2");
}
