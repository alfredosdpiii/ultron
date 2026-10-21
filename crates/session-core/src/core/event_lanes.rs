//! The session's lanes as the completion-event dispatcher sees them (crate::async_events): the root lane is the
//! session actor's (its runs, its queue, its transcript), asked through the actor's input; another lane is a child
//! conversation of the durable runtime (a subagent's), steered through its inbox while it runs.
//!
//! The dispatcher's events come from the host's shell jobs (`job_done`), the typed-agent host (`task_done`), the
//! subagent host (`child_done`) and detached cells (`cell_done`); [`install`] wires them, the root-turn abort that
//! cancels a root's jobs, and the budget check that keeps a spent root from being re-invoked.

use super::*;
use crate::async_events::{
    Accepted, Cancelled, Dispatcher, DispatcherOptions, EventLanes, Execution, RuntimeEvent, async_events_enabled,
    max_event_runs_from_env,
};
use crate::subagents::DurableChildren;
use harness::chat::{ChatHarness, SubmissionDraft, WhenBusy};
use std::sync::mpsc::channel;

/// What the dispatcher asks of the root lane (answered by the actor, in order with its other work).
pub enum LaneCall {
    Execution,
    Accept { message: Value, root: String },
    Steer(Value),
    NextRun(Value),
    CancelQueued(String),
}

pub enum LaneAnswer {
    Execution(Execution),
    Accepted(Accepted),
    Queued(Result<String, String>),
    Cancelled(Result<Cancelled, String>),
}

struct SessionLanes {
    actor: Mutex<Sender<Input>>,
    chat: ChatHarness,
    children: Arc<DurableChildren>,
    /// Messages kept for a child lane's next run (child lanes are never re-invoked).
    waiting: Mutex<Vec<(String, Value)>>,
}

impl SessionLanes {
    fn ask(&self, call: LaneCall) -> Option<LaneAnswer> {
        let (reply, answer) = channel();
        lock(&self.actor).send(Input::EventLane(call, reply)).ok()?;
        // The actor drops the reply when it closes: the session is going away.
        answer.recv().ok()
    }
}

/// The text of a custom event message (its content string).
fn content(message: &Value) -> String {
    message["content"].as_str().unwrap_or_default().to_string()
}

impl EventLanes for SessionLanes {
    fn execution(&self, lane: &str) -> Execution {
        if lane == LANE {
            return match self.ask(LaneCall::Execution) {
                Some(LaneAnswer::Execution(execution)) => execution,
                _ => Execution::Other,
            };
        }
        let running = self
            .children
            .conversation_of_lane(lane)
            .and_then(|conversation| self.chat.live(conversation).ok())
            .is_some_and(|live| !live["run"].is_null());
        if running { Execution::Run } else { Execution::Idle }
    }

    fn accept(&self, lane: &str, message: &Value, root: &str) -> Accepted {
        if lane != LANE {
            return Accepted::Refused("child lanes are never re-invoked".into());
        }
        match self.ask(LaneCall::Accept {
            message: message.clone(),
            root: root.to_string(),
        }) {
            Some(LaneAnswer::Accepted(accepted)) => accepted,
            _ => Accepted::Refused("the session is closing".into()),
        }
    }

    fn steer(&self, lane: &str, message: &Value) -> Result<String, String> {
        if lane == LANE {
            return match self.ask(LaneCall::Steer(message.clone())) {
                Some(LaneAnswer::Queued(queued)) => queued,
                _ => Err("the session is closing".into()),
            };
        }
        let conversation = self
            .children
            .conversation_of_lane(lane)
            .ok_or_else(|| format!("No conversation runs lane {lane}"))?;
        let handle = self
            .chat
            .conversation(conversation)
            .map_err(|e| e.to_string())?
            .ok_or("The lane's conversation is gone")?;
        let submission = handle
            .submit(SubmissionDraft::Input {
                content: json!(content(message)),
                when_busy: WhenBusy::Steer,
                request_id: None,
            })
            .map_err(|e| e.to_string())?;
        Ok(format!("{lane}#{}", submission.id.0))
    }

    fn next_run(&self, lane: &str, message: &Value) -> Result<String, String> {
        if lane == LANE {
            return match self.ask(LaneCall::NextRun(message.clone())) {
                Some(LaneAnswer::Queued(queued)) => queued,
                _ => Err("the session is closing".into()),
            };
        }
        let mut waiting = lock(&self.waiting);
        waiting.push((lane.to_string(), message.clone()));
        Ok(format!("{lane}#waiting-{}", waiting.len()))
    }

    fn cancel_queued(&self, lane: &str, entry: &str) -> Result<Cancelled, String> {
        if lane == LANE {
            return match self.ask(LaneCall::CancelQueued(entry.to_string())) {
                Some(LaneAnswer::Cancelled(cancelled)) => cancelled,
                _ => Err("the session is closing".into()),
            };
        }
        // A child conversation's run places its steers itself: what it did not take stays its own.
        Ok(Cancelled::AlreadyConsumed)
    }
}

/// Ask the actor to report its activity every 250 ms until the session closes.
pub(super) fn tick_activity(actor: Sender<Input>, stop: harness::scheduler::Cancel) {
    std::thread::spawn(move || {
        while !stop.is_cancelled() {
            std::thread::sleep(std::time::Duration::from_millis(250));
            if actor.send(Input::ActivityTick).is_err() {
                return;
            }
        }
    });
}

/// The dispatcher of a session, wired to its lanes, its host's shell jobs, its usage ledger and its task hosts.
/// `stop` ends the thread that forwards the task hosts' completions.
pub(super) fn install(
    lane: &Lane,
    env: &RunEnv,
    actor: Sender<Input>,
    chat: ChatHarness,
    children: Arc<DurableChildren>,
    stop: harness::scheduler::Cancel,
) -> Arc<Dispatcher> {
    let stack = lane.stack.clone();
    let agents = stack.agents.clone();
    let mut options = DispatcherOptions::new(Arc::new(move |run: &str| {
        agents
            .usage_root_for_lane(LANE, run)
            .unwrap_or_else(|| format!("turn:{run}"))
    }));
    options.enabled = stack.events.enabled() && async_events_enabled((env.var)("ULTRON_ASYNC_EVENTS").as_deref());
    options.max_runs = max_event_runs_from_env((env.var)("ULTRON_ASYNC_EVENTS_MAX_RUNS").as_deref());
    // With events on, the dispatcher re-invokes the idle root when its work ends: the root lane is woken
    // (crate::wake), so its rlm.collect never waits; without them it waits in its cell like every other lane.
    stack.wake.set(options.enabled);
    // A completion re-invokes the root only within the budget of the request that started the work.
    let usage = stack.usage.clone();
    options.refuse = Some(Arc::new(move |root: &str| usage.reinvoke_refusal(root).ok().flatten()));
    // Esc on a root turn stops the shell jobs it started.
    let host = stack.host.clone();
    let tools = stack.tool_calls().clone();
    options.on_root_aborted = Some(Arc::new(move |root: &str| {
        host.cancel_root(root);
        tools.cancel_root(root);
    }));
    // Event summaries are masked with the session's secrets (`maskCellOutput`).
    options.mask = Some(stack.mask());
    // A backgrounded cell's end carries the context reminder its result would have carried.
    let reminders = stack.context_reminders.clone();
    options.note = Some(Arc::new(move |lane: &str, text: &str| {
        reminders.reminder(lane, text, 0)
    }));
    let lanes = Arc::new(SessionLanes {
        actor: Mutex::new(actor),
        chat,
        children: children.clone(),
        waiting: Mutex::new(Vec::new()),
    });
    let dispatcher = Dispatcher::new(options, lanes);
    // A job belongs to the lane that started it and to that lane's root turn.
    let (roots_children, roots_agents) = (children.clone(), stack.agents.clone());
    stack.host.set_roots(Arc::new(move |lane: &str| {
        if lane == LANE {
            roots_children.current_root()
        } else {
            roots_children.usage_root_of_lane(lane, &roots_agents)
        }
    }));
    let jobs = Arc::downgrade(&dispatcher);
    stack.host.on_job_end(Arc::new(move |end: host::JobEnd| {
        let (Some(dispatcher), Some(event)) = (jobs.upgrade(), end.event()) else {
            return;
        };
        dispatcher.publish(RuntimeEvent {
            kind: event["kind"].as_str().unwrap_or("job_done").into(),
            id: end.record.id.clone(),
            status: end.record.status.clone(),
            summary: event["summary"].as_str().unwrap_or_default().into(),
            fetch: event["fetch"].as_str().unwrap_or_default().into(),
            lane: end.record.lane.clone(),
            root_id: end.record.root_id.clone(),
        });
    }));
    // A detached REPL tool call's end is announced like a job's (`toolEvent`), under the root that started it.
    let (tool_roots_children, tool_roots_agents) = (children.clone(), stack.agents.clone());
    stack.tool_calls().set_roots(Arc::new(move |lane: &str| {
        if lane == LANE {
            tool_roots_children.current_root()
        } else {
            tool_roots_children.usage_root_of_lane(lane, &tool_roots_agents)
        }
    }));
    let tool_ends = Arc::downgrade(&dispatcher);
    stack
        .tool_calls()
        .on_end(Arc::new(move |end: kernel_apis::ToolCallEnd| {
            let (Some(dispatcher), Some((id, status, summary, fetch))) =
                (tool_ends.upgrade(), kernel_apis::tool_calls::tool_event(&end))
            else {
                return;
            };
            dispatcher.publish(RuntimeEvent {
                kind: "tool_done".into(),
                id,
                status,
                summary,
                fetch,
                lane: end.call["lane"].as_str().unwrap_or(LANE).into(),
                root_id: end.call["rootId"].as_str().map(str::to_string),
            });
        }));
    // Subagents', typed tasks' and detached cells' ends, forwarded until the session closes.
    let (forward, events, agents) = (Arc::downgrade(&dispatcher), stack.events.clone(), stack.agents.clone());
    std::thread::spawn(move || {
        while !stop.is_cancelled() {
            let Some(dispatcher) = forward.upgrade() else { return };
            forward_ends(&events, &children, &agents, &dispatcher);
            drop(dispatcher);
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    });
    dispatcher
}

/// Publish the task hosts' completions that have arrived, each with the usage root of the work.
fn forward_ends(
    events: &crate::compose::RuntimeEvents,
    children: &DurableChildren,
    agents: &delegation::agents::Agents,
    dispatcher: &Dispatcher,
) {
    for mut event in events.drain_ends() {
        if event.root_id.is_none() {
            event.root_id = if event.kind == "child_done" {
                children.usage_root_of_lane(&format!("{}{}", crate::subagents::LANE_PREFIX, event.id), agents)
            } else {
                None
            }
            .or_else(|| children.current_root());
        }
        dispatcher.publish(event);
    }
}

impl Core {
    /// The dispatcher's question about the root lane.
    pub(super) fn event_lane(&mut self, call: LaneCall, reply: Sender<LaneAnswer>) {
        let answer = match call {
            LaneCall::Execution => LaneAnswer::Execution(if self.run.is_some() {
                Execution::Run
            } else {
                Execution::Idle
            }),
            LaneCall::Accept { message, root } => LaneAnswer::Accepted(self.accept_event(message, &root)),
            LaneCall::Steer(message) => LaneAnswer::Queued(if self.run.is_none() {
                Err("no run is in progress".into())
            } else {
                self.enqueue(QueueKind::Steer, content(&message), Vec::new(), None)
            }),
            LaneCall::NextRun(message) => LaneAnswer::Queued(self.keep_for_next_run(message)),
            LaneCall::CancelQueued(entry) => LaneAnswer::Cancelled({
                let queued = lock(&self.queues).items.iter().any(|q| q.entry_id == entry);
                if queued {
                    self.remove_queued(&[entry]).map(|_| Cancelled::Cancelled)
                } else {
                    // Placed by the run (or withdrawn with it): the run had it.
                    Ok(Cancelled::AlreadyConsumed)
                }
            }),
        };
        let _ = reply.send(answer);
    }

    /// Re-invoke the idle root with the event message, the run charged to `root` (`continueRootTurn`).
    fn accept_event(&mut self, message: Value, root: &str) -> Accepted {
        if self.run.is_some() {
            return Accepted::Busy;
        }
        if let Err((error, _)) = self.prompt_with(message, None, true) {
            return Accepted::Refused(error);
        }
        let Some(operation) = self.operation_id() else {
            return Accepted::Refused("the run did not start".into());
        };
        let agents = &self.lane.stack.agents;
        agents.continue_root_turn(&operation, root);
        agents.begin_root_turn(&operation);
        self.durable.children.set_root(Some(root.to_string()));
        Accepted::Run(operation)
    }

    /// An event the root may not run now waits in the transcript, seen with the root's next run.
    fn keep_for_next_run(&mut self, message: Value) -> Result<String, String> {
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
        self.append(message)?;
        Ok(self.tip().unwrap_or_default())
    }

    /// Whether detached work of the session is active: a run, a compaction, a running shell job or subagent, an
    /// event waiting for delivery (TypeScript's `holdActivity`). Reported as an `activity` event (the worker does not retire while it
    /// is active), on every tick and before a run's end.
    pub(super) fn report_activity(&mut self) {
        // Running work is read before its ends are forwarded: work that ends in between has queued its end (a
        // subagent queues it in the step that ends it), which the forward then publishes. Forwarding first, a
        // subagent ending between the forward and the read was neither running nor pending, and an unattached
        // worker retired without its child_done event.
        let running = self.run.is_some()
            || self.compacting.is_some()
            || self.lane.stack.host.running_jobs() > 0
            || self.lane.stack.tool_calls().running(LANE, &|_| false) > 0
            || !self.lane.stack.delegation.running(LANE).is_empty()
            || self.lane.stack.agents.running_announced(LANE) > 0
            || self.lane.stack.lanes.detached_running(LANE) > 0;
        let stack = self.lane.stack.clone();
        forward_ends(&stack.events, &self.durable.children, &stack.agents, &self.events);
        let active = running || self.events.pending_for(LANE) > 0;
        self.emit(json!({"type": "activity", "lane": LANE, "active": active}));
    }

    /// Root-owned work whose completion may still re-invoke the root (`async.pending`).
    pub(super) fn async_pending(&mut self) -> Value {
        // A completion that arrived but is not forwarded yet still has to re-invoke the root.
        let stack = self.lane.stack.clone();
        forward_ends(&stack.events, &self.durable.children, &stack.agents, &self.events);
        let events = &self.events;
        if !events.enabled() {
            return json!({"pending": false, "jobs": 0, "tools": 0, "tasks": 0, "events": 0, "running": 0});
        }
        let aborted = |root: Option<&str>| events.root_aborted(root);
        let jobs = self.lane.stack.host.running(LANE, &aborted);
        let tools = self.lane.stack.tool_calls().running(LANE, &aborted);
        // Subagents, typed tasks whose end is announced to the root (`task_done`), and detached root cells
        // (`cell_done`): each end re-invokes the root, so a print run waits for them.
        let tasks = self.lane.stack.delegation.running(LANE).len()
            + self.lane.stack.agents.running_announced(LANE)
            + self.lane.stack.lanes.detached_running(LANE);
        let pending = events.pending_for(LANE);
        let running = usize::from(self.run.is_some());
        json!({
            "pending": jobs + tools + tasks + pending + running > 0,
            "jobs": jobs,
            "tools": tools,
            "tasks": tasks,
            "events": pending,
            "running": running,
        })
    }
}
