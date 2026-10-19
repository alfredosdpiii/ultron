//! Automatic compaction of the root lane, run by the session actor at a run's checkpoints as the session worker's
//! harness does (packages/agent/src/harness/runtime/drive): before a generation's request and before the run ends,
//! a threshold compaction when the context crosses `contextWindow - reserveTokens`; on a response that overflowed
//! the window, an overflow compaction, after which the request is retried once (crate::turns asks; another overflow
//! fails the run). Each one is journaled inside the run's operation (`summary.deciding` .. the compaction entry and
//! the resumed checkpoint), so a worker killed during one reopens at the checkpoint and compacts again there.
//!
//! Before a threshold compaction the `before_compaction` hook runs: ContextControl trims old tool results instead
//! when that brings the context under the threshold (`ULTRON_COMPACTION_TRIM=off` disables it). The summary runs
//! off the actor (core::compacting): an `abort` cancels it, the compaction ends `aborted` and the run with it.

use super::*;
use crate::auto_compaction::{self, Reason};
use crate::compaction_op::prepared_value;
use crate::turns::{Answer, Settled};
use compacting::{AttemptFn, CANCELLED, RetryStep};
use compaction::CompactionSettings;

/// How a run's compaction ended (before its `compaction_end`).
enum Work {
    /// The entry committed.
    Done,
    /// An abort cancelled it.
    Aborted,
    /// It failed.
    Failed(String),
}

/// A compaction ready to summarize: its preparation and the messages it keeps.
pub(super) struct Prepared {
    pub preparation: compaction::CompactionPreparation,
    pub retained_tail: Vec<Value>,
}

impl Prepared {
    fn value(&self, settings: &CompactionSettings) -> Value {
        prepared_value(&self.preparation, &self.retained_tail, &settings_value(settings))
    }
}

fn settings_value(settings: &CompactionSettings) -> Value {
    json!({
        "enabled": settings.enabled, "reserveTokens": settings.reserve_tokens,
        "keepRecentTokens": settings.keep_recent_tokens,
    })
}

impl Core {
    /// The compaction settings the root lane runs with: settings.json's (`compaction.*`, per-model overrides), with
    /// `set_auto_compaction` deciding whether it is on. Invalid token settings turn automatic compaction off.
    pub(super) fn compaction_settings(&self) -> CompactionSettings {
        auto_compaction::settings(
            &self.runtime.config.settings,
            &self.model.provider,
            &self.model.id,
            Some(self.auto_compaction),
        )
        .unwrap_or(CompactionSettings {
            enabled: false,
            ..CompactionSettings::default()
        })
    }

    /// The operation settings a run is admitted with (`pi.op.state` `settings`).
    pub(super) fn run_settings(&self) -> Value {
        let mut settings = session_journal::run::default_settings();
        settings["compaction"] = settings_value(&self.compaction_settings());
        settings
    }

    /// What a compaction of the branch summarizes and keeps (context edits applied first: what the model no longer
    /// sees is neither summarized back in nor kept). With nothing to cut it keeps the whole branch after an
    /// (empty-conversation) summary, as the native harness does.
    pub(super) fn prepare_compaction(&self, settings: CompactionSettings) -> Result<Prepared, String> {
        self.prepare_compaction_of(settings, false)
    }

    /// [`Core::prepare_compaction`]; `before_overflow`: of the branch before its overflowed answer (the tip), as
    /// the compaction that answer started was prepared.
    fn prepare_compaction_of(&self, settings: CompactionSettings, before_overflow: bool) -> Result<Prepared, String> {
        let pi = self.pi();
        let mut path: Vec<Value> = pi.branch_entries(None).into_iter().cloned().collect();
        if before_overflow
            && path
                .last()
                .is_some_and(|e| e["type"] == "message" && e["message"]["stopReason"] == "error")
        {
            path.pop();
        }
        let path = session_journal::context::apply_context_edits(&path);
        let preparation = match compaction::prepare_compaction(&path, settings) {
            Some(preparation) => preparation,
            None => {
                if path.last().is_some_and(|e| e["type"] == "compaction") {
                    return Err("Already compacted".into());
                }
                // After an earlier compaction: what it kept onwards, and its summary to update.
                let previous = path.iter().rposition(|e| e["type"] == "compaction");
                let first = match previous {
                    Some(index) => path[index]["firstKeptEntryId"].as_str(),
                    None => path.first().and_then(|e| e["id"].as_str()),
                }
                .ok_or("Nothing to compact (session too small)")?;
                let previous_summary = previous.and_then(|index| path[index]["summary"].as_str().map(str::to_string));
                let projection = pi.build_session_projection();
                let tokens = compaction::estimate_projected_context_tokens(&projection, &path).tokens;
                compaction::CompactionPreparation {
                    first_kept_entry_id: first.to_string(),
                    messages_to_summarize: Vec::new(),
                    turn_prefix_messages: Vec::new(),
                    is_split_turn: false,
                    tokens_before: tokens,
                    previous_summary,
                    file_ops: Default::default(),
                    settings,
                }
            }
        };
        let retained_tail = tail_from(&path, &preparation.first_kept_entry_id).unwrap_or_default();
        Ok(Prepared {
            preparation,
            retained_tail,
        })
    }

    fn compaction_event(&mut self, kind: &str, reason: Reason, status: Option<(&str, Option<&str>)>) {
        let mut fields = vec![
            ("type", json!(kind)),
            ("lane", json!(LANE)),
            ("runId", self.run_id()),
            ("reason", json!(reason.name())),
        ];
        if let Some((status, error)) = status {
            fields.push(("status", json!(status)));
            if let Some(message) = error {
                fields.push(("error", json!({"message": message})));
            }
        }
        self.emit(event(fields));
    }

    fn journal_error(&self) -> Option<String> {
        self.run
            .as_ref()
            .and_then(|run| run.journal.error().map(str::to_string))
    }

    /// The compaction the journal started (`summary.deciding`): the summary request (off the actor, cancelled by an
    /// abort), the compaction entry and the resumed checkpoint, then `compaction_end`. An error leaves the run to
    /// fail (or ends it aborted after an abort).
    fn complete_compaction(&mut self, reason: Reason, prepared: &Prepared) -> Result<(), String> {
        let signal = AbortSignal::new();
        self.compacting = Some(signal.clone());
        let work = self.compaction_work(reason, prepared, &signal);
        self.compacting = None;
        let message = match work {
            Work::Done => {
                // A compacted context starts a new approach to the limit: its next crossing reminds again.
                self.lane.stack.context_reminders.rearm(LANE);
                self.announce();
                self.compaction_event("compaction_end", reason, Some(("completed", None)));
                return Ok(());
            }
            Work::Aborted => {
                if let Some(run) = self.run.as_mut() {
                    run.journal.abandon_compaction();
                }
                self.compaction_event("compaction_end", reason, Some(("aborted", None)));
                return Err(CANCELLED.into());
            }
            Work::Failed(message) => message,
        };
        // The run fails with the compaction's error (`summarization_failed`).
        if let Some(run) = self.run.as_mut() {
            run.journal.abandon_compaction();
            // Its error replaces the overflowed answer's: the run ends with the compaction's.
            run.error = Some(message.clone());
            run.compaction_failed = true;
        }
        self.compaction_event("compaction_end", reason, Some(("failed", Some(message.as_str()))));
        Err(message)
    }

    /// The summary of a run's compaction, journaled; nothing is announced.
    fn compaction_work(&mut self, reason: Reason, prepared: &Prepared, signal: &AbortSignal) -> Work {
        if signal.aborted() {
            return Work::Aborted;
        }
        let retry = self.summary_retry();
        let id = match self.run.as_mut() {
            Some(run) => run.journal.compaction_request(&mut lock_session(&self.session), &retry),
            None => None,
        };
        let Some(id) = id else {
            return Work::Failed(self.journal_error().unwrap_or_else(|| "The run has no journal".into()));
        };
        let summarizer = self.summarizer();
        let (preparation, cancel) = (prepared.preparation.clone(), signal.clone());
        let attempt: AttemptFn<compaction::CompactionResult> =
            Arc::new(move || summarizer.run(&preparation, None, &cancel));
        let (run_id, task_id) = self.run.as_ref().map_or_else(Default::default, |run| {
            (run.id.clone(), run.journal.compaction_task_id().unwrap_or_default())
        });
        let mut journal = |core: &mut Core, step: RetryStep<'_>| {
            let Some(run) = core.run.as_mut() else {
                return Err("No run is in progress".to_string());
            };
            let mut session = lock_session(&core.session);
            match step {
                RetryStep::Wait {
                    usage,
                    error,
                    not_before,
                } => run
                    .journal
                    .compaction_retry_wait(&mut session, usage.as_ref(), error, not_before),
                RetryStep::Retry => run.journal.compaction_retry(&mut session),
            }
            drop(session);
            core.journal_error().map_or(Ok(()), Err)
        };
        let (result, last) = self.summary_attempts(
            signal,
            retry,
            (&run_id, &task_id),
            json!({"source": "compaction", "reason": reason.name()}),
            attempt,
            &mut journal,
        );
        let last_usage = last.map(|message| message.usage);
        if signal.aborted() {
            return Work::Aborted;
        }
        let result = match result {
            Ok(result) => result,
            Err(message) => {
                // A failed summary response's usage is recorded before the run fails with it.
                if let (Some(usage), Some(run)) = (last_usage, self.run.as_mut()) {
                    let usage = serde_json::to_value(&usage).unwrap_or(Value::Null);
                    run.journal.compaction_usage(&mut lock_session(&self.session), &usage);
                }
                return Work::Failed(message);
            }
        };
        let usage = serde_json::to_value(&result.usage).unwrap_or(Value::Null);
        let mut entry = Map::new();
        entry.insert("type".into(), "compaction".into());
        entry.insert("summary".into(), result.summary.into());
        entry.insert("retainedTail".into(), Value::Array(prepared.retained_tail.clone()));
        entry.insert("tokensBefore".into(), result.tokens_before.into());
        entry.insert(
            "details".into(),
            json!({"readFiles": result.read_files, "modifiedFiles": result.modified_files}),
        );
        entry.insert("usage".into(), usage.clone());
        entry.insert("fromHook".into(), false.into());
        let Some(run) = self.run.as_mut() else {
            return Work::Failed("No run is in progress".into());
        };
        let mut session = lock_session(&self.session);
        run.journal.compaction_usage(&mut session, &usage);
        run.journal.complete_compaction(&mut session, &id, entry);
        drop(session);
        match self.journal_error() {
            None => Work::Done,
            Some(error) => Work::Failed(error),
        }
    }

    /// Resume the checkpoint without the compaction it started (a `before_compaction` hook declined it): what
    /// waited for the boundary is placed, `compaction_end` with `status`. `waiting`: the context entries pending
    /// before the hook.
    fn decline(&mut self, reason: Reason, waiting: usize, status: &str) {
        // The trim's edit waits for the run's next boundary, as a queued write does.
        let pending = context_lane::pending_count(&lock_session(&self.session));
        if pending > waiting {
            self.queue_update();
        }
        // Before a request, the boundary is now: what waited for it is placed with the resumed checkpoint.
        let placement = {
            let session = lock_session(&self.session);
            let tip = session.branch_tip(LANE).flatten();
            let (mut writes, placed) = context_lane::placement(&session, tip.clone());
            if let Some(placed) = placed.filter(|placed| Some(placed) != tip.as_ref()) {
                writes.push(Write::set(BRANCH_TIP, LANE, placed.into()));
            }
            writes
        };
        let resumed = match self.run.as_mut() {
            Some(run) => run
                .journal
                .decline_compaction(&mut lock_session(&self.session), placement),
            None => false,
        };
        self.compaction_event("compaction_end", reason, Some((status, None)));
        if resumed && pending > 0 {
            self.announce();
            self.queue_update();
        }
    }

    /// A checkpoint of the run in progress (before a generation's request, or before the run ends): compact when the
    /// context crossed the threshold, or when the worker died during an overflow compaction here. Returns whether
    /// it compacted; an error is the failed compaction the run ends with.
    pub(super) fn checkpoint_compaction(&mut self) -> Result<bool, String> {
        let Some(run) = self.run.as_mut() else {
            return Ok(false);
        };
        if run.cancelled || run.signal.aborted() || run.compaction_failed {
            return Ok(false);
        }
        let resumed = run.resume_overflow.take();
        let settings = self.compaction_settings();
        let Ok(prepared) = self.prepare_compaction_of(settings, resumed.is_some()) else {
            return Ok(false);
        };
        let reason = if resumed.is_some() {
            Reason::Overflow
        } else {
            let path: Vec<Value> = self.pi().branch_entries(None).into_iter().cloned().collect();
            let trigger = self
                .run
                .as_ref()
                .and_then(|run| run.journal.run().map(|r| r.trigger().to_string()));
            let due = auto_compaction::threshold_due(
                &settings,
                &path,
                trigger.as_deref(),
                prepared.preparation.tokens_before,
                self.model.context_window,
            );
            if !due {
                return Ok(false);
            }
            Reason::Threshold
        };
        if let Some(run) = self.run.as_mut() {
            run.journal.begin_compaction(
                &mut lock_session(&self.session),
                reason.name(),
                prepared.value(&settings),
                resumed,
            );
        }
        self.compaction_event("compaction_start", reason, None);
        // `before_compaction`: ContextControl may trim old tool results instead of a threshold compaction.
        let waiting = context_lane::pending_count(&lock_session(&self.session));
        let window = i64::try_from(self.model.context_window).ok();
        let tokens = i64::try_from(prepared.preparation.tokens_before).unwrap_or(i64::MAX);
        let reserve = i64::try_from(settings.reserve_tokens).unwrap_or(i64::MAX);
        if self
            .context
            .before_compaction(LANE, reason.name(), tokens, window, reserve)
            .unwrap_or(false)
        {
            self.decline(reason, waiting, "declined");
            return Ok(false);
        }
        self.complete_compaction(reason, &prepared)?;
        Ok(true)
    }

    /// A root response overflowed the context window (crate::turns): unless this generation already recovered from
    /// one, the overflowed answer commits as an error with the compaction it starts, and the request is retried once
    /// the compaction completes. Otherwise the error is the answer the runtime commits (the run fails with it).
    pub(super) fn overflow(&mut self, message: AssistantMessage, answer: Answer<Settled>) {
        let error = auto_compaction::as_error(&message);
        let settings = self.compaction_settings();
        let recoverable = self.run.as_ref().is_some_and(|run| {
            !run.cancelled
                && !run.signal.aborted()
                && !run.compaction_failed
                && run.journal.run().is_some_and(|r| !r.overflow_recovery_used())
        }) && settings.enabled;
        let prepared = if recoverable {
            self.prepare_compaction(settings).ok()
        } else {
            None
        };
        let Some(prepared) = prepared else {
            return answer.give(Settled::Answer(Box::new(error)));
        };
        if let Some(run) = self.run.as_mut() {
            run.journal
                .compact_with_answer(&lock_session(&self.session), prepared.value(&settings));
        }
        self.on_agent_event(AgentEvent::MessageEnd {
            message: Message::Assistant(error),
        });
        self.compaction_event("compaction_start", Reason::Overflow, None);
        match self.complete_compaction(Reason::Overflow, &prepared) {
            Ok(_) => answer.give(Settled::Retry),
            Err(error) => answer.give(Settled::Failed(error)),
        }
    }
}

/// The messages of the kept entries of `path`, from entry `first` to the branch's end; none without that entry.
fn tail_from(path: &[Value], first: &str) -> Option<Vec<Value>> {
    let kept_from = path.iter().position(|e| e["id"] == first)?;
    Some(
        path[kept_from..]
            .iter()
            .filter(|e| e["type"] != "compaction")
            .flat_map(|e| match e["type"].as_str() {
                Some("message") => vec![e["message"].clone()],
                _ => session_journal::context::entry_context_messages(e),
            })
            .collect(),
    )
}
