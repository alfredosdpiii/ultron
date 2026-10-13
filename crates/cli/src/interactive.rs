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
    BashResult, ForkMessage, LaneClient, LaneEvent, ModelInfo, PromptImage, QueueKind, Queued, SessionListing,
    SessionOpened,
};
use interactive::run::{UiEvent, run_stdout, spawn_gated_input};
use serde_json::{Value, json};
use session_core::core::{Op, QueueKind as CoreQueue};
use session_file::Env as _;
use std::io::{BufRead, IsTerminal, Write};
use std::rc::Rc;
use std::sync::{Arc, mpsc};
use tui::keybindings::{KeybindingsManager, Platform};
use tui::qol_keys::native_app_keybindings;
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
    Bash(String, bool),
    Compact(Option<String>),
    SetThinking(String),
    ForkMessages,
    Fork(String),
    Clone,
    Export(Option<String>),
    Usage,
    SaveSetting(String, Value),
    Agents,
    KillAgent(String),
    ReadSettings,
    RlmStatus,
    Credentials,
    Logout(String, String),
    Changelog,
    Share,
    LoginProviders(String),
    SaveApiKey(String, String),
    DeleteSession(String),
    RenameSession(String, String),
    Tree,
    Navigate(Option<String>, bool, Option<String>),
    SetLabel(String, Option<String>),
    Shutdown,
}

/// Pi `ImageContent` blocks.
fn image_values(images: Vec<PromptImage>) -> Vec<Value> {
    images
        .into_iter()
        .map(|i| json!({"type": "image", "data": i.data, "mimeType": i.mime_type}))
        .collect()
}

/// The TUI's [`LaneClient`] over the link thread.
struct DaemonLane {
    commands: mpsc::Sender<Command>,
}

impl DaemonLane {
    fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }

    fn enqueue(&self, kind: CoreQueue, text: &str, images: Vec<PromptImage>) -> String {
        let entry_id = uuid();
        self.send(Command::Op(Op::Enqueue {
            kind,
            message: text.to_string(),
            images: image_values(images),
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
        self.send(Command::Prompt(Op::Prompt {
            message: text.to_string(),
            images: image_values(images),
            queue: None,
        }));
    }
    fn steer(&self, text: &str) -> String {
        self.enqueue(CoreQueue::Steer, text, Vec::new())
    }
    fn follow_up(&self, text: &str) -> String {
        self.enqueue(CoreQueue::FollowUp, text, Vec::new())
    }
    fn steer_with_images(&self, text: &str, images: Vec<PromptImage>) -> String {
        self.enqueue(CoreQueue::Steer, text, images)
    }
    fn follow_up_with_images(&self, text: &str, images: Vec<PromptImage>) -> String {
        self.enqueue(CoreQueue::FollowUp, text, images)
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
    fn bash(&self, command: &str, exclude: bool) {
        self.send(Command::Bash(command.to_string(), exclude));
    }
    fn abort_bash(&self) {
        self.send(Command::Op(Op::AbortBash));
    }
    fn compact(&self, instructions: Option<&str>) {
        self.send(Command::Compact(instructions.map(str::to_string)));
    }
    fn set_thinking(&self, level: &str) {
        self.send(Command::SetThinking(level.to_string()));
    }
    fn fork_messages(&self) {
        self.send(Command::ForkMessages);
    }
    fn fork(&self, entry_id: &str) {
        self.send(Command::Fork(entry_id.to_string()));
    }
    fn clone_session(&self) {
        self.send(Command::Clone);
    }
    fn export(&self, path: Option<&str>) {
        self.send(Command::Export(path.map(str::to_string)));
    }
    fn usage_report(&self) {
        self.send(Command::Usage);
    }
    fn save_setting(&self, key: &str, value: Value) {
        self.send(Command::SaveSetting(key.to_string(), value));
    }
    fn agents(&self) {
        self.send(Command::Agents);
    }
    fn read_settings(&self) {
        self.send(Command::ReadSettings);
    }
    fn rlm_status(&self) {
        self.send(Command::RlmStatus);
    }
    fn credentials(&self) {
        self.send(Command::Credentials);
    }
    fn changelog(&self) {
        self.send(Command::Changelog);
    }
    fn login_providers(&self, query: &str) {
        self.send(Command::LoginProviders(query.to_string()));
    }
    fn save_api_key(&self, provider: &str, key: &str) {
        self.send(Command::SaveApiKey(provider.to_string(), key.to_string()));
    }
    fn share(&self) {
        self.send(Command::Share);
    }
    fn logout(&self, provider: &str, done: &str) {
        self.send(Command::Logout(provider.to_string(), done.to_string()));
    }
    fn tree(&self) {
        self.send(Command::Tree);
    }
    fn delete_session(&self, path: &str) {
        self.send(Command::DeleteSession(path.to_string()));
    }
    fn rename_session(&self, path: &str, name: &str) {
        self.send(Command::RenameSession(path.to_string(), name.to_string()));
    }
    fn navigate(&self, target: Option<&str>, summarize: bool, instructions: Option<&str>) {
        self.send(Command::Navigate(
            target.map(str::to_string),
            summarize,
            instructions.map(str::to_string),
        ));
    }
    fn set_label(&self, entry_id: &str, label: Option<&str>) {
        self.send(Command::SetLabel(entry_id.to_string(), label.map(str::to_string)));
    }
    fn kill_agent(&self, id: &str) {
        self.send(Command::KillAgent(id.to_string()));
    }
}

/// The agent hub's rows from `rlm.list_subagents` (`{id, definition, state, parentId?, result?}`): running children
/// run, finished ones are idle, failed or cancelled ones aborted.
pub fn agent_views(listed: &Value) -> Vec<interactive::agent_hub::AgentView> {
    use interactive::agent_hub::{AgentKind, AgentStatus, AgentView};
    listed["subagents"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|a| AgentView {
            id: a["id"].as_str().unwrap_or_default().to_string(),
            kind: AgentKind::Sub,
            parent_id: a["parentId"].as_str().map(str::to_string),
            status: match a["state"].as_str() {
                Some("running" | "pending" | "queued") => AgentStatus::Running,
                Some("completed" | "succeeded") => AgentStatus::Idle,
                Some("failed" | "cancelled" | "interrupted") => AgentStatus::Aborted,
                _ => AgentStatus::Parked,
            },
            unread: 0,
            task: a["result"]["summary"].as_str().map(str::to_string),
            last_activity: 0,
            model: None,
            thinking: None,
            metrics: None,
        })
        .collect()
}

fn model_info(model: &Value) -> ModelInfo {
    if let Ok(model) = serde_json::from_value::<ai::Model>(model.clone()) {
        return catalog_info(&model);
    }
    ModelInfo {
        provider: model["provider"].as_str().unwrap_or_default().to_string(),
        id: model["id"].as_str().unwrap_or_default().to_string(),
        name: model["name"].as_str().unwrap_or_default().to_string(),
        reasoning: model["reasoning"] == true,
        context_window: model["contextWindow"].as_u64().unwrap_or(0),
        thinking_levels: Vec::new(),
    }
}

fn catalog_info(m: &ai::Model) -> ModelInfo {
    ModelInfo {
        provider: m.provider.clone(),
        id: m.id.clone(),
        name: m.name.clone(),
        reasoning: m.reasoning,
        context_window: m.context_window,
        thinking_levels: ai::models::supported_thinking_levels(m)
            .into_iter()
            .map(str::to_string)
            .collect(),
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
        Some("compaction_start") => vec![LaneEvent::CompactionStarted],
        Some("entry_added") if event["entry"]["type"] == "compaction" => {
            vec![LaneEvent::Compacted(Ok(event["entry"].clone()))]
        }
        Some("config_update") if event["property"] == "thinkingLevel" => event["value"]
            .as_str()
            .map(|l| LaneEvent::Thinking(l.to_string()))
            .into_iter()
            .collect(),
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
fn opened(link: &Link) -> Result<(SessionOpened, ModelInfo, Vec<Value>), String> {
    let described = link.op(&Op::Describe)?.unwrap_or_default();
    Ok((
        SessionOpened {
            session_id: described["sessionId"].as_str().unwrap_or_default().to_string(),
            session_file: described["sessionFile"].as_str().map(str::to_string),
            name: described["name"].as_str().map(str::to_string),
            messages: described["messages"].as_array().cloned().unwrap_or_default(),
        },
        model_info(&described["model"]),
        described["entries"].as_array().cloned().unwrap_or_default(),
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
            cwd: s["cwd"].as_str().unwrap_or_default().to_string(),
        })
        .collect()
}

/// The link thread: owns the link, runs the TUI's commands one at a time.
fn link_thread(
    mut link: Link,
    commands: mpsc::Receiver<Command>,
    ui: mpsc::Sender<UiEvent>,
    no_session: bool,
    settings_path: (std::path::PathBuf, std::path::PathBuf),
    signalled: Arc<std::sync::atomic::AtomicI32>,
    catalog_env: Env,
) {
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
        Ok((session, model, entries)) => {
            *name = session.name.clone();
            send(LaneEvent::Opened(session));
            send(LaneEvent::Entries(entries));
            send(LaneEvent::Model(model));
        }
        Err(error) => send(LaneEvent::Error(error)),
    };
    open(&link, &mut name);
    let mut last_recovery: Option<std::time::Instant> = None;
    let mut gave_up = false;
    loop {
        let command = match commands.recv_timeout(std::time::Duration::from_millis(250)) {
            Ok(command) => command,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // The worker went away without being asked to (it crashed or was killed): the session file is
                // intact, so attach again, which starts a fresh worker (`#handleLocalAttachmentState`).
                if link.connection().attachment().is_none() {
                    let now = std::time::Instant::now();
                    if last_recovery.is_some_and(|t| now.duration_since(t) < WORKER_RECOVERY_INTERVAL) {
                        if !gave_up {
                            gave_up = true;
                            send(LaneEvent::Status(format!(
                                "Error: The session worker stopped again; {}",
                                daemon::lost::RESUME_HINT
                            )));
                        }
                        continue;
                    }
                    last_recovery = Some(now);
                    gave_up = false;
                    send(LaneEvent::Status(
                        "The session worker stopped unexpectedly; reattaching the Session…".into(),
                    ));
                    let id = link.session_id();
                    match link.attach(&id) {
                        Ok(()) => {
                            open(&link, &mut name);
                            send(LaneEvent::Status(
                                "The session worker stopped unexpectedly and was restarted; the Session is intact, but work running in the old worker (such as background jobs) was lost.".into(),
                            ));
                        }
                        Err(error) => send(LaneEvent::Status(format!("Error: {error}"))),
                    }
                }
                continue;
            }
        };
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
                    Ok(()) => {
                        open(&link, &mut name);
                        send(LaneEvent::Status("Resumed session".into()));
                    }
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
            Command::Bash(command, exclude) => {
                // Answered on its own thread, so Esc (abortBash) reaches the worker while it runs.
                let started = start_call(&link, "ultron.session-control", "bash", json!([command, exclude]));
                let ui = ui.clone();
                std::thread::spawn(move || {
                    let result = started
                        .and_then(|(connection, request)| connection.wait(&request).map_err(|e| e.to_string()))
                        .map(|value| bash_result(&value.unwrap_or_default()));
                    let _ = ui.send(UiEvent::Lane(LaneEvent::BashDone(result)));
                });
            }
            Command::Compact(instructions) => {
                let started = start_call(
                    &link,
                    "pi.agent-controller",
                    "compact",
                    json!([{"customInstructions": instructions}]),
                );
                let ui = ui.clone();
                std::thread::spawn(move || {
                    // Success shows through the lane's `entry_added` of the compaction entry.
                    let result =
                        started.and_then(|(connection, request)| connection.wait(&request).map_err(|e| e.to_string()));
                    if let Err(error) = result {
                        let error = daemon::lost::describe(&error).unwrap_or(error);
                        let error = error
                            .strip_prefix("session_operation_failed: ")
                            .map(str::to_string)
                            .unwrap_or(error);
                        let _ = ui.send(UiEvent::Lane(LaneEvent::Compacted(Err(error))));
                    }
                });
            }
            Command::SetThinking(level) => {
                if let Err(error) = link.op(&Op::SetThinking { level }) {
                    send(LaneEvent::Error(error));
                }
            }
            Command::ForkMessages => match link.op(&Op::ForkMessages) {
                Ok(listed) => send(LaneEvent::ForkMessages(
                    listed
                        .unwrap_or_default()
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|m| ForkMessage {
                            entry_id: m["entryId"].as_str().unwrap_or_default().to_string(),
                            text: m["text"].as_str().unwrap_or_default().to_string(),
                        })
                        .collect(),
                )),
                Err(error) => send(LaneEvent::Error(error)),
            },
            Command::Fork(entry_id) => {
                let text = link
                    .op(&Op::ForkSource {
                        entry_id: entry_id.clone(),
                    })
                    .map(|t| t.and_then(|t| t.as_str().map(str::to_string)).unwrap_or_default());
                let forked = text.and_then(|text| {
                    fork_to(&mut link, json!({"entryId": entry_id, "position": "before"})).map(|()| text)
                });
                match forked {
                    Ok(text) => {
                        open(&link, &mut name);
                        send(LaneEvent::Forked {
                            editor_text: text,
                            status: "Forked to new session".into(),
                        });
                    }
                    Err(error) => send(LaneEvent::Status(format!("Error: {error}"))),
                }
            }
            Command::Clone => {
                let leaf = link
                    .op(&Op::Entries { since: None })
                    .map(|e| e.unwrap_or_default()["leafId"].clone());
                match leaf {
                    Ok(Value::Null) => send(LaneEvent::Status("Nothing to clone yet".into())),
                    Ok(_) => match fork_to(&mut link, json!({"position": "at"})) {
                        Ok(()) => {
                            open(&link, &mut name);
                            send(LaneEvent::Forked {
                                editor_text: String::new(),
                                status: "Cloned to new session".into(),
                            });
                        }
                        Err(error) => send(LaneEvent::Status(format!("Error: {error}"))),
                    },
                    Err(error) => send(LaneEvent::Status(format!("Error: {error}"))),
                }
            }
            Command::Export(path) => send(LaneEvent::Status(match export_session(&link, path.as_deref()) {
                Ok(path) => format!("Session exported to: {path}"),
                Err(error) => format!("Error: Failed to export session: {error}"),
            })),
            Command::Usage => match usage_report(&link) {
                Ok(text) => send(LaneEvent::Notice(text)),
                Err(error) => send(LaneEvent::Status(format!("Error: {error}"))),
            },
            Command::DeleteSession(path) => {
                // Through the server, which closes the session's worker first (client-tui-sessions.ts).
                let id = session_journal::repo::read_metadata(std::path::Path::new(&path)).map(|m| m.id);
                let result = match id {
                    Some(id) if id == link.session_id() => Err("Cannot delete the currently active session".into()),
                    Some(id) => link.remove(&id).map_err(|e| format!("Failed to delete: {e}")),
                    None => Ok(()),
                };
                send(LaneEvent::SessionDeleted(result));
                if let Ok(listed) = link.op(&Op::ListSessions) {
                    send(LaneEvent::Sessions(listings(listed.unwrap_or_default())));
                }
            }
            Command::RenameSession(path, new_name) => {
                let id = session_journal::repo::read_metadata(std::path::Path::new(&path)).map(|m| m.id);
                let renamed = match id {
                    Some(id) if id == link.session_id() => link.op(&Op::SetName { name: new_name }).map(|_| ()),
                    Some(id) => link.rename(&id, &new_name),
                    None => Ok(()),
                };
                if let Err(error) = renamed {
                    send(LaneEvent::Status(format!("Error: {error}")));
                }
                if let Ok(listed) = link.op(&Op::ListSessions) {
                    send(LaneEvent::Sessions(listings(listed.unwrap_or_default())));
                }
            }
            Command::Tree => match link.op(&Op::Tree) {
                Ok(read) => {
                    let read = read.unwrap_or_default();
                    send(LaneEvent::Tree {
                        tree: read["tree"].clone(),
                        leaf: read["leafId"].as_str().map(str::to_string),
                    });
                }
                Err(error) => send(LaneEvent::Status(format!("Error: {error}"))),
            },
            Command::Navigate(target, summarize, instructions) => {
                // The agent controller's navigate runs on its own worker thread (a summary is a model call).
                let started = start_call(
                    &link,
                    "pi.agent-controller",
                    "navigate",
                    json!([{"targetId": target, "summarize": summarize, "label": null, "customInstructions": instructions}]),
                );
                let result = started
                    .and_then(|(connection, request)| connection.wait(&request).map_err(|e| e.to_string()))
                    .map_err(|error| {
                        let error = daemon::lost::describe(&error).unwrap_or(error);
                        error
                            .strip_prefix("session_operation_failed: ")
                            .map(str::to_string)
                            .unwrap_or(error)
                    });
                if result.is_ok() {
                    // The transcript is the new position's path.
                    open(&link, &mut name);
                }
                send(LaneEvent::Navigated(result.map(|_| ())));
            }
            Command::SetLabel(entry_id, label) => {
                if let Err(error) = link.op(&Op::SetLabel { entry_id, label }) {
                    send(LaneEvent::Status(format!("Error: {error}")));
                }
            }
            Command::RlmStatus => {
                if let Ok(listed) = link.op(&Op::Agents) {
                    send(LaneEvent::RlmStatus(listed.unwrap_or_default()));
                }
            }
            Command::Agents => match link.op(&Op::Agents) {
                Ok(listed) => send(LaneEvent::Agents(agent_views(&listed.unwrap_or_default()))),
                Err(error) => send(LaneEvent::Status(format!("Error: {error}"))),
            },
            Command::KillAgent(id) => {
                if let Err(error) = link.op(&Op::KillAgent { id }) {
                    send(LaneEvent::Status(format!("Error: {error}")));
                }
            }
            Command::SaveSetting(key, value) => {
                let (cwd, agent_dir) = &settings_path;
                let mut settings = config::settings::SettingsManager::create(cwd, agent_dir, false);
                save_setting(&mut settings, &key, value.clone());
                // What the session applies live (worker-settings.ts): compaction and the queue modes.
                let live = match key.as_str() {
                    "compaction.enabled" => Some(Op::SetAutoCompaction(value == true)),
                    "steeringMode" | "followUpMode" => Some(Op::SetQueueMode {
                        kind: if key == "steeringMode" {
                            CoreQueue::Steer
                        } else {
                            CoreQueue::FollowUp
                        },
                        mode: value.as_str().unwrap_or("all").to_string(),
                    }),
                    _ => None,
                };
                if let Some(op) = live
                    && let Err(error) = link.op(&op)
                {
                    send(LaneEvent::Status(format!("Error: {error}")));
                }
            }
            Command::Changelog => {
                // `getChangelogPath()`: the package's CHANGELOG.md beside its docs. Lexing it takes a while, so on
                // its own thread: neither the UI nor the session's other commands wait for it.
                let path = catalog_env.docs.parent().map(|p| p.join("CHANGELOG.md"));
                let ui = ui.clone();
                std::thread::spawn(move || {
                    let content = path.and_then(|p| std::fs::read_to_string(p).ok());
                    let tokens = interactive::changelog::changelog_tokens(content.as_deref());
                    let _ = ui.send(UiEvent::Lane(LaneEvent::Changelog(tokens)));
                });
            }
            Command::Share => share_session(&link, &send),
            Command::LoginProviders(query) => {
                let auth = config::paths::AgentPaths::new(settings_path.1.clone()).auth();
                send(LaneEvent::LoginProviders {
                    providers: login_options(&catalog_env, &auth),
                    query,
                });
            }
            Command::SaveApiKey(provider, key) => {
                let auth = config::paths::AgentPaths::new(settings_path.1.clone()).auth();
                let name = interactive::selectors::provider_name(&provider);
                let saved = config::auth::AuthStorage::create(auth.clone())
                    .modify(&provider, |_| Some(json!({"type": "api_key", "key": key})));
                match saved {
                    Err(error) => send(LaneEvent::Status(format!(
                        "Error: Failed to save API key for {name}: {error}"
                    ))),
                    Ok(_) => {
                        let done = format!("Saved API key for {name}. Credentials saved to {}", auth.display());
                        send(LaneEvent::Status(format!(
                            "{done}. Reloading the Session's credentials…"
                        )));
                        // The session reads credentials per request; the catalog follows the providers.
                        send(LaneEvent::Catalog(catalog(&catalog_env)));
                        let model = link.op(&Op::Describe).ok().flatten().map(|d| model_info(&d["model"]));
                        let hint = match model {
                            None => format!(". Use /model to select one of {name}'s models"),
                            Some(m) if m.id.is_empty() => format!(". Use /model to select one of {name}'s models"),
                            Some(m) if m.provider == provider => String::new(),
                            Some(m) => format!(
                                ". The Session still uses {}/{}; /model switches to {name}'s models",
                                m.provider, m.id
                            ),
                        };
                        send(LaneEvent::Status(format!("{done}{hint}")));
                    }
                }
            }
            Command::Credentials => {
                let path = config::paths::AgentPaths::new(settings_path.1.clone()).auth();
                match config::auth::AuthStorage::create(path).list() {
                    Ok(stored) => send(LaneEvent::Credentials(
                        stored
                            .into_iter()
                            .map(|(id, c)| (id, c["type"].as_str().unwrap_or("api_key").to_string()))
                            .collect(),
                    )),
                    Err(error) => send(LaneEvent::Status(format!(
                        "Error: Could not read stored credentials: {error}"
                    ))),
                }
            }
            Command::Logout(provider, done) => {
                let path = config::paths::AgentPaths::new(settings_path.1.clone()).auth();
                match config::auth::AuthStorage::create(path).delete(&provider) {
                    Ok(()) => {
                        // The session reads credentials per request; the client's catalog follows the providers.
                        send(LaneEvent::Catalog(catalog(&catalog_env)));
                        send(LaneEvent::Status(done));
                    }
                    Err(error) => send(LaneEvent::Status(format!("Error: Logout failed: {error}"))),
                }
            }
            Command::ReadSettings => {
                let (cwd, agent_dir) = &settings_path;
                let settings = config::settings::SettingsManager::create(cwd, agent_dir, false);
                let env_overrides = [
                    ("rlm.frameModel", "ULTRON_RLM_FRAME_MODEL"),
                    ("review.model", "ULTRON_REVIEW_MODEL"),
                    ("claudeCode.frameModel", "ULTRON_CLAUDE_FRAME_MODEL"),
                    ("claudeCode.childModel", "ULTRON_CLAUDE_CHILD_MODEL"),
                    ("claudeCode.model", "ULTRON_CLAUDE_MODEL"),
                ]
                .into_iter()
                .filter_map(|(key, name)| {
                    let value = std::env::var(name).ok()?.trim().to_string();
                    let well_formed = key.starts_with("claudeCode.") && key != "claudeCode.frameModel"
                        || value
                            .split_once('/')
                            .is_some_and(|(p, m)| !p.is_empty() && !m.is_empty());
                    (!value.is_empty() && well_formed).then(|| (key.to_string(), (name.to_string(), value)))
                })
                .collect();
                let mut themes = vec!["dark".to_string(), "light".to_string()];
                if let Ok(dir) = std::fs::read_dir(agent_dir.join("themes")) {
                    let mut custom: Vec<String> = dir
                        .flatten()
                        .filter_map(|e| {
                            e.file_name()
                                .to_string_lossy()
                                .strip_suffix(".json")
                                .map(str::to_string)
                        })
                        .filter(|n| !themes.contains(n))
                        .collect();
                    custom.sort();
                    themes.extend(custom);
                }
                let terminal_theme =
                    match tui::theme::detect_terminal_background_from_env(std::env::var("COLORFGBG").ok().as_deref())
                        .theme
                    {
                        tui::theme::TerminalTheme::Light => "light",
                        tui::theme::TerminalTheme::Dark => "dark",
                    };
                send(LaneEvent::Settings {
                    settings: Value::Object(settings.settings().clone()),
                    env_overrides,
                    themes,
                    terminal_theme: terminal_theme.to_string(),
                });
            }
            Command::Shutdown => break,
        }
    }
    // Exiting aborts a running turn; the worker commits what it has. A signal only releases the session: a busy
    // worker keeps its work running detached (leaving the TUI on a signal is not an abort, as in TypeScript).
    if signalled.load(std::sync::atomic::Ordering::SeqCst) == 0 {
        let _ = link.op(&Op::Abort);
        link.close(no_session);
    } else {
        link.close(false);
    }
}

/// Save one setting the way the worker's `setSetting` does: dotted keys into their object, per-model thinking
/// levels into `modelThinkingLevels`, `null` removes.
fn save_setting(settings: &mut config::settings::SettingsManager, key: &str, value: Value) {
    if key == "modelThinkingLevel" {
        let provider = value["provider"].as_str().unwrap_or_default();
        let model = value["modelId"].as_str().unwrap_or_default();
        match value["level"].as_str() {
            Some(level) => settings.set_model_thinking_level(provider, model, level),
            None => settings.remove_model_thinking_level(provider, model),
        }
        return;
    }
    let stored = (!value.is_null()).then_some(value);
    match key.split_once('.') {
        Some((field, rest)) => settings.set_global_nested(field, rest, stored),
        None => settings.set_global(key, stored),
    }
}

/// Pi's `/share`: the session as HTML in a secret gist (`gh gist create --public=false`), then the viewer and gist
/// URLs.
fn share_session(link: &Link, send: &dyn Fn(LaneEvent)) {
    let status = |text: String| send(LaneEvent::Status(text));
    let auth = std::process::Command::new("gh")
        .args(["auth", "status"])
        .stdin(std::process::Stdio::null())
        .output();
    match auth {
        Err(_) => {
            return status("Error: GitHub CLI (gh) is not installed. Install it from https://cli.github.com/".into());
        }
        Ok(out) if !out.status.success() => {
            return status("Error: GitHub CLI is not logged in. Run 'gh auth login' first.".into());
        }
        Ok(_) => {}
    }
    let dir = std::env::temp_dir().join(format!("ultron-share-{}", uuid()));
    if let Err(error) = std::fs::create_dir_all(&dir) {
        return status(format!("Error: Failed to export session: {error}"));
    }
    let file = dir.join("session.html");
    let result = (|| {
        export_session(link, Some(&file.display().to_string()))
            .map_err(|e| format!("Error: Failed to export session: {e}"))?;
        status("Creating gist...".into());
        let out = std::process::Command::new("gh")
            .args(["gist", "create", "--public=false"])
            .arg(&file)
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| format!("Error: Failed to create gist: {e}"))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            return Err(format!(
                "Error: Failed to create gist: {}",
                if stderr.is_empty() {
                    "Unknown error".into()
                } else {
                    stderr
                }
            ));
        }
        let gist_url = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let gist_id = gist_url
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let Some(gist_id) = gist_id else {
            return Err("Error: Failed to parse gist ID from gh output".into());
        };
        let base = std::env::var("PI_SHARE_VIEWER_URL").unwrap_or_else(|_| "https://pi.dev/session/".into());
        let preview = format!("{base}#{gist_id}");
        Ok(format!(
            "Share URL: {}\nGist: {}",
            tui::image::hyperlink(&preview, &preview),
            tui::image::hyperlink(&gist_url, &gist_url)
        ))
    })();
    let _ = std::fs::remove_dir_all(&dir);
    status(result.unwrap_or_else(|e| e));
}

/// `loginProviderOptions`: every built-in provider's sign-in options with what is configured (a stored credential,
/// else the variables that supply a key), sorted by name.
fn login_options(env: &Env, auth: &std::path::Path) -> Vec<interactive::selectors::AuthProvider> {
    use interactive::selectors::{AuthProvider, locale_compare, provider_logins};
    let stored: std::collections::HashMap<String, String> = config::auth::AuthStorage::create(auth.to_path_buf())
        .list()
        .unwrap_or_default()
        .into_iter()
        .map(|(id, c)| (id, c["type"].as_str().unwrap_or("api_key").to_string()))
        .collect();
    let source = ai::env::EnvSource {
        lookup: &*env.var,
        exists: &|p: &std::path::Path| p.exists(),
        home: Some(env.home.clone()),
    };
    // Providers whose key prompt is not Pi's generic one (several fields or a choice first).
    const CUSTOM: [&str; 4] = [
        "amazon-bedrock",
        "cloudflare-ai-gateway",
        "cloudflare-workers-ai",
        "google-vertex",
    ];
    let mut options = Vec::new();
    for p in provider_logins() {
        let status = match stored.get(&p.id) {
            Some(kind) => Some((kind.clone(), Some("stored credential".to_string()))),
            None => ai::env::find_env_keys_in(&p.id, &source)
                .filter(|keys| !keys.is_empty())
                .map(|keys| ("api_key".to_string(), Some(keys.join(", ")))),
        };
        let base = AuthProvider {
            id: p.id.clone(),
            name: p.name.clone(),
            auth_type: String::new(),
            status,
            api_key_method: p.api_key.clone(),
            login_label: p.oauth.clone().flatten(),
            generic_login: !CUSTOM.contains(&p.id.as_str()),
        };
        if p.oauth.is_some() {
            options.push(AuthProvider {
                auth_type: "oauth".into(),
                ..base.clone()
            });
        }
        if p.api_key.is_some() {
            options.push(AuthProvider {
                auth_type: "api_key".into(),
                ..base
            });
        }
    }
    options.sort_by(|a, b| locale_compare(&a.name, &b.name));
    options
}

/// The models the providers with credentials make available.
fn catalog(env: &Env) -> Vec<ModelInfo> {
    session_core::lane::available_models(env)
        .iter()
        .map(catalog_info)
        .collect()
}

/// Start a long call of the attached session (one the worker runs on its own thread, so later calls such as
/// `abortBash` are not queued behind it) without waiting for it.
fn start_call(
    link: &Link,
    service: &str,
    member: &str,
    args: Value,
) -> Result<(Arc<client::runtime::Connection>, String), String> {
    let session = link.connection().session_target().map_err(|e| e.to_string())?;
    let request = link
        .connection()
        .start(session, service, member, args)
        .map_err(|e| e.to_string())?;
    Ok((link.connection(), request))
}

fn bash_result(value: &Value) -> BashResult {
    BashResult {
        output: value["output"].as_str().unwrap_or_default().to_string(),
        exit_code: value["exitCode"].as_i64(),
        cancelled: value["cancelled"] == true,
        truncated: value["truncated"] == true,
        full_output_path: value["fullOutputPath"].as_str().map(str::to_string),
    }
}

/// `#forkSession`: a new session from this one's `forkPath`, the fork recorded in it, then attached.
fn fork_to(link: &mut Link, fork_path: Value) -> Result<(), String> {
    let source = link.session_id();
    let created = link.create(json!({"forkFromSessionId": source, "forkPath": fork_path}))?;
    if let Some(file) = link.session_file(&created)? {
        link.op(&Op::Forked { target: file })?;
    }
    link.attach(&created)
}

/// Pi's `/export` (`exportSession`): HTML through the exporter, or the Pi-format JSONL, from the session's entries
/// as Pi entries; returns the path written.
fn export_session(link: &Link, output: Option<&str>) -> Result<String, String> {
    let read = link.op(&Op::Entries { since: None })?.unwrap_or_default();
    let described = link.op(&Op::Describe)?.unwrap_or_default();
    let session_id = link.session_id();
    let entries = read["entries"].as_array().cloned().unwrap_or_default();
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    // Pi entry times are already `toISOString()` text.
    let timestamp = entries.first().and_then(|e| e["timestamp"].as_str()).map_or_else(
        || session_file::js::iso_string(session_file::SystemEnv.now_ms()),
        str::to_string,
    );
    let header = json!({"type": "session", "version": 3, "id": session_id, "timestamp": timestamp,
        "cwd": cwd.display().to_string()});
    let stem = described["sessionFile"]
        .as_str()
        .and_then(|f| std::path::Path::new(f).file_name())
        .map(|n| {
            let name = n.to_string_lossy();
            match name.rfind('.') {
                Some(dot) if dot > 0 => name[..dot].to_string(),
                _ => name.into_owned(),
            }
        })
        .unwrap_or_else(|| session_id.clone());
    if let Some(path) = output.filter(|p| p.ends_with(".jsonl")) {
        let path = config::paths::normalize(&cwd.join(path));
        let lines: Vec<String> = std::iter::once(&header).chain(&entries).map(Value::to_string).collect();
        std::fs::write(&path, format!("{}\n", lines.join("\n"))).map_err(|e| e.to_string())?;
        return Ok(path.display().to_string());
    }
    let data = json!({"header": header, "entries": entries, "leafId": read["leafId"]});
    let html = crate::export_html::generate_html(&data, None);
    let path = config::paths::normalize(
        &cwd.join(output.map_or_else(|| format!("ultron-session-{stem}.html"), str::to_string)),
    );
    std::fs::write(&path, html).map_err(|e| e.to_string())?;
    Ok(path.display().to_string())
}

/// `/usage`: the session report of the session's file (`renderSessionReport`), as `ultron usage` prints it.
fn usage_report(link: &Link) -> Result<String, String> {
    let described = link.op(&Op::Describe)?.unwrap_or_default();
    let file = described["sessionFile"]
        .as_str()
        .ok_or("The Session worker returned no session report")?;
    let log = crate::usage::log::read_session_log(std::path::Path::new(file)).map_err(|e| e.message().to_string())?;
    let report = crate::usage::report::build_session_report(&log);
    let zone = crate::localtime::Zone::from_env(std::env::var("TZ").ok().as_deref());
    let home = std::env::var("HOME").unwrap_or_default();
    let options = crate::usage::text::TextOptions {
        utc: false,
        home: &home,
        zone: &zone,
    };
    Ok(crate::usage::text::render_session_report(&report, &options).join("\n"))
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

/// A second unexpected worker stop within this long is reported instead of recovered (`WORKER_RECOVERY_INTERVAL_MS`).
const WORKER_RECOVERY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// How long exit waits for the session's server to stop.
const SHUTDOWN_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Run the interactive TUI on `target`'s session until the user exits.
pub fn run_interactive(parsed: &Args, env: &Env, target: Target) -> Result<i32, String> {
    if matches!(target, Target::New { .. } | Target::Ephemeral) {
        crate::print_mode::resolve_model(parsed, env)?;
    }
    let model = crate::print_mode::resolve_model(parsed, env).ok();
    let models = catalog(env);
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
            native_app_keybindings(Platform::new("linux", false)),
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
    let server_id = link.connection().server_id.clone();
    let no_session = parsed.no_session;
    let signalled = Arc::new(std::sync::atomic::AtomicI32::new(0));
    let link_worker = {
        let ui = ui_tx.clone();
        let settings_path = (env.cwd.clone(), env.agent_dir());
        let signalled = signalled.clone();
        let catalog_env = env.clone();
        std::thread::spawn(move || link_thread(link, command_rx, ui, no_session, settings_path, signalled, catalog_env))
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
            thinking_levels: Vec::new(),
        },
        |m| catalog_info(&m),
    );
    let hide_thinking = settings.settings().get("hideThinkingBlock") == Some(&Value::Bool(true));
    let enabled_models: Vec<String> = settings
        .settings()
        .get("enabledModels")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
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
    // A signal leaves the TUI as closing its input does (the terminal is restored, the link closes), then the
    // process exits with the signal's code.
    let (code, end) = (signalled.clone(), std::sync::Mutex::new(ui_tx.clone()));
    daemon::signals::set_handler(Arc::new(move |signal| {
        code.store(daemon::signals::exit_code(signal), std::sync::atomic::Ordering::SeqCst);
        let _ = end.lock().unwrap_or_else(|e| e.into_inner()).send(UiEvent::InputClosed);
        std::thread::sleep(crate::print_mode::PROMPT_ABORT_WAIT);
        std::process::exit(daemon::signals::exit_code(signal));
    }));
    app.set_hide_thinking(hide_thinking);
    app.set_enabled_models(enabled_models);
    let tree_filter = settings
        .settings()
        .get("treeFilterMode")
        .and_then(Value::as_str)
        .unwrap_or("default")
        .to_string();
    let skip_summary = settings.branch_summary_settings()["skipPrompt"] == true;
    app.set_tree_settings(&tree_filter, skip_summary);
    app.set_auto_compact(settings.compaction_enabled() != false);
    // `getExternalEditorCommand`: the setting, else $VISUAL, else $EDITOR.
    let external_editor = settings
        .settings()
        .get("externalEditor")
        .and_then(Value::as_str)
        .filter(|c| !c.trim().is_empty())
        .map(str::to_string)
        .or_else(|| (env.var)("VISUAL").filter(|v| !v.is_empty()))
        .or_else(|| (env.var)("EDITOR").filter(|v| !v.is_empty()))
        .or_else(|| Some("nano".to_string()));
    app.set_external_editor(external_editor);
    spawn_gated_input(ui_tx, app.input_gate());
    let result = run_stdout(&mut app, &ui_rx, &tui::terminal::size);
    // Its prompt history is written before the process can exit.
    drop(app);
    let _ = command_tx.send(Command::Shutdown);
    // The link thread detaches and stops the server (its socket and record are removed, its worker shut down).
    // Exiting before that leaves a half-closed server in the instance directory that the next start in it has to
    // wait out (cli_interactive_prompt_history_survives_restart and cli_interactive_resume_lists_this_directory_with_names
    // restart in the same directory and timed out under load), so wait for it; the bound only guards a hung worker.
    let deadline = std::time::Instant::now() + SHUTDOWN_WAIT;
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
    fn cli_interactive_compaction_and_thinking_events_become_tui_events() {
        assert_eq!(
            tui_events(&json!({"type": "compaction_start", "reason": "manual"})),
            [LaneEvent::CompactionStarted]
        );
        let entry = json!({"type": "compaction", "id": "c1", "tokensBefore": 15, "retainedTail": []});
        assert_eq!(
            tui_events(&json!({"type": "entry_added", "entry": entry})),
            [LaneEvent::Compacted(Ok(entry.clone()))]
        );
        // Other entries arrive with their message events.
        assert!(tui_events(&json!({"type": "entry_added", "entry": {"type": "message"}})).is_empty());
        assert_eq!(
            tui_events(&json!({"type": "config_update", "property": "thinkingLevel", "value": "high"})),
            [LaneEvent::Thinking("high".into())]
        );
        assert!(tui_events(&json!({"type": "config_update", "property": "model", "value": {}})).is_empty());
    }

    #[test]
    fn cli_interactive_subagents_become_agent_hub_rows() {
        use interactive::agent_hub::AgentStatus;
        let listed = json!({"subagents": [
            {"id": "a", "definition": "rlm-child@1", "state": "running"},
            {"id": "b", "definition": "rlm-child@1", "state": "completed", "parentId": "a", "result": {"summary": "done it"}},
            {"id": "c", "definition": "rlm-child@1", "state": "cancelled"},
            {"id": "d", "definition": "rlm-child@1", "state": "parked"},
        ]});
        let rows = agent_views(&listed);
        let statuses: Vec<AgentStatus> = rows.iter().map(|r| r.status).collect();
        assert_eq!(
            statuses,
            [
                AgentStatus::Running,
                AgentStatus::Idle,
                AgentStatus::Aborted,
                AgentStatus::Parked
            ]
        );
        assert_eq!(rows[1].parent_id.as_deref(), Some("a"));
        assert_eq!(rows[1].task.as_deref(), Some("done it"));
        assert!(agent_views(&json!({})).is_empty());
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
