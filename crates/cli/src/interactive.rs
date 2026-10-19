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
    BashResult, CatalogRefresh, ForkMessage, LaneClient, LaneEvent, ModelInfo, PromptImage, QueueKind, Queued,
    SessionListing, SessionOpened,
};
use interactive::run::{UiEvent, run_stdout, spawn_gated_input};
use serde_json::{Value, json};
use session_core::core::{Op, QueueKind as CoreQueue};
use session_file::Env as _;
use std::io::{BufRead, IsTerminal, Write};
use std::rc::Rc;
use std::sync::{Arc, mpsc};
use tui::keybindings::KeybindingsManager;
use tui::keybindings_file::UserBindings;
use tui::qol_keys::native_app_keybindings;
use tui::theme::{ColorMode, builtin_theme_json, create_theme};

/// The tree's nodes with `checkpoint: true` on the entries a checkpoint (with a commit) is attached to.
pub fn mark_checkpoints(mut tree: Value, checkpoints: &Value) -> Value {
    let marked: std::collections::HashSet<&str> = checkpoints["checkpoints"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["commit"].is_string())
        .filter_map(|c| c["entryId"].as_str())
        .collect();
    if marked.is_empty() {
        return tree;
    }
    let mut stack: Vec<&mut Value> = tree.as_array_mut().into_iter().flatten().collect();
    while let Some(node) = stack.pop() {
        if node["entry"]["id"].as_str().is_some_and(|id| marked.contains(id)) {
            node["checkpoint"] = json!(true);
        }
        if let Some(children) = node.get_mut("children").and_then(Value::as_array_mut) {
            stack.extend(children.iter_mut());
        }
    }
    tree
}

fn uuid() -> String {
    session_file::SystemEnv.random_uuid()
}

/// What the TUI asks of the link thread.
enum Command {
    /// The link saw the session's worker restart under it (its `connection` event `reconnecting`).
    WorkerStopped,
    Op(Op),
    /// Start a turn, then report the session's (automatic) name.
    Prompt(Op),
    NewSession,
    Resume(String),
    ListSessions,
    /// `/import` without a path: Pi's sessions of this working directory.
    ListPiSessions,
    SelectModel(String, String),
    /// The run ended: report the session's name (a model title may have replaced the automatic one).
    RefreshName,
    /// The session was re-attached after its Radius connection recovered: show it again (`#openLane`).
    Reopen,
    Bash(String, bool),
    Compact(Option<String>),
    SetThinking(String),
    ForkMessages,
    Fork(String),
    Clone,
    /// `/export [path]` in the named theme.
    Export(Option<String>, Option<String>),
    Usage,
    /// `/goal <args>`: the session goal's command; its reply text is shown as a notice.
    Goal(String),
    SaveSetting(String, Value),
    Agents,
    /// A read-only inspection request and the title its answer is rendered under.
    Inspect(String, String, Value),
    KillAgent(String),
    SteerAgent(String, String),
    ReviveAgent(String),
    AgentTranscript(String),
    ReadSettings,
    ReadMode,
    /// `/autoreview`.
    Autoreview(interactive::lane::AutoreviewRequest),
    RlmStatus,
    Credentials,
    Logout(String, String),
    Changelog,
    /// `/mcp args` (`LegacyExtensionCommands.run`): run on its own thread, its dialogs served meanwhile.
    RunMcp(String),
    /// `/share`; the flag is set when the user cancels the gist upload.
    Share(Arc<std::sync::atomic::AtomicBool>, Option<String>),
    /// `/bug`: the summary written by the session's model.
    BugSummary(Option<String>),
    AbortBugSummary,
    /// `/bug`: build the report and upload or export it.
    BugReport(interactive::lane::BugReportRequest),
    /// `/bug`: export the report the failed upload built.
    BugReportExport,
    LoginProviders(String),
    SaveApiKey(String, String),
    /// A credential a multi-field API-key login produced (provider, `{type, key?, env?}`).
    SaveCredential(String, Value),
    /// A read-only inspection for the RLM views, answered as [`LaneEvent::RlmInspected`] with the tag.
    RlmInspect(String, String, Value),
    /// Every project's native sessions (`/resume`'s All scope).
    ListAllSessions,
    /// Pi's sessions of every project (`/import`'s All scope).
    ListAllPiSessions,
    /// A subscription login saved its credential (provider, name): report it as `runLoginDialog` does.
    LoginSaved(String, String),
    /// Refresh the model catalogs (`/scoped-models`).
    RefreshCatalog,
    DeleteSession(String),
    RenameSession(String, String),
    /// `/import <path>`: a Pi session file to import through the server.
    ImportPi(String),
    Tree,
    Navigate(Option<String>, bool, Option<String>),
    /// Restore the working tree's files to an entry's checkpoint.
    RestoreFiles(String),
    SetLabel(String, Option<String>),
    /// The answer to a `/mcp` dialog.
    ExtensionUiRespond(String, Value),
    /// `/trust`: the worker's trust state.
    ReadTrust,
    /// A `/trust` choice: trusted, and its `(path, decision)` updates.
    SetProjectTrust(bool, Vec<(String, Option<bool>)>),
    /// `/debug`: what the TUI shows.
    WriteDebugLog(interactive::debug_log::DebugCapture),
    /// `/reload`.
    ReloadPlugins,
    Shutdown,
}

/// `importPiSessionFile` (client-tui-sessions.ts): this process reads the Pi file and the server imports its text
/// (the server may run elsewhere); the answer says what was imported and where to switch.
pub fn import_pi(link: &crate::link::Link, path: &str) -> LaneEvent {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) => {
            let message = crate::usage::log::io_message(&error, "open", std::path::Path::new(path));
            return LaneEvent::Status(format!("Error: Failed to import session: {message}"));
        }
    };
    let result = match link.import_pi(path, &content) {
        Ok(result) => result,
        Err(error) => return LaneEvent::Status(format!("Error: Failed to import session: {error}")),
    };
    let session_id = result["session"]["sessionId"].as_str().unwrap_or_default().to_string();
    let imported = if result["alreadyImported"] == true {
        format!("Already imported as session {session_id}")
    } else {
        format!(
            "Imported {} entries from {path} as session {session_id}",
            result["imported"].as_u64().unwrap_or(0)
        )
    };
    let current = session_id == link.session_id();
    LaneEvent::PiImported {
        imported,
        source: path.to_string(),
        session_file: if current {
            None
        } else {
            link.session_file(&session_id).ok().flatten()
        },
        current,
    }
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
    env: Env,
    ui: mpsc::Sender<UiEvent>,
    /// The subscription login running now (its pending prompt).
    login: Arc<crate::tui_login::LoginSlot>,
    /// Set when the user cancels `/share`'s gist upload.
    share_cancel: Arc<std::sync::atomic::AtomicBool>,
    /// Set when the user cancels `/bug`'s upload.
    bug_cancel: Arc<std::sync::atomic::AtomicBool>,
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
    fn abort_retry(&self) {
        self.send(Command::Op(Op::AbortRetry));
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
    fn list_pi_sessions(&self) {
        self.send(Command::ListPiSessions);
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
    fn export(&self, path: Option<&str>, theme: Option<&str>) {
        self.send(Command::Export(path.map(str::to_string), theme.map(str::to_string)));
    }
    fn usage_report(&self) {
        self.send(Command::Usage);
    }
    fn goal(&self, args: &str) {
        self.send(Command::Goal(args.to_string()));
    }
    fn save_setting(&self, key: &str, value: Value) {
        self.send(Command::SaveSetting(key.to_string(), value));
    }
    fn agents(&self) {
        self.send(Command::Agents);
    }
    fn inspect(&self, title: &str, request: &str, payload: Value) {
        self.send(Command::Inspect(title.to_string(), request.to_string(), payload));
    }
    fn read_settings(&self) {
        self.send(Command::ReadSettings);
    }
    fn read_mode(&self) {
        self.send(Command::ReadMode);
    }
    fn autoreview(&self, request: interactive::lane::AutoreviewRequest) {
        self.send(Command::Autoreview(request));
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
    fn startup_changelog(&self) {
        let (env, ui) = (self.env.clone(), self.ui.clone());
        std::thread::spawn(move || {
            if let Some(notice) = crate::startup_checks::startup_changelog(&env) {
                let _ = ui.send(UiEvent::Lane(LaneEvent::StartupNotice(notice)));
            }
        });
    }
    fn subscription_auth_warning(&self, provider: &str) {
        let (env, ui, provider) = (self.env.clone(), self.ui.clone(), provider.to_string());
        std::thread::spawn(move || {
            if crate::startup_checks::subscription_auth_warns(&env, &provider) {
                let notice = interactive::lane::StartupNotice::SubscriptionAuth;
                let _ = ui.send(UiEvent::Lane(LaneEvent::StartupNotice(notice)));
            }
        });
    }
    fn mcp_command(&self, args: &str) {
        self.send(Command::RunMcp(args.to_string()));
    }
    fn login_providers(&self, query: &str) {
        self.send(Command::LoginProviders(query.to_string()));
    }
    fn save_api_key(&self, provider: &str, key: &str) {
        self.send(Command::SaveApiKey(provider.to_string(), key.to_string()));
    }
    fn oauth_login(&self, provider: &str, name: &str) -> bool {
        if !crate::tui_login::supported(&self.env, provider) {
            return false;
        }
        let ui = self.ui.clone();
        let send: Arc<dyn Fn(LaneEvent) + Send + Sync> = Arc::new(move |event| {
            let _ = ui.send(UiEvent::Lane(event));
        });
        let commands = self.commands.clone();
        let (id, label) = (provider.to_string(), name.to_string());
        let saved = Box::new(move || {
            let _ = commands.send(Command::LoginSaved(id, label));
        });
        crate::tui_login::start(
            self.env.clone(),
            provider.to_string(),
            name.to_string(),
            send,
            self.login.clone(),
            saved,
        );
        true
    }
    fn login_reply(&self, answer: Option<String>) {
        self.login.reply(answer);
    }
    fn refresh_catalog(&self) {
        self.send(Command::RefreshCatalog);
    }
    fn share(&self, theme: Option<&str>) {
        self.share_cancel.store(false, std::sync::atomic::Ordering::SeqCst);
        self.send(Command::Share(self.share_cancel.clone(), theme.map(str::to_string)));
    }
    fn cancel_share(&self) {
        self.share_cancel.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    fn bug_report_target(&self) -> (String, bool) {
        let offline = (self.env.var)("PI_OFFLINE").is_some_and(|v| !v.is_empty());
        (
            crate::bug_report::gateway_host(&crate::bug_report::gateway_url(&self.env)),
            offline,
        )
    }
    fn bug_report_summary(&self, hint: Option<&str>) {
        self.send(Command::BugSummary(hint.map(str::to_string)));
    }
    fn abort_bug_report_summary(&self) {
        self.send(Command::AbortBugSummary);
    }
    fn bug_report(&self, request: interactive::lane::BugReportRequest) {
        self.send(Command::BugReport(request));
    }
    fn bug_report_export(&self) {
        self.send(Command::BugReportExport);
    }
    fn cancel_bug_report(&self) {
        self.bug_cancel.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    fn save_credential(&self, provider: &str, credential: Value) {
        self.send(Command::SaveCredential(provider.to_string(), credential));
    }
    fn rlm_inspect(&self, tag: &str, request: &str, payload: Value) {
        self.send(Command::RlmInspect(tag.to_string(), request.to_string(), payload));
    }
    fn list_all_sessions(&self) {
        self.send(Command::ListAllSessions);
    }
    fn list_all_pi_sessions(&self) {
        self.send(Command::ListAllPiSessions);
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
    fn import_pi(&self, path: &str) {
        self.send(Command::ImportPi(path.to_string()));
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
    fn restore_files(&self, entry_id: &str) {
        self.send(Command::RestoreFiles(entry_id.to_string()));
    }
    fn kill_agent(&self, id: &str) {
        self.send(Command::KillAgent(id.to_string()));
    }
    fn steer_agent(&self, id: &str, text: &str) {
        self.send(Command::SteerAgent(id.to_string(), text.to_string()));
    }
    fn revive_agent(&self, id: &str) {
        self.send(Command::ReviveAgent(id.to_string()));
    }
    fn agent_transcript(&self, id: &str) {
        self.send(Command::AgentTranscript(id.to_string()));
    }
    fn extension_ui_respond(&self, id: &str, answer: Value) {
        self.send(Command::ExtensionUiRespond(id.to_string(), answer));
    }
    fn read_trust(&self) {
        self.send(Command::ReadTrust);
    }
    fn set_project_trust(&self, trusted: bool, updates: Vec<(String, Option<bool>)>) {
        self.send(Command::SetProjectTrust(trusted, updates));
    }
    fn write_debug_log(&self, capture: interactive::debug_log::DebugCapture) {
        self.send(Command::WriteDebugLog(capture));
    }
    fn reload_plugins(&self) {
        self.send(Command::ReloadPlugins);
    }
}

/// The dialogs and notices `/mcp` and the MCP gateway raise in the worker (`#serveExtensionUI`): polled between the
/// link's commands, without waiting, so a dialog shows within one tick. A different session starts again from its open dialogs.
#[derive(Default)]
struct ExtensionUiPoll {
    session: Option<String>,
    cursor: Option<u64>,
}

impl ExtensionUiPoll {
    fn poll(&mut self, link: &Link, send: &dyn Fn(LaneEvent)) {
        let session = link.session_id();
        if self.session.as_deref() != Some(session.as_str()) {
            self.session = Some(session);
            self.cursor = None;
        }
        let connection = link.connection();
        let polled = connection
            .session_target()
            .and_then(|target| connection.call(target, "ultron.extension-ui", "poll", json!([self.cursor, 0])));
        let Ok(Some(result)) = polled else {
            self.cursor = None;
            return;
        };
        self.cursor = result["cursor"].as_u64();
        for item in result["requests"].as_array().into_iter().flatten() {
            send(LaneEvent::ExtensionUi(item["request"].clone()));
        }
    }
}

/// The agent hub's notice for what a steer (`agent_message.send`'s `{state, steered}`) or a revive (`{id}`) did.
pub fn agent_notice(id: &str, action: &str, answer: Result<Option<Value>, String>) -> String {
    let error = |error: String| {
        let error = error
            .strip_prefix("session_operation_failed: ")
            .unwrap_or(&error)
            .to_string();
        format!("Error: {error}")
    };
    match (action, answer) {
        (_, Err(e)) => error(e),
        ("steer", Ok(answer)) if answer.as_ref().is_some_and(|a| a["steered"] == true) => {
            format!("Steered \"{id}\".")
        }
        ("steer", Ok(_)) => format!("\"{id}\" has no live run: the message waits in its inbox."),
        (_, Ok(answer)) => {
            let new = answer
                .as_ref()
                .and_then(|a| a["id"].as_str())
                .unwrap_or_default()
                .to_string();
            format!("Revived \"{id}\" as \"{new}\".")
        }
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
                Some("failed" | "cancelled") => AgentStatus::Aborted,
                // Left by a worker that died while it ran: the hub can revive it.
                Some("interrupted") => AgentStatus::Parked,
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
                .map(|item| {
                    let message = item["type"] == "message";
                    let user = message && item["message"]["role"] == "user";
                    Queued {
                        entry_id: item["entryId"].as_str().unwrap_or_default().to_string(),
                        kind: match item["kind"].as_str() {
                            Some("steer") => QueueKind::Steer,
                            Some("nextRun") => QueueKind::NextRun,
                            _ => QueueKind::FollowUp,
                        },
                        // client-tui-chat.ts `#syncQueues`: a message's user text (none for another role), else
                        // `<customType>`.
                        text: if user {
                            session_core::view::message_text(&item["message"])
                        } else if message {
                            String::new()
                        } else {
                            format!("<{}>", item["customType"].as_str().unwrap_or("undefined"))
                        },
                        user,
                    }
                })
                .collect(),
        )],
        Some("session_name") => vec![LaneEvent::Name(event["name"].as_str().map(str::to_string))],
        Some("compaction_start") => vec![LaneEvent::CompactionStarted {
            reason: event["reason"].as_str().unwrap_or("manual").to_string(),
        }],
        // A manual compaction's failure answers its operation; an automatic one that left no entry ends here.
        Some("compaction_end") if event["reason"] != "manual" && event["status"] != "completed" => {
            vec![LaneEvent::CompactionEnded {
                status: event["status"].as_str().unwrap_or_default().to_string(),
                error: event["error"]["message"].as_str().map(str::to_string),
            }]
        }
        Some("summarization_retry_scheduled") => vec![LaneEvent::SummarizationRetryScheduled {
            attempt: event["attempt"].as_u64().unwrap_or(0),
            max_attempts: event["maxAttempts"].as_u64().unwrap_or(0),
            delay_ms: event["delayMs"].as_u64().unwrap_or(0),
            error: text("errorMessage"),
        }],
        Some("summarization_retry_attempt_start") => vec![LaneEvent::SummarizationRetryStarted {
            branch_summary: event["source"] == "branchSummary",
            reason: event["reason"].as_str().unwrap_or("manual").to_string(),
        }],
        Some("summarization_retry_finished") => vec![LaneEvent::SummarizationRetryFinished],
        Some("retry_scheduled") => vec![LaneEvent::AutoRetryStart {
            attempt: event["attempt"].as_u64().unwrap_or(0),
            max_attempts: event["maxAttempts"].as_u64().unwrap_or(0),
            delay_ms: event["delayMs"].as_u64().unwrap_or(0),
        }],
        Some("retry_end") => vec![LaneEvent::AutoRetryEnd {
            attempt: event["attempt"].as_u64().unwrap_or(0),
            failed: (event["success"] != true)
                .then(|| event["finalError"].as_str().unwrap_or("Unknown error").to_string()),
        }],
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
            all_messages_text: s["allMessagesText"].as_str().unwrap_or_default().to_string(),
            cwd: s["cwd"].as_str().unwrap_or_default().to_string(),
        })
        .collect()
}

/// Pi's profile directory (read-only here): `PI_CODING_AGENT_DIR`, else `~/.pi/agent` (client-tui-sessions.ts
/// `piAgentDir`).
pub fn pi_agent_dir(env: &Env) -> String {
    let dir =
        (env.var)("PI_CODING_AGENT_DIR").unwrap_or_else(|| env.home.join(".pi").join("agent").display().to_string());
    session_file::paths::resolve_path(&dir)
}

/// Pi's per-project session directory under `<piAgentDir>/sessions` (`piSessionDirForCwd`).
pub fn pi_session_dir_for_cwd(cwd: &str, agent_dir: &str) -> String {
    let resolved = session_file::paths::resolve_path(cwd);
    let relative = resolved.strip_prefix(['/', '\\']).unwrap_or(&resolved);
    format!("{agent_dir}/sessions/--{}--", relative.replace(['/', '\\', ':'], "-"))
}

/// `SessionManager.list(cwd, piSessionDirForCwd(cwd, piAgentDir))`: Pi's sessions of this project, newest first, as
/// Pi's selector lists them. Pi's files are only read.
pub fn pi_sessions(env: &Env) -> Result<Vec<SessionListing>, String> {
    let cwd = env.cwd.display().to_string();
    let dir = pi_session_dir_for_cwd(&cwd, &pi_agent_dir(env));
    let sessions =
        session_file::list_sessions(&session_file::SystemEnv, &cwd, Some(&dir)).map_err(|e| e.to_string())?;
    Ok(sessions
        .into_iter()
        .map(|s| SessionListing {
            path: s.path,
            id: s.id,
            name: s.name,
            modified: s.modified.unwrap_or(0),
            message_count: s.message_count,
            first_message: s.first_message,
            all_messages_text: s.all_messages_text,
            cwd: s.cwd,
        })
        .collect())
}

/// The link thread: owns the link, runs the TUI's commands one at a time.
#[allow(clippy::too_many_arguments)]
fn link_thread(
    mut link: Link,
    commands: mpsc::Receiver<Command>,
    ui: mpsc::Sender<UiEvent>,
    no_session: bool,
    settings_path: (std::path::PathBuf, std::path::PathBuf),
    signalled: Arc<std::sync::atomic::AtomicI32>,
    catalog_env: Env,
    reporter: crate::bug_report::Reporter,
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
            // `plugins.prepareSession` for the attached session (client-tui.ts): `/reload` rebuilds this selection.
            let connection = link.connection();
            let prepared = connection.call(
                connection.server_target(),
                "pi.presentation-plugins",
                "prepareSession",
                json!([{"sessionId": session.session_id, "packagePaths": null}]),
            );
            if let Ok(data) = prepared
                && let Err(error) = apply_presentation_plugins(&data.unwrap_or(Value::Null))
            {
                send(LaneEvent::Status(format!("Error: {error}")));
            }
            send(LaneEvent::Opened(session));
            send(LaneEvent::Entries(entries));
            send(LaneEvent::Model(model));
        }
        Err(error) => send(LaneEvent::Error(error)),
    };
    open(&link, &mut name);
    let mut last_recovery: Option<std::time::Instant> = None;
    let mut extension_ui = ExtensionUiPoll::default();
    loop {
        extension_ui.poll(&link, &send);
        let command = match commands.recv_timeout(std::time::Duration::from_millis(250)) {
            Ok(command) => command,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
        };
        match command {
            // The worker went away without being asked to (it crashed or was killed) and the link re-attached the
            // session to a fresh worker: the session file is intact, so show it again (`#handleLocalAttachmentState`).
            // A second stop within WORKER_RECOVERY_INTERVAL is reported instead.
            Command::WorkerStopped => {
                if link
                    .remote_route()
                    .is_some_and(|route| matches!(route, crate::link::RemoteRoute::Radius { .. }))
                {
                    continue;
                }
                let now = std::time::Instant::now();
                if last_recovery.is_some_and(|t| now.duration_since(t) < WORKER_RECOVERY_INTERVAL) {
                    send(LaneEvent::Status(format!(
                        "Error: The session worker stopped again; {}",
                        daemon::lost::RESUME_HINT
                    )));
                    continue;
                }
                last_recovery = Some(now);
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
            Command::Reopen => open(&link, &mut name),
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
            Command::ListPiSessions => match pi_sessions(&catalog_env) {
                Ok(sessions) => send(LaneEvent::PiSessions(sessions)),
                Err(error) => send(LaneEvent::Error(error)),
            },
            Command::SelectModel(provider, model_id) => {
                match link.op(&Op::SetModel {
                    provider: provider.clone(),
                    model_id: model_id.clone(),
                }) {
                    Ok(model) => {
                        // `Models.select` (models-provider.ts): the selection becomes the saved default.
                        let (cwd, agent_dir) = &settings_path;
                        let mut settings =
                            config::settings::SettingsManager::for_worker(cwd, agent_dir, &catalog_env.home);
                        settings.set_global("defaultProvider", Some(json!(provider)));
                        settings.set_global("defaultModel", Some(json!(model_id)));
                        send(LaneEvent::Model(model_info(&model.unwrap_or_default())));
                    }
                    Err(error) => send(LaneEvent::Error(error)),
                }
            }
            Command::Bash(command, exclude) => {
                // Answered on its own thread, so Esc (abortBash) reaches the worker while it runs.
                let started = start_call(&link, "ultron.session-control", "bash", json!([command, exclude]));
                let ui = ui.clone();
                std::thread::spawn(move || {
                    let result =
                        started.and_then(|(connection, request)| connection.wait(&request).map_err(|e| e.to_string()));
                    let event = LaneEvent::BashDone(result.map(|value| bash_result(&value.unwrap_or_default())));
                    let _ = ui.send(UiEvent::Lane(event));
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
            Command::Export(path, theme) => send(LaneEvent::Status(
                match export_session(&link, path.as_deref(), theme.as_deref(), &catalog_env) {
                    Ok(path) => format!("Session exported to: {path}"),
                    Err(error) => format!("Error: Failed to export session: {error}"),
                },
            )),
            Command::Inspect(title, request, payload) => {
                send(LaneEvent::Status(match link.op(&Op::Inspect { request, payload }) {
                    Ok(value) => interactive::inspection::format_inspection(&title, &value.unwrap_or(Value::Null)),
                    Err(error) => format!("Error: {error}"),
                }))
            }
            Command::RunMcp(args) => {
                // Off the link thread: `/mcp` may run long, and its dialogs wait on this client, so the link keeps
                // polling meanwhile. The worker dispatches it to the gateway.
                let (connection, ui) = (link.connection(), ui.clone());
                std::thread::spawn(move || {
                    let send = |event| {
                        let _ = ui.send(UiEvent::Lane(event));
                    };
                    let run = connection.session_target().and_then(|target| {
                        connection.call(target, "pi.legacy-extension-commands", "run", json!(["mcp", args]))
                    });
                    match run {
                        Ok(result) => {
                            let result = result.unwrap_or_default();
                            for notification in result["notifications"].as_array().into_iter().flatten() {
                                send(LaneEvent::Status(notification.as_str().unwrap_or_default().to_string()));
                            }
                        }
                        Err(error) => send(LaneEvent::Status(format!("Error: {}", crate::link::fail(error)))),
                    }
                });
            }
            Command::Usage => match usage_report(&link) {
                Ok(text) => send(LaneEvent::Notice(text)),
                Err(error) => send(LaneEvent::Status(format!("Error: {error}"))),
            },
            // The worker owns the goal and starts its rounds; only this command (a person) changes it.
            Command::Goal(args) => match link.op(&Op::Goal { args }) {
                Ok(reply) => send(LaneEvent::GoalReply(
                    reply
                        .as_ref()
                        .and_then(Value::as_str)
                        .unwrap_or("No goal reply")
                        .to_string(),
                )),
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
            Command::ImportPi(path) => send(import_pi(&link, &path)),
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
                    // Entries with a working-tree checkpoint are marked for the tree's restore key.
                    let checkpoints = link.op(&Op::Checkpoints).ok().flatten().unwrap_or_default();
                    send(LaneEvent::Tree {
                        tree: mark_checkpoints(read["tree"].clone(), &checkpoints),
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
                // AgentSession `navigateTree`'s `editorText`: the selected user or custom message's text.
                send(LaneEvent::Navigated(result.map(|answer| {
                    answer.and_then(|a| a["editorText"].as_str().map(str::to_string))
                })));
            }
            Command::RestoreFiles(entry_id) => {
                let restored = link.op(&Op::RestoreFiles {
                    entry_id: Some(entry_id),
                    checkpoint: None,
                });
                send(LaneEvent::FilesRestored(restored.map(|answer| {
                    answer
                        .and_then(|a| a["message"].as_str().map(str::to_string))
                        .unwrap_or_default()
                })));
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
            Command::ExtensionUiRespond(id, answer) => {
                let connection = link.connection();
                let _ = connection
                    .session_target()
                    .and_then(|target| connection.call(target, "ultron.extension-ui", "respond", json!([id, answer])));
            }
            Command::KillAgent(id) => {
                if let Err(error) = link.op(&Op::KillAgent { id }) {
                    send(LaneEvent::Status(format!("Error: {error}")));
                }
            }
            Command::SteerAgent(id, message) => {
                let answer = link.op(&Op::SteerAgent {
                    id: id.clone(),
                    message,
                });
                send(LaneEvent::AgentNotice(agent_notice(&id, "steer", answer)));
            }
            Command::ReviveAgent(id) => {
                let answer = link.op(&Op::ReviveAgent { id: id.clone() });
                send(LaneEvent::AgentNotice(agent_notice(&id, "revive", answer)));
            }
            Command::AgentTranscript(id) => match link.op(&Op::AgentTranscript { id: id.clone() }) {
                Ok(read) => send(LaneEvent::AgentTranscript {
                    id,
                    messages: read.unwrap_or_default()["messages"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default(),
                }),
                Err(error) => send(LaneEvent::AgentNotice(format!("Error: {error}"))),
            },
            Command::SaveSetting(key, value) => {
                let (cwd, agent_dir) = &settings_path;
                let mut settings = config::settings::SettingsManager::for_worker(cwd, agent_dir, &catalog_env.home);
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
            Command::Share(cancel, theme) => share_session(&link, &send, &cancel, theme.as_deref(), &catalog_env),
            Command::BugSummary(hint) => {
                let op = Op::BugSummary { hint };
                let started = start_call(&link, daemon::worker::OPS_SERVICE, "op", json!([op.to_json()]));
                let ui = ui.clone();
                std::thread::spawn(move || {
                    let result = started
                        .and_then(|(connection, request)| connection.wait(&request).map_err(crate::link::fail))
                        .map(|text| text.and_then(|t| t.as_str().map(str::to_string)).unwrap_or_default())
                        .map_err(|e| {
                            e.strip_prefix("session_operation_failed: ")
                                .map_or(e.clone(), str::to_string)
                        });
                    let _ = ui.send(UiEvent::Lane(LaneEvent::BugSummary(result)));
                });
            }
            Command::AbortBugSummary => {
                let _ = link.op(&Op::AbortBugSummary);
            }
            Command::BugReport(request) => {
                let answer_ui = ui.clone();
                let answer: Arc<dyn Fn(interactive::lane::BugReportOutcome) + Send + Sync> = Arc::new(move |outcome| {
                    let _ = answer_ui.send(UiEvent::Lane(LaneEvent::BugReport(outcome)));
                });
                reporter.deliver(&link, &catalog_env, request, session_op_runner(&link), answer);
            }
            Command::BugReportExport => {
                let outcome = reporter.export_last(&catalog_env, &|op| link.op(&op));
                send(LaneEvent::BugReport(outcome));
            }
            Command::SaveCredential(provider, credential) => {
                let auth = config::paths::AgentPaths::new(settings_path.1.clone()).auth();
                let name = interactive::selectors::provider_name(&provider);
                let saved = config::auth::AuthStorage::create(auth.clone()).modify(&provider, |_| Some(credential));
                match saved {
                    Err(error) => send(LaneEvent::Status(format!(
                        "Error: Failed to save API key for {name}: {error}"
                    ))),
                    Ok(_) => credentials_saved(&link, &catalog_env, &send, "Saved API key for", &provider, &name),
                }
            }
            Command::RlmInspect(tag, request, payload) => send(LaneEvent::RlmInspected {
                tag,
                result: link
                    .op(&Op::Inspect { request, payload })
                    .map(Option::unwrap_or_default),
            }),
            Command::ListAllSessions => {
                let dir = crate::session::session_directory(None, &catalog_env);
                let repo = session_journal::SessionRepo::new(&dir, session_journal::SystemClock::shared());
                let listed = session_journal::names::list_all_sessions(&repo)
                    .into_iter()
                    .map(|s| SessionListing {
                        path: s.metadata.path.display().to_string(),
                        id: s.metadata.id,
                        name: s.name,
                        modified: s.metadata.modified_at as i64,
                        message_count: s.message_count,
                        first_message: s.first_message,
                        all_messages_text: s.all_messages_text,
                        cwd: s.metadata.cwd,
                    })
                    .collect();
                send(LaneEvent::AllSessions(listed));
            }
            Command::ListAllPiSessions => {
                let dir = std::path::Path::new(&pi_agent_dir(&catalog_env)).join("sessions");
                let listed =
                    session_file::list_all_sessions(&session_file::SystemEnv, Some(&dir.display().to_string()))
                        .into_iter()
                        .map(|s| SessionListing {
                            path: s.path,
                            id: s.id,
                            name: s.name,
                            modified: s.modified.unwrap_or(0),
                            message_count: s.message_count,
                            first_message: s.first_message,
                            all_messages_text: s.all_messages_text,
                            cwd: s.cwd,
                        })
                        .collect();
                send(LaneEvent::PiAllSessions(listed));
            }
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
                    Ok(_) => credentials_saved(&link, &catalog_env, &send, "Saved API key for", &provider, &name),
                }
            }
            Command::RefreshCatalog => {
                // `models.refresh()`: from the network unless offline (`PI_OFFLINE`), on its own thread.
                let (env, ui) = (catalog_env.clone(), ui.clone());
                std::thread::spawn(move || {
                    let refresh = if (env.var)("PI_OFFLINE").is_none() {
                        match session_core::model_refresh::refresh_catalogs(
                            &env,
                            ai::model_refresh::DEFAULT_CATALOG_BASE_URL,
                            &ai::model_refresh::management_user_agent(&crate::version()),
                            false,
                        ) {
                            Ok(errors) => {
                                CatalogRefresh::Done(errors.into_iter().map(|(provider, _)| provider).collect())
                            }
                            Err(error) if error == session_core::model_refresh::REFRESH_TIMED_OUT => {
                                CatalogRefresh::TimedOut
                            }
                            Err(error) => CatalogRefresh::Failed(error),
                        }
                    } else {
                        CatalogRefresh::Done(Vec::new())
                    };
                    let _ = ui.send(UiEvent::Lane(LaneEvent::Catalog(catalog(&env))));
                    let _ = ui.send(UiEvent::Lane(LaneEvent::CatalogRefreshed(refresh)));
                });
            }
            Command::LoginSaved(provider, name) => {
                // `reloadWorkerAuth`: the catalog refresh reaches the network unless offline (`PI_OFFLINE`).
                if (catalog_env.var)("PI_OFFLINE").is_none() {
                    let _ = session_core::model_refresh::refresh_catalogs(
                        &catalog_env,
                        ai::model_refresh::DEFAULT_CATALOG_BASE_URL,
                        &ai::model_refresh::management_user_agent(&crate::version()),
                        false,
                    );
                }
                credentials_saved(&link, &catalog_env, &send, "Logged in to", &provider, &name);
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
                let settings = config::settings::SettingsManager::for_worker(cwd, agent_dir, &catalog_env.home);
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
            Command::ReadMode => {
                let (cwd, agent_dir) = &settings_path;
                let settings = config::settings::SettingsManager::for_worker(cwd, agent_dir, &catalog_env.home);
                let mode = session_core::lane::PromptMode::from_settings(&settings);
                send(LaneEvent::Mode {
                    engineering: mode.engineering,
                    always_skills: mode.always_skills,
                });
            }
            Command::Autoreview(request) => send(crate::autoreview_toggle::autoreview(
                request,
                &catalog_env,
                crate::autoreview_cli::platform(),
                &crate::autoreview_toggle::run_command,
            )),
            Command::ReadTrust => send(match read_trust(&link) {
                Ok(view) => LaneEvent::Trust(view),
                Err(error) => LaneEvent::Status(format!("Error: {error}")),
            }),
            Command::SetProjectTrust(trusted, updates) => {
                let updates: Vec<Value> = updates
                    .into_iter()
                    .map(|(path, decision)| json!({"path": path, "decision": decision}))
                    .collect();
                send(LaneEvent::Status(
                    match session_call(&link, "ultron.session-control", "setProjectTrust", json!([updates])) {
                        Ok(_) => interactive::trust_selector::saved_status(trusted),
                        Err(error) => format!("Error: {error}"),
                    },
                ));
            }
            Command::WriteDebugLog(capture) => {
                send(write_debug_log(&link, &catalog_env, &settings_path.1, &capture));
            }
            Command::ReloadPlugins => {
                // `KeybindingsManager.reload()` (interactive-mode.ts `/reload`): keybindings.json is read again.
                send(LaneEvent::Keybindings(user_keybindings(
                    &settings_path.1,
                    crate::terminal_settings::platform(&catalog_env),
                )));
                send(LaneEvent::Status(match reload_plugins(&link) {
                    Ok(()) => "Reloaded plugins.".into(),
                    Err(error) => format!("Error: {error}"),
                }));
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

/// One call of the attached session's services.
fn session_call(link: &Link, service: &str, member: &str, args: Value) -> Result<Option<Value>, String> {
    let connection = link.connection();
    connection
        .session_target()
        .and_then(|target| connection.call(target, service, member, args))
        .map_err(|e| e.to_string())
}

/// `/trust`: the worker's `readSettings` and the trust choices for its cwd (`getProjectTrustOptions`).
fn read_trust(link: &Link) -> Result<interactive::trust_selector::TrustView, String> {
    let read = session_call(link, "ultron.session-control", "readSettings", json!([]))?.unwrap_or_default();
    let cwd = read["cwd"].as_str().unwrap_or_default().to_string();
    let display = |p: &std::path::Path| p.display().to_string();
    let choices = config::trust::trust_options(std::path::Path::new(&cwd), false)
        .into_iter()
        .map(|option| interactive::trust_selector::TrustChoice {
            label: option.label,
            trusted: option.trusted,
            updates: option.updates.iter().map(|u| (display(&u.path), u.decision)).collect(),
            saved_path: option.saved_path.as_deref().map(display),
        })
        .collect();
    Ok(interactive::trust_selector::TrustView {
        cwd,
        choices,
        saved: read["savedTrust"]["path"]
            .as_str()
            .map(|path| (path.to_string(), read["savedTrust"]["decision"] == true)),
        project_trusted: read["projectTrusted"] == true,
    })
}

/// `/debug`: the worker's `debugInfo` and this client's and server's facts with what the TUI shows, written to
/// `<agentDir>/ultron-debug.log` (0600).
fn write_debug_log(
    link: &Link,
    env: &Env,
    agent_dir: &std::path::Path,
    capture: &interactive::debug_log::DebugCapture,
) -> LaneEvent {
    let worker = session_call(link, "ultron.session-control", "debugInfo", json!([]))
        .map(Option::unwrap_or_default)
        .map_err(|e| {
            if e == "Session is not attached" {
                "No Session is attached".into()
            } else {
                e
            }
        });
    let server_id = link.connection().server_id.clone();
    let socket = [false, true]
        .into_iter()
        .map(|durable| crate::instance(env, durable).dir.join(format!("{server_id}.sock")))
        .find(|p| p.exists());
    let client = interactive::debug_log::ClientFacts {
        time: session_file::js::iso_string(daemon::paths::now_ms()),
        process: format!(
            "ultron {} · rust · {} · pid {}",
            crate::version(),
            daemon::session_control::platform(),
            std::process::id()
        ),
        cwd: std::env::current_dir()
            .map(|d| d.display().to_string())
            .unwrap_or_default(),
        server_id: Some(server_id),
        transport: Some(link.remote_route().map_or("unix", |r| r.transport()).into()),
        socket_path: match link.remote_route() {
            Some(route) => route.socket_path().map(|p| p.display().to_string()),
            None => socket.map(|p| p.display().to_string()),
        },
    };
    let (data, summary) = interactive::debug_log::debug_log(capture, &client, &worker);
    let path = agent_dir.join(interactive::debug_log::DEBUG_LOG_NAME);
    let written = std::fs::create_dir_all(agent_dir).and_then(|()| {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .and_then(|mut file| std::io::Write::write_all(&mut file, data.as_bytes()))
    });
    match written {
        Ok(()) => LaneEvent::DebugLogWritten {
            path: path.display().to_string(),
            summary,
        },
        Err(error) => LaneEvent::Status(format!("Error: Could not write debug log: {error}")),
    }
}

/// `createPresentationFacetLoaders` (plugins/bundled.ts): the presentation-plugin data a server sent is an object
/// whose `presentationFacetBundles`, when present, is a list of facet bundle artifacts. Bundles are JavaScript facets
/// the TypeScript TUI loads; this client cannot run them, so data that selects any is refused (the Rust server never
/// selects any: plugin packages are not built by it).
pub fn apply_presentation_plugins(data: &Value) -> Result<(), String> {
    let Some(object) = data.as_object() else {
        return Err("Invalid presentation plugin data".into());
    };
    match object.get("presentationFacetBundles") {
        None => Ok(()),
        Some(Value::Array(bundles)) if bundles.is_empty() => Ok(()),
        Some(Value::Array(bundles)) => Err(format!(
            "{} presentation plugin bundle(s) cannot be loaded by this client",
            bundles.len()
        )),
        Some(_) => Err("Invalid presentation plugin bundle list".into()),
    }
}

/// `/reload` (slash-commands-provider.ts): the server rebuilds this client's prepared presentation plugins, the
/// attached worker reloads its session plugins, then the client applies the rebuilt presentation data
/// (`reloadPresentationPlugins(data)`, client-tui.ts:579-600).
fn reload_plugins(link: &Link) -> Result<(), String> {
    let connection = link.connection();
    let data = connection
        .call(
            connection.server_target(),
            "pi.presentation-plugins",
            "reload",
            json!([]),
        )
        .map_err(|e| e.to_string())?
        .unwrap_or(Value::Null);
    session_call(link, "pi.session-plugins", "reload", json!([]))?;
    apply_presentation_plugins(&data)
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
fn share_session(
    link: &Link,
    send: &dyn Fn(LaneEvent),
    cancel: &std::sync::atomic::AtomicBool,
    theme: Option<&str>,
    env: &Env,
) {
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
        export_session(link, Some(&file.display().to_string()), theme, env)
            .map_err(|e| format!("Error: Failed to export session: {e}"))?;
        // The "Creating gist..." loader shows while gh runs; Esc on it kills this gh and reports nothing more.
        send(LaneEvent::ShareLoader(true));
        let out = gist_create(&file, cancel);
        send(LaneEvent::ShareLoader(false));
        let Some(out) = out.map_err(|e| format!("Error: Failed to create gist: {e}"))? else {
            return Ok(None);
        };
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
        let preview = crate::export_html::share_viewer_url(&|k| (env.var)(k), &gist_id);
        Ok(Some(format!(
            "Share URL: {}\nGist: {}",
            tui::image::hyperlink(&preview, &preview),
            tui::image::hyperlink(&gist_url, &gist_url)
        )))
    })();
    let _ = std::fs::remove_dir_all(&dir);
    match result {
        Ok(Some(text)) | Err(text) => status(text),
        Ok(None) => {}
    }
}

/// `gh gist create --public=false <file>`, killed when `cancel` is set (`None` then).
fn gist_create(
    file: &std::path::Path,
    cancel: &std::sync::atomic::AtomicBool,
) -> std::io::Result<Option<std::process::Output>> {
    use std::sync::atomic::Ordering;
    let mut child = std::process::Command::new("gh")
        .args(["gist", "create", "--public=false"])
        .arg(file)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    loop {
        if child.try_wait()?.is_some() {
            return child.wait_with_output().map(Some);
        }
        if cancel.load(Ordering::SeqCst) {
            // Only the gh this call started.
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// What follows saving a credential (`runLoginDialog`): where it went, the refreshed catalog, and whether the session's
/// model is that provider's.
fn credentials_saved(link: &Link, env: &Env, send: &dyn Fn(LaneEvent), action: &str, provider: &str, name: &str) {
    let auth = config::paths::AgentPaths::new(env.agent_dir()).auth();
    let done = format!("{action} {name}. Credentials saved to {}", auth.display());
    send(LaneEvent::Status(format!(
        "{done}. Reloading the Session's credentials…"
    )));
    // The session reads credentials per request; the catalog follows the providers.
    send(LaneEvent::Catalog(catalog(env)));
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

/// `loginProviderOptions`: every built-in provider's sign-in options with what is configured (a stored credential,
/// else the variables that supply a key), sorted by name.
pub fn login_options(env: &Env, auth: &std::path::Path) -> Vec<interactive::selectors::AuthProvider> {
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
            // The Claude Code CLI's own login: configured when its CLI is present.
            None if p.id == ai::claude_code::PROVIDER => ai::claude_code::configured(&source)
                .then(|| ("api_key".to_string(), Some(ai::claude_code::AUTH_SOURCE.to_string()))),
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
    // models.json Radius providers (`"oauth": "radius"`) sign in as the built-in Radius provider does, under their
    // own id and name; a key comes from RADIUS_API_KEY as Radius's does.
    let registry = session_core::lane::registry(env);
    let radius = provider_logins().into_iter().find(|p| p.id == "radius");
    for id in registry.config().provider_ids() {
        let builtin = ai::models::builtin_providers().any(|p| p == id);
        let (Some((name, _)), Some(radius), false) = (registry.radius_gateway(id), radius.as_ref(), builtin) else {
            continue;
        };
        let status = match stored.get(id) {
            Some(kind) => Some((kind.clone(), Some("stored credential".to_string()))),
            None => ai::env::find_env_keys_in("radius", &source)
                .filter(|keys| !keys.is_empty())
                .map(|keys| ("api_key".to_string(), Some(keys.join(", ")))),
        };
        let base = AuthProvider {
            id: id.to_string(),
            name,
            auth_type: "oauth".into(),
            status,
            api_key_method: radius.api_key.clone(),
            login_label: radius.oauth.clone().flatten(),
            generic_login: true,
        };
        let api_key = radius.api_key.is_some().then(|| AuthProvider {
            auth_type: "api_key".into(),
            ..base.clone()
        });
        options.push(base);
        options.extend(api_key);
    }
    // models.json's own providers (not a built-in's override, not a Radius gateway) sign in with an API key
    // (`provider-composer.ts`: "API key", saved through the generic prompt), their status from the stored credential
    // or the configured `apiKey` (`configuredRequestAuthStatus`).
    for id in registry.config().provider_ids() {
        let builtin = ai::models::builtin_providers().any(|p| p == id);
        if builtin || registry.radius_gateway(id).is_some() {
            continue;
        }
        let config = registry.config().provider(id);
        let name = config
            .and_then(|c| c.get("name"))
            .and_then(Value::as_str)
            .unwrap_or(id)
            .to_string();
        let status = match stored.get(id) {
            Some(kind) => Some((kind.clone(), Some("stored credential".to_string()))),
            None => config
                .and_then(|c| c.get("apiKey"))
                .and_then(Value::as_str)
                .and_then(|key| configured_key_status(key, &*env.var)),
        };
        options.push(AuthProvider {
            id: id.to_string(),
            name,
            auth_type: "api_key".into(),
            status,
            api_key_method: Some(("API key".into(), true)),
            login_label: None,
            generic_login: true,
        });
    }
    options.sort_by(|a, b| locale_compare(&a.name, &b.name));
    options
}

/// `configuredRequestAuthStatus` of a models.json `apiKey`: a `!command`, the environment variables a template names
/// (when all are set), or the key itself.
fn configured_key_status(key: &str, var: &dyn Fn(&str) -> Option<String>) -> Option<(String, Option<String>)> {
    if key.starts_with('!') {
        return Some(("api_key".into(), Some("models_json_command".into())));
    }
    let names = ai::model_registry::env_names(key);
    if names.is_empty() {
        return Some(("api_key".into(), Some("models_json_key".into())));
    }
    names
        .iter()
        .all(|name| var(name).is_some_and(|v| !v.is_empty()))
        .then(|| ("api_key".into(), Some(names.join(", "))))
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
/// An operation of the attached session that any thread can run (the link stays with its thread).
fn session_op_runner(link: &Link) -> Arc<dyn Fn(Op) -> Result<Option<Value>, String> + Send + Sync> {
    let connection = link.connection();
    Arc::new(move |op: Op| {
        let session = connection.session_target().map_err(|e| e.to_string())?;
        let request = connection
            .start(session, daemon::worker::OPS_SERVICE, "op", json!([op.to_json()]))
            .map_err(|e| e.to_string())?;
        connection.wait(&request).map_err(crate::link::fail)
    })
}

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
fn export_session(link: &Link, output: Option<&str>, theme: Option<&str>, env: &Env) -> Result<String, String> {
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
    let html = crate::export_html::export_page(&data, theme, env)?;
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
    let (open_args, open_env) = (parsed.clone(), env.clone());
    run_tui(
        parsed,
        env,
        Box::new(move |watch| Link::open(&open_args, &open_env, &target, "interactive", watch)),
    )
}

/// The client TUI (`runClientTui`) on a server this client reaches but does not run (`ultron client --connect`):
/// the session `selection` names on it. A Radius route's connection is recovered while the TUI waits, with its
/// statuses (`Radius disconnected; retrying…`, `Reconnecting to Radius…`, `Reattaching Session…`); a Unix route's
/// loss is reported.
pub fn run_remote_interactive(
    parsed: &Args,
    env: &Env,
    route: crate::link::RemoteRoute,
    selection: crate::link::RemoteSelection,
) -> Result<i32, String> {
    run_tui(
        parsed,
        env,
        Box::new(move |watch| Link::open_remote(route, &selection, watch)),
    )
}

/// Whether a link watch event says the session's worker restarted under it (crate::link `needs_rejoin`): the link
/// re-attaches by itself; the TUI shows the session again. A Radius route recovers through its own events.
pub fn is_worker_restart(event: &Value) -> bool {
    event["type"] == "connection"
        && event["state"] == "reconnecting"
        && event["message"] == crate::link::WORKER_RESTARTED
}

/// What a remote link's recovery events show (client-tui.ts `#handleConnectionState`, `#handleAttachmentState`,
/// `#handleLocalConnectionState`), given whether the lane is open: the TUI events, and whether the session is to be
/// shown again (the lane reopened after a reattachment).
pub fn recovery_events(event: &Value, lane_open: &mut bool) -> (Vec<LaneEvent>, bool) {
    use client::radius::{AttachmentState, ConnectionState, attachment_state_view, connection_state_view};
    let view = match event["type"].as_str() {
        Some("radius_connection") => {
            let state = match event["state"].as_str() {
                Some("connecting") => ConnectionState::Connecting,
                Some("connected") => ConnectionState::Connected,
                _ => ConnectionState::Disconnected,
            };
            connection_state_view(state, *lane_open)
        }
        Some("radius_attachment") => {
            let id = event["sessionId"].as_str().unwrap_or_default().to_string();
            let state = if event["state"] == "attached" {
                AttachmentState::Attached(id.clone())
            } else {
                AttachmentState::Attaching(id.clone())
            };
            attachment_state_view(&state, &id)
        }
        Some("connection") if event["state"] == "lost" => {
            let message = event["message"].as_str().unwrap_or_default();
            return (
                vec![LaneEvent::Recovery {
                    busy: false,
                    status: format!("Error: {message}"),
                }],
                false,
            );
        }
        _ => return (Vec::new(), false),
    };
    let Some(view) = view else {
        return (Vec::new(), false);
    };
    let reopen = event["type"] == "radius_attachment" && !view.busy && !*lane_open;
    if view.close_lane {
        *lane_open = false;
    }
    if !view.busy {
        *lane_open = true;
    }
    (
        vec![LaneEvent::Recovery {
            busy: view.busy,
            status: view.status.to_string(),
        }],
        reopen,
    )
}

/// `KeybindingsManager.create(agentDir)` (core/keybindings.ts): the user bindings in `<agentDir>/keybindings.json`,
/// legacy names migrated and ordered by the TypeScript definitions; nothing when the file is missing or malformed.
pub fn user_keybindings(agent_dir: &std::path::Path, platform: tui::keybindings::Platform) -> UserBindings {
    tui::keybindings_file::load_user_bindings(
        &tui::keybindings_file::keybindings_path(agent_dir),
        &tui::keybindings::app_keybindings(platform),
    )
}

/// Run the TUI over the link `open_link` opens (its watch events go to the TUI) until the user exits.
/// The TUI's debug outputs and Termux flag from `env` ([`interactive::run::terminal_debug`]): the redraw log goes to
/// the agent directory (`logDirectory: agentDir`), dumps to `dump_dir`; the write-log file is named by local time.
pub fn terminal_debug(
    env: &Env,
    dump_dir: &std::path::Path,
) -> (Option<std::sync::Arc<tui::render_debug::RenderDebug>>, bool) {
    let now = session_core::lane::now_ms();
    let zone = crate::localtime::Zone::from_env((env.var)("TZ").as_deref());
    let fields = zone.fields(now as f64);
    let second = (now.div_euclid(1000) + zone.offset(now as f64)).rem_euclid(60) as u32;
    let local = tui::render_debug::LocalStamp {
        year: fields.year,
        month: fields.month,
        day: fields.day,
        hour: fields.hour,
        minute: fields.minute,
        second,
    };
    interactive::run::terminal_debug(
        &|name| (env.var)(name),
        &env.agent_dir(),
        dump_dir,
        local,
        std::process::id(),
        Box::new(session_core::lane::now_ms),
    )
}

fn run_tui(
    parsed: &Args,
    env: &Env,
    open_link: Box<dyn FnOnce(crate::link::WatchSink) -> Result<Link, String>>,
) -> Result<i32, String> {
    let model = crate::print_mode::resolve_model(parsed, env).ok();
    let models = catalog(env);
    let splash =
        !(env.var)("ULTRON_SPLASH").is_some_and(|v| matches!(v.trim().to_lowercase().as_str(), "off" | "0" | "false"));
    // The worker's project trust: a trusted project's `.pi/settings.json` applies, a distrusted one's does not.
    let settings = config::settings::SettingsManager::for_worker(&env.cwd, &env.agent_dir(), &env.home);
    let qol = interactive::qol::from_settings(&Value::Object(settings.settings().clone()), &|k| (env.var)(k));
    let caps = crate::terminal_settings::capabilities(env, &settings);
    let theme_name = settings.theme().unwrap_or_else(|| "dark".into());
    let theme_json = builtin_theme_json(&theme_name)
        .or_else(|| builtin_theme_json("dark"))
        .ok_or("the dark theme is missing")?;
    let theme = Rc::new(create_theme(&theme_json, ColorMode::from_capabilities(&caps), None)?);
    // What every transcript markdown reads (`getCapabilities().hyperlinks`, `markdown.codeBlockIndent`); this thread
    // paints.
    interactive::render_settings::set(interactive::render_settings::RenderSettings {
        hyperlinks: caps.hyperlinks,
        code_block_indent: Some(settings.code_block_indent()),
    });
    let platform = crate::terminal_settings::platform(env);
    let user_keys = user_keybindings(&env.agent_dir(), platform);
    let key_context = crate::terminal_settings::key_context(env);
    let keys = || KeybindingsManager::new(native_app_keybindings(platform), user_keys.clone(), key_context);
    // `ensureTool("fd")` before the TUI starts (client-tui.ts `runClientTui`).
    let fd_path = crate::tools_manager::ensure_fd(env, crate::tools_manager::GITHUB);

    let (ui_tx, ui_rx) = mpsc::channel::<UiEvent>();
    let (command_tx, command_rx) = mpsc::channel::<Command>();
    let watch = {
        let (ui, commands) = (ui_tx.clone(), command_tx.clone());
        let lane_open = std::sync::Mutex::new(true);
        move |event: Value| {
            if event["type"] == "run_end" {
                let _ = commands.send(Command::RefreshName);
            }
            if is_worker_restart(&event) {
                let _ = commands.send(Command::WorkerStopped);
            }
            let (recovery, reopen) = recovery_events(&event, &mut lane_open.lock().unwrap_or_else(|e| e.into_inner()));
            for lane_event in tui_events(&event).into_iter().chain(recovery) {
                let _ = ui.send(UiEvent::Lane(lane_event));
            }
            if reopen {
                let _ = commands.send(Command::Reopen);
            }
        }
    };
    let link = open_link(Arc::new(watch))?;
    let server_id = link.connection().server_id.clone();
    let no_session = parsed.no_session;
    let signalled = Arc::new(std::sync::atomic::AtomicI32::new(0));
    let bug_cancel: Arc<std::sync::atomic::AtomicBool> = Arc::default();
    let link_worker = {
        let ui = ui_tx.clone();
        let settings_path = (env.cwd.clone(), env.agent_dir());
        let signalled = signalled.clone();
        let catalog_env = env.clone();
        let reporter = crate::bug_report::Reporter::new(bug_cancel.clone(), parsed.api_key.is_some());
        std::thread::spawn(move || {
            link_thread(
                link,
                command_rx,
                ui,
                no_session,
                settings_path,
                signalled,
                catalog_env,
                reporter,
            )
        })
    };
    let client = DaemonLane {
        commands: command_tx.clone(),
        env: env.clone(),
        ui: ui_tx.clone(),
        login: Arc::default(),
        share_cancel: Arc::default(),
        bug_cancel: bug_cancel.clone(),
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
    // `rlm` cells as the TS client highlights them (highlight.js's Python grammar, theme.ts `highlightCode`); the
    // syntect highlighter of the QoL settings replaces it when that is on.
    let highlight = interactive::rlm::highlight::rlm_highlighter(&theme);
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
        highlight,
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
    // Inline tool images: Kitty, on the alternate screen (TypeScript's alternate screen turns iTerm2 images off) and in
    // the split viewport (drawn into scrollback as TypeScript's main screen draws them).
    let wezterm = (env.var)("WEZTERM_PANE").is_some_and(|v| !v.is_empty())
        || (env.var)("TERM_PROGRAM").is_some_and(|v| v.eq_ignore_ascii_case("wezterm"));
    app.set_image_protocol(caps.images.filter(|p| *p == tui::image::ImageProtocol::Kitty), wezterm);
    // The theme setting (custom themes from `<agent dir>/themes`, automatic light/dark pairs) and the presentation
    // settings the TUI applies to itself.
    let mut registry = tui::theme::registry::ThemeRegistry::new(
        std::path::PathBuf::new(),
        env.agent_dir().join("themes"),
        ColorMode::from_capabilities(&caps),
    );
    // `setRegisteredThemes(resourceLoader.getThemes().themes)`: the themes the settings and theme directories register.
    let _ = registry.set_registered_themes(crate::themes::load(env, registry.mode).themes);
    app.set_theme_controller(interactive::theme_setup::ThemeController::new(
        registry,
        settings.theme(),
        (env.var)("COLORFGBG").as_deref(),
    ));
    app.apply_startup_settings(&Value::Object(settings.settings().clone()), &|k| (env.var)(k));
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
    // `checkForNewVersion`: the registry's latest release, off the UI thread; the transcript shows a newer one.
    {
        let (env, ui) = (env.clone(), ui_tx.clone());
        std::thread::spawn(move || {
            if let Some(version) = crate::startup_checks::check_for_new_version(&env, &crate::version()) {
                let notice = interactive::lane::StartupNotice::NewVersion(version);
                let _ = ui.send(UiEvent::Lane(LaneEvent::StartupNotice(notice)));
            }
        });
    }
    {
        let wake = ui_tx.clone();
        app.set_autocomplete_waker(move || {
            let _ = wake.send(UiEvent::Wake);
        });
    }
    spawn_gated_input(ui_tx, app.input_gate());
    let (debug, termux) = terminal_debug(env, std::path::Path::new(tui::render_debug::RENDER_DUMP_DIR));
    app.set_terminal_debug(debug, termux);
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
            all_messages_text: "hi".into(),
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
    fn cli_interactive_run_retry_events_become_tui_events() {
        assert_eq!(
            tui_events(
                &json!({"type": "retry_scheduled", "lane": "main", "attempt": 2, "maxAttempts": 3,
                "delayMs": 4000, "errorMessage": "Overloaded"})
            ),
            [LaneEvent::AutoRetryStart {
                attempt: 2,
                max_attempts: 3,
                delay_ms: 4000
            }]
        );
        assert_eq!(
            tui_events(&json!({"type": "retry_end", "success": true, "attempt": 2})),
            [LaneEvent::AutoRetryEnd {
                attempt: 2,
                failed: None
            }]
        );
        assert_eq!(
            tui_events(&json!({"type": "retry_end", "success": false, "attempt": 3, "finalError": "Overloaded"})),
            [LaneEvent::AutoRetryEnd {
                attempt: 3,
                failed: Some("Overloaded".into())
            }]
        );
    }

    #[test]
    fn cli_interactive_summarization_retry_events_become_tui_events() {
        assert_eq!(
            tui_events(
                &json!({"type": "summarization_retry_scheduled", "lane": "main", "attempt": 1, "maxAttempts": 3,
                "delayMs": 2000, "errorMessage": "Overloaded"})
            ),
            [LaneEvent::SummarizationRetryScheduled {
                attempt: 1,
                max_attempts: 3,
                delay_ms: 2000,
                error: "Overloaded".into()
            }]
        );
        assert_eq!(
            tui_events(
                &json!({"type": "summarization_retry_attempt_start", "source": "compaction", "reason": "threshold"})
            ),
            [LaneEvent::SummarizationRetryStarted {
                branch_summary: false,
                reason: "threshold".into()
            }]
        );
        assert_eq!(
            tui_events(&json!({"type": "summarization_retry_attempt_start", "source": "branchSummary"})),
            [LaneEvent::SummarizationRetryStarted {
                branch_summary: true,
                reason: "manual".into()
            }]
        );
        assert_eq!(
            tui_events(&json!({"type": "summarization_retry_finished", "lane": "main"})),
            [LaneEvent::SummarizationRetryFinished]
        );
    }

    #[test]
    fn cli_interactive_compaction_and_thinking_events_become_tui_events() {
        assert_eq!(
            tui_events(&json!({"type": "compaction_start", "reason": "manual"})),
            [LaneEvent::CompactionStarted {
                reason: "manual".into()
            }]
        );
        assert_eq!(
            tui_events(&json!({"type": "compaction_start", "reason": "overflow"})),
            [LaneEvent::CompactionStarted {
                reason: "overflow".into()
            }]
        );
        // An automatic compaction that left no entry ends the indicator; a completed one ends with its entry, a
        // manual one with its operation's answer.
        assert_eq!(
            tui_events(
                &json!({"type": "compaction_end", "reason": "threshold", "status": "failed", "error": {"message": "Auto-compaction failed: boom"}})
            ),
            [LaneEvent::CompactionEnded {
                status: "failed".into(),
                error: Some("Auto-compaction failed: boom".into())
            }]
        );
        assert_eq!(
            tui_events(&json!({"type": "compaction_end", "reason": "threshold", "status": "declined"})),
            [LaneEvent::CompactionEnded {
                status: "declined".into(),
                error: None
            }]
        );
        assert!(
            tui_events(&json!({"type": "compaction_end", "reason": "threshold", "status": "completed"})).is_empty()
        );
        assert!(tui_events(&json!({"type": "compaction_end", "reason": "manual", "status": "failed"})).is_empty());
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
    fn cli_interactive_agent_hub_notices_and_parked_subagents() {
        assert_eq!(
            agent_notice("t1", "steer", Ok(Some(json!({"state": "delivered", "steered": true})))),
            "Steered \"t1\"."
        );
        assert_eq!(
            agent_notice("t1", "steer", Ok(Some(json!({"state": "pending", "steered": false})))),
            "\"t1\" has no live run: the message waits in its inbox."
        );
        assert_eq!(
            agent_notice("t1", "revive", Ok(Some(json!({"id": "t2", "revived": "t1"})))),
            "Revived \"t1\" as \"t2\"."
        );
        assert_eq!(
            agent_notice(
                "t1",
                "steer",
                Err("session_operation_failed: Receiver task has already finished".into())
            ),
            "Error: Receiver task has already finished"
        );
        use interactive::agent_hub::AgentStatus;
        let listed = json!({"subagents": [
            {"id": "a", "state": "running"}, {"id": "b", "state": "interrupted"},
            {"id": "c", "state": "cancelled"}, {"id": "d", "state": "completed"}
        ]});
        let statuses: Vec<AgentStatus> = agent_views(&listed).iter().map(|v| v.status).collect();
        assert_eq!(
            statuses,
            [
                AgentStatus::Running,
                AgentStatus::Parked,
                AgentStatus::Aborted,
                AgentStatus::Idle
            ]
        );
    }

    #[test]
    fn cli_interactive_watch_events_become_tui_events() {
        // Only the link's report of a worker restart starts the TUI's recovery (not a lost server or a rejoin).
        use crate::link::{WORKER_RESTARTED, connection_event};
        assert!(is_worker_restart(&connection_event(
            "reconnecting",
            Some(WORKER_RESTARTED)
        )));
        assert!(!is_worker_restart(&connection_event(
            "reconnecting",
            Some("The Ultron server went away")
        )));
        assert!(!is_worker_restart(&connection_event("rejoined", None)));
        let queued = json!({"type": "queue_update", "lane": "main", "queues": [
            {"type": "message", "kind": "steer", "entryId": "e1", "message": {"role": "user", "content": [{"type": "text", "text": "go"}]}}
        ]});
        assert_eq!(
            tui_events(&queued),
            [LaneEvent::Queues(vec![Queued {
                entry_id: "e1".into(),
                kind: QueueKind::Steer,
                text: "go".into(),
                user: true,
            }])]
        );
        let next = json!({"type": "queue_update", "lane": "main", "queues": [
            {"type": "message", "kind": "nextRun", "entryId": "e2", "message": {"role": "user", "content": [{"type": "text", "text": "later"}]}},
            {"type": "message", "kind": "followUp", "entryId": "e3", "message": {"role": "user", "content": [{"type": "text", "text": "after"}]}}
        ]});
        assert_eq!(
            tui_events(&next),
            [LaneEvent::Queues(vec![
                Queued {
                    entry_id: "e2".into(),
                    kind: QueueKind::NextRun,
                    text: "later".into(),
                    user: true,
                },
                Queued {
                    entry_id: "e3".into(),
                    kind: QueueKind::FollowUp,
                    text: "after".into(),
                    user: true,
                }
            ])]
        );
        // Queued host items (client-tui-chat.ts `#syncQueues`): a custom-role message shows no text, a non-message
        // item its `<customType>`; neither is the user's.
        let host = json!({"type": "queue_update", "lane": "main", "queues": [
            {"type": "message", "kind": "followUp", "entryId": "r1", "message": {"role": "custom", "customType": "wake", "content": "event"}},
            {"type": "custom", "kind": "steer", "entryId": "r2", "customType": "ultron-wake"}
        ]});
        assert_eq!(
            tui_events(&host),
            [LaneEvent::Queues(vec![
                Queued {
                    entry_id: "r1".into(),
                    kind: QueueKind::FollowUp,
                    text: String::new(),
                    user: false,
                },
                Queued {
                    entry_id: "r2".into(),
                    kind: QueueKind::Steer,
                    text: "<ultron-wake>".into(),
                    user: false,
                }
            ])]
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
