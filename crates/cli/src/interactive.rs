//! `ultron` with no prompt on a terminal: the interactive TUI (crates/interactive) as a client of the session it
//! shows, like TypeScript's client TUI (experimental/client-tui.ts): this process's server generation, the session
//! attached through a [`Link`] (the session runs in its worker: lane, host stack, format-4 journal, names), its
//! transcript's lane-watch events turned into the TUI's [`LaneEvent`]s, and every TUI command a session operation.
//!
//! The UI thread owns the app and the terminal and never waits on the session: [`DaemonLane`] (the TUI's
//! [`LaneClient`]) hands commands to a link thread, which owns the [`Link`] and answers through UI events.

use crate::Env;
use crate::args::Args;
use crate::link::Link;
use crate::print_mode::Target;
use interactive::app::{App, AppOptions};
use interactive::history::{PROMPT_HISTORY_FILE, PromptHistoryStore};
use interactive::lane::{
    LaneClient, LaneEvent, ModelInfo, PromptImage, QueueKind, Queued, SessionListing, SessionOpened,
};
use interactive::run::{UiEvent, run_stdout, spawn_input};
use serde_json::{Value, json};
use session_core::core::{Op, QueueKind as CoreQueue};
use session_file::Env as _;
use std::io::{BufRead, IsTerminal, Write};
use std::rc::Rc;
use std::sync::{Arc, mpsc};
use tui::keybindings::{KeybindingsManager, Platform, app_keybindings};
use tui::theme::{ColorMode, builtin_theme_json, create_theme};

fn uuid() -> String {
    session_file::SystemEnv.random_uuid()
}

/// What the TUI asks of the link thread.
enum Command {
    Op(Op),
    /// Start a turn, then report the session's (automatic) name.
    Prompt(Op),
    NewSession,
    Resume(String),
    ListSessions,
    SelectModel(String, String),
    /// The run ended: report the session's name (a model title may have replaced the automatic one).
    RefreshName,
    Shutdown,
}

/// The TUI's [`LaneClient`] over the link thread.
struct DaemonLane {
    commands: mpsc::Sender<Command>,
}

impl DaemonLane {
    fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }

    fn enqueue(&self, kind: CoreQueue, text: &str) -> String {
        let entry_id = uuid();
        self.send(Command::Op(Op::Enqueue {
            kind,
            message: text.to_string(),
            images: Vec::new(),
            entry_id: Some(entry_id.clone()),
        }));
        entry_id
    }
}

impl LaneClient for DaemonLane {
    fn prompt(&self, text: &str) {
        self.prompt_with_images(text, Vec::new());
    }
    fn prompt_with_images(&self, text: &str, images: Vec<PromptImage>) {
        let images = images
            .into_iter()
            .map(|i| json!({"type": "image", "data": i.data, "mimeType": i.mime_type}))
            .collect();
        self.send(Command::Prompt(Op::Prompt {
            message: text.to_string(),
            images,
            queue: None,
        }));
    }
    fn steer(&self, text: &str) -> String {
        self.enqueue(CoreQueue::Steer, text)
    }
    fn follow_up(&self, text: &str) -> String {
        self.enqueue(CoreQueue::FollowUp, text)
    }
    fn cancel_queued(&self, entry_id: &str) {
        self.send(Command::Op(Op::CancelQueued {
            entry_id: entry_id.to_string(),
        }));
    }
    fn abort(&self) {
        self.send(Command::Op(Op::Abort));
    }
    fn new_session(&self) {
        self.send(Command::NewSession);
    }
    fn resume(&self, path: &str) {
        self.send(Command::Resume(path.to_string()));
    }
    fn list_sessions(&self) {
        self.send(Command::ListSessions);
    }
    fn set_name(&self, name: &str) {
        self.send(Command::Op(Op::SetName { name: name.to_string() }));
    }
    fn select_model(&self, provider: &str, id: &str) {
        self.send(Command::SelectModel(provider.to_string(), id.to_string()));
    }
}

fn model_info(model: &Value) -> ModelInfo {
    ModelInfo {
        provider: model["provider"].as_str().unwrap_or_default().to_string(),
        id: model["id"].as_str().unwrap_or_default().to_string(),
        name: model["name"].as_str().unwrap_or_default().to_string(),
        reasoning: model["reasoning"] == true,
        context_window: model["contextWindow"].as_u64().unwrap_or(0),
    }
}

fn catalog_info(m: &ai::Model) -> ModelInfo {
    ModelInfo {
        provider: m.provider.clone(),
        id: m.id.clone(),
        name: m.name.clone(),
        reasoning: m.reasoning,
        context_window: m.context_window,
    }
}

/// The TUI events a lane-watch event is (`None` for the ones it does not show).
pub fn tui_events(event: &Value) -> Vec<LaneEvent> {
    let text = |key: &str| event[key].as_str().unwrap_or_default().to_string();
    match event["type"].as_str() {
        Some("run_start") => vec![LaneEvent::OperationStarted(text("runId"))],
        Some("message_start" | "message_update") if event["message"]["role"] == "assistant" => {
            vec![LaneEvent::Streaming(event["message"].clone())]
        }
        Some("message_end") => vec![LaneEvent::Message(event["message"].clone())],
        Some("tool_start") => vec![LaneEvent::ToolStarted {
            id: text("toolCallId"),
            name: text("toolName"),
            args: event["args"].clone(),
        }],
        Some("tool_end") => vec![LaneEvent::ToolEnded { id: text("toolCallId") }],
        Some("queue_update") => vec![LaneEvent::Queues(
            event["queues"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|item| item["type"] == "message")
                .map(|item| Queued {
                    entry_id: item["entryId"].as_str().unwrap_or_default().to_string(),
                    kind: if item["kind"] == "steer" {
                        QueueKind::Steer
                    } else {
                        QueueKind::FollowUp
                    },
                    text: session_core::view::message_text(&item["message"]),
                })
                .collect(),
        )],
        Some("session_name") => vec![LaneEvent::Name(event["name"].as_str().map(str::to_string))],
        Some("run_end") => vec![LaneEvent::OperationEnded {
            // An aborted run is not a failure the TUI reports.
            error: (event["status"] == "failed")
                .then(|| event["error"]["message"].as_str().map(str::to_string))
                .flatten(),
        }],
        _ => Vec::new(),
    }
}

/// The session the link is attached to, as the TUI shows it after it opened.
fn opened(link: &Link) -> Result<(SessionOpened, ModelInfo), String> {
    let described = link.op(&Op::Describe)?.unwrap_or_default();
    Ok((
        SessionOpened {
            session_id: described["sessionId"].as_str().unwrap_or_default().to_string(),
            session_file: described["sessionFile"].as_str().map(str::to_string),
            name: described["name"].as_str().map(str::to_string),
            messages: described["messages"].as_array().cloned().unwrap_or_default(),
        },
        model_info(&described["model"]),
    ))
}

fn listings(value: Value) -> Vec<SessionListing> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .map(|s| SessionListing {
            path: s["path"].as_str().unwrap_or_default().to_string(),
            id: s["id"].as_str().unwrap_or_default().to_string(),
            name: s["name"].as_str().map(str::to_string),
            modified: s["modified"].as_i64().unwrap_or(0),
            message_count: s["messageCount"].as_u64().unwrap_or(0) as usize,
            first_message: s["firstMessage"].as_str().unwrap_or_default().to_string(),
        })
        .collect()
}

/// The link thread: owns the link, runs the TUI's commands one at a time.
fn link_thread(mut link: Link, commands: mpsc::Receiver<Command>, ui: mpsc::Sender<UiEvent>, no_session: bool) {
    let send = |event: LaneEvent| {
        let _ = ui.send(UiEvent::Lane(event));
    };
    let mut name: Option<String> = None;
    let report_name = |link: &Link, name: &mut Option<String>| {
        let current = link
            .op(&Op::Describe)
            .ok()
            .flatten()
            .and_then(|d| d["name"].as_str().map(str::to_string));
        if current != *name {
            *name = current.clone();
            send(LaneEvent::Name(current));
        }
    };
    let open = |link: &Link, name: &mut Option<String>| match opened(link) {
        Ok((session, model)) => {
            *name = session.name.clone();
            send(LaneEvent::Opened(session));
            send(LaneEvent::Model(model));
        }
        Err(error) => send(LaneEvent::Error(error)),
    };
    open(&link, &mut name);
    for command in commands {
        match command {
            Command::Op(op) => {
                if let Err(error) = link.op(&op) {
                    send(LaneEvent::Error(error));
                }
            }
            Command::Prompt(op) => match link.op(&op) {
                // The worker names an unnamed session from its first prompt as the run starts.
                Ok(_) => report_name(&link, &mut name),
                Err(error) => send(LaneEvent::Error(error)),
            },
            Command::RefreshName => report_name(&link, &mut name),
            Command::NewSession => match link.create(json!({})).and_then(|id| link.attach(&id)) {
                Ok(()) => {
                    open(&link, &mut name);
                    send(LaneEvent::Status("✓ New session started".into()));
                }
                Err(error) => send(LaneEvent::Error(error)),
            },
            Command::Resume(path) => {
                let id = session_journal::repo::read_metadata(std::path::Path::new(&path)).map(|m| m.id);
                match id
                    .ok_or_else(|| format!("Not a session file: {path}"))
                    .and_then(|id| link.attach(&id))
                {
                    Ok(()) => open(&link, &mut name),
                    Err(error) => send(LaneEvent::Error(error)),
                }
            }
            Command::ListSessions => match link.op(&Op::ListSessions) {
                Ok(listed) => send(LaneEvent::Sessions(listings(listed.unwrap_or_default()))),
                Err(error) => send(LaneEvent::Error(error)),
            },
            Command::SelectModel(provider, model_id) => match link.op(&Op::SetModel { provider, model_id }) {
                Ok(model) => send(LaneEvent::Model(model_info(&model.unwrap_or_default()))),
                Err(error) => send(LaneEvent::Error(error)),
            },
            Command::Shutdown => break,
        }
    }
    // Exiting aborts a running turn; the worker commits what it has.
    let _ = link.op(&Op::Abort);
    link.close(no_session);
}

/// `selectInteractiveSession` for `ultron -r`: this directory's sessions as a numbered list (with their names), read
/// a number from stdin. One session is taken without asking.
pub fn pick_session(
    sessions: &[session_journal::names::SessionListing],
    out: &mut dyn Write,
    input: &mut dyn BufRead,
) -> Option<session_journal::repo::Metadata> {
    match sessions {
        [] => return None,
        [only] => return Some(only.metadata.clone()),
        _ => {}
    }
    let _ = writeln!(out, "Available sessions:");
    for (i, s) in sessions.iter().enumerate() {
        let name = s.name.as_deref().map(|n| format!("  {n}")).unwrap_or_default();
        let _ = writeln!(out, "  {}. {}{name}", i + 1, s.metadata.id);
    }
    let _ = write!(out, "Select a session number: ");
    let _ = out.flush();
    let mut answer = String::new();
    input.read_line(&mut answer).ok()?;
    let index = answer.trim().parse::<usize>().ok()?.checked_sub(1)?;
    sessions.get(index).map(|s| s.metadata.clone())
}

/// Whether stdin and stdout are a terminal (the TUI runs only then).
pub fn is_terminal() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// Run the interactive TUI on `target`'s session until the user exits.
pub fn run_interactive(parsed: &Args, env: &Env, target: Target) -> Result<i32, String> {
    if matches!(target, Target::New { .. } | Target::Ephemeral) {
        crate::print_mode::resolve_model(parsed, env)?;
    }
    let model = crate::print_mode::resolve_model(parsed, env).ok();
    let models: Vec<ModelInfo> = session_core::lane::available_models(env)
        .iter()
        .map(catalog_info)
        .collect();
    let splash =
        !(env.var)("ULTRON_SPLASH").is_some_and(|v| matches!(v.trim().to_lowercase().as_str(), "off" | "0" | "false"));
    let settings = config::settings::SettingsManager::create(&env.cwd, &env.agent_dir(), false);
    let qol = interactive::qol::from_settings(&Value::Object(settings.settings().clone()), &|k| (env.var)(k));
    let caps = tui::image::detect_capabilities(&|k| (env.var)(k), false, false);
    let theme_name = settings.theme().unwrap_or_else(|| "dark".into());
    let theme_json = builtin_theme_json(&theme_name)
        .or_else(|| builtin_theme_json("dark"))
        .ok_or("the dark theme is missing")?;
    let theme = Rc::new(create_theme(&theme_json, ColorMode::from_capabilities(&caps), None)?);
    let keys = || {
        KeybindingsManager::new(
            app_keybindings(Platform::new("linux", false)),
            Vec::new(),
            Default::default(),
        )
    };
    let fd_path = std::env::split_paths(&(env.var)("PATH").unwrap_or_default())
        .map(|dir| dir.join("fd"))
        .find(|p| p.is_file());

    let (ui_tx, ui_rx) = mpsc::channel::<UiEvent>();
    let (command_tx, command_rx) = mpsc::channel::<Command>();
    let watch = {
        let (ui, commands) = (ui_tx.clone(), command_tx.clone());
        move |event: Value| {
            if event["type"] == "run_end" {
                let _ = commands.send(Command::RefreshName);
            }
            for lane_event in tui_events(&event) {
                let _ = ui.send(UiEvent::Lane(lane_event));
            }
        }
    };
    let link = Link::open(parsed, env, &target, "interactive", Arc::new(watch))?;
    let server_id = link.connection.server_id.clone();
    let no_session = parsed.no_session;
    let link_worker = {
        let ui = ui_tx.clone();
        std::thread::spawn(move || link_thread(link, command_rx, ui, no_session))
    };
    let client = DaemonLane {
        commands: command_tx.clone(),
    };
    let model = model.map_or_else(
        || ModelInfo {
            provider: String::new(),
            id: String::new(),
            name: String::new(),
            reasoning: false,
            context_window: 0,
        },
        |m| catalog_info(&m),
    );
    let options = AppOptions {
        theme,
        keys: keys(),
        editor_keys: keys(),
        version: crate::version(),
        cwd: env.cwd.display().to_string(),
        home: Some(env.home.display().to_string()).filter(|h| !h.is_empty()),
        server_id,
        models,
        model,
        thinking_level: parsed.thinking.clone().unwrap_or_else(|| "medium".into()),
        splash,
        history: PromptHistoryStore::new(
            Some(env.agent_dir().join(PROMPT_HISTORY_FILE)),
            &env.cwd.display().to_string(),
        ),
        fd_path,
        highlight: Rc::new(|code: &str| code.split('\n').map(str::to_string).collect()),
        qol,
    };
    let mut app = App::new(options, Box::new(client));
    spawn_input(ui_tx);
    let result = run_stdout(&mut app, &ui_rx, &tui::terminal::size);
    let _ = command_tx.send(Command::Shutdown);
    // The link thread detaches and stops the server; give it a moment.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !link_worker.is_finished() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    result.map_err(|e| e.to_string())?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listing(id: &str, name: Option<&str>) -> session_journal::names::SessionListing {
        session_journal::names::SessionListing {
            metadata: session_journal::repo::Metadata {
                id: id.into(),
                created_at: 0,
                storage_version: 1,
                cwd: "/c".into(),
                path: format!("/s/{id}.jsonl").into(),
                modified_at: 0.0,
                parent_session_id: None,
                legacy_parent_session_path: None,
            },
            name: name.map(str::to_string),
            automatic_name: false,
            first_message: "hi".into(),
            message_count: 1,
        }
    }

    #[test]
    fn cli_resume_picker_lists_names_and_reads_a_number() {
        let sessions = [listing("a1", Some("Fix the parser")), listing("b2", None)];
        let mut out = Vec::new();
        let picked = pick_session(&sessions, &mut out, &mut "2\n".as_bytes());
        assert_eq!(picked.map(|m| m.id).as_deref(), Some("b2"));
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "Available sessions:\n  1. a1  Fix the parser\n  2. b2\nSelect a session number: "
        );
        assert_eq!(pick_session(&sessions, &mut Vec::new(), &mut "x\n".as_bytes()), None);
        assert_eq!(
            pick_session(&sessions[..1], &mut Vec::new(), &mut "".as_bytes())
                .map(|m| m.id)
                .as_deref(),
            Some("a1")
        );
    }

    #[test]
    fn cli_interactive_watch_events_become_tui_events() {
        let queued = json!({"type": "queue_update", "lane": "main", "queues": [
            {"type": "message", "kind": "steer", "entryId": "e1", "message": {"role": "user", "content": [{"type": "text", "text": "go"}]}}
        ]});
        assert_eq!(
            tui_events(&queued),
            [LaneEvent::Queues(vec![Queued {
                entry_id: "e1".into(),
                kind: QueueKind::Steer,
                text: "go".into()
            }])]
        );
        let failed = json!({"type": "run_end", "status": "failed", "error": {"message": "boom"}});
        assert_eq!(
            tui_events(&failed),
            [LaneEvent::OperationEnded {
                error: Some("boom".into())
            }]
        );
        let aborted = json!({"type": "run_end", "status": "aborted"});
        assert_eq!(tui_events(&aborted), [LaneEvent::OperationEnded { error: None }]);
        let user = json!({"type": "message_start", "message": {"role": "user"}});
        assert!(tui_events(&user).is_empty());
    }
}
