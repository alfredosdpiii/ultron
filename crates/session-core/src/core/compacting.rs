//! A compaction in progress, off the actor's thread: the summary request runs on a worker thread while the actor keeps serving the operations that must not wait for them. `abort` cancels the
//! compaction at once (manual `/compact`, threshold and overflow compactions alike) and `state` reports
//! `isCompacting`, as AgentSession does (`abortCompaction`, `isCompacting`); every other input waits for the
//! compaction, in order, as it did while the actor was blocked. A prompt with no run in progress is refused, as
//! TypeScript refuses one during a manual compaction; a tree navigation is refused as `navigateTree` refuses it
//! while `isCompacting`; a `compact` aborts what is in progress first (`compact` awaits `abort()`), then runs. A
//! tree navigation's branch summary runs the same way (core::navigate). (Pi extensions' compaction hooks,
//! `session_before_compact` and `session_compact`, are not supported by ultron-rs.)

use super::*;
use crate::compaction_op::{self, SummaryRetry};

/// The error a cancelled compaction ends with (AgentSession: `Compaction cancelled`).
pub(super) const CANCELLED: &str = "Compaction cancelled";
/// A prompt during a manual compaction (AgentSession `prompt`).
const PROMPT_WHILE_COMPACTING: &str =
    "Cannot submit a prompt while compaction is in progress. Wait for compaction to finish and retry.";
/// A prompt during a navigation (harness `LaneBusy`: the navigation is the lane's operation).
const LANE_BUSY: &str = "Lane \"main\" already has an active operation";
/// A tree navigation during a compaction or another navigation's summary (AgentSession `navigateTree`).
pub(super) const NAVIGATE_WHILE_COMPACTING: &str =
    "Wait for the current compaction or tree navigation to finish before navigating the session tree.";

/// Sends the actor its wake-up when the work ends, also when it panics.
struct Wake(Sender<Input>);

impl Drop for Wake {
    fn drop(&mut self) {
        let _ = self.0.send(Input::Woken);
    }
}

/// The session's model call for a summary, taken off the actor (`summarize` as the actor makes it).
pub(super) struct Summarizer {
    provider: Arc<Provider>,
    model: Model,
    api_key: Option<String>,
    thinking: String,
    session_id: String,
}

impl Summarizer {
    /// One summary request (`request`), cancelled by `signal`.
    async fn stream(&self, request: compaction::SummaryRequest, signal: &AbortSignal) -> ai::AssistantMessage {
        let options = SimpleStreamOptions {
            base: StreamOptions {
                api_key: self.api_key.clone(),
                max_tokens: Some(request.max_tokens),
                cache_retention: Some(ai::CacheRetention::None),
                session_id: request.session_id.clone(),
                signal: Some(signal.clone()),
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

    /// `/bug`'s report summary of `messages` (`summarizeForBugReport`, `generateBugReportSummary`), cancelled by
    /// `signal`; a retryable provider error is retried under `retry` (`completeSummarization`'s retry policy).
    pub(super) fn bug_report(
        &self,
        messages: &[Value],
        hint: Option<&str>,
        retry: SummaryRetry,
        signal: &AbortSignal,
    ) -> Result<String, String> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| error.to_string())?;
        let mut attempt = 1;
        loop {
            let request = compaction::bug_summary_request(
                messages,
                hint,
                &self.model,
                Some(&self.thinking),
                Some(&self.session_id),
                now_ms(),
            );
            let response = runtime.block_on(self.stream(request, signal));
            if !signal.aborted() && attempt < retry.max_attempts && harness::chat::models::is_retryable(&response) {
                let delay = i64::try_from(retry.delay_ms(attempt)).unwrap_or(i64::MAX);
                wait_until(now_ms().saturating_add(delay), signal);
                attempt += 1;
                continue;
            }
            if signal.aborted() {
                return Err("Bug report summary was cancelled".into());
            }
            return compaction::bug_summary_text(&response);
        }
    }

    /// The branch summary of `entries` (`generateBranchSummary`), cancelled by `signal`.
    pub(super) fn branch(
        &self,
        entries: &[Value],
        custom_instructions: Option<&str>,
        replace_instructions: bool,
        reserve_tokens: u64,
        signal: &AbortSignal,
    ) -> Attempt<compaction::BranchSummary> {
        if signal.aborted() {
            return (Err(CANCELLED.into()), None);
        }
        let options = compaction::BranchSummaryOptions {
            model: &self.model,
            custom_instructions,
            replace_instructions,
            reserve_tokens,
            now_ms: now_ms(),
        };
        let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(runtime) => runtime,
            Err(error) => return (Err(error.to_string()), None),
        };
        let mut last = None;
        let result = runtime.block_on(compaction::generate_branch_summary(
            entries,
            &options,
            async |request| {
                let response = self.stream(request, signal).await;
                last = Some(response.clone());
                response
            },
        ));
        let result = result.map_err(|error| match error {
            compaction::BranchSummaryError::Aborted => CANCELLED.to_string(),
            compaction::BranchSummaryError::Failed(message) => message,
        });
        (result, last)
    }

    /// Summarize `preparation`, cancelled by `signal`; the result and the last summary response.
    pub(super) fn run(
        &self,
        preparation: &compaction::CompactionPreparation,
        custom_instructions: Option<&str>,
        signal: &AbortSignal,
    ) -> Attempt<compaction::CompactionResult> {
        if signal.aborted() {
            return (Err(CANCELLED.into()), None);
        }
        let options = compaction::CompactOptions {
            model: &self.model,
            custom_instructions,
            thinking_level: Some(&self.thinking),
            session_id: Some(&self.session_id),
            now_ms: now_ms(),
        };
        let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(runtime) => runtime,
            Err(error) => return (Err(error.to_string()), None),
        };
        let mut last = None;
        let result = runtime.block_on(compaction::compact(preparation, &options, async |request| {
            let response = self.stream(request, signal).await;
            last = Some(response.clone());
            response
        }));
        (result, last)
    }
}

/// One summary attempt's result and the last response it got (whether a failure is retryable).
pub(super) type Attempt<T> = (Result<T, String>, Option<ai::AssistantMessage>);

/// A summary attempt, run off the actor (again on a retry).
pub(super) type AttemptFn<T> = Arc<dyn Fn() -> Attempt<T> + Send + Sync>;

/// What the summary attempts journal between attempts (`summary.retry_wait`, then the next attempt).
pub(super) enum RetryStep<'a> {
    /// A retryable attempt failed: its response's usage, the error and when the next attempt may start.
    Wait {
        usage: Option<Value>,
        error: &'a str,
        not_before: i64,
    },
    /// The wait ended: the next attempt starts.
    Retry,
}

/// Waits until `not_before` (milliseconds since the epoch) or until `signal` aborts.
fn wait_until(not_before: i64, signal: &AbortSignal) {
    let delay = u64::try_from(not_before.saturating_sub(now_ms())).unwrap_or(0);
    if delay == 0 || signal.aborted() {
        return;
    }
    if let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_all().build() {
        // The timer is made inside the runtime (it needs its reactor).
        let _ = runtime.block_on(async {
            tokio::time::timeout(std::time::Duration::from_millis(delay), signal.cancelled()).await
        });
    }
}

impl Core {
    /// The summary call as this session makes it now.
    pub(super) fn summarizer(&self) -> Summarizer {
        Summarizer {
            provider: self.provider.clone(),
            model: self.model.clone(),
            api_key: self.keys.request_key(&self.model.provider),
            thinking: self.thinking.clone(),
            session_id: lock_session(&self.session).id().to_string(),
        }
    }

    /// Run `work` on its own thread while the actor serves what may not wait for it ([`Core::while_compacting`]);
    /// the other inputs are kept, in order, for after it. `Err` when the work panicked.
    pub(super) fn off_actor<T: Send + 'static>(
        &mut self,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, String> {
        let wake = Wake(self.tx.clone());
        let handle = std::thread::spawn(move || {
            let _wake = wake;
            work()
        });
        // Without its input (a core driven directly, not by `run_loop`), the actor just waits.
        if let Some(inputs) = self.inputs.take() {
            while let Ok(input) = inputs.recv() {
                match input {
                    Input::Woken => break,
                    other => self.while_compacting(other),
                }
            }
            self.inputs = Some(inputs);
        }
        handle.join().map_err(|_| "The compaction worker failed".to_string())
    }

    /// An input that arrived while a compaction's work runs off the actor.
    fn while_compacting(&mut self, input: Input) {
        match input {
            Input::Op(Op::Abort, reply) => {
                self.abort();
                reply(Ok(None));
            }
            Input::Op(Op::State, reply) => reply(Ok(Some(self.state()))),
            Input::Op(Op::Prompt { .. } | Op::PromptAndWait { .. }, reply) if self.run.is_none() => {
                // AgentSession runs a prompt alongside a branch summary; the lane admits one operation at a time
                // (harness `acceptRun`: `LaneBusy`), and the navigation is the lane's operation.
                reply(Err(if self.navigating {
                    LANE_BUSY.into()
                } else {
                    PROMPT_WHILE_COMPACTING.into()
                }));
            }
            Input::Op(Op::Navigate { .. }, reply) => reply(Err(NAVIGATE_WHILE_COMPACTING.into())),
            // `compact` awaits `abort()` (the compaction, branch summary and run in progress) before it compacts.
            Input::Op(compact @ Op::Compact { .. }, reply) => {
                self.abort();
                self.deferred.push_back(Input::Op(compact, reply));
            }
            Input::ActivityTick => self.report_activity(),
            // Closing cancels the compaction; the close itself runs once it has ended.
            close @ (Input::Close(_) | Input::HandOver(_)) => {
                self.cancel_compaction();
                self.deferred.push_back(close);
            }
            other => self.deferred.push_back(other),
        }
    }

    /// `abort`: the compaction in progress is cancelled (a manual one's cancel request journaled first), then the
    /// run in progress is aborted.
    pub(super) fn abort(&mut self) {
        if let Some(signal) = self.compacting.clone()
            && !signal.aborted()
        {
            self.cancel_compaction();
            // A failed write leaves the operation running in the journal: its terminal transaction still ends it.
            if self.run.is_none() {
                let _ = compaction_op::request_cancel(&mut lock_session(&self.session), now_ms());
            }
        }
        if let Some(run) = self.run.as_mut()
            && !run.signal.aborted()
        {
            run.signal.abort();
            run.cancelled = true;
            run.journal.request_cancel(&mut lock_session(&self.session), now_ms());
            self.abort_turn();
        }
    }

    /// Cancel the compaction or branch summary in progress.
    fn cancel_compaction(&self) {
        if let Some(signal) = &self.compacting {
            signal.abort();
        }
    }

    /// The retry policy of a summary attempt (`normalizedRetryPolicy`): settings.json's `retry`, off when this
    /// session turned automatic retries off.
    pub(super) fn summary_retry(&self) -> SummaryRetry {
        SummaryRetry::from_settings(&self.runtime.config.settings["retry"], self.auto_retry)
    }

    /// The summary attempts of operation `run_id`'s task `step` (`runStructuralGeneration` and its retries): each
    /// runs off the actor; a failure whose last response is a retryable provider error (`isRetryableAssistantError`)
    /// waits `retryDelayMs` under the policy (off the actor, ended by `abort`) and is tried again until the policy's
    /// attempts run out. `journal` records the wait and the next attempt. Emits the harness's `retry_scheduled`,
    /// `retry_start` and `retry_end`. Answers the last attempt and its response.
    pub(super) fn summary_attempts<T: Send + 'static>(
        &mut self,
        signal: &AbortSignal,
        retry: SummaryRetry,
        ids: (&str, &str),
        source: Value,
        attempt: AttemptFn<T>,
        journal: &mut dyn FnMut(&mut Core, RetryStep<'_>) -> Result<(), String>,
    ) -> Attempt<T> {
        let mut scheduled = false;
        let outcome = self.attempt_loop(signal, retry, ids, &source, attempt, journal, &mut scheduled);
        // AgentSession's `onRetryFinished`: the retries ended, however they did.
        if scheduled {
            self.emit(json!({"type": "summarization_retry_finished", "lane": LANE}));
        }
        outcome
    }

    #[allow(clippy::too_many_arguments)]
    fn attempt_loop<T: Send + 'static>(
        &mut self,
        signal: &AbortSignal,
        retry: SummaryRetry,
        (run_id, step): (&str, &str),
        source: &Value,
        attempt: AttemptFn<T>,
        journal: &mut dyn FnMut(&mut Core, RetryStep<'_>) -> Result<(), String>,
        scheduled: &mut bool,
    ) -> Attempt<T> {
        let mut number = 1;
        loop {
            let run = attempt.clone();
            let (result, last) = self.off_actor(move || run()).unwrap_or_else(|error| (Err(error), None));
            if signal.aborted() {
                return (result, last);
            }
            let error = match &result {
                Err(error)
                    if number < retry.max_attempts
                        && last.as_ref().is_some_and(harness::chat::models::is_retryable) =>
                {
                    error.clone()
                }
                _ => {
                    if number > 1 {
                        let mut fields = vec![
                            ("type", json!("retry_end")),
                            ("lane", json!(LANE)),
                            ("runId", json!(run_id)),
                            ("step", json!(step)),
                            ("attempt", json!(number)),
                            ("success", json!(result.is_ok())),
                        ];
                        if let Err(error) = &result {
                            fields.push(("finalError", json!(error)));
                        }
                        self.emit(event(fields));
                    }
                    return (result, last);
                }
            };
            let delay = retry.delay_ms(number);
            let not_before = now_ms().saturating_add(i64::try_from(delay).unwrap_or(i64::MAX));
            let usage = last.as_ref().and_then(|m| serde_json::to_value(&m.usage).ok());
            if let Err(write) = journal(
                self,
                RetryStep::Wait {
                    usage,
                    error: &error,
                    not_before,
                },
            ) {
                return (Err(write), last);
            }
            *scheduled = true;
            self.emit(event(vec![
                ("type", json!("retry_scheduled")),
                ("lane", json!(LANE)),
                ("runId", json!(run_id)),
                ("step", json!(step)),
                ("attempt", json!(number + 1)),
                ("maxAttempts", json!(retry.max_attempts)),
                ("delayMs", json!(delay)),
                ("notBefore", json!(not_before)),
                ("errorMessage", json!(error)),
            ]));
            // AgentSession's `onRetryScheduled` (`retryAssistantCall`): the retry's number, the retries allowed
            // (`maxRetries`), the delay and the provider's error.
            let provider_error = last
                .as_ref()
                .and_then(|m| m.error_message.clone())
                .unwrap_or_else(|| error.clone());
            self.emit(json!({
                "type": "summarization_retry_scheduled", "lane": LANE, "attempt": number,
                "maxAttempts": retry.max_attempts - 1, "delayMs": delay, "errorMessage": provider_error,
            }));
            let cancel = signal.clone();
            let _ = self.off_actor(move || wait_until(not_before, &cancel));
            if signal.aborted() {
                return (Err(CANCELLED.into()), None);
            }
            number += 1;
            self.emit(event(vec![
                ("type", json!("retry_start")),
                ("lane", json!(LANE)),
                ("runId", json!(run_id)),
                ("step", json!(step)),
                ("attempt", json!(number)),
            ]));
            // `onRetryAttemptStart`, with the summary's source (`branchSummary`, or `compaction` and its reason).
            let mut start = json!({"type": "summarization_retry_attempt_start", "lane": LANE});
            for (key, value) in source.as_object().into_iter().flatten() {
                start[key] = value.clone();
            }
            self.emit(start);
            if let Err(write) = journal(self, RetryStep::Retry) {
                return (Err(write), None);
            }
        }
    }

    /// A manual compaction: `compaction_start`, the journaled operation (the summary, cancellable), the entry, `compaction_end`; answers Pi's `CompactionResult` fields of the compaction entry.
    pub(super) fn compact(&mut self, instructions: Option<&str>) -> Result<Value, String> {
        self.idle()?;
        let lane = json!(LANE);
        self.emit(event(vec![
            ("type", json!("compaction_start")),
            ("lane", lane.clone()),
            ("reason", json!("manual")),
        ]));
        let signal = AbortSignal::new();
        self.compacting = Some(signal.clone());
        let outcome = self.compact_now(instructions, &signal);
        let id = match outcome {
            Ok(id) => id,
            Err(error) => {
                // AgentSession clears its compaction state before `compaction_end`.
                self.compacting = None;
                let cancelled = error == CANCELLED;
                let mut fields = vec![
                    ("type", json!("compaction_end")),
                    ("lane", lane),
                    ("reason", json!("manual")),
                    ("status", json!(if cancelled { "aborted" } else { "failed" })),
                ];
                if !cancelled {
                    fields.push(("error", json!({"message": error})));
                }
                self.emit(event(fields));
                return Err(error);
            }
        };
        self.announce();
        self.compacting = None;
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
    /// path after an (empty-conversation) summary, as the native harness does. A cancelled compaction ends its
    /// operation `aborted` with [`CANCELLED`].
    fn compact_now(&mut self, custom_instructions: Option<&str>, signal: &AbortSignal) -> Result<String, String> {
        let prepared = self.prepare_compaction(compaction::CompactionSettings::default())?;
        let mut op = CompactionOp::begin(
            &mut lock_session(&self.session),
            &prepared.preparation,
            &prepared.retained_tail,
            custom_instructions,
        )?;
        if signal.aborted() {
            op.abort(&mut lock_session(&self.session))?;
            return Err(CANCELLED.into());
        }
        let retry = self.summary_retry();
        op.request(&mut lock_session(&self.session), retry)?;
        let summarizer = self.summarizer();
        let (preparation, instructions, cancel) = (
            prepared.preparation.clone(),
            custom_instructions.map(str::to_string),
            signal.clone(),
        );
        let attempt: AttemptFn<compaction::CompactionResult> =
            Arc::new(move || summarizer.run(&preparation, instructions.as_deref(), &cancel));
        let ids = (op.operation_id().to_string(), op.task_id().to_string());
        let mut journal = |core: &mut Core, step: RetryStep<'_>| {
            let mut session = lock_session(&core.session);
            match step {
                RetryStep::Wait {
                    usage,
                    error,
                    not_before,
                } => op.retry_wait(&mut session, usage.as_ref(), error, not_before),
                RetryStep::Retry => op.retry(&mut session),
            }
        };
        let (result, _) = self.summary_attempts(
            signal,
            retry,
            (&ids.0, &ids.1),
            json!({"source": "compaction", "reason": "manual"}),
            attempt,
            &mut journal,
        );
        if signal.aborted() {
            op.abort(&mut lock_session(&self.session))?;
            return Err(CANCELLED.into());
        }
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                op.fail(&mut lock_session(&self.session), &error)?;
                return Err(error);
            }
        };
        let usage = serde_json::to_value(&result.usage).map_err(|e| e.to_string())?;
        op.record_usage(&mut lock_session(&self.session), &usage)?;
        let mut entry = Map::new();
        entry.insert("type".into(), "compaction".into());
        entry.insert("summary".into(), result.summary.clone().into());
        entry.insert("retainedTail".into(), Value::Array(prepared.retained_tail));
        entry.insert("tokensBefore".into(), result.tokens_before.into());
        entry.insert(
            "details".into(),
            json!({"readFiles": result.read_files, "modifiedFiles": result.modified_files}),
        );
        entry.insert("usage".into(), usage);
        entry.insert("fromHook".into(), false.into());
        op.complete(&mut lock_session(&self.session), entry)
    }
}
