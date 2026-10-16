//! The startup notices in the running app (interactive-mode.ts `init`): a session that opens without messages asks
//! for the changelog (a resumed one does not), notices that arrive before the session opened are shown with it,
//! the subscription-auth check runs for an `anthropic` model at open and on every model change until it warned,
//! and the warning shows once.

use std::cell::RefCell;
use std::rc::Rc;

use interactive::app::{App, AppOptions};
use interactive::history::PromptHistoryStore;
use interactive::lane::{LaneClient, LaneEvent, ModelInfo, SessionOpened, StartupNotice};
use tui::keybindings::{KeybindingsManager, Platform};
use tui::qol_keys::native_app_keybindings;
use tui::theme::{ColorMode, builtin_theme_json, create_theme};

#[derive(Default)]
struct Calls {
    changelog: usize,
    auth_checks: Vec<String>,
}

struct Lane(Rc<RefCell<Calls>>);

impl LaneClient for Lane {
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
    fn startup_changelog(&self) {
        self.0.borrow_mut().changelog += 1;
    }
    fn subscription_auth_warning(&self, provider: &str) {
        self.0.borrow_mut().auth_checks.push(provider.to_string());
    }
}

fn model(provider: &str) -> ModelInfo {
    ModelInfo {
        provider: provider.into(),
        id: "m".into(),
        name: "M".into(),
        reasoning: true,
        context_window: 1000,
        thinking_levels: Vec::new(),
    }
}

fn app(provider: &str) -> (App, Rc<RefCell<Calls>>) {
    let keys = || {
        KeybindingsManager::new(
            native_app_keybindings(Platform::new("linux", false)),
            Vec::new(),
            Default::default(),
        )
    };
    let calls = Rc::new(RefCell::new(Calls::default()));
    let theme = Rc::new(create_theme(&builtin_theme_json("dark").unwrap(), ColorMode::Truecolor, None).unwrap());
    let app = App::new(
        AppOptions {
            theme,
            keys: keys(),
            editor_keys: keys(),
            version: "1.2.3".into(),
            cwd: "/p".into(),
            home: None,
            server_id: "server".into(),
            models: vec![model(provider)],
            model: model(provider),
            thinking_level: "medium".into(),
            splash: false,
            history: PromptHistoryStore::new(None, "/p"),
            fd_path: None,
            highlight: Rc::new(|code: &str| code.split('\n').map(str::to_string).collect()),
            qol: interactive::qol::QolSettings::parity(),
        },
        Box::new(Lane(calls.clone())),
    );
    (app, calls)
}

fn opened(messages: Vec<serde_json::Value>) -> LaneEvent {
    LaneEvent::Opened(SessionOpened {
        session_id: "sess".into(),
        session_file: None,
        name: None,
        messages,
    })
}

fn screen(app: &mut App) -> String {
    app.render(100, 60)
        .iter()
        .map(|l| tui::utils::strip_terminal_sequences(l))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn interactive_startup_asks_for_the_changelog_only_for_a_session_without_messages() {
    let (mut fresh, calls) = app("openai");
    fresh.apply(opened(Vec::new()));
    assert_eq!(calls.borrow().changelog, 1);
    // Opening another session later (a switch) does not ask again.
    fresh.apply(opened(Vec::new()));
    assert_eq!(calls.borrow().changelog, 1);
    let (mut resumed, calls) = app("openai");
    resumed.apply(opened(vec![
        serde_json::json!({"role": "user", "content": "hi", "timestamp": 1}),
    ]));
    assert_eq!(calls.borrow().changelog, 0);
}

#[test]
fn interactive_startup_notices_show_once_the_session_is_open() {
    let (mut app, _) = app("openai");
    app.apply(LaneEvent::StartupNotice(StartupNotice::NewVersion("9.9.9".into())));
    app.apply(LaneEvent::StartupNotice(StartupNotice::WhatsNew {
        markdown: "## [1.2.3]\n\n- New thing".into(),
        collapse: false,
        version: "1.2.3".into(),
    }));
    assert!(!screen(&mut app).contains("Update Available"));
    app.apply(opened(Vec::new()));
    let text = screen(&mut app);
    assert!(text.contains("Update Available"), "{text}");
    assert!(
        text.contains("New version 9.9.9 is available. Run ultron update"),
        "{text}"
    );
    assert!(text.contains("What's New"), "{text}");
    assert!(text.contains("New thing"), "{text}");
    app.apply(LaneEvent::StartupNotice(StartupNotice::WhatsNew {
        markdown: "## [2.0.0]\n\n- Later".into(),
        collapse: true,
        version: "2.0.0".into(),
    }));
    let text = screen(&mut app);
    assert!(
        text.contains("Updated to v2.0.0. Use /changelog to view full changelog."),
        "{text}"
    );
}

#[test]
fn interactive_subscription_auth_warning_is_checked_for_anthropic_and_shown_once() {
    let (mut app, calls) = app("anthropic");
    app.apply(opened(Vec::new()));
    assert_eq!(calls.borrow().auth_checks, vec!["anthropic".to_string()]);
    app.apply(LaneEvent::Model(model("openai")));
    assert_eq!(
        calls.borrow().auth_checks.len(),
        1,
        "only an anthropic model is checked"
    );
    app.apply(LaneEvent::Model(model("anthropic")));
    assert_eq!(calls.borrow().auth_checks.len(), 2);
    app.apply(LaneEvent::StartupNotice(StartupNotice::SubscriptionAuth));
    app.apply(LaneEvent::StartupNotice(StartupNotice::SubscriptionAuth));
    let text = screen(&mut app);
    assert_eq!(
        text.matches("Warning: Anthropic subscription auth is active.").count(),
        1,
        "{text}"
    );
    // Once it warned, no more checks.
    app.apply(LaneEvent::Model(model("anthropic")));
    assert_eq!(calls.borrow().auth_checks.len(), 2);
}
