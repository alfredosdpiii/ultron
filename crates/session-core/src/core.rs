//! The session actor: one session (format-4 journal), its lane (the `rlm` kernel over the composition root's host
//! stack) and the run in progress, owned by one thread. Every client operation is an [`Op`] answered through its
//! reply; the run's progress is reported as TypeScript lane-watch events (`run_start`, `message_*`, `tool_*`,
//! `entry_added`, `queue_update`, `compaction_*`, `config_update`, `run_end`) through the event sink, which the
//! session worker folds into its `pi.transcript` state.
//!
//! The run executes the agent loop on a thread of its own; its events come back to the actor as [`Input::Agent`],
//! so operations keep being answered while a run streams. Runs and compactions are journaled as the harness journals
//! them ([`crate::journal`], [`crate::compaction_op`]); reads see the session as Pi entries ([`crate::pi_view`]).

use crate::compaction_op::CompactionOp;

#[path = "navigate.rs"]
mod navigate;
use crate::compose::{Runtime, Services};
use crate::durable_store::{SharedSession, lock as lock_session};
use crate::journal::RunJournal;
use crate::lane::{self, Lane, Provider, api_supported, now_ms};
use crate::view::{self, message_text};
use crate::{ModelFlags, RunEnv, naming, pi_view};
use agent::{AgentEvent, StreamFn};
use ai::{AbortSignal, AssistantMessage, Message, Model, SimpleStreamOptions, StreamOptions};
use serde_json::{Map, Value, json};
use session_file::SessionManager;
use session_journal::repo::Metadata;
use session_journal::run::{self, PENDING_ENTRY};
use session_journal::session::{BRANCH_TIP, LANE_CONFIG, LANE_STATE};
use session_journal::{Session, SessionRepo, SystemClock, Write, names};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;

pub const LANE: &str = "main";

/// `InvalidMessage` (`reason: "empty"`) of a queued message without text or image (lane.ts `enqueue`).
pub const EMPTY_QUEUED: &str = "Queued input must contain text or an image";

/// A queue a message can join while a run streams.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueKind {
    Steer,
    FollowUp,
    /// Held until the next run starts (the agent controller's `nextRun`): the run in progress never takes it, and
    /// the next run's admission places it before its prompt.
    NextRun,
}

impl QueueKind {
    pub fn name(self) -> &'static str {
        match self {
            QueueKind::Steer => "steer",
            QueueKind::FollowUp => "followUp",
            QueueKind::NextRun => "nextRun",
        }
    }

    pub fn parse(name: &str) -> Option<QueueKind> {
        match name {
            "steer" => Some(QueueKind::Steer),
            "followUp" => Some(QueueKind::FollowUp),
            "nextRun" => Some(QueueKind::NextRun),
            _ => None,
        }
    }
}

/// What an operation answers: data for the response (`None`: no data), or the error text.
pub type OpResult = Result<Option<Value>, String>;
/// Where an operation's answer goes (called once, possibly later, from the actor's thread).
pub type Reply = Box<dyn FnOnce(OpResult) + Send>;

/// The operations a client sends (the commands of `rpc-native.ts` the session worker serves, and the agent
/// controller's members). Session selection (new, switch, fork, clone) is the server's: clients re-attach.
#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    /// Start a run; with a run in progress the message joins `queue` (none: refused).
    Prompt {
        message: String,
        images: Vec<Value>,
        queue: Option<QueueKind>,
    },
    /// Start a run and answer when it ends (`{accepted, operationId, error}`), as the agent controller does.
    PromptAndWait {
        message: String,
        images: Vec<Value>,
    },
    /// Queue a message (as `entry_id` when the client chose it); answers its entry id.
    Enqueue {
        kind: QueueKind,
        message: String,
        images: Vec<Value>,
        entry_id: Option<String>,
    },
    /// Drop one queued message; answers whether it was queued.
    CancelQueued {
        entry_id: String,
    },
    /// Empty the queues: `{steering, followUp}` texts.
    ClearQueue,
    Abort,
    State,
    AvailableModels,
    SetModel {
        provider: String,
        model_id: String,
    },
    CycleModel,
    ThinkingLevels,
    SetThinking {
        level: String,
    },
    CycleThinking,
    SetQueueMode {
        kind: QueueKind,
        mode: String,
    },
    Compact {
        instructions: Option<String>,
    },
    SetAutoCompaction(bool),
    SetAutoRetry(bool),
    AbortRetry,
    /// A user's own command (RPC `bash`, the TUI's `!`/`!!`): the extensions' `user_bash` first, then the shell.
    /// `id` tags its streamed output (`bash_execution_update`), as AgentSession's `executeBash` `options.id`.
    Bash {
        command: String,
        exclude: bool,
        id: Option<String>,
    },
    AbortBash,
    /// `/bug`'s summary written by the session's model (AgentSession `summarizeForBugReport`): the report text.
    BugSummary {
        hint: Option<String>,
    },
    /// Cancel the bug report summary in progress (its loader's Esc).
    AbortBugSummary,
    /// What a shared or reported session carries besides its entries (session-share.ts
    /// `createShareTrailingEntries`): `{ systemPrompt, tools: [{ name, description, parameters }] }`.
    ShareContext,
    /// `sessionManager.appendCustomEntry(customType, data)` (`/bug` records its report): after the run in progress.
    AppendCustom {
        custom_type: String,
        data: Value,
    },
    Stats,
    ForkMessages,
    Entries {
        since: Option<String>,
    },
    Tree,
    LastAssistantText,
    SetName {
        name: String,
    },
    Messages,
    /// The text of the user message `entry_id`, which a fork starts before (an error for any other entry).
    ForkSource {
        entry_id: String,
    },
    /// The current Pi leaf (an error without one), which a clone copies to.
    CloneSource,
    /// Record the session-start marker in the forked session file `target` (`control.forked`).
    Forked {
        target: String,
    },
    /// The whole session as the TUI shows it: `{sessionId, sessionFile, name, messages, model, thinkingLevel}`.
    Describe,
    /// Sessions of this working directory, newest first (`{path, id, name, modified, messageCount, firstMessage,
    /// allMessagesText, cwd}`).
    ListSessions,
    /// Continue what a dead worker left (crate::recovery): the first queued message runs, else an interrupted run
    /// continues. Answers `{resumed, runId, reason, recovered}`; nothing starts while a run is in progress.
    Recover,
    /// The subagents of the root lane (`rlm.list_subagents` as the root sees them: `{subagents: [...]}`), for the
    /// TUI's agent hub.
    Agents,
    /// Move `main`'s tip to `target_id` (`None`: the root), optionally summarizing the branch left (`/tree`), and
    /// label the target (or the branch summary).
    Navigate {
        target_id: Option<String>,
        summarize: bool,
        instructions: Option<String>,
        label: Option<String>,
    },
    /// Label a tree entry (`None` removes the label).
    SetLabel {
        entry_id: String,
        label: Option<String>,
    },
    /// Cancel a subagent and its unfinished descendants (the agent hub's kill).
    KillAgent {
        id: String,
    },
    /// Steer a subagent with `message` (the agent hub's steer: `agent_message.send` from the root lane with
    /// `receiver_role: "child"` and `steer: true`); answers the family module's `{id, duplicate, state, steered}`.
    SteerAgent {
        id: String,
        message: String,
    },
    /// Start an interrupted subagent again from its brief (the agent hub's revive); answers `{id, revived}`.
    ReviveAgent {
        id: String,
    },
    /// A subagent's transcript (`agents.transcript`): `{id, busy, messages}`, its child conversation's messages.
    AgentTranscript {
        id: String,
    },
    /// `/goal <args>`: the session goal's command (set, pause, resume, clear, check); answers its text.
    Goal {
        args: String,
    },
    /// Start another root conversation in the session's durable runtime with `message` (Pi Durable's ownerless
    /// conversations: each runs, survives restarts and resumes on its own); answers `{conversationId}`.
    StartConversation {
        message: String,
    },
    /// Give conversation `id` another input (queued while it runs).
    ConversationInput {
        id: u64,
        message: String,
    },
    /// Conversation `id`: `{id, busy, messages: [{role, text}]}`; without an id, every root conversation's.
    Conversations {
        id: Option<u64>,
    },
    /// Root-owned work whose completion may still re-invoke the root (`async.pending`): running jobs and subagents
    /// of the root lane, events waiting for delivery, a run in progress.
    AsyncPending,
    /// A read-only inspection request (crate::inspect: `agents.status`, `memory.list`, `rlm.frames`, ...) answered
    /// by the session's host stack as the root lane's; anything else is refused.
    Inspect {
        request: String,
        payload: Value,
    },
    /// The session's slash commands (RPC `get_commands`: crate::commands).
    Commands,
    /// Runs an extension's command `/name args` (`pi.legacy-extension-commands.run`), answered when its handler
    /// returned; the actor goes on meanwhile.
    ExtensionCommand {
        name: String,
        args: String,
    },
    /// An extension command's argument completions for `prefix` (`commands/complete`), off the actor.
    ExtensionCompletions {
        name: String,
        prefix: String,
    },
    /// The extensions' keyboard shortcuts the TUI binds under its resolved keybindings (action, keys) and the
    /// conflict warnings (runner.ts `getShortcuts`, `getShortcutDiagnostics`): `{shortcuts: [{key, description,
    /// extensionPath}], diagnostics: [{type, message, path}]}`.
    ExtensionShortcuts {
        keybindings: Vec<(String, Vec<String>)>,
    },
    /// Runs the extension shortcut bound to `key` under `keybindings` (`shortcuts/run`), answered when its handler
    /// returned; the actor goes on meanwhile.
    ExtensionShortcut {
        key: String,
        keybindings: Vec<(String, Vec<String>)>,
    },
    /// Pi's `session_before_fork` before a client forks (`position` "before") or clones ("at") the session at
    /// `entry_id`: `{cancelled}` (control `beforeFork`), off the actor.
    BeforeFork {
        entry_id: String,
        position: String,
    },
}

impl Op {
    /// The operation a JSON request names (`{op, ...}`), as clients send it over the worker's control service.
    pub fn from_json(value: &Value) -> Result<Op, String> {
        let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
        let need = |key: &str| text(key).ok_or_else(|| format!("{key} must be a string"));
        let images = || value["images"].as_array().cloned().unwrap_or_default();
        let kind = |key: &str| text(key).as_deref().and_then(QueueKind::parse);
        let op = value["op"].as_str().ok_or("op must be a string")?;
        Ok(match op {
            "prompt" => Op::Prompt {
                message: need("message")?,
                images: images(),
                queue: kind("queue"),
            },
            "promptAndWait" => Op::PromptAndWait {
                message: need("message")?,
                images: images(),
            },
            "enqueue" => Op::Enqueue {
                kind: kind("kind").ok_or("kind must be steer, followUp or nextRun")?,
                message: need("message")?,
                images: images(),
                entry_id: text("entryId"),
            },
            "cancelQueued" => Op::CancelQueued {
                entry_id: need("entryId")?,
            },
            "clearQueue" => Op::ClearQueue,
            "abort" => Op::Abort,
            "state" => Op::State,
            "availableModels" => Op::AvailableModels,
            "setModel" => Op::SetModel {
                provider: need("provider")?,
                model_id: need("modelId")?,
            },
            "cycleModel" => Op::CycleModel,
            "thinkingLevels" => Op::ThinkingLevels,
            "setThinking" => Op::SetThinking { level: need("level")? },
            "cycleThinking" => Op::CycleThinking,
            "setQueueMode" => Op::SetQueueMode {
                kind: kind("kind")
                    .filter(|kind| *kind != QueueKind::NextRun)
                    .ok_or("kind must be steer or followUp")?,
                mode: need("mode")?,
            },
            "compact" => Op::Compact {
                instructions: text("instructions"),
            },
            "setAutoCompaction" => Op::SetAutoCompaction(value["enabled"] == true),
            "setAutoRetry" => Op::SetAutoRetry(value["enabled"] == true),
            "abortRetry" => Op::AbortRetry,
            "bash" => Op::Bash {
                command: need("command")?,
                exclude: value["exclude"] == true,
                id: value["id"].as_str().map(str::to_string),
            },
            "abortBash" => Op::AbortBash,
            "bugSummary" => Op::BugSummary {
                hint: value["hint"].as_str().map(str::to_string),
            },
            "abortBugSummary" => Op::AbortBugSummary,
            "shareContext" => Op::ShareContext,
            "appendCustom" => Op::AppendCustom {
                custom_type: need("customType")?,
                data: value["data"].clone(),
            },
            "stats" => Op::Stats,
            "forkMessages" => Op::ForkMessages,
            "entries" => Op::Entries { since: text("since") },
            "tree" => Op::Tree,
            "lastAssistantText" => Op::LastAssistantText,
            "setName" => Op::SetName { name: need("name")? },
            "messages" => Op::Messages,
            "forkSource" => Op::ForkSource {
                entry_id: need("entryId")?,
            },
            "cloneSource" => Op::CloneSource,
            "forked" => Op::Forked {
                target: need("target")?,
            },
            "describe" => Op::Describe,
            "listSessions" => Op::ListSessions,
            "recover" => Op::Recover,
            "agents" => Op::Agents,
            "setLabel" => Op::SetLabel {
                entry_id: need("entryId")?,
                label: text("label"),
            },
            "navigate" => Op::Navigate {
                target_id: text("targetId"),
                summarize: value["summarize"] == true,
                instructions: text("customInstructions"),
                label: text("label"),
            },
            "killAgent" => Op::KillAgent { id: need("id")? },
            "steerAgent" => Op::SteerAgent {
                id: need("id")?,
                message: need("message")?,
            },
            "reviveAgent" => Op::ReviveAgent { id: need("id")? },
            "agentTranscript" => Op::AgentTranscript { id: need("id")? },
            "goal" => Op::Goal {
                args: text("args").unwrap_or_default(),
            },
            "startConversation" => Op::StartConversation {
                message: need("message")?,
            },
            "conversationInput" => Op::ConversationInput {
                id: value["id"].as_u64().ok_or("id must be a conversation id")?,
                message: need("message")?,
            },
            "conversations" => Op::Conversations {
                id: value["id"].as_u64(),
            },
            "asyncPending" => Op::AsyncPending,
            "commands" => Op::Commands,
            "extensionCompletions" => Op::ExtensionCompletions {
                name: need("name")?,
                prefix: value["prefix"].as_str().unwrap_or_default().to_string(),
            },
            "extensionCommand" => Op::ExtensionCommand {
                name: need("name")?,
                args: value["args"].as_str().unwrap_or_default().to_string(),
            },
            "extensionShortcuts" => Op::ExtensionShortcuts {
                keybindings: keybindings_from_json(&value["keybindings"]),
            },
            "extensionShortcut" => Op::ExtensionShortcut {
                key: need("key")?,
                keybindings: keybindings_from_json(&value["keybindings"]),
            },
            "beforeFork" => Op::BeforeFork {
                entry_id: need("entryId")?,
                position: need("position")?,
            },
            "inspect" => Op::Inspect {
                request: need("request")?,
                payload: value.get("payload").cloned().unwrap_or(Value::Null),
            },
            other => return Err(format!("Unknown session operation: {other}")),
        })
    }

    /// The JSON request for this operation ([`Op::from_json`] reads it back).
    pub fn to_json(&self) -> Value {
        match self {
            Op::Prompt { message, images, queue } => {
                json!({"op": "prompt", "message": message, "images": images, "queue": queue.map(QueueKind::name)})
            }
            Op::PromptAndWait { message, images } => {
                json!({"op": "promptAndWait", "message": message, "images": images})
            }
            Op::Enqueue {
                kind,
                message,
                images,
                entry_id,
            } => {
                json!({"op": "enqueue", "kind": kind.name(), "message": message, "images": images, "entryId": entry_id})
            }
            Op::CancelQueued { entry_id } => json!({"op": "cancelQueued", "entryId": entry_id}),
            Op::ClearQueue => json!({"op": "clearQueue"}),
            Op::Abort => json!({"op": "abort"}),
            Op::State => json!({"op": "state"}),
            Op::AvailableModels => json!({"op": "availableModels"}),
            Op::SetModel { provider, model_id } => json!({"op": "setModel", "provider": provider, "modelId": model_id}),
            Op::CycleModel => json!({"op": "cycleModel"}),
            Op::ThinkingLevels => json!({"op": "thinkingLevels"}),
            Op::SetThinking { level } => json!({"op": "setThinking", "level": level}),
            Op::CycleThinking => json!({"op": "cycleThinking"}),
            Op::SetQueueMode { kind, mode } => json!({"op": "setQueueMode", "kind": kind.name(), "mode": mode}),
            Op::Compact { instructions } => json!({"op": "compact", "instructions": instructions}),
            Op::SetAutoCompaction(enabled) => json!({"op": "setAutoCompaction", "enabled": enabled}),
            Op::SetAutoRetry(enabled) => json!({"op": "setAutoRetry", "enabled": enabled}),
            Op::AbortRetry => json!({"op": "abortRetry"}),
            Op::Bash { command, exclude, id } => {
                let mut op = json!({"op": "bash", "command": command, "exclude": exclude});
                if let Some(id) = id {
                    op["id"] = json!(id);
                }
                op
            }
            Op::AbortBash => json!({"op": "abortBash"}),
            Op::BugSummary { hint } => json!({"op": "bugSummary", "hint": hint}),
            Op::AbortBugSummary => json!({"op": "abortBugSummary"}),
            Op::ShareContext => json!({"op": "shareContext"}),
            Op::AppendCustom { custom_type, data } => {
                json!({"op": "appendCustom", "customType": custom_type, "data": data})
            }
            Op::Stats => json!({"op": "stats"}),
            Op::ForkMessages => json!({"op": "forkMessages"}),
            Op::Entries { since } => json!({"op": "entries", "since": since}),
            Op::Tree => json!({"op": "tree"}),
            Op::LastAssistantText => json!({"op": "lastAssistantText"}),
            Op::SetName { name } => json!({"op": "setName", "name": name}),
            Op::Messages => json!({"op": "messages"}),
            Op::ForkSource { entry_id } => json!({"op": "forkSource", "entryId": entry_id}),
            Op::CloneSource => json!({"op": "cloneSource"}),
            Op::Forked { target } => json!({"op": "forked", "target": target}),
            Op::Describe => json!({"op": "describe"}),
            Op::ListSessions => json!({"op": "listSessions"}),
            Op::Recover => json!({"op": "recover"}),
            Op::Agents => json!({"op": "agents"}),
            Op::SetLabel { entry_id, label } => json!({"op": "setLabel", "entryId": entry_id, "label": label}),
            Op::Navigate {
                target_id,
                summarize,
                instructions,
                label,
            } => {
                json!({"op": "navigate", "targetId": target_id, "summarize": summarize, "customInstructions": instructions, "label": label})
            }
            Op::KillAgent { id } => json!({"op": "killAgent", "id": id}),
            Op::SteerAgent { id, message } => json!({"op": "steerAgent", "id": id, "message": message}),
            Op::ReviveAgent { id } => json!({"op": "reviveAgent", "id": id}),
            Op::AgentTranscript { id } => json!({"op": "agentTranscript", "id": id}),
            Op::Goal { args } => json!({"op": "goal", "args": args}),
            Op::StartConversation { message } => json!({"op": "startConversation", "message": message}),
            Op::ConversationInput { id, message } => json!({"op": "conversationInput", "id": id, "message": message}),
            Op::Conversations { id } => json!({"op": "conversations", "id": id}),
            Op::AsyncPending => json!({"op": "asyncPending"}),
            Op::Inspect { request, payload } => json!({"op": "inspect", "request": request, "payload": payload}),
            Op::Commands => json!({"op": "commands"}),
            Op::ExtensionCommand { name, args } => json!({"op": "extensionCommand", "name": name, "args": args}),
            Op::ExtensionCompletions { name, prefix } => {
                json!({"op": "extensionCompletions", "name": name, "prefix": prefix})
            }
            Op::BeforeFork { entry_id, position } => {
                json!({"op": "beforeFork", "entryId": entry_id, "position": position})
            }
            Op::ExtensionShortcuts { keybindings } => {
                json!({"op": "extensionShortcuts", "keybindings": keybindings_json(keybindings)})
            }
            Op::ExtensionShortcut { key, keybindings } => {
                json!({"op": "extensionShortcut", "key": key, "keybindings": keybindings_json(keybindings)})
            }
        }
    }
}

/// Resolved keybindings as JSON: `{action: [keys]}` in order.
fn keybindings_json(keybindings: &[(String, Vec<String>)]) -> Value {
    Value::Object(
        keybindings
            .iter()
            .map(|(action, keys)| (action.clone(), json!(keys)))
            .collect(),
    )
}

fn keybindings_from_json(value: &Value) -> Vec<(String, Vec<String>)> {
    value
        .as_object()
        .into_iter()
        .flatten()
        .map(|(action, keys)| {
            let keys = keys
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|k| k.as_str().map(str::to_string))
                .collect();
            (action.clone(), keys)
        })
        .collect()
}

/// `getShortcuts` and `getShortcutDiagnostics` of the session's extensions as [`Op::ExtensionShortcuts`] answers.
pub fn extension_shortcuts(
    host: Option<&Arc<extensions::ExtensionHost>>,
    keybindings: &[(String, Vec<String>)],
) -> Value {
    extension_issues(&[], host, keybindings)
}

/// [`extension_shortcuts`] after the extensions that did not load (`resourceLoader.getExtensions().errors`, type
/// `error`), as interactive-mode.ts lists them first under `[Extension issues]`.
pub fn extension_issues(
    load_errors: &[extensions::manifest::Diagnostic],
    host: Option<&Arc<extensions::ExtensionHost>>,
    keybindings: &[(String, Vec<String>)],
) -> Value {
    let resolved = host.map(|host| host.shortcuts(keybindings)).unwrap_or_default();
    let errors = load_errors
        .iter()
        .map(|d| json!({"type": "error", "message": d.message, "path": d.path.display().to_string()}));
    json!({
        "shortcuts": resolved
            .shortcuts
            .iter()
            .map(|(_, s)| json!({"key": s.key, "description": s.description, "extensionPath": s.extension_path}))
            .collect::<Vec<_>>(),
        "diagnostics": errors
            .chain(
                resolved
                    .diagnostics
                    .iter()
                    .map(|(message, path)| json!({"type": "warning", "message": message, "path": path})),
            )
            .collect::<Vec<_>>(),
    })
}

/// What reaches the actor.
pub enum Input {
    Op(Op, Reply),
    Agent(Box<AgentEvent>),
    /// The model's title for the session arrived.
    Title(Box<AssistantMessage>),
    RunEnd,
    /// A user's command ended: its `BashResult` (camelCase, as recorded), or the error that failed it.
    BashDone {
        command: String,
        exclude: bool,
        result: Result<BashOutcome, String>,
        reply: Reply,
    },
    /// A chunk of a running user command's output (`bash_execution_update`).
    BashOutput {
        id: Option<String>,
        delta: String,
    },
    /// An extension handler failed (the router's error listeners: `extension_error`).
    ExtensionError(Value),
    /// Stop the lane; the sender is told when done.
    Close(std::sync::mpsc::Sender<()>),
    /// Stop the lane for a successor worker (its shell jobs keep running for it); the sender is told when done.
    HandOver(std::sync::mpsc::Sender<()>),
    /// The completion-event dispatcher's question about the root lane (crate::async_events).
    EventLane(event_lanes::LaneCall, std::sync::mpsc::Sender<event_lanes::LaneAnswer>),
    /// Report whether detached work is active (the worker keeps itself alive while it is).
    ActivityTick,
    /// The durable runtime running the turn (crate::turns).
    Turn(crate::turns::Request),
    /// The durable runtime has no work for the root conversation.
    HarnessIdle,
    /// A compaction's work off the actor ended (core::compacting).
    Woken,
    /// An extension's request that needs the actor (crate::extension_host).
    Extension(crate::extension_host::Call),
}

/// How a user's command ended (crate::user_bash).
pub enum BashOutcome {
    /// It ran, or an extension's `user_bash` answered it: the `BashResult` to record and answer with.
    Ran(Value),
    /// An extension's `user_bash` failed it (TypeScript fails closed): nothing is recorded; the message is the error
    /// the extension runner reported.
    FailedClosed(String),
}

/// Where lane-watch events go.
pub type EventSink = Box<dyn FnMut(Value) + Send>;

#[derive(Clone)]
struct Queued {
    kind: QueueKind,
    message: Value,
    /// The pending entry (`pi.pending.entry`) the message waits as; it becomes the message's entry id.
    entry_id: String,
    /// The run's turn the loop took it for (`turn_start`s it had emitted, plus one); 0 while queued.
    turn: u64,
    /// The durable runtime's submission it waits as (none while no run is in progress).
    submission: Option<durable::SubmissionId>,
}

/// The steering and follow-up queues, drained by the running loop. `delivered` holds drained messages until their
/// `message_end` reaches the actor, so clients see them queued until then.
struct Queues {
    items: Vec<Queued>,
    delivered: Vec<Queued>,
    steering_all: bool,
    follow_up_all: bool,
}

impl Queues {
    fn visible(&self) -> Vec<&Queued> {
        self.delivered.iter().chain(&self.items).collect()
    }
}

/// `selectAcceptedInbox` (lane.ts): the queued messages a run's admission takes, in queue order: every next-run
/// message and, when `take_queued`, every steering and follow-up message (only the first of a kind whose mode is
/// one-at-a-time). Messages already waiting in the durable runtime's inbox belong to a run and are never taken.
fn select_accepted(queues: &Queues, take_queued: bool) -> Vec<(String, Value)> {
    let (mut steer_taken, mut follow_up_taken) = (false, false);
    let mut selected = Vec::new();
    for item in queues.items.iter().filter(|q| q.submission.is_none()) {
        let eligible = match item.kind {
            QueueKind::NextRun => true,
            QueueKind::Steer => take_queued && (queues.steering_all || !steer_taken),
            QueueKind::FollowUp => take_queued && (queues.follow_up_all || !follow_up_taken),
        };
        if eligible {
            steer_taken |= item.kind == QueueKind::Steer;
            follow_up_taken |= item.kind == QueueKind::FollowUp;
            selected.push((item.entry_id.clone(), item.message.clone()));
        }
    }
    selected
}

fn lock<T>(shared: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Running {
    id: String,
    signal: AbortSignal,
    journal: RunJournal,
    /// The run's prompt (its transcript message), announced when the first generation starts.
    prompt: Value,
    /// The queued messages its admission placed before the prompt, announced with it.
    captured: Vec<Value>,
    /// The durable runtime's submission of the prompt.
    submission: Option<durable::SubmissionId>,
    /// The run's own prompt has not reached `message_end` yet (later user messages are steering or follow-ups).
    prompt_pending: bool,
    /// The `turn_start`s the actor has processed; the loop counts its own in `Queues::turns`.
    turns: u64,
    started_tip: Value,
    /// The last assistant message's error, if the run ended on one.
    error: Option<String>,
    /// Answered when the run ends (`PromptAndWait`).
    waiter: Option<Reply>,
    /// A client aborted the run (it ends `aborted`, whatever its last response says).
    cancelled: bool,
    /// An automatic compaction failed: the run ends with its error and compacts no more.
    compaction_failed: bool,
    /// The worker died during an overflow compaction of this run: the next checkpoint compacts first, resuming
    /// this `resumeAfter` (crate::core::auto_compact).
    resume_overflow: Option<Value>,
}

/// How a session is opened.
pub struct OpenOptions {
    pub env: RunEnv,
    pub flags: ModelFlags,
    /// The session root (`<root>/--<cwd>--/<file>.jsonl`).
    pub session_root: PathBuf,
    pub metadata: Metadata,
    /// `print`, `json`, `rpc`, `interactive` (extensions see it).
    pub mode: String,
    pub services: Services,
    /// Name the session from its first prompt (and by the model's title); off for sessions removed at the end.
    pub auto_name: bool,
}

/// What a response did to its generation's retries.
enum RetryNote {
    /// A retry was scheduled: `{step, attempt, maxAttempts, delayMs, notBefore, errorMessage}`.
    Scheduled(Value),
    /// A retried generation ended: `{step, attempt, success, finalError?}`.
    Ended(Value),
}

/// The session actor.
pub struct Core {
    env: RunEnv,
    flags: ModelFlags,
    repo: SessionRepo,
    runtime: Runtime,
    provider: Arc<Provider>,
    model: Model,
    models: Vec<Model>,
    thinking: String,
    session: SharedSession,
    lane: Lane,
    tx: Sender<Input>,
    sink: EventSink,
    run: Option<Running>,
    /// The run's provider message being hidden: the empty response an aborted run ends with.
    hiding: bool,
    queues: Arc<Mutex<Queues>>,
    auto_compaction: bool,
    auto_retry: bool,
    bash: Option<kernel::Signal>,
    /// The bug report summary in progress.
    bug_summary: Option<AbortSignal>,
    /// How many of the session's entries were announced (`entry_added`).
    announced: usize,
    /// How many of the session's usage rows were announced (`usage`).
    announced_usage: usize,
    /// Queued messages committed with the generation that takes them, until their `message_end` arrives.
    taken: Vec<String>,
    run_ids: session_file::Uuidv7,
    auto_name: bool,
    /// What opening the session settled (crate::recovery), until [`Op::Recover`] acts on it.
    recovered: Option<crate::recovery::Recovered>,
    /// The durable runtime that runs every turn (crate::turns).
    durable: durable_turns::Durable,
    /// The run reopened from a dead worker's journal and resumed in the durable runtime.
    resumed: Option<String>,
    /// Completions of detached work, delivered to the lanes that own it (crate::async_events).
    events: Arc<crate::async_events::Dispatcher>,
    /// Model-owned context (`ctx.*`) and collapse on return on `main` (core::context_lane).
    context: Arc<kernel_apis::ContextControl>,
    branches: Arc<context_lane::RootBranches>,
    /// The actor's input, while `run_loop` serves it (a compaction's work reads it too, core::compacting).
    inputs: Option<std::sync::mpsc::Receiver<Input>>,
    /// Inputs that arrived during a compaction and wait for its end, in order.
    deferred: std::collections::VecDeque<Input>,
    /// The compaction or branch summary in progress (manual or automatic): `abort` cancels it, `state` reports it
    /// (`isCompacting`).
    compacting: Option<AbortSignal>,
    /// The `session_before_compact` dispatch in flight: `abort` gives it up (the hooks' `signal`).
    hook: Option<extensions::CancelToken>,
    /// A tree navigation's branch summary is in progress (core::navigate).
    navigating: bool,
    /// What waits for the run in progress to end: a manual `compact` aborts the run first (AgentSession `compact`
    /// awaits `abort()`), then runs.
    after_run: Vec<Input>,
    /// The research-loop brake and the skill hint on `main`'s turns (crate::nudge).
    nudger: crate::nudge::ToolRoundNudger,
    skill_nudger: crate::nudge::SkillExtractionNudger,
}

mod auto_compact;
mod compacting;
mod extension_ops;
mod user_bash;
pub use extension_ops::before_fork;
pub use user_bash::USER_BASH_FAILED;
mod agent_ops;
pub mod budget_gate;
mod context_children;
mod context_lane;
mod durable_turns;
pub mod event_lanes;

/// A user message with text and images, as the transcript stores it.
pub fn user_message(text: String, images: Vec<Value>) -> Value {
    let mut content = vec![json!({"type": "text", "text": text})];
    content.extend(images);
    json!({"role": "user", "content": content, "timestamp": now_ms()})
}

/// A transcript message as stored: a failed tool's result carries no `details`.
fn stored(message: &Message) -> Value {
    let mut value = serde_json::to_value(message).unwrap_or(Value::Null);
    if value["role"] == "toolResult" && value["isError"] == true && value["details"] == json!({}) {
        value.as_object_mut().map(|m| m.remove("details"));
    }
    value
}

fn event(fields: Vec<(&str, Value)>) -> Value {
    Value::Object(
        fields
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect::<Map<_, _>>(),
    )
}

/// The lane configuration a session runs with: the session's own for a resumed session without model flags, else
/// the flags' (written for a new session). Returns the stored configuration and the model it names.
pub fn lane_config(flags: &ModelFlags, env: &RunEnv, session: Option<&mut Session>) -> Result<(Value, Model), String> {
    let stored = session
        .as_ref()
        .and_then(|s| s.value(session_journal::session::LANE_CONFIG, "main").cloned());
    // Without --model a resumed session restores its own model; a new one starts on the settings' default
    // (sdk.ts: restore from the session first, then `findInitialModel`).
    // A `--model <pattern>:<thinking>` suffix or a scoped pattern's level names the thinking level unless
    // `--thinking` does; otherwise the settings' level for the model.
    let choice = match (&flags.model, &stored) {
        (None, Some(_)) => Err("No model selected: pass --model <provider>/<id>".to_string()),
        _ => lane::resolve_choice(flags, env, stored.is_some()),
    };
    let thinking = match (&flags.thinking, &choice) {
        (Some(level), _) => level.clone(),
        (None, Ok(choice)) => choice
            .thinking
            .clone()
            .unwrap_or_else(|| lane::initial_thinking(&choice.model, env)),
        (None, Err(_)) => "medium".into(),
    };
    let flag_model = choice.map(|choice| choice.model);
    let wanted = match (&flag_model, stored) {
        (Ok(m), _) => run::lane_config(&m.provider, &m.id, &thinking, &["rlm"]),
        (Err(_), Some(mut stored)) => {
            stored["activeToolNames"] = json!(["rlm"]);
            stored
        }
        (Err(e), None) => return Err(e.clone()),
    };
    let config = match session {
        Some(session) => run::ensure_lane(session, &wanted)?,
        None => wanted,
    };
    let model = match (
        config["model"]["provider"].as_str(),
        config["model"]["modelId"].as_str(),
        flag_model,
    ) {
        (Some(provider), Some(id), Ok(m)) if (provider, id) == (m.provider.as_str(), m.id.as_str()) => m,
        (Some(provider), Some(id), _) => lane::catalog_model(provider, id, env)?,
        (_, _, m) => m?,
    };
    Ok((config, model))
}

/// Apply a naming response: its usage as a row of its own, and its title while the name is still automatic.
pub fn record_naming(session: &mut Session, response: &AssistantMessage) -> Result<(), String> {
    let mut row = Map::new();
    row.insert(
        "id".into(),
        session_journal::lock_clock(&session.clock()).next_id().into(),
    );
    row.insert(
        "usage".into(),
        serde_json::to_value(&response.usage).map_err(|e| e.to_string())?,
    );
    row.insert("adjustment".into(), false.into());
    row.insert(
        "details".into(),
        json!({"source": "session-naming", "model": format!("{}/{}", response.provider, response.model)}),
    );
    session.commit(&[Write::Usage(row)])?;
    if let Some(title) = naming::title_of(response) {
        names::replace_auto_name(session, &title)?;
    }
    Ok(())
}

impl Core {
    /// Open `options.metadata`'s session with its lane. `tx` is the actor's own input (runs report through it).
    pub fn open(options: OpenOptions, tx: Sender<Input>, sink: EventSink) -> Result<Core, String> {
        let OpenOptions {
            env,
            flags,
            session_root,
            metadata,
            mode,
            services,
            auto_name,
        } = options;
        let repo = SessionRepo::new(&session_root, SystemClock::shared());
        let mut session = repo.open(&metadata)?;
        // A worker that died left its run open. A run of the durable runtime resumes there; one journaled without
        // it (an older build, the TypeScript product) is settled before anything reads the lane.
        let open_operation = session
            .value(LANE_STATE, LANE)
            .and_then(|state| state["currentOperationId"].as_str().map(str::to_string));
        // Only a run resumes in the durable runtime; another operation a dead worker left (a manual compaction, a
        // navigation) is settled as interrupted.
        let durable_records = crate::durable_store::has_records(&session)
            && open_operation.as_ref().is_none_or(|operation| {
                session
                    .value(session_journal::run::OP_META, operation)
                    .is_none_or(|meta| meta["intent"]["kind"].as_str().is_none_or(|kind| kind == "run"))
            });
        let recovered = match &open_operation {
            Some(_) if !durable_records => crate::recovery::settle(&mut session)?,
            _ => crate::recovery::Recovered {
                queued: crate::recovery::inbox(&session),
                ..Default::default()
            },
        };
        let resume = open_operation.filter(|_| durable_records);
        // Extensions load before the model is chosen: the providers they declare can be the session's model
        // (agent-session-services.ts registers them before `findInitialModel`).
        let sessions_dir = session_root.join(lane::session_directory_name(&metadata.cwd));
        let mut runtime = Runtime::new(&env, &mode, sessions_dir, services)?;
        let (config, model) = lane_config(&flags, &env, Some(&mut session))?;
        let api_key = lane::api_key(&flags, &env, &model.provider);
        let announced = session.state().entries().len();
        let announced_usage = session.state().usage_rows().len();
        let session_id = session.id().to_string();
        let session: SharedSession = Arc::new(Mutex::new(session));
        // The host modules' documents are values of the session (crate::session_modules).
        runtime.services.values = Some(crate::session_modules::session_values(&session));
        // Subagents run as child conversations of the durable runtime; their task journal is in the session.
        let children = crate::subagents::DurableChildren::new();
        let unfinished = crate::subagents::unfinished(&session);
        let lane = Lane::open_with(
            &runtime,
            &model,
            api_key,
            &session_id,
            Some(children.host(session.clone())),
        )?;
        // The runtime counters are the session value `ultron.module/stats` (counting continues from a resumed one).
        lane.stack
            .stats
            .attach(Arc::new(crate::session_stats::SessionStore(session.clone())));
        let branches = Arc::new(context_lane::RootBranches::new(session.clone()));
        // One module answers `ctx.*` from every lane's kernel; each lane edits its own branch.
        let child_context = Arc::new(context_children::ChildContext::new(children.clone(), session.clone()));
        let context = Arc::new(kernel_apis::ContextControl::new(Arc::new(
            context_children::LaneBranches {
                root: branches.clone(),
                children: child_context.clone(),
            },
        )));
        child_context.observe(&context, runtime.extensions.clone());
        let _ = lane.stack.context.set(context.clone());
        // Requests read the settings as they are now (crate::request_settings): the stream options, transport and
        // idle timeout of `/settings` apply to the next request.
        let provider = Arc::new(Provider::new().with_live_settings(runtime.config.settings_reader(), true));
        let queues = Arc::new(Mutex::new(Queues {
            items: recovered
                .queued
                .iter()
                .filter_map(|q| {
                    Some(Queued {
                        kind: QueueKind::parse(&q.kind)?,
                        message: q.message.clone(),
                        entry_id: q.entry_id.clone(),
                        turn: 0,
                        submission: None,
                    })
                })
                .collect(),
            delivered: Vec::new(),
            steering_all: true,
            follow_up_all: true,
        }));
        let durable = durable_turns::open_durable(
            &session,
            &tx,
            &lane,
            provider.clone(),
            &env,
            &flags,
            queues.clone(),
            children.clone(),
            child_context,
        )?;
        children.attach(durable.chat.clone());
        // The agent hub steers a child lane's running conversation through its inbox (crate::agent_hub).
        let steer_children = children.clone();
        let _ = lane
            .stack
            .lane_steer
            .set(Arc::new(move |child_lane: &str, message: &str| {
                let (Some(chat), Some(conversation)) =
                    (steer_children.chat(), steer_children.conversation_of_lane(child_lane))
                else {
                    return false;
                };
                if chat.live(conversation).map_or(true, |live| live["run"].is_null()) {
                    return false;
                }
                chat.conversation(conversation).ok().flatten().is_some_and(|handle| {
                    handle
                        .submit(harness::chat::SubmissionDraft::Input {
                            content: json!(message),
                            when_busy: harness::chat::WhenBusy::Steer,
                            request_id: None,
                        })
                        .is_ok()
                })
            }));
        let _ = lane.stack.budget_note.set(crate::budget_note::budget_note(
            children.clone(),
            lane.stack.agents.clone(),
            lane.stack.usage.clone(),
        ));
        // Each lane's context use for its reminder: the estimate the harness compacts by, over the lane's durable
        // conversation (the root's, or the child's); its summary entry marks a compaction.
        let meter_children = children.clone();
        lane.stack.context_reminders.set_meter(Arc::new(move |lane_name: &str| {
            let chat = meter_children.chat()?;
            let conversation = if lane_name == lane::MAIN {
                durable::ROOT_CONVERSATION_ID
            } else {
                meter_children.conversation_of_lane(lane_name)?
            };
            let view = chat.conversation(conversation).ok().flatten()?.context(None).ok()?;
            Some(crate::context_reminder::Measure {
                tokens: harness::chat::compaction::estimate_context(&view, &[]),
                epoch: view.head.as_ref().map(|head| head.id.0),
            })
        }));
        let _ = lane
            .stack
            .swarm
            .set(Arc::new(crate::swarm_ops::SwarmOps::new(children.clone())));
        // rlm.spawn(fork=True): the child's conversation starts as its caller's.
        lane.stack.delegation.set_fork_point(children.fork_point_hook());
        children.restore(&lane.stack, &unfinished)?;
        let events = event_lanes::install(
            &lane,
            &env,
            tx.clone(),
            durable.chat.clone(),
            children.clone(),
            durable.stop.clone(),
        );
        event_lanes::tick_activity(tx.clone(), durable.stop.clone());
        // Idle kernels are swept every minute (evicted after their idle TTL), not only when a lane is leased.
        let sweep_stop = durable.stop.clone();
        lane.stack
            .lanes
            .sweep_every(delegation::lanes::IDLE_SWEEP_INTERVAL, move |interval| {
                sweep_stop.wait_timeout(interval).is_some()
            });
        // `compaction.enabled` (default on); `set_auto_compaction` changes it for the session.
        let auto_compaction = runtime.config.settings["compaction"]["enabled"] != false;
        let (env_var, async_events) = (env.var.clone(), runtime.config.async_events);
        let mut core = Core {
            thinking: config["thinkingLevel"].as_str().unwrap_or("medium").to_string(),
            models: lane::available_models(&env),
            provider,
            env,
            flags,
            repo,
            runtime,
            model,
            session,
            lane,
            tx,
            sink,
            run: None,
            hiding: false,
            queues,
            auto_compaction,
            auto_retry: true,
            bash: None,
            bug_summary: None,
            announced_usage,
            announced,
            taken: Vec::new(),
            run_ids: session_file::Uuidv7::default(),
            auto_name,
            recovered: Some(recovered),
            durable,
            resumed: None,
            events,
            context,
            branches,
            inputs: None,
            deferred: std::collections::VecDeque::new(),
            compacting: None,
            hook: None,
            navigating: false,
            after_run: Vec::new(),
            nudger: crate::nudge::ToolRoundNudger::new(
                crate::nudge::tool_rounds_nudge_from_env((env_var)("ULTRON_TOOL_ROUNDS_NUDGE").as_deref()),
                async_events,
            ),
            skill_nudger: crate::nudge::SkillExtractionNudger::new(crate::nudge::skill_nudge_from_env(
                (env_var)("ULTRON_SKILL_NUDGE").as_deref(),
            )),
        };
        if let Some(operation) = resume {
            core.resumed = Some(core.reopen_run(&operation)?);
        } else {
            // Context entries a dead worker's run left waiting join the branch now; a resumed run places them at
            // its next boundary.
            core.branches.place(true)?;
        }
        // Whatever else the runtime holds (subagents, background work) continues now.
        core.durable.chat.resume().map_err(|e| e.to_string())?;
        core.open_extensions()?;
        Ok(core)
    }

    pub fn model(&self) -> &Model {
        &self.model
    }

    pub fn thinking_level(&self) -> &str {
        &self.thinking
    }

    pub fn session(&self) -> SharedSession {
        self.session.clone()
    }

    pub fn runtime(&self) -> &Runtime {
        &self.runtime
    }

    /// The lane snapshot transcript state starts from (`LaneSnapshot`): the branch's entries, configuration, usage.
    pub fn snapshot(&self) -> Value {
        let entries = lock_session(&self.session).branch_entries(LANE).unwrap_or_default();
        let mut usage = session_journal::usage::empty_usage();
        for row in lock_session(&self.session).state().usage_rows() {
            usage = session_journal::usage::add_usage(&usage, &row["usage"]);
        }
        json!({
            "lane": LANE,
            "transcript": entries,
            "tipId": lock_session(&self.session).branch_tip(LANE).flatten(),
            "configuration": {"model": {"provider": self.model.provider, "modelId": self.model.id}, "thinkingLevel": self.thinking, "activeToolNames": ["rlm"]},
            "stats": {"messageCount": entries.iter().filter(|e| e["type"] == "message").count(), "usage": usage},
            "operation": null,
            "queues": [],
            "faulted": false,
        })
    }

    /// Serve inputs until a close (or the input ends).
    pub fn run_loop(mut self, inputs: std::sync::mpsc::Receiver<Input>) {
        self.inputs = Some(inputs);
        while let Some(input) = self.next_input() {
            match input {
                Input::Op(op, reply) => self.op(op, reply),
                Input::Agent(event) => self.on_agent_event(*event),
                Input::Title(response) => {
                    if record_naming(&mut lock_session(&self.session), &response).is_ok() {
                        self.name_changed();
                    }
                }
                Input::RunEnd => self.run_end(),
                Input::Turn(request) => self.on_turn(request),
                Input::HarnessIdle => self.harness_idle(),
                Input::BashDone {
                    command,
                    exclude,
                    result,
                    reply,
                } => self.bash_done(command, exclude, result, reply),
                Input::BashOutput { id, delta } => self.emit_bash_output(id, delta),
                Input::ExtensionError(error) => self.emit_extension_error(error),
                Input::Close(done) => {
                    self.close();
                    let _ = done.send(());
                    return;
                }
                Input::HandOver(done) => {
                    self.close_for_successor();
                    let _ = done.send(());
                    return;
                }
                Input::EventLane(call, reply) => self.event_lane(call, reply),
                Input::ActivityTick => self.report_activity(),
                // A compaction's wake-up is read by the compaction itself.
                Input::Woken => {}
                Input::Extension(call) => self.extension_call(call),
            }
        }
        self.close();
    }

    /// What waited for a compaction first, then the actor's input.
    fn next_input(&mut self) -> Option<Input> {
        if let Some(input) = self.deferred.pop_front() {
            return Some(input);
        }
        self.inputs.as_ref()?.recv().ok()
    }

    fn emit(&mut self, value: Value) {
        (self.sink)(value);
        self.refresh_extension_facts();
    }

    /// The session's name now (`session_name`, a Rust-only event the worker serves as `ultron.session-ops` state,
    /// not on the transcript).
    fn name_changed(&mut self) {
        let name = lock_session(&self.session).name();
        self.emit(json!({"type": "session_name", "lane": LANE, "name": name}));
    }

    /// AgentSession `setSessionName`: `session_info_changed` to clients (Pi's RPC event) and, without waiting, to
    /// the extensions.
    fn session_info_changed(&mut self) {
        let name = lock_session(&self.session).name();
        let mut fields = vec![("type", json!("session_info_changed")), ("lane", json!(LANE))];
        if let Some(name) = &name {
            fields.push(("name", json!(name)));
        }
        self.emit(event(fields));
        if let Some(host) = self
            .extension_host()
            .filter(|h| h.router().has_handlers("session_info_changed"))
        {
            let mut event = json!({"type": "session_info_changed"});
            if let Some(name) = name {
                event["name"] = json!(name);
            }
            std::thread::spawn(move || {
                host.router().emit(&event);
            });
        }
    }

    fn api_key(&self) -> Option<String> {
        lane::api_key(&self.flags, &self.env, &self.model.provider)
    }

    fn run_id(&self) -> Value {
        self.run.as_ref().map_or(Value::Null, |r| json!(r.id))
    }

    /// Announce the entries and usage rows committed since the last announcement, as `entry_added` and `usage`
    /// events (a usage event carries the lane's totals after it).
    fn announce(&mut self) {
        let entries: Vec<Value> = lock_session(&self.session).state().entries()[self.announced..].to_vec();
        self.announced += entries.len();
        for entry in entries {
            // A context edit that lands on the branch is published to extensions (`pi.events`).
            if let Some(edit) = self.context.entry_added(LANE, &entry)
                && let Some(host) = &self.runtime.extensions
            {
                host.publish(kernel_apis::context_control::CONTEXT_EDIT_EVENT, &edit);
            }
            self.emit(event(vec![
                ("type", json!("entry_added")),
                ("lane", json!(LANE)),
                ("entry", entry),
            ]));
        }
        let rows: Vec<Value> = lock_session(&self.session)
            .state()
            .usage_rows()
            .into_iter()
            .cloned()
            .collect();
        for index in self.announced_usage..rows.len() {
            let totals = rows[..=index]
                .iter()
                .fold(session_journal::usage::empty_usage(), |total, row| {
                    session_journal::usage::add_usage(&total, &row["usage"])
                });
            self.emit(event(vec![
                ("type", json!("usage")),
                ("lane", json!(LANE)),
                ("row", rows[index].clone()),
                ("totals", totals),
            ]));
        }
        self.announced_usage = rows.len();
    }

    fn tip(&self) -> Option<String> {
        lock_session(&self.session).branch_tip(LANE).flatten()
    }

    /// The lane's stored state (`pi.lane.state`), idle when none is stored.
    fn lane_state(&self) -> Value {
        lock_session(&self.session)
            .value(LANE_STATE, LANE)
            .cloned()
            .unwrap_or_else(|| json!({"currentOperationId": null, "lastOperationId": null, "inbox": []}))
    }

    /// Append a message to `main` outside a run (the lane's `appendMessage`: the entry, the new tip and the lane's
    /// state in one transaction) and announce the entry.
    fn append(&mut self, message: Value) -> Result<(), String> {
        let id = session_journal::lock_clock(&lock_session(&self.session).clock()).next_id();
        let mut entry = Map::new();
        entry.insert("id".into(), id.clone().into());
        entry.insert("parentId".into(), self.tip().map_or(Value::Null, Value::from));
        entry.insert("type".into(), "message".into());
        entry.insert("message".into(), message);
        let lane_state = self.lane_state();
        lock_session(&self.session).commit(&[
            Write::Entry(entry),
            Write::set(BRANCH_TIP, LANE, id.into()),
            Write::set(LANE_STATE, LANE, lane_state),
        ])?;
        self.announce();
        Ok(())
    }

    /// A queued message taken by the loop joins the branch under its pending entry's id.
    fn commit_queued(&mut self, queued: &Queued, message: Value) -> Result<(), String> {
        let mut entry = Map::new();
        entry.insert("id".into(), queued.entry_id.clone().into());
        entry.insert("parentId".into(), self.tip().map_or(Value::Null, Value::from));
        entry.insert("type".into(), "message".into());
        entry.insert("message".into(), message);
        lock_session(&self.session).commit(&[
            Write::Entry(entry),
            Write::delete(PENDING_ENTRY, &queued.entry_id),
            Write::set(BRANCH_TIP, LANE, queued.entry_id.clone().into()),
        ])?;
        Ok(())
    }

    fn queue_update(&mut self) {
        let items: Vec<Value> = lock(&self.queues)
            .visible()
            .into_iter()
            .map(|q| json!({"type": "message", "kind": q.kind.name(), "entryId": q.entry_id, "message": q.message}))
            .collect();
        self.emit(event(vec![
            ("type", json!("queue_update")),
            ("lane", json!(LANE)),
            ("queues", Value::Array(items)),
        ]));
    }

    /// Store the model and thinking level as the lane's configuration (`pi.lane.config`).
    fn store_lane_config(&mut self) -> Result<(), String> {
        let mut config = lock_session(&self.session)
            .value(LANE_CONFIG, LANE)
            .cloned()
            .unwrap_or_else(|| run::lane_config("", "", "", &["rlm"]));
        config["model"] = json!({"provider": self.model.provider, "modelId": self.model.id});
        config["thinkingLevel"] = json!(self.thinking);
        if lock_session(&self.session).value(LANE_CONFIG, LANE) != Some(&config) {
            lock_session(&self.session).commit(&[Write::set(LANE_CONFIG, LANE, config)])?;
        }
        Ok(())
    }

    fn idle(&self) -> Result<(), String> {
        if self.run.is_some() {
            return Err("Agent is already processing".into());
        }
        Ok(())
    }

    fn journal(&mut self, event: &AgentEvent) {
        if let Some(run) = self.run.as_mut() {
            run.journal.event(&mut lock_session(&self.session), event);
        }
    }

    fn on_agent_event(&mut self, agent_event: AgentEvent) {
        let run = self.run_id();
        let lane = json!(LANE);
        match &agent_event {
            AgentEvent::MessageStart { message } => {
                let value = stored(message);
                let aborted = self.run.as_ref().is_some_and(|r| r.signal.aborted());
                if aborted
                    && value["role"] == "assistant"
                    && value["stopReason"] == "aborted"
                    && value["content"].as_array().is_none_or(Vec::is_empty)
                {
                    self.hiding = true;
                    return;
                }
                self.journal(&agent_event);
                self.emit(event(vec![
                    ("type", json!("message_start")),
                    ("runId", run),
                    ("lane", lane),
                    ("message", value),
                ]));
            }
            AgentEvent::MessageUpdate {
                message,
                assistant_message_event,
            } => {
                if self.hiding {
                    return;
                }
                self.journal(&agent_event);
                // The frame (`AssistantMessageFrame`): the provider event without its partial message; a tool
                // call's start names the call.
                let mut frame = serde_json::to_value(assistant_message_event).unwrap_or(Value::Null);
                if frame["type"] == "toolcall_start"
                    && let Some(index) = frame["contentIndex"].as_u64()
                {
                    let call = frame["partial"]["content"][index as usize].clone();
                    frame["id"] = call["id"].clone();
                    frame["toolName"] = call["name"].clone();
                }
                if let Some(map) = frame.as_object_mut() {
                    map.shift_remove("partial");
                }
                self.emit(event(vec![
                    ("type", json!("message_update")),
                    ("runId", run),
                    ("lane", lane),
                    ("message", serde_json::to_value(message).unwrap_or(Value::Null)),
                    ("frame", frame),
                ]));
            }
            AgentEvent::MessageEnd { message } => {
                if std::mem::take(&mut self.hiding) {
                    return;
                }
                let value = stored(message);
                if value["role"] == "assistant"
                    && let Some(running) = self.run.as_mut()
                {
                    running.error = value["errorMessage"].as_str().map(str::to_string);
                }
                self.emit(event(vec![
                    ("type", json!("message_end")),
                    ("runId", run),
                    ("lane", lane),
                    ("message", value.clone()),
                ]));
                let delivered = if value["role"] == "user" {
                    let mut queues = lock(&self.queues);
                    let found = queues
                        .delivered
                        .iter()
                        .position(|q| message_text(&q.message) == message_text(&value));
                    found.map(|i| queues.delivered.remove(i))
                } else {
                    None
                };
                let was_queued = delivered.is_some();
                let answered = value["role"] == "assistant";
                let round_message = (value["role"] != "user").then(|| value.clone());
                let mut retry = None;
                if value["role"] == "user" {
                    let first = self
                        .run
                        .as_mut()
                        .is_some_and(|run| std::mem::take(&mut run.prompt_pending));
                    // The run's prompt was committed when the run was admitted; a steering or follow-up message
                    // joins the branch when the loop takes it.
                    let committed = match &delivered {
                        // Committed when the generation that takes it began.
                        Some(queued) if self.taken.contains(&queued.entry_id) => {
                            self.taken.retain(|id| *id != queued.entry_id);
                            Ok(())
                        }
                        Some(queued) => self.commit_queued(queued, value),
                        None if !first => lock_session(&self.session).append_message(LANE, value).map(drop),
                        None => Ok(()),
                    };
                    if let Err(error) = committed {
                        self.fail_run(error);
                    }
                } else {
                    if let Message::Assistant(answer) = message {
                        retry = self.retry_decision(answer);
                    }
                    self.journal(&agent_event);
                }
                self.announce();
                if was_queued {
                    self.queue_update();
                }
                let retried = matches!(retry, Some(RetryNote::Scheduled(_)));
                if let Some(note) = retry {
                    self.emit_retry(note);
                }
                // A response that is retried ends no turn (no `turn_end` at `assistant.retry_wait`).
                if let Some(message) = round_message.filter(|_| !retried) {
                    self.count_round(&message);
                }
                // Once the model answered after a cell's output that returned finished tasks, it collapses.
                if answered {
                    let _ = self.context.message_end(LANE, "assistant");
                }
            }
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } => {
                self.journal(&agent_event);
                self.emit(event(vec![
                    ("type", json!("tool_start")),
                    ("runId", run),
                    ("lane", lane),
                    ("toolCallId", json!(tool_call_id)),
                    ("toolName", json!(tool_name)),
                    ("args", args.clone()),
                ]));
            }
            AgentEvent::ToolExecutionUpdate {
                tool_call_id,
                tool_name,
                args,
                partial_result,
            } => self.emit(event(vec![
                ("type", json!("tool_update")),
                ("runId", run),
                ("lane", lane),
                ("toolCallId", json!(tool_call_id)),
                ("toolName", json!(tool_name)),
                ("args", args.clone()),
                (
                    "partialResult",
                    serde_json::to_value(partial_result).unwrap_or(Value::Null),
                ),
            ])),
            AgentEvent::ToolExecutionEnd {
                tool_call_id,
                tool_name,
                result,
                is_error,
            } => {
                let mut result = serde_json::to_value(result).unwrap_or(Value::Null);
                if *is_error && result["details"] == json!({}) {
                    result.as_object_mut().map(|m| m.remove("details"));
                }
                self.emit(event(vec![
                    ("type", json!("tool_end")),
                    ("runId", run),
                    ("lane", lane),
                    ("toolCallId", json!(tool_call_id)),
                    ("toolName", json!(tool_name)),
                    ("result", result),
                    ("isError", json!(is_error)),
                ]));
            }
            // The lane's own boundaries: turns and the run's end are reconstructed by clients.
            AgentEvent::TurnStart => self.begin_generation(),
            AgentEvent::AgentStart | AgentEvent::AgentEnd { .. } | AgentEvent::TurnEnd { .. } => {}
        }
    }

    /// A generation begins. The queued messages the loop took for it (drained before its `turn_start`) join the
    /// branch in the same transaction as `assistant.ready`, and the lane's inbox keeps the rest. A cancelled run
    /// starts no generation.
    ///
    /// Only messages taken for this turn or an earlier one: the actor processes `turn_start` after the loop emitted
    /// it, and by then a fast loop may already have drained the next turn's follow-up. Taking every delivered message
    /// committed that follow-up before this turn's assistant message (seen under load as `steer`, `follow-up`,
    /// `assistant`, `assistant` instead of `steer`, `assistant`, `follow-up`, `assistant`).
    fn begin_generation(&mut self) {
        let Some(run) = self.run.as_mut() else { return };
        run.turns += 1;
        let turn = run.turns;
        if run.signal.aborted() {
            return;
        }
        let taken: Vec<Queued> = lock(&self.queues)
            .delivered
            .iter()
            .filter(|q| q.turn <= turn && !self.taken.contains(&q.entry_id))
            .cloned()
            .collect();
        // Context entries appended since the last boundary join the branch first, in the same transaction.
        let tip = self.tip();
        let (mut writes, mut parent) = context_lane::placement(&lock_session(&self.session), tip.clone());
        for queued in &taken {
            let mut entry = Map::new();
            entry.insert("id".into(), queued.entry_id.clone().into());
            entry.insert("parentId".into(), parent.clone().map_or(Value::Null, Value::from));
            entry.insert("type".into(), "message".into());
            entry.insert("message".into(), queued.message.clone());
            writes.push(Write::Entry(entry));
            writes.push(Write::delete(PENDING_ENTRY, &queued.entry_id));
            parent = Some(queued.entry_id.clone());
        }
        let trigger = taken.last().map(|q| q.entry_id.clone());
        // The retry wait ended (`runRetryWait`): the step's next attempt starts.
        let retry_start = self
            .run
            .as_ref()
            .and_then(|run| run.journal.run())
            .and_then(|journal| Some((journal.retry_waiting()?, journal.step_id().to_string())));
        if let Some((attempt, step)) = retry_start {
            let run = self.run_id();
            self.emit(json!({"type": "retry_start", "lane": LANE, "runId": run, "step": step, "attempt": attempt}));
        }
        if let Some(placed) = parent.filter(|placed| Some(placed) != tip.as_ref()) {
            writes.push(Write::set(BRANCH_TIP, LANE, placed.into()));
        }
        let mut inbox = self.lane_state()["inbox"].clone();
        if let Some(items) = inbox.as_array_mut() {
            items.retain(|item| !taken.iter().any(|q| item["entryId"] == q.entry_id.as_str()));
        }
        self.taken.extend(taken.iter().map(|q| q.entry_id.clone()));
        if let Some(run) = self.run.as_mut() {
            run.journal.set_inbox(inbox);
            run.journal
                .begin_with(&mut lock_session(&self.session), writes, trigger);
        }
    }

    /// A journal write failed: the run stops (its error is reported when it ends).
    fn fail_run(&mut self, error: String) {
        if let Some(run) = self.run.as_mut() {
            run.signal.abort();
            run.error.get_or_insert(error);
        }
    }

    fn run_end(&mut self) {
        let operation = self.operation_id();
        if let Some(operation) = &operation {
            self.lane.stack.agents.end_root_turn(operation);
        }
        let Some(mut run) = self.run.take() else { return };
        if !run.cancelled && run.error.is_none() {
            run.error = self.unanswered(run.submission);
        }
        // The run's last boundary: collapse what the model answered after, then place what is still waiting.
        let _ = self.context.run_end(LANE);
        let waiting = context_lane::pending_count(&lock_session(&self.session));
        if let Err(error) = self.branches.place(true) {
            run.error.get_or_insert(error);
        }
        self.announce();
        // Entries that waited for the boundary left the lane's queue.
        if waiting > 0 {
            self.queue_update();
        }
        self.durable.bridge.set_user_abort(false);
        let aborted = run.cancelled;
        let finished = if aborted {
            run.journal.finish_aborted(&mut lock_session(&self.session))
        } else {
            let code = if run.compaction_failed {
                "summarization_failed"
            } else {
                "assistant_error"
            };
            let failure = run.error.as_deref().map(|m| (code, m));
            run.journal.finish(Some(&mut lock_session(&self.session)), failure)
        };
        let mut error = if aborted { None } else { run.error.clone() };
        if let Err(write) = finished {
            error.get_or_insert(write);
        }
        // An error answer commits with the run's terminal transaction.
        self.announce();
        self.taken.clear();
        lock(&self.queues).delivered.clear();
        let status = if aborted {
            "aborted"
        } else if error.is_some() {
            "failed"
        } else {
            "completed"
        };
        // Detached work the run leaves running keeps the worker alive: reported before the run's end is.
        self.report_activity();
        let mut fields = vec![
            ("type", json!("run_end")),
            ("lane", json!(LANE)),
            ("runId", json!(run.id)),
            ("status", json!(status)),
        ];
        if let Some(message) = &error {
            fields.push(("error", json!({"code": "run_failed", "message": message})));
        }
        fields.extend([
            ("fromTipId", run.started_tip.clone()),
            ("tipId", self.tip().map_or(Value::Null, Value::from)),
            ("endedAt", json!(now_ms())),
        ]);
        self.emit(event(fields));
        // The dispatcher's `run_end`: an aborted root never restarts work; unconsumed steers are taken back.
        if let Some(operation) = &operation {
            self.events.run_ended(LANE, operation, aborted);
        }
        self.nudger.run_ended(&run.id);
        self.skill_nudger.run_ended(&run.id);
        // What the run counted is in the session file when its end is reported.
        self.lane.stack.stats.flush();
        // Failure streaks of the hints' loop detector start over with the next run.
        self.lane.stack.cell_hints().run_ended(LANE);
        if let Some(waiter) = run.waiter {
            let error = error.map_or(Value::Null, |m| json!({"code": "run_failed", "message": m}));
            waiter(Ok(Some(
                json!({"accepted": true, "operationId": run.id, "error": error}),
            )));
        }
        // What waited for the run's end runs next, before any other input.
        for input in std::mem::take(&mut self.after_run).into_iter().rev() {
            self.deferred.push_front(input);
        }
        self.emit_agent_end();
    }

    fn bash_done(&mut self, command: String, exclude: bool, result: Result<BashOutcome, String>, reply: Reply) {
        self.bash = None;
        let result = match result {
            Ok(BashOutcome::Ran(result)) => result,
            // An answer, not a failure of the call: the client tells it from a command that could not run.
            Ok(BashOutcome::FailedClosed(error)) => return reply(Ok(Some(json!({ USER_BASH_FAILED: error })))),
            Err(e) => return reply(Err(e)),
        };
        let message = user_bash::bash_message(&command, &result, exclude, now_ms());
        self.emit(event(vec![
            ("type", json!("message_start")),
            ("lane", json!(LANE)),
            ("message", message.clone()),
        ]));
        self.emit(event(vec![
            ("type", json!("message_end")),
            ("lane", json!(LANE)),
            ("message", message.clone()),
        ]));
        if let Err(error) = self.append(message) {
            return reply(Err(error));
        }
        reply(Ok(Some(result)));
    }

    /// AgentSession's `bash_execution_update`: a chunk of the running command's output, under its id.
    fn emit_bash_output(&mut self, id: Option<String>, delta: String) {
        let mut fields = vec![("type", json!("bash_execution_update")), ("lane", json!(LANE))];
        if let Some(id) = id {
            fields.push(("id", json!(id)));
        }
        fields.push(("delta", json!(delta)));
        self.emit(event(fields));
    }

    /// An extension handler failed (rpc-mode.ts `onError`, interactive-mode.ts `showExtensionError`).
    fn emit_extension_error(&mut self, error: Value) {
        self.emit(event(vec![
            ("type", json!("extension_error")),
            ("lane", json!(LANE)),
            ("extensionPath", error["extensionPath"].clone()),
            ("event", error["event"].clone()),
            ("error", error["error"].clone()),
        ]));
    }

    /// Stop the lane (kernels, subagents, extensions).
    pub fn close(&mut self) {
        self.stop(false);
    }

    /// As [`Core::close`] when a successor worker takes the session over: shell jobs keep running for it.
    pub fn close_for_successor(&mut self) {
        self.stop(true);
    }

    fn stop(&mut self, hand_over: bool) {
        if let Some(run) = &self.run {
            run.signal.abort();
        }
        // A successor takes the session: what this worker's hosts would still journal (a subagent's wait failing
        // as the runtime closes) is dropped, so its tasks stay unfinished for the successor to reattach.
        if hand_over {
            self.durable.children.hand_over();
            if let Some(values) = &self.runtime.services.values {
                values.hand_over();
            }
        }
        // The durable runtime stops without settling anything: a run in progress resumes when the session opens.
        self.durable.stop.abort();
        // `cleanupSessionResources(sessionId)` of AgentSession.dispose: the provider connections cached under this
        // session's provider session IDs (the Codex WebSockets) are closed with it.
        for id in self.durable.chat.provider_session_ids().unwrap_or_default() {
            ai::openai_codex_responses::close_openai_codex_websocket_sessions(Some(&id));
        }
        let _ = self.durable.chat.close();
        if let Some(signal) = &self.bash {
            signal.abort("closing");
        }
        if hand_over {
            self.lane.hand_over();
        } else {
            self.lane.shutdown();
        }
        self.emit_session_shutdown();
        if let Some(host) = &self.runtime.extensions {
            host.shutdown();
        }
        self.events.close();
    }

    /// The Pi view of the session the read operations use.
    fn pi(&self) -> SessionManager {
        pi_view::pi_manager(&lock_session(&self.session))
    }

    fn session_file(&self) -> String {
        lock_session(&self.session).metadata().path.display().to_string()
    }

    fn op(&mut self, op: Op, reply: Reply) {
        match op {
            Op::Prompt { message, images, queue } => {
                let result = if self.run.is_some() {
                    match queue {
                        Some(kind) => self.enqueue(kind, message, images, None).map(drop),
                        None => Err("Agent is already processing. Specify streamingBehavior ('steer' or 'followUp') to queue the message.".into()),
                    }
                } else {
                    self.prompt(message, images, None).map_err(|(error, _)| error)
                };
                reply(result.map(|()| None));
            }
            Op::PromptAndWait { message, images } => {
                if self.run.is_some() {
                    return reply(Err("Agent is already processing".into()));
                }
                // Answered when the run ends.
                if let Err((error, reply)) = self.prompt(message, images, Some(reply)) {
                    reply(Err(error));
                }
            }
            bash @ Op::Bash { .. } => self.bash(bash, reply),
            Op::AppendCustom { custom_type, data } => {
                self.extension_call(crate::extension_host::Call::AppendCustom { custom_type, data });
                reply(Ok(None));
            }
            Op::BugSummary { hint } => {
                let summarizer = self.summarizer();
                let messages = view::messages(&self.pi());
                let retry = self.summary_retry();
                let signal = AbortSignal::new();
                self.bug_summary = Some(signal.clone());
                std::thread::spawn(move || {
                    reply(
                        summarizer
                            .bug_report(&messages, hint.as_deref(), retry, &signal)
                            .map(|text| Some(json!(text))),
                    )
                });
            }
            // AgentSession `compact`: `await this.abort()` first, then compact once the session is idle.
            compact @ Op::Compact { .. } if self.run.is_some() => {
                self.abort();
                self.after_run.push(Input::Op(compact, reply));
            }
            Op::ExtensionCommand { name, args } => {
                // A command may prompt and has no timeout: it runs off the actor.
                let stack = self.lane.stack.clone();
                std::thread::spawn(move || reply(stack.run_extension_command(&name, &args).map(Some)));
            }
            Op::ExtensionCompletions { name, prefix } => {
                let stack = self.lane.stack.clone();
                std::thread::spawn(move || reply(stack.complete_extension_command(&name, &prefix).map(Some)));
            }
            Op::ExtensionShortcut { key, keybindings } => {
                // A handler may prompt: it runs off the actor, as TypeScript runs it without blocking input.
                let host = self.runtime.extensions.clone();
                std::thread::spawn(move || {
                    let run = host
                        .ok_or_else(|| format!("No extension shortcut {key}"))
                        .and_then(|host| host.run_shortcut(&key, &keybindings).map_err(|e| e.to_string()));
                    reply(run.map(|_| None));
                });
            }
            Op::BeforeFork { entry_id, position } => {
                let host = self.runtime.extensions.clone();
                std::thread::spawn(move || reply(Ok(Some(before_fork(host.as_ref(), &entry_id, &position)))));
            }
            Op::Recover => {
                let result = self.recover();
                reply(result);
            }
            other => {
                let result = self.sync_op(other);
                reply(result);
            }
        }
    }

    fn sync_op(&mut self, op: Op) -> OpResult {
        // `/usage` reads the session file it describes: the counters still in memory are saved first.
        if matches!(op, Op::Describe) {
            self.lane.stack.stats.flush();
        }
        match op {
            Op::Prompt { .. }
            | Op::PromptAndWait { .. }
            | Op::Bash { .. }
            | Op::BugSummary { .. }
            | Op::AppendCustom { .. }
            | Op::Recover
            | Op::ExtensionCommand { .. }
            | Op::ExtensionCompletions { .. }
            | Op::ExtensionShortcut { .. }
            | Op::BeforeFork { .. } => {
                unreachable!("answered in op")
            }
            Op::ExtensionShortcuts { keybindings } => Ok(Some(extension_issues(
                &self.runtime.extension_errors,
                self.runtime.extensions.as_ref(),
                &keybindings,
            ))),
            Op::Enqueue {
                kind,
                message,
                images,
                entry_id,
            } => self.enqueue(kind, message, images, entry_id).map(|id| Some(json!(id))),
            Op::CancelQueued { entry_id } => {
                let found = lock(&self.queues).items.iter().any(|q| q.entry_id == entry_id);
                if found {
                    self.remove_queued(&[entry_id])?;
                }
                Ok(Some(json!(found)))
            }
            Op::ClearQueue => Ok(Some(self.clear_queue())),
            Op::Abort => {
                self.abort();
                Ok(None)
            }
            Op::State => Ok(Some(self.state())),
            Op::AvailableModels => {
                // The registry is read now (credentials and extension providers change while the session runs).
                self.models = lane::available_models(&self.env);
                Ok(Some(Value::Array(
                    self.models
                        .iter()
                        .filter_map(|m| serde_json::to_value(m).ok())
                        .collect(),
                )))
            }
            Op::SetModel { provider, model_id } => {
                let find = |models: &[Model]| {
                    models
                        .iter()
                        .find(|m| m.provider == provider && m.id == model_id)
                        .cloned()
                };
                let model = match find(&self.models) {
                    Some(model) => model,
                    None => {
                        self.models = lane::available_models(&self.env);
                        find(&self.models).ok_or_else(|| format!("Model not found: {provider}/{model_id}"))?
                    }
                };
                self.model = model;
                self.lane.stack.images.set_model(None, self.model.clone());
                self.store_lane_config()?;
                serde_json::to_value(&self.model).map(Some).map_err(|e| e.to_string())
            }
            Op::CycleModel => {
                if self.models.len() < 2 {
                    return Ok(Some(Value::Null));
                }
                let index = self
                    .models
                    .iter()
                    .position(|m| m.provider == self.model.provider && m.id == self.model.id);
                self.model = self.models[index.map_or(0, |i| (i + 1) % self.models.len())].clone();
                self.lane.stack.images.set_model(None, self.model.clone());
                self.store_lane_config()?;
                Ok(Some(json!({
                    "model": serde_json::to_value(&self.model).map_err(|e| e.to_string())?,
                    "thinkingLevel": self.thinking,
                    "isScoped": false,
                })))
            }
            Op::ThinkingLevels => Ok(Some(json!(self.thinking_levels()))),
            Op::SetThinking { level } => self.set_thinking(&level).map(|()| None),
            Op::CycleThinking => {
                let levels = self.thinking_levels();
                if levels.len() < 2 {
                    return Ok(Some(Value::Null));
                }
                let index = levels.iter().position(|l| *l == self.thinking);
                let next = levels[index.map_or(0, |i| (i + 1) % levels.len())].clone();
                self.set_thinking(&next)?;
                Ok(Some(json!(next)))
            }
            Op::SetQueueMode { kind, mode } => {
                let mut queues = lock(&self.queues);
                let all = mode == "all";
                match kind {
                    QueueKind::Steer => queues.steering_all = all,
                    QueueKind::FollowUp => queues.follow_up_all = all,
                    QueueKind::NextRun => return Err("kind must be steer or followUp".into()),
                }
                Ok(None)
            }
            Op::Compact { instructions } => self.compact(instructions.as_deref()).map(Some),
            Op::SetAutoCompaction(enabled) => {
                self.auto_compaction = enabled;
                Ok(None)
            }
            Op::SetAutoRetry(enabled) => {
                self.auto_retry = enabled;
                self.durable
                    .auto_retry
                    .store(enabled, std::sync::atomic::Ordering::SeqCst);
                Ok(None)
            }
            // `abort_retry`: a run waiting to retry its generation is aborted (`requestAbort` of the operation).
            Op::AbortRetry => {
                if self
                    .run
                    .as_ref()
                    .and_then(|run| run.journal.run())
                    .is_some_and(|journal| journal.retry_waiting().is_some())
                {
                    self.abort();
                }
                Ok(None)
            }
            Op::AbortBash => {
                if let Some(signal) = &self.bash {
                    signal.abort("abort_bash");
                }
                Ok(None)
            }
            Op::AbortBugSummary => {
                if let Some(signal) = self.bug_summary.take() {
                    signal.abort();
                }
                Ok(None)
            }
            Op::ShareContext => {
                let tool = agent::AgentTool::tool(&*self.lane.tool);
                Ok(Some(json!({
                    "systemPrompt": self.runtime.system.current(),
                    "tools": [{"name": tool.name, "description": tool.description, "parameters": tool.parameters}],
                })))
            }
            Op::Stats => Ok(Some(view::stats(
                &self.pi(),
                Some(&self.session_file()),
                lock_session(&self.session).id(),
                self.model.context_window,
            ))),
            Op::ForkMessages => Ok(Some(Value::Array(view::fork_messages(&self.pi())))),
            Op::Entries { since } => {
                let pi = self.pi();
                let mut entries = view::entries(&pi);
                if let Some(since) = since {
                    let index = entries
                        .iter()
                        .position(|e| e["id"] == since.as_str())
                        .ok_or_else(|| format!("Entry not found: {since}"))?;
                    entries.drain(..=index);
                }
                Ok(Some(
                    json!({"entries": entries, "leafId": pi.leaf_id().cloned().unwrap_or(Value::Null)}),
                ))
            }
            Op::Tree => Ok(Some(self.labeled_tree())),
            Op::LastAssistantText => Ok(Some(json!(view::last_assistant_text(&self.pi())))),
            Op::SetName { name } => {
                names::set_user_name(&mut lock_session(&self.session), Some(&name))?;
                self.name_changed();
                self.session_info_changed();
                Ok(None)
            }
            Op::Messages => Ok(Some(Value::Array(view::messages(&self.pi())))),
            Op::ForkSource { entry_id } => {
                self.idle()?;
                let pi = self.pi();
                let entry = pi
                    .entry(&entry_id)
                    .filter(|e| e["type"] == "message" && e["message"]["role"] == "user")
                    .ok_or_else(|| format!("Entry is not a user message: {entry_id}"))?;
                Ok(Some(json!(message_text(&entry["message"]))))
            }
            Op::CloneSource => {
                self.idle()?;
                let leaf = self.pi().leaf_id().cloned().filter(|l| !l.is_null());
                leaf.map(Some)
                    .ok_or_else(|| "Cannot clone session: no current entry selected".to_string())
            }
            Op::Forked { target } => {
                let metadata = session_journal::repo::read_metadata(Path::new(&target))
                    .ok_or_else(|| format!("Not a session file: {target}"))?;
                let mut session = self.repo.open(&metadata)?;
                // The fork's first worker reads (and removes) the marker: its `session_start` says "fork".
                session.commit(&[Write::set(
                    extension_ops::SESSION_START.0,
                    extension_ops::SESSION_START.1,
                    json!({"previousSessionFile": self.session_file()}),
                )])?;
                self.extensions_forked(&target);
                Ok(None)
            }
            Op::Describe => Ok(Some(json!({
                "sessionId": lock_session(&self.session).id(),
                "sessionFile": self.session_file(),
                "name": lock_session(&self.session).name(),
                "messages": view::messages(&self.pi()),
                // The lane's own entries (compactions and branch summaries as entries), as the TUI draws them.
                "entries": lock_session(&self.session).branch_entries(LANE).unwrap_or_default(),
                "model": serde_json::to_value(&self.model).unwrap_or(Value::Null),
                "thinkingLevel": self.thinking,
            }))),
            Op::Navigate {
                target_id,
                summarize,
                instructions,
                label,
            } => self
                .navigate(target_id, summarize, instructions.as_deref(), label)
                .map(Some),
            Op::SetLabel { entry_id, label } => self.set_label(&entry_id, label.as_deref()).map(|()| None),
            Op::Agents => self
                .lane
                .stack
                .delegation
                .handle("rlm.list_subagents", json!({}), LANE)
                .map(Some),
            Op::KillAgent { id } => self
                .lane
                .stack
                .delegation
                .cancel(&id, "Killed from the agent hub")
                .map(Some),
            Op::SteerAgent { id, message } => self.steer_agent(&id, &message).map(Some),
            Op::ReviveAgent { id } => self.revive_agent(&id).map(Some),
            Op::AgentTranscript { id } => self.agent_transcript(&id).map(Some),
            Op::StartConversation { message } => self.start_conversation(&message).map(Some),
            Op::ConversationInput { id, message } => self.conversation_input(id, &message).map(|()| None),
            Op::Conversations { id } => self.conversations(id).map(Some),
            Op::AsyncPending => Ok(Some(self.async_pending())),
            Op::Commands => Ok(Some(Value::Array(self.lane.stack.commands()))),
            // Root-owned work whose completion may still re-invoke the root, asked through the inspector.
            Op::Inspect { request, .. } if request == "async.pending" => Ok(Some(self.async_pending())),
            // The worker answers the pool itself (session-worker.ts `inspect`): kernels and guard statistics.
            Op::Inspect { request, .. } if request == "rlm.pool" => Ok(Some(self.lane.stack.pool_stats())),
            Op::Inspect { request, payload } => {
                let runtime = self.lane.stack.cell_handler(LANE, None);
                crate::inspect::inspect(Some(&*runtime), &request, payload, &kernel::Signal::new()).map(Some)
            }
            Op::Goal { args } => self
                .lane
                .stack
                .goal
                .command(&args, &kernel::Signal::new())
                .map(|text| Some(json!(text))),
            Op::ListSessions => {
                let listed =
                    names::list_sessions(Path::new(self.repo.root()), &lock_session(&self.session).metadata().cwd);
                Ok(Some(Value::Array(
                    listed
                        .into_iter()
                        .map(|s| {
                            json!({"path": s.metadata.path.display().to_string(), "id": s.metadata.id, "name": s.name,
                                "modified": s.metadata.modified_at as i64, "messageCount": s.message_count,
                                "firstMessage": s.first_message, "allMessagesText": s.all_messages_text,
                                "cwd": s.metadata.cwd})
                        })
                        .collect(),
                )))
            }
        }
    }

    /// What a response means for the generation's retries (the harness's `publishResponse`): a retryable error
    /// within the policy's attempts is retried after `retryDelayMs` (journaled as `assistant.retry_wait`); a later
    /// attempt's response ends the retries.
    fn retry_decision(&mut self, answer: &ai::AssistantMessage) -> Option<RetryNote> {
        let policy = self.summary_retry();
        let run = self.run.as_mut()?;
        if run.signal.aborted() {
            return None;
        }
        let journal = run.journal.run()?;
        let (attempt, step) = (journal.attempt(), journal.step_id().to_string());
        let error = answer.stop_reason == ai::StopReason::Error;
        if error
            && attempt < policy.max_attempts
            && harness::chat::models::is_retryable(answer)
            && !harness::chat::models::is_context_overflow(answer)
        {
            let delay = policy.delay_ms(attempt);
            let not_before = now_ms().saturating_add(i64::try_from(delay).unwrap_or(i64::MAX));
            let message = answer
                .error_message
                .clone()
                .unwrap_or_else(|| "Assistant request failed".into());
            run.journal.schedule_retry(not_before, message.clone());
            return Some(RetryNote::Scheduled(json!({
                "step": step, "attempt": attempt + 1, "maxAttempts": policy.max_attempts, "delayMs": delay,
                "notBefore": not_before, "errorMessage": message,
            })));
        }
        if attempt <= 1 {
            return None;
        }
        let success = !matches!(answer.stop_reason, ai::StopReason::Error | ai::StopReason::Aborted);
        let mut end = json!({"step": step, "attempt": attempt, "success": success});
        if !success {
            end["finalError"] = json!(answer.error_message.clone().unwrap_or_else(|| {
                let reason = serde_json::to_value(answer.stop_reason).unwrap_or(Value::Null);
                format!("Assistant request ended with {}", reason.as_str().unwrap_or_default())
            }));
        }
        Some(RetryNote::Ended(end))
    }

    /// `retry_scheduled` / `retry_end` of the run's generation.
    fn emit_retry(&mut self, note: RetryNote) {
        let (kind, fields) = match note {
            RetryNote::Scheduled(fields) => ("retry_scheduled", fields),
            RetryNote::Ended(fields) => ("retry_end", fields),
        };
        let mut event = json!({"type": kind, "lane": LANE, "runId": self.run_id()});
        for (key, value) in fields.as_object().into_iter().flatten() {
            event[key] = value.clone();
        }
        self.emit(event);
    }

    /// A model turn of the run that answers without tools ends with its answer (`turn_end`); a tool round's turn
    /// ends with its last cell ([`crate::turns::Request::RoundEnded`]).
    fn count_round(&mut self, message: &Value) {
        if self.run.is_none() || message["role"] != "assistant" {
            return;
        }
        let calls = message["content"]
            .as_array()
            .map_or(0, |c| c.iter().filter(|b| b["type"] == "toolCall").count());
        if calls == 0 {
            self.turn_ended(0, 0);
        }
    }

    /// `turn_end` on `main`: the brake and the skill hint steer the run when due.
    pub(super) fn turn_ended(&mut self, calls: usize, failed: usize) {
        let Some(run) = self.run.as_ref().map(|r| r.id.clone()) else {
            return;
        };
        let running = self.root_started_running();
        let steers: Vec<String> = [
            self.nudger.turn_ended(&run, calls, running),
            self.skill_nudger.turn_ended(&run, calls, failed, running),
        ]
        .into_iter()
        .flatten()
        .collect();
        for steer in steers {
            self.lane.stack.stats.nudge(&steer);
            let _ = self.enqueue(QueueKind::Steer, steer, Vec::new(), None);
        }
    }

    /// `pendingRootNotifications`: subagents and root-level tasks the root started that have not ended.
    fn root_started_running(&self) -> u64 {
        use kernel_apis::TaskHost;
        let subagents = self.lane.stack.delegation.running(LANE).len();
        let tasks = self
            .lane
            .stack
            .agents
            .tasks()
            .unwrap_or_default()
            .iter()
            .filter(|task| task["parentId"].is_null() && task["result"].is_null())
            .count();
        (subagents + tasks) as u64
    }

    fn thinking_levels(&self) -> Vec<String> {
        ai::models::supported_thinking_levels(&self.model)
            .into_iter()
            .map(str::to_string)
            .collect()
    }

    fn set_thinking(&mut self, level: &str) -> Result<(), String> {
        let levels = self.thinking_levels();
        if !levels.iter().any(|l| l == level) {
            return Err(format!(
                "Thinking level {level} is unavailable; choose one of: {}",
                levels.join(", ")
            ));
        }
        if self.thinking != level {
            self.thinking = level.to_string();
            self.store_lane_config()?;
            self.emit(event(vec![
                ("type", json!("config_update")),
                ("lane", json!(LANE)),
                ("property", json!("thinkingLevel")),
                ("value", json!(level)),
            ]));
        }
        Ok(())
    }

    fn state(&self) -> Value {
        let queues = lock(&self.queues);
        let mut state = Map::new();
        state.insert("model".into(), serde_json::to_value(&self.model).unwrap_or(Value::Null));
        state.insert("thinkingLevel".into(), self.thinking.clone().into());
        state.insert("isStreaming".into(), self.run.is_some().into());
        state.insert("isCompacting".into(), self.compacting.is_some().into());
        let mode = |all: bool| if all { "all" } else { "one-at-a-time" };
        state.insert("steeringMode".into(), mode(queues.steering_all).into());
        state.insert("followUpMode".into(), mode(queues.follow_up_all).into());
        state.insert("sessionFile".into(), self.session_file().into());
        state.insert("sessionId".into(), lock_session(&self.session).id().into());
        if let Some(name) = lock_session(&self.session).name() {
            state.insert("sessionName".into(), name.into());
        }
        state.insert("autoCompactionEnabled".into(), self.auto_compaction.into());
        state.insert("messageCount".into(), view::messages(&self.pi()).len().into());
        state.insert(
            "pendingMessageCount".into(),
            (queues.items.len() + queues.delivered.len()).into(),
        );
        Value::Object(state)
    }

    /// Start a run of `message`; `waiter` is answered when it ends. On failure the waiter comes back with the error.
    #[allow(clippy::result_large_err)]
    fn prompt(&mut self, message: String, images: Vec<Value>, waiter: Option<Reply>) -> Result<(), (String, Reply)> {
        self.prompt_with(user_message(message, images), waiter, true)
    }

    /// Start a run of the user message `prompt` (see [`Core::prompt`]).
    #[allow(clippy::result_large_err)]
    ///
    /// `take_queued`: the admission also takes the steering and follow-up messages queued while the lane was idle
    /// (by the queue modes); messages queued for the next run are always taken.
    fn prompt_with(&mut self, prompt: Value, waiter: Option<Reply>, take_queued: bool) -> Result<(), (String, Reply)> {
        let fail = |error: String, waiter: Option<Reply>| -> Result<(), (String, Reply)> {
            match waiter {
                Some(waiter) => Err((error, waiter)),
                None => Err((error, Box::new(|_| {}))),
            }
        };
        // A stored OAuth credential may authenticate by header alone (Kimi's bearer): no key, but `getAuth` resolves.
        let api_key = self.api_key();
        if api_key.is_none() && !lane::header_auth(&self.env, &self.model.provider) {
            return fail(format!("No API key found for {}", self.model.provider), waiter);
        }
        let text = message_text(&prompt);
        let continuation = prompt.get(crate::recovery::RECOVERY_DEPTH).is_some();
        // Name a new conversation from its first prompt; the model's title may replace it before the run ends.
        let mut naming_request = None;
        // RPC clients see the session as TypeScript's RPC mode does: unnamed until they name it.
        // A completion event's run (a custom message) never names the session.
        let auto_name = self.auto_name
            && !continuation
            && prompt["role"] != "custom"
            && self.runtime.mode != "rpc"
            && !(self.env.var)("ULTRON_AUTO_NAME")
                .is_some_and(|v| matches!(v.trim().to_lowercase().as_str(), "off" | "0" | "false"));
        let named = if auto_name {
            names::apply_auto_name(&mut lock_session(&self.session), &text)
        } else {
            Ok(None)
        };
        match named {
            Ok(Some(_)) => {
                self.name_changed();
                let naming = naming::naming_settings(&self.runtime.config.settings);
                if naming.enabled {
                    // A `provider/id` setting names a built-in or models.json model; else the provider's
                    // cheapest, which models.json may override (its URL, headers).
                    let named = naming
                        .model
                        .as_deref()
                        .and_then(|spec| spec.split_once('/'))
                        .and_then(|(provider, id)| lane::catalog_model(provider, id, &self.env).ok());
                    let naming_model = named.unwrap_or_else(|| {
                        let chosen = naming::naming_model(naming.model.as_deref(), &self.model, &api_supported);
                        if chosen.provider == self.model.provider && chosen.id == self.model.id {
                            return self.model.clone();
                        }
                        lane::catalog_model(&chosen.provider, &chosen.id, &self.env).unwrap_or(chosen)
                    });
                    let key = if naming_model.provider == self.model.provider {
                        api_key.clone()
                    } else {
                        lane::api_key(&ModelFlags::default(), &self.env, &naming_model.provider)
                    };
                    naming_request = Some((naming_model, key, text));
                }
            }
            Ok(None) => {}
            Err(error) => return fail(error, waiter),
        }
        if let Err(error) = self.store_lane_config() {
            return fail(error, waiter);
        }
        let lane_config = lock_session(&self.session)
            .value(LANE_CONFIG, LANE)
            .cloned()
            .unwrap_or(Value::Null);
        let started_tip = self.tip().map_or(Value::Null, Value::from);
        let settings = self.run_settings();
        // The run takes the messages queued while the lane was idle, placed before its prompt in the order they were
        // queued (`selectAcceptedInbox`); the rest wait in the inbox and the run delivers them.
        let captured: Vec<(String, Value)> = select_accepted(&lock(&self.queues), take_queued);
        let mut inbox = self.lane_state()["inbox"].clone();
        if let Some(items) = inbox.as_array_mut() {
            items.retain(|item| !captured.iter().any(|(id, _)| item["entryId"] == id.as_str()));
        }
        let mut journal = match RunJournal::admit_capturing(
            Some(&mut lock_session(&self.session)),
            lane_config,
            prompt.clone(),
            (self.model.max_tokens, self.model.context_window),
            settings,
            &captured,
            inbox,
        ) {
            Ok(journal) => journal,
            Err(error) => return fail(error, waiter),
        };
        if !captured.is_empty() {
            lock(&self.queues)
                .items
                .retain(|q| !captured.iter().any(|(id, _)| *id == q.entry_id));
            self.queue_update();
        }
        let captured: Vec<Value> = captured.into_iter().map(|(_, message)| message).collect();
        // The title call runs beside the turn and is applied whenever it answers (while the name is automatic).
        if let Some((naming_model, key, text)) = naming_request {
            let (tx, provider) = (self.tx.clone(), self.provider.clone());
            std::thread::spawn(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
                    return;
                };
                let request = naming::request(&text);
                let options = naming::options(key);
                let mut sink = |_: &ai::AssistantMessageEvent| {};
                let call = provider.stream(&naming_model, &request, &options, &mut sink);
                if let Ok(title) = runtime.block_on(async { tokio::time::timeout(naming::TIMEOUT, call).await }) {
                    let _ = tx.send(Input::Title(Box::new(title)));
                }
            });
        }
        // Work the turn admits is charged to a usage root of its own; the ledger counts its responses.
        journal.ledger_counts_turns();
        journal.set_retry_policy(self.summary_retry().value());
        if let Some(options) = self.provider.stream_options() {
            journal.set_stream_options(options);
        }
        self.before_agent_start(&prompt, journal.run().map(|r| r.operation_id()));
        if let Some(run) = journal.run() {
            self.lane.stack.agents.begin_root_turn(run.operation_id());
            self.durable
                .children
                .set_root(self.lane.stack.agents.usage_root_for_lane(LANE, run.operation_id()));
        }
        // The turn runs in the durable runtime; its prompt is the entry the run just admitted.
        let prompt_entry = self.tip().unwrap_or_default();
        let submission = match self.start_turn(&prompt, &prompt_entry) {
            Ok(submission) => submission,
            Err(error) => {
                let _ = journal.finish(Some(&mut lock_session(&self.session)), Some(("run_failed", &error)));
                return fail(error, waiter);
            }
        };
        let signal = AbortSignal::new();
        let mut random = [0u8; 16];
        let _ = getrandom_fill(&mut random);
        let id = self.run_ids.next(now_ms(), random);
        self.branches.run_started();
        // Per-turn guard times start from zero (`fileHooks.beginTurn()` on the root lane's `run_start`), and a
        // session in which the root ran keeps its counters, even when they are all zero.
        self.lane.stack.begin_turn();
        self.lane.stack.stats.touch();
        self.run = Some(Running {
            id: id.clone(),
            signal,
            journal,
            prompt,
            captured,
            submission: Some(submission),
            prompt_pending: true,
            turns: 0,
            started_tip,
            error: None,
            waiter,
            cancelled: false,
            compaction_failed: false,
            resume_overflow: None,
        });
        self.emit(event(vec![
            ("type", json!("run_start")),
            ("runId", json!(id)),
            ("startedAt", json!(now_ms())),
            ("lane", json!(LANE)),
        ]));
        self.spawn_waiter();
        Ok(())
    }

    /// [`Op::Recover`]: start what the dead worker left: the first queued message, else the interrupted run's
    /// continuation (unless it was interrupted [`crate::recovery::MAX_RECOVERY_DEPTH`] times in a row, or
    /// `ULTRON_RECOVERY_RESUME` is `off`).
    fn recover(&mut self) -> OpResult {
        let Some(recovered) = self.recovered.take() else {
            return Ok(Some(json!({"resumed": false, "reason": "nothing to recover"})));
        };
        let summary = json!({
            "operationId": recovered.operation_id, "status": recovered.status, "depth": recovered.depth,
            "interruptedCalls": recovered.interrupted_calls, "queued": recovered.queued.len(),
        });
        let answer = |resumed: bool, run: Value, reason: &str| {
            Ok(Some(
                json!({"resumed": resumed, "runId": run, "reason": reason, "recovered": summary}),
            ))
        };
        // A run of the durable runtime was resumed when the session opened.
        if let Some(run) = self.resumed.take() {
            return answer(true, json!(run), "durable run resumed");
        }
        if self.run.is_some() {
            return answer(false, Value::Null, "a run is in progress");
        }
        let resume_off = (self.env.var)("ULTRON_RECOVERY_RESUME")
            .is_some_and(|v| matches!(v.trim().to_lowercase().as_str(), "off" | "0" | "false"));
        // The interrupted run continues first; queued messages stay queued and the loop delivers them as it would
        // have (steering during the run, follow-ups when it would end). Without one, the first queued message runs.
        // A message queued for the next run waits for one; it never starts it.
        let first = lock(&self.queues)
            .items
            .iter()
            .find(|q| q.kind != QueueKind::NextRun)
            .cloned();
        let prompt = match first {
            _ if recovered.resume && !resume_off => {
                let mut prompt = user_message(crate::recovery::CONTINUE_PROMPT.to_string(), Vec::new());
                prompt[crate::recovery::RECOVERY_DEPTH] = json!(recovered.depth + 1);
                Some((prompt, "interrupted run"))
            }
            // An aborted run's queue waits for the user, as it would have after a live abort.
            Some(queued) if recovered.status.as_deref() != Some("aborted") => {
                self.remove_queued(std::slice::from_ref(&queued.entry_id))?;
                let mut prompt = queued.message;
                prompt["timestamp"] = json!(now_ms());
                Some((prompt, "queued message"))
            }
            _ => None,
        };
        let Some((prompt, reason)) = prompt else {
            let reason = if recovered.operation_id.is_none() {
                "nothing to recover"
            } else if recovered.resume {
                "resuming is off"
            } else {
                "the interrupted run is not resumed"
            };
            return answer(false, Value::Null, reason);
        };
        // Recovery runs the first queued message (or the continuation); the others are delivered by the run, in
        // order, as before.
        match self.prompt_with(prompt, None, false) {
            Ok(()) => answer(true, self.run_id(), reason),
            Err((error, _)) => Err(error),
        }
    }

    fn enqueue(
        &mut self,
        kind: QueueKind,
        message: String,
        images: Vec<Value>,
        entry_id: Option<String>,
    ) -> Result<String, String> {
        if message.is_empty() && images.is_empty() {
            return Err(EMPTY_QUEUED.into());
        }
        let message = user_message(message, images);
        // The message waits in the lane's inbox as a pending entry.
        let entry_id =
            entry_id.unwrap_or_else(|| session_journal::lock_clock(&lock_session(&self.session).clock()).next_id());
        let mut lane_state = self.lane_state();
        if let Some(inbox) = lane_state["inbox"].as_array_mut() {
            inbox.push(json!({"entryId": entry_id, "kind": kind.name()}));
        }
        if let Some(run) = self.run.as_mut() {
            run.journal.set_inbox(lane_state["inbox"].clone());
        }
        lock_session(&self.session).commit(&[
            Write::set(PENDING_ENTRY, &entry_id, json!({"type": "message", "payload": message})),
            Write::set(LANE_STATE, LANE, lane_state),
        ])?;
        lock(&self.queues).items.push(Queued {
            kind,
            message,
            entry_id: entry_id.clone(),
            turn: 0,
            submission: None,
        });
        // During a run the message waits in the durable runtime's inbox for its boundary.
        if self.run.is_some() {
            self.submit_queued()?;
        }
        self.queue_update();
        Ok(entry_id)
    }

    /// Remove queued messages (their pending entries and inbox items) in one transaction.
    fn remove_queued(&mut self, ids: &[String]) -> Result<(), String> {
        let mut lane_state = self.lane_state();
        if let Some(inbox) = lane_state["inbox"].as_array_mut() {
            inbox.retain(|item| !ids.iter().any(|id| item["entryId"] == id.as_str()));
        }
        if let Some(run) = self.run.as_mut() {
            run.journal.set_inbox(lane_state["inbox"].clone());
        }
        let mut writes: Vec<Write> = ids.iter().map(|id| Write::delete(PENDING_ENTRY, id)).collect();
        writes.push(Write::set(LANE_STATE, LANE, lane_state));
        lock_session(&self.session).commit(&writes)?;
        if !ids.is_empty() {
            self.withdraw_queued(ids);
        }
        for id in ids {
            let mut queues = lock(&self.queues);
            if let Some(index) = queues.items.iter().position(|q| q.entry_id == *id) {
                queues.items.remove(index);
            }
            drop(queues);
            self.queue_update();
        }
        Ok(())
    }

    fn clear_queue(&mut self) -> Value {
        let (steering, follow_up, cleared) = {
            let queues = lock(&self.queues);
            let visible = queues.visible();
            let pick = |kind: QueueKind| -> Vec<String> {
                visible
                    .iter()
                    .filter(|q| q.kind == kind)
                    .map(|q| message_text(&q.message))
                    .collect()
            };
            let cleared: Vec<String> = queues.items.iter().map(|q| q.entry_id.clone()).collect();
            // A next-run message is listed with the follow-ups, as RPC's `queue_update` lists it.
            let mut follow_up = pick(QueueKind::FollowUp);
            follow_up.extend(pick(QueueKind::NextRun));
            (pick(QueueKind::Steer), follow_up, cleared)
        };
        if !cleared.is_empty() {
            let _ = self.remove_queued(&cleared);
        }
        json!({"steering": steering, "followUp": follow_up})
    }

    fn bash(&mut self, op: Op, reply: Reply) {
        let Op::Bash { command, exclude, id } = op else { return };
        let signal = kernel::Signal::new();
        self.bash = Some(signal.clone());
        let host = self.lane.host.clone();
        let extensions = self.extension_host();
        let cwd = self.env.cwd.display().to_string();
        let tx = self.tx.clone();
        // session-control-provider.ts `bash`: the `shellCommandPrefix` setting runs first; the message keeps the
        // command as typed.
        let prefix = self
            .runtime
            .config
            .settings
            .as_object()
            .and_then(config::settings_keys::shell_command_prefix);
        std::thread::spawn(move || {
            // Off the actor: a handler may ask the session for anything while it decides.
            let result = user_bash::run(
                extensions.as_deref(),
                &host,
                &command,
                prefix.as_deref(),
                exclude,
                &cwd,
                &signal,
                &mut |delta| {
                    let _ = tx.send(Input::BashOutput {
                        id: id.clone(),
                        delta: delta.to_string(),
                    });
                },
            );
            let _ = tx.send(Input::BashDone {
                command,
                exclude,
                result,
                reply,
            });
        });
    }
}

fn getrandom_fill(bytes: &mut [u8; 16]) -> Result<(), String> {
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
        ^ u128::from(std::process::id());
    bytes.copy_from_slice(&seed.to_le_bytes());
    Ok(())
}

/// Start an actor for `options` on its own thread; returns its input, or the open error.
pub fn spawn(options: OpenOptions, sink: EventSink) -> Result<(Sender<Input>, JoinHandle<()>, Value), String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let input = tx.clone();
    let handle = std::thread::spawn(move || match Core::open(options, input, sink) {
        Ok(core) => {
            let _ = ready_tx.send(Ok((
                core.snapshot(),
                core.model().clone(),
                core.thinking_level().to_string(),
            )));
            core.run_loop(rx);
        }
        Err(error) => {
            let _ = ready_tx.send(Err(error));
        }
    });
    let ready = ready_rx.recv().map_err(|e| e.to_string())??;
    let (snapshot, model, thinking) = ready;
    let mut summary = json!({"snapshot": snapshot, "model": serde_json::to_value(&model).unwrap_or(Value::Null)});
    summary["thinkingLevel"] = json!(thinking);
    Ok((tx, handle, summary))
}

/// Run `op` on the actor and wait for its answer.
pub fn call(tx: &Sender<Input>, op: Op) -> OpResult {
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    tx.send(Input::Op(
        op,
        Box::new(move |result| {
            let _ = reply_tx.send(result);
        }),
    ))
    .map_err(|_| "Session is closed".to_string())?;
    reply_rx.recv().map_err(|_| "Session is closed".to_string())?
}
