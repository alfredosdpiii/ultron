//! A lane run's harness operation journal in the format-4 session (crates/session-journal `run`): the run is admitted
//! with its prompt, each turn's stream is journaled as it arrives, assistant messages and tool results commit as
//! entries, and the run finishes with its outcome. Print, JSON and RPC runs write the same records.

use crate::op_journal::Run;
use agent::AgentEvent;
use ai::Message;
use serde_json::{Value, json};
use session_journal::Session;

/// One run's journal writer. A failed write stops further writes and is reported when the run ends.
pub struct RunJournal {
    run: Option<Run>,
    /// The model's `(max_tokens, context_window)`.
    model: (u64, u64),
    error: Option<String>,
    /// The session's usage ledger counts each response (`recordTurn`) before its entry commits; the journal does
    /// not count it again.
    ledger_counts_turns: bool,
}

impl RunJournal {
    /// Admit a run of `prompt` with the lane `config` (`--no-session` runs pass no session and journal nothing).
    pub fn admit(
        session: Option<&mut Session>,
        config: Value,
        prompt: Value,
        model: (u64, u64),
    ) -> Result<Self, String> {
        Self::admit_with_settings(session, config, prompt, model, session_journal::run::default_settings())
    }

    /// [`RunJournal::admit`] with the operation's `settings` (its compaction settings as the run uses them).
    pub fn admit_with_settings(
        session: Option<&mut Session>,
        config: Value,
        prompt: Value,
        model: (u64, u64),
        settings: Value,
    ) -> Result<Self, String> {
        Self::admit_capturing(session, config, prompt, model, settings, &[], json!([]))
    }

    /// [`RunJournal::admit_with_settings`] placing the queued messages the admission takes (`(entryId, message)`)
    /// before the prompt and leaving `inbox` in the lane's state (see [`Run::admit_with`]).
    pub fn admit_capturing(
        session: Option<&mut Session>,
        config: Value,
        prompt: Value,
        model: (u64, u64),
        settings: Value,
        captured: &[(String, Value)],
        inbox: Value,
    ) -> Result<Self, String> {
        let run = match session {
            Some(session) => Some(Run::admit_with(session, config, settings, prompt, captured, inbox)?),
            None => None,
        };
        Ok(RunJournal {
            run,
            model,
            error: None,
            ledger_counts_turns: false,
        })
    }

    /// Continue the journal of the open operation `operation_id` (a run the durable runtime resumes).
    pub fn reopen(session: &Session, operation_id: &str, model: (u64, u64)) -> Result<Self, String> {
        Ok(RunJournal {
            run: Some(Run::reopen(session, operation_id)?),
            model,
            error: None,
            ledger_counts_turns: false,
        })
    }

    /// Leave counting responses to the usage ledger (see [`RunJournal::event`]).
    pub fn ledger_counts_turns(&mut self) {
        self.ledger_counts_turns = true;
    }

    /// The operation being journaled.
    pub fn run(&self) -> Option<&Run> {
        self.run.as_ref()
    }

    fn apply(&mut self, session: &mut Session, f: impl FnOnce(&mut Run, &mut Session) -> Result<(), String>) {
        if self.error.is_some() {
            return;
        }
        if let Some(run) = self.run.as_mut()
            && let Err(e) = f(run, session)
        {
            self.error = Some(e);
        }
    }

    /// Journal one loop event.
    pub fn event(&mut self, session: &mut Session, event: &AgentEvent) {
        let (max_tokens, context_window) = self.model;
        match event {
            AgentEvent::TurnStart => self.apply(session, |run, s| run.begin_assistant(s, max_tokens, context_window)),
            AgentEvent::MessageStart {
                message: Message::Assistant(partial),
            } => {
                let event = json!({"type": "start", "partial": partial});
                self.apply(session, |run, s| run.stream_event(s, &event));
            }
            AgentEvent::MessageUpdate {
                assistant_message_event,
                ..
            } => {
                if let Ok(event) = serde_json::to_value(assistant_message_event) {
                    self.apply(session, |run, s| run.stream_event(s, &event));
                }
            }
            AgentEvent::MessageEnd {
                message: Message::Assistant(message),
            } => {
                if let Ok(value) = serde_json::to_value(message) {
                    let count = !self.ledger_counts_turns;
                    self.apply(session, |run, s| {
                        if count {
                            run.record_turn(s, &value)?;
                        }
                        run.commit_assistant(s, value.clone()).map(|_| ())
                    });
                }
            }
            AgentEvent::ToolExecutionStart { tool_call_id, args, .. } => {
                self.apply(session, |run, s| run.tool_started(s, tool_call_id, args.clone()));
            }
            AgentEvent::MessageEnd {
                message: Message::ToolResult(result),
            } => {
                if let Ok(value) = serde_json::to_value(result) {
                    self.apply(session, |run, s| {
                        run.tool_finished(s, &result.tool_call_id, value.clone())
                    });
                }
            }
            _ => {}
        }
    }

    /// Begin a generation with `before` (the queued messages it takes) in its `assistant.ready` transaction.
    pub fn begin_with(&mut self, session: &mut Session, before: Vec<session_journal::Write>, trigger: Option<String>) {
        let (max_tokens, context_window) = self.model;
        self.apply(session, |run, s| {
            run.begin_assistant_with(s, max_tokens, context_window, before, trigger)
        });
    }

    /// The stream options of the run's generations (see [`Run::set_stream_options`]).
    pub fn set_stream_options(&mut self, options: Value) {
        if let Some(run) = self.run.as_mut() {
            run.set_stream_options(options);
        }
    }

    /// The generation's retry policy (see [`Run::set_retry_policy`]).
    pub fn set_retry_policy(&mut self, policy: Value) {
        if let Some(run) = self.run.as_mut() {
            run.set_retry_policy(policy);
        }
    }

    /// The next error answer is retried (see [`Run::schedule_retry`]).
    pub fn schedule_retry(&mut self, not_before: i64, error: String) {
        if let Some(run) = self.run.as_mut() {
            run.schedule_retry(not_before, error);
        }
    }

    /// The lane's queued messages as the run's lane state lists them.
    pub fn set_inbox(&mut self, inbox: Value) {
        if let Some(run) = self.run.as_mut() {
            run.set_inbox(inbox);
        }
    }

    /// Record a cancel request at `at` (milliseconds).
    pub fn request_cancel(&mut self, session: &mut Session, at: i64) {
        self.apply(session, |run, s| run.request_cancel(s, at));
    }

    /// End a cancelled run (status `aborted`); returns the first write error.
    pub fn finish_aborted(self, session: &mut Session) -> Result<(), String> {
        if let Some(error) = self.error {
            return Err(error);
        }
        match self.run {
            Some(run) => run.finish_aborted(session),
            None => Ok(()),
        }
    }

    /// End the run (`failure`: the error kind and message it ended with); returns the first write error.
    pub fn finish(self, session: Option<&mut Session>, failure: Option<(&str, &str)>) -> Result<(), String> {
        if let Some(error) = self.error {
            return Err(error);
        }
        match (self.run, session) {
            (Some(run), Some(session)) => run.finish(session, failure),
            _ => Ok(()),
        }
    }

    /// The run's next answer is an overflow that starts a compaction (see [`Run::compact_with_answer`]).
    pub fn compact_with_answer(&mut self, session: &Session, prepared: Value) {
        if let Some(run) = self.run.as_mut() {
            run.compact_with_answer(session, prepared);
        }
    }

    /// A compaction starts at the current checkpoint (see [`Run::begin_compaction`]).
    pub fn begin_compaction(
        &mut self,
        session: &mut Session,
        reason: &str,
        prepared: Value,
        resume_after: Option<Value>,
    ) {
        self.apply(session, |run, s| {
            run.begin_compaction(s, reason, prepared, resume_after)
        });
    }

    /// The summary request: returns the compaction entry's id (none without a journal or after a write error).
    pub fn compaction_request(
        &mut self,
        session: &mut Session,
        retry: &crate::compaction_op::SummaryRetry,
    ) -> Option<String> {
        let mut id = None;
        self.apply(session, |run, s| {
            id = Some(run.compaction_request(s, retry)?);
            Ok(())
        });
        id
    }

    /// The task id of the compaction in progress (the harness's retry `step`).
    pub fn compaction_task_id(&self) -> Option<String> {
        self.run.as_ref()?.compaction_task_id()
    }

    /// See [`Run::compaction_retry_wait`].
    pub fn compaction_retry_wait(
        &mut self,
        session: &mut Session,
        usage: Option<&Value>,
        error: &str,
        not_before: i64,
    ) {
        self.apply(session, |run, s| run.compaction_retry_wait(s, usage, error, not_before));
    }

    /// See [`Run::compaction_retry`].
    pub fn compaction_retry(&mut self, session: &mut Session) {
        self.apply(session, |run, s| run.compaction_retry(s));
    }

    pub fn compaction_usage(&mut self, session: &mut Session, usage: &Value) {
        self.apply(session, |run, s| run.compaction_usage(s, usage));
    }

    pub fn complete_compaction(&mut self, session: &mut Session, id: &str, entry: serde_json::Map<String, Value>) {
        self.apply(session, |run, s| run.complete_compaction(s, id, entry));
    }

    /// See [`Run::complete_compaction_from_hook`]: the compaction entry's id (none after a write error).
    pub fn complete_compaction_from_hook(
        &mut self,
        session: &mut Session,
        entry: serde_json::Map<String, Value>,
        usage: Option<Value>,
    ) -> Option<String> {
        let mut id = None;
        self.apply(session, |run, s| {
            id = Some(run.complete_compaction_from_hook(s, entry, usage)?);
            Ok(())
        });
        id
    }

    /// See [`Run::decline_compaction`]; false also without a journal or after a write error.
    pub fn decline_compaction(&mut self, session: &mut Session, placement: Vec<session_journal::Write>) -> bool {
        let mut resumed = false;
        self.apply(session, |run, s| {
            resumed = run.decline_compaction(s, placement)?;
            Ok(())
        });
        resumed
    }

    pub fn abandon_compaction(&mut self) {
        if let Some(run) = self.run.as_mut() {
            run.abandon_compaction();
        }
    }

    /// The first write error so far.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
}
