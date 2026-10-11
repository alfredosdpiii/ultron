//! The RPC session as `rpc-native.ts` drives it: this process's server generation, the session attached through a
//! [`Link`], every command an operation of the session worker (crates/session-core `Op`), and the run's progress
//! the transcript's lane-watch events translated to Pi's RPC events. New, switched, forked and cloned sessions are
//! created by the server and re-attached, as TypeScript clients do.

use super::events::{LaneEvent, RpcEventTranslator};
use super::{Input, Output, QueueKind, Reply, RpcSession};
use crate::Env;
use crate::args::Args;
use crate::link::Link;
use crate::print_mode::Target;
use serde_json::{Value, json};
use session_core::core::Op;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::Sender;

/// What reaches the command loop besides stdin.
pub enum Event {
    /// A lane-watch event of the attached session.
    Watch(Value),
    /// A `bash` command finished: its response, and the count of commands still running.
    BashDone {
        reply: Reply,
        result: Result<Option<Value>, String>,
        pending: Arc<AtomicUsize>,
    },
}

pub struct Remote<'a> {
    env: &'a Env,
    parsed: &'a Args,
    link: Option<Link>,
    tx: Sender<Input<Event>>,
    translator: RpcEventTranslator,
    running: bool,
    bash: Arc<AtomicUsize>,
}

/// The session id of a `switch_session` selector: an id, or a session file whose header names one.
fn selector_id(env: &Env, selector: &str) -> Result<String, String> {
    if !(selector.contains('/') || selector.contains('\\') || selector.ends_with(".jsonl")) {
        return Ok(selector.to_string());
    }
    let path = config::paths::normalize(&env.cwd.join(selector));
    let line = crate::session::read_first_line(&path)?;
    let invalid = || format!("Session file is not a valid ultron session: {selector}");
    let header: Value = serde_json::from_str(&line).map_err(|_| invalid())?;
    if header["type"] != "session" && header["kind"] != "header" {
        return Err(invalid());
    }
    header["id"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| format!("Session file has no ID: {selector}"))
}

impl<'a> Remote<'a> {
    pub fn open(parsed: &'a Args, env: &'a Env, target: Target, tx: Sender<Input<Event>>) -> Result<Self, String> {
        if matches!(target, Target::New { .. } | Target::Ephemeral) {
            crate::print_mode::resolve_model(parsed, env)?;
        }
        let watch = tx.clone();
        let link = Link::open(
            parsed,
            env,
            &target,
            "rpc",
            Arc::new(move |event| {
                let _ = watch.send(Input::Event(Event::Watch(event)));
            }),
        )?;
        Ok(Remote {
            env,
            parsed,
            link: Some(link),
            tx,
            translator: RpcEventTranslator::default(),
            running: false,
            bash: Arc::new(AtomicUsize::new(0)),
        })
    }

    fn link(&self) -> Result<&Link, String> {
        self.link.as_ref().ok_or_else(|| "The session is closed".to_string())
    }

    fn op(&self, op: Op) -> Result<Option<Value>, String> {
        self.link()?.op(&op)
    }

    /// An operation whose data is a value the command answers with as it is (`Null` on failure).
    fn read(&self, op: Op) -> Value {
        self.op(op).ok().flatten().unwrap_or(Value::Null)
    }

    /// Re-attach to session `id`.
    fn switch_to(&mut self, id: &str) -> Result<(), String> {
        let link = self.link.as_mut().ok_or("The session is closed")?;
        link.attach(id)?;
        self.running = false;
        Ok(())
    }

    /// Fork the current session (`forkPath`), record the fork in the new session, and attach to it.
    fn fork_to(&mut self, fork_path: Value) -> Result<(), String> {
        let source = self.link()?.session_id.clone();
        let created = self
            .link
            .as_mut()
            .ok_or("The session is closed")?
            .create(json!({"forkFromSessionId": source, "forkPath": fork_path}))?;
        if let Some(file) = self.link()?.session_file(&created)? {
            self.op(Op::Forked { target: file })?;
        }
        self.switch_to(&created)
    }
}

impl RpcSession for Remote<'_> {
    type Event = Event;

    fn deliver(&mut self, event: Event, out: &Output) {
        let event = match event {
            Event::Watch(event) => event,
            Event::BashDone { reply, result, pending } => {
                reply.send(result);
                pending.fetch_sub(1, Ordering::SeqCst);
                return;
            }
        };
        match event["type"].as_str() {
            Some("run_start") => self.running = true,
            Some("run_end") => self.running = false,
            _ => {}
        }
        if let Some(lane_event) = LaneEvent::from_watch(&event) {
            for line in self.translator.translate(lane_event) {
                out.emit(&line);
            }
        }
    }

    fn busy(&self) -> bool {
        self.running || self.bash.load(Ordering::SeqCst) > 0
    }

    fn input_ended(&mut self) {
        self.abort();
        self.abort_bash();
    }

    fn close(&mut self) {
        if let Some(link) = self.link.take() {
            link.close(self.parsed.no_session);
        }
    }

    fn is_running(&self) -> bool {
        self.running
    }

    fn prompt(&mut self, message: String, images: Vec<Value>, _out: &Output) -> Result<(), String> {
        self.op(Op::Prompt {
            message,
            images,
            queue: None,
        })?;
        // The run starts now; its `run_start` follows on the transcript.
        self.running = true;
        Ok(())
    }

    fn enqueue(&mut self, kind: QueueKind, message: String, images: Vec<Value>, _out: &Output) -> Result<(), String> {
        self.op(Op::Enqueue {
            kind,
            message,
            images,
            entry_id: None,
        })
        .map(drop)
    }

    fn abort(&mut self) {
        let _ = self.op(Op::Abort);
    }

    fn clear_queue(&mut self, _out: &Output) -> Value {
        self.read(Op::ClearQueue)
    }

    fn new_session(&mut self) -> Result<(), String> {
        if self.running {
            return Err("Agent is already processing".into());
        }
        let created = self.link.as_mut().ok_or("The session is closed")?.create(json!({}))?;
        self.switch_to(&created)
    }

    fn state(&self) -> Value {
        self.read(Op::State)
    }

    fn available_models(&self) -> Vec<Value> {
        self.read(Op::AvailableModels).as_array().cloned().unwrap_or_default()
    }

    fn set_model(&mut self, provider: &str, model_id: &str) -> Result<Value, String> {
        self.op(Op::SetModel {
            provider: provider.into(),
            model_id: model_id.into(),
        })
        .map(Option::unwrap_or_default)
    }

    fn cycle_model(&mut self) -> Result<Option<Value>, String> {
        self.op(Op::CycleModel).map(|v| v.filter(|v| !v.is_null()))
    }

    fn thinking_levels(&self) -> Vec<String> {
        self.read(Op::ThinkingLevels)
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|l| l.as_str().map(str::to_string))
            .collect()
    }

    fn set_thinking_level(&mut self, level: &str, _out: &Output) -> Result<(), String> {
        self.op(Op::SetThinking { level: level.into() }).map(drop)
    }

    fn cycle_thinking_level(&mut self, _out: &Output) -> Result<Option<String>, String> {
        self.op(Op::CycleThinking)
            .map(|v| v.and_then(|v| v.as_str().map(str::to_string)))
    }

    fn set_queue_mode(&mut self, kind: QueueKind, mode: &str) -> Result<(), String> {
        self.op(Op::SetQueueMode {
            kind,
            mode: mode.into(),
        })
        .map(drop)
    }

    fn compact(&mut self, custom_instructions: Option<&str>, _out: &Output) -> Result<Value, String> {
        self.op(Op::Compact {
            instructions: custom_instructions.map(str::to_string),
        })
        .map(Option::unwrap_or_default)
    }

    fn set_auto_compaction(&mut self, enabled: bool) {
        let _ = self.op(Op::SetAutoCompaction(enabled));
    }

    fn set_auto_retry(&mut self, enabled: bool) {
        let _ = self.op(Op::SetAutoRetry(enabled));
    }

    fn abort_retry(&mut self) {
        let _ = self.op(Op::AbortRetry);
    }

    fn bash(&mut self, command: String, exclude_from_context: bool, reply: Reply) -> Result<(), String> {
        let link = self.link()?;
        let session = link.connection.session_target().map_err(|e| e.to_string())?;
        let request = link
            .connection
            .start(
                session,
                "ultron.session-control",
                "bash",
                json!([command, exclude_from_context]),
            )
            .map_err(|e| e.to_string())?;
        let connection = link.connection.clone();
        let (pending, tx) = (self.bash.clone(), self.tx.clone());
        pending.fetch_add(1, Ordering::SeqCst);
        std::thread::spawn(move || {
            let result = connection.wait(&request).map_err(|e| e.to_string());
            // The command loop answers it: that thread holds the process's stdout.
            let _ = tx.send(Input::Event(Event::BashDone { reply, result, pending }));
        });
        Ok(())
    }

    fn abort_bash(&mut self) {
        let _ = self.op(Op::AbortBash);
    }

    fn session_stats(&self) -> Value {
        self.read(Op::Stats)
    }

    fn switch_session(&mut self, selector: &str) -> Result<(), String> {
        if self.running {
            return Err("Agent is already processing".into());
        }
        let id = selector_id(self.env, selector)?;
        if id == self.link()?.session_id {
            return Ok(());
        }
        if self.link()?.session_file(&id)?.is_none() {
            return Err(format!("Session not found: {id}"));
        }
        self.switch_to(&id)
    }

    fn fork(&mut self, entry_id: &str) -> Result<String, String> {
        let text = self
            .op(Op::ForkSource {
                entry_id: entry_id.into(),
            })?
            .and_then(|t| t.as_str().map(str::to_string))
            .unwrap_or_default();
        // As in Pi, the new session holds only the path from the root to just before that message.
        self.fork_to(json!({"entryId": entry_id, "position": "before"}))?;
        Ok(text)
    }

    fn clone_session(&mut self) -> Result<(), String> {
        self.op(Op::CloneSource)?;
        self.fork_to(json!({"position": "at"}))
    }

    fn fork_messages(&self) -> Vec<Value> {
        self.read(Op::ForkMessages).as_array().cloned().unwrap_or_default()
    }

    fn entries(&self, since: Option<&str>) -> Result<(Vec<Value>, Value), String> {
        let data = self
            .op(Op::Entries {
                since: since.map(str::to_string),
            })?
            .unwrap_or_default();
        Ok((
            data["entries"].as_array().cloned().unwrap_or_default(),
            data["leafId"].clone(),
        ))
    }

    fn tree(&self) -> (Value, Value) {
        let data = self.read(Op::Tree);
        (data["tree"].clone(), data["leafId"].clone())
    }

    fn last_assistant_text(&self) -> Option<String> {
        self.read(Op::LastAssistantText).as_str().map(str::to_string)
    }

    fn set_session_name(&mut self, name: &str) -> Result<(), String> {
        self.op(Op::SetName { name: name.into() }).map(drop)
    }

    fn messages(&self) -> Vec<Value> {
        self.read(Op::Messages).as_array().cloned().unwrap_or_default()
    }

    fn commands(&self) -> Result<Vec<Value>, String> {
        Err("get_commands is not implemented in the Rust build yet".into())
    }
}

/// Run RPC mode over `input`/`output` until the input ends; returns the exit code.
pub fn run(
    parsed: &Args,
    env: &Env,
    target: Target,
    input: Box<dyn std::io::Read + Send>,
    output: Box<dyn std::io::Write + Send>,
) -> Result<i32, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut session = Remote::open(parsed, env, target, tx.clone())?;
    let reader = std::thread::spawn(move || {
        for line in super::jsonl::JsonlLines::new(input) {
            match line {
                Ok(line) => {
                    if tx.send(Input::Line(line)).is_err() {
                        return;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = tx.send(Input::Eof);
    });
    let out = Output::new(output);
    super::serve(&mut session, &rx, &out);
    drop(rx);
    let _ = reader;
    Ok(0)
}
