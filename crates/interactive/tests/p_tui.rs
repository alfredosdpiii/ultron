//! The client TUI's live settings, dialogs and RLM pane against a recording `LaneClient`, with literal expectations
//! from experimental/client-tui.ts (`#applySettingLocally`, `#handleEscape`, `#serveExtensionUI`, `#showTextInput`,
//! `#showTextEditor`, `#toggleRlmPane`, `#autoOpenRlmPane`, `#renderWaveSummary`, `#suspend`), client-tui-auth.ts
//! (multi-field API-key logins; flows replayed from fixtures/api-key-login.json, `node
//! crates/interactive/fixtures/generate-api-key-login.mjs`), client-tui-pi-commands.ts (`/share`'s loader),
//! client-tui-sessions.ts with session-selector.ts (Tab's scope) and the theme controller.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use interactive::api_key_login::{ApiKeyLogin, Step};
use interactive::app::{App, AppOptions};
use interactive::history::PromptHistoryStore;
use interactive::lane::{LaneClient, LaneEvent, LoginEvent, LoginPrompt, ModelInfo, SessionListing, SessionOpened};
use interactive::qol::QolSettings;
use interactive::run::{UiEvent, paint_loop};
use serde_json::{Value, json};
use tui::clipboard::{Clipboard, ClipboardService};
use tui::keybindings::{KeybindingsManager, Platform};
use tui::qol_keys::native_app_keybindings;
use tui::theme::{ColorMode, Theme, builtin_theme_json, create_theme};

#[derive(Default)]
struct Calls(RefCell<Vec<String>>);

struct Recorder(Rc<Calls>);

impl Recorder {
    fn log(&self, entry: String) {
        self.0.0.borrow_mut().push(entry);
    }
}

impl LaneClient for Recorder {
    fn prompt(&self, text: &str) {
        self.log(format!("prompt {text}"));
    }
    fn steer(&self, _: &str) -> String {
        String::new()
    }
    fn follow_up(&self, _: &str) -> String {
        String::new()
    }
    fn cancel_queued(&self, _: &str) {}
    fn abort(&self) {
        self.log("abort".into());
    }
    fn new_session(&self) {}
    fn resume(&self, path: &str) {
        self.log(format!("resume {path}"));
    }
    fn list_sessions(&self) {
        self.log("list".into());
    }
    fn list_all_sessions(&self) {
        self.log("list_all".into());
    }
    fn list_pi_sessions(&self) {
        self.log("list_pi".into());
    }
    fn list_all_pi_sessions(&self) {
        self.log("list_all_pi".into());
    }
    fn set_name(&self, _: &str) {}
    fn select_model(&self, _: &str, _: &str) {}
    fn tree(&self) {
        self.log("tree".into());
    }
    fn fork_messages(&self) {
        self.log("fork_messages".into());
    }
    fn extension_ui_respond(&self, id: &str, answer: Value) {
        self.log(format!("respond {id} {answer}"));
    }
    fn save_setting(&self, key: &str, value: Value) {
        self.log(format!("setting {key}={value}"));
    }
    fn share(&self, _theme: Option<&str>) {
        self.log("share".into());
    }
    fn cancel_share(&self) {
        self.log("cancel_share".into());
    }
    fn save_credential(&self, provider: &str, credential: Value) {
        self.log(format!("credential {provider} {credential}"));
    }
    fn login_reply(&self, answer: Option<String>) {
        self.log(format!("login_reply {answer:?}"));
    }
    fn rlm_status(&self) {
        self.log("rlm_status".into());
    }
    fn rlm_inspect(&self, tag: &str, request: &str, payload: Value) {
        self.log(format!("inspect {tag} {request} {payload}"));
    }
    fn read_settings(&self) {
        self.log("read_settings".into());
    }
}

fn keys() -> KeybindingsManager {
    KeybindingsManager::new(
        native_app_keybindings(Platform::new("linux", false)),
        Vec::new(),
        Default::default(),
    )
}

fn dark() -> Theme {
    create_theme(&builtin_theme_json("dark").unwrap(), ColorMode::Truecolor, None).unwrap()
}

fn light() -> Theme {
    create_theme(&builtin_theme_json("light").unwrap(), ColorMode::Truecolor, None).unwrap()
}

/// An app on an opened session, its theme, its calls and a clock the test moves.
struct Harness {
    app: App,
    theme: Rc<Theme>,
    calls: Rc<Calls>,
    clock: Rc<Cell<Instant>>,
}

fn harness_with(qol: QolSettings, splash: bool) -> Harness {
    let theme = Rc::new(dark());
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
            theme: Rc::clone(&theme),
            keys: keys(),
            editor_keys: keys(),
            version: "1.2.3".into(),
            cwd: "/p".into(),
            home: None,
            server_id: "server".into(),
            models: vec![model.clone()],
            model,
            thinking_level: "medium".into(),
            splash,
            history: PromptHistoryStore::new(None, "/p"),
            fd_path: None,
            highlight: Rc::new(|code: &str| code.split('\n').map(str::to_string).collect()),
            qol,
        },
        Box::new(Recorder(calls.clone())),
    );
    let clock = Rc::new(Cell::new(Instant::now()));
    let now = Rc::clone(&clock);
    app.set_clock(Box::new(move || now.get()));
    app.apply(LaneEvent::Opened(SessionOpened {
        session_id: "sess".into(),
        session_file: Some("/s/cur".into()),
        name: None,
        messages: Vec::new(),
    }));
    Harness {
        app,
        theme,
        calls,
        clock,
    }
}

fn harness() -> Harness {
    harness_with(QolSettings::parity(), false)
}

impl Harness {
    fn calls(&self) -> Vec<String> {
        self.calls.0.borrow().clone()
    }
    fn clear(&self) {
        self.calls.0.borrow_mut().clear();
    }
    fn advance(&self, ms: u64) {
        self.clock.set(self.clock.get() + Duration::from_millis(ms));
    }
    fn screen_at(&mut self, width: usize, height: usize) -> String {
        self.app
            .render(width, height)
            .iter()
            .map(|l| tui::utils::strip_terminal_sequences(l).trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }
    fn screen(&mut self) -> String {
        self.screen_at(80, 30)
    }
    fn typed(&mut self, text: &str) {
        for c in text.chars() {
            self.app.handle_input(&c.to_string());
        }
    }
    fn submit(&mut self, text: &str) {
        self.typed(text);
        self.app.handle_input("\r");
    }
}

const ESC: &str = "\x1b";
const DOWN: &str = "\x1b[B";

// ---- multi-field API-key logins ----

fn prompt_json(prompt: &LoginPrompt) -> Value {
    match prompt {
        LoginPrompt::Select { message, options } => json!({
            "type": "select",
            "message": message,
            "options": options.iter().map(|(id, label)| json!({"id": id, "label": label})).collect::<Vec<_>>(),
        }),
        LoginPrompt::Text { message, secret, .. } => {
            json!({"type": if *secret { "secret" } else { "text" }, "message": message})
        }
        LoginPrompt::ManualCode { message } => json!({"type": "manual_code", "message": message}),
    }
}

fn event_json(event: &LoginEvent) -> Value {
    match event {
        LoginEvent::Info { message, links } => json!({
            "type": "info",
            "message": message,
            "links": links.iter().map(|(label, url)| json!({"label": label, "url": url})).collect::<Vec<_>>(),
        }),
        other => panic!("unexpected event {other:?}"),
    }
}

#[test]
fn interactive_p_tui_api_key_login_flows_match_typescript() {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/api-key-login.json")).unwrap();
    let cases = fixture["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 14);
    for case in cases {
        let provider = case["provider"].as_str().unwrap();
        let (mut login, mut steps) = ApiKeyLogin::start(provider).unwrap();
        let mut seen = Vec::new();
        let mut outcome = None;
        let mut answers = case["answers"].as_array().unwrap().iter();
        loop {
            let mut prompted = false;
            for step in steps.drain(..) {
                match step {
                    Step::Prompt(p) => {
                        seen.push(json!({"prompt": prompt_json(&p)}));
                        prompted = true;
                    }
                    Step::Event(e) => seen.push(json!({"event": event_json(&e)})),
                    Step::Done(credential) => outcome = Some(json!({"credential": credential})),
                    Step::Failed(error) => outcome = Some(json!({"error": error})),
                }
            }
            if !prompted {
                break;
            }
            match answers.next().and_then(Value::as_str) {
                Some(answer) => steps = login.answer(answer.to_string()),
                // The user cancelled this prompt: TypeScript's flow rejects with `Login cancelled`.
                None => {
                    outcome = Some(json!({"error": "Login cancelled"}));
                    break;
                }
            }
        }
        assert_eq!(Value::Array(seen), case["steps"], "{provider} {}", case["answers"]);
        let want = match case.get("credential") {
            Some(credential) => json!({"credential": credential}),
            None => json!({"error": case["error"]}),
        };
        assert_eq!(outcome.unwrap(), want, "{provider} {}", case["answers"]);
    }
    assert!(ApiKeyLogin::start("openai").is_none());
}

fn bedrock_option() -> interactive::selectors::AuthProvider {
    interactive::selectors::AuthProvider {
        id: "amazon-bedrock".into(),
        name: "Amazon Bedrock".into(),
        auth_type: "api_key".into(),
        status: None,
        api_key_method: Some(("AWS credentials or bearer token".into(), true)),
        login_label: None,
        generic_login: false,
    }
}

#[test]
fn interactive_p_tui_login_runs_the_bedrock_flow_in_the_dialog_and_saves_the_credential() {
    let mut h = harness();
    h.app.apply(LaneEvent::LoginProviders {
        providers: vec![bedrock_option()],
        query: "amazon-bedrock".into(),
    });
    let s = h.screen();
    assert!(
        s.contains("Select Amazon Bedrock authentication method:") && s.contains("→ Bearer token"),
        "{s}"
    );
    // AWS profile: the notice, then the profile prompt.
    h.app.handle_input(DOWN);
    h.app.handle_input("\r");
    let s = h.screen();
    assert!(
        s.contains("Amazon Bedrock supports AWS profiles") && s.contains("Enter AWS profile name"),
        "{s}"
    );
    h.typed("work");
    h.app.handle_input("\r");
    assert_eq!(
        h.calls(),
        ["credential amazon-bedrock {\"type\":\"api_key\",\"env\":{\"AWS_PROFILE\":\"work\"}}"]
    );
    assert!(!h.screen().contains("Login to Amazon Bedrock"));
    // Esc closes the dialog silently, nothing saved and no subscription login answered.
    h.clear();
    h.app.apply(LaneEvent::LoginProviders {
        providers: vec![bedrock_option()],
        query: "amazon-bedrock".into(),
    });
    h.app.handle_input("\r");
    h.app.handle_input(ESC);
    assert!(h.calls().is_empty(), "{:?}", h.calls());
    assert!(!h.screen().contains("Enter Amazon Bedrock bearer token"));
}

// ---- extension UI ----

fn ui(h: &mut Harness, request: Value) {
    h.app.apply(LaneEvent::ExtensionUi(request));
}

#[test]
fn interactive_p_tui_extension_input_counts_down_and_cancels() {
    let mut h = harness();
    ui(
        &mut h,
        json!({"method": "input", "id": "i1", "title": "Name?", "placeholder": "hint", "timeout": 2500}),
    );
    let s = h.screen();
    assert!(
        s.contains("Name? (3s)") && s.contains("enter submit  escape/ctrl+c cancel"),
        "{s}"
    );
    // The placeholder is not shown (ExtensionInputComponent ignores it).
    assert!(!s.contains("hint"), "{s}");
    assert!(h.app.animating());
    h.advance(1000);
    assert!(h.screen().contains("Name? (2s)"));
    h.advance(1999);
    h.app.tick();
    assert!(h.calls().is_empty());
    h.advance(1);
    h.app.tick();
    assert_eq!(h.calls(), ["respond i1 {\"cancelled\":true}"]);
    // Without a timeout: no countdown; Enter submits.
    ui(&mut h, json!({"method": "input", "id": "i2", "title": "Plain"}));
    assert!(!h.app.animating());
    h.typed("ok");
    h.app.handle_input("\r");
    assert_eq!(h.calls().last().unwrap(), "respond i2 {\"value\":\"ok\"}");
}

// ---- Esc, quiet startup, suspend ----

#[test]
fn interactive_p_tui_double_escape_action_tree_fork_none() {
    for (action, want) in [("tree", Some("tree")), ("fork", Some("fork_messages")), ("none", None)] {
        let mut h = harness();
        h.app
            .apply_startup_settings(&json!({"doubleEscapeAction": action}), &|_| None);
        h.app.handle_input(ESC);
        h.advance(499);
        h.app.handle_input(ESC);
        assert_eq!(h.calls(), want.into_iter().collect::<Vec<_>>(), "{action}");
        // Too slow: the second Esc starts a new window.
        h.clear();
        h.app.handle_input(ESC);
        h.advance(500);
        h.app.handle_input(ESC);
        assert!(h.calls().is_empty(), "{action}");
    }
    // Text in the editor: Esc does nothing.
    let mut h = harness();
    h.typed("draft");
    h.app.handle_input(ESC);
    h.app.handle_input(ESC);
    assert!(h.calls().is_empty());
}

#[test]
fn interactive_p_tui_quiet_startup_hides_the_splash_live() {
    let splash = |h: &mut Harness| h.screen_at(120, 80).contains("$$$$");
    let mut h = harness_with(QolSettings::parity(), true);
    h.app.apply_startup_settings(&json!({}), &|_| None);
    assert!(splash(&mut h));
    h.app.apply_startup_settings(&json!({"quietStartup": true}), &|_| None);
    assert!(!splash(&mut h));
    // ULTRON_SPLASH=off already turned it off: quietStartup false does not bring it back.
    let mut h = harness_with(QolSettings::parity(), false);
    h.app.apply_startup_settings(&json!({"quietStartup": false}), &|_| None);
    assert!(!splash(&mut h));
}

#[test]
fn interactive_p_tui_ctrl_z_suspends_through_the_injected_hook() {
    let mut h = harness();
    let ran = Rc::new(Cell::new(0));
    let count = Rc::clone(&ran);
    h.app.set_suspend_hook(Box::new(move || {
        count.set(count.get() + 1);
        Ok(())
    }));
    let (tx, rx) = mpsc::channel();
    tx.send(UiEvent::Input("\x1a".into())).unwrap();
    tx.send(UiEvent::InputClosed).unwrap();
    let mut out = String::new();
    paint_loop(
        &mut h.app,
        &rx,
        &mut |data| {
            out.push_str(data);
            Ok(())
        },
        &|| (80, 24),
        None,
    )
    .unwrap();
    assert_eq!(ran.get(), 1);
    // The terminal was given back (bracketed paste off, cursor shown, alternate screen left) and taken again.
    let left = out.find("\x1b[?2004l\x1b[?25h\x1b[?1049l").expect("left the screen");
    assert!(out[left..].contains("\x1b[?2004h"), "{out:?}");
    // A failing hook says why.
    h.app.set_suspend_hook(Box::new(|| {
        Err("Suspend to background is not supported on Windows".into())
    }));
    h.app.handle_input("\x1a");
    assert!(h.app.take_suspend_request());
    h.app.suspend();
    assert!(h.screen().contains("Suspend to background is not supported on Windows"));
}

// ---- clipboard copies ----

struct CopyClipboard(Result<Option<String>, String>);

impl Clipboard for CopyClipboard {
    fn get_text(&mut self) -> Result<String, String> {
        Err("none".into())
    }
    fn set_text(&mut self, _: &str) -> Result<(), String> {
        Ok(())
    }
    fn get_image_png(&mut self) -> Result<Vec<u8>, String> {
        Err("none".into())
    }
    fn copy_checked(&mut self, _: &str) -> Result<Option<String>, String> {
        self.0.clone()
    }
}

fn wait_status(h: &mut Harness, needle: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        h.app.poll_services();
        let s = h.screen();
        if s.contains(needle) {
            return s;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("never showed {needle}");
}

#[test]
fn interactive_p_tui_copy_uses_the_clipboard_before_osc52() {
    let answer =
        json!({"role": "assistant", "stopReason": "stop", "content": [{"type": "text", "text": "the answer"}]});
    for (result, status, osc) in [
        (Ok(None), "Copied last agent message to clipboard", None),
        (
            Ok(Some("\x1b]52;c;eA==\x07".to_string())),
            "Copied last agent message to clipboard",
            Some("\x1b]52;c;eA==\x07"),
        ),
        (
            Err("Clipboard unavailable: install `xclip` or `xsel`, or check X11 access".to_string()),
            "Error: Clipboard unavailable: install `xclip` or `xsel`",
            None,
        ),
    ] {
        let mut h = harness();
        h.app.apply(LaneEvent::Message(answer.clone()));
        h.app.set_clipboard(ClipboardService::spawn(CopyClipboard(result)));
        h.app.terminal_writes.clear();
        h.submit("/copy");
        wait_status(&mut h, status);
        assert_eq!(h.app.terminal_writes, osc.into_iter().collect::<Vec<_>>());
    }
}

// ---- /share ----

#[test]
fn interactive_p_tui_share_loader_cancels_with_escape() {
    let mut h = harness();
    h.submit("/share");
    assert_eq!(h.calls(), ["share"]);
    h.app.apply(LaneEvent::ShareLoader(true));
    let s = h.screen();
    assert!(
        s.contains("Creating gist...") && s.contains("escape/ctrl+c cancel"),
        "{s}"
    );
    assert!(h.app.animating());
    h.app.handle_input(ESC);
    assert_eq!(h.calls().last().unwrap(), "cancel_share");
    let s = h.screen();
    assert!(s.contains("Share cancelled") && !s.contains("Creating gist..."), "{s}");
    // The upload ending closes the loader.
    h.app.apply(LaneEvent::ShareLoader(true));
    h.app.apply(LaneEvent::ShareLoader(false));
    assert!(!h.screen().contains("Creating gist..."));
}

// ---- session scope ----

fn listing(path: &str, first: &str, cwd: &str) -> SessionListing {
    SessionListing {
        path: path.into(),
        id: path.into(),
        name: None,
        modified: 1,
        message_count: 2,
        first_message: first.into(),
        all_messages_text: first.into(),
        cwd: cwd.into(),
    }
}

#[test]
fn interactive_p_tui_resume_tab_lists_every_project() {
    let mut h = harness();
    h.submit("/resume");
    h.app
        .apply(LaneEvent::Sessions(vec![listing("/s/a", "here ask", "/p")]));
    h.app.handle_input("\t");
    assert_eq!(h.calls(), ["list", "list_all"]);
    let s = h.screen_at(120, 30);
    assert!(
        s.contains("Resume Session (All)") && s.contains("○ Current Folder | Loading ..."),
        "{s}"
    );
    assert!(s.contains("No sessions found"), "{s}");
    h.app.apply(LaneEvent::AllSessions(vec![
        listing("/s/a", "here ask", "/p"),
        listing("/s/b", "elsewhere ask", "/other/proj"),
    ]));
    let s = h.screen_at(120, 30);
    assert!(s.contains("○ Current Folder | ◉ All") && s.contains("tab scope"), "{s}");
    assert!(s.contains("elsewhere ask") && s.contains("/other/proj 2"), "{s}");
    // Back to this folder (already listed: no new request), then All again (listed once).
    h.app.handle_input("\t");
    let s = h.screen_at(120, 30);
    assert!(
        s.contains("◉ Current Folder | ○ All") && !s.contains("elsewhere ask"),
        "{s}"
    );
    h.app.handle_input("\t");
    assert_eq!(h.calls(), ["list", "list_all"]);
    // /import's picker lists Pi's sessions of every project the same way, without the rename hint.
    h.app.handle_input(ESC);
    h.submit("/import");
    h.app.apply(LaneEvent::PiSessions(Vec::new()));
    let s = h.screen_at(120, 30);
    assert!(
        s.contains("No sessions in current folder. Press Tab to view all.") && !s.contains("rename"),
        "{s}"
    );
    h.app.handle_input("\t");
    assert_eq!(h.calls().last().unwrap(), "list_all_pi");
}

// ---- RLM pane ----

fn running_child() -> Value {
    json!({"subagents": [{"id": "ultron-task-a", "definition": "rlm-child@1", "state": "running", "input": {"preview": "{\"prompt\": \"audit the parser\"}"}}]})
}

#[test]
fn interactive_p_tui_rlm_pane_opens_beside_the_chat_and_moves_the_focus() {
    let mut h = harness();
    h.screen_at(160, 40);
    h.app.handle_input("\x1bw");
    assert!(h.calls().contains(&"rlm_status".to_string()), "{:?}", h.calls());
    let frame = h.app.render(160, 40);
    // Chat 95 columns, the border, the pane 64 columns (rlmPaneWidth(160)).
    assert_eq!(interactive::app::rlm_pane_width(160), 64);
    let border = tui::utils::strip_terminal_sequences(&frame[0]);
    assert_eq!(border.chars().nth(95), Some('│'), "{border:?}");
    // Focused, keys go to the pane: typing does not reach the editor; Alt+W gives the focus back.
    h.typed("x");
    assert_eq!(h.app.editor_text(), "");
    h.app.handle_input("\x1bw");
    h.typed("x");
    assert_eq!(h.app.editor_text(), "x");
    // The pane's close key (focused) closes it.
    h.app.handle_input("\x1bw");
    h.app.handle_input("q");
    let frame = h.app.render(160, 40);
    assert_ne!(
        tui::utils::strip_terminal_sequences(&frame[0]).chars().nth(95),
        Some('│')
    );
}

#[test]
fn interactive_p_tui_rlm_pane_falls_back_to_the_graph_when_narrow() {
    let mut h = harness();
    h.screen_at(100, 40);
    h.app.handle_input("\x1bw");
    let s = h.screen_at(100, 40);
    // The full-screen graph's hints (`focusHints`).
    assert!(s.contains("back"), "{s}");
    h.app.handle_input(ESC);
    assert!(!h.screen_at(100, 40).contains("back"));
}

#[test]
fn interactive_p_tui_rlm_pane_auto_opens_once_per_turn_without_focus() {
    let mut h = harness();
    h.screen_at(160, 40);
    h.app.apply(LaneEvent::OperationStarted("op1".into()));
    h.app.apply(LaneEvent::RlmStatus(running_child()));
    let frame = h.app.render(160, 40);
    assert_eq!(
        tui::utils::strip_terminal_sequences(&frame[0]).chars().nth(95),
        Some('│')
    );
    // Unfocused: typing stays in the editor.
    h.typed("y");
    assert_eq!(h.app.editor_text(), "y");
    // Closed by the user during the turn: the next poll does not open it again.
    h.app.handle_input("\x7f");
    h.app.handle_input("\x1bw");
    h.app.handle_input("q");
    h.app.apply(LaneEvent::RlmStatus(running_child()));
    let frame = h.app.render(160, 40);
    assert_ne!(
        tui::utils::strip_terminal_sequences(&frame[0]).chars().nth(95),
        Some('│')
    );
    // rlmPaneAutoOpen off (or ULTRON_RLM_PANE_AUTO=off): never by itself.
    let mut h = harness();
    h.app
        .apply_startup_settings(&json!({}), &|k| (k == "ULTRON_RLM_PANE_AUTO").then(|| "off".into()));
    h.screen_at(160, 40);
    h.app.apply(LaneEvent::RlmStatus(running_child()));
    let frame = h.app.render(160, 40);
    assert_ne!(
        tui::utils::strip_terminal_sequences(&frame[0]).chars().nth(95),
        Some('│')
    );
    // Too narrow: no auto-open (never the full-screen graph).
    let mut h = harness();
    h.screen_at(100, 40);
    h.app.apply(LaneEvent::RlmStatus(running_child()));
    assert!(!h.screen_at(100, 40).contains("back"));
}

#[test]
fn interactive_p_tui_wave_summary_shows_above_the_editor_while_children_run() {
    let mut h = harness();
    h.app.apply(LaneEvent::RlmStatus(running_child()));
    let s = h.screen_at(100, 40);
    assert!(s.contains("▶") && s.contains("running wave 1/1"), "{s}");
    // Hidden while the docked RLM panel shows.
    h.app.handle_input("\x12");
    assert!(!h.screen_at(100, 40).contains("running wave"));
}

#[test]
fn interactive_p_tui_rlm_polls_inspections_on_the_clock() {
    let mut h = harness();
    assert!(h.app.poll_rlm());
    assert_eq!(
        h.calls(),
        [
            "rlm_status",
            "inspect agents agents.status {\"graph\":true}",
            "inspect frames rlm.frames {\"limit\":200}",
            "inspect pool rlm.pool {}",
            "inspect context ctx.state {}",
            "inspect instances instances.list {}",
        ]
    );
    h.clear();
    h.advance(4999);
    assert!(!h.app.poll_rlm());
    h.advance(1);
    assert!(h.app.poll_rlm());
    // While an RLM view shows, every second.
    h.app.handle_input("\x12");
    h.clear();
    h.advance(1000);
    assert!(h.app.poll_rlm());
}

/// Older workers reject `agents.status {graph: true}` (client-tui.ts `#graphListing`): the TUI asks for the plain
/// listing at once and keeps asking for it; any other failure keeps the graph listing and shows as the error.
#[test]
fn interactive_p_tui_rlm_agents_status_falls_back_to_the_plain_listing() {
    let mut h = harness();
    h.app.handle_input("\x12");
    h.clear();
    h.app.apply(LaneEvent::RlmInspected {
        tag: "agents".into(),
        result: Err("Ultron task host is closed".into()),
    });
    assert!(h.calls().is_empty(), "{:?}", h.calls());
    h.app.apply(LaneEvent::RlmInspected {
        tag: "agents".into(),
        result: Err("Unknown payload field: graph".into()),
    });
    assert_eq!(h.calls(), ["inspect agents agents.status {}"]);
    h.clear();
    h.advance(5000);
    assert!(h.app.poll_rlm());
    assert!(
        h.calls().contains(&"inspect agents agents.status {}".to_string()),
        "{:?}",
        h.calls()
    );
    // Rejected again (the plain listing has no graph field to drop): no third request.
    h.clear();
    h.app.apply(LaneEvent::RlmInspected {
        tag: "agents".into(),
        result: Err("Unknown payload field: graph".into()),
    });
    assert!(h.calls().is_empty(), "{:?}", h.calls());
}

fn agents_status(tasks: Value) -> LaneEvent {
    LaneEvent::RlmInspected {
        tag: "agents".into(),
        result: Ok(json!({"tasks": tasks})),
    }
}

/// `progress.assess` per running task, at most eight (`RLM_MAX_ASSESSED`), fetched only while the docked panel or
/// the full-screen graph shows (client-tui.ts `#refreshRlm`); the tasks are `agents.status`'s and the subagents it
/// does not list.
#[test]
fn interactive_p_tui_rlm_assesses_running_tasks_only_while_a_view_shows() {
    let tasks: Vec<Value> = (0..10)
        .map(|i| json!({"id": format!("t{i}"), "definition": "review@1", "state": if i == 1 { "completed" } else { "running" }}))
        .collect();
    let mut h = harness();
    // Hidden (the wave summary is not a view that assesses).
    h.app.apply(agents_status(json!(tasks)));
    assert!(
        !h.calls().iter().any(|c| c.contains("progress.assess")),
        "{:?}",
        h.calls()
    );
    // The docked panel.
    h.app.handle_input("\x12");
    h.clear();
    h.app.apply(agents_status(json!(tasks)));
    let assessed: Vec<String> = h
        .calls()
        .into_iter()
        .filter(|c| c.contains("progress.assess"))
        .collect();
    let ids: Vec<String> = ["t0", "t2", "t3", "t4", "t5", "t6", "t7", "t8"]
        .iter()
        .map(|id| format!("inspect progress:2:{id} progress.assess {{\"task_id\":\"{id}\"}}"))
        .collect();
    assert_eq!(assessed, ids);
    // A subagent the status does not list is assessed too; one it lists is not counted twice.
    h.app.apply(LaneEvent::RlmStatus(json!({"subagents": [
        {"id": "t0", "definition": "rlm-child@1", "state": "running"},
        {"id": "child", "definition": "rlm-child@1", "state": "running"}
    ]})));
    h.clear();
    h.app.apply(agents_status(
        json!([{"id": "t0", "definition": "review@1", "state": "running"}]),
    ));
    assert_eq!(
        h.calls()
            .into_iter()
            .filter(|c| c.contains("progress.assess"))
            .collect::<Vec<_>>(),
        [
            "inspect progress:3:t0 progress.assess {\"task_id\":\"t0\"}",
            "inspect progress:3:child progress.assess {\"task_id\":\"child\"}",
        ]
    );
    // Closed again: nothing is assessed.
    h.app.handle_input("\x12");
    h.clear();
    h.app.apply(agents_status(json!(tasks)));
    assert!(
        !h.calls().iter().any(|c| c.contains("progress.assess")),
        "{:?}",
        h.calls()
    );
    // The full-screen graph assesses as well.
    h.app.handle_input("\x1bg");
    h.clear();
    h.app.apply(agents_status(
        json!([{"id": "t9", "definition": "review@1", "state": "running"}]),
    ));
    assert!(
        h.calls()
            .iter()
            .any(|c| c.ends_with("progress.assess {\"task_id\":\"t9\"}")),
        "{:?}",
        h.calls()
    );
}

/// The round's assessments show in the graph's task details once its last answer arrives; a stale round's
/// answers are dropped.
#[test]
fn interactive_p_tui_rlm_progress_shows_in_the_task_details() {
    let mut h = harness();
    h.app.handle_input("\x1bg");
    h.app.apply(agents_status(json!([
        {"id": "ultron-task-a", "definition": "rlm-child@1", "state": "running", "input": {"preview": "{\"prompt\": \"audit the parser\"}"}}
    ])));
    let tag = h
        .calls()
        .into_iter()
        .find(|c| c.contains("progress.assess"))
        .unwrap()
        .split(' ')
        .nth(1)
        .unwrap()
        .to_string();
    // A newer round starts (the next poll); the first round's answer is then dropped.
    h.clear();
    h.app.apply(agents_status(json!([
        {"id": "ultron-task-a", "definition": "rlm-child@1", "state": "running", "input": {"preview": "{\"prompt\": \"audit the parser\"}"}}
    ])));
    let stale = tag;
    let tag = h
        .calls()
        .into_iter()
        .find(|c| c.contains("progress.assess"))
        .unwrap()
        .split(' ')
        .nth(1)
        .unwrap()
        .to_string();
    assert_ne!(stale, tag);
    h.app.apply(LaneEvent::RlmInspected {
        tag: stale,
        result: Ok(json!({"classification": "stale-round", "receipts": []})),
    });
    h.app.apply(LaneEvent::RlmInspected {
        tag,
        result: Ok(json!({"classification": "making_progress", "receipts": [{}, {}]})),
    });
    let mut shown = String::new();
    for _ in 0..10 {
        shown = h.screen_at(120, 40);
        if shown.contains("making_progress") {
            break;
        }
        h.app.handle_input(DOWN);
        h.app.handle_input("\r");
    }
    assert!(shown.contains("making_progress · 2 receipts"), "{shown}");
    assert!(!shown.contains("stale-round"), "{shown}");
}

#[test]
fn interactive_p_tui_rlm_focus_loads_frame_traces() {
    let mut h = harness();
    h.app.apply(LaneEvent::RlmInspected {
        tag: "frames".into(),
        result: Ok(json!({"frames": [{"id": "frame-1", "status": "complete", "task": "Classify", "spent": {"calls": 1, "tokens": 10}, "startedAt": 1.0, "endedAt": 2.0}]})),
    });
    h.app.handle_input("\x1bg");
    let s = h.screen_at(120, 40);
    assert!(s.contains("Classify"), "{s}");
    // Select the frame's row and open its details: its trace is asked for.
    for _ in 0..10 {
        if h.calls().iter().any(|c| c.starts_with("inspect trace:")) {
            break;
        }
        h.app.handle_input(DOWN);
        h.app.handle_input("\r");
        h.screen_at(120, 40);
    }
    let call = h
        .calls()
        .into_iter()
        .find(|c| c.starts_with("inspect trace:"))
        .expect("trace requested");
    assert!(call.ends_with("rlm.frames {\"id\":\"frame-1\"}"), "{call}");
    let key = call
        .trim_start_matches("inspect trace:")
        .split(' ')
        .next()
        .unwrap()
        .to_string();
    h.app.apply(LaneEvent::RlmInspected {
        tag: format!("trace:{key}"),
        result: Err("trace gone".into()),
    });
    assert!(h.screen_at(120, 40).contains("unavailable: trace gone"));
}

// ---- themes and local settings ----

fn controller(
    setting: Option<&str>,
    colorfgbg: Option<&str>,
    custom: Option<&std::path::Path>,
) -> interactive::theme_setup::ThemeController {
    let registry = tui::theme::registry::ThemeRegistry::new(
        std::path::PathBuf::new(),
        custom.map(std::path::Path::to_path_buf).unwrap_or_default(),
        ColorMode::Truecolor,
    );
    interactive::theme_setup::ThemeController::new(registry, setting.map(str::to_string), colorfgbg)
}

#[test]
fn interactive_p_tui_theme_from_settings_custom_and_automatic() {
    // A custom theme from `<agent dir>/themes`.
    let dir = std::env::temp_dir().join(format!("p-tui-themes-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let light_json =
        include_str!("../../tui/src/theme/light.json").replace("\"name\": \"light\"", "\"name\": \"paper\"");
    std::fs::write(dir.join("paper.json"), light_json).unwrap();
    let mut h = harness();
    h.app.set_theme_controller(controller(Some("paper"), None, Some(&dir)));
    assert_eq!(h.theme.fg("accent", "x"), light().fg("accent", "x"));
    // An automatic pair follows the terminal (COLORFGBG 0;15: light background).
    let mut h = harness();
    h.app
        .set_theme_controller(controller(Some("light/dark"), Some("0;15"), None));
    assert_eq!(h.theme.fg("accent", "x"), light().fg("accent", "x"));
    let mut h = harness();
    h.app
        .set_theme_controller(controller(Some("light/dark"), Some("15;0"), None));
    assert_eq!(h.theme.fg("accent", "x"), dark().fg("accent", "x"));
    // A theme that does not load falls back to dark and says so.
    let mut h = harness();
    h.app
        .set_theme_controller(controller(Some("missing"), None, Some(&dir)));
    let s = h.screen_at(120, 30);
    assert!(
        s.contains("Error: Failed to load theme \"missing\": Theme not found: missing"),
        "{s}"
    );
    assert!(s.contains("Fell back to dark theme."), "{s}");
    // No setting: a confident detection applies and is saved.
    let mut h = harness();
    h.app.set_theme_controller(controller(None, Some("0;15"), None));
    assert_eq!(h.theme.fg("accent", "x"), light().fg("accent", "x"));
    assert_eq!(h.calls(), ["setting theme=\"light\""]);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn interactive_p_tui_theme_previews_while_browsing_and_applies_live() {
    let mut h = harness();
    h.app.set_theme_controller(controller(Some("dark"), None, None));
    h.app.apply(LaneEvent::Settings {
        settings: json!({"theme": "dark"}),
        env_overrides: Default::default(),
        themes: vec!["dark".into(), "light".into()],
        terminal_theme: "dark".into(),
    });
    h.typed("theme");
    h.app.handle_input("\r");
    // Moving to light previews it; Esc restores the setting's theme.
    h.app.handle_input(DOWN);
    assert_eq!(h.theme.fg("accent", "x"), light().fg("accent", "x"));
    h.app.handle_input(ESC);
    assert_eq!(h.theme.fg("accent", "x"), dark().fg("accent", "x"));
    // Picking light saves and applies it.
    h.app.handle_input("\r");
    h.app.handle_input(DOWN);
    h.app.handle_input("\r");
    assert_eq!(h.calls().last().unwrap(), "setting theme=\"light\"");
    assert_eq!(h.theme.fg("accent", "x"), light().fg("accent", "x"));
}

#[test]
fn interactive_p_tui_local_settings_apply_at_startup() {
    let mut h = harness();
    assert!(!h.app.show_hardware_cursor && !h.app.clear_on_shrink);
    h.app.apply_startup_settings(
        &json!({"showHardwareCursor": true, "terminal": {"clearOnShrink": true}, "editorPaddingX": 3.7}),
        &|_| None,
    );
    assert!(h.app.show_hardware_cursor && h.app.clear_on_shrink);
    h.typed("hi");
    let s = h.screen();
    assert!(s.lines().any(|l| l.starts_with("   hi")), "{s}");
    // The painter shows the hardware cursor at the editor's.
    let (tx, rx) = mpsc::channel();
    tx.send(UiEvent::InputClosed).unwrap();
    let mut out = String::new();
    paint_loop(
        &mut h.app,
        &rx,
        &mut |data| {
            out.push_str(data);
            Ok(())
        },
        &|| (80, 24),
        None,
    )
    .unwrap();
    assert!(out.contains("\x1b[?25h"), "{out:?}");
}

/// `/settings` changes that `#applySettingLocally` applies to the running TUI.
#[test]
fn interactive_p_tui_local_settings_apply_live_from_settings() {
    let mut h = harness();
    let open = |h: &mut Harness| {
        h.app.apply(LaneEvent::Settings {
            settings: json!({}),
            env_overrides: Default::default(),
            themes: vec!["dark".into(), "light".into()],
            terminal_theme: "dark".into(),
        });
    };
    let change = |h: &mut Harness, search: &str| {
        open(h);
        h.typed(search);
        h.app.handle_input("\r");
        h.app.handle_input(ESC);
    };
    change(&mut h, "hardware cursor");
    assert_eq!(h.calls().last().unwrap(), "setting showHardwareCursor=true");
    assert!(h.app.show_hardware_cursor);
    change(&mut h, "clear on shrink");
    assert_eq!(h.calls().last().unwrap(), "setting terminal.clearOnShrink=true");
    assert!(h.app.clear_on_shrink);
    change(&mut h, "editor padding");
    assert_eq!(h.calls().last().unwrap(), "setting editorPaddingX=1");
    h.typed("hi");
    assert!(h.screen().lines().any(|l| l.starts_with(" hi")), "{}", h.screen());
    // Double-escape action, read when used: fork.
    h.app.handle_input("\x7f");
    h.app.handle_input("\x7f");
    change(&mut h, "double-escape");
    assert_eq!(h.calls().last().unwrap(), "setting doubleEscapeAction=\"fork\"");
    h.clear();
    h.app.handle_input(ESC);
    h.app.handle_input(ESC);
    assert_eq!(h.calls(), ["fork_messages"]);
}

// ---- the persona (wave 41) ----

fn open_settings(h: &mut Harness, settings: Value) {
    h.app.apply(LaneEvent::Settings {
        settings,
        env_overrides: Default::default(),
        themes: vec!["dark".into(), "light".into()],
        terminal_theme: "dark".into(),
    });
}

/// `/settings` lists `persona` (ultron|off) outside the TypeScript-parity build; a change writes it as
/// `/persona ultron|off` does (off removes the key).
#[test]
fn interactive_w18s_settings_lists_persona_and_writes_it_like_the_command() {
    let mut h = harness_with(QolSettings::default(), false);
    open_settings(&mut h, json!({"persona": "ultron"}));
    h.typed("persona");
    let screen = h.screen_at(160, 40);
    let row = screen
        .lines()
        .find(|l| l.contains("Persona"))
        .unwrap_or_else(|| panic!("no Persona row:\n{screen}"));
    assert!(row.trim_end().ends_with("ultron"), "{row}");
    assert!(
        screen.contains("The Ultron persona: personality.md in the system prompt"),
        "{screen}"
    );
    h.app.handle_input("\r");
    assert_eq!(h.calls().last().unwrap(), "setting persona=null");
    h.app.handle_input(ESC);
    open_settings(&mut h, json!({}));
    h.typed("persona");
    let screen = h.screen_at(160, 40);
    let row = screen.lines().find(|l| l.contains("Persona")).unwrap();
    assert!(row.trim_end().ends_with("off"), "{row}");
    h.app.handle_input("\r");
    assert_eq!(h.calls().last().unwrap(), "setting persona=\"ultron\"");
}

/// The TypeScript-parity build (`ULTRON_TUI_PARITY=1`) shows exactly TypeScript's rows: no persona.
#[test]
fn interactive_w18s_settings_in_the_parity_build_has_no_persona_row() {
    let mut h = harness();
    open_settings(&mut h, json!({"persona": "ultron"}));
    h.typed("persona");
    let screen = h.screen_at(160, 40);
    assert!(!screen.contains("Persona"), "{screen}");
}

fn nudge(n: u32) -> Value {
    json!({"role": "custom", "customType": "ultron.nudge", "content": "Continue: take that next step now.",
           "display": true, "details": {"nudge": n, "max": 5}, "timestamp": 1})
}

fn session_screen(messages: Vec<Value>) -> String {
    let mut h = harness();
    h.app.apply(LaneEvent::Opened(SessionOpened {
        session_id: "sess".into(),
        session_file: Some("/s/cur".into()),
        name: None,
        messages,
    }));
    h.submit("/session");
    h.screen_at(120, 60)
}

/// `/session` counts the persona's auto-continues, only when there were some.
#[test]
fn interactive_w18s_session_info_counts_auto_continues_only_when_there_were_some() {
    let user = json!({"role": "user", "content": "do it", "timestamp": 1});
    let screen = session_screen(vec![user.clone(), nudge(1), nudge(2)]);
    let line = screen
        .lines()
        .find(|l| l.contains("Auto-continues:"))
        .unwrap_or_else(|| panic!("no auto-continues line:\n{screen}"));
    assert_eq!(line.trim(), "Auto-continues: 2");
    let tools = screen.lines().position(|l| l.contains("Tools:")).unwrap();
    assert!(
        screen.lines().nth(tools + 1).unwrap().contains("Auto-continues: 2"),
        "{screen}"
    );
    let screen = session_screen(vec![user]);
    assert!(screen.contains("Tools:"), "{screen}");
    assert!(!screen.contains("Auto-continues"), "{screen}");
}
