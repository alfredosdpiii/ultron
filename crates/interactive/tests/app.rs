//! Ported tests: experimental-ultron-logo.test.ts (splash) and prompt-history behavior from
//! experimental/prompt-history.ts; plus the client TUI's key and command handling against a recording
//! `LaneClient` (what each key sends to the session), with literal expectations from client-tui.ts.

use std::cell::RefCell;
use std::rc::Rc;

use interactive::app::{App, AppOptions, context_usage};
use interactive::header::{ULTRON_LOGO, color_logo_line, halve_logo, splash_lines};
use interactive::history::{PromptHistoryStore, merge_prompt_history};
use interactive::lane::{LaneClient, LaneEvent, ModelInfo, SessionOpened};
use serde_json::json;
use tui::keybindings::{KeybindingsManager, Platform, app_keybindings};
use tui::theme::{ColorMode, builtin_theme_json, create_theme};

const FULL_HEIGHT: usize = 55;

#[test]
fn interactive_splash_full_logo_centered_when_it_fits() {
    let lines = splash_lines(120, Some(80));
    assert_eq!(lines.len(), FULL_HEIGHT);
    assert_eq!(ULTRON_LOGO.split('\n').count(), FULL_HEIGHT);
    assert!(lines.iter().all(|l| l.chars().count() <= 120));
    assert!(lines[0].starts_with(&format!("{}$", " ".repeat(10))));
}

#[test]
fn interactive_splash_half_logo_in_normal_terminal_none_in_tiny() {
    let half = splash_lines(80, Some(45));
    assert_eq!(half.len(), halve_logo(ULTRON_LOGO).len());
    assert!(half.len() < FULL_HEIGHT);
    assert!(half.iter().all(|l| l.chars().count() <= 80));
    assert!(splash_lines(40, Some(45)).is_empty());
    assert!(splash_lines(80, Some(20)).is_empty());
}

/// Row thresholds recorded from TS `splashLines(100, rows)`: 30 -> 0, 41 -> 0, 42 -> 28, 68 -> 28, 69 -> 55 lines.
#[test]
fn interactive_splash_needs_rows_for_the_ui_below() {
    for (rows, want) in [(30, 0), (41, 0), (42, 28), (68, 28), (69, 55)] {
        assert_eq!(splash_lines(100, Some(rows)).len(), want, "{rows} rows");
    }
}

#[test]
fn interactive_splash_halving_keeps_logo_characters() {
    assert_eq!(halve_logo("$$$d\n$$$$"), ["$d"]);
    assert_eq!(halve_logo("$$\n$$"), ["$"]);
}

#[test]
fn interactive_splash_two_tone_colouring() {
    assert_eq!(
        color_logo_line("  $$dd$", &|l| format!("[{l}]"), &|b| format!("({b})")),
        "  ($$)[dd]($)"
    );
}

#[test]
fn interactive_prompt_history_merge_dedupes_in_group_order() {
    let merged = merge_prompt_history(
        &[
            vec!["b".into(), " a ".into(), "".into()],
            vec!["a".into(), "c".into(), "b".into()],
        ],
        500,
    );
    assert_eq!(merged, ["b", "a", "c"]);
    assert_eq!(merge_prompt_history(&[vec!["x".into(), "y".into()]], 1), ["x"]);
}

#[test]
fn interactive_prompt_history_current_project_first_and_skips_repeats() {
    let dir = tempfile_dir();
    let path = dir.join("prompt-history.jsonl");
    std::fs::write(
        &path,
        "{\"text\":\"other\",\"cwd\":\"/elsewhere\",\"timestamp\":3}\n{\"text\":\"mine old\",\"cwd\":\"/p\",\"timestamp\":1}\nnot json\n{\"text\":\"mine new\",\"cwd\":\"/p\",\"timestamp\":2}",
    )
    .unwrap();
    let mut store = PromptHistoryStore::new(Some(path.clone()), "/p");
    assert_eq!(store.load(), ["mine new", "mine old", "other"]);
    assert_eq!(store.append("  mine new  ", 4), None);
    assert_eq!(store.append("fresh", 5).as_deref(), Some("fresh"));
    assert_eq!(store.history(&[]), ["fresh", "mine new", "mine old", "other"]);
    // The append is written by a background thread; wait for it.
    for _ in 0..100 {
        if std::fs::read_to_string(&path).unwrap().contains("fresh") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.ends_with("\n{\"text\":\"fresh\",\"cwd\":\"/p\",\"timestamp\":5}\n"),
        "{text:?}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

fn tempfile_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("interactive-test-{}-{}", std::process::id(), rand_suffix()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn rand_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

#[test]
fn interactive_context_usage_is_last_usage_plus_estimates() {
    let transcript = [
        json!({"role": "user", "content": "x".repeat(40)}),
        json!({"role": "assistant", "content": [], "stopReason": "stop", "usage": {"input": 100, "output": 20, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 0}}),
        json!({"role": "user", "content": [{"type": "text", "text": "y".repeat(9)}]}),
    ];
    // 120 from the usage, ceil(9 / 4) = 3 after it.
    assert_eq!(context_usage(&transcript, 1000), Some((123, 12.3)));
    // Without usage everything is estimated: ceil(40 / 4) + 0 + 3.
    assert_eq!(context_usage(&transcript[..1], 1000).map(|c| c.0), Some(10));
    assert_eq!(context_usage(&transcript, 0), None);
}

#[derive(Default)]
struct Calls(RefCell<Vec<String>>);

struct Recorder(Rc<Calls>);

impl LaneClient for Recorder {
    fn prompt(&self, text: &str) {
        self.0.0.borrow_mut().push(format!("prompt {text}"));
    }
    fn steer(&self, text: &str) -> String {
        self.0.0.borrow_mut().push(format!("steer {text}"));
        "s1".into()
    }
    fn follow_up(&self, text: &str) -> String {
        self.0.0.borrow_mut().push(format!("follow_up {text}"));
        "f1".into()
    }
    fn cancel_queued(&self, id: &str) {
        self.0.0.borrow_mut().push(format!("cancel {id}"));
    }
    fn abort(&self) {
        self.0.0.borrow_mut().push("abort".into());
    }
    fn new_session(&self) {
        self.0.0.borrow_mut().push("new".into());
    }
    fn resume(&self, path: &str) {
        self.0.0.borrow_mut().push(format!("resume {path}"));
    }
    fn list_sessions(&self) {
        self.0.0.borrow_mut().push("list".into());
    }
    fn goal(&self, args: &str) {
        self.0.0.borrow_mut().push(format!("goal {args}"));
    }
    fn set_name(&self, name: &str) {
        self.0.0.borrow_mut().push(format!("name {name}"));
    }
    fn select_model(&self, provider: &str, id: &str) {
        self.0.0.borrow_mut().push(format!("model {provider}/{id}"));
    }
}

fn model(id: &str) -> ModelInfo {
    ModelInfo {
        provider: "anthropic".into(),
        id: id.into(),
        name: id.into(),
        reasoning: true,
        context_window: 1000,
        thinking_levels: Vec::new(),
    }
}

fn app() -> (App, Rc<Calls>) {
    let theme = Rc::new(create_theme(&builtin_theme_json("dark").unwrap(), ColorMode::Truecolor, None).unwrap());
    let keys = || {
        KeybindingsManager::new(
            app_keybindings(Platform::new("linux", false)),
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
            models: vec![model("a"), model("b")],
            model: model("a"),
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
    app.render(80, 24)
        .iter()
        .map(|l| tui::utils::strip_terminal_sequences(l).trim_end().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn calls(c: &Calls) -> Vec<String> {
    c.0.borrow().clone()
}

#[test]
fn interactive_enter_prompts_when_idle_and_steers_while_running() {
    let (mut app, c) = app();
    typed(&mut app, "hello");
    app.handle_input("\r");
    assert_eq!(calls(&c), ["prompt hello"]);
    assert!(screen(&mut app).contains(" Running turn…"));
    app.apply(LaneEvent::OperationStarted("op1".into()));
    typed(&mut app, "more");
    app.handle_input("\r");
    assert_eq!(calls(&c), ["prompt hello", "steer more"]);
    assert!(screen(&mut app).contains(" Queued s1."));
    // Alt+Enter queues a follow-up.
    typed(&mut app, "later");
    app.handle_input("\x1b\r");
    assert_eq!(calls(&c).last().unwrap(), "follow_up later");
    app.apply(LaneEvent::OperationEnded { error: None });
    assert!(!screen(&mut app).contains("Queued"));
}

#[test]
fn interactive_escape_restores_queued_messages_then_aborts() {
    let (mut app, c) = app();
    app.apply(LaneEvent::OperationStarted("op1".into()));
    app.apply(LaneEvent::Queues(vec![
        interactive::lane::Queued {
            entry_id: "f1".into(),
            kind: interactive::lane::QueueKind::FollowUp,
            text: "second".into(),
            user: true,
        },
        interactive::lane::Queued {
            entry_id: "s1".into(),
            kind: interactive::lane::QueueKind::Steer,
            text: "first".into(),
            user: true,
        },
    ]));
    let s = screen(&mut app);
    assert!(
        s.contains(" Steering: first\n Follow-up: second\n ↳ Alt+Up to edit all queued messages"),
        "{s}"
    );
    app.handle_input("\x1b");
    assert_eq!(calls(&c), ["cancel s1", "cancel f1", "abort"]);
    let s = screen(&mut app);
    assert!(s.contains(" Aborting op1…"), "{s}");
    assert!(s.contains(" first\n\n second"), "{s}");
    // Idle, Escape sends nothing.
    app.apply(LaneEvent::OperationEnded { error: None });
    app.handle_input("\x1b");
    assert_eq!(calls(&c).len(), 3);
}

/// client-tui.ts `#restoreQueuedToEditor`: only queued `type: "message"` items with `role: "user"` return to the
/// editor (Esc and Alt+Up); a queued runtime event (a custom message) stays queued, shown, and is not cancelled. The
/// dequeue hint shows only while a user message is queued (`#syncQueues`).
#[test]
fn interactive_dequeue_leaves_queued_runtime_events_queued() {
    use interactive::lane::{QueueKind, Queued};
    let (mut app, c) = app();
    app.apply(LaneEvent::OperationStarted("op1".into()));
    let queued = |id: &str, kind, text: &str, user| Queued {
        entry_id: id.into(),
        kind,
        text: text.into(),
        user,
    };
    app.apply(LaneEvent::Queues(vec![
        queued("r1", QueueKind::FollowUp, "", false),
        queued("u1", QueueKind::FollowUp, "mine", true),
        queued("r2", QueueKind::Steer, "<ultron-wake>", false),
    ]));
    let s = screen(&mut app);
    assert!(
        s.contains(" Steering: <ultron-wake>\n Follow-up:\n Follow-up: mine\n ↳ Alt+Up to edit all queued messages"),
        "{s}"
    );
    app.handle_input("\x1b[1;3A");
    assert_eq!(calls(&c), ["cancel u1"]);
    let s = screen(&mut app);
    assert!(s.contains("Restored 1 queued message to editor"), "{s}");
    assert_eq!(app.editor_text(), "mine");
    // The runtime events stay pending, without the dequeue hint.
    assert!(s.contains(" Steering: <ultron-wake>\n Follow-up:\n"), "{s}");
    assert!(!s.contains("to edit all queued messages"), "{s}");
    // Alt+Up again: nothing of the user's is queued.
    app.handle_input("\x1b[1;3A");
    assert_eq!(calls(&c), ["cancel u1"]);
    assert!(screen(&mut app).contains("No queued messages to restore"));
    // Esc aborts without cancelling the runtime events.
    app.handle_input("\x1b");
    assert_eq!(calls(&c), ["cancel u1", "abort"]);
}

/// client-tui-chat.ts `#syncQueues` with `QUEUE_LABELS`: steering first, then the rest in queue order, a next-run
/// message labelled "Next run".
#[test]
fn interactive_pending_messages_label_next_run_like_typescript() {
    use interactive::lane::{QueueKind, Queued};
    let (mut app, _) = app();
    app.apply(LaneEvent::OperationStarted("op1".into()));
    let queued = |id: &str, kind, text: &str| Queued {
        entry_id: id.into(),
        kind,
        text: text.into(),
        user: true,
    };
    app.apply(LaneEvent::Queues(vec![
        queued("n1", QueueKind::NextRun, "next time"),
        queued("f1", QueueKind::FollowUp, "after"),
        queued("s1", QueueKind::Steer, "now"),
    ]));
    let s = screen(&mut app);
    assert!(
        s.contains(" Steering: now\n Next run: next time\n Follow-up: after\n ↳ Alt+Up to edit all queued messages"),
        "{s}"
    );
}

#[test]
fn interactive_ctrl_c_clears_then_exits_and_ctrl_d_exits_when_empty() {
    let (mut app, _) = app();
    typed(&mut app, "draft");
    app.handle_input("\x04");
    assert!(!app.exit, "Ctrl+D with text deletes, it does not exit");
    app.handle_input("\x03");
    assert!(!app.exit);
    assert!(!screen(&mut app).contains("draft"));
    app.handle_input("\x03");
    assert!(app.exit, "a second Ctrl+C within 500 ms exits");
    let (mut app, _) = self::app();
    app.handle_input("\x03");
    std::thread::sleep(std::time::Duration::from_millis(600));
    app.handle_input("\x03");
    assert!(!app.exit, "presses 600 ms apart only clear");
    app.handle_input("\x04");
    assert!(app.exit);
}

#[test]
fn interactive_slash_commands() {
    let (mut app, c) = app();
    typed(&mut app, "/name");
    app.handle_input("\r");
    assert!(screen(&mut app).contains(" Usage: /name <name>"));
    typed(&mut app, "/name  Big refactor ");
    app.handle_input("\r");
    assert_eq!(calls(&c), ["name Big refactor"]);
    let s = screen(&mut app);
    assert!(s.contains(" Session name set: Big refactor"), "{s}");
    assert!(s.contains("/p • Big refactor"), "{s}");
    typed(&mut app, "/model b");
    // The first Enter takes the argument completion, the second submits.
    app.handle_input("\r");
    app.handle_input("\r");
    assert_eq!(calls(&c).last().unwrap(), "model anthropic/b");
    assert!(screen(&mut app).contains(" Selected anthropic/b."));
    typed(&mut app, "/model");
    app.handle_input("\r");
    let s = screen(&mut app);
    assert!(s.contains(" Select model:") && s.contains("→ b (selected)"), "{s}");
    app.handle_input("\x1b[A");
    app.handle_input("\r");
    assert_eq!(calls(&c).last().unwrap(), "model anthropic/a");
    typed(&mut app, "/new");
    app.handle_input("\r");
    typed(&mut app, "/resume");
    app.handle_input("\r");
    assert_eq!(calls(&c)[calls(&c).len() - 2..], ["new", "list"]);
    typed(&mut app, "/goal");
    app.handle_input("\r");
    assert_eq!(calls(&c).last().unwrap(), "goal ");
    typed(&mut app, "/goals");
    app.handle_input("\r");
    // `/goals` is an inspection command now (its request is asserted in tests/commands.rs).
    let s = screen(&mut app);
    assert!(
        !s.contains("/goals is not available") && !s.contains("Unknown slash command: /goals"),
        "{s}"
    );
    typed(&mut app, "/bogus");
    app.handle_input("\r");
    assert!(screen(&mut app).contains(" Unknown slash command: /bogus"));
    typed(&mut app, "/quit");
    app.handle_input("\r");
    assert!(app.exit);
}

#[test]
fn interactive_lane_name_event_shows_in_footer() {
    let (mut app, _) = app();
    app.apply(LaneEvent::Name(Some("Auto title".into())));
    assert!(screen(&mut app).lines().any(|l| l == "/p • Auto title"));
    assert_eq!(app.session_name(), Some("Auto title"));
}
