//! Delegated children as durable tasks: the one journal behind `rlm.spawn` / `rlm.collect` / `rlm.results` /
//! `rlm.cancel` (and, with `keep`, `rlm.ask`).
//!
//! A child is a task of kind `ultron.child`. An ordinary child is owned by the cell task that spawned it, so the
//! spawning cell holds `completing` (and its run waits) until the child is terminal, and aborting the cell or its run
//! aborts the child (structured concurrency, spec §5.5). A persistent child (`keep=True`) is a background task of the
//! parent's conversation: it stays alive across restarts holding its conversation, and follow-up questions go to it
//! through its inbox. Each child owns its own conversation, created in the child's first commit with the brief as its
//! input submission. A per-child timeout is a durable deadline in the checkpoint: after a restart the child waits
//! only for the time that is left, and a child past its deadline ends `failed`, which aborts its conversation's work.

use std::sync::{Arc, LazyLock, OnceLock};

use durable::{
    ConversationId, ConversationOwnership, EntryDraft, EntryId, SubmissionId, SubmissionStatus, TaskDefinition, TaskId,
    TaskOptions, TaskOutcome, TaskOutcomeError, TaskOwnership, TaskRecord, TaskState, TaskStatus, Tx,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::submissions::{Submissions, place_initial_input};
use super::util::{from_value, to_value};
use super::{AgentChange, ModelRef, Services, configure, create_documents};
use crate::scheduler::{Cancel, Ctx, HarnessError, NextState, Result, Task, TaskRuntime};

pub const CHILD_KIND: &str = "ultron.child";

/// What spawns a child.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChildInput {
    pub name: String,
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// A persistent child: it stays alive after its first answer and takes follow-up questions.
    #[serde(default)]
    pub keep: bool,
    /// The child's thinking level (`rlm.spawn(thinking=...)`), set on its conversation's agent when it starts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    /// Where the child's lane works when not in the session's directory (a worktree child), and the extra
    /// environment of its processes: kept with the task so a restarted worker resumes it there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<(String, String)>,
    /// The run of a typed agent (`agents.*`) rather than an `rlm.spawn` subagent.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub agent: bool,
    /// The usage root the child's model turns are charged to (the root turn that started it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_root: Option<String>,
    /// `rlm.spawn(fork=True)`: the child's conversation is its parent's, forked at a visible entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork: Option<ChildFork>,
    /// `rlm.spawn(fork=True)` from a conversation that does not keep its history in the runtime (the root's is the
    /// session transcript): the messages the child's conversation starts with, before its brief.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<Value>,
}

/// Where a forked child's conversation starts: a visible entry of its parent's conversation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChildFork {
    pub conversation: ConversationId,
    pub entry: EntryId,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum ChildCheckpoint {
    Start,
    /// Waiting for the answer to `submission` in the child's conversation; `until` is the durable deadline.
    Wait {
        conversation: ConversationId,
        submission: SubmissionId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        until: Option<u64>,
    },
    /// A persistent child after its first answer: alive, holding its conversation, until cancelled.
    Idle {
        conversation: ConversationId,
        result: Value,
    },
}

static DEFINITION: LazyLock<TaskDefinition> =
    LazyLock::new(|| TaskDefinition::new(CHILD_KIND, 1, |_| to_value(&ChildCheckpoint::Start)));

/// Late-bound services a child task needs from its Harness.
#[derive(Default)]
pub(crate) struct ChildServices {
    pub(crate) submissions: OnceLock<Arc<Submissions>>,
}

fn result(status: &str, value: Option<Value>, error: Option<String>) -> Value {
    let mut result = json!({ "status": status, "verification": "unverified" });
    if let Some(value) = value {
        result["value"] = value;
    }
    if let Some(error) = error {
        result["error"] = json!(error);
    }
    result
}

fn checkpoint_of(task: &TaskRecord) -> Result<ChildCheckpoint> {
    match &task.state {
        TaskState::Pending { checkpoint }
        | TaskState::Running { checkpoint }
        | TaskState::Waiting { checkpoint, .. } => from_value(checkpoint.clone(), "child checkpoint"),
        _ => Err(HarnessError::msg("A terminal child has no checkpoint")),
    }
}

/// The built-in child task.
pub(crate) fn child_task(services: Arc<Services>) -> Task {
    let (start, wait) = (services.clone(), services);
    Task::define(CHILD_KIND, 1, |input| (DEFINITION.initial)(input))
        .phase("start", move |task, runtime| start_phase(&start, task, runtime))
        .phase("wait", move |task, runtime| wait_phase(&wait, task, runtime))
        .phase("idle", |_, runtime| Err(runtime.signal().wait()))
        .abort(|task, runtime| {
            let conversation = match checkpoint_of(task)? {
                ChildCheckpoint::Wait { conversation, .. } | ChildCheckpoint::Idle { conversation, .. } => {
                    Some(conversation.0)
                }
                ChildCheckpoint::Start => None,
            };
            let reason = if task.abort_reason.is_some() {
                "Interrupted by a restart"
            } else {
                "Cancelled"
            };
            runtime.commit(&runtime.ctx(), |_, _| {
                Ok(Some(NextState::Terminal {
                    outcome: TaskOutcome::Aborted {
                        reason: None,
                        result: Some(json!({
                            "result": result("cancelled", None, Some(reason.into())),
                            "conversation": conversation,
                        })),
                    },
                }))
            })
        })
        .build()
}

fn start_phase(_services: &Services, task: &TaskRecord, runtime: &TaskRuntime) -> Result<()> {
    let input: ChildInput = from_value(task.input.clone(), "child input")?;
    let parent = runtime.conversation_id();
    let model = match input.model.clone() {
        Some(model) => Some(model),
        None => super::generation::read_agent(runtime)?.model,
    };
    let task_id = runtime.task_id();
    let now = runtime.now()?;
    runtime.commit(&runtime.ctx(), |tx, _| {
        let _ = parent;
        // A forked child's conversation starts as its parent's history up to the fork entry.
        let conversation = match input.fork {
            Some(fork) => {
                let id = tx
                    .fork_conversation(fork.conversation, fork.entry, ConversationOwnership::Task(task_id))?
                    .id;
                create_documents(tx, id, true)?;
                id
            }
            None => {
                let id = tx.create_conversation(ConversationOwnership::Task(task_id))?.id;
                create_documents(tx, id, false)?;
                id
            }
        };
        if let Some(model) = &model {
            configure(tx, conversation, &AgentChange::model(model.clone()))?;
        }
        if let Some(level) = &input.thinking {
            let change = AgentChange {
                model: None,
                thinking_level: Some(Some(level.clone())),
            };
            configure(tx, conversation, &change)?;
        }
        for message in &input.history {
            let kind = match message["role"].as_str() {
                Some("assistant") => super::entries::ASSISTANT,
                Some("toolResult") => super::entries::TOOL_RESULT,
                _ => super::entries::USER,
            };
            tx.append_entry(
                conversation,
                EntryDraft {
                    model: Some(vec![message.clone()]),
                    ..EntryDraft::new(kind)
                },
            )?;
        }
        let submission = place_initial_input(tx, conversation, &json!(input.prompt), now)?;
        // The registry learns what the child holds: its brief is its first work.
        let mut brief = input.prompt.clone();
        if brief.chars().count() > 200 {
            brief = brief.chars().take(200).collect::<String>() + "…";
        }
        let work = serde_json::Map::from_iter([("work".to_owned(), json!(brief))]);
        super::swarm::register(
            tx,
            &input.name,
            Some(conversation),
            Some(task_id.0.to_string()),
            &work,
            now,
        )?;
        Ok(Some(NextState::Running {
            checkpoint: to_value(&ChildCheckpoint::Wait {
                conversation,
                submission,
                until: input.timeout_ms.map(|timeout| now + timeout),
            }),
        }))
    })
}

/// The answer text of a settled submission.
fn answer_text(tx: &mut Tx<'_>, answer: durable::EntryId) -> Result<Value> {
    let Some(entry) = tx.entry(answer)? else {
        return Ok(Value::Null);
    };
    let message = entry
        .model
        .and_then(|model| model.into_iter().next())
        .unwrap_or(Value::Null);
    let text: Vec<String> = message["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block["type"] == "text")
        .filter_map(|block| block["text"].as_str().map(str::to_owned))
        .collect();
    Ok(json!(text.join("\n")))
}

fn wait_phase(services: &Services, task: &TaskRecord, runtime: &TaskRuntime) -> Result<()> {
    let ChildCheckpoint::Wait {
        conversation,
        submission,
        until,
    } = checkpoint_of(task)?
    else {
        return Err(HarnessError::msg("wait phase without a submission"));
    };
    let input: ChildInput = from_value(task.input.clone(), "child input")?;
    let submissions = services
        .children
        .submissions
        .get()
        .cloned()
        .ok_or_else(|| HarnessError::msg("Child tasks need an open ChatHarness"))?;
    // The deadline is durable: a timer on the Harness clock cancels the wait when it is reached.
    let deadline = Cancel::new();
    if let Some(until) = until {
        let (timer, sleeper, done) = (deadline.clone(), runtime.clone(), runtime.signal().clone());
        std::thread::spawn(move || {
            if sleeper.sleep(until, &Ctx::with_signal(&done)).is_ok() {
                timer.cancel(HarnessError::msg("deadline"));
            }
        });
    }
    let linked = Cancel::any(&[runtime.signal(), &deadline]);
    let ctx = Ctx::with_signal(linked.signal());
    let mut settled = submissions.wait(submission, &ctx);
    // A message steered into the child while it answered continues it: the final boundary that settles the brief
    // starts a run for what it placed, and the child answers when that run (and any after it) settles, as an agent
    // loop takes steering before it ends.
    let mut waited = submission;
    while let Ok(record) = &settled
        && record.status == SubmissionStatus::Done
    {
        let live = runtime.snapshot(&super::docs::LIVE, super::util::owner(conversation))?;
        let next = live
            .as_ref()
            .and_then(|live| live["run"]["inputs"].as_array()?.last()?.as_u64())
            .map(SubmissionId)
            .filter(|next| *next != waited);
        let Some(next) = next else { break };
        waited = next;
        settled = submissions.wait(next, &ctx);
    }
    runtime.signal().check()?;
    let settled = match settled {
        Ok(settled) => settled,
        Err(_) if deadline.is_cancelled() => {
            let timeout = input.timeout_ms.unwrap_or_default();
            let error = format!("Subagent {} timed out after {timeout} ms", input.name);
            // Ending `failed` records cancellation intent: the child's conversation work is aborted.
            return runtime.commit(&runtime.ctx(), |_, _| {
                Ok(Some(NextState::Terminal {
                    outcome: TaskOutcome::Failed {
                        error: TaskOutcomeError {
                            message: error.clone(),
                            detail: Some(json!({ "reason": "timeout" })),
                        },
                        result: Some(json!({
                            "result": result("cancelled", None, Some(error.clone())),
                            "conversation": conversation.0,
                        })),
                    },
                }))
            });
        }
        Err(error) => return Err(error),
    };
    runtime.commit(&runtime.ctx(), |tx, _| {
        let child_result = match (settled.status, settled.answer) {
            (SubmissionStatus::Done, Some(answer)) => result("succeeded", Some(answer_text(tx, answer)?), None),
            _ => result(
                "failed",
                None,
                Some(format!(
                    "Subagent {} ended without an answer: {}",
                    input.name,
                    settled
                        .detail
                        .as_ref()
                        .and_then(Value::as_str)
                        .or(settled.reason.as_deref())
                        .unwrap_or("unanswered")
                )),
            ),
        };
        if input.keep {
            return Ok(Some(NextState::Running {
                checkpoint: to_value(&ChildCheckpoint::Idle {
                    conversation,
                    result: child_result,
                }),
            }));
        }
        let receipt = json!({ "result": child_result, "conversation": conversation.0 });
        Ok(Some(NextState::Terminal {
            outcome: TaskOutcome::Completed { result: receipt },
        }))
    })
}

/// Spawn a child in `tx`: owned by `cell` (an ordinary child) or, with `keep`, a background task of `conversation`.
pub fn spawn_child(
    tx: &mut Tx<'_>,
    conversation: ConversationId,
    cell: Option<TaskId>,
    input: &ChildInput,
) -> Result<TaskId> {
    let options = match (input.keep, cell) {
        (false, Some(cell)) => TaskOptions {
            ownership: TaskOwnership::Task(cell),
            ..TaskOptions::default()
        },
        _ => TaskOptions {
            ownership: TaskOwnership::Conversation,
            conversation_id: Some(conversation),
            background: true,
            ..TaskOptions::default()
        },
    };
    Ok(tx.create_task(&DEFINITION, to_value(input), options)?)
}

/// What `collect` and `results` report for one child: `{ id, result }`, plus its conversation.
pub fn child_report(record: &TaskRecord) -> Option<Value> {
    let id = record.id.0.to_string();
    let receipt = match &record.state {
        TaskState::Terminal { outcome } => match outcome {
            TaskOutcome::Completed { result } => result.clone(),
            TaskOutcome::Failed {
                result: Some(result), ..
            }
            | TaskOutcome::Aborted {
                result: Some(result), ..
            } => result.clone(),
            TaskOutcome::Failed { error, .. } => {
                json!({ "result": result("failed", None, Some(error.message.clone())) })
            }
            TaskOutcome::Aborted { .. } => json!({ "result": result("cancelled", None, None) }),
            TaskOutcome::Faulted { error } => {
                json!({ "result": result("failed", None, Some(error.message.clone())) })
            }
            TaskOutcome::Orphaned { reason } => json!({ "result": result("interrupted", None, Some(reason.clone())) }),
        },
        // A persistent child reports its first answer while it stays alive.
        TaskState::Running { checkpoint } | TaskState::Pending { checkpoint } if checkpoint["phase"] == "idle" => {
            json!({ "result": checkpoint["result"], "conversation": checkpoint["conversation"] })
        }
        _ => return None,
    };
    let mut report = json!({ "id": id, "result": receipt["result"] });
    if !receipt["conversation"].is_null() {
        report["conversation"] = receipt["conversation"].clone();
    }
    Some(report)
}

/// Whether the child has an answer to report (terminal, or a persistent child idle after its first answer).
pub fn child_done(record: &TaskRecord) -> bool {
    record.state.status() == TaskStatus::Terminal || child_report(record).is_some()
}
