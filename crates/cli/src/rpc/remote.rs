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
    /// A signal ended RPC mode: release the session (a busy worker keeps its work, detached; nothing is aborted,
    /// as leaving RPC mode is not an abort) and exit with this code.
    Signal(i32),
}

pub struct Remote<'a> {
    env: &'a Env,
    parsed: &'a Args,
    link: Option<Link>,
    tx: Sender<Input<Event>>,
    translator: RpcEventTranslator,
    running: bool,
    /// The run in progress (its `run_start` id).
    run_id: Option<String>,
    /// The run that was in progress when the link lost its worker or server.
    lost_run: Option<String>,
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
            run_id: None,
            lost_run: None,
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
        let source = self.link()?.session_id();
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
            Event::Signal(code) => {
                if let Some(link) = self.link.take() {
                    link.close(false);
                }
                std::process::exit(code);
            }
        };
        // A run its worker lost ends for RPC clients as failed when the link rejoins (recovery settled it), unless
        // its end, or another run, was seen meanwhile.
        if event["type"] == "connection" && event["state"] == "reconnecting" {
            self.lost_run = self.run_id.clone().filter(|_| self.running);
        }
        let lost = event["type"] == "connection"
            && event["state"] == "rejoined"
            && self
                .lost_run
                .take()
                .is_some_and(|lost| self.running && self.run_id.as_deref() == Some(&lost));
        let event = if lost {
            json!({"type": "run_end", "status": "failed", "endedAt": session_core::lane::now_ms(),
                "error": {"code": "interrupted", "message": "The session worker stopped during this run"}})
        } else {
            event
        };
        match event["type"].as_str() {
            Some("run_start") => {
                self.running = true;
                self.run_id = event["runId"].as_str().map(str::to_string);
            }
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
        // Leaving is not an abort (TS: a root turn in flight when its client closes stdin runs to completion): the
        // session is released at once and a busy worker keeps its work running detached.
        if daemon::signals::received().is_some() {
            return;
        }
        self.close();
        self.running = false;
        self.bash.store(0, Ordering::SeqCst);
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
        let connection = self.link()?.connection();
        let session = connection.session_target().map_err(|e| e.to_string())?;
        let request = connection
            .start(
                session,
                "ultron.session-control",
                "bash",
                json!([command, exclude_from_context]),
            )
            .map_err(|e| e.to_string())?;
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
        if id == self.link()?.session_id() {
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
    // A signal ends RPC mode as the end of its input does (the session detaches, the server closes), then exits
    // with the signal's code.
    let signalled = Arc::new(std::sync::atomic::AtomicI32::new(0));
    let (code, end) = (signalled.clone(), std::sync::Mutex::new(tx.clone()));
    daemon::signals::set_handler(Arc::new(move |signal| {
        let exit = daemon::signals::exit_code(signal);
        code.store(exit, Ordering::SeqCst);
        // Leaving RPC on a signal releases the session without aborting the running turn (as in TS): a busy worker
        // keeps its work running detached.
        let _ = end
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .send(Input::Event(Event::Signal(exit)));
        std::thread::sleep(crate::print_mode::PROMPT_ABORT_WAIT);
        std::process::exit(daemon::signals::exit_code(signal));
    }));
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
    let code = signalled.load(Ordering::SeqCst);
    Ok(if code != 0 {
        code
    } else {
        daemon::signals::received().map_or(0, daemon::signals::exit_code)
    })
}
