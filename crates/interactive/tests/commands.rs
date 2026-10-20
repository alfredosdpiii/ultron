//! The client TUI's commands and keys ported after the first batch, against a recording `LaneClient`, with
//! literal expectations from client-tui.ts, client-tui-actions.ts (`nextModel`), client-tui-commands.ts
//! (`pathArgument`), slash-commands-provider.ts (`/thinking`) and client-tui-chat.ts (`#addEntry`).

use std::cell::RefCell;
use std::rc::Rc;

use interactive::app::{App, AppOptions, next_model, path_argument};
use interactive::history::PromptHistoryStore;
use interactive::lane::{AutoreviewRequest, BashResult, ForkMessage, LaneClient, LaneEvent, ModelInfo, SessionOpened};
use serde_json::{Value, json};
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
    fn inspect(&self, title: &str, request: &str, payload: serde_json::Value) {
        self.push(format!("inspect {title} {request} {payload}"));
    }
    fn cancel_queued(&self, id: &str) {
        self.push(format!("cancel {id}"));
    }
    fn abort(&self) {
        self.push("abort".into());
    }
    fn abort_retry(&self) {
        self.push("abort_retry".into());
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
    fn list_pi_sessions(&self) {
        self.push("list_pi".into());
    }
    fn set_name(&self, name: &str) {
        self.push(format!("name {name}"));
    }
    fn select_model(&self, provider: &str, id: &str) {
        self.push(format!("model {provider}/{id}"));
    }
    fn bash(&self, command: &str, exclude: bool) {
        self.push(format!("bash {command} {exclude}"));
    }
    fn abort_bash(&self) {
        self.push("abort_bash".into());
    }
    fn compact(&self, instructions: Option<&str>) {
        self.push(format!("compact {instructions:?}"));
    }
    fn set_thinking(&self, level: &str) {
        self.push(format!("thinking {level}"));
    }
    fn fork_messages(&self) {
        self.push("fork_messages".into());
    }
    fn fork(&self, entry_id: &str) {
        self.push(format!("fork {entry_id}"));
    }
    fn clone_session(&self) {
        self.push("clone".into());
    }
    fn export(&self, path: Option<&str>, theme: Option<&str>) {
        self.push(format!("export {path:?} {theme:?}"));
    }
    fn usage_report(&self) {
        self.push("usage".into());
    }
    fn goal(&self, args: &str) {
        self.push(format!("goal {args}"));
    }
    fn save_setting(&self, key: &str, value: Value) {
        self.push(format!("setting {key}={value}"));
    }
    fn agents(&self) {
        self.push("agents".into());
    }
    fn tree(&self) {
        self.push("tree".into());
    }
    fn navigate(&self, target: Option<&str>, summarize: bool, instructions: Option<&str>) {
        self.push(format!("navigate {target:?} {summarize} {instructions:?}"));
    }
    fn set_label(&self, entry_id: &str, label: Option<&str>) {
        self.push(format!("label {entry_id} {label:?}"));
    }
    fn import_pi(&self, path: &str) {
        self.push(format!("import {path}"));
    }
    fn delete_session(&self, path: &str) {
        self.push(format!("delete {path}"));
    }
    fn rename_session(&self, path: &str, name: &str) {
        self.push(format!("rename {path} {name}"));
    }
    fn read_settings(&self) {
        self.push("read_settings".into());
    }
    fn read_mode(&self) {
        self.push("read_mode".into());
    }
    fn read_persona(&self) {
        self.push("read_persona".into());
    }
    fn session_persona(&self, command: &str, args: &str) {
        self.push(format!("session {command} {args}"));
    }
    fn autoreview(&self, request: AutoreviewRequest) {
        self.push(match request {
            AutoreviewRequest::Status => "autoreview status".into(),
            AutoreviewRequest::Accounts => "autoreview accounts".into(),
            AutoreviewRequest::Off => "autoreview off".into(),
            AutoreviewRequest::On { accounts, reviewing_as } => {
                format!("autoreview on {accounts:?} as {reviewing_as}")
            }
        });
    }
    fn login_providers(&self, query: &str) {
        self.push(format!("login_providers {query}"));
    }
    fn save_api_key(&self, provider: &str, key: &str) {
        self.push(format!("save_api_key {provider} {key}"));
    }
    fn rlm_status(&self) {
        self.push("rlm_status".into());
    }
}

fn model(id: &str, reasoning: bool) -> ModelInfo {
    ModelInfo {
        provider: "anthropic".into(),
        id: id.into(),
        name: format!("Model {id}"),
        reasoning,
        context_window: 1000,
        thinking_levels: Vec::new(),
    }
}

fn app_with(qol: interactive::qol::QolSettings, models: Vec<ModelInfo>) -> (App, Rc<Calls>) {
    let theme = Rc::new(create_theme(&builtin_theme_json("dark").unwrap(), ColorMode::Truecolor, None).unwrap());
    let keys = || {
        KeybindingsManager::new(
            native_app_keybindings(Platform::new("linux", false)),
            Vec::new(),
            Default::default(),
        )
    };
    let calls = Rc::new(Calls::default());
    let current = models[0].clone();
    let mut app = App::new(
        AppOptions {
            theme,
            keys: keys(),
            editor_keys: keys(),
            version: "1.2.3".into(),
            cwd: "/p".into(),
            home: None,
            server_id: "server".into(),
            models,
            model: current,
            thinking_level: "medium".into(),
            splash: false,
            history: PromptHistoryStore::new(None, "/p"),
            fd_path: None,
            highlight: Rc::new(|code: &str| code.split('\n').map(str::to_string).collect()),
            qol,
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

fn app() -> (App, Rc<Calls>) {
    app_with(
        interactive::qol::QolSettings::parity(),
        vec![model("a", true), model("b", true), model("c", false)],
    )
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
    app.render(80, 30)
        .iter()
        .map(|l| tui::utils::strip_terminal_sequences(l).trim_end().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn calls(c: &Calls) -> Vec<String> {
    c.0.borrow().clone()
}

fn footer(app: &mut App) -> String {
    screen(app).lines().last().unwrap_or("").to_string()
}

#[test]
fn interactive_commands_path_argument_like_ts() {
    assert_eq!(path_argument(""), None);
    assert_eq!(path_argument("   "), None);
    assert_eq!(path_argument("out.html rest"), Some("out.html".into()));
    assert_eq!(path_argument("  a\tb"), Some("a".into()));
    assert_eq!(path_argument("\"my file.jsonl\" x"), Some("my file.jsonl".into()));
    assert_eq!(path_argument("'q'"), Some("q".into()));
    assert_eq!(path_argument("\"open"), None);
}

#[test]
fn interactive_commands_next_model_wraps_and_respects_scope() {
    let models = [model("a", true), model("b", true), model("c", false)];
    assert_eq!(next_model(&models, &models[0], 1, &[]).unwrap().id, "b");
    assert_eq!(next_model(&models, &models[0], -1, &[]).unwrap().id, "c");
    assert_eq!(next_model(&models, &models[2], 1, &[]).unwrap().id, "a");
    // An unknown current model starts before the first.
    assert_eq!(next_model(&models, &model("z", true), 1, &[]).unwrap().id, "a");
    let scope = ["anthropic/a".to_string(), "c".to_string()];
    assert_eq!(next_model(&models, &models[0], 1, &scope).unwrap().id, "c");
    assert_eq!(next_model(&models, &models[2], 1, &scope).unwrap().id, "a");
    assert_eq!(
        next_model(&models, &models[0], 1, &["anthropic/*".to_string()])
            .unwrap()
            .id,
        "b"
    );
    assert_eq!(
        next_model(&models, &models[0], 1, &["a".to_string()]).unwrap_err(),
        "Only one model in scope"
    );
    // A scope matching nothing falls back to every model.
    assert_eq!(
        next_model(&models, &models[0], 1, &["x/y".to_string()]).unwrap().id,
        "b"
    );
    assert_eq!(
        next_model(&models[..1], &models[0], 1, &[]).unwrap_err(),
        "Only one model available"
    );
}

#[test]
fn interactive_commands_bang_runs_bash_and_double_bang_excludes() {
    let (mut app, c) = app();
    submit(&mut app, "!ls -la");
    assert_eq!(calls(&c), ["bash ls -la false"]);
    assert!(screen(&mut app).contains("$ ls -la"), "{}", screen(&mut app));
    // A second command while one runs goes back into the editor.
    submit(&mut app, "!!pwd");
    assert_eq!(calls(&c).len(), 1);
    let s = screen(&mut app);
    assert!(
        s.contains("A bash command is already running. Press Esc to cancel it first."),
        "{s}"
    );
    assert_eq!(app.editor_text(), "!!pwd");
    // Esc cancels the command, not a turn.
    app.handle_input("\x1b");
    assert_eq!(calls(&c).last().unwrap(), "abort_bash");
    app.apply(LaneEvent::BashDone(Ok(BashResult {
        cancelled: true,
        ..BashResult::default()
    })));
    app.apply(LaneEvent::Message(
        json!({"role": "bashExecution", "command": "ls -la", "output": "", "cancelled": true}),
    ));
    let s = screen(&mut app);
    assert!(s.contains("(cancelled)") && !s.contains("Running..."), "{s}");
    app.handle_input("\x15");
    submit(&mut app, "!!pwd");
    assert_eq!(calls(&c).last().unwrap(), "bash pwd true");
    // `!` alone is a prompt.
    app.apply(LaneEvent::BashDone(Err("boom".into())));
    assert!(screen(&mut app).contains("Error: boom"));
    submit(&mut app, "!");
    assert_eq!(calls(&c).last().unwrap(), "prompt !");
}

#[test]
fn interactive_commands_bash_execution_message_renders_exit_code_and_output() {
    let (mut app, _) = app();
    app.apply(LaneEvent::Message(
        json!({"role": "bashExecution", "command": "make", "output": "built\n", "exitCode": 2, "cancelled": false, "truncated": false}),
    ));
    let s = screen(&mut app);
    let rows: Vec<&str> = s.lines().collect();
    let at = rows.iter().position(|l| *l == " $ make").expect(&s);
    assert_eq!(&rows[at + 1..at + 6], ["", " built", "", "", " (exit 2)"]);
}

#[test]
fn interactive_commands_compaction_redraws_transcript_and_unknown_context() {
    let (mut app, c) = app();
    app.apply(LaneEvent::Message(json!({"role": "user", "content": "hello"})));
    app.apply(LaneEvent::Message(json!({"role": "assistant", "content": [{"type": "text", "text": "Hi."}], "stopReason": "stop", "usage": {"input": 10, "output": 5, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 15, "cost": {"total": 0}}})));
    assert!(footer(&mut app).is_empty() || screen(&mut app).contains("1.5%/1.0k"));
    submit(&mut app, "/compact keep it short");
    assert_eq!(calls(&c), ["compact Some(\"keep it short\")"]);
    assert!(screen(&mut app).contains("Compacting…"));
    app.apply(LaneEvent::CompactionStarted {
        reason: "manual".into(),
    });
    assert!(screen(&mut app).contains("Compacting context... (escape to cancel)"));
    app.apply(LaneEvent::Compacted(Ok(json!({
        "type": "compaction", "tokensBefore": 15,
        "retainedTail": [{"role": "user", "content": "hello"}],
    }))));
    let s = screen(&mut app);
    assert!(s.contains("[compaction] compacted from 15 tokens"), "{s}");
    assert!(!s.contains("Hi.") && !s.contains("Compacting"), "{s}");
    assert!(s.contains("?/1.0k"), "{s}");
    // A measured reply after the compaction counts again.
    app.apply(LaneEvent::Message(json!({"role": "assistant", "content": [{"type": "text", "text": "After."}], "stopReason": "stop", "usage": {"input": 20, "output": 5, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 25, "cost": {"total": 0}}})));
    assert!(screen(&mut app).contains("2.5%/1.0k"), "{}", screen(&mut app));
}

/// Esc during a manual compaction cancels it (interactive-mode.ts `abortCompaction`): the session is asked to abort,
/// and the compaction's cancelled answer clears the indicator with "Compaction cancelled", no rejection.
#[test]
fn interactive_commands_escape_cancels_a_manual_compaction() {
    let (mut app, c) = app();
    submit(&mut app, "/compact");
    app.apply(LaneEvent::CompactionStarted {
        reason: "manual".into(),
    });
    assert!(screen(&mut app).contains("Compacting context... (escape to cancel)"));
    app.handle_input("\x1b");
    assert_eq!(calls(&c).last().unwrap(), "abort");
    app.apply(LaneEvent::Compacted(Err("Compaction cancelled".into())));
    let s = screen(&mut app);
    assert!(
        s.contains("Compaction cancelled") && !s.contains("Operation rejected"),
        "{s}"
    );
    assert!(!s.contains("Compacting context"), "{s}");
    // Idle again: Esc no longer aborts anything.
    let before = calls(&c).len();
    app.handle_input("\x1b");
    assert!(!calls(&c)[before..].contains(&"abort".to_string()), "{:?}", calls(&c));
}

/// A run's retry (interactive-mode.ts `auto_retry_start` / `auto_retry_end`): the countdown without an error line,
/// Esc stops the retry (`abortRetry`, not the run's abort); a final failure says `Retry failed after N attempts`.
#[test]
fn interactive_commands_auto_retry_shows_the_countdown_and_esc_stops_it() {
    let (mut app, c) = app();
    submit(&mut app, "hello");
    app.apply(LaneEvent::OperationStarted("run".into()));
    app.apply(LaneEvent::AutoRetryStart {
        attempt: 1,
        max_attempts: 3,
        delay_ms: 2000,
    });
    let s = screen(&mut app);
    assert!(s.contains("Retrying (1/3) in 2s... (escape to cancel)"), "{s}");
    assert!(!s.contains("Error:"), "{s}");
    app.handle_input("\x1b");
    assert_eq!(calls(&c).last().unwrap(), "abort_retry");
    // A retry that succeeded clears the countdown and shows nothing else.
    app.apply(LaneEvent::AutoRetryStart {
        attempt: 2,
        max_attempts: 3,
        delay_ms: 4000,
    });
    app.apply(LaneEvent::AutoRetryEnd {
        attempt: 2,
        failed: None,
    });
    let s = screen(&mut app);
    assert!(!s.contains("Retrying") && !s.contains("Retry failed"), "{s}");
    // Esc is the run's abort again once the retry is over.
    app.handle_input("\x1b");
    assert_eq!(calls(&c).last().unwrap(), "abort");
    app.apply(LaneEvent::AutoRetryStart {
        attempt: 3,
        max_attempts: 3,
        delay_ms: 8000,
    });
    app.apply(LaneEvent::AutoRetryEnd {
        attempt: 3,
        failed: Some("Overloaded".into()),
    });
    let s = screen(&mut app);
    assert!(s.contains("Error: Retry failed after 3 attempts: Overloaded"), "{s}");
    assert!(!s.contains("Retrying"), "{s}");
}

/// A summary's retry (interactive-mode.ts `summarization_retry_*`): `Error: <provider error>` in the transcript and
/// the `RetryStatusIndicator` countdown; when the retry starts, the compaction's indicator for its reason (or the
/// branch summary's) replaces it; `summarization_retry_finished` clears only a retry countdown.
#[test]
fn interactive_commands_summarization_retry_shows_like_interactive_mode() {
    let (mut app, _) = app();
    submit(&mut app, "/compact");
    app.apply(LaneEvent::CompactionStarted {
        reason: "manual".into(),
    });
    app.apply(LaneEvent::SummarizationRetryScheduled {
        attempt: 1,
        max_attempts: 3,
        delay_ms: 2000,
        error: "Overloaded".into(),
    });
    let s = screen(&mut app);
    assert!(s.contains("Error: Overloaded"), "{s}");
    assert!(s.contains("Retrying (1/3) in 2s... (escape to cancel)"), "{s}");
    assert!(!s.contains("Compacting context"), "{s}");
    app.apply(LaneEvent::SummarizationRetryStarted {
        branch_summary: false,
        reason: "manual".into(),
    });
    let s = screen(&mut app);
    assert!(
        s.contains("Compacting context... (escape to cancel)") && !s.contains("Retrying"),
        "{s}"
    );
    // The compaction's own indicator stays when the retries end.
    app.apply(LaneEvent::SummarizationRetryFinished);
    assert!(screen(&mut app).contains("Compacting context... (escape to cancel)"));
    // A countdown cleared by the end of the retries; an overflow compaction's and a branch summary's indicators.
    app.apply(LaneEvent::SummarizationRetryScheduled {
        attempt: 2,
        max_attempts: 3,
        delay_ms: 4001,
        error: "Overloaded".into(),
    });
    assert!(screen(&mut app).contains("Retrying (2/3) in 5s..."));
    app.apply(LaneEvent::SummarizationRetryFinished);
    assert!(!screen(&mut app).contains("Retrying"));
    app.apply(LaneEvent::SummarizationRetryStarted {
        branch_summary: false,
        reason: "overflow".into(),
    });
    assert!(screen(&mut app).contains("Context overflow detected, Auto-compacting... (escape to cancel)"));
    app.apply(LaneEvent::SummarizationRetryStarted {
        branch_summary: true,
        reason: "manual".into(),
    });
    assert!(screen(&mut app).contains("Summarizing branch... (escape to cancel)"));
}

/// An automatic compaction during a run (interactive-mode.ts `CompactionStatusIndicator`): its own status line by
/// reason, then the run's working indicator again; a failed one is shown in the transcript.
#[test]
fn interactive_commands_auto_compaction_status_line_then_working_again() {
    let (mut app, _) = app();
    app.apply(LaneEvent::OperationStarted("run-1".into()));
    assert!(screen(&mut app).contains("Working... (escape to abort)"));
    app.apply(LaneEvent::CompactionStarted {
        reason: "threshold".into(),
    });
    let s = screen(&mut app);
    assert!(s.contains("Auto-compacting... (escape to cancel)"), "{s}");
    assert!(!s.contains("Working..."), "{s}");
    app.apply(LaneEvent::Compacted(Ok(json!({
        "type": "compaction", "tokensBefore": 990005, "retainedTail": [{"role": "user", "content": "hello"}],
    }))));
    let s = screen(&mut app);
    assert!(s.contains("[compaction] compacted from 990005 tokens"), "{s}");
    assert!(
        s.contains("Working... (escape to abort)") && !s.contains("Auto-compacting"),
        "{s}"
    );
    app.apply(LaneEvent::CompactionStarted {
        reason: "overflow".into(),
    });
    assert!(
        screen(&mut app).contains("Context overflow detected, Auto-compacting... (escape to cancel)"),
        "{}",
        screen(&mut app)
    );
    app.apply(LaneEvent::CompactionEnded {
        status: "failed".into(),
        error: Some("Context overflow recovery failed: summary failed".into()),
    });
    let s = screen(&mut app);
    assert!(s.contains("Context overflow recovery failed: summary failed"), "{s}");
    assert!(
        s.contains("Working... (escape to abort)") && !s.contains("Auto-compacting"),
        "{s}"
    );
    // Declined (trimmed instead): back to working, nothing else shown.
    app.apply(LaneEvent::CompactionStarted {
        reason: "threshold".into(),
    });
    app.apply(LaneEvent::CompactionEnded {
        status: "declined".into(),
        error: None,
    });
    let s = screen(&mut app);
    assert!(
        s.contains("Working... (escape to abort)") && !s.contains("Auto-compacting"),
        "{s}"
    );
    // After the run, a compaction leaves no indicator.
    app.apply(LaneEvent::OperationEnded { error: None });
    app.apply(LaneEvent::CompactionStarted {
        reason: "manual".into(),
    });
    app.apply(LaneEvent::CompactionEnded {
        status: "aborted".into(),
        error: None,
    });
    let s = screen(&mut app);
    assert!(!s.contains("Working...") && !s.contains("Compacting"), "{s}");
    assert!(s.contains("Auto-compaction cancelled"), "{s}");
}

#[test]
fn interactive_commands_compaction_rejection_uses_the_lane_wording() {
    let (mut app, _) = app();
    app.apply(LaneEvent::Compacted(Err(
        "Nothing to compact (session too small)".into()
    )));
    assert!(screen(&mut app).contains("Operation rejected: Lane \"main\" has nothing to compact"));
    app.apply(LaneEvent::Compacted(Err("Summary failed".into())));
    assert!(screen(&mut app).contains("Operation rejected: Summary failed"));
}

#[test]
fn interactive_commands_thinking_argument_selector_and_cycle() {
    let (mut app, c) = app();
    submit(&mut app, "/thinking HIGH");
    assert_eq!(calls(&c), ["thinking high"]);
    assert!(screen(&mut app).contains("Thinking level: high."));
    submit(&mut app, "/thinking nope");
    let rows: String = app
        .render(120, 30)
        .iter()
        .map(|l| tui::utils::strip_terminal_sequences(l))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        rows.contains("Error: Unknown thinking level \"nope\". Available levels: off, minimal, low, medium, high."),
        "{rows}"
    );
    submit(&mut app, "/thinking");
    let s = screen(&mut app);
    assert!(
        s.contains("Select thinking level:") && s.contains("high (selected)") && s.contains("Deep reasoning"),
        "{s}"
    );
    app.handle_input("\x1b[A");
    app.handle_input("\r");
    assert_eq!(calls(&c).last().unwrap(), "thinking medium");
    // Shift+Tab steps through the model's levels and wraps.
    app.handle_input("\x1b[Z");
    assert_eq!(calls(&c).last().unwrap(), "thinking high");
    assert!(screen(&mut app).contains("Thinking level: high"));
    app.handle_input("\x1b[Z");
    assert_eq!(calls(&c).last().unwrap(), "thinking off");
    // A model's own levels win over Pi's default five.
    app.apply(LaneEvent::Model(ModelInfo {
        thinking_levels: vec!["off".into(), "xhigh".into()],
        ..model("a", true)
    }));
    app.handle_input("\x1b[Z");
    assert_eq!(calls(&c).last().unwrap(), "thinking xhigh");
}

#[test]
fn interactive_commands_thinking_cycle_needs_a_reasoning_model() {
    let (mut app, c) = app_with(interactive::qol::QolSettings::parity(), vec![model("c", false)]);
    app.handle_input("\x1b[Z");
    assert!(calls(&c).is_empty());
    assert!(screen(&mut app).contains("Current model does not support thinking"));
}

#[test]
fn interactive_commands_ctrl_p_switches_and_reports_thinking() {
    let (mut app, c) = app();
    app.handle_input("\x10");
    assert_eq!(calls(&c), ["model anthropic/b"]);
    assert!(screen(&mut app).contains("Switched to Model b (thinking: medium)"));
    app.handle_input("\x10");
    // A model without reasoning shows no thinking level.
    assert!(screen(&mut app).contains("Switched to Model c"));
    assert!(!screen(&mut app).contains("Model c (thinking"));
}

#[test]
fn interactive_commands_ctrl_t_hides_thinking_and_saves_setting() {
    let (mut app, c) = app();
    app.apply(LaneEvent::Message(json!({"role": "assistant", "content": [{"type": "thinking", "thinking": "Deep thought."}, {"type": "text", "text": "Answer."}], "stopReason": "stop"})));
    assert!(screen(&mut app).contains("Deep thought."));
    app.handle_input("\x14");
    let s = screen(&mut app);
    assert!(
        !s.contains("Deep thought.") && s.contains("Thinking...") && s.contains("Thinking blocks: hidden"),
        "{s}"
    );
    assert_eq!(calls(&c), ["setting hideThinkingBlock=true"]);
    app.handle_input("\x14");
    assert!(screen(&mut app).contains("Thinking blocks: visible"));
    assert_eq!(calls(&c).last().unwrap(), "setting hideThinkingBlock=false");
}

#[test]
fn interactive_commands_fork_selector_picks_and_forks() {
    let (mut app, c) = app();
    submit(&mut app, "/fork");
    assert_eq!(calls(&c), ["fork_messages"]);
    app.apply(LaneEvent::ForkMessages(Vec::new()));
    assert!(screen(&mut app).contains("No messages to fork from"));
    app.apply(LaneEvent::ForkMessages(vec![
        ForkMessage {
            entry_id: "e1".into(),
            text: "first\nask".into(),
        },
        ForkMessage {
            entry_id: "e2".into(),
            text: "second".into(),
        },
    ]));
    let s = screen(&mut app);
    assert!(s.contains("Fork from Message"), "{s}");
    assert!(
        s.contains("  first ask") && s.contains("› second") && s.contains("  Message 2 of 2"),
        "{s}"
    );
    // Up from the newest; Up again wraps to the newest; Down wraps back to the oldest.
    app.handle_input("\x1b[A");
    assert!(screen(&mut app).contains("› first ask"));
    app.handle_input("\x1b[A");
    assert!(screen(&mut app).contains("› second"));
    app.handle_input("\x1b[B");
    assert!(screen(&mut app).contains("› first ask"));
    app.handle_input("\r");
    assert_eq!(calls(&c).last().unwrap(), "fork e1");
    app.apply(LaneEvent::Forked {
        editor_text: "first\nask".into(),
        status: "Forked to new session".into(),
    });
    assert_eq!(app.editor_text(), "first\nask");
    assert!(screen(&mut app).contains("Forked to new session"));
}

#[test]
fn interactive_commands_fork_selector_cancels_on_escape() {
    let (mut app, c) = app();
    app.apply(LaneEvent::ForkMessages(vec![ForkMessage {
        entry_id: "e1".into(),
        text: "only".into(),
    }]));
    app.handle_input("\x1b");
    assert!(!screen(&mut app).contains("Fork from Message"));
    assert!(calls(&c).is_empty());
}

#[test]
fn interactive_commands_export_clone_usage_reach_the_lane() {
    let (mut app, c) = app();
    submit(&mut app, "/export \"a b.html\" extra");
    submit(&mut app, "/export");
    submit(&mut app, "/clone");
    submit(&mut app, "/usage");
    assert_eq!(
        calls(&c),
        [
            "export Some(\"a b.html\") Some(\"dark\")",
            "export None Some(\"dark\")",
            "clone",
            "usage"
        ]
    );
    app.apply(LaneEvent::Notice("Report line".into()));
    assert!(screen(&mut app).contains(" Report line"));
}

/// `/export` names the theme drawn now (`generateHtml(..., theme.name)`): a custom theme's own name, the new theme
/// after a `/settings` change, and `dark` after a theme that failed to load.
#[test]
fn interactive_commands_export_names_the_active_theme() {
    let dir = std::env::temp_dir().join(format!("commands-export-theme-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let paper = include_str!("../../tui/src/theme/light.json").replace("\"name\": \"light\"", "\"name\": \"paper\"");
    std::fs::write(dir.join("paper.json"), paper).unwrap();
    let controller = |setting: &str| {
        let registry =
            tui::theme::registry::ThemeRegistry::new(std::path::PathBuf::new(), dir.clone(), ColorMode::Truecolor);
        interactive::theme_setup::ThemeController::new(registry, Some(setting.to_string()), None)
    };
    let (mut failed, f) = app();
    let (mut custom, p) = app();
    custom.set_theme_controller(controller("paper"));
    submit(&mut custom, "/export");
    let (mut app, c) = app();
    app.set_theme_controller(controller("dark"));
    submit(&mut app, "/export");
    // `/settings` → Theme → light.
    app.apply(settings_event());
    typed(&mut app, "theme");
    app.handle_input("\r");
    app.handle_input("\x1b[B");
    app.handle_input("\r");
    app.handle_input("\x1b");
    assert!(!screen(&mut app).contains("Type to search"));
    submit(&mut app, "/export x.html");
    failed.set_theme_controller(controller("missing"));
    submit(&mut failed, "/export");
    let _ = std::fs::remove_dir_all(&dir);
    let exports = |c: &Rc<Calls>| -> Vec<String> { calls(c).into_iter().filter(|x| x.starts_with("export")).collect() };
    assert_eq!(exports(&p), ["export None Some(\"paper\")"]);
    assert_eq!(
        exports(&c),
        ["export None Some(\"dark\")", "export Some(\"x.html\") Some(\"light\")"]
    );
    assert_eq!(exports(&f), ["export None Some(\"dark\")"]);
}

#[test]
fn interactive_commands_ctrl_o_reports_expansion() {
    let (mut app, _) = app();
    app.handle_input("\x0f");
    assert!(screen(&mut app).contains("Tool output: expanded"));
    app.handle_input("\x0f");
    assert!(screen(&mut app).contains("Tool output: collapsed"));
}

#[test]
fn interactive_commands_history_search_puts_the_match_in_the_editor() {
    let (mut app, _) = app();
    app.handle_input("\x1br");
    assert!(screen(&mut app).contains("No prompt history yet"));
    submit(&mut app, "first prompt");
    submit(&mut app, "second prompt");
    app.handle_input("\x1br");
    assert!(screen(&mut app).contains("history search:"));
    typed(&mut app, "fir");
    app.handle_input("\r");
    assert_eq!(app.editor_text(), "first prompt");
}

#[test]
fn interactive_commands_overlays_stay_off_in_parity() {
    let (mut app, c) = app();
    app.handle_input("\x1b[15~");
    app.handle_input("\x1ba");
    assert!(calls(&c).is_empty(), "{:?}", calls(&c));
}

#[test]
fn interactive_commands_f5_retries_only_a_failed_turn() {
    let (mut app, c) = app_with(
        interactive::qol::QolSettings::default(),
        vec![model("a", true), model("b", true)],
    );
    app.apply(LaneEvent::Message(json!({"role": "user", "content": "do it"})));
    app.apply(LaneEvent::Message(
        json!({"role": "assistant", "content": [{"type": "text", "text": "Done."}], "stopReason": "stop"}),
    ));
    app.handle_input("\x1b[15~");
    assert!(calls(&c).is_empty());
    assert!(screen(&mut app).contains("Nothing to retry"));
    app.apply(LaneEvent::Message(json!({"role": "user", "content": "again"})));
    app.apply(LaneEvent::Message(
        json!({"role": "assistant", "content": [], "stopReason": "error", "errorMessage": "Overloaded"}),
    ));
    app.handle_input("\x1b[15~");
    assert_eq!(calls(&c), ["prompt again"]);
    app.handle_input("\x1ba");
    assert_eq!(calls(&c).last().unwrap(), "agents");
    app.handle_input("\x1bl");
    assert!(app.take_reset_display());
    assert!(!app.take_reset_display());
}

#[test]
fn interactive_commands_esc_esc_asks_for_rewind_targets_when_idle() {
    let (mut app, c) = app_with(
        interactive::qol::QolSettings::default(),
        vec![model("a", true), model("b", true)],
    );
    app.handle_input("\x1b");
    assert!(calls(&c).is_empty());
    app.handle_input("\x1b");
    assert_eq!(calls(&c), ["fork_messages"]);
    // A key in between breaks the double tap.
    typed(&mut app, "x");
    app.handle_input("\x15");
    app.handle_input("\x1b");
    assert_eq!(calls(&c).len(), 1);
}

#[test]
fn interactive_commands_rewind_forks_before_the_picked_user_turn() {
    let (mut app, c) = app_with(
        interactive::qol::QolSettings::default(),
        vec![model("a", true), model("b", true)],
    );
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
    let forkable = vec![
        ForkMessage {
            entry_id: "u1".into(),
            text: "one".into(),
        },
        ForkMessage {
            entry_id: "u2".into(),
            text: "two".into(),
        },
    ];
    app.handle_input("\x1b");
    app.handle_input("\x1b");
    app.apply(LaneEvent::ForkMessages(forkable.clone()));
    assert!(screen(&mut app).contains("Rewind"));
    // The newest turn is a reply: nothing after it to drop.
    app.handle_input("\r");
    assert!(screen(&mut app).contains("Already at this point"));
    app.handle_input("\x1b");
    app.handle_input("\x1b");
    app.apply(LaneEvent::ForkMessages(forkable));
    app.handle_input("\x1b[D");
    app.handle_input("\r");
    assert_eq!(calls(&c).last().unwrap(), "fork u2");
}

#[test]
fn interactive_commands_agents_opens_the_hub_with_overlays_only() {
    // Without the overlays `/agents` is the inspection command (inspection-commands.ts): `agents.status`.
    let (mut app, c) = app();
    submit(&mut app, "/agents");
    assert_eq!(calls(&c), ["inspect /agents agents.status {}"]);
    submit(&mut app, "/agents task-1");
    assert_eq!(calls(&c)[1], r#"inspect /agents task-1 agents.inspect {"id":"task-1"}"#);
    submit(&mut app, "/progress");
    assert_eq!(calls(&c).len(), 2);
    assert!(screen(&mut app).contains("Usage: /progress <task-id>"));
    let (mut app, c) = app_with(
        interactive::qol::QolSettings::default(),
        vec![model("a", true), model("b", true)],
    );
    submit(&mut app, "/agents");
    assert_eq!(calls(&c), ["agents"]);
    app.apply(LaneEvent::Agents(Vec::new()));
    assert!(
        app.render(80, 20)
            .iter()
            .any(|l| tui::utils::strip_terminal_sequences(l).contains("No agents in this session"))
    );
}

/// The history writer runs on its own thread; dropping the store waits for it, so a prompt recorded just before
/// exit is in the file (cli_interactive_prompt_history_survives_restart failed when the process won the race).
#[test]
fn interactive_commands_prompt_history_is_on_disk_when_the_store_drops() {
    let dir = std::env::temp_dir().join(format!("interactive-hist-{}", std::process::id()));
    let path = dir.join("nested").join("prompt-history.jsonl");
    for i in 0..50 {
        let mut store = PromptHistoryStore::new(Some(path.clone()), "/p");
        store.append(&format!("prompt {i}"), i);
        drop(store);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.ends_with(&format!(
                "{{\"text\":\"prompt {i}\",\"cwd\":\"/p\",\"timestamp\":{i}}}\n"
            )),
            "{text}"
        );
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn interactive_commands_ctrl_shift_f_opens_transcript_search_with_overlays() {
    let (mut app, _) = app_with(
        interactive::qol::QolSettings::default(),
        vec![model("a", true), model("b", true)],
    );
    app.apply(LaneEvent::Message(
        json!({"role": "user", "content": "find the needle here"}),
    ));
    let _ = app.render(80, 24);
    app.handle_input("\x1b[102;6u");
    typed(&mut app, "needle");
    let s = screen(&mut app);
    assert!(s.contains("needle") && s.contains("1/1"), "{s}");
    app.handle_input("\x1b");
    assert!(screen(&mut app).contains("ultron v"));
    // Parity mode: no transcript search.
    let (mut parity, _) = crate::app();
    parity.handle_input("\x1b[102;6u");
    assert!(!screen(&mut parity).contains("1/1"));
}

/// A 1x1 PNG.
const PNG: [u8; 67] = [
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00,
    0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0a, 0x49,
    0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00,
    0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

#[derive(Default)]
struct ImageCalls(RefCell<Vec<(String, usize)>>);

struct ImageRecorder(Rc<ImageCalls>);

impl LaneClient for ImageRecorder {
    fn prompt(&self, _: &str) {}
    fn steer(&self, text: &str) -> String {
        self.0.0.borrow_mut().push((format!("steer {text}"), 0));
        "s".into()
    }
    fn follow_up(&self, text: &str) -> String {
        self.0.0.borrow_mut().push((format!("follow_up {text}"), 0));
        "f".into()
    }
    fn steer_with_images(&self, text: &str, images: Vec<interactive::lane::PromptImage>) -> String {
        self.0.0.borrow_mut().push((format!("steer {text}"), images.len()));
        "s".into()
    }
    fn follow_up_with_images(&self, text: &str, images: Vec<interactive::lane::PromptImage>) -> String {
        self.0.0.borrow_mut().push((format!("follow_up {text}"), images.len()));
        "f".into()
    }
    fn cancel_queued(&self, _: &str) {}
    fn abort(&self) {}
    fn new_session(&self) {}
    fn resume(&self, _: &str) {}
    fn list_sessions(&self) {}
    fn set_name(&self, _: &str) {}
    fn select_model(&self, _: &str, _: &str) {}
}

#[test]
fn interactive_commands_images_go_with_steering_and_follow_ups() {
    let dir = std::env::temp_dir().join(format!("interactive-img-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let png = dir.join("shot.png");
    std::fs::write(&png, PNG).unwrap();
    let theme = Rc::new(create_theme(&builtin_theme_json("dark").unwrap(), ColorMode::Truecolor, None).unwrap());
    let keys = || {
        KeybindingsManager::new(
            native_app_keybindings(Platform::new("linux", false)),
            Vec::new(),
            Default::default(),
        )
    };
    let calls = Rc::new(ImageCalls::default());
    let mut app = App::new(
        AppOptions {
            theme,
            keys: keys(),
            editor_keys: keys(),
            version: "1".into(),
            cwd: "/p".into(),
            home: None,
            server_id: "s".into(),
            models: vec![model("a", true)],
            model: model("a", true),
            thinking_level: "medium".into(),
            splash: false,
            history: PromptHistoryStore::new(None, "/p"),
            fd_path: None,
            highlight: Rc::new(|code: &str| code.split('\n').map(str::to_string).collect()),
            qol: interactive::qol::QolSettings {
                attachments: true,
                ..interactive::qol::QolSettings::parity()
            },
        },
        Box::new(ImageRecorder(calls.clone())),
    );
    app.apply(LaneEvent::Opened(SessionOpened {
        session_id: "s".into(),
        session_file: None,
        name: None,
        messages: Vec::new(),
    }));
    app.apply(LaneEvent::OperationStarted("op".into()));
    let paste = |app: &mut App| app.handle_input(&format!("\x1b[200~{}\x1b[201~", png.display()));
    paste(&mut app);
    typed(&mut app, "look");
    app.handle_input("\r");
    paste(&mut app);
    typed(&mut app, "later");
    app.handle_input("\x1b\r");
    typed(&mut app, "plain");
    app.handle_input("\r");
    let got = calls.0.borrow().clone();
    assert_eq!(got.len(), 3, "{got:?}");
    assert_eq!(got[0].1, 1);
    assert!(got[0].0.starts_with("steer [Image #1"), "{got:?}");
    assert_eq!(got[1].1, 1);
    assert!(got[1].0.starts_with("follow_up [Image #"), "{got:?}");
    assert_eq!(got[2], ("steer plain".to_string(), 0));
    let _ = std::fs::remove_dir_all(dir);
}

/// `reviewCommand` (slash-commands-provider.ts): `/review` is sent as typed (`/review ${args}`.trim()); the worker's
/// agent controller expands it (session_core::review_command). While a run is in progress it is queued as steering.
#[test]
fn interactive_commands_review_is_sent_as_typed_for_the_worker_to_expand() {
    let (mut app, c) = app();
    submit(&mut app, "/review main --post");
    submit(&mut app, "/review");
    app.apply(LaneEvent::OperationStarted("op".into()));
    submit(&mut app, "/review 12");
    assert_eq!(
        calls(&c),
        ["prompt /review main --post", "prompt /review", "steer /review 12"]
    );
    assert!(screen(&mut app).contains("Queued s1."), "{}", screen(&mut app));
}

fn entry(id: &str, parent: Option<&str>, role: &str, text: &str) -> Value {
    let message = if role == "user" {
        json!({"role": "user", "content": text})
    } else {
        json!({"role": "assistant", "content": [{"type": "text", "text": text}], "stopReason": "stop"})
    };
    json!({"type": "message", "id": id, "parentId": parent, "message": message})
}

/// `/tree`: the selector over the tree, the summary choice, then the navigation to before a user message (whose
/// text returns to the editor).
#[test]
fn interactive_commands_tree_navigates_before_a_user_message() {
    let (mut app, c) = app();
    submit(&mut app, "/tree");
    assert_eq!(calls(&c), ["tree"]);
    app.apply(LaneEvent::Tree {
        tree: json!([]),
        leaf: None,
    });
    assert!(screen(&mut app).contains("No entries in session"));
    let tree = json!([{"entry": entry("u1", None, "user", "first ask"), "children": [
        {"entry": entry("a1", Some("u1"), "assistant", "One."), "children": [
            {"entry": entry("u2", Some("a1"), "user", "second ask"), "children": [
                {"entry": entry("a2", Some("u2"), "assistant", "Two."), "children": []}]}]}]}]);
    app.apply(LaneEvent::Tree {
        tree,
        leaf: Some("a2".into()),
    });
    let s = screen(&mut app);
    assert!(
        s.contains("Session Tree") && s.contains("› • assistant: Two.") && s.contains("(4/4)"),
        "{s}"
    );
    // The leaf itself: nothing to do.
    app.handle_input("\r");
    assert!(screen(&mut app).contains("Already at this point"));
    submit(&mut app, "/tree");
    app.apply(LaneEvent::Tree {
        tree: json!([{"entry": entry("u1", None, "user", "first ask"), "children": [
            {"entry": entry("a1", Some("u1"), "assistant", "One."), "children": [
                {"entry": entry("u2", Some("a1"), "user", "second ask"), "children": []}]}]}]),
        leaf: Some("u2".into()),
    });
    app.handle_input("\x1b[A");
    app.handle_input("\x1b[A");
    assert!(screen(&mut app).contains("› • user: first ask"));
    app.handle_input("\r");
    assert!(screen(&mut app).contains("Summarize branch?"));
    // Esc returns to the tree on the same entry.
    app.handle_input("\x1b");
    assert!(screen(&mut app).contains("› • user: first ask"));
    app.handle_input("\r");
    app.handle_input("\r");
    // The selected entry goes to the session (AgentSession `navigateTree` resolves it), which answers the text.
    assert_eq!(calls(&c).last().unwrap(), "navigate Some(\"u1\") false None");
    app.apply(LaneEvent::Navigated(Ok(Some("first ask".into()))));
    assert_eq!(app.editor_text(), "first ask");
    assert!(screen(&mut app).contains("Navigated to selected point"));
    app.apply(LaneEvent::Navigated(Err("boom".into())));
    assert!(screen(&mut app).contains("Error: boom"));
}

#[test]
fn interactive_commands_tree_summarize_and_filters() {
    let (mut app, c) = app();
    app.apply(LaneEvent::Tree {
        tree: json!([{"entry": entry("u1", None, "user", "ask"), "children": [
            {"entry": entry("a1", Some("u1"), "assistant", "One."), "children": [
                {"entry": entry("u2", Some("a1"), "user", "later"), "children": []}]}]}]),
        leaf: Some("u2".into()),
    });
    app.handle_input("\x1b[A");
    app.handle_input("\r");
    app.handle_input("\x1b[B");
    app.handle_input("\r");
    assert_eq!(calls(&c).last().unwrap(), "navigate Some(\"a1\") true None");
    app.apply(LaneEvent::Tree {
        tree: json!([{"entry": entry("u1", None, "user", "ask"), "children": [
            {"entry": entry("a1", Some("u1"), "assistant", "One."), "children": []}]}]),
        leaf: Some("a1".into()),
    });
    // Ctrl+U: user messages only.
    app.handle_input("\x15");
    let s = screen(&mut app);
    assert!(s.contains("[user]") && !s.contains("assistant: One."), "{s}");
    // Ctrl+U again toggles back to the default view.
    app.handle_input("\x15");
    let s = screen(&mut app);
    assert!(!s.contains("[user]") && s.contains("assistant: One."), "{s}");
    // Typing searches.
    typed(&mut app, "zzz");
    assert!(screen(&mut app).contains("No entries found"));
}

/// Esc while a navigation's branch summary runs cancels it (interactive-mode.ts: `onEscape` is
/// `abortBranchSummary` while summarizing); the cancelled navigation shows "Branch summarization cancelled" and the
/// tree selector again on the same entry. Esc no longer aborts once the navigation answered.
#[test]
fn interactive_commands_escape_cancels_a_branch_summary() {
    let (mut app, c) = app();
    let tree = json!([{"entry": entry("u1", None, "user", "ask"), "children": [
        {"entry": entry("a1", Some("u1"), "assistant", "One."), "children": [
            {"entry": entry("u2", Some("a1"), "user", "later"), "children": []}]}]}]);
    app.apply(LaneEvent::Tree {
        tree,
        leaf: Some("u2".into()),
    });
    app.handle_input("\x1b[A");
    app.handle_input("\r");
    app.handle_input("\x1b[B");
    app.handle_input("\r");
    assert_eq!(calls(&c).last().unwrap(), "navigate Some(\"a1\") true None");
    assert!(screen(&mut app).contains("Summarizing branch… (Esc to cancel)"));
    app.handle_input("\x1b");
    assert_eq!(calls(&c).last().unwrap(), "abort");
    app.apply(LaneEvent::Navigated(Err("Branch summarization cancelled".into())));
    let s = screen(&mut app);
    assert!(
        s.contains("Branch summarization cancelled") && !s.contains("Error:"),
        "{s}"
    );
    assert!(s.contains("Session Tree") && s.contains("› • assistant: One."), "{s}");
    // The selector's Esc closes it; nothing is aborted any more.
    let before = calls(&c).len();
    app.handle_input("\x1b");
    app.handle_input("\x1b");
    assert!(!calls(&c)[before..].contains(&"abort".to_string()), "{:?}", calls(&c));
    // A navigation without a summary is not cancelled by Esc.
    app.apply(LaneEvent::Tree {
        tree: json!([{"entry": entry("u1", None, "user", "ask"), "children": [
            {"entry": entry("a1", Some("u1"), "assistant", "One."), "children": [
                {"entry": entry("u2", Some("a1"), "user", "later"), "children": []}]}]}]),
        leaf: Some("u2".into()),
    });
    app.handle_input("\x1b[A");
    app.handle_input("\r");
    app.handle_input("\r");
    assert_eq!(calls(&c).last().unwrap(), "navigate Some(\"a1\") false None");
    let before = calls(&c).len();
    app.handle_input("\x1b");
    assert!(!calls(&c)[before..].contains(&"abort".to_string()), "{:?}", calls(&c));
}

fn listing(path: &str, name: Option<&str>, first: &str, modified: i64) -> interactive::lane::SessionListing {
    interactive::lane::SessionListing {
        path: path.into(),
        id: path.into(),
        name: name.map(str::to_string),
        modified,
        message_count: 2,
        first_message: first.into(),
        all_messages_text: first.into(),
        cwd: "/p".into(),
    }
}

/// `/resume`: Pi's session selector, rename and delete with confirmation.
#[test]
fn interactive_commands_session_selector_renames_and_deletes() {
    let (mut app, c) = app();
    app.apply(LaneEvent::Opened(SessionOpened {
        session_id: "cur".into(),
        session_file: Some("/s/cur".into()),
        name: None,
        messages: Vec::new(),
    }));
    submit(&mut app, "/resume");
    assert_eq!(calls(&c).last().unwrap(), "list");
    app.apply(LaneEvent::Sessions(vec![
        listing("/s/old", Some("Named one"), "old ask", 1),
        listing("/s/cur", None, "current ask", 3),
    ]));
    let s = screen(&mut app);
    assert!(
        s.contains("Resume Session (Current Fold ◉ Current Folder | ○ All  Name: All  Sort: Threaded"),
        "{s}"
    );
    // Tab's scope (session-selector.ts `SessionSelectorHeader`).
    assert!(s.contains("◉ Current Folder | ○ All"), "{s}");
    assert!(s.contains("› current ask") && s.contains("  Named one"), "{s}");
    // The current session cannot be deleted.
    app.handle_input("\x04");
    assert!(screen(&mut app).contains("Cannot delete the currently active session"));
    app.handle_input("\x1b[B");
    app.handle_input("\x04");
    assert!(screen(&mut app).contains("Delete session?"));
    app.handle_input("\r");
    assert_eq!(calls(&c).last().unwrap(), "delete /s/old");
    app.apply(LaneEvent::SessionDeleted(Ok(())));
    assert!(screen(&mut app).contains("Session moved to trash"));
    // Rename the selected session.
    app.handle_input("\x12");
    assert!(screen(&mut app).contains("Rename Session"));
    typed(&mut app, "X");
    app.handle_input("\r");
    assert!(calls(&c).last().unwrap().starts_with("rename /s/"), "{:?}", calls(&c));
    // Ctrl+N: named only.
    app.apply(LaneEvent::Sessions(vec![listing("/s/cur", None, "current ask", 3)]));
    app.handle_input("\x0e");
    assert!(screen(&mut app).contains("No named sessions in current folder."));
    app.handle_input("\x1b");
    assert!(!screen(&mut app).contains("Resume Session"));
}

#[test]
fn interactive_commands_session_search_regex_and_phrase() {
    let (mut app, c) = app();
    app.apply(LaneEvent::Sessions(vec![
        listing("/s/a", None, "fix the parser bug", 2),
        listing("/s/b", None, "write docs", 1),
    ]));
    typed(&mut app, "re:^/s/b");
    let s = screen(&mut app);
    assert!(s.contains("write docs") && !s.contains("fix the parser"), "{s}");
    for _ in 0..8 {
        app.handle_input("\x7f");
    }
    typed(&mut app, "\"parser bug\"");
    let s = screen(&mut app);
    assert!(s.contains("fix the parser bug") && !s.contains("write docs"), "{s}");
    app.handle_input("\r");
    assert_eq!(calls(&c).last().unwrap(), "resume /s/a");
}

/// Pi's selector searches `allMessagesText`: a word only a later message holds finds its session, while the row
/// still shows the first message.
#[test]
fn interactive_commands_session_search_finds_a_word_from_a_later_message() {
    let (mut app, c) = app();
    let mut later = listing("/s/a", None, "fix the parser", 2);
    later.all_messages_text = "fix the parser Done. now the kestrel lexer Kestrel lexer fixed.".into();
    app.apply(LaneEvent::Sessions(vec![later, listing("/s/b", None, "write docs", 1)]));
    typed(&mut app, "kestrel");
    let s = screen(&mut app);
    assert!(s.contains("fix the parser") && !s.contains("write docs"), "{s}");
    app.handle_input("\r");
    assert_eq!(calls(&c).last().unwrap(), "resume /s/a");
}

fn settings_event() -> LaneEvent {
    LaneEvent::Settings {
        settings: json!({}),
        env_overrides: Default::default(),
        themes: vec!["dark".into(), "light".into()],
        terminal_theme: "dark".into(),
    }
}

/// `/settings`: search, cycling a value saves it (and `compaction.enabled` drops the footer's `(auto)`), unused
/// settings say so.
#[test]
fn interactive_commands_settings_cycle_and_save() {
    let (mut app, c) = app();
    submit(&mut app, "/settings");
    assert_eq!(calls(&c), ["read_settings"]);
    app.apply(settings_event());
    let s = screen(&mut app);
    assert!(
        s.contains("→ Models") && s.contains("Auto-compact") && s.contains("Type to search"),
        "{s}"
    );
    app.handle_input("\x1b[B");
    app.handle_input("\r");
    assert_eq!(calls(&c).last().unwrap(), "setting compaction.enabled=false");
    assert!(!screen(&mut app).contains("(auto)"));
    typed(&mut app, "mermaid");
    app.handle_input("\r");
    assert_eq!(calls(&c).last().unwrap(), "setting markdown.mermaid=\"off\"");
    assert!(screen(&mut app).contains("Saved markdown.mermaid to settings."));
    for _ in 0..7 {
        app.handle_input("\x7f");
    }
    typed(&mut app, "http");
    app.handle_input("\r");
    assert_eq!(calls(&c).last().unwrap(), "setting httpIdleTimeoutMs=0");
    assert!(screen(&mut app).contains("HTTP idle timeout: disabled"));
    app.handle_input("\x1b");
    assert!(!screen(&mut app).contains("Type to search"));
}

#[test]
fn interactive_commands_settings_theme_and_model_thinking() {
    let (mut app, c) = app();
    app.apply(settings_event());
    typed(&mut app, "theme");
    app.handle_input("\r");
    let s = screen(&mut app);
    assert!(
        s.contains("Select a theme") && s.contains("✓ dark") && s.contains("Automatic"),
        "{s}"
    );
    app.handle_input("\x1b[B");
    app.handle_input("\r");
    assert_eq!(calls(&c).last().unwrap(), "setting theme=\"light\"");
    for _ in 0..5 {
        app.handle_input("\x7f");
    }
    typed(&mut app, "per model");
    app.handle_input("\r");
    assert!(screen(&mut app).contains("Per-Model Thinking Level"));
    // The session's model first; then its levels.
    app.handle_input("\r");
    assert!(screen(&mut app).contains("Thinking Level for a [anthropic]"));
    app.handle_input("\x1b[B");
    app.handle_input("\r");
    let all = calls(&c);
    assert!(
        all.contains(
            &"setting modelThinkingLevel={\"level\":\"minimal\",\"modelId\":\"a\",\"provider\":\"anthropic\"}"
                .to_string()
        ) || all
            .iter()
            .any(|x| x.starts_with("setting modelThinkingLevel=") && x.contains("minimal")),
        "{all:?}"
    );
    // The current model's override applies now.
    assert_eq!(all.last().unwrap(), "thinking minimal");
    app.handle_input("\x1b");
    assert!(screen(&mut app).contains("1 configured"));
}

fn provider(id: &str, name: &str, kind: &str, method: &str) -> interactive::selectors::AuthProvider {
    interactive::selectors::AuthProvider {
        id: id.into(),
        name: name.into(),
        auth_type: kind.into(),
        status: None,
        api_key_method: (kind == "api_key").then(|| (method.to_string(), true)),
        login_label: None,
        generic_login: true,
    }
}

/// `/login`: method, provider, then the masked key prompt; the key is saved through the session link.
#[test]
fn interactive_commands_login_with_an_api_key() {
    let (mut app, c) = app();
    submit(&mut app, "/login");
    assert_eq!(calls(&c), ["login_providers "]);
    let providers = vec![
        provider("anthropic", "Anthropic", "oauth", ""),
        provider("anthropic", "Anthropic", "api_key", "Anthropic API key"),
        provider("groq", "Groq", "api_key", "Groq API key"),
    ];
    app.apply(LaneEvent::LoginProviders {
        providers: providers.clone(),
        query: String::new(),
    });
    assert!(screen(&mut app).contains("Select authentication method:"));
    app.handle_input("\x1b[B");
    app.handle_input("\r");
    let s = screen(&mut app);
    assert!(
        s.contains("Select provider to configure:") && s.contains("Groq • unconfigured"),
        "{s}"
    );
    assert!(!s.contains("[subscription]"), "{s}");
    typed(&mut app, "groq");
    app.handle_input("\r");
    assert!(screen(&mut app).contains("Enter Groq API key"));
    typed(&mut app, "secret");
    let s = screen(&mut app);
    assert!(s.contains("••••••") && !s.contains("secret"), "{s}");
    app.handle_input("\r");
    assert_eq!(calls(&c).last().unwrap(), "save_api_key groq secret");
    // A provider named in the command, with one method, goes straight to its prompt; subscriptions are not ported.
    app.apply(LaneEvent::LoginProviders {
        providers: providers.clone(),
        query: "groq".into(),
    });
    assert!(screen(&mut app).contains("Login to Groq"));
    app.handle_input("\x1b");
    app.apply(LaneEvent::LoginProviders {
        providers,
        query: "Anthropic".into(),
    });
    assert!(screen(&mut app).contains("Select authentication method for Anthropic:"));
    app.handle_input("\r");
    let wide: String = app
        .render(300, 30)
        .iter()
        .map(|l| tui::utils::strip_terminal_sequences(l))
        .collect();
    assert!(wide.contains("signing in with an account is not available"), "{wide}");
}

#[test]
fn interactive_commands_alt_escape_opens_rewind_like_two_taps() {
    let (mut app, c) = app_with(
        interactive::qol::QolSettings::default(),
        vec![model("a", true), model("b", true)],
    );
    app.handle_input("\x1b\x1b");
    assert_eq!(calls(&c), ["fork_messages"]);
}

/// Ctrl+R docks the RLM panel (and polls the subagents); `/rlm focus` opens the graph, Esc closes it.
#[test]
fn interactive_commands_rlm_panel_and_focus() {
    let (mut app, c) = app();
    app.handle_input("\x12");
    assert_eq!(calls(&c), ["rlm_status"]);
    let s = screen(&mut app);
    assert!(
        s.contains("RLM idle 0 nodes  alt+g focus") && s.contains("◆ turn"),
        "{s}"
    );
    app.apply(LaneEvent::RlmStatus(
        json!({"subagents": [{"id": "t1", "definition": "rlm-child@1", "state": "running"}]}),
    ));
    assert!(screen(&mut app).contains("RLM"), "{}", screen(&mut app));
    app.handle_input("\x12");
    assert!(!screen(&mut app).contains("RLM idle"));
    submit(&mut app, "/rlm focus");
    let s = screen(&mut app);
    assert!(s.contains("RLM graph") && s.contains("escape/q back"), "{s}");
    app.handle_input("\x1b");
    assert!(!screen(&mut app).contains("RLM graph"));
}

/// `/goal ...` goes to the session goal's command with its arguments (client-tui-commands.ts `goal`), and the reply
/// text is shown as a notice; the reply's absence reads "No goal reply" there (cli/src/interactive.rs).
#[test]
fn interactive_commands_goal_reaches_the_session_goal_and_shows_its_reply() {
    let (mut app, c) = app();
    submit(&mut app, "/goal ship the parser");
    submit(&mut app, "/goal pause");
    submit(&mut app, "/goal");
    assert_eq!(calls(&c), ["goal ship the parser", "goal pause", "goal "]);
    assert!(!screen(&mut app).contains("not available in the Rust build"));
    app.apply(LaneEvent::GoalReply("Goal set: ship the parser".into()));
    assert!(screen(&mut app).contains(" Goal set: ship the parser"));
}

/// An assistant message whose route changed (the Claude subscription limit) shows its notice once in the transcript.
#[test]
fn interactive_commands_a_claude_route_notice_is_shown_under_its_answer() {
    let (mut app, _) = app();
    let notice = "Claude subscription limit reached; using API credits until 2026-09-21T15:13:20.000Z";
    let message = json!({
        "role": "assistant",
        "content": [{"type": "text", "text": "from the api"}],
        "api": "anthropic-messages", "provider": "anthropic", "model": "claude-opus-5-5",
        "usage": {"input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 2,
            "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}},
        "stopReason": "stop", "timestamp": 1,
        "diagnostics": [{"type": "claude_subscription_route", "timestamp": 1,
            "details": {"route": "api", "notice": notice}}],
    });
    assert_eq!(interactive::lane::route_notice(&message).as_deref(), Some(notice));
    app.apply(LaneEvent::Message(message.clone()));
    let shown = screen(&mut app);
    // The notice wraps at the terminal width.
    let flat = shown.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(flat.contains(notice), "{shown}");
    assert_eq!(shown.matches("using API credits").count(), 1, "{shown}");
    // Without the diagnostic (or on a user message) nothing is added.
    let mut plain = message;
    plain["diagnostics"] = json!([]);
    assert_eq!(interactive::lane::route_notice(&plain), None);
    assert_eq!(
        interactive::lane::route_notice(
            &json!({"role": "user", "diagnostics": [{"type": "claude_subscription_route", "details": {"notice": "x"}}]})
        ),
        None
    );
}

/// Pi's `/import [path]` (client-tui-sessions.ts `handleImport`/`importPiSessionFile`): the path is resolved against
/// the working directory and imported through the server; the answer offers to switch, and either choice says
/// where the session is.
#[test]
fn interactive_commands_import_resolves_the_path_imports_and_offers_to_switch() {
    let (mut app, c) = app();
    submit(&mut app, "/import pi/session.jsonl");
    submit(&mut app, "/import \"/abs/a b.jsonl\"");
    assert_eq!(calls(&c), ["import /p/pi/session.jsonl", "import /abs/a b.jsonl"]);
    assert!(screen(&mut app).contains("Importing /abs/a b.jsonl…"));

    let imported = |current: bool, file: Option<&str>| LaneEvent::PiImported {
        imported: "Imported 2 entries from /p/a.jsonl as session s-1".into(),
        source: "/p/a.jsonl".into(),
        session_file: file.map(str::to_string),
        current,
    };
    app.apply(imported(false, Some("/sessions/s-1.jsonl")));
    let s = screen(&mut app);
    assert!(
        s.contains("Imported 2 entries from /p/a.jsonl as session s-1. Switch to it?"),
        "{s}"
    );
    assert!(
        s.contains("Switch to the imported session") && s.contains("Stay here"),
        "{s}"
    );
    app.handle_input("\r");
    assert_eq!(calls(&c).last().unwrap(), "resume /sessions/s-1.jsonl");
    assert!(screen(&mut app).contains("Session imported from: /p/a.jsonl"));

    // "Stay here", and Escape, leave the session where it is and say how to open it.
    app.apply(imported(false, Some("/sessions/s-1.jsonl")));
    app.handle_input("\x1b[B");
    app.handle_input("\r");
    let stay = "Imported 2 entries from /p/a.jsonl as session s-1. Use /resume to open it.";
    assert!(screen(&mut app).contains(stay));
    app.apply(imported(false, Some("/sessions/s-1.jsonl")));
    app.handle_input("\x1b");
    assert!(screen(&mut app).contains(stay));
    assert_eq!(calls(&c).iter().filter(|call| call.starts_with("resume")).count(), 1);

    // The current session is not offered.
    app.apply(imported(true, None));
    assert!(screen(&mut app).contains("Imported 2 entries from /p/a.jsonl as session s-1 (the current session)"));
    assert!(!screen(&mut app).contains("Switch to it?"));
}

/// Pi's `/import` without a path (client-tui-sessions.ts `showPiSessionPicker`): Pi's session selector over Pi's
/// sessions of this project; a selection is imported (`importPiSessionFile`), deleting or renaming a Pi session is
/// refused ("Pi sessions are read-only here"), Escape closes it.
#[test]
fn interactive_commands_import_without_a_path_picks_a_pi_session_read_only() {
    let (mut app, c) = app();
    submit(&mut app, "/import");
    assert_eq!(calls(&c), ["list_pi"]);
    app.apply(LaneEvent::PiSessions(vec![
        listing("/pi/old.jsonl", Some("Pi named"), "old pi ask", 1),
        listing("/pi/new.jsonl", None, "new pi ask", 3),
    ]));
    let s = screen(&mut app);
    assert!(s.contains("› new pi ask") && s.contains("  Pi named"), "{s}");
    // Pi's files are only read: a confirmed delete and a rename are refused, nothing is sent.
    app.handle_input("\x04");
    assert!(screen(&mut app).contains("Delete session?"));
    app.handle_input("\r");
    let s = screen(&mut app);
    assert!(
        s.contains("Pi sessions are read-only here") && s.contains("new pi ask"),
        "{s}"
    );
    app.handle_input("\x12");
    typed(&mut app, "X");
    app.handle_input("\r");
    assert!(screen(&mut app).contains("Pi sessions are read-only here"));
    assert_eq!(calls(&c), ["list_pi"]);
    // Choosing one imports it through the server, as `/import <path>` does.
    app.handle_input("\x1b[B");
    app.handle_input("\r");
    assert_eq!(calls(&c), ["list_pi", "import /pi/old.jsonl"]);
    let s = screen(&mut app);
    assert!(
        s.contains("Importing /pi/old.jsonl…") && !s.contains("old pi ask"),
        "{s}"
    );
    // Escape closes the picker without importing.
    submit(&mut app, "/import");
    app.apply(LaneEvent::PiSessions(vec![listing(
        "/pi/new.jsonl",
        None,
        "new pi ask",
        3,
    )]));
    assert!(screen(&mut app).contains("new pi ask"));
    app.handle_input("\x1b");
    assert!(!screen(&mut app).contains("new pi ask"));
    assert_eq!(calls(&c), ["list_pi", "import /pi/old.jsonl", "list_pi"]);
}

fn mode_event(engineering: bool) -> LaneEvent {
    LaneEvent::Mode {
        engineering,
        always_skills: vec!["ponytail".into(), "ship-with-no-mistakes".into()],
    }
}

/// `/mode`: a picker of the modes with the settings' one selected; choosing another writes `engineering`.
#[test]
fn interactive_commands_mode_picker_switches_the_mode() {
    let qol = interactive::qol::QolSettings {
        overlays: true,
        ..interactive::qol::QolSettings::parity()
    };
    let (mut app, c) = app_with(qol, vec![model("a", true)]);
    submit(&mut app, "/mode");
    assert_eq!(calls(&c), ["read_mode"]);
    app.apply(mode_event(true));
    let s = screen(&mut app);
    assert!(
        s.contains("Select mode:") && s.contains("Engineering (selected)") && s.contains("Default"),
        "{s}"
    );
    assert!(s.contains("alwaysSkills in the system prompt"), "{s}");
    app.handle_input("\x1b[A");
    app.handle_input("\r");
    assert_eq!(calls(&c).last().unwrap(), "setting engineering=false");
    let s = screen(&mut app);
    assert!(s.contains("Engineering mode off"), "{s}");
    // A name switches without the picker.
    submit(&mut app, "/mode engineering");
    app.apply(mode_event(false));
    assert_eq!(calls(&c).last().unwrap(), "setting engineering=true");
    assert!(screen(&mut app).contains("Engineering mode on: always-on skills (ponytail"));
    submit(&mut app, "/mode research");
    assert!(screen(&mut app).contains("Unknown mode \"research\". Available modes: default, engineering."));
    // The parity build has exactly the client TUI's commands.
    let (mut parity, pc) = app_with(interactive::qol::QolSettings::parity(), vec![model("a", true)]);
    submit(&mut parity, "/mode engineering");
    assert!(screen(&mut parity).contains("Unknown slash command: /mode"));
    assert!(!calls(&pc).contains(&"read_mode".to_string()));
}

/// `/engineering [true|false]` as client-tui-pi-commands.ts `setEngineeringMode`: report, set, usage.
#[test]
fn interactive_commands_engineering_reports_and_sets_the_mode() {
    let (mut app, c) = app();
    submit(&mut app, "/engineering");
    assert_eq!(calls(&c), ["read_mode"]);
    app.apply(mode_event(true));
    assert!(screen(&mut app).contains("Engineering mode is on (always-on skills: ponytail, ship-with-no-mistakes)"));
    submit(&mut app, "/engineering off");
    app.apply(mode_event(true));
    assert_eq!(calls(&c).last().unwrap(), "setting engineering=false");
    submit(&mut app, "/engineering sometimes");
    assert!(screen(&mut app).contains("Usage: /engineering true|false"));
    assert_eq!(calls(&c).last().unwrap(), "setting engineering=false");
}

/// Submits an argument the command completes: the first Enter takes the completion, the second submits (editor.ts).
fn completed(app: &mut App, text: &str) {
    typed(app, text);
    app.handle_input("\r");
    app.handle_input("\r");
}

fn gh_accounts(accounts: &[(&str, &str)]) -> LaneEvent {
    LaneEvent::AutoreviewAccounts(
        accounts
            .iter()
            .map(|(login, host)| (login.to_string(), host.to_string()))
            .collect(),
    )
}

/// `/autoreview [on|off]` as client-tui-pi-commands.ts `setAutoreview`: the status, off, usage, and `on` reviewing as
/// the only account or as the one (or all) the picker chose.
#[test]
fn interactive_commands_autoreview_picks_the_account_to_review_as() {
    let (mut app, c) = app();
    submit(&mut app, "/autoreview");
    completed(&mut app, "/autoreview off");
    submit(&mut app, "/autoreview sometimes");
    assert!(screen(&mut app).contains("Usage: /autoreview on|off"));
    assert_eq!(calls(&c), ["autoreview status", "autoreview off"]);
    // One account: no picker.
    completed(&mut app, "/autoreview on");
    assert_eq!(calls(&c).last().unwrap(), "autoreview accounts");
    app.apply(gh_accounts(&[("octo", "github.com")]));
    assert_eq!(calls(&c).last().unwrap(), "autoreview on None as github.com/octo");
    // None logged in.
    completed(&mut app, "/autoreview on");
    app.apply(gh_accounts(&[]));
    assert!(screen(&mut app).contains("No gh account is logged in: run gh auth login, then /autoreview on"));
    // Several: the picker, then one of them.
    completed(&mut app, "/autoreview on");
    app.apply(gh_accounts(&[("octo", "github.com"), ("work", "ghe.example.com")]));
    let s = screen(&mut app);
    assert!(
        s.contains("Review pull requests as")
            && s.contains("octo (github.com)")
            && s.contains("work (ghe.example.com)")
            && s.contains("All logged-in accounts"),
        "{s}"
    );
    app.handle_input("\x1b[B");
    app.handle_input("\r");
    assert_eq!(
        calls(&c).last().unwrap(),
        "autoreview on Some([\"ghe.example.com/work\"]) as ghe.example.com/work"
    );
    // All of them.
    completed(&mut app, "/autoreview on");
    app.apply(gh_accounts(&[("octo", "github.com"), ("work", "ghe.example.com")]));
    app.handle_input("\x1b[B");
    app.handle_input("\x1b[B");
    app.handle_input("\r");
    assert_eq!(
        calls(&c).last().unwrap(),
        "autoreview on None as every logged-in account"
    );
    // Esc leaves it as it was.
    completed(&mut app, "/autoreview on");
    app.apply(gh_accounts(&[("octo", "github.com"), ("work", "ghe.example.com")]));
    let before = calls(&c).len();
    app.handle_input("\x1b");
    assert!(screen(&mut app).contains("Autoreview left as it was"));
    assert_eq!(calls(&c).len(), before);
}

/// `/persona [on|off|auto|ultron|none]` (Rust-only, with the overlays): report the settings' persona, write the
/// setting (`ultron`, `none`), or set the session's own toggle (`on`, `off`, `auto`); usage otherwise.
#[test]
fn interactive_commands_persona_reports_and_writes_the_setting() {
    let qol = interactive::qol::QolSettings {
        overlays: true,
        ..interactive::qol::QolSettings::parity()
    };
    let (mut app, c) = app_with(qol, vec![model("a", true)]);
    submit(&mut app, "/persona");
    assert_eq!(calls(&c), ["read_persona"]);
    app.apply(LaneEvent::Persona(Some("ultron".into())));
    assert!(screen(&mut app).contains("Persona is ultron: personality.md is in the system prompt"));
    app.apply(LaneEvent::Persona(None));
    assert!(screen(&mut app).contains("Persona is off (/persona ultron turns it on)"));
    app.apply(LaneEvent::Persona(Some("jarvis".into())));
    assert!(screen(&mut app).contains("Persona \"jarvis\" is not known, so it is off"));
    completed(&mut app, "/persona ultron");
    assert_eq!(calls(&c).last().unwrap(), "setting persona=\"ultron\"");
    assert!(screen(&mut app).contains("Persona ultron on from the next message"));
    completed(&mut app, "/persona none");
    assert_eq!(calls(&c).last().unwrap(), "setting persona=null");
    assert!(screen(&mut app).contains("Persona off from the next message"));
    // The session's own toggle goes to the session, which answers with a status line.
    submit(&mut app, "/persona ON");
    assert_eq!(calls(&c).last().unwrap(), "session persona on");
    // An argument the completion list has exactly is accepted first, then submitted.
    for (typed, call) in [
        ("/persona off", "session persona off"),
        ("/persona auto", "session persona auto"),
    ] {
        completed(&mut app, typed);
        assert_eq!(calls(&c).last().unwrap(), call);
    }
    app.apply(LaneEvent::Status(
        "Persona is off for this session; keep-going off".into(),
    ));
    assert!(screen(&mut app).contains("Persona is off for this session; keep-going off"));
    let before = calls(&c).len();
    submit(&mut app, "/persona jarvis");
    assert!(
        screen(&mut app).contains("Usage: /persona on|off|auto (this session) or ultron|none (the global setting)")
    );
    assert_eq!(calls(&c).len(), before);
    // The parity build has exactly the client TUI's commands.
    let (mut parity, pc) = app_with(interactive::qol::QolSettings::parity(), vec![model("a", true)]);
    submit(&mut parity, "/persona ultron");
    assert!(screen(&mut parity).contains("Unknown slash command: /persona"));
    assert!(calls(&pc).is_empty());
}

/// `/keepgoing [on|off]` (Rust-only, with the overlays) goes to the session; keep-going's nudges are one line each,
/// `keep-going (n/max)`, the last one `keep-going: limit reached, final report`.
#[test]
fn interactive_commands_keepgoing_goes_to_the_session_and_its_nudges_are_one_line() {
    let qol = interactive::qol::QolSettings {
        overlays: true,
        ..interactive::qol::QolSettings::parity()
    };
    let (mut tui, c) = app_with(qol, vec![model("a", true)]);
    submit(&mut tui, "/keepgoing");
    completed(&mut tui, "/keepgoing off");
    submit(&mut tui, "/keepgoing ON");
    assert_eq!(
        calls(&c),
        ["session keepgoing ", "session keepgoing off", "session keepgoing on"]
    );
    submit(&mut tui, "/keepgoing forever");
    assert!(screen(&mut tui).contains("Usage: /keepgoing on|off"));
    assert_eq!(calls(&c).len(), 3);
    // The session's one-line notice when a prompt starts keep-going mode.
    tui.apply(LaneEvent::Status(
        "keep-going on: \"keep going\" (/keepgoing off ends it)".into(),
    ));
    assert!(screen(&mut tui).contains("keep-going on: \"keep going\""));
    let nudge = |details: Value| {
        json!({"role": "custom", "customType": "ultron.nudge", "display": true,
               "content": "Keep going: the user is away and asked you to work until the task is done.",
               "details": details, "timestamp": 1})
    };
    let (mut live, _) = app();
    live.apply(LaneEvent::Message(nudge(
        json!({"nudge": 7, "max": 100, "keepGoing": true, "final": false}),
    )));
    live.apply(LaneEvent::Message(nudge(
        json!({"nudge": 101, "max": 100, "keepGoing": true, "final": true}),
    )));
    let s = screen(&mut live);
    assert!(s.contains("keep-going (7/100)"), "{s}");
    assert!(s.contains("keep-going: limit reached, final report"), "{s}");
    assert!(!s.contains("the user is away") && !s.contains("auto-continue"), "{s}");
    let (mut parity, pc) = app_with(interactive::qol::QolSettings::parity(), vec![model("a", true)]);
    submit(&mut parity, "/keepgoing off");
    assert!(screen(&mut parity).contains("Unknown slash command: /keepgoing"));
    assert!(calls(&pc).is_empty());
}

/// The persona's auto-nudge is one line, `auto-continue (n/5)`, live and in a reopened transcript, never the nudge
/// text as a user bubble.
#[test]
fn interactive_commands_persona_nudge_is_one_auto_continue_line() {
    let nudge = |n: u64| {
        json!({"role": "custom", "customType": "ultron.nudge", "display": true,
               "content": "Continue: take that next step now. End the turn only when the work is done and verified, or when you need the user; then ask one direct question.",
               "details": {"nudge": n, "max": 5}, "timestamp": 1})
    };
    let (mut live, _) = app();
    live.apply(LaneEvent::Message(nudge(2)));
    let s = screen(&mut live);
    assert!(s.contains("auto-continue (2/5)"), "{s}");
    assert!(!s.contains("take that next step"), "{s}");
    assert!(!s.contains("[ultron.nudge]"), "{s}");
    let (mut reopened, _) = app();
    reopened.apply(LaneEvent::Opened(SessionOpened {
        session_id: "s1".into(),
        session_file: None,
        name: None,
        messages: vec![json!({"role": "user", "content": "fix it", "timestamp": 1}), nudge(1)],
    }));
    let s = screen(&mut reopened);
    assert!(s.contains("fix it") && s.contains("auto-continue (1/5)"), "{s}");
    assert!(!s.contains("take that next step"), "{s}");
}

/// `/schedules` (Rust-only, with the overlays) asks the session for its scheduled prompts as `/schedules` lines; a
/// fired schedule's prompt is one line, `scheduled: <name> (run n)`, live and in a reopened transcript, never its
/// prompt as a user bubble.
#[test]
fn interactive_commands_schedules_lists_and_a_fired_schedule_is_one_line() {
    let qol = interactive::qol::QolSettings {
        overlays: true,
        ..interactive::qol::QolSettings::parity()
    };
    let (mut tui, c) = app_with(qol, vec![model("a", true)]);
    submit(&mut tui, "/schedules");
    assert_eq!(calls(&c), [r#"inspect /schedules schedule.list {"text":true}"#]);
    let (mut parity, pc) = app_with(interactive::qol::QolSettings::parity(), vec![model("a", true)]);
    submit(&mut parity, "/schedules");
    assert!(screen(&mut parity).contains("Unknown slash command: /schedules"));
    assert!(calls(&pc).is_empty());

    let fired = |run: u64| {
        json!({"role": "custom", "customType": "ultron.schedule", "display": true, "content": "check the build",
               "details": {"name": "build", "id": "sch-1a2b3c4d", "run": run}, "timestamp": 1})
    };
    let (mut live, _) = app();
    live.apply(LaneEvent::Message(fired(3)));
    let s = screen(&mut live);
    assert!(s.contains("scheduled: build (run 3)"), "{s}");
    assert!(
        !s.contains("check the build") && !s.contains("[ultron.schedule]"),
        "{s}"
    );
    let (mut reopened, _) = app();
    reopened.apply(LaneEvent::Opened(SessionOpened {
        session_id: "s1".into(),
        session_file: None,
        name: None,
        messages: vec![json!({"role": "user", "content": "watch it", "timestamp": 1}), fired(1)],
    }));
    let s = screen(&mut reopened);
    assert!(s.contains("watch it") && s.contains("scheduled: build (run 1)"), "{s}");
    assert!(!s.contains("check the build"), "{s}");
}
