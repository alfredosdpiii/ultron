//! The durable many-agent APIs under `ultron claude` (swarm_api.py): persistent children (`rlm.spawn(keep=True)`,
//! `rlm.ask`), the swarm (`swarm.*`), the task board (`board.*`) and shared notes (`notes.*`), served by the same
//! code as in a native session (session_core::swarm_ops over `harness::chat::host_ops`), on a durable runtime of the
//! server's own Ultron session.
//!
//! A native session's durable runtime runs the root's turns too; here Claude Code owns the root, so the runtime runs
//! only child conversations (a persistent child, and its own persistent children): the root conversation exists, so
//! the root lane's requests are its conversation's, but it never runs (no request of it reaches the model). The
//! runtime's journal is in the session (session_core::durable_store), so persistent children, the board, the notes
//! and the swarm's messages survive a restart and `claude --resume` reopens them.
//!
//! - A persistent child runs on an Ultron model, never a Claude Code process: `claude -p` cannot be asked a
//!   follow-up in the same context, and the `claude-code` provider runs without tools (no `rlm`). Its model is
//!   `model=` of the spawn, else the server's Ultron child model ([`keep_model`]); a `claude-code/` model is refused.
//! - Its model turns are charged to the session's usage ledger as a native session charges a persistent child (the
//!   ledger's default root), and the session's budget gate stops a spent tree's requests.
//! - `swarm.send(to, text, wake=True)` to the root (Claude Code: the name it registered, else `root`) cannot start
//!   a turn of the runtime's root conversation: the message is sent, and also goes to the root's inbox as a
//!   `<swarm_message>` line, which the next cell result or the wait-events hook brings to Claude Code
//!   (crate::hooks::WAIT_EVENTS).

use crate::root_sink::RootSink;
use ai::AssistantMessage;
use durable::{ConversationId, ROOT_CONVERSATION_ID};
use harness::chat::{AgentChange, ChatHarness, ChatOptions, CompactionPolicy, ModelRef, RetryPolicy, Settings};
use harness::scheduler::Registry;
use serde_json::{Map, Value, json};
use session_core::compose::Runtime;
use session_core::durable_store::{JournalStorage, SharedSession};
use session_core::lane::{Lane, MAIN};
use session_core::subagents::{CONVERSATION_LANE_PREFIX, DurableChildren};
use session_core::swarm_ops::SwarmOps;
use session_core::turns::{Bridge, TurnCells, TurnModels};
use std::sync::Arc;

/// Generation partials are not committed (as the native runtime: nothing shows a child's stream).
const NO_PARTIALS_MS: u64 = 365 * 24 * 3600 * 1000;

/// The swarm, board and notes requests (the rest of what session_core::swarm_ops serves is `rlm.*`).
pub const SHARED_RECORDS: [&str; 12] = [
    "swarm.register",
    "swarm.agents",
    "swarm.route",
    "swarm.send",
    "swarm.inbox",
    "board.add",
    "board.claim",
    "board.update",
    "board.list",
    "notes.read",
    "notes.append",
    "notes.write",
];

/// Whether `payload` of an `rlm.spawn` asks for a persistent child.
pub fn is_keep_spawn(kind: &str, payload: &Map<String, Value>) -> bool {
    let keep = |value: Option<&Value>| value == Some(&Value::Bool(true));
    kind == "rlm.spawn" && (keep(payload.get("keep")) || keep(payload.get("kwargs").and_then(|k| k.get("keep"))))
}

/// The model persistent children run on when the spawn names none: the server's Ultron child model
/// (`--ultron-child-model`, `claudeCode.ultronChildModel`, the default model), else `rlm.childModel`; never a
/// Claude Code model (a persistent child needs the `rlm` tool).
pub fn keep_model(ultron_child_model: Option<&str>, settings_child_model: Option<&str>) -> Option<ModelRef> {
    [ultron_child_model, settings_child_model]
        .into_iter()
        .flatten()
        .map(str::trim)
        .filter(|model| !model.starts_with("claude-code/"))
        .find_map(|model| crate::mcp_server::split_model(model).map(|(provider, id)| ModelRef::new(&provider, &id)))
}

/// The durable runtime of an `ultron mcp` root and the APIs served over it.
pub struct DurableApis {
    pub chat: ChatHarness,
    pub children: Arc<DurableChildren>,
    pub swarm: Arc<SwarmOps>,
    /// Where persistent children run when the spawn names no model.
    pub model: Option<ModelRef>,
    sink: Arc<RootSink>,
}

/// Open the durable runtime over `session` for `lane`'s stack and serve the APIs from it (sets `lane.stack.swarm`).
pub fn open(
    session: &SharedSession,
    lane: &Lane,
    runtime: &Runtime,
    env: &session_core::RunEnv,
    model: Option<ModelRef>,
    sink: Arc<RootSink>,
) -> Result<Arc<DurableApis>, String> {
    let stack = lane.stack.clone();
    let children = DurableChildren::new();
    // The children's session (their contracts are values of it, session_core::kept_contracts). Its subagent host
    // is not used: this server's subagents are Claude Code processes or in-memory loops.
    drop(children.host(session.clone()));
    let storage = JournalStorage::open(session.clone(), None).map_err(|e| e.message().to_string())?;
    // Nothing reaches an actor: the root conversation never runs, and children's requests never use the bridge.
    let bridge = Bridge::new(Arc::new(|_| {}));
    let provider =
        Arc::new(session_core::lane::Provider::new().with_live_settings(runtime.config.settings_reader(), true));
    let stream_provider = provider.clone();
    let retry = runtime.config.settings["retry"].clone();
    let settings: harness::chat::SettingsSource = Arc::new(move || {
        let policy = session_core::compaction_op::SummaryRetry::from_settings(&retry, true);
        Settings {
            stream: stream_provider
                .stream_options()
                .and_then(|options| options.as_object().cloned())
                .unwrap_or_default(),
            retry: RetryPolicy {
                enabled: policy.max_attempts > 1,
                max_retries: policy.max_attempts - 1,
                base_delay_ms: policy.base_delay_ms,
                max_delay_ms: policy.max_delay_ms,
            },
            compaction: CompactionPolicy {
                enabled: false,
                ..CompactionPolicy::default()
            },
            partial_interval_ms: NO_PARTIALS_MS,
            follow_up_after_steer: true,
            ..Settings::default()
        }
    });
    let (catalog_env, key_env) = (env.clone(), env.clone());
    // Requests of no conversation carry this provider session: every request is a child's.
    let root_session = Arc::new(std::sync::OnceLock::new());
    let _ = root_session.set(format!("ultron-claude-root-{}", crate::random_hex(8)));
    let (charged, charge_stack) = (children.clone(), stack.clone());
    let budget = session_core::view_images::context_image_budget((env.var)("ULTRON_CONTEXT_IMAGES").as_deref());
    let models = TurnModels {
        bridge: bridge.clone(),
        provider: provider as Arc<dyn agent::StreamFn>,
        resolve: Arc::new(move |provider, id| session_core::lane::catalog_model(provider, id, &catalog_env).ok()),
        api_key: Arc::new(move |provider| {
            session_core::lane::api_key(&session_core::ModelFlags::default(), &key_env, provider)
        }),
        root_session,
        // Each response is charged to the ledger when it arrives, to the root its conversation belongs to (the
        // ledger's default root for a persistent child), as the native runtime's `charge_turns`.
        charge: Some(Arc::new(
            move |conversation: ConversationId, message: &AssistantMessage| {
                let root = charged.usage_root(conversation, &charge_stack.agents);
                let usage = serde_json::to_value(&message.usage).unwrap_or(Value::Null);
                let _ = charge_stack.usage.record_turn(
                    root.as_deref(),
                    usage["totalTokens"].as_f64(),
                    usage["cost"]["total"].as_f64(),
                );
            },
        )),
        transform_child: Some(Arc::new(move |_, messages: &mut Vec<ai::Message>| {
            session_core::view_images::apply_image_budget(messages, budget);
        })),
        trace: session_core::request_trace::RequestTrace::from_env(&*env.var)
            .map(|trace| (trace, session_core::durable_store::lock(session).id().to_string())),
    };
    let cells = TurnCells {
        bridge,
        tool: lane.tool.clone(),
        progress: stack.cell_progress.clone(),
        stack: stack.clone(),
        children: children.clone(),
    };
    // A child conversation's system prompt is a subagent's.
    let (section_children, section_stack) = (children.clone(), stack.clone());
    let sections: harness::chat::SectionSource = Arc::new(move |conversation, _| {
        let mut sections = indexmap::IndexMap::new();
        if conversation != ROOT_CONVERSATION_ID {
            let system = section_children
                .lane_of(conversation)
                .and_then(|lane| section_children.system_of(&lane))
                .unwrap_or_else(|| section_stack.child_system());
            sections.insert("system".to_string(), system);
        }
        Ok(sections)
    });
    let mut options = ChatOptions::new(Registry::new(), Arc::new(models), Arc::new(cells));
    options.settings = Some(settings);
    options.sections = Some(sections);
    // The session's per-root limits on every child run (session_core::core::budget_gate).
    let (root_children, root_stack) = (children.clone(), stack.clone());
    let gate = session_core::core::budget_gate::BudgetGate::new(
        stack.usage.clone(),
        stack.stats.clone(),
        Arc::new(move |conversation| {
            if conversation == ROOT_CONVERSATION_ID {
                root_children.current_root()
            } else {
                root_children.usage_root(conversation, &root_stack.agents)
            }
        }),
    );
    let request_gate = gate.clone();
    options.hooks.gate_request = Some(Arc::new(move |run| request_gate.before_request(run)));
    options.hooks.gate_tool = Some(Arc::new(move |run, call| gate.before_tool(run, call)));
    // A child conversation compacts at its threshold, as in a native session.
    options.hooks.compaction = Some(Arc::new(|conversation| {
        Some(CompactionPolicy {
            enabled: conversation != ROOT_CONVERSATION_ID,
            background_tokens: 0,
            ..CompactionPolicy::default()
        })
    }));
    let chat = ChatHarness::open(storage, options).map_err(|e| e.to_string())?;
    let root = chat.root(None).map_err(|e| e.to_string())?;
    // A persistent child spawned without `model=` runs on its parent conversation's model.
    if let Some(model) = &model {
        root.configure(&AgentChange::model(model.clone()))
            .map_err(|e| e.to_string())?;
    }
    children.attach(chat.clone());
    let swarm = Arc::new(SwarmOps::new(children.clone()).with_contracts(Arc::downgrade(&stack)));
    let _ = stack.swarm.set(swarm.clone());
    // Whatever the runtime holds (persistent children of an earlier server) continues now.
    chat.resume().map_err(|e| e.to_string())?;
    Ok(Arc::new(DurableApis {
        chat,
        children,
        swarm,
        model,
        sink,
    }))
}

/// The name an agent goes by in the swarm: what it registered for `conversation`, else `root` for the root.
fn agent_name(agents: &Value, conversation: ConversationId) -> String {
    agents
        .as_object()
        .and_then(|agents| {
            agents
                .iter()
                .find(|(_, entry)| entry["conversation"].as_u64() == Some(conversation.0))
                .map(|(name, _)| name.clone())
        })
        .unwrap_or_else(|| {
            if conversation == ROOT_CONVERSATION_ID {
                "root".into()
            } else {
                format!("conversation-{}", conversation.0)
            }
        })
}

impl DurableApis {
    /// The conversation `lane` works in, if it has one in this runtime (the root lane, a persistent child's lane).
    pub fn conversation(&self, lane: &str) -> Option<ConversationId> {
        if lane == MAIN {
            return Some(ROOT_CONVERSATION_ID);
        }
        if let Some(id) = lane.strip_prefix(CONVERSATION_LANE_PREFIX) {
            return id.parse().ok().map(ConversationId);
        }
        self.children.conversation_of_lane(lane)
    }

    /// Why `kind` with `payload` from `lane` cannot be served here, if it cannot; `None` passes it on.
    pub fn refusal(&self, lane: &str, kind: &str, payload: &Map<String, Value>) -> Option<String> {
        let shared = SHARED_RECORDS.contains(&kind) || kind == "rlm.ask" || is_keep_spawn(kind, payload);
        if !shared {
            return None;
        }
        if self.conversation(lane).is_none() {
            let api = if kind.starts_with("rlm.") {
                if kind == "rlm.ask" {
                    "rlm.ask"
                } else {
                    "rlm.spawn(keep=True)"
                }
            } else {
                kind.split('.').next().unwrap_or(kind)
            };
            return Some(format!(
                "{api} is not available to this subagent in ultron claude: only the root and its persistent children (rlm.spawn(keep=True)) share the session's persistent agents, swarm, board and notes; put what the root needs in your answer"
            ));
        }
        if !is_keep_spawn(kind, payload) {
            return None;
        }
        let named = payload
            .get("kwargs")
            .and_then(|kwargs| kwargs.get("model"))
            .and_then(Value::as_str);
        match named {
            Some(model) if model.trim().starts_with("claude-code/") => Some(format!(
                "rlm.spawn(keep=True) cannot run on {model}: a persistent child in ultron claude runs on an Ultron model (a Claude Code model has no rlm tool and cannot take follow-ups); name one with model=\"provider/model\""
            )),
            None if self.model.is_none() && lane == MAIN => Some(
                "rlm.spawn(keep=True) needs an Ultron model in ultron claude (a Claude Code subagent cannot take follow-ups): name one with model=\"provider/model\", or set one with --ultron-child-model, claudeCode.ultronChildModel or rlm.childModel"
                    .into(),
            ),
            _ => None,
        }
    }

    /// `swarm.send(..., wake=True)` from `lane` whose recipient is the root: the message is sent, and the root
    /// (Claude Code) gets it in its inbox instead of a turn of the runtime's root conversation. `None`: not such a
    /// request (pass it on).
    pub fn wake_root(
        &self,
        lane: &str,
        kind: &str,
        payload: &Map<String, Value>,
        next: session_core::request_layer::Next<'_>,
    ) -> Option<Result<Value, String>> {
        if kind != "swarm.send" || payload.get("wake") != Some(&Value::Bool(true)) {
            return None;
        }
        let to = payload.get("to").and_then(Value::as_str)?;
        let agents = self.chat.commit(harness::chat::swarm::agents).unwrap_or(Value::Null);
        // The root's name: what it registered, else `root` (its name in the swarm until it registers another).
        let root = match agents.get(to) {
            Some(entry) => entry["conversation"].as_u64() == Some(ROOT_CONVERSATION_ID.0),
            None => to == "root",
        };
        if !root {
            return None;
        }
        let mut quiet = payload.clone();
        quiet.insert("wake".into(), false.into());
        let sent = match next(quiet) {
            Ok(sent) => sent,
            Err(error) => return Some(Err(error)),
        };
        let from = self
            .conversation(lane)
            .map_or_else(|| lane.to_string(), |conversation| agent_name(&agents, conversation));
        let text = payload.get("text").and_then(Value::as_str).unwrap_or_default();
        self.sink.publish(format!(
            "<swarm_message from=\"{}\" to=\"{}\">\n{text}\n</swarm_message>",
            attribute(&from),
            attribute(to)
        ));
        Some(Ok(json!({"id": sent["id"], "woke": true})))
    }

    /// Stop the runtime (persistent children stay in the journal, for the next server of this session).
    pub fn close(&self) {
        let _ = self.chat.close();
    }
}

/// Text as an XML attribute value.
pub fn attribute(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
