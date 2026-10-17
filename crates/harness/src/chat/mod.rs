//! The conversation half of upstream Pi Durable's Harness (`harness/{harness,submissions,inbox,live,generation,tool,
//! compaction,context,prompt,usage}.ts`, spec §2.1, §6, §8), RLM-first: the model's one tool is the persistent Python
//! REPL, so the tool task is a durable REPL cell ([`cell`]) run by a [`CellExecutor`].
//!
//! A [`ChatHarness`] wraps the task [`Harness`](crate::scheduler::Harness): it installs the built-in tasks
//! (`pi.generation`, `pi.tool`, `pi.compaction`) into the application's registry, creates every conversation with
//! its built-in documents, admits submissions through `pi.inbox`, and settles them from the run tasks. Everything is
//! a durable record: a crash loses at most an uncommitted throttle window, and reopening resumes runs, resends pinned
//! requests, and turns interrupted cells into interrupted results without running them again.

pub mod cell;
pub mod children;
pub mod compaction;
pub mod context;
pub mod docs;
pub mod entries;
pub mod faux;
pub mod generation;
pub mod host_ops;
pub mod inbox;
pub mod kernel_cells;
pub mod live;
pub mod migrate;
pub mod models;
pub mod prompt;
pub mod submissions;
pub mod swarm;
mod throttle;
pub mod usage;
mod util;

use std::sync::Arc;

use ai::{AssistantMessage, Message};
use durable::{
    ConversationId, ConversationOwnership, ConversationQuery, Cursor, Doc, EntryDraft, EntryId, EntryQuery,
    EntryRecord, Owner, ROOT_CONVERSATION_ID, ScanOrder, SessionEnd, Storage, SubmissionId, SubmissionRecord, TaskId,
    TaskRecord, Tx, TxScope,
};
use indexmap::IndexMap;
use serde_json::{Value, json};

pub use cell::{CellCall, CellExecutor, CellProgress, CellResult, Diagnostic, Replay};
pub use context::ContextView;
pub use generation::{Agent, Control, GenerationCheckpoint};
pub use inbox::{QueueMode, QueueModes};
pub use models::{
    CompactionPolicy, ModelRef, Models, RetryPolicy, Settings, SettingsSource, StreamRequest, ToolExecution,
};
pub use submissions::{SubmissionAbort, SubmissionDraft, WhenBusy, admit_run_with_steering, admit_submission};

use crate::scheduler::{
    AbortResult, Clock, Ctx, Extension, FallibleClock, Harness, HarnessError, HarnessOptions, Inspection, Registry,
    Report, Result, safe_now,
};
use submissions::Submissions;

/// Renders the system prompt sections of a conversation (key → text), in order, under the invocation's signal. An
/// error is reported and the sections already shown stay.
pub type SectionSource =
    Arc<dyn Fn(ConversationId, &crate::scheduler::Cancel) -> Result<IndexMap<String, String>> + Send + Sync>;

/// `onYield`: user content to continue the run with after a final answer.
pub type OnYield = Arc<dyn Fn(&AssistantMessage) -> Option<Value> + Send + Sync>;
/// `beforeRequest`: replace the messages of one request.
pub type BeforeRequest = Arc<dyn Fn(Vec<Message>) -> Vec<Message> + Send + Sync>;
/// `afterResponse`: observe a terminal provider message.
pub type AfterResponse = Arc<dyn Fn(&AssistantMessage) + Send + Sync>;

/// The compaction policy of one conversation, instead of the settings' (`None`: the settings').
pub type CompactionFor = Arc<dyn Fn(ConversationId) -> Option<CompactionPolicy> + Send + Sync>;
/// `before_compaction` of a blocking threshold compaction: `(conversation, estimated context tokens, threshold)`;
/// entries (`EntryDraft` JSON, as a write takes) to append instead of compacting, or none to compact.
pub type BeforeCompaction = Arc<dyn Fn(ConversationId, u64, u64) -> Vec<Value> + Send + Sync>;

/// Generation hooks (spec §7.2), as plain callbacks.
#[derive(Clone, Default)]
pub struct Hooks {
    /// A conversation's own compaction policy.
    pub compaction: Option<CompactionFor>,
    /// Asked before a blocking threshold compaction; its entries decline it.
    pub before_compaction: Option<BeforeCompaction>,
    /// After a final answer: user content to continue the run with, instead of yielding. The first one wins.
    pub on_yield: Option<OnYield>,
    /// Replace the messages of one request (not stored).
    pub before_request: Option<BeforeRequest>,
    /// Observe every terminal provider message before classification.
    pub after_response: Option<AfterResponse>,
}

/// What the built-in tasks read: shared by every invocation of one Harness.
pub(crate) struct Services {
    pub(crate) models: Arc<dyn Models>,
    pub(crate) cells: Arc<dyn CellExecutor>,
    pub(crate) settings: SettingsSource,
    pub(crate) sections: Option<SectionSource>,
    pub(crate) hooks: Hooks,
    pub(crate) children: children::ChildServices,
}

impl Services {
    /// The compaction policy of `conversation`: its own ([`Hooks::compaction`]), else the settings'.
    pub(crate) fn compaction_policy(&self, conversation: ConversationId, settings: &Settings) -> CompactionPolicy {
        self.hooks
            .compaction
            .as_ref()
            .and_then(|policy| policy(conversation))
            .unwrap_or(settings.compaction)
    }
}

/// Options of [`ChatHarness::open`]. `registry` is the application's; the built-in tasks are installed into it as
/// extension `pi`, replacing an earlier Harness's.
#[derive(Clone)]
pub struct ChatOptions {
    pub registry: Registry,
    pub models: Arc<dyn Models>,
    pub cells: Arc<dyn CellExecutor>,
    pub settings: Option<SettingsSource>,
    pub sections: Option<SectionSource>,
    pub hooks: Hooks,
    pub now: Option<FallibleClock>,
    pub on_report: Option<Report>,
}

impl ChatOptions {
    pub fn new(registry: Registry, models: Arc<dyn Models>, cells: Arc<dyn CellExecutor>) -> ChatOptions {
        ChatOptions {
            registry,
            models,
            cells,
            settings: None,
            sections: None,
            hooks: Hooks::default(),
            now: None,
            on_report: None,
        }
    }
}

/// A change to a conversation's `pi.agent`: a given field replaces the stored one, `Some(None)` clears it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgentChange {
    pub model: Option<Option<ModelRef>>,
    pub thinking_level: Option<Option<String>>,
}

impl AgentChange {
    pub fn model(model: ModelRef) -> AgentChange {
        AgentChange {
            model: Some(Some(model)),
            thinking_level: None,
        }
    }
}

/// Apply one change to `pi.agent`.
pub fn configure(tx: &mut Tx<'_>, conversation_id: ConversationId, change: &AgentChange) -> Result<()> {
    let state = tx.doc(&docs::AGENT, Owner::Conversation(conversation_id))?;
    match &change.model {
        None => {}
        Some(None) => util::delete(&state, &["model"])?,
        Some(Some(model)) => util::set(&state, &["model"], util::to_value(model))?,
    }
    match &change.thinking_level {
        None => {}
        Some(None) => util::delete(&state, &["thinkingLevel"])?,
        Some(Some(level)) => util::set(&state, &["thinkingLevel"], json!(level))?,
    }
    Ok(())
}

/// The built-in creation hook: empty `pi.live`, `pi.inbox`, `pi.usage`, a fresh `pi.provider` and the conversation's
/// `pi.agent`, in the creating commit.
pub fn create_documents(tx: &mut Tx<'_>, conversation_id: ConversationId, fork: bool) -> Result<()> {
    let owner = Owner::Conversation(conversation_id);
    for doc in [&docs::LIVE, &docs::INBOX, &docs::USAGE, &docs::PROVIDER] {
        tx.doc(doc, owner)?;
    }
    // A fork copies `pi.agent` as of its fork entry.
    if !fork {
        tx.doc(&docs::AGENT, owner)?;
    }
    Ok(())
}

/// The durable conversation runtime over one Session. Clones share it.
#[derive(Clone)]
pub struct ChatHarness {
    harness: Harness,
    submissions: Arc<Submissions>,
    now: Clock,
    settings: SettingsSource,
}

impl ChatHarness {
    /// Open over storage: install the built-in tasks, open the task Harness (live tasks reload; surviving `running`
    /// tasks become `pending`), and start serving submissions. Nothing runs until a submit, a wait, or `resume`.
    pub fn open(storage: impl Storage + Send + 'static, options: ChatOptions) -> Result<ChatHarness> {
        let report: Report = {
            let on_report = options.on_report.clone();
            Arc::new(move |error| {
                if let Some(on_report) = &on_report {
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| on_report(error)));
                }
            })
        };
        let now = safe_now(options.now.clone(), report.clone());
        let settings: SettingsSource = options.settings.clone().unwrap_or_else(|| Arc::new(Settings::default));
        let services = Arc::new(Services {
            models: options.models.clone(),
            cells: options.cells.clone(),
            settings: settings.clone(),
            sections: options.sections.clone(),
            hooks: options.hooks.clone(),
            children: children::ChildServices::default(),
        });
        options.registry.install(Extension::tasks(
            "pi",
            vec![
                generation::generation_task(services.clone()),
                cell::cell_task(services.clone()),
                compaction::compaction_task(services.clone()),
                children::child_task(services.clone()),
            ],
        ))?;
        let clock = now.clone();
        let mut harness_options = HarnessOptions::new(Arc::new(options.registry.clone()));
        harness_options.now = Some(Arc::new(move || Ok(clock())));
        harness_options.on_report = Some(report);
        harness_options.settle_outcome = Some(Arc::new(live::settle_scheduler_outcome));
        harness_options.withdraw_inputs = Some(Arc::new(inbox::withdraw_queued_inputs));
        let harness = Harness::open(storage, harness_options)?;
        let submissions = match Submissions::new(harness.session().clone()) {
            Ok(submissions) => submissions,
            Err(error) => {
                let _ = harness.close();
                return Err(error);
            }
        };
        let _ = services.children.submissions.set(submissions.clone());
        Ok(ChatHarness {
            harness,
            submissions,
            now,
            settings,
        })
    }

    /// The task Harness underneath.
    pub fn harness(&self) -> &Harness {
        &self.harness
    }

    pub fn now(&self) -> u64 {
        (self.now)()
    }

    pub fn resume(&self) -> Result<()> {
        self.harness.resume()
    }

    pub fn commit<T>(&self, change: impl FnOnce(&mut Tx<'_>) -> Result<T>) -> Result<T> {
        self.harness.commit(change)
    }

    fn handle(&self, id: ConversationId) -> Conversation {
        Conversation { id, chat: self.clone() }
    }

    /// The root conversation, created at its reserved ID with `agent` applied on first use.
    pub fn root(&self, agent: Option<&AgentChange>) -> Result<Conversation> {
        let id = self.harness.commit(|tx| {
            if tx.conversation(ROOT_CONVERSATION_ID)?.is_some() {
                return Ok(ROOT_CONVERSATION_ID);
            }
            let record = tx.create_root_conversation()?;
            create_documents(tx, record.id, false)?;
            if let Some(agent) = agent {
                configure(tx, record.id, agent)?;
            }
            Ok(record.id)
        })?;
        Ok(self.handle(id))
    }

    pub fn create_conversation(
        &self,
        ownership: ConversationOwnership,
        agent: Option<&AgentChange>,
    ) -> Result<Conversation> {
        let id = self.harness.commit(|tx| {
            let record = tx.create_conversation(ownership)?;
            create_documents(tx, record.id, false)?;
            if let Some(agent) = agent {
                configure(tx, record.id, agent)?;
            }
            Ok(record.id)
        })?;
        Ok(self.handle(id))
    }

    pub fn conversation(&self, id: ConversationId) -> Result<Option<Conversation>> {
        let record = self.harness.commit(|tx| Ok(tx.conversation(id)?))?;
        Ok(record.map(|record| self.handle(record.id)))
    }

    /// Handle for an existing submission.
    pub fn submission(&self, id: SubmissionId) -> Result<Option<Submission>> {
        Ok(self
            .submissions
            .exists(id)?
            .then(|| Submission { id, chat: self.clone() }))
    }

    /// Withdraw a queued submission (optionally only of `conversation_id`); placed and settled ones are reported.
    pub fn abort_submission(
        &self,
        id: SubmissionId,
        conversation_id: Option<ConversationId>,
    ) -> Result<SubmissionAbort> {
        self.submissions.abort(id, conversation_id)
    }

    pub fn get_task(&self, id: TaskId) -> Result<Option<TaskRecord>> {
        self.harness.get_task(id)
    }

    pub fn abort_task(&self, id: TaskId, ctx: &Ctx) -> Result<AbortResult> {
        self.harness.abort_task(id, ctx)
    }

    pub fn wait_for_task(&self, id: TaskId, ctx: &Ctx) -> Result<TaskRecord> {
        self.harness.wait_for_task(id, ctx)
    }

    pub fn wait_for_idle(&self, ctx: &Ctx) -> Result<()> {
        self.harness.wait_for_idle(ctx)
    }

    pub fn inspect(&self) -> Result<Inspection> {
        self.harness.inspect()
    }

    pub fn snapshot(&self, doc: &Doc, owner: Owner) -> Result<Option<Arc<Value>>> {
        self.harness.snapshot(doc, owner)
    }

    /// A conversation's committed `pi.live`.
    pub fn live(&self, conversation_id: ConversationId) -> Result<Value> {
        Ok(self
            .snapshot(&docs::LIVE, Owner::Conversation(conversation_id))?
            .map(|value| (*value).clone())
            .unwrap_or(json!({})))
    }

    /// A conversation's committed queued items, as `(submission, mode)`.
    pub fn inbox(&self, conversation_id: ConversationId) -> Result<Vec<(SubmissionId, String)>> {
        let state = self.snapshot(&docs::INBOX, Owner::Conversation(conversation_id))?;
        Ok(state
            .and_then(|state| state["items"].as_array().cloned())
            .unwrap_or_default()
            .iter()
            .map(|item| {
                (
                    SubmissionId(item["id"].as_u64().unwrap_or_default()),
                    item["mode"].as_str().unwrap_or_default().to_owned(),
                )
            })
            .collect())
    }

    /// Every conversation's provider session ID (`pi.provider`), the key provider connection caches use (the Codex
    /// WebSocket cache), in conversation order.
    pub fn provider_session_ids(&self) -> Result<Vec<String>> {
        let mut ids = Vec::new();
        for id in self.conversation_ids()? {
            if let Some(state) = self.snapshot(&docs::PROVIDER, Owner::Conversation(id))?
                && let Some(session) = state["sessionId"].as_str()
            {
                ids.push(session.to_owned());
            }
        }
        Ok(ids)
    }

    fn conversation_ids(&self) -> Result<Vec<ConversationId>> {
        self.harness.commit(|tx| {
            let mut ids = Vec::new();
            let mut cursor: Option<Cursor> = None;
            loop {
                let page = tx.committed_scan_conversations(&ConversationQuery::default(), 256, cursor.as_ref())?;
                ids.extend(page.items.into_iter().map(|record| record.id));
                match page.next {
                    Some(next) => cursor = Some(next),
                    None => return Ok(ids),
                }
            }
        })
    }

    /// Sum every conversation's committed `pi.usage` (spec §8.6).
    pub fn usage(&self) -> Result<Value> {
        let conversations = self.conversation_ids()?;
        let mut total = json!({ "models": {}, "tools": {} });
        for id in conversations {
            if let Some(state) = self.snapshot(&docs::USAGE, Owner::Conversation(id))? {
                usage::add_usage_state(&mut total, &state);
            }
        }
        Ok(total)
    }

    /// Seal admission, join every invocation, close Storage.
    pub fn close(&self) -> Result<()> {
        self.harness.close()
    }

    pub fn closed(&self) -> SessionEnd {
        self.harness.closed()
    }

    fn submit(&self, conversation_id: ConversationId, draft: &SubmissionDraft) -> Result<Submission> {
        // Submitting asks for progress.
        let _ = self.harness.resume();
        // Queue modes and the clock are read on the Session line, when admission decides.
        let id = self.harness.commit(|tx| {
            admit_submission(
                tx,
                conversation_id,
                draft,
                (self.now)(),
                (self.settings)().queue_modes(),
            )
        })?;
        Ok(Submission { id, chat: self.clone() })
    }

    fn submit_run(
        &self,
        conversation_id: ConversationId,
        first: &SubmissionDraft,
        steering: &[SubmissionDraft],
    ) -> Result<Vec<Submission>> {
        let _ = self.harness.resume();
        let placed = self.harness.commit(|tx| {
            admit_run_with_steering(
                tx,
                conversation_id,
                first,
                steering,
                (self.now)(),
                (self.settings)().queue_modes(),
            )
        })?;
        match placed {
            Some(ids) => Ok(ids
                .into_iter()
                .map(|id| Submission { id, chat: self.clone() })
                .collect()),
            None => std::iter::once(first)
                .chain(steering)
                .map(|draft| self.submit(conversation_id, draft))
                .collect(),
        }
    }
}

/// Handle of one durable submission.
#[derive(Clone)]
pub struct Submission {
    pub id: SubmissionId,
    chat: ChatHarness,
}

impl Submission {
    pub fn status(&self) -> Result<SubmissionRecord> {
        self.chat.submissions.status(self.id)
    }

    /// Wait until `done` or `unanswered`; enables scheduling. Cancelling `ctx` cancels only the wait.
    pub fn wait(&self, ctx: &Ctx) -> Result<SubmissionRecord> {
        let _ = self.chat.harness.resume();
        self.chat.submissions.wait(self.id, ctx)
    }

    /// Withdraw it while queued.
    pub fn abort(&self) -> Result<SubmissionAbort> {
        match self.chat.submissions.abort(self.id, None)? {
            SubmissionAbort::NotFound => Err(HarnessError::Error(format!("Submission {} does not exist", self.id.0))),
            result => Ok(result),
        }
    }
}

/// Handle of one conversation.
#[derive(Clone)]
pub struct Conversation {
    pub id: ConversationId,
    chat: ChatHarness,
}

impl Conversation {
    pub fn chat(&self) -> &ChatHarness {
        &self.chat
    }

    /// Admit a submission (spec §6).
    pub fn submit(&self, draft: SubmissionDraft) -> Result<Submission> {
        self.chat.submit(self.id, &draft)
    }

    /// Admit input that starts a run with the steering its first request takes ([`admit_run_with_steering`]);
    /// when the conversation cannot start a run at once, each draft is admitted on its own. The submissions are in
    /// draft order.
    pub fn submit_run(&self, first: SubmissionDraft, steering: Vec<SubmissionDraft>) -> Result<Vec<Submission>> {
        self.chat.submit_run(self.id, &first, &steering)
    }

    /// Start a new context: a `pi.reset` write with `head: "self"`, placed at once when idle, else at the next
    /// boundary.
    pub fn reset(&self, handoff: Option<&str>) -> Result<()> {
        let mut entry = json!({ "kind": entries::RESET, "head": "self" });
        if let Some(text) = handoff {
            entry["model"] = json!([{ "role": "user", "content": text, "timestamp": self.chat.now() }]);
        }
        self.submit(SubmissionDraft::write(entry))?;
        Ok(())
    }

    /// Start a manual compaction, owned by the conversation.
    pub fn compact(&self, instructions: Option<&str>) -> Result<TaskId> {
        let _ = self.chat.harness.resume();
        let id = self.id;
        self.commit(|tx| compaction::create_compaction(tx, id, "manual", instructions, None))
    }

    /// Commit with this conversation as the default for `Tx::create_task`.
    pub fn commit<T>(&self, change: impl FnOnce(&mut Tx<'_>) -> Result<T>) -> Result<T> {
        let scope = TxScope {
            conversation_id: Some(self.id),
            task_id: None,
        };
        crate::scheduler::commit_on(self.chat.harness.session(), scope, change)
    }

    pub fn configure(&self, change: &AgentChange) -> Result<()> {
        let id = self.id;
        self.commit(|tx| configure(tx, id, change))
    }

    /// The committed context, or the context cut off at the visible entry `at`.
    pub fn context(&self, at: Option<EntryId>) -> Result<ContextView> {
        let id = self.id;
        self.commit(|tx| context::read_context(tx, id, at))
    }

    /// Every visible entry, oldest first.
    pub fn entries(&self) -> Result<Vec<EntryRecord>> {
        let id = self.id;
        self.commit(|tx| {
            let query = EntryQuery {
                conversation_id: id,
                min_entry_id: None,
                max_entry_id: None,
                order: Some(ScanOrder::Ascending),
            };
            let mut entries = Vec::new();
            let mut cursor: Option<Cursor> = None;
            loop {
                let page = tx.committed_scan_entries(&query, 256, cursor.as_ref())?;
                entries.extend(page.items);
                match page.next {
                    Some(next) => cursor = Some(next),
                    None => return Ok(entries),
                }
            }
        })
    }

    /// Append an entry directly (an application write to an idle conversation).
    pub fn append(&self, draft: EntryDraft) -> Result<EntryRecord> {
        let id = self.id;
        self.commit(|tx| Ok(tx.append_entry(id, draft)?))
    }

    /// Fork history at a visible entry into a new conversation.
    pub fn fork(&self, at: EntryId, ownership: ConversationOwnership) -> Result<Conversation> {
        let parent = self.id;
        let id = self.chat.harness.commit(|tx| {
            let record = tx.fork_conversation(parent, at, ownership)?;
            create_documents(tx, record.id, true)?;
            Ok(record.id)
        })?;
        Ok(self.chat.handle(id))
    }

    /// Withdraw queued inputs and abort the live non-background work reachable from this conversation.
    pub fn abort(&self, background: bool, ctx: &Ctx) -> Result<()> {
        self.chat
            .harness
            .conversation(self.id)?
            .map_or(Ok(()), |handle| handle.abort(background, ctx))
    }

    pub fn wait_for_idle(&self, ctx: &Ctx) -> Result<()> {
        self.chat
            .harness
            .conversation(self.id)?
            .map_or(Ok(()), |handle| handle.wait_for_idle(ctx))
    }
}
