//! Extension commands in the client TUI (slash-commands-provider.ts): `/extension <name> [args]`
//! (`legacyExtensionCommand`) and each extension command as a slash command of its own (`directExtensionCommand`,
//! registered after `/extension`; a name a built-in has, or one `SlashCommandRegistry` refuses, is skipped and
//! stays reachable through `/extension`), described as registered, else "Extension command". The list is the
//! session's, in its load order (crates/session-core/fixtures/extension-commands.json); literal expectations from
//! the TypeScript source.

use std::cell::RefCell;
use std::rc::Rc;

use interactive::app::{App, AppOptions};
use interactive::extension_commands::{direct_extension_commands, ordered, valid_name};
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

/// The session's extension commands in the fixture's load order, plus a name a built-in has and an invalid one.
fn listed() -> Vec<(String, Option<String>)> {
    [
        ("proj-entry", Some("Listed in project settings")),
        ("proj-cmd", Some("Project command")),
        ("listed-cmd", Some("Listed in settings")),
        ("greet:1", Some("Another greet")),
        ("greet:2", Some("Say hello")),
        ("bare", None),
        (
            "mcp",
            Some("MCP servers: status, connect or disconnect, sign in, enable or disable"),
        ),
        ("model", Some("An extension's model command")),
        ("Bad_Name", Some("Not a slash command name")),
    ]
    .into_iter()
    .map(|(n, d)| (n.to_string(), d.map(str::to_string)))
    .collect()
}

#[test]
fn interactive_extension_commands_direct_commands_skip_builtins_and_invalid_names() {
    assert!(valid_name("greet:1") && valid_name("a-b") && valid_name("9x"));
    assert!(!valid_name("Bad_Name") && !valid_name(":x") && !valid_name("-x") && !valid_name(""));
    let direct = direct_extension_commands(&listed());
    let names: Vec<&str> = direct.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        ["proj-entry", "proj-cmd", "listed-cmd", "greet:1", "greet:2", "bare"]
    );
    assert_eq!(direct[5].1, "Extension command");
    assert_eq!(direct[4].1, "Say hello");
}

#[test]
fn interactive_extension_commands_run_directly_and_through_slash_extension() {
    let (mut app, calls) = app();
    // Before the session listed them, an extension command is unknown.
    submit(&mut app, "/greet:2 a b");
    assert!(calls.0.borrow().is_empty());
    app.apply(LaneEvent::ExtensionCommands(listed()));
    submit(&mut app, "/greet:2 a b");
    submit(&mut app, "/bare");
    submit(&mut app, "/extension greet:1 x y");
    submit(&mut app, "/extension Bad_Name z");
    // A name the registry refuses is no slash command of its own (`/extension` above still runs it).
    submit(&mut app, "/Bad_Name");
    let shown = screen(&mut app);
    assert!(shown.contains("Unknown slash command: /Bad_Name"), "{shown}");
    let runs: Vec<String> = calls
        .0
        .borrow()
        .iter()
        .filter(|c| c.starts_with("extension "))
        .cloned()
        .collect();
    assert_eq!(
        runs,
        [
            "extension greet:2|a b",
            "extension bare|",
            "extension greet:1|x y",
            "extension Bad_Name|z"
        ]
    );
    submit(&mut app, "/extension");
    assert!(
        screen(&mut app).contains("Error: Usage: /extension <name> [args]"),
        "{}",
        screen(&mut app)
    );
}

#[test]
fn interactive_extension_commands_are_offered_by_autocomplete_with_their_descriptions() {
    let (mut app, _) = app();
    app.apply(LaneEvent::ExtensionCommands(listed()));
    typed(&mut app, "/greet");
    let shown = screen(&mut app);
    assert!(shown.contains("greet:1") && shown.contains("Another greet"), "{shown}");
    assert!(shown.contains("greet:2") && shown.contains("Say hello"), "{shown}");
    let (mut app, _) = self::app();
    app.apply(LaneEvent::ExtensionCommands(listed()));
    typed(&mut app, "/bar");
    let shown = screen(&mut app);
    assert!(shown.contains("bare") && shown.contains("Extension command"), "{shown}");
}

#[test]
fn interactive_extension_commands_sit_after_slash_extension_before_mcp() {
    let names: Vec<String> = ordered(&listed()).into_iter().map(|(name, _, _)| name).collect();
    let at = names.iter().position(|n| n == "extension").unwrap();
    assert_eq!(
        names[at..at + 9],
        [
            "extension",
            "proj-entry",
            "proj-cmd",
            "listed-cmd",
            "greet:1",
            "greet:2",
            "bare",
            "mcp",
            "thinking"
        ]
    );
    // Without extension commands, the client TUI's own list.
    let plain: Vec<String> = ordered(&[]).into_iter().map(|(name, _, _)| name).collect();
    assert_eq!(plain.len(), interactive::commands::COMMANDS.len());
    assert_eq!(plain.len() + 6, names.len());
    assert_eq!(names.iter().filter(|n| *n == "mcp").count(), 1);
}

#[test]
fn interactive_extension_commands_complete_arguments_off_the_ui_thread() {
    let (mut app, calls) = app();
    app.apply(LaneEvent::ExtensionCommands(listed()));
    typed(&mut app, "/greet:2 a");
    while app.fire_autocomplete() {}
    // Asked once for the prefix typed; nothing shown until the answer arrives.
    let asked: Vec<String> = calls
        .0
        .borrow()
        .iter()
        .filter(|c| c.starts_with("complete"))
        .cloned()
        .collect();
    assert_eq!(
        asked.last().map(String::as_str),
        Some("complete greet:2|a"),
        "{asked:?}"
    );
    assert!(!screen(&mut app).contains("alpha"));
    app.apply(LaneEvent::ExtensionCompletions {
        name: "greet:2".into(),
        prefix: "a".into(),
        items: Some(vec![("alpha".into(), "alpha".into(), Some("the first".into()))]),
    });
    let shown = screen(&mut app);
    assert!(shown.contains("alpha") && shown.contains("the first"), "{shown}");
    // A built-in's arguments are never asked of an extension.
    let (mut app, calls2) = self::app();
    app.apply(LaneEvent::ExtensionCommands(listed()));
    typed(&mut app, "/thinking h");
    while app.fire_autocomplete() {}
    assert!(!calls2.0.borrow().iter().any(|c| c.starts_with("complete")));
}
