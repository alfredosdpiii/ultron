//! Port of upstream `test/chat-support.ts` for the conversation runtime: a faux model, a scripted REPL cell executor
//! (cells named by their code: `hold` waits for its gate and returns its scripted result), live settings, and
//! transcript helpers. Every conversation's model is `faux/faux-1`.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use ai::{Message, Tool, UserContent, UserMessageContent};
use durable::{ConversationId, EntryRecord, Storage, SubmissionRecord};
use harness::chat::faux::{Faux, FauxOptions};
use harness::chat::host_ops::HostOps;
use harness::chat::{
    AgentChange, CellCall, CellExecutor, CellProgress, CellResult, ChatHarness, ChatOptions, Conversation, Hooks,
    Models, Replay, SectionSource, Settings, Submission,
};
use harness::scheduler::{Cancel, HarnessError, Registry};
use serde_json::{Value, json};

use crate::support::{eventually, js};

/// A cell body run against the Harness's host operations.
pub type CellBody = Arc<dyn Fn(&HostOps, &CellCall, &Cancel) -> Result<CellResult, String> + Send + Sync>;

/// What a scripted cell does: publish `output` chunks, wait for `gate` (or cancellation), then return `result`.
#[derive(Clone, Default)]
pub struct Script {
    /// Run this body (with the Harness's host operations) instead.
    pub body: Option<CellBody>,
    pub gate: Option<Cancel>,
    pub output: Vec<String>,
    pub result: Option<CellResult>,
    /// Fail the cell with this message instead (an executor error).
    pub fail: Option<String>,
    /// The first `blocking` runs publish `run N` output and `{run: N}` details, then block until cancelled.
    pub blocking: usize,
    /// Panic in the executor (the scheduler faults the cell).
    pub panic: bool,
}

/// A REPL tool whose cells are scripted by their code; every run is recorded.
#[derive(Default)]
pub struct ScriptCells {
    /// The open Harness's host operations, for scripted bodies.
    pub ops: Mutex<Option<HostOps>>,
    scripts: Mutex<HashMap<String, Script>>,
    runs: Mutex<Vec<String>>,
    replay: Mutex<HashMap<String, Replay>>,
}

pub fn rlm_tool() -> Tool {
    Tool {
        name: "rlm".into(),
        description: "Run Python in the persistent REPL".into(),
        parameters: json!({
            "type": "object",
            "properties": { "code": { "type": "string" } },
            "required": ["code"],
        }),
        constrained_sampling: None,
    }
}

impl ScriptCells {
    pub fn script(&self, code: &str, script: Script) {
        self.scripts.lock().unwrap().insert(code.to_owned(), script);
    }

    /// `hold`: waits for `gate`, then returns `result`.
    pub fn hold(&self, code: &str, gate: &Cancel, result: CellResult) {
        self.script(
            code,
            Script {
                gate: Some(gate.clone()),
                result: Some(result),
                ..Script::default()
            },
        );
    }

    pub fn set_replay(&self, code: &str, replay: Replay) {
        self.replay.lock().unwrap().insert(code.to_owned(), replay);
    }

    pub fn runs(&self) -> Vec<String> {
        self.runs.lock().unwrap().clone()
    }
}

impl CellExecutor for ScriptCells {
    fn tool(&self) -> Tool {
        rlm_tool()
    }

    fn replay(&self, call: &CellCall) -> Replay {
        self.replay
            .lock()
            .unwrap()
            .get(call.code())
            .copied()
            .unwrap_or_default()
    }

    fn execute(&self, call: &CellCall, progress: &CellProgress, signal: &Cancel) -> Result<CellResult, String> {
        self.runs.lock().unwrap().push(call.code().to_owned());
        let script = self.scripts.lock().unwrap().get(call.code()).cloned();
        let Some(script) = script else {
            return Ok(CellResult::text(format!("ran {}", call.code())));
        };
        let run = self
            .runs
            .lock()
            .unwrap()
            .iter()
            .filter(|code| *code == call.code())
            .count();
        if let Some(body) = &script.body {
            let ops = self
                .ops
                .lock()
                .unwrap()
                .clone()
                .expect("open_chat sets the host operations");
            return body(&ops, call, signal);
        }
        if script.panic {
            panic!("cell {} broke", call.code());
        }
        if script.blocking > 0 {
            progress.output(&format!("run {run}\n"));
            progress.details(json!({ "run": run }));
            if run <= script.blocking {
                return Err(signal.wait().to_string());
            }
            return Ok(script
                .result
                .unwrap_or_else(|| CellResult::text(format!("run {run}\n"))));
        }
        for chunk in &script.output {
            progress.output(chunk);
        }
        if let Some(gate) = &script.gate {
            let linked = Cancel::any(&[gate, signal]);
            linked.signal().wait();
            if let Some(reason) = signal.reason() {
                return Err(reason.to_string());
            }
        }
        if let Some(message) = script.fail {
            return Err(message);
        }
        Ok(script
            .result
            .unwrap_or_else(|| CellResult::text(format!("ran {}", call.code()))))
    }
}

/// Models, cells, registry and settings that survive a close/reopen, like a host process's own objects.
#[derive(Clone)]
pub struct Setup {
    pub faux: Faux,
    pub cells: Arc<ScriptCells>,
    pub registry: Registry,
    pub settings: Arc<Mutex<Settings>>,
    pub reports: Arc<Mutex<Vec<HarnessError>>>,
    pub now: Arc<Mutex<Option<u64>>>,
    pub hooks: Hooks,
    pub sections: Option<SectionSource>,
    /// Replaces the faux provider as the Harness's models.
    pub models: Option<Arc<dyn Models>>,
}

impl Setup {
    pub fn new() -> Setup {
        Setup::with(FauxOptions::default())
    }

    pub fn with(options: FauxOptions) -> Setup {
        Setup {
            faux: Faux::new(options),
            cells: Arc::default(),
            registry: Registry::new(),
            settings: Arc::default(),
            reports: Arc::default(),
            now: Arc::default(),
            hooks: Hooks::default(),
            sections: None,
            models: None,
        }
    }

    pub fn settings(&self, change: impl FnOnce(&mut Settings)) {
        change(&mut self.settings.lock().unwrap());
    }

    pub fn fix_clock(&self, now: u64) {
        *self.now.lock().unwrap() = Some(now);
    }

    pub fn reports(&self) -> Vec<HarnessError> {
        self.reports.lock().unwrap().clone()
    }
}

/// Open a ChatHarness over `storage` and return its root, configured with the faux model on first creation.
pub fn open_chat(storage: impl Storage + Send + 'static, setup: &Setup) -> (ChatHarness, Conversation) {
    let models: Arc<dyn Models> = match &setup.models {
        Some(models) => models.clone(),
        None => Arc::new(setup.faux.clone()),
    };
    let mut options = ChatOptions::new(setup.registry.clone(), models, setup.cells.clone());
    options.sections = setup.sections.clone();
    let settings = setup.settings.clone();
    options.settings = Some(Arc::new(move || settings.lock().unwrap().clone()));
    let reports = setup.reports.clone();
    options.on_report = Some(Arc::new(move |error| reports.lock().unwrap().push(error.clone())));
    let now = setup.now.clone();
    options.now = Some(Arc::new(move || {
        Ok(now.lock().unwrap().unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64
        }))
    }));
    options.hooks = setup.hooks.clone();
    let chat = ChatHarness::open(storage, options).unwrap();
    *setup.cells.ops.lock().unwrap() = Some(HostOps::new(chat.clone()));
    let root = chat.root(Some(&AgentChange::model(Faux::model_ref()))).unwrap();
    (chat, root)
}

/// Text of the first text content of a message.
pub fn text_of(message: &Message) -> Option<String> {
    match message {
        Message::System(_) => None,
        Message::User(user) => match &user.content {
            UserMessageContent::Text(text) => Some(text.clone()),
            UserMessageContent::Blocks(blocks) => blocks.iter().find_map(|block| match block {
                UserContent::Text(text) => Some(text.text.clone()),
                _ => None,
            }),
        },
        Message::Assistant(assistant) => assistant.content.iter().find_map(|block| match block {
            ai::AssistantContent::Text(text) => Some(text.text.clone()),
            _ => None,
        }),
        Message::ToolResult(result) => result.content.iter().find_map(|block| match block {
            UserContent::Text(text) => Some(text.text.clone()),
            _ => None,
        }),
    }
}

pub fn first_message(entry: &EntryRecord) -> Option<Message> {
    entry
        .model
        .as_ref()
        .and_then(|model| model.first())
        .and_then(|message| serde_json::from_value(message.clone()).ok())
}

/// Kind and text of each entry, skipping system entries (upstream `transcript()`).
pub fn transcript(entries: &[EntryRecord]) -> Vec<String> {
    entries
        .iter()
        .filter(|entry| entry.kind != "pi.system")
        .map(|entry| {
            let text = first_message(entry).and_then(|message| match message {
                Message::ToolResult(_) => None,
                message => text_of(&message),
            });
            match text {
                Some(text) => format!("{}:{text}", entry.kind),
                None => entry.kind.clone(),
            }
        })
        .collect()
}

/// The conversation's queued items as `[id, mode]`.
pub fn inbox(chat: &ChatHarness, conversation: &Conversation) -> Vec<(u64, String)> {
    chat.inbox(conversation.id)
        .unwrap()
        .into_iter()
        .map(|(id, mode)| (id.0, mode))
        .collect()
}

pub fn live(chat: &ChatHarness, conversation_id: ConversationId) -> Value {
    chat.live(conversation_id).unwrap()
}

/// Wait until the first slot of the round runs.
pub fn tool_running(chat: &ChatHarness, conversation: &Conversation) {
    eventually(|| live(chat, conversation.id)["tools"][0]["status"] == "running");
}

pub fn status(submission: &Submission) -> Value {
    js(&submission.status().unwrap())
}

pub fn record(record: &SubmissionRecord) -> Value {
    js(record)
}

/// Assistant entries of the conversation, oldest first.
pub fn assistants(conversation: &Conversation) -> Vec<EntryRecord> {
    conversation
        .entries()
        .unwrap()
        .into_iter()
        .filter(|entry| entry.kind == "pi.assistant")
        .collect()
}

/// A completed cell result asking for a control.
pub fn controlled(control: harness::chat::Control) -> CellResult {
    CellResult {
        output: Some(Vec::new()),
        control: Some(control),
        ..CellResult::default()
    }
}
