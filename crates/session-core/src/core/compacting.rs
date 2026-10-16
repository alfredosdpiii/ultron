//! A compaction in progress, off the actor's thread: the summary request and the extensions' compaction hooks run
//! on a worker thread while the actor keeps serving the operations that must not wait for them. `abort` cancels the
//! compaction at once (manual `/compact`, threshold and overflow compactions alike) and `state` reports
//! `isCompacting`, as AgentSession does (`abortCompaction`, `isCompacting`); every other input waits for the
//! compaction, in order, as it did while the actor was blocked. A prompt with no run in progress is refused, as
//! TypeScript refuses one during a manual compaction; a tree navigation is refused as `navigateTree` refuses it
//! while `isCompacting`; a `compact` aborts what is in progress first (`compact` awaits `abort()`), then runs. A
//! tree navigation's branch summary runs the same way (core::navigate).
//!
//! The `session_before_compact` hooks get the compaction's `signal`: `abort` cancels their dispatch at once
//! (`notifications/cancelled` to the extension, its late answer ignored; `extensions::Router::emit_cancellable`).
//!
//! Extensions see the compaction as AgentSession shows it to them: `session_before_compact` (`{ preparation,
//! branchEntries, customInstructions?, reason, willRetry }`) before the summary request, whose `cancel` cancels the
//! compaction and whose `compaction` replaces the summary (the entry is `fromHook`, no request is made); then
//! `session_compact` (`{ compactionEntry, fromExtension, reason, willRetry }`) once the entry is committed, or
//! `session_compact_failed` (`{ reason, errorMessage?, aborted, willRetry, fromExtension }`) when it is not.

use super::*;
use crate::compaction_op::{self, SummaryRetry};
use extensions::ExtensionHost;

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

/// What the `session_before_compact` hooks decided.
pub(super) enum HookDecision {
    /// No hook, or none that cancelled or supplied a compaction: summarize.
    Summarize,
    /// A hook cancelled the compaction.
    Cancel,
    /// A hook supplied the compaction (`{ summary, firstKeptEntryId, tokensBefore, usage?, details? }`).
    Compaction(Value),
}

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

/// JavaScript truthiness of a hook result's member.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|n| n != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// `CompactionPreparation` as an extension receives it.
fn preparation_json(preparation: &compaction::CompactionPreparation) -> Value {
    let ops = &preparation.file_ops;
    let mut value = json!({
        "firstKeptEntryId": preparation.first_kept_entry_id,
        "messagesToSummarize": preparation.messages_to_summarize,
        "turnPrefixMessages": preparation.turn_prefix_messages,
        "isSplitTurn": preparation.is_split_turn,
        "tokensBefore": preparation.tokens_before,
        "fileOps": {"read": ops.read, "written": ops.written, "edited": ops.edited},
        "settings": {
            "enabled": preparation.settings.enabled, "reserveTokens": preparation.settings.reserve_tokens,
            "keepRecentTokens": preparation.settings.keep_recent_tokens,
        },
    });
    if let Some(summary) = &preparation.previous_summary {
        value["previousSummary"] = json!(summary);
    }
    value
}

impl Core {
    /// The summary call as this session makes it now.
    pub(super) fn summarizer(&self) -> Summarizer {
        Summarizer {
            provider: self.provider.clone(),
            model: self.model.clone(),
            api_key: self.api_key(),
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

    /// Cancel the compaction or branch summary in progress: its signal, and the hooks' dispatch in flight.
    fn cancel_compaction(&self) {
        if let Some(signal) = &self.compacting {
            signal.abort();
        }
        if let Some(hook) = &self.hook {
            hook.cancel(None);
        }
    }

    fn has_hook(&self, event: &str) -> bool {
        self.extensions_for(event).is_some()
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

    /// The extension host when an extension handles `event`.
    fn extensions_for(&self, event: &str) -> Option<Arc<ExtensionHost>> {
        self.runtime
            .extensions
            .clone()
            .filter(|host| host.router().has_handlers(event))
    }

    /// `session_before_compact` for `preparation` (`reason`: manual, threshold or overflow).
    pub(super) fn before_compact(
        &mut self,
        preparation: &compaction::CompactionPreparation,
        custom_instructions: Option<&str>,
        reason: &str,
        will_retry: bool,
    ) -> HookDecision {
        if !self.has_hook("session_before_compact") {
            return HookDecision::Summarize;
        }
        let branch: Vec<Value> = self.pi().branch_entries(None).into_iter().cloned().collect();
        let mut event = json!({
            "type": "session_before_compact", "preparation": preparation_json(preparation), "branchEntries": branch,
            "reason": reason, "willRetry": will_retry,
        });
        if let Some(instructions) = custom_instructions {
            event["customInstructions"] = json!(instructions);
        }
        match self.cancellable_hook(event) {
            // Given up by an abort: the compaction's signal is aborted, so it ends cancelled.
            Err(extensions::Cancelled(_)) => HookDecision::Cancel,
            Ok(result) => Self::decision(result),
        }
    }

    /// A `session_before_*` event off the actor with the operation's `signal`: `abort` gives the dispatch up
    /// (`Err`). No handler, or a dispatch that failed, answers `None`.
    pub(super) fn cancellable_hook(&mut self, event: Value) -> Result<Option<Value>, extensions::Cancelled> {
        let Some(host) = self.extensions_for(event["type"].as_str().unwrap_or_default()) else {
            return Ok(None);
        };
        let hook = extensions::CancelToken::new();
        self.hook = Some(hook.clone());
        let result = self.off_actor(move || host.router().emit_cancellable(&event, &hook));
        self.hook = None;
        result.unwrap_or(Ok(None))
    }

    /// An observe-only event off the actor (`session_compact`, `session_compact_failed`, `session_tree`).
    pub(super) fn observe_hook(&mut self, event: Value) {
        if let Some(host) = self.extensions_for(event["type"].as_str().unwrap_or_default()) {
            let _ = self.off_actor(move || host.router().emit(&event));
        }
    }

    /// Run `hooks` with `isCompacting` still true: AgentSession emits `session_compact` (and an automatic
    /// compaction's `session_compact_failed`) before it clears its abort controller.
    pub(super) fn still_compacting(&mut self, hooks: impl FnOnce(&mut Self)) {
        let previous = self.compacting.replace(AbortSignal::new());
        hooks(self);
        self.compacting = previous;
    }

    /// What a `session_before_compact` result decides.
    fn decision(result: Option<Value>) -> HookDecision {
        match result {
            Some(result) if truthy(result.get("cancel")) => HookDecision::Cancel,
            Some(result) if result["compaction"].is_object() => HookDecision::Compaction(result["compaction"].clone()),
            _ => HookDecision::Summarize,
        }
    }

    /// `session_compact`: the committed compaction entry `id`.
    pub(super) fn after_compact(&mut self, id: &str, from_extension: bool, reason: &str, will_retry: bool) {
        if !self.has_hook("session_compact") {
            return;
        }
        let entry = self.pi().entry(id).cloned().unwrap_or(Value::Null);
        self.observe_hook(json!({
            "type": "session_compact", "compactionEntry": entry, "fromExtension": from_extension, "reason": reason,
            "willRetry": will_retry,
        }));
    }

    /// `session_compact_failed`: the compaction ended without an entry (`error`: why, unless it was cancelled).
    pub(super) fn compact_failed(&mut self, reason: &str, error: Option<&str>, from_extension: bool) {
        let mut event = json!({
            "type": "session_compact_failed", "reason": reason, "aborted": error.is_none(), "willRetry": false,
            "fromExtension": from_extension,
        });
        if let Some(error) = error {
            event["errorMessage"] = json!(error);
        }
        self.observe_hook(event);
    }

    /// The compaction entry for a hook's `compaction` (`fromHook`), keeping the branch from its `firstKeptEntryId`
    /// (the preparation's when it names no entry of the branch); with the usage it reported.
    pub(super) fn hook_entry(
        &self,
        compaction: &Value,
        prepared: &auto_compact::Prepared,
    ) -> (Map<String, Value>, Option<Value>) {
        let retained_tail = compaction["firstKeptEntryId"]
            .as_str()
            .and_then(|first| self.retained_tail_from(first))
            .unwrap_or_else(|| prepared.retained_tail.clone());
        let mut entry = Map::new();
        entry.insert("type".into(), "compaction".into());
        entry.insert(
            "summary".into(),
            compaction["summary"].as_str().unwrap_or_default().into(),
        );
        entry.insert("retainedTail".into(), Value::Array(retained_tail));
        entry.insert(
            "tokensBefore".into(),
            compaction["tokensBefore"]
                .as_u64()
                .unwrap_or(prepared.preparation.tokens_before)
                .into(),
        );
        if let Some(details) = compaction.get("details").filter(|d| !d.is_null()) {
            entry.insert("details".into(), details.clone());
        }
        let usage = compaction.get("usage").filter(|u| u.is_object()).cloned();
        if let Some(usage) = &usage {
            entry.insert("usage".into(), usage.clone());
        }
        entry.insert("fromHook".into(), true.into());
        (entry, usage)
    }

    /// A manual compaction: `compaction_start`, the journaled operation (the hooks, then the summary, both
    /// cancellable), the entry, `compaction_end`; answers Pi's `CompactionResult` fields of the compaction entry.
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
        let (id, from_hook) = match outcome {
            Ok(done) => done,
            Err((error, from_hook)) => {
                // AgentSession clears its compaction state before `compaction_end` and `session_compact_failed`.
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
                let failure = format!("Compaction failed: {error}");
                self.compact_failed("manual", (!cancelled).then_some(failure.as_str()), from_hook);
                return Err(error);
            }
        };
        self.announce();
        // `session_compact` runs while the compaction still counts (`isCompacting`), `compaction_end` after.
        self.after_compact(&id, from_hook, "manual", false);
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

    /// Summarize the current path (or take a hook's compaction) and commit the native compaction entry (its
    /// retained tail is the kept messages) through a journaled compaction operation ([`CompactionOp`]); returns
    /// its id and whether a hook supplied it. Nothing to cut keeps the whole path after an (empty-conversation)
    /// summary, as the native harness does. A cancelled compaction ends its operation `aborted` with
    /// [`CANCELLED`]; the error says whether a hook's compaction failed.
    fn compact_now(
        &mut self,
        custom_instructions: Option<&str>,
        signal: &AbortSignal,
    ) -> Result<(String, bool), (String, bool)> {
        let failed = |error: String| (error, false);
        let prepared = self
            .prepare_compaction(compaction::CompactionSettings::default())
            .map_err(failed)?;
        let mut op = CompactionOp::begin(
            &mut lock_session(&self.session),
            &prepared.preparation,
            &prepared.retained_tail,
            custom_instructions,
        )
        .map_err(failed)?;
        let decision = self.before_compact(&prepared.preparation, custom_instructions, "manual", false);
        if signal.aborted() || matches!(decision, HookDecision::Cancel) {
            op.abort(&mut lock_session(&self.session)).map_err(failed)?;
            return Err((CANCELLED.into(), false));
        }
        if let HookDecision::Compaction(compaction) = decision {
            let (entry, usage) = self.hook_entry(&compaction, &prepared);
            let id = op
                .complete_from_hook(&mut lock_session(&self.session), entry, usage)
                .map_err(|error| (error, true))?;
            return Ok((id, true));
        }
        let retry = self.summary_retry();
        op.request(&mut lock_session(&self.session), retry).map_err(failed)?;
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
            op.abort(&mut lock_session(&self.session)).map_err(failed)?;
            return Err((CANCELLED.into(), false));
        }
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                op.fail(&mut lock_session(&self.session), &error).map_err(failed)?;
                return Err(failed(error));
            }
        };
        let usage = serde_json::to_value(&result.usage).map_err(|e| failed(e.to_string()))?;
        op.record_usage(&mut lock_session(&self.session), &usage)
            .map_err(failed)?;
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
        let id = op.complete(&mut lock_session(&self.session), entry).map_err(failed)?;
        Ok((id, false))
    }
}
