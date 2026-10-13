//! Port of upstream Pi Durable `test/harness-ownership.test.ts` ("ownership": structured concurrency, cascades,
//! idle scopes, holds) and `test/harness-cancellation-barrier.test.ts`, each case run against Memory, SQLite and
//! JSONL storage. Queued inputs queue in `pi.inbox` behind a conversation marked busy, as upstream's `busy()` does;
//! the cases about tools, subagents and supervisors through conversation handles are not ported.

mod support;

use durable::{
    AbortReason, ConversationId, ConversationOwnership, JoinPolicy, Session, StorageError, StorageWrite, SubmissionId,
    SubmissionType, TaskId, TaskOptions, TaskOwnership, TaskRecord, TaskState, TaskStatus, Tx,
};
use harness::chat::{QueueModes, SubmissionDraft, admit_submission};
use harness::scheduler::{
    AbortResult, Cancel, Conversation, Ctx, Harness, HarnessError, NextState, Task, TaskInspectionState,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use support::*;

/// Named gates a held task waits on, each opened with an ending, and how often each named task ran.
#[derive(Clone, Default)]
struct Gates {
    gates: Arc<Mutex<HashMap<String, Cancel>>>,
    runs: Arc<Mutex<HashMap<String, usize>>>,
}

impl Gates {
    fn gate(&self, name: &str) -> Cancel {
        self.gates.lock().unwrap().entry(name.to_owned()).or_default().clone()
    }

    /// Open `name` with "completed" or "failed".
    fn open(&self, name: &str, ending: &str) {
        self.gate(name).cancel(HarnessError::msg(ending));
    }

    fn runs(&self, name: &str) -> usize {
        self.runs.lock().unwrap().get(name).copied().unwrap_or(0)
    }
}

/// A task that holds until its named gate opens or it is aborted. With `slowAbort`, its abort handler first waits
/// for the gate `abort.<name>`.
fn hold(gates: &Gates) -> Task {
    let (g1, g2) = (gates.clone(), gates.clone());
    Task::define("test.hold", 1, |_| json!({ "phase": "hold" }))
        .phase("hold", move |task, runtime| {
            let name = task.input["name"].as_str().unwrap().to_owned();
            *g1.runs.lock().unwrap().entry(name.clone()).or_default() += 1;
            let gate = g1.gate(&name);
            let either = Cancel::any(&[&gate, runtime.signal()]);
            either.signal().wait();
            let Some(ending) = gate.reason() else {
                return Err(runtime.signal().reason().unwrap());
            };
            runtime.commit(&runtime.ctx(), |_, _| {
                Ok(Some(NextState::Terminal {
                    outcome: if ending.message() == "completed" {
                        durable::TaskOutcome::Completed { result: Value::Null }
                    } else {
                        durable::TaskOutcome::Failed {
                            error: durable::TaskOutcomeError {
                                message: "gate failed".into(),
                                detail: None,
                            },
                            result: None,
                        }
                    },
                }))
            })
        })
        .abort(move |task, runtime| {
            if task.input["slowAbort"] == json!(true) {
                // Upstream waits for the gate only; close then joins it only if its reservation won the race with
                // close. Waking on the signal too keeps close from waiting on a gate nobody opens.
                let gate = g2.gate(&format!("abort.{}", task.input["name"].as_str().unwrap()));
                Cancel::any(&[&gate, runtime.signal()]).signal().wait();
                if !gate.is_cancelled() {
                    return Err(runtime.signal().reason().unwrap());
                }
            }
            runtime.commit(&runtime.ctx(), |_, _| {
                Ok(Some(NextState::Terminal {
                    outcome: durable::TaskOutcome::Aborted {
                        reason: None,
                        result: None,
                    },
                }))
            })
        })
        .build()
}

/// Waits on the tasks in its input, then completes; its abort handler ends it `aborted`.
fn waiter() -> Task {
    Task::define("test.waiter", 1, |_| json!({ "phase": "wait" }))
        .phase("wait", |task, runtime| {
            let on: Vec<TaskId> = serde_json::from_value(task.input["on"].clone()).unwrap();
            runtime.commit(&runtime.ctx(), |_, _| {
                Ok(Some(NextState::Waiting {
                    checkpoint: json!({ "phase": "done" }),
                    on,
                    policy: JoinPolicy::AllSettled,
                }))
            })
        })
        .phase("done", |_, runtime| {
            runtime.commit(&runtime.ctx(), |_, _| Ok(completed(Value::Null)))
        })
        .abort(|_, runtime| {
            runtime.commit(&runtime.ctx(), |_, _| {
                Ok(Some(NextState::Terminal {
                    outcome: durable::TaskOutcome::Aborted {
                        reason: None,
                        result: None,
                    },
                }))
            })
        })
        .build()
}

/// Never registered: aborting it can only orphan it.
fn unregistered() -> Task {
    Task::define("test.unregistered", 1, |_| json!({ "phase": "hold" })).build()
}

struct Tree {
    owner: TaskId,
    child: ConversationId,
    inner: TaskId,
}

#[derive(Default, Clone, Copy)]
struct TreeOptions {
    background: bool,
    slow_inner: bool,
    slow_owner: bool,
}

fn hold_input(name: &str, slow: bool) -> Value {
    if slow {
        json!({ "name": name, "slowAbort": true })
    } else {
        json!({ "name": name })
    }
}

/// In `parent`, stage task `owner` and a conversation it owns holding task `inner`, in one commit.
fn owned_child(parent: &Conversation, hold: &Task, name: &str, options: TreeOptions) -> Tree {
    parent
        .commit(|tx| {
            let owner = tx.create_task(
                hold.definition(),
                hold_input(name, options.slow_owner),
                TaskOptions {
                    background: options.background,
                    ..TaskOptions::default()
                },
            )?;
            let child = tx.create_conversation(ConversationOwnership::Task(owner))?;
            let inner = tx.create_task(
                hold.definition(),
                hold_input(&format!("{name}.inner"), options.slow_inner),
                TaskOptions {
                    conversation_id: Some(child.id),
                    ..TaskOptions::default()
                },
            )?;
            Ok(Tree {
                owner,
                child: child.id,
                inner,
            })
        })
        .unwrap()
}

struct Env {
    opened: Opened,
    root: Conversation,
    gates: Gates,
    hold: Task,
}

impl Env {
    fn harness(&self) -> &Harness {
        &self.opened.harness
    }

    fn status(&self, id: TaskId) -> TaskStatus {
        self.harness().get_task(id).unwrap().unwrap().state.status()
    }

    fn outcome_status(&self, id: TaskId) -> String {
        let record = self.harness().wait_for_task(id, &bg()).unwrap();
        outcome(&record)["status"].as_str().unwrap().to_owned()
    }

    fn conversation(&self, id: ConversationId) -> Conversation {
        self.harness().conversation(id).unwrap().unwrap()
    }

    fn tree(&self, parent: &Conversation, name: &str, options: TreeOptions) -> Tree {
        owned_child(parent, &self.hold, name, options)
    }
}

fn open_env(storage: Controlled, gates: Gates) -> Env {
    let hold = hold(&gates);
    let opened = open_tasks(storage.clone(), vec![hold.clone(), waiter()], OpenOptions::default()).unwrap();
    let root = opened.harness.root().unwrap();
    opened.harness.resume().unwrap();
    Env {
        opened,
        root,
        gates,
        hold,
    }
}

/// Mark a conversation busy with `task` standing in for its run (upstream `busy()`), so submissions queue.
fn busy(conversation: &Conversation, task: TaskId) {
    conversation
        .commit(|tx| {
            let live = tx.doc(
                &harness::chat::docs::LIVE,
                durable::Owner::Conversation(conversation.id),
            )?;
            live.set(
                &[chord_delta::Seg::from("run")],
                json!({ "taskId": task.0, "inputs": [] }),
            )
            .map_err(|error| HarnessError::Session(durable::SessionError::Delta(error)))?;
            Ok(())
        })
        .unwrap();
}

/// Submit to a busy conversation: the submission queues in its `pi.inbox` (upstream `conversation.submit()`).
fn queue(conversation: &Conversation, kind: SubmissionType) -> SubmissionId {
    let draft = match kind {
        SubmissionType::Input => SubmissionDraft::input("later"),
        SubmissionType::Write => SubmissionDraft::write(json!({ "kind": "note" })),
    };
    conversation
        .commit(|tx| admit_submission(tx, conversation.id, &draft, 0, QueueModes::default()))
        .unwrap()
}

fn submission(harness: &Harness, id: SubmissionId) -> Value {
    js(&harness.submission(id).unwrap().unwrap())
}

const NONE: TreeOptions = TreeOptions {
    background: false,
    slow_inner: false,
    slow_owner: false,
};
const BACKGROUND: TreeOptions = TreeOptions {
    background: true,
    slow_inner: false,
    slow_owner: false,
};

#[test]
fn harness_ownership_keeps_a_conversation_busy_while_its_owned_subtree_is_live_holding_the_completed_owner() {
    each_backend(|backend| {
        let env = open_env(Controlled::new(backend), Gates::default());
        let tree = env.tree(&env.root, "owner", NONE);
        env.gates.open("owner", "completed");
        eventually(|| env.status(tree.owner) == TaskStatus::Completing);
        let r = env.root.clone();
        let idle = spawn(move || r.wait_for_idle(&bg()));
        let h = env.harness().clone();
        let harness_idle = spawn(move || h.wait_for_idle(&bg()));
        assert!(!settled(&idle));
        assert!(!settled(&harness_idle));
        env.gates.open("owner.inner", "completed");
        idle.join().unwrap().unwrap();
        harness_idle.join().unwrap().unwrap();
        assert_eq!(env.outcome_status(tree.owner), "completed");
        env.harness().close().unwrap();
    });
}

#[test]
fn harness_ownership_stops_idle_traversal_at_a_background_owner() {
    each_backend(|backend| {
        let env = open_env(Controlled::new(backend), Gates::default());
        let tree = env.tree(&env.root, "background", BACKGROUND);
        env.root.wait_for_idle(&bg()).unwrap();
        env.harness().wait_for_idle(&bg()).unwrap();
        // The background child is its own scope.
        let child = env.conversation(tree.child);
        let c = child.clone();
        let child_idle = spawn(move || c.wait_for_idle(&bg()));
        assert!(!settled(&child_idle));
        child.abort(false, &bg()).unwrap();
        child_idle.join().unwrap().unwrap();
        env.harness().abort_task(tree.owner, &bg()).unwrap();
        env.harness().close().unwrap();
    });
}

#[test]
fn harness_ownership_cascades_an_abort_mark_to_the_owned_foreground_subtree_and_withdraws_its_queued_inputs() {
    each_backend(|backend| {
        let env = open_env(Controlled::new(backend), Gates::default());
        let tree = env.tree(&env.root, "owner", NONE);
        let nested = env.conversation(tree.child);
        let deeper = env.tree(&nested, "deeper", NONE);
        let shielded = env.tree(&nested, "shielded", BACKGROUND);
        // Busy conversations queue submissions; the inner task stands in for the child's run.
        busy(&nested, tree.inner);
        let queued = queue(&nested, SubmissionType::Input);
        assert_eq!(
            env.harness().abort_task(tree.owner, &bg()).unwrap(),
            AbortResult::Marked
        );
        for id in [tree.owner, tree.inner, deeper.owner, deeper.inner] {
            assert_eq!(env.outcome_status(id), "aborted");
        }
        // A nested background owner is a boundary.
        assert_ne!(env.status(shielded.owner), TaskStatus::Terminal);
        assert_ne!(env.status(shielded.inner), TaskStatus::Terminal);
        assert_matches(
            &submission(env.harness(), queued),
            json!({ "status": "unanswered", "reason": "aborted" }),
        );
        env.harness().abort_task(shielded.owner, &bg()).unwrap();
        env.harness().close().unwrap();
    });
}

#[test]
fn harness_ownership_cascades_a_failed_owner_but_not_a_completed_one() {
    each_backend(|backend| {
        let env = open_env(Controlled::new(backend), Gates::default());
        let failed = env.tree(&env.root, "failed", NONE);
        let done = env.tree(&env.root, "done", NONE);
        env.gates.open("failed", "failed");
        env.gates.open("done", "completed");
        assert_eq!(env.outcome_status(failed.inner), "aborted");
        assert_eq!(env.outcome_status(failed.owner), "failed");
        eventually(|| env.status(done.owner) == TaskStatus::Completing);
        assert_ne!(env.status(done.inner), TaskStatus::Terminal);
        env.gates.open("done.inner", "completed");
        assert_eq!(env.outcome_status(done.owner), "completed");
        env.harness().close().unwrap();
    });
}

#[test]
fn harness_ownership_aborts_a_background_task_directly_with_its_ordinary_subtree() {
    each_backend(|backend| {
        let env = open_env(Controlled::new(backend), Gates::default());
        let tree = env.tree(&env.root, "background", BACKGROUND);
        env.harness().abort_task(tree.owner, &bg()).unwrap();
        assert_eq!(env.outcome_status(tree.inner), "aborted");
        env.harness().close().unwrap();
    });
}

#[test]
fn harness_ownership_aborts_a_conversation_withdrawing_inputs_keeping_writes_and_background_work() {
    each_backend(|backend| {
        let env = open_env(Controlled::new(backend), Gates::default());
        let foreground = env.tree(&env.root, "foreground", NONE);
        let background = env.tree(&env.root, "background", BACKGROUND);
        busy(&env.root, foreground.owner);
        let input = queue(&env.root, SubmissionType::Input);
        let write = queue(&env.root, SubmissionType::Write);
        env.root.abort(false, &bg()).unwrap();
        for id in [foreground.owner, foreground.inner] {
            assert_eq!(env.status(id), TaskStatus::Terminal);
        }
        assert_ne!(env.status(background.owner), TaskStatus::Terminal);
        assert_ne!(env.status(background.inner), TaskStatus::Terminal);
        assert_matches(&submission(env.harness(), input), json!({ "reason": "aborted" }));
        assert_eq!(submission(env.harness(), write)["status"], "queued");
        env.harness().abort_task(background.owner, &bg()).unwrap();
        env.harness().close().unwrap();
    });
}

#[test]
fn harness_ownership_aborts_work_created_below_a_held_failed_owner_but_not_below_a_terminal_one() {
    each_backend(|backend| {
        let env = open_env(Controlled::new(backend), Gates::default());
        let tree = env.tree(
            &env.root,
            "owner",
            TreeOptions {
                slow_inner: true,
                ..NONE
            },
        );
        env.gates.open("owner", "failed");
        eventually(|| env.harness().get_task(tree.inner).unwrap().unwrap().abort_requested);
        assert_eq!(env.status(tree.owner), TaskStatus::Completing);
        let child = env.conversation(tree.child);
        let create = |name: &str| {
            child
                .commit(|tx| {
                    Ok(tx.create_task(env.hold.definition(), json!({ "name": name }), TaskOptions::default())?)
                })
                .unwrap()
        };
        let during = create("during");
        assert_eq!(env.outcome_status(during), "aborted");
        env.gates.open("abort.owner.inner", "completed");
        assert_eq!(env.outcome_status(tree.owner), "failed");
        // A terminal owner never cascades: work in its conversation runs normally.
        let after = create("after");
        env.gates.open("after", "completed");
        assert_eq!(env.outcome_status(after), "completed");
        env.harness().close().unwrap();
    });
}

#[test]
fn harness_ownership_withdraws_queued_inputs_below_the_aborted_task_but_keeps_its_own_conversations_queue() {
    each_backend(|backend| {
        let env = open_env(Controlled::new(backend), Gates::default());
        let tree = env.tree(&env.root, "owner", NONE);
        let child = env.conversation(tree.child);
        busy(&env.root, tree.owner);
        busy(&child, tree.inner);
        let own = queue(&env.root, SubmissionType::Input);
        let below = queue(&child, SubmissionType::Input);
        let write = queue(&child, SubmissionType::Write);
        env.harness().abort_task(tree.owner, &bg()).unwrap();
        env.harness().wait_for_task(tree.inner, &bg()).unwrap();
        assert_eq!(submission(env.harness(), own)["status"], "queued");
        assert_eq!(submission(env.harness(), below)["status"], "unanswered");
        assert_eq!(submission(env.harness(), write)["status"], "queued");
        env.harness().close().unwrap();
    });
}

#[test]
fn harness_ownership_keeps_a_nested_background_owners_subtree_when_its_background_owner_is_aborted() {
    each_backend(|backend| {
        let env = open_env(Controlled::new(backend), Gates::default());
        let outer = env.tree(&env.root, "outer", BACKGROUND);
        let inner = env.tree(&env.conversation(outer.child), "inner", BACKGROUND);
        env.harness().abort_task(outer.owner, &bg()).unwrap();
        assert_eq!(env.outcome_status(outer.inner), "aborted");
        assert_ne!(env.status(inner.owner), TaskStatus::Terminal);
        assert_ne!(env.status(inner.inner), TaskStatus::Terminal);
        env.harness().abort_task(inner.owner, &bg()).unwrap();
        env.harness().close().unwrap();
    });
}

#[test]
fn harness_ownership_decides_idle_after_reopen_from_owner_edges_it_loads_first() {
    each_backend(|backend| {
        let gates = Gates::default();
        let storage = Controlled::new(backend);
        let env = open_env(storage.clone(), gates.clone());
        let foreground = env.tree(&env.root, "fg", NONE);
        let deeper = env.tree(&env.conversation(foreground.child), "fg2", NONE);
        let background = env.tree(&env.root, "bg", BACKGROUND);
        gates.open("fg", "completed");
        gates.open("fg2", "completed");
        // Both owners hold their outcomes while the work below them runs.
        eventually(|| env.status(foreground.owner) == TaskStatus::Completing);
        eventually(|| env.status(deeper.owner) == TaskStatus::Completing);
        env.harness().close().unwrap();

        let env = open_env(storage.reopen(), gates.clone());
        // Two levels below completed foreground owners, the inner tasks keep the root busy.
        let r = env.root.clone();
        let root_idle = spawn(move || r.wait_for_idle(&bg()));
        let h = env.harness().clone();
        let harness_idle = spawn(move || h.wait_for_idle(&bg()));
        assert!(!settled(&root_idle));
        assert!(!settled(&harness_idle));
        gates.open("fg.inner", "completed");
        gates.open("fg2.inner", "completed");
        // The background subtree does not count.
        root_idle.join().unwrap().unwrap();
        harness_idle.join().unwrap().unwrap();
        assert_eq!(env.status(foreground.owner), TaskStatus::Terminal);
        assert_ne!(env.status(background.inner), TaskStatus::Terminal);
        env.harness().abort_task(background.owner, &bg()).unwrap();
        env.harness().close().unwrap();
    });
}

/// Without a Harness nothing derives marks: a cancelled owner with a live task below it, written through a plain
/// Session (as a crash between the owner's change and its cascade leaves it).
fn unapplied_cascade(storage: &Controlled, hold: &Task, abort_requested: bool, state: TaskState) -> (TaskId, TaskId) {
    let session = Session::new(storage.clone());
    let (owner, inner) = session
        .commit(|tx| {
            let root = tx.create_conversation(ConversationOwnership::Ownerless)?;
            let options = TaskOptions {
                conversation_id: Some(root.id),
                ..TaskOptions::default()
            };
            let owner = tx.create_task(hold.definition(), json!({ "name": "gone" }), options)?;
            let child = tx.create_conversation(ConversationOwnership::Task(owner))?;
            let options = TaskOptions {
                conversation_id: Some(child.id),
                ..TaskOptions::default()
            };
            let inner = tx.create_task(hold.definition(), json!({ "name": "orphan" }), options)?;
            Ok((owner, inner))
        })
        .unwrap();
    session
        .commit(|tx: &mut Tx<'_>| {
            let record = tx.task(owner)?.unwrap();
            tx.set_task(TaskRecord {
                abort_requested,
                state,
                ..record
            })
        })
        .unwrap();
    session.close().unwrap();
    (owner, inner)
}

#[test]
fn harness_ownership_derives_marks_a_crash_left_unapplied_below_a_held_failed_owner_at_open() {
    each_backend(|backend| {
        let gates = Gates::default();
        let storage = Controlled::new(backend);
        let failed = serde_json::from_value(
            json!({ "status": "completing", "outcome": { "status": "failed", "error": { "message": "crash" } } }),
        )
        .unwrap();
        let (owner, inner) = unapplied_cascade(&storage, &hold(&gates), false, failed);
        let env = open_env(storage.reopen(), gates);
        assert_eq!(env.outcome_status(inner), "aborted");
        // The owner finishes only after the work below it.
        assert_eq!(env.outcome_status(owner), "failed");
        env.harness().close().unwrap();
    });
}

#[test]
fn harness_ownership_derives_marks_a_crash_left_unapplied_below_an_abort_marked_owner_at_open() {
    each_backend(|backend| {
        let gates = Gates::default();
        let storage = Controlled::new(backend);
        let pending = TaskState::Pending {
            checkpoint: json!({ "phase": "hold" }),
        };
        let (owner, inner) = unapplied_cascade(&storage, &hold(&gates), true, pending);
        let env = open_env(storage.reopen(), gates);
        assert_eq!(env.outcome_status(inner), "aborted");
        assert_eq!(env.outcome_status(owner), "aborted");
        env.harness().close().unwrap();
    });
}

#[test]
fn harness_ownership_withdraws_an_input_queued_below_a_held_failed_owner_after_its_cascade() {
    each_backend(|backend| {
        let env = open_env(Controlled::new(backend), Gates::default());
        let tree = env.tree(
            &env.root,
            "owner",
            TreeOptions {
                slow_inner: true,
                ..NONE
            },
        );
        let child = env.conversation(tree.child);
        // The inner task stands in for the child's run, which stays busy after the cascade.
        busy(&child, tree.inner);
        env.gates.open("owner", "failed");
        eventually(|| env.harness().get_task(tree.inner).unwrap().unwrap().abort_requested);
        let late = queue(&child, SubmissionType::Input);
        eventually(|| submission(env.harness(), late)["status"] == "unanswered");
        assert_matches(
            &submission(env.harness(), late),
            json!({ "status": "unanswered", "reason": "aborted" }),
        );
        env.gates.open("abort.owner.inner", "completed");
        env.harness().wait_for_task(tree.owner, &bg()).unwrap();
        env.harness().close().unwrap();
    });
}

#[test]
fn harness_ownership_marks_work_admitted_after_reopen_below_a_cancelled_owner_whose_edge_was_not_loaded() {
    each_backend(|backend| {
        let gates = Gates::default();
        let storage = Controlled::new(backend);
        let env = open_env(storage.clone(), gates.clone());
        let tree = env.tree(
            &env.root,
            "owner",
            TreeOptions {
                slow_owner: true,
                ..NONE
            },
        );
        gates.open("owner.inner", "completed");
        env.harness().wait_for_task(tree.inner, &bg()).unwrap();
        // The owner stays live and cancelled in its slow abort handler.
        env.harness().abort_task(tree.owner, &bg()).unwrap();
        env.harness().close().unwrap();

        // The child is empty at open, so nothing loads its edge until new work arrives.
        let env = open_env(storage.reopen(), gates.clone());
        let child = env.conversation(tree.child);
        let late = child
            .commit(|tx| {
                Ok(tx.create_task(env.hold.definition(), json!({ "name": "late" }), TaskOptions::default())?)
            })
            .unwrap();
        assert_eq!(env.outcome_status(late), "aborted");
        gates.open("abort.owner", "completed");
        assert_eq!(env.outcome_status(tree.owner), "aborted");
        env.harness().close().unwrap();
    });
}

#[test]
fn harness_ownership_cascades_from_an_owner_the_scheduler_orphans() {
    each_backend(|backend| {
        let env = open_env(Controlled::new(backend), Gates::default());
        let (owner, inner) = env
            .root
            .commit(|tx| {
                let owner = tx.create_task(
                    unregistered().definition(),
                    json!({ "name": "unregistered" }),
                    TaskOptions::default(),
                )?;
                let child = tx.create_conversation(ConversationOwnership::Task(owner))?;
                let options = TaskOptions {
                    conversation_id: Some(child.id),
                    ..TaskOptions::default()
                };
                Ok((
                    owner,
                    tx.create_task(env.hold.definition(), json!({ "name": "below" }), options)?,
                ))
            })
            .unwrap();
        assert_eq!(env.harness().abort_task(owner, &bg()).unwrap(), AbortResult::Marked);
        assert_eq!(
            outcome(&env.harness().wait_for_task(owner, &bg()).unwrap()),
            json!({ "status": "orphaned", "reason": "missing_task" })
        );
        assert_eq!(env.outcome_status(inner), "aborted");
        env.harness().close().unwrap();
    });
}

#[test]
fn harness_ownership_cancels_only_the_callers_wait_never_the_shared_work() {
    each_backend(|backend| {
        let env = open_env(Controlled::new(backend), Gates::default());
        let tree = env.tree(&env.root, "owner", NONE);
        let waiting = Cancel::new();
        let ctx = Ctx::with_signal(&waiting);
        let r = env.root.clone();
        let idle = spawn(move || r.wait_for_idle(&ctx));
        // Settle only: a wait that starts after its signal was cancelled is rejected with the same reason.
        flush();
        waiting.cancel(HarnessError::msg("stop waiting"));
        assert_eq!(idle.join().unwrap().unwrap_err().message(), "stop waiting");
        assert_ne!(env.status(tree.inner), TaskStatus::Terminal);

        // Cancelling an abort after its commit leaves the marks in place.
        let slow = env.tree(&env.root, "slow", NONE);
        env.root
            .commit(|tx| {
                let record = tx.task(slow.owner)?.unwrap();
                tx.set_task(TaskRecord {
                    input: json!({ "name": "slow", "slowAbort": true }),
                    ..record
                })?;
                Ok(())
            })
            .unwrap();
        let aborting = Cancel::new();
        let ctx = Ctx::with_signal(&aborting);
        let r = env.root.clone();
        let abort = spawn(move || r.abort(false, &ctx));
        eventually(|| env.harness().get_task(slow.owner).unwrap().unwrap().abort_requested);
        aborting.cancel(HarnessError::msg("stop aborting"));
        assert_eq!(abort.join().unwrap().unwrap_err().message(), "stop aborting");
        env.gates.open("abort.slow", "completed");
        for id in [tree.owner, tree.inner, slow.owner, slow.inner] {
            assert_eq!(env.outcome_status(id), "aborted");
        }
        env.harness().close().unwrap();
    });
}

#[test]
fn harness_ownership_aborts_a_waiting_child_whose_awaited_task_completes_in_the_commit_that_marks_its_owner() {
    each_backend(|backend| {
        let env = open_env(Controlled::new(backend), Gates::default());
        let (owner, dependency, blocked) = env
            .root
            .commit(|tx| {
                let owner = tx.create_task(
                    env.hold.definition(),
                    json!({ "name": "owner" }),
                    TaskOptions::default(),
                )?;
                let child = tx.create_conversation(ConversationOwnership::Task(owner))?;
                let dependency = tx.create_task(
                    env.hold.definition(),
                    json!({ "name": "dependency" }),
                    TaskOptions::default(),
                )?;
                let options = TaskOptions {
                    conversation_id: Some(child.id),
                    ..TaskOptions::default()
                };
                let blocked = tx.create_task(waiter().definition(), json!({ "on": [dependency.0] }), options)?;
                Ok((owner, dependency, blocked))
            })
            .unwrap();
        eventually(|| env.gates.runs("dependency") == 1 && env.gates.runs("owner") == 1);
        eventually(|| env.status(blocked) == TaskStatus::Waiting);
        // One commit completes the dependency and marks the owner.
        env.root
            .commit(|tx| {
                let completed_dependency = tx.task(dependency)?.unwrap();
                let marked = tx.task(owner)?.unwrap();
                tx.set_task(TaskRecord {
                    state: TaskState::Terminal {
                        outcome: durable::TaskOutcome::Completed { result: Value::Null },
                    },
                    memos: None,
                    ..completed_dependency
                })?;
                tx.set_task(TaskRecord {
                    abort_requested: true,
                    ..marked
                })?;
                Ok(())
            })
            .unwrap();
        assert_eq!(env.outcome_status(blocked), "aborted");
        env.harness().close().unwrap();
    });
}

/// Fail the first commit that abort-marks the task in `target`.
fn fail_mark_of(storage: &Controlled, target: Arc<Mutex<Option<TaskId>>>) {
    storage.fail_commit_when(move |writes| {
        let target = (*target.lock().unwrap())?;
        writes
            .iter()
            .any(|write| matches!(write, StorageWrite::Task { value } if value.id == target && value.abort_requested))
            .then(|| StorageError::Failed("disk gone".into()))
    });
}

#[test]
fn harness_ownership_applies_marks_found_through_an_edge_loaded_after_reopen_once_a_failed_cascade_is_reopened() {
    each_backend(|backend| {
        let gates = Gates::default();
        let storage = Controlled::new(backend);
        let env = open_env(storage.clone(), gates.clone());
        let tree = env.tree(
            &env.root,
            "owner",
            TreeOptions {
                slow_owner: true,
                ..NONE
            },
        );
        gates.open("owner.inner", "completed");
        env.harness().wait_for_task(tree.inner, &bg()).unwrap();
        env.harness().abort_task(tree.owner, &bg()).unwrap();
        env.harness().close().unwrap();

        let target: Arc<Mutex<Option<TaskId>>> = Arc::default();
        let reopened = storage.reopen();
        fail_mark_of(&reopened, target.clone());
        let env = open_env(reopened.clone(), gates.clone());
        let child = env.conversation(tree.child);
        let late = child
            .commit(|tx| {
                let id = tx.create_task(env.hold.definition(), json!({ "name": "late" }), TaskOptions::default())?;
                *target.lock().unwrap() = Some(id);
                Ok(id)
            })
            .unwrap();
        // The failed cascade commit fails the Harness; `closed` waits for its invocations, and the owner's abort
        // handler ignores its signal: let it return.
        gates.open("abort.owner", "completed");
        assert!(matches!(env.harness().closed(), durable::SessionEnd::Failed(_)));
        env.harness().close().unwrap();
        // Reopening derives the mark again.
        let env = open_env(storage.reopen(), gates);
        assert_eq!(env.outcome_status(late), "aborted");
        env.harness().close().unwrap();
    });
}

#[test]
fn harness_ownership_fails_the_harness_on_a_failed_cascade_commit_and_reopening_applies_the_cascade() {
    each_backend(|backend| {
        let gates = Gates::default();
        let storage = Controlled::new(backend);
        let target: Arc<Mutex<Option<TaskId>>> = Arc::default();
        fail_mark_of(&storage, target.clone());
        let env = open_env(storage.clone(), gates.clone());
        let tree = env.tree(&env.root, "owner", NONE);
        *target.lock().unwrap() = Some(tree.inner);
        gates.open("owner", "failed");
        let failure = durable::SessionError::Storage(StorageError::Failed("disk gone".into()));
        assert_eq!(env.harness().closed(), durable::SessionEnd::Failed(failure.clone()));
        assert_eq!(reports(&env.opened), vec![HarnessError::Session(failure)]);
        env.harness().close().unwrap();
        let env = open_env(storage.reopen(), gates);
        assert_eq!(env.outcome_status(tree.inner), "aborted");
        env.harness().close().unwrap();
    });
}

// ─── harness-cancellation-barrier.test.ts ───────────────────────────────────────

/// Runs until aborted; its abort handler ends it `aborted`.
fn dormant(name: &str) -> Task {
    Task::define(name, 1, |_| json!({ "phase": "work" }))
        .phase("work", |_, runtime| aborted(runtime))
        .abort(|_, runtime| {
            runtime.commit(&runtime.ctx(), |_, _| {
                Ok(Some(NextState::Terminal {
                    outcome: durable::TaskOutcome::Aborted {
                        reason: None,
                        result: None,
                    },
                }))
            })
        })
        .build()
}

fn child_of(owner: TaskId) -> TaskOptions {
    TaskOptions {
        ownership: TaskOwnership::Task(owner),
        ..TaskOptions::default()
    }
}

#[test]
fn harness_barrier_does_not_start_the_next_phase_while_the_cascade_that_marks_it_has_not_committed() {
    each_backend(|backend| {
        let storage = Controlled::new(backend);
        let (entered, advance) = (Deferred::new(), Deferred::new());
        let ran_next = Arc::new(Mutex::new(false));
        let (e, a, r) = (entered.clone(), advance.clone(), ran_next.clone());
        let child_task = Task::define("test.child", 1, |_| json!({ "phase": "first" }))
            .phase("first", move |_, runtime| {
                e.resolve();
                a.wait();
                runtime.commit(&runtime.ctx(), |_, _| Ok(running(json!({ "phase": "next" }))))
            })
            .phase("next", move |_, runtime| {
                *r.lock().unwrap() = true;
                aborted(runtime)
            })
            .abort(|_, runtime| {
                runtime.commit(&runtime.ctx(), |_, _| {
                    Ok(Some(NextState::Terminal {
                        outcome: durable::TaskOutcome::Aborted {
                            reason: None,
                            result: None,
                        },
                    }))
                })
            })
            .build();
        let parent_task = dormant("test.dormant");
        let opened = open_tasks(
            storage.clone(),
            vec![parent_task.clone(), child_task.clone()],
            OpenOptions::default(),
        )
        .unwrap();
        let root = opened.harness.root().unwrap();
        let parent = root
            .commit(|tx| Ok(tx.create_task(parent_task.definition(), Value::Null, TaskOptions::default())?))
            .unwrap();
        let child = root
            .commit(|tx| Ok(tx.create_task(child_task.definition(), Value::Null, child_of(parent))?))
            .unwrap();
        let target = child;
        opened.harness.resume().unwrap();
        entered.wait();
        // Hold the commit that abort-marks the child.
        let gate = storage.hold_commits_when(move |writes| {
            writes.iter().any(
                |write| matches!(write, StorageWrite::Task { value } if value.id == target && value.abort_requested),
            )
        });
        let h = opened.harness.clone();
        let aborting = spawn(move || h.abort_task(parent, &bg()));
        gate.wait_entered();
        // The parent's request is durable, the cascade's mark of the child is not: its phase must not advance.
        advance.resolve();
        // Settle, then check the phase did not advance: a slow machine can only let this pass wrongly.
        for _ in 0..5 {
            flush();
        }
        assert!(!*ran_next.lock().unwrap());
        gate.release();
        assert_eq!(
            outcome(&opened.harness.wait_for_task(child, &bg()).unwrap())["status"],
            "aborted"
        );
        aborting.join().unwrap().unwrap();
        assert!(!*ran_next.lock().unwrap());
        opened.harness.close().unwrap();
    });
}

/// Write records directly to the storage, bypassing every Session rule (a state an earlier crash or version left).
fn write_raw(storage: &Controlled, records: Vec<TaskRecord>) {
    let mut handle = storage.clone();
    durable::Storage::commit(
        &mut handle,
        records.into_iter().map(|value| StorageWrite::Task { value }).collect(),
    )
    .unwrap();
}

fn raw_task(storage: &Controlled, id: TaskId) -> TaskRecord {
    durable::Storage::task(storage, id).unwrap().unwrap()
}

#[test]
fn harness_barrier_lets_a_fail_fast_request_in_the_same_pass_override_a_restart_mark() {
    each_backend(|backend| {
        let storage = Controlled::new(backend);
        let dormant_task = dormant("test.dormant");
        let session = Session::new(storage.clone());
        let conversation = session
            .commit(|tx| tx.create_conversation(ConversationOwnership::Ownerless))
            .unwrap();
        let options = TaskOptions {
            conversation_id: Some(conversation.id),
            ..TaskOptions::default()
        };
        let parent = session
            .commit(|tx| tx.create_task(dormant_task.definition(), Value::Null, options))
            .unwrap();
        let failed = session
            .commit(|tx| tx.create_task(dormant_task.definition(), Value::Null, child_of(parent)))
            .unwrap();
        let child = session
            .commit(|tx| tx.create_task(dormant_task.definition(), Value::Null, child_of(parent)))
            .unwrap();
        session.close().unwrap();
        let storage = storage.reopen();
        // A restart-abandoned owner that waits failFast on a failed child and a live one.
        let owner = raw_task(&storage, parent);
        let failed_record = raw_task(&storage, failed);
        write_raw(
            &storage,
            vec![
                TaskRecord {
                    abort_requested: true,
                    abort_reason: Some(AbortReason::Restart),
                    state: TaskState::Waiting {
                        checkpoint: json!({ "phase": "work" }),
                        on: vec![failed, child],
                        policy: JoinPolicy::FailFast,
                    },
                    ..owner
                },
                TaskRecord {
                    state: serde_json::from_value(json!({ "status": "terminal", "outcome": { "status": "failed", "error": { "message": "failed" } } })).unwrap(),
                    ..failed_record
                },
            ],
        );
        // The child's definition is missing: a restart mark would keep it waiting, a request orphans it.
        let opened = open_tasks(storage, vec![], OpenOptions::default()).unwrap();
        opened.harness.resume().unwrap();
        eventually(|| opened.harness.get_task(child).unwrap().unwrap().state.status() == TaskStatus::Terminal);
        assert_matches(
            &js(&opened.harness.get_task(child).unwrap().unwrap().state),
            json!({ "outcome": { "status": "orphaned", "reason": "missing_task" } }),
        );
        opened.harness.close().unwrap();
    });
}

#[test]
fn harness_barrier_upgrades_restart_marks_down_the_tree_when_an_abort_is_requested_above_them() {
    each_backend(|backend| {
        let storage = Controlled::new(backend);
        let dormant_task = dormant("test.dormant");
        let missing = dormant("test.missing");
        let session = Session::new(storage.clone());
        let conversation = session
            .commit(|tx| tx.create_conversation(ConversationOwnership::Ownerless))
            .unwrap();
        let options = TaskOptions {
            conversation_id: Some(conversation.id),
            ..TaskOptions::default()
        };
        let top = session
            .commit(|tx| tx.create_task(dormant_task.definition(), Value::Null, options))
            .unwrap();
        let middle = session
            .commit(|tx| {
                tx.create_task(
                    dormant_task.definition(),
                    Value::Null,
                    TaskOptions {
                        abandon_on_restart: true,
                        ..child_of(top)
                    },
                )
            })
            .unwrap();
        let leaf = session
            .commit(|tx| tx.create_task(missing.definition(), Value::Null, child_of(middle)))
            .unwrap();
        session.close().unwrap();
        // Only Dormant is registered: the leaf waits for its definition under the restart cascade.
        let opened = open_tasks(storage.reopen(), vec![dormant_task], OpenOptions::default()).unwrap();
        opened.harness.resume().unwrap();
        eventually(|| opened.harness.get_task(leaf).unwrap().unwrap().abort_reason == Some(AbortReason::Restart));
        eventually(|| {
            let inspection = opened.harness.inspect().unwrap();
            let task = inspection.tasks.iter().find(|task| task.record.id == leaf).unwrap();
            matches!(task.state, TaskInspectionState::Blocked { .. })
        });
        assert_ne!(
            opened.harness.get_task(leaf).unwrap().unwrap().state.status(),
            TaskStatus::Terminal
        );
        // A request on the top task reaches the leaf two levels down, which is then orphaned.
        opened.harness.abort_task(top, &bg()).unwrap();
        for (id, status) in [(leaf, "orphaned"), (middle, "aborted"), (top, "aborted")] {
            assert_eq!(
                outcome(&opened.harness.wait_for_task(id, &bg()).unwrap())["status"],
                status
            );
        }
        opened.harness.close().unwrap();
    });
}

#[test]
fn harness_barrier_marks_a_flagged_task_that_holds_its_outcome_aborting_its_owned_work_and_keeps_the_outcome() {
    each_backend(|backend| {
        let storage = Controlled::new(backend);
        let dormant_task = dormant("test.dormant");
        let session = Session::new(storage.clone());
        let conversation = session
            .commit(|tx| tx.create_conversation(ConversationOwnership::Ownerless))
            .unwrap();
        let options = TaskOptions {
            conversation_id: Some(conversation.id),
            abandon_on_restart: true,
            ..TaskOptions::default()
        };
        let holder = session
            .commit(|tx| tx.create_task(dormant_task.definition(), Value::Null, options))
            .unwrap();
        let child = session
            .commit(|tx| tx.create_task(dormant_task.definition(), Value::Null, child_of(holder)))
            .unwrap();
        session.close().unwrap();
        let storage = storage.reopen();
        let record = raw_task(&storage, holder);
        write_raw(
            &storage,
            vec![TaskRecord {
                state: TaskState::Completing {
                    outcome: durable::TaskOutcome::Completed { result: Value::Null },
                },
                ..record
            }],
        );
        let opened = open_tasks(storage, vec![dormant_task], OpenOptions::default()).unwrap();
        opened.harness.resume().unwrap();
        assert_eq!(
            outcome(&opened.harness.wait_for_task(child, &bg()).unwrap())["status"],
            "aborted"
        );
        let settled = opened.harness.wait_for_task(holder, &bg()).unwrap();
        assert_matches(
            &js(&settled),
            json!({ "abortReason": "restart", "state": { "outcome": { "status": "completed" } } }),
        );
        opened.harness.close().unwrap();
    });
}

#[test]
fn harness_ownership_holds_a_fault_as_completing_while_owned_work_is_live_and_cascades_it() {
    each_backend(|backend| {
        let gates = Gates::default();
        let hold_task = hold(&gates);
        let h = hold_task.clone();
        let inner_id: Arc<Mutex<Option<TaskId>>> = Arc::default();
        let i = inner_id.clone();
        // Creates a child in its own conversation-owned subtree, then throws: the scheduler's fault must hold.
        let faulting = Task::define("test.faulting-owner", 1, |_| json!({ "phase": "run" }))
            .phase("run", move |task, runtime| {
                let owner = task.id;
                runtime.commit(&runtime.ctx(), |tx, _| {
                    let child = tx.create_conversation(ConversationOwnership::Task(owner))?;
                    let options = TaskOptions {
                        conversation_id: Some(child.id),
                        ..TaskOptions::default()
                    };
                    let slow = json!({ "name": "below", "slowAbort": true });
                    *i.lock().unwrap() = Some(tx.create_task(h.definition(), slow, options)?);
                    Ok(None)
                })?;
                Err(HarnessError::msg("owner broke"))
            })
            .build();
        let opened = open_tasks(
            Controlled::new(backend),
            vec![hold_task, faulting.clone()],
            OpenOptions::default(),
        )
        .unwrap();
        let root = opened.harness.root().unwrap();
        let owner = start(&root, &faulting, false);
        opened.harness.resume().unwrap();
        eventually(|| inner_id.lock().unwrap().is_some());
        let inner = inner_id.lock().unwrap().unwrap();
        // The failed outcome is cancellation intent: the work below is marked while the owner holds.
        eventually(|| opened.harness.get_task(inner).unwrap().unwrap().abort_requested);
        assert_matches(
            &js(&opened.harness.get_task(owner).unwrap().unwrap().state),
            json!({ "status": "completing", "outcome": { "status": "faulted", "error": { "message": "owner broke" } } }),
        );
        gates.open("abort.below", "completed");
        assert_eq!(
            outcome(&opened.harness.wait_for_task(inner, &bg()).unwrap())["status"],
            "aborted"
        );
        assert_eq!(
            outcome(&opened.harness.wait_for_task(owner, &bg()).unwrap()),
            json!({ "status": "faulted", "error": { "message": "owner broke" } })
        );
        opened.harness.close().unwrap();
    });
}
