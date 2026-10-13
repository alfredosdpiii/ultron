//! The session actor's side of durable turns (crate::turns): starting a turn in the durable runtime, answering a
//! generation's start with the request the TypeScript agent loop would send, noticing the end of a turn, aborting
//! one, and resuming one a dead worker left (projecting onto the transcript what the runtime committed and the
//! transcript had not, then letting the runtime continue).

use super::*;
use crate::durable_store::JournalStorage;
use crate::turns::{Answer, Bridge, GenerationStart, Request, TurnCells, TurnModels};
use agent::AgentToolResult;
use durable::{ROOT_CONVERSATION_ID, SubmissionId, SubmissionStatus};
use harness::chat::{
    AgentChange, ChatHarness, ChatOptions, CompactionPolicy, ModelRef, QueueMode, RetryPolicy, Settings,
    SubmissionAbort, SubmissionDraft, ToolExecution, WhenBusy,
};
use harness::scheduler::{Cancel, Ctx, Registry};
use session_journal::run::{OP_META, OP_STATE};
use std::sync::atomic::{AtomicBool, Ordering};

/// A round whose answer the transcript holds: its calls `(id, name)`, and the results reported before a restart.
type HeldRound = (Vec<(String, String)>, Vec<(String, AgentToolResult, bool)>);

/// Generation partials are not committed: the transcript has the progress frames, and a resent request replaces
/// whatever a dead worker was streaming.
const NO_PARTIALS_MS: u64 = 365 * 24 * 3600 * 1000;

/// The durable runtime of one session and what the actor keeps of it.
pub(super) struct Durable {
    pub chat: ChatHarness,
    pub root: harness::chat::Conversation,
    pub bridge: Arc<Bridge>,
    pub auto_retry: Arc<AtomicBool>,
    /// Cancelled when the session closes: idle waiters stop.
    pub stop: Cancel,
}

fn harness_error(error: harness::scheduler::HarnessError) -> String {
    error.to_string()
}

/// Open the durable runtime over the session journal; its turns report to the actor through `tx`.
#[allow(clippy::too_many_arguments)]
pub(super) fn open_durable(
    session: &SharedSession,
    tx: &Sender<Input>,
    lane: &Lane,
    provider: Arc<Provider>,
    env: &RunEnv,
    flags: &ModelFlags,
    queues: Arc<Mutex<Queues>>,
    children: Arc<crate::subagents::DurableChildren>,
) -> Result<Durable, String> {
    let to_actor = tx.clone();
    let to_actor = Mutex::new(to_actor);
    let bridge = Bridge::new(Arc::new(move |request| {
        let _ = lock(&to_actor).send(Input::Turn(request));
    }));
    let storage = JournalStorage::open(session.clone(), Some(crate::turns::observer(bridge.clone())))
        .map_err(|e| e.message().to_string())?;
    let auto_retry = Arc::new(AtomicBool::new(true));
    let retry = auto_retry.clone();
    let settings_queues = queues;
    let settings: harness::chat::SettingsSource = Arc::new(move || {
        let (steering, follow_up) = {
            let queues = lock(&settings_queues);
            (queues.steering_all, queues.follow_up_all)
        };
        let mode = |all: bool| if all { QueueMode::All } else { QueueMode::OneAtATime };
        Settings {
            retry: RetryPolicy {
                enabled: retry.load(Ordering::SeqCst),
                ..RetryPolicy::default()
            },
            compaction: CompactionPolicy {
                enabled: false,
                ..CompactionPolicy::default()
            },
            partial_interval_ms: NO_PARTIALS_MS,
            tool_execution: ToolExecution::Parallel,
            steering_mode: mode(steering),
            follow_up_mode: mode(follow_up),
            ..Settings::default()
        }
    });
    let catalog_env = env.clone();
    let (key_env, key_flags) = (env.clone(), flags.clone());
    let root_session = Arc::new(std::sync::OnceLock::new());
    let models = TurnModels {
        bridge: bridge.clone(),
        provider: provider as Arc<dyn agent::StreamFn>,
        resolve: Arc::new(move |provider, id| lane::catalog_model(provider, id, &catalog_env).ok()),
        api_key: Arc::new(move |provider| lane::api_key(&key_flags, &key_env, provider)),
        root_session: root_session.clone(),
    };
    let cells = TurnCells {
        bridge: bridge.clone(),
        tool: lane.tool.clone(),
        progress: lane.stack.cell_progress.clone(),
        stack: lane.stack.clone(),
        children: children.clone(),
    };
    // A child conversation's system prompt is its subagent's (the root's request is built by the actor).
    let (section_children, root_system) = (children, lane.stack.system().to_string());
    let sections: harness::chat::SectionSource = Arc::new(move |conversation, _| {
        let mut sections = indexmap::IndexMap::new();
        if conversation != ROOT_CONVERSATION_ID {
            let system = section_children
                .lane_of(conversation)
                .and_then(|lane| section_children.system_of(&lane))
                .unwrap_or_else(|| root_system.clone());
            sections.insert("system".to_string(), system);
        }
        Ok(sections)
    });
    let mut options = ChatOptions::new(Registry::new(), Arc::new(models), Arc::new(cells));
    options.settings = Some(settings);
    options.sections = Some(sections);
    let chat = ChatHarness::open(storage, options).map_err(harness_error)?;
    let root = chat.root(None).map_err(harness_error)?;
    let provider_session = chat
        .snapshot(
            &harness::chat::docs::PROVIDER,
            durable::Owner::Conversation(ROOT_CONVERSATION_ID),
        )
        .map_err(harness_error)?
        .and_then(|state| state["sessionId"].as_str().map(str::to_string))
        .unwrap_or_default();
    let _ = root_session.set(provider_session);
    Ok(Durable {
        chat,
        root,
        bridge,
        auto_retry,
        stop: Cancel::new(),
    })
}

/// The text of a transcript message, for matching a placed user message to its queued item.
fn text_of(message: &Message) -> String {
    message_text(&serde_json::to_value(message).unwrap_or(Value::Null))
}

impl Core {
    /// The operation of the run in progress.
    pub(super) fn operation_id(&self) -> Option<String> {
        self.run
            .as_ref()
            .and_then(|run| run.journal.run())
            .map(|run| run.operation_id().to_string())
    }

    /// Charge a root response to its usage root in the ledger (after the operation journal counted it).
    pub(super) fn charge_turn(&self, message: &Value) {
        let Some(operation) = self.operation_id() else { return };
        let root = self.lane.stack.agents.usage_root_for_lane(LANE, &operation);
        let usage = &message["usage"];
        let _ = self.lane.stack.usage.record_turn(
            root.as_deref(),
            usage["totalTokens"].as_f64(),
            usage["cost"]["total"].as_f64(),
        );
    }

    /// The submission a queued message is waiting as (request ID `<entry>#<attempt>`): the newest one, and the
    /// attempt a new submission would use.
    fn queued_submission(&self, entry_id: &str) -> (Option<durable::SubmissionRecord>, u64) {
        let mut found = None;
        let mut attempt = 0;
        loop {
            let request = format!("{entry_id}#{attempt}");
            let record = self
                .durable
                .chat
                .commit(|tx| Ok(tx.submission_by_request(ROOT_CONVERSATION_ID, &request)?))
                .ok()
                .flatten();
            match record {
                Some(record) => {
                    found = Some(record);
                    attempt += 1;
                }
                None => return (found, attempt),
            }
        }
    }

    /// Queue every queued message that is not waiting in the runtime's inbox yet (a run is in progress).
    pub(super) fn submit_queued(&mut self) -> Result<(), String> {
        let pending: Vec<Queued> = lock(&self.queues)
            .items
            .iter()
            .filter(|q| q.submission.is_none())
            .cloned()
            .collect();
        for queued in pending {
            let (existing, attempt) = self.queued_submission(&queued.entry_id);
            let id = match existing {
                Some(record) if matches!(record.status, SubmissionStatus::Queued | SubmissionStatus::Placed) => {
                    record.id
                }
                _ => {
                    let when = match queued.kind {
                        QueueKind::Steer => WhenBusy::Steer,
                        QueueKind::FollowUp => WhenBusy::FollowUp,
                    };
                    let draft = SubmissionDraft::Input {
                        content: queued.message["content"].clone(),
                        when_busy: when,
                        request_id: Some(format!("{}#{attempt}", queued.entry_id)),
                    };
                    self.durable.root.submit(draft).map_err(harness_error)?.id
                }
            };
            if let Some(item) = lock(&self.queues)
                .items
                .iter_mut()
                .find(|q| q.entry_id == queued.entry_id)
            {
                item.submission = Some(id);
            }
        }
        Ok(())
    }

    /// Take queued messages back out of the runtime's inbox (an aborted run: they wait for the user again).
    pub(super) fn withdraw_queued(&mut self, ids: &[String]) {
        let withdrawn: Vec<SubmissionId> = {
            let mut queues = lock(&self.queues);
            queues
                .items
                .iter_mut()
                .filter(|q| ids.is_empty() || ids.contains(&q.entry_id))
                .filter_map(|q| q.submission.take())
                .collect()
        };
        for id in withdrawn {
            if let Ok(SubmissionAbort::NotFound) = self.durable.chat.abort_submission(id, Some(ROOT_CONVERSATION_ID)) {
                continue;
            }
        }
    }

    /// Start the durable turn of the admitted `prompt` (its transcript entry `prompt_entry`): the conversation's
    /// model, a fresh context, the prompt as input, then the queued messages behind it.
    pub(super) fn start_turn(&mut self, prompt: &Value, prompt_entry: &str) -> Result<SubmissionId, String> {
        self.durable.bridge.clear();
        self.durable.bridge.set_user_abort(false);
        let change = AgentChange {
            model: Some(Some(ModelRef::new(&self.model.provider, &self.model.id))),
            thinking_level: Some(Some(self.thinking.clone())),
        };
        self.durable.root.configure(&change).map_err(harness_error)?;
        self.durable.root.reset(None).map_err(harness_error)?;
        let draft = SubmissionDraft::Input {
            content: prompt["content"].clone(),
            when_busy: WhenBusy::FollowUp,
            request_id: Some(prompt_entry.to_string()),
        };
        let submission = self.durable.root.submit(draft).map_err(harness_error)?;
        self.submit_queued()?;
        Ok(submission.id)
    }

    /// Notice when the runtime has nothing left to do for the turn.
    pub(super) fn spawn_waiter(&self) {
        let (root, stop, tx) = (self.durable.root.clone(), self.durable.stop.clone(), self.tx.clone());
        std::thread::spawn(move || {
            if root.wait_for_idle(&Ctx::with_signal(&stop)).is_ok() {
                let _ = tx.send(Input::HarnessIdle);
            }
        });
    }

    /// The runtime is idle: unless it has work again, wait (in the host) for what the turn started that still
    /// runs (subagents, detached cells), whose ends continue the turn; else the turn ends.
    pub(super) fn harness_idle(&mut self) {
        let Some(run) = self.run.as_ref() else { return };
        let live = self.durable.chat.live(ROOT_CONVERSATION_ID).unwrap_or(Value::Null);
        let queued = self
            .durable
            .chat
            .inbox(ROOT_CONVERSATION_ID)
            .map(|items| !items.is_empty())
            .unwrap_or(false);
        if !live["run"].is_null() || queued {
            self.spawn_waiter();
            return;
        }
        let (events, signal, tx) = (self.lane.events.clone(), run.signal.clone(), self.tx.clone());
        std::thread::spawn(move || {
            let messages = events.next_message(&signal);
            let _ = tx.send(Input::RuntimeMessages(messages));
        });
    }

    /// Completions of work the turn started: each continues the turn as input; none ends it.
    pub(super) fn runtime_messages(&mut self, messages: Vec<Message>) {
        if self.run.is_none() {
            return;
        }
        if messages.is_empty() || self.run.as_ref().is_some_and(|r| r.cancelled) {
            return self.run_end();
        }
        for message in messages {
            let content = serde_json::to_value(&message).map(|v| v["content"].clone());
            let draft = SubmissionDraft::Input {
                content: content.unwrap_or(Value::Null),
                when_busy: WhenBusy::FollowUp,
                request_id: None,
            };
            if let Err(error) = self.durable.root.submit(draft) {
                self.fail_run(harness_error(error));
                return self.run_end();
            }
        }
        self.spawn_waiter();
    }

    /// The error the turn's prompt ended with when the runtime left it unanswered (faults; model errors and aborts
    /// are reported by their messages).
    pub(super) fn unanswered(&self, submission: Option<SubmissionId>) -> Option<String> {
        let submission = submission?;
        let record = self
            .durable
            .chat
            .commit(|tx| Ok(tx.submission(submission)?))
            .ok()
            .flatten()?;
        if record.status != SubmissionStatus::Unanswered {
            return None;
        }
        let reason = record.reason.clone().unwrap_or_default();
        if matches!(reason.as_str(), "aborted" | "model_error") {
            return None;
        }
        Some(
            record
                .detail
                .as_ref()
                .and_then(Value::as_str)
                .map_or(reason, str::to_string),
        )
    }

    /// The user aborted the turn: the runtime's run is aborted (off the actor thread: an abort joins the run) and
    /// the queued messages wait for the user again.
    pub(super) fn abort_turn(&mut self) {
        self.durable.bridge.set_user_abort(true);
        self.withdraw_queued(&[]);
        let chat = self.durable.chat.clone();
        std::thread::spawn(move || {
            let live = chat.live(ROOT_CONVERSATION_ID).unwrap_or(Value::Null);
            if let Some(task) = live["run"]["taskId"].as_u64() {
                let _ = chat.abort_task(durable::TaskId(task), &Ctx::background());
            }
        });
    }

    /// The transcript messages of the run in progress, from its prompt (`fromTipId` to the tip).
    fn run_messages(&self, source_tip: Option<&str>) -> Result<(Vec<Value>, Vec<Value>), String> {
        let session = lock_session(&self.session);
        let before = session_journal::context::build_session_context(&session.state().branch_path(source_tip)?);
        let all = session.context_messages(LANE)?;
        let run = all.get(before.len()..).map(<[Value]>::to_vec).unwrap_or_default();
        Ok((before, run))
    }

    /// The request the agent loop would send now: the system message, the history before the run's prompt with
    /// its clock lines, then the run's messages.
    fn request_messages(&self, source_tip: Option<&str>) -> Result<Vec<Message>, String> {
        let (mut history, run) = self.run_messages(source_tip)?;
        let Some(prompt) = run.first() else {
            return Err("The run has no prompt".into());
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
        messages.extend(lane::to_llm(&run));
        Ok(messages)
    }

    /// A generation is about to request the model: the turn's `turn_start`, the user messages it takes (queued ones
    /// commit with its `assistant.ready`), and the request it sends.
    pub(super) fn generation(&mut self, users: Vec<Message>, answer: Answer<Result<GenerationStart, String>>) {
        let Some(run) = self.run.as_ref() else {
            return answer.give(Err("No run is in progress".into()));
        };
        let source_tip = run.journal.run().and_then(|r| r.source_tip().map(str::to_string));
        let prompt_pending = run.prompt_pending;
        let prompt = run.prompt.clone();
        // Placed users the transcript already holds (the prompt, or what an earlier attempt of this request took).
        let mut committed: Vec<String> = match self.run_messages(source_tip.as_deref()) {
            Ok((_, messages)) => {
                let start = messages
                    .iter()
                    .rposition(|m| m["role"] == "assistant" || m["role"] == "toolResult")
                    .map_or(0, |i| i + 1);
                messages[start..]
                    .iter()
                    .filter(|m| m["role"] == "user")
                    .map(message_text)
                    .collect()
            }
            Err(_) => Vec::new(),
        };
        let mut fresh = Vec::new();
        for user in users {
            let text = text_of(&user);
            match committed.iter().position(|t| *t == text) {
                Some(index) => {
                    committed.remove(index);
                }
                None => fresh.push(user),
            }
        }
        let turn = self.run.as_ref().map_or(0, |r| r.turns) + 1;
        {
            let mut queues = lock(&self.queues);
            for user in &fresh {
                let text = text_of(user);
                if let Some(index) = queues.items.iter().position(|q| message_text(&q.message) == text) {
                    let mut queued = queues.items.remove(index);
                    queued.turn = turn;
                    queues.delivered.push(queued);
                }
            }
        }
        self.on_agent_event(AgentEvent::TurnStart);
        let mut announced = Vec::new();
        if prompt_pending && let Ok(message) = serde_json::from_value::<Message>(prompt) {
            announced.push(message);
        }
        announced.extend(fresh);
        for message in announced {
            self.on_agent_event(AgentEvent::MessageStart {
                message: message.clone(),
            });
            self.on_agent_event(AgentEvent::MessageEnd { message });
        }
        let start = self
            .request_messages(source_tip.as_deref())
            .map(|messages| GenerationStart {
                messages,
                api_key: self.api_key(),
            });
        answer.give(start);
    }

    pub(super) fn on_turn(&mut self, request: Request) {
        match request {
            Request::Generation { users, answer } => self.generation(users, answer),
            Request::Event(event) => self.on_agent_event(*event),
        }
    }

    /// Resume the operation a dead worker left open: the run continues in the runtime (a pinned request is resent,
    /// a cell that was running reports `interrupted`), after the transcript catches up with what the runtime had
    /// committed. Returns the resumed run's id.
    pub(super) fn reopen_run(&mut self, operation_id: &str) -> Result<String, String> {
        let model = (self.model.max_tokens, self.model.context_window);
        let (mut journal, meta, state) = {
            let session = lock_session(&self.session);
            (
                RunJournal::reopen(&session, operation_id, model)?,
                session.value(OP_META, operation_id).cloned().unwrap_or(Value::Null),
                session.value(OP_STATE, operation_id).cloned().unwrap_or(Value::Null),
            )
        };
        let prompt_entry = meta["intent"]["promptEntryIds"][0]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let prompt = lock_session(&self.session)
            .state()
            .entry(&prompt_entry)
            .map(|entry| entry["message"].clone())
            .unwrap_or(Value::Null);
        let cancelled = state["control"]["status"] == "cancel_requested";
        journal.ledger_counts_turns();
        let mut random = [0u8; 16];
        let _ = getrandom_fill(&mut random);
        let id = self.run_ids.next(now_ms(), random);
        let source_tip = meta["sourceTipId"].as_str().map(str::to_string);
        let unreported = journal.run().map(|r| r.unreported_calls()).unwrap_or_default();
        let submission = self
            .durable
            .chat
            .commit(|tx| Ok(tx.submission_by_request(ROOT_CONVERSATION_ID, &prompt_entry)?))
            .map_err(harness_error)?;
        self.lane.stack.agents.begin_root_turn(operation_id);
        self.run = Some(Running {
            id: id.clone(),
            signal: AbortSignal::new(),
            journal,
            prompt_pending: false,
            turns: 0,
            started_tip: source_tip.clone().map_or(Value::Null, Value::from),
            error: None,
            waiter: None,
            cancelled,
            prompt: prompt.clone(),
            submission: submission.as_ref().map(|s| s.id),
        });
        self.emit(event(vec![
            ("type", json!("run_start")),
            ("runId", json!(id)),
            ("startedAt", json!(now_ms())),
            ("lane", json!(LANE)),
        ]));
        // A prompt admitted to the transcript but not to the runtime is submitted now (idempotent by its entry).
        if submission.is_none() && !cancelled {
            self.durable.bridge.clear();
            let draft = SubmissionDraft::Input {
                content: prompt["content"].clone(),
                when_busy: WhenBusy::FollowUp,
                request_id: Some(prompt_entry.clone()),
            };
            let submitted = self.durable.root.submit(draft).map_err(harness_error)?;
            if let Some(run) = self.run.as_mut() {
                run.submission = Some(submitted.id);
            }
        } else {
            self.catch_up(source_tip.as_deref(), &unreported)?;
        }
        if !cancelled {
            self.submit_queued()?;
        }
        self.durable.chat.resume().map_err(harness_error)?;
        if cancelled {
            self.abort_turn();
        }
        self.spawn_waiter();
        Ok(id)
    }

    /// Project onto the transcript what the runtime committed for the run that the transcript does not hold yet:
    /// answers (with their tool starts), cell results, and the user messages a committed answer took. Everything
    /// goes through the actor's input in commit order, ahead of what the resumed runtime reports.
    fn catch_up(&mut self, source_tip: Option<&str>, unreported: &[String]) -> Result<(), String> {
        let entries = self.durable.root.entries().map_err(harness_error)?;
        let start = entries
            .iter()
            .rposition(|e| e.kind == harness::chat::entries::RESET)
            .map_or(0, |i| i + 1);
        let harness_messages: Vec<(String, Value)> = entries[start..]
            .iter()
            .filter_map(|e| Some((e.kind.clone(), e.model.as_ref()?.first()?.clone())))
            .collect();
        let (_, transcript) = self.run_messages(source_tip)?;
        let bridge = self.durable.bridge.clone();
        let mut position = 0;
        let mut pending_users: Vec<Message> = Vec::new();
        // The round whose answer the transcript holds: its calls, the results reported before the worker died, and
        // whether the bridge knows the round yet.
        let mut round: Option<HeldRound> = None;
        for (kind, message) in harness_messages {
            let role = message["role"].as_str().unwrap_or_default().to_string();
            let held = transcript.get(position).is_some_and(|t| t["role"] == role.as_str());
            match kind.as_str() {
                harness::chat::entries::USER => {
                    if held {
                        position += 1;
                    } else if let Ok(user) = serde_json::from_value::<Message>(message) {
                        pending_users.push(user);
                    }
                }
                harness::chat::entries::ASSISTANT => {
                    let Ok(answer) = serde_json::from_value::<ai::AssistantMessage>(message) else {
                        continue;
                    };
                    if held {
                        position += 1;
                        let calls = answer
                            .content
                            .iter()
                            .filter_map(|c| match c {
                                ai::AssistantContent::ToolCall(call) => Some((call.id.clone(), call.name.clone())),
                                _ => None,
                            })
                            .collect();
                        round = Some((calls, Vec::new()));
                        continue;
                    }
                    // The answer of a generation whose start the transcript never saw.
                    round = None;
                    bridge.request(Request::Generation {
                        users: std::mem::take(&mut pending_users),
                        answer: Answer::default(),
                    });
                    bridge.answer_committed(answer);
                }
                harness::chat::entries::TOOL_RESULT => {
                    if held {
                        position += 1;
                        continue;
                    }
                    let call_id = message["toolCallId"].as_str().unwrap_or_default().to_string();
                    if let Some((calls, mut done)) = round.take() {
                        if !unreported.contains(&call_id) {
                            // Reported before the worker died: its result is pending in the operation's batch.
                            done.push((
                                call_id,
                                AgentToolResult {
                                    content: serde_json::from_value(message["content"].clone()).unwrap_or_default(),
                                    details: message.get("details").cloned().unwrap_or_else(|| json!({})),
                                    usage: None,
                                    terminate: None,
                                },
                                message["isError"] == true,
                            ));
                            round = Some((calls, done));
                            continue;
                        }
                        bridge.resume_round(calls, done);
                    }
                    bridge.result_committed(&message);
                }
                _ => {}
            }
        }
        if let Some((calls, done)) = round.take() {
            bridge.resume_round(calls, done);
        }
        Ok(())
    }

    /// A new ownerless root conversation on the session's model, with `message` as its first input.
    pub(super) fn start_conversation(&mut self, message: &str) -> Result<Value, String> {
        let change = AgentChange {
            model: Some(Some(ModelRef::new(&self.model.provider, &self.model.id))),
            thinking_level: Some(Some(self.thinking.clone())),
        };
        let conversation = self
            .durable
            .chat
            .create_conversation(durable::ConversationOwnership::Ownerless, Some(&change))
            .map_err(harness_error)?;
        conversation
            .submit(SubmissionDraft::input(json!([{"type": "text", "text": message}])))
            .map_err(harness_error)?;
        Ok(json!({"conversationId": conversation.id.0}))
    }

    fn other_conversation(&self, id: u64) -> Result<harness::chat::Conversation, String> {
        let id = durable::ConversationId(id);
        if id == ROOT_CONVERSATION_ID {
            return Err("The session's own conversation takes prompts, not conversation inputs".into());
        }
        self.durable
            .chat
            .conversation(id)
            .map_err(harness_error)?
            .ok_or_else(|| format!("Unknown conversation {}", id.0))
    }

    pub(super) fn conversation_input(&mut self, id: u64, message: &str) -> Result<(), String> {
        self.other_conversation(id)?
            .submit(SubmissionDraft::input(json!([{"type": "text", "text": message}])))
            .map(drop)
            .map_err(harness_error)
    }

    /// A conversation as clients read it: whether it runs, and its messages' roles and texts.
    fn conversation_view(&self, conversation: &harness::chat::Conversation) -> Result<Value, String> {
        let messages: Vec<Value> = conversation
            .entries()
            .map_err(harness_error)?
            .iter()
            .filter_map(|entry| entry.model.as_ref()?.first().cloned())
            .filter(|message| matches!(message["role"].as_str(), Some("user" | "assistant" | "toolResult")))
            .map(|message| json!({"role": message["role"], "text": message_text(&message)}))
            .collect();
        let live = self.durable.chat.live(conversation.id).map_err(harness_error)?;
        let queued = !self
            .durable
            .chat
            .inbox(conversation.id)
            .map_err(harness_error)?
            .is_empty();
        Ok(json!({"id": conversation.id.0, "busy": !live["run"].is_null() || queued, "messages": messages}))
    }

    pub(super) fn conversations(&self, id: Option<u64>) -> Result<Value, String> {
        if let Some(id) = id {
            return self.conversation_view(&self.other_conversation(id)?);
        }
        let ids = self
            .durable
            .chat
            .commit(|tx| {
                let mut ids = Vec::new();
                let mut cursor: Option<durable::Cursor> = None;
                loop {
                    let page =
                        tx.committed_scan_conversations(&durable::ConversationQuery::default(), 256, cursor.as_ref())?;
                    ids.extend(
                        page.items
                            .into_iter()
                            .filter(|c| c.owner.is_none() && c.parent.is_none() && c.id != ROOT_CONVERSATION_ID)
                            .map(|c| c.id),
                    );
                    match page.next {
                        Some(next) => cursor = Some(next),
                        None => return Ok(ids),
                    }
                }
            })
            .map_err(harness_error)?;
        let mut views = Vec::new();
        for id in ids {
            if let Some(conversation) = self.durable.chat.conversation(id).map_err(harness_error)? {
                views.push(self.conversation_view(&conversation)?);
            }
        }
        Ok(json!({"conversations": views}))
    }
}
