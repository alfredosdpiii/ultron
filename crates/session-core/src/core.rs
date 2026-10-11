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
use crate::compose::{Runtime, Services};
use crate::journal::RunJournal;
use crate::lane::{self, Lane, Provider, api_supported, now_ms};
use crate::view::{self, message_text};
use crate::{ModelFlags, RunEnv, naming, pi_view};
use agent::{AgentContext, AgentEvent, AgentLoopConfig, StreamFn, agent_loop};
use ai::{AbortSignal, AssistantMessage, Message, Model, SimpleStreamOptions, StreamOptions};
use kernel::HostHandler;
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

/// A queue a message can join while a run streams.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueKind {
    Steer,
    FollowUp,
}

impl QueueKind {
    pub fn name(self) -> &'static str {
        match self {
            QueueKind::Steer => "steer",
            QueueKind::FollowUp => "followUp",
        }
    }

    pub fn parse(name: &str) -> Option<QueueKind> {
        match name {
            "steer" => Some(QueueKind::Steer),
            "followUp" => Some(QueueKind::FollowUp),
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
    Bash {
        command: String,
        exclude: bool,
    },
    AbortBash,
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
    /// Sessions of this working directory, newest first (`{path, id, name, modified, messageCount, firstMessage}`).
    ListSessions,
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
                kind: kind("kind").ok_or("kind must be steer or followUp")?,
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
                kind: kind("kind").ok_or("kind must be steer or followUp")?,
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
            },
            "abortBash" => Op::AbortBash,
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
            Op::Bash { command, exclude } => json!({"op": "bash", "command": command, "exclude": exclude}),
            Op::AbortBash => json!({"op": "abortBash"}),
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
        }
    }
}

/// What reaches the actor.
pub enum Input {
    Op(Op, Reply),
    Agent(Box<AgentEvent>),
    /// The model's title for the session arrived.
    Title(Box<AssistantMessage>),
    RunEnd,
    BashDone {
        command: String,
        exclude: bool,
        result: Result<Value, String>,
        reply: Reply,
    },
    /// Stop the lane; the sender is told when done.
    Close(std::sync::mpsc::Sender<()>),
}

/// Where lane-watch events go.
pub type EventSink = Box<dyn FnMut(Value) + Send>;

#[derive(Clone)]
struct Queued {
    kind: QueueKind,
    message: Value,
    /// The pending entry (`pi.pending.entry`) the message waits as; it becomes the message's entry id.
    entry_id: String,
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

    /// Take the queued messages of `kind` the queue mode releases (all, or the first).
    fn drain(&mut self, kind: QueueKind) -> Vec<Message> {
        let all = match kind {
            QueueKind::Steer => self.steering_all,
            QueueKind::FollowUp => self.follow_up_all,
        };
        let mut taken = Vec::new();
        let mut index = 0;
        while index < self.items.len() {
            if self.items[index].kind == kind && (all || taken.is_empty()) {
                taken.push(self.items.remove(index));
            } else {
                index += 1;
            }
        }
        let messages = taken
            .iter()
            .flat_map(|q| lane::to_llm(std::slice::from_ref(&q.message)))
            .collect();
        self.delivered.extend(taken);
        messages
    }
}

fn lock<T>(shared: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Running {
    id: String,
    signal: AbortSignal,
    thread: JoinHandle<()>,
    journal: RunJournal,
    /// The run's own prompt has not reached `message_end` yet (later user messages are steering or follow-ups).
    prompt_pending: bool,
    started_tip: Value,
    /// The last assistant message's error, if the run ended on one.
    error: Option<String>,
    /// Answered when the run ends (`PromptAndWait`).
    waiter: Option<Reply>,
    /// A client aborted the run (it ends `aborted`, whatever its last response says).
    cancelled: bool,
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
    session: Session,
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
    /// How many of the session's entries were announced (`entry_added`).
    announced: usize,
    /// How many of the session's usage rows were announced (`usage`).
    announced_usage: usize,
    /// Queued messages committed with the generation that takes them, until their `message_end` arrives.
    taken: Vec<String>,
    run_ids: session_file::Uuidv7,
    auto_name: bool,
}

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
    let flag_model = lane::resolve_model(flags, env);
    let thinking = flags.thinking.clone().unwrap_or_else(|| "medium".into());
    let stored = session
        .as_ref()
        .and_then(|s| s.value(session_journal::session::LANE_CONFIG, "main").cloned());
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
        let (config, model) = lane_config(&flags, &env, Some(&mut session))?;
        let sessions_dir = session_root.join(lane::session_directory_name(&metadata.cwd));
        let runtime = Runtime::new(&env, &mode, sessions_dir, services)?;
        let api_key = lane::api_key(&flags, &env, &model.provider);
        let lane = Lane::open(&runtime, &model, api_key, session.id())?;
        let announced = session.state().entries().len();
        let announced_usage = session.state().usage_rows().len();
        Ok(Core {
            thinking: config["thinkingLevel"].as_str().unwrap_or("medium").to_string(),
            models: lane::available_models(&env),
            provider: Arc::new(Provider::new()),
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
            queues: Arc::new(Mutex::new(Queues {
                items: Vec::new(),
                delivered: Vec::new(),
                steering_all: true,
                follow_up_all: true,
            })),
            auto_compaction: true,
            auto_retry: true,
            bash: None,
            announced_usage,
            announced,
            taken: Vec::new(),
            run_ids: session_file::Uuidv7::default(),
            auto_name,
        })
    }

    pub fn model(&self) -> &Model {
        &self.model
    }

    pub fn thinking_level(&self) -> &str {
        &self.thinking
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn runtime(&self) -> &Runtime {
        &self.runtime
    }

    /// The lane snapshot transcript state starts from (`LaneSnapshot`): the branch's entries, configuration, usage.
    pub fn snapshot(&self) -> Value {
        let entries = self.session.branch_entries(LANE).unwrap_or_default();
        let mut usage = session_journal::usage::empty_usage();
        for row in self.session.state().usage_rows() {
            usage = session_journal::usage::add_usage(&usage, &row["usage"]);
        }
        json!({
            "lane": LANE,
            "transcript": entries,
            "tipId": self.session.branch_tip(LANE).flatten(),
            "configuration": {"model": {"provider": self.model.provider, "modelId": self.model.id}, "thinkingLevel": self.thinking, "activeToolNames": ["rlm"]},
            "stats": {"messageCount": entries.iter().filter(|e| e["type"] == "message").count(), "usage": usage},
            "operation": null,
            "queues": [],
            "faulted": false,
        })
    }

    /// Serve inputs until a close (or the input ends).
    pub fn run_loop(mut self, inputs: std::sync::mpsc::Receiver<Input>) {
        for input in inputs {
            match input {
                Input::Op(op, reply) => self.op(op, reply),
                Input::Agent(event) => self.on_agent_event(*event),
                Input::Title(response) => {
                    if record_naming(&mut self.session, &response).is_ok() {
                        self.name_changed();
                    }
                }
                Input::RunEnd => self.run_end(),
                Input::BashDone {
                    command,
                    exclude,
                    result,
                    reply,
                } => self.bash_done(command, exclude, result, reply),
                Input::Close(done) => {
                    self.close();
                    let _ = done.send(());
                    return;
                }
            }
        }
        self.close();
    }

    fn emit(&mut self, value: Value) {
        (self.sink)(value);
    }

    /// The session's name now (`session_name`, a Rust-only event the worker serves as `ultron.session-ops` state,
    /// not on the transcript).
    fn name_changed(&mut self) {
        let name = self.session.name();
        self.emit(json!({"type": "session_name", "lane": LANE, "name": name}));
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
        let entries: Vec<Value> = self.session.state().entries()[self.announced..].to_vec();
        self.announced += entries.len();
        for entry in entries {
            self.emit(event(vec![
                ("type", json!("entry_added")),
                ("lane", json!(LANE)),
                ("entry", entry),
            ]));
        }
        let rows: Vec<Value> = self.session.state().usage_rows().into_iter().cloned().collect();
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
        self.session.branch_tip(LANE).flatten()
    }

    /// The lane's stored state (`pi.lane.state`), idle when none is stored.
    fn lane_state(&self) -> Value {
        self.session
            .value(LANE_STATE, LANE)
            .cloned()
            .unwrap_or_else(|| json!({"currentOperationId": null, "lastOperationId": null, "inbox": []}))
    }

    /// Append a message to `main` outside a run (the lane's `appendMessage`: the entry, the new tip and the lane's
    /// state in one transaction) and announce the entry.
    fn append(&mut self, message: Value) -> Result<(), String> {
        let id = session_journal::lock_clock(&self.session.clock()).next_id();
        let mut entry = Map::new();
        entry.insert("id".into(), id.clone().into());
        entry.insert("parentId".into(), self.tip().map_or(Value::Null, Value::from));
        entry.insert("type".into(), "message".into());
        entry.insert("message".into(), message);
        let lane_state = self.lane_state();
        self.session.commit(&[
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
        self.session.commit(&[
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
        let mut config = self
            .session
            .value(LANE_CONFIG, LANE)
            .cloned()
            .unwrap_or_else(|| run::lane_config("", "", "", &["rlm"]));
        config["model"] = json!({"provider": self.model.provider, "modelId": self.model.id});
        config["thinkingLevel"] = json!(self.thinking);
        if self.session.value(LANE_CONFIG, LANE) != Some(&config) {
            self.session.commit(&[Write::set(LANE_CONFIG, LANE, config)])?;
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
            run.journal.event(&mut self.session, event);
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
                        None if !first => self.session.append_message(LANE, value).map(drop),
                        None => Ok(()),
                    };
                    if let Err(error) = committed {
                        self.fail_run(error);
                    }
                } else {
                    self.journal(&agent_event);
                }
                self.announce();
                if was_queued {
                    self.queue_update();
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
    fn begin_generation(&mut self) {
        let Some(run) = self.run.as_ref() else { return };
        if run.signal.aborted() {
            return;
        }
        let taken: Vec<Queued> = lock(&self.queues)
            .delivered
            .iter()
            .filter(|q| !self.taken.contains(&q.entry_id))
            .cloned()
            .collect();
        let mut writes = Vec::new();
        let mut parent = self.tip();
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
        if let Some(tip) = &trigger {
            writes.push(Write::set(BRANCH_TIP, LANE, tip.clone().into()));
        }
        let mut inbox = self.lane_state()["inbox"].clone();
        if let Some(items) = inbox.as_array_mut() {
            items.retain(|item| !taken.iter().any(|q| item["entryId"] == q.entry_id.as_str()));
        }
        self.taken.extend(taken.iter().map(|q| q.entry_id.clone()));
        if let Some(run) = self.run.as_mut() {
            run.journal.set_inbox(inbox);
            run.journal.begin_with(&mut self.session, writes, trigger);
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
        let Some(run) = self.run.take() else { return };
        let _ = run.thread.join();
        let aborted = run.cancelled;
        let finished = if aborted {
            run.journal.finish_aborted(&mut self.session)
        } else {
            let failure = run.error.as_deref().map(|m| ("assistant_error", m));
            run.journal.finish(Some(&mut self.session), failure)
        };
        let mut error = if aborted { None } else { run.error.clone() };
        if let Err(write) = finished {
            error.get_or_insert(write);
        }
        self.taken.clear();
        lock(&self.queues).delivered.clear();
        let status = if aborted {
            "aborted"
        } else if error.is_some() {
            "failed"
        } else {
            "completed"
        };
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
        if let Some(waiter) = run.waiter {
            let error = error.map_or(Value::Null, |m| json!({"code": "run_failed", "message": m}));
            waiter(Ok(Some(
                json!({"accepted": true, "operationId": run.id, "error": error}),
            )));
        }
    }

    fn bash_done(&mut self, command: String, exclude: bool, result: Result<Value, String>, reply: Reply) {
        self.bash = None;
        let result = match result {
            Ok(result) => result,
            Err(e) => return reply(Err(e)),
        };
        let mut message = Map::new();
        message.insert("role".into(), "bashExecution".into());
        message.insert("command".into(), command.into());
        message.insert("output".into(), result["output"].clone());
        if !result["exit_code"].is_null() {
            message.insert("exitCode".into(), result["exit_code"].clone());
        }
        message.insert("cancelled".into(), result["cancelled"].clone());
        message.insert("truncated".into(), result["truncated"].clone());
        if let Some(path) = result["full_output_path"].as_str() {
            message.insert("fullOutputPath".into(), path.into());
        }
        message.insert("timestamp".into(), now_ms().into());
        if exclude {
            message.insert("excludeFromContext".into(), true.into());
        }
        let message = Value::Object(message);
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
        let mut data = Map::new();
        for (from, to) in [
            ("output", "output"),
            ("exit_code", "exitCode"),
            ("cancelled", "cancelled"),
            ("truncated", "truncated"),
            ("full_output_path", "fullOutputPath"),
        ] {
            if !result[from].is_null() {
                data.insert(to.into(), result[from].clone());
            }
        }
        reply(Ok(Some(Value::Object(data))));
    }

    /// Stop the lane (kernels, subagents, extensions).
    pub fn close(&mut self) {
        if let Some(run) = &self.run {
            run.signal.abort();
        }
        if let Some(signal) = &self.bash {
            signal.abort("closing");
        }
        self.lane.shutdown();
        if let Some(host) = &self.runtime.extensions {
            host.shutdown();
        }
    }

    /// The Pi view of the session the read operations use.
    fn pi(&self) -> SessionManager {
        pi_view::pi_manager(&self.session)
    }

    fn session_file(&self) -> String {
        self.session.metadata().path.display().to_string()
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
            other => {
                let result = self.sync_op(other);
                reply(result);
            }
        }
    }

    fn sync_op(&mut self, op: Op) -> OpResult {
        match op {
            Op::Prompt { .. } | Op::PromptAndWait { .. } | Op::Bash { .. } => unreachable!("answered later"),
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
                if let Some(run) = self.run.as_mut()
                    && !run.signal.aborted()
                {
                    run.signal.abort();
                    run.cancelled = true;
                    run.journal.request_cancel(&mut self.session, now_ms());
                }
                Ok(None)
            }
            Op::State => Ok(Some(self.state())),
            Op::AvailableModels => Ok(Some(Value::Array(
                self.models
                    .iter()
                    .filter_map(|m| serde_json::to_value(m).ok())
                    .collect(),
            ))),
            Op::SetModel { provider, model_id } => {
                let model = self
                    .models
                    .iter()
                    .find(|m| m.provider == provider && m.id == model_id)
                    .cloned()
                    .ok_or_else(|| format!("Model not found: {provider}/{model_id}"))?;
                self.model = model;
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
                Ok(None)
            }
            // No retry is ever scheduled in the Rust build, so there is nothing to abort.
            Op::AbortRetry => Ok(None),
            Op::AbortBash => {
                if let Some(signal) = &self.bash {
                    signal.abort("abort_bash");
                }
                Ok(None)
            }
            Op::Stats => Ok(Some(view::stats(
                &self.pi(),
                Some(&self.session_file()),
                self.session.id(),
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
            Op::Tree => {
                let pi = self.pi();
                Ok(Some(
                    json!({"tree": view::tree(&pi), "leafId": pi.leaf_id().cloned().unwrap_or(Value::Null)}),
                ))
            }
            Op::LastAssistantText => Ok(Some(json!(view::last_assistant_text(&self.pi())))),
            Op::SetName { name } => {
                names::set_user_name(&mut self.session, Some(&name))?;
                self.name_changed();
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
                session.commit(&[Write::set(
                    "ultron.session.start",
                    "fork",
                    json!({"previousSessionFile": self.session_file()}),
                )])?;
                session.commit(&[Write::delete("ultron.session.start", "fork")])?;
                Ok(None)
            }
            Op::Describe => Ok(Some(json!({
                "sessionId": self.session.id(),
                "sessionFile": self.session_file(),
                "name": self.session.name(),
                "messages": view::messages(&self.pi()),
                "model": serde_json::to_value(&self.model).unwrap_or(Value::Null),
                "thinkingLevel": self.thinking,
            }))),
            Op::ListSessions => {
                let listed = names::list_sessions(Path::new(self.repo.root()), &self.session.metadata().cwd);
                Ok(Some(Value::Array(
                    listed
                        .into_iter()
                        .map(|s| {
                            json!({"path": s.metadata.path.display().to_string(), "id": s.metadata.id, "name": s.name,
                                "modified": s.metadata.modified_at as i64, "messageCount": s.message_count,
                                "firstMessage": s.first_message})
                        })
                        .collect(),
                )))
            }
        }
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
        state.insert("isCompacting".into(), false.into());
        let mode = |all: bool| if all { "all" } else { "one-at-a-time" };
        state.insert("steeringMode".into(), mode(queues.steering_all).into());
        state.insert("followUpMode".into(), mode(queues.follow_up_all).into());
        state.insert("sessionFile".into(), self.session_file().into());
        state.insert("sessionId".into(), self.session.id().into());
        if let Some(name) = self.session.name() {
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
        let fail = |error: String, waiter: Option<Reply>| -> Result<(), (String, Reply)> {
            match waiter {
                Some(waiter) => Err((error, waiter)),
                None => Err((error, Box::new(|_| {}))),
            }
        };
        let Some(api_key) = self.api_key() else {
            return fail(format!("No API key found for {}", self.model.provider), waiter);
        };
        let text = message.clone();
        let prompt = user_message(message, images);
        let mut history = match self.session.context_messages(LANE) {
            Ok(history) => history,
            Err(error) => return fail(error, waiter),
        };
        history.push(prompt.clone());
        if lane::clock_enabled(&self.env)
            && let Some(lined) = lane::with_clock_lines(&history)
        {
            history = lined;
        }
        history.pop();
        let mut messages: Vec<Message> = self.lane.system_message(&self.runtime.system).into_iter().collect();
        messages.extend(lane::to_llm(&history));
        let context = AgentContext {
            messages,
            tools: self.lane.tools(),
        };
        let mut config = AgentLoopConfig::new(self.model.clone());
        config.api_key = Some(api_key.clone());
        config.reasoning = Some(self.thinking.clone()).filter(|l| l != "off");
        let signal = AbortSignal::new();
        let (steer_queue, steer_signal) = (self.queues.clone(), signal.clone());
        config.get_steering_messages = Some(Box::new(move || {
            if steer_signal.aborted() {
                return Vec::new();
            }
            lock(&steer_queue).drain(QueueKind::Steer)
        }));
        let (follow_queue, follow_signal) = (self.queues.clone(), signal.clone());
        let events = self.lane.events.clone();
        config.get_follow_up_messages = Some(Box::new(move || {
            if follow_signal.aborted() {
                return Vec::new();
            }
            let queued = lock(&follow_queue).drain(QueueKind::FollowUp);
            if !queued.is_empty() {
                return queued;
            }
            // Work the run started that is still running (subagents, detached cells): the host waits for it
            // here, between turns, and its completion continues the run.
            events.next_message(&follow_signal)
        }));
        // Name a new conversation from its first prompt; the model's title may replace it before the run ends.
        let mut naming_request = None;
        // RPC clients see the session as TypeScript's RPC mode does: unnamed until they name it.
        let auto_name = self.auto_name
            && self.runtime.mode != "rpc"
            && !(self.env.var)("ULTRON_AUTO_NAME")
                .is_some_and(|v| matches!(v.trim().to_lowercase().as_str(), "off" | "0" | "false"));
        let named = if auto_name {
            names::apply_auto_name(&mut self.session, &text)
        } else {
            Ok(None)
        };
        match named {
            Ok(Some(_)) => {
                self.name_changed();
                let naming = naming::naming_settings(&self.runtime.config.settings);
                if naming.enabled {
                    let naming_model = naming::naming_model(naming.model.as_deref(), &self.model, &api_supported);
                    let key = if naming_model.provider == self.model.provider {
                        Some(api_key)
                    } else {
                        lane::env_api_key(&self.env, &naming_model.provider)
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
        let lane_config = self.session.value(LANE_CONFIG, LANE).cloned().unwrap_or(Value::Null);
        let started_tip = self.tip().map_or(Value::Null, Value::from);
        let journal = match RunJournal::admit(
            Some(&mut self.session),
            lane_config,
            prompt.clone(),
            (self.model.max_tokens, self.model.context_window),
        ) {
            Ok(journal) => journal,
            Err(error) => return fail(error, waiter),
        };
        let prompts = lane::to_llm(std::slice::from_ref(&prompt));
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
        let tx = self.tx.clone();
        let provider = self.provider.clone();
        let run_signal = signal.clone();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build();
            if let Ok(runtime) = runtime {
                let events = tx.clone();
                let mut emit = move |event: AgentEvent| {
                    let _ = events.send(Input::Agent(Box::new(event)));
                };
                runtime.block_on(agent_loop(prompts, context, config, &mut emit, &run_signal, &*provider));
            }
            let _ = tx.send(Input::RunEnd);
        });
        let mut random = [0u8; 16];
        let _ = getrandom_fill(&mut random);
        let id = self.run_ids.next(now_ms(), random);
        self.run = Some(Running {
            id: id.clone(),
            signal,
            thread,
            journal,
            prompt_pending: true,
            started_tip,
            error: None,
            waiter,
            cancelled: false,
        });
        self.emit(event(vec![
            ("type", json!("run_start")),
            ("runId", json!(id)),
            ("startedAt", json!(now_ms())),
            ("lane", json!(LANE)),
        ]));
        Ok(())
    }

    fn enqueue(
        &mut self,
        kind: QueueKind,
        message: String,
        images: Vec<Value>,
        entry_id: Option<String>,
    ) -> Result<String, String> {
        let message = user_message(message, images);
        // The message waits in the lane's inbox as a pending entry.
        let entry_id = entry_id.unwrap_or_else(|| session_journal::lock_clock(&self.session.clock()).next_id());
        let mut lane_state = self.lane_state();
        if let Some(inbox) = lane_state["inbox"].as_array_mut() {
            inbox.push(json!({"entryId": entry_id, "kind": kind.name()}));
        }
        if let Some(run) = self.run.as_mut() {
            run.journal.set_inbox(lane_state["inbox"].clone());
        }
        self.session.commit(&[
            Write::set(PENDING_ENTRY, &entry_id, json!({"type": "message", "payload": message})),
            Write::set(LANE_STATE, LANE, lane_state),
        ])?;
        lock(&self.queues).items.push(Queued {
            kind,
            message,
            entry_id: entry_id.clone(),
        });
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
        self.session.commit(&writes)?;
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
            (pick(QueueKind::Steer), pick(QueueKind::FollowUp), cleared)
        };
        if !cleared.is_empty() {
            let _ = self.remove_queued(&cleared);
        }
        json!({"steering": steering, "followUp": follow_up})
    }

    fn bash(&mut self, op: Op, reply: Reply) {
        let Op::Bash { command, exclude } = op else { return };
        let signal = kernel::Signal::new();
        self.bash = Some(signal.clone());
        let host = self.lane.host.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let mut payload = Map::new();
            payload.insert("command".into(), command.clone().into());
            let result = host.handle("bash", payload, &signal);
            let _ = tx.send(Input::BashDone {
                command,
                exclude,
                result,
                reply,
            });
        });
    }

    async fn summarize(&self, request: compaction::SummaryRequest) -> AssistantMessage {
        let options = SimpleStreamOptions {
            base: StreamOptions {
                api_key: self.api_key(),
                max_tokens: Some(request.max_tokens),
                cache_retention: Some(ai::CacheRetention::None),
                session_id: request.session_id.clone(),
                ..Default::default()
            },
            reasoning: request.reasoning.clone(),
            ..Default::default()
        };
        let mut sink = |_: &ai::AssistantMessageEvent| {};
        self.provider
            .stream(&self.model, &request.context, &options, &mut sink)
            .await
    }

    /// A manual compaction: `compaction_start`, the journaled operation, the entry, `compaction_end`; answers Pi's
    /// `CompactionResult` fields of the compaction entry.
    fn compact(&mut self, instructions: Option<&str>) -> Result<Value, String> {
        self.idle()?;
        let lane = json!(LANE);
        self.emit(event(vec![
            ("type", json!("compaction_start")),
            ("lane", lane.clone()),
            ("reason", json!("manual")),
        ]));
        let id = match self.compact_now(instructions) {
            Ok(id) => id,
            Err(error) => {
                self.emit(event(vec![
                    ("type", json!("compaction_end")),
                    ("lane", lane),
                    ("reason", json!("manual")),
                    ("status", json!("failed")),
                    ("error", json!({"message": error})),
                ]));
                return Err(error);
            }
        };
        self.announce();
        self.emit(event(vec![
            ("type", json!("compaction_end")),
            ("lane", lane),
            ("reason", json!("manual")),
            ("status", json!("completed")),
        ]));
        let pi = self.pi();
        let entry = pi
            .entry(&id)
            .filter(|e| e["type"] == "compaction")
            .ok_or("Compaction produced no compaction entry")?;
        let mut data = Map::new();
        for key in ["summary", "firstKeptEntryId", "tokensBefore", "usage", "details"] {
            if let Some(value) = entry.get(key).filter(|v| !v.is_null()) {
                data.insert(key.into(), value.clone());
            }
        }
        Ok(Value::Object(data))
    }

    /// Summarize the current path and commit the native compaction entry (its retained tail is the kept messages)
    /// through a journaled compaction operation ([`CompactionOp`]); returns its id. Nothing to cut keeps the whole
    /// path after an (empty-conversation) summary, as the native harness does.
    fn compact_now(&mut self, custom_instructions: Option<&str>) -> Result<String, String> {
        let pi = self.pi();
        let path: Vec<Value> = pi.branch_entries(None).into_iter().cloned().collect();
        let settings = compaction::CompactionSettings::default();
        let preparation = match compaction::prepare_compaction(&path, settings) {
            Some(preparation) => preparation,
            None => {
                if path.last().is_some_and(|e| e["type"] == "compaction") {
                    return Err("Already compacted".into());
                }
                let first = path
                    .first()
                    .and_then(|e| e["id"].as_str())
                    .ok_or("Nothing to compact (session too small)")?;
                let projection = pi.build_session_projection();
                let tokens = compaction::estimate_projected_context_tokens(&projection, &path).tokens;
                compaction::CompactionPreparation {
                    first_kept_entry_id: first.to_string(),
                    messages_to_summarize: Vec::new(),
                    turn_prefix_messages: Vec::new(),
                    is_split_turn: false,
                    tokens_before: tokens,
                    previous_summary: None,
                    file_ops: Default::default(),
                    settings,
                }
            }
        };
        // The messages of the kept entries, from the first kept one to the branch's end.
        let kept_from = path
            .iter()
            .position(|e| e["id"] == preparation.first_kept_entry_id.as_str())
            .unwrap_or(path.len());
        let retained_tail: Vec<Value> = path[kept_from..]
            .iter()
            .filter(|e| e["type"] != "compaction")
            .flat_map(|e| match e["type"].as_str() {
                Some("message") => vec![e["message"].clone()],
                _ => session_journal::context::entry_context_messages(e),
            })
            .collect();
        let op = CompactionOp::begin(&mut self.session, &preparation, &retained_tail, custom_instructions)?;
        let thinking = self.thinking.clone();
        let session_id = self.session.id().to_string();
        let options = compaction::CompactOptions {
            model: &self.model,
            custom_instructions,
            thinking_level: Some(&thinking),
            session_id: Some(&session_id),
            now_ms: now_ms(),
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        let result = match runtime.block_on(compaction::compact(&preparation, &options, async |request| {
            self.summarize(request).await
        })) {
            Ok(result) => result,
            Err(error) => {
                op.fail(&mut self.session, &error)?;
                return Err(error);
            }
        };
        let usage = serde_json::to_value(&result.usage).map_err(|e| e.to_string())?;
        op.record_usage(&mut self.session, &usage)?;
        let mut entry = Map::new();
        entry.insert("type".into(), "compaction".into());
        entry.insert("summary".into(), result.summary.clone().into());
        entry.insert("retainedTail".into(), Value::Array(retained_tail));
        entry.insert("tokensBefore".into(), result.tokens_before.into());
        entry.insert(
            "details".into(),
            json!({"readFiles": result.read_files, "modifiedFiles": result.modified_files}),
        );
        entry.insert("usage".into(), usage);
        entry.insert("fromHook".into(), false.into());
        op.complete(&mut self.session, entry)
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
