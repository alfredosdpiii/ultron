//! The agent hub's actions reach the session: opening an agent's transcript reads it (`agentTranscript`) and shows
//! it, submitting in the transcript steers the agent (`steerAgent`) and reads the transcript again, `r` on a parked
//! (interrupted) agent revives it (`reviveAgent`) and lists the agents again, steering a parked agent revives it
//! first, and what the session answered shows as the hub's notice.

use std::cell::RefCell;
use std::rc::Rc;

use interactive::agent_hub::{AgentKind, AgentStatus, AgentView};
use interactive::app::{App, AppOptions};
use interactive::history::PromptHistoryStore;
use interactive::lane::{LaneClient, LaneEvent, ModelInfo, SessionOpened};
use serde_json::json;
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
    fn agents(&self) {
        self.push("agents".into());
    }
    fn kill_agent(&self, id: &str) {
        self.push(format!("kill {id}"));
    }
    fn steer_agent(&self, id: &str, text: &str) {
        self.push(format!("steer {id} {text}"));
    }
    fn revive_agent(&self, id: &str) {
        self.push(format!("revive {id}"));
    }
    fn agent_transcript(&self, id: &str) {
        self.push(format!("transcript {id}"));
    }
}

fn model() -> ModelInfo {
    ModelInfo {
        provider: "anthropic".into(),
        id: "a".into(),
        name: "a".into(),
        reasoning: true,
        context_window: 200_000,
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

fn agent(id: &str, status: AgentStatus, last: i64) -> AgentView {
    AgentView {
        id: id.into(),
        kind: AgentKind::Sub,
        parent_id: None,
        status,
        unread: 0,
        task: None,
        last_activity: last,
        model: None,
        thinking: None,
        metrics: None,
    }
}

fn screen(app: &mut App) -> String {
    app.render(100, 30)
        .iter()
        .map(|l| tui::utils::strip_terminal_sequences(l).trim_end().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Drain the calls made so far.
fn take(c: &Calls) -> Vec<String> {
    std::mem::take(&mut *c.0.borrow_mut())
}

#[test]
fn interactive_agent_hub_reads_steers_and_revives_through_the_session() {
    let (mut app, c) = app();
    app.handle_input("\x1ba");
    assert_eq!(take(&c), ["agents"]);
    app.apply(LaneEvent::Agents(vec![
        agent("scout", AgentStatus::Running, 50),
        agent("fixer", AgentStatus::Parked, 10),
    ]));
    assert!(screen(&mut app).contains("scout"));

    // Enter opens the running agent's transcript, read from the session.
    app.handle_input("\r");
    assert_eq!(take(&c), ["transcript scout"]);
    app.apply(LaneEvent::AgentTranscript {
        id: "scout".into(),
        messages: vec![
            json!({"role": "user", "content": "map the parser", "timestamp": 1}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "Reading parser.rs"}], "timestamp": 2}),
        ],
    });
    assert!(screen(&mut app).contains("Reading parser.rs"), "{}", screen(&mut app));

    // Submitting steers it, then reads the transcript again.
    for ch in "focus on src".chars() {
        app.handle_input(&ch.to_string());
    }
    app.handle_input("\r");
    assert_eq!(take(&c), ["steer scout focus on src", "transcript scout"]);

    // Back in the table, `r` on the parked agent revives it and lists the agents again.
    app.handle_input("\x1b");
    app.handle_input("j");
    app.handle_input("r");
    assert_eq!(take(&c), ["revive fixer", "agents"]);
    app.apply(LaneEvent::AgentNotice("Revived \"fixer\" as \"t9\".".into()));
    assert!(
        screen(&mut app).contains("Revived \"fixer\" as \"t9\"."),
        "{}",
        screen(&mut app)
    );

    // Steering a parked agent revives it first.
    app.handle_input("\r");
    assert_eq!(take(&c), ["transcript fixer"]);
    for ch in "go on".chars() {
        app.handle_input(&ch.to_string());
    }
    app.handle_input("\r");
    assert_eq!(take(&c), ["revive fixer", "agents"]);
}
