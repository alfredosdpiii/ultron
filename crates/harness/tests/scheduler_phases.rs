//! Port of upstream Pi Durable `test/harness-tasks.test.ts` ("task phases", "task runtime", "task scheduling"),
//! each case run against Memory, SQLite and JSONL storage. Cases about agents, hooks, tools, context reads and
//! document watches through the runtime are not ported (those runtimes are not).

mod support;

use durable::{
    ConversationOwnership, DocDefinition, DocSemantics, EntryDraft, JoinPolicy, Owner, TaskId, TaskOptions, TaskStatus,
};
use harness::scheduler::{Cancel, Ctx, Extension, HarnessError, NextState, Task, TaskRuntime};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use support::*;

#[test]
fn harness_tasks_continue_one_invocation_through_checkpoint_progress() {
    each_backend(|backend| {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let runtimes = Arc::new(Mutex::new(Vec::<usize>::new()));
        let (seen2, runtimes2) = (seen.clone(), runtimes.clone());
        let counter = Task::define("test.counter", 1, |_| json!({ "phase": "count", "n": 0 }))
            .phase("count", move |task, runtime| {
                seen2
                    .lock()
                    .unwrap()
                    .push(js(&task.state)["checkpoint"]["n"].as_u64().unwrap());
                let address = runtime as *const TaskRuntime as usize;
                let mut list = runtimes2.lock().unwrap();
                if !list.contains(&address) {
                    list.push(address);
                }
                drop(list);
                let to = task.input["to"].as_u64().unwrap();
                runtime.commit(&runtime.ctx(), |_, current| {
                    let n = js(&current.state)["checkpoint"]["n"].as_u64().unwrap() + 1;
                    Ok(if n == to {
                        completed(json!(n))
                    } else {
                        running(json!({ "phase": "count", "n": n }))
                    })
                })
            })
            .build();
        let (opened, root, _) = open_root(backend, vec![counter.clone()], OpenOptions::default());
        let id = root
            .commit(|tx| Ok(tx.create_task(counter.definition(), json!({ "to": 3 }), TaskOptions::default())?))
            .unwrap();
        opened.harness.resume().unwrap();
        let receipt = opened.harness.wait_for_task(id, &bg()).unwrap();
        assert_eq!(outcome(&receipt), json!({ "status": "completed", "result": 3 }));
        assert_eq!(*seen.lock().unwrap(), vec![0, 1, 2]);
        assert_eq!(runtimes.lock().unwrap().len(), 1);
        opened.harness.close().unwrap();
    });
}

#[test]
fn harness_tasks_fault_a_phase_without_durable_progress_and_a_throwing_phase() {
    each_backend(|backend| {
        let idle = one_step("test.idle", |_, _| Ok(()));
        // A commit that returns no state is not progress.
        let document_only = one_step("test.document-only", |_, runtime| {
            runtime.commit(&runtime.ctx(), |_, _| Ok(None))
        });
        let throws = one_step("test.throws", |_, _| Err(HarnessError::msg("boom")));
        let (opened, root, _) = open_root(
            backend,
            vec![idle.clone(), document_only.clone(), throws.clone()],
            OpenOptions::default(),
        );
        let ids = [
            start(&root, &idle, false),
            start(&root, &document_only, false),
            start(&root, &throws, false),
        ];
        opened.harness.resume().unwrap();
        let outcomes: Vec<Value> = ids
            .iter()
            .map(|id| outcome(&opened.harness.wait_for_task(*id, &bg()).unwrap()))
            .collect();
        assert_eq!(
            outcomes,
            vec![
                json!({ "status": "faulted", "error": { "message": "Task test.idle phase run returned without durable progress" } }),
                json!({ "status": "faulted", "error": { "message": "Task test.document-only phase run returned without durable progress" } }),
                json!({ "status": "faulted", "error": { "message": "boom" } }),
            ]
        );
        opened.harness.close().unwrap();
    });
}

#[test]
fn harness_tasks_keep_a_committed_terminal_outcome_when_the_handler_throws_afterwards() {
    each_backend(|backend| {
        let late = Arc::new(Mutex::new(None));
        let late2 = late.clone();
        let done = one_step("test.done", move |_, runtime| {
            runtime.commit(&runtime.ctx(), |_, _| Ok(completed(json!("ok"))))?;
            *late2.lock().unwrap() = Some(runtime.commit(&runtime.ctx(), |_, _| Ok(None)));
            Err(HarnessError::msg("after terminal"))
        });
        let (opened, root, _) = open_root(backend, vec![done.clone()], OpenOptions::default());
        let id = start(&root, &done, false);
        opened.harness.resume().unwrap();
        assert_eq!(
            outcome(&opened.harness.wait_for_task(id, &bg()).unwrap()),
            json!({ "status": "completed", "result": "ok" })
        );
        eventually(|| late.lock().unwrap().is_some());
        let error = late.lock().unwrap().clone().unwrap().unwrap_err();
        assert!(
            error.message().contains(&format!("Task {} is terminal", id.0)),
            "{error}"
        );
        opened.harness.close().unwrap();
    });
}

#[test]
fn harness_tasks_compare_checkpoints_by_value_including_arrays() {
    each_backend(|backend| {
        let collect = Task::define("test.collect", 1, |_| json!({ "phase": "collect", "items": [] }))
            .phase("collect", |task, runtime| {
                let items = js(&task.state)["checkpoint"]["items"].as_array().unwrap().clone();
                // Two rounds of progress, then an equal copy of the checkpoint, which is no progress.
                let mut next = items.clone();
                if items.len() < 2 {
                    next.push(json!(format!("item{}", items.len())));
                }
                runtime.commit(&runtime.ctx(), |_, _| {
                    Ok(running(json!({ "phase": "collect", "items": next })))
                })
            })
            .build();
        let (opened, root, _) = open_root(backend, vec![collect.clone()], OpenOptions::default());
        let id = start(&root, &collect, false);
        opened.harness.resume().unwrap();
        assert_eq!(
            outcome(&opened.harness.wait_for_task(id, &bg()).unwrap()),
            json!({ "status": "faulted", "error": { "message": "Task test.collect phase collect returned without durable progress" } })
        );
        opened.harness.close().unwrap();
    });
}

#[test]
fn harness_tasks_commit_results_with_entries_keep_memos_until_terminal_and_retire_task_documents() {
    each_backend(|backend| {
        let progress = DocDefinition::new("test.task-progress", 1, DocSemantics::Task, |_| json!({ "lines": [] }))
            .define()
            .unwrap();
        let child = one_step("test.child", |_, runtime| {
            runtime.commit(&runtime.ctx(), |_, _| Ok(completed(Value::Null)))
        });
        let child_id = Arc::new(Mutex::new(None));
        let progress_seen = Arc::new(Mutex::new(Value::Null));
        let memo_checks = Arc::new(Mutex::new(Vec::new()));
        let (p1, p2, c1, ch, s1, m1) = (
            progress.clone(),
            progress.clone(),
            child_id.clone(),
            child.clone(),
            progress_seen.clone(),
            memo_checks.clone(),
        );
        let writer = Task::define("test.writer", 1, |_| json!({ "phase": "write" }))
            .phase("write", move |task, runtime| {
                let ctx = runtime.ctx();
                let mut checks = m1.lock().unwrap();
                checks.push(json!(runtime.memo("choice")?));
                let winners = [
                    runtime.memo_set("choice", json!("a"), &ctx)?,
                    runtime.memo_set("choice", json!("b"), &ctx)?,
                ];
                checks.push(json!(winners));
                checks.push(json!(runtime.memo("choice")?));
                // Memo names never resolve to inherited object properties.
                checks.push(json!(runtime.memo("toString")?));
                checks.push(runtime.memo_set("toString", json!("own"), &ctx)?);
                drop(checks);
                let task_id = task.id;
                runtime.commit(&ctx, |tx, _| {
                    let draft = tx.doc(&p1, Owner::Task(task_id))?;
                    draft
                        .push(&[chord_delta::Seg::Key("lines".into())], vec![json!("wrote")])
                        .unwrap();
                    // Task creation defaults to the task's own conversation.
                    *c1.lock().unwrap() = Some(tx.create_task(ch.definition(), Value::Null, TaskOptions::default())?);
                    Ok(running(json!({ "phase": "answer" })))
                })
            })
            .phase("answer", move |task, runtime| {
                let ctx = runtime.ctx();
                assert_eq!(js(&task.memos), json!({ "choice": "a", "toString": "own" }));
                let task_id = task.id;
                runtime.commit(&ctx, |tx, _| {
                    let draft = tx.doc(&p2, Owner::Task(task_id))?;
                    *s1.lock().unwrap() = draft.get(&[chord_delta::Seg::Key("lines".into())]).unwrap().unwrap();
                    Ok(None)
                })?;
                runtime.commit(&ctx, |tx, current| {
                    let mut draft = EntryDraft::new("answer");
                    draft.model = Some(vec![json!({ "role": "user", "content": "done", "timestamp": 1 })]);
                    let entry = tx.append_entry(current.conversation_id, draft)?;
                    Ok(completed(json!({ "entryId": entry.id })))
                })
            })
            .build();
        let (opened, root, _) = open_root(backend, vec![writer.clone(), child.clone()], OpenOptions::default());
        let harness = &opened.harness;
        let conversation = harness.create_conversation(ConversationOwnership::Ownerless).unwrap();
        let id = start(&conversation, &writer, false);
        harness.resume().unwrap();
        let receipt = harness.wait_for_task(id, &bg()).unwrap();
        assert!(js(&receipt.state).get("checkpoint").is_none());
        assert!(receipt.memos.is_none());
        assert_eq!(
            *memo_checks.lock().unwrap(),
            vec![Value::Null, json!(["a", "a"]), json!("a"), Value::Null, json!("own")]
        );
        let entry_id = durable::EntryId(outcome(&receipt)["result"]["entryId"].as_u64().unwrap());
        let entry = harness.commit(|tx| Ok(tx.entry(entry_id)?)).unwrap().unwrap();
        assert_eq!(entry.kind, "answer");
        assert_eq!(entry.conversation_id, conversation.id);
        assert_eq!(entry.by_task_id, Some(id));
        assert_eq!(*progress_seen.lock().unwrap(), json!(["wrote"]));
        assert_eq!(harness.snapshot(&progress, Owner::Task(id)).unwrap(), None);
        assert_eq!(harness.get_task(id).unwrap(), Some(receipt));
        let child_id = child_id.lock().unwrap().unwrap();
        assert_eq!(
            harness.wait_for_task(child_id, &bg()).unwrap().conversation_id,
            conversation.id
        );
        assert_ne!(root.id, conversation.id);
        harness.close().unwrap();
    });
}

fn waiter_task(
    name: &str,
    on: Arc<Mutex<Vec<TaskId>>>,
    outcomes: Arc<Mutex<Vec<String>>>,
    order: Arc<Mutex<Vec<String>>>,
) -> Task {
    let (on2, order2) = (on.clone(), order.clone());
    Task::define(name, 1, |_| json!({ "phase": "wait" }))
        .phase("wait", move |_, runtime| {
            order2.lock().unwrap().push("wait".into());
            let on = on2.lock().unwrap().clone();
            runtime.commit(&runtime.ctx(), |_, _| {
                Ok(Some(NextState::Waiting {
                    checkpoint: json!({ "phase": "resume" }),
                    on,
                    policy: JoinPolicy::AllSettled,
                }))
            })
        })
        .phase("resume", move |_, runtime| {
            order.lock().unwrap().push("resume".into());
            let ids = on.lock().unwrap().clone();
            *outcomes.lock().unwrap() = runtime
                .outcomes(&ids)?
                .iter()
                .map(|outcome| js(outcome)["status"].as_str().unwrap().to_owned())
                .collect();
            runtime.commit(&runtime.ctx(), |_, _| Ok(completed(Value::Null)))
        })
        .abort(|_, runtime| runtime.commit(&runtime.ctx(), |_, _| Ok(aborted_with("test"))))
        .build()
}

#[test]
fn harness_tasks_resume_a_waiting_task_once_every_awaited_task_is_terminal() {
    each_backend(|backend| {
        let gate = Deferred::new();
        let order = Arc::new(Mutex::new(Vec::<String>::new()));
        let on = Arc::new(Mutex::new(Vec::new()));
        let outcomes = Arc::new(Mutex::new(Vec::new()));
        let waiter = waiter_task("test.waiter", on.clone(), outcomes.clone(), order.clone());
        let (o1, g1, o2) = (order.clone(), gate.clone(), order.clone());
        let first = one_step("test.first", move |_, runtime| {
            o1.lock().unwrap().push("first".into());
            g1.wait();
            runtime.commit(&runtime.ctx(), |_, _| Ok(completed(Value::Null)))
        });
        let faulting = one_step("test.faulting", move |_, _| {
            o2.lock().unwrap().push("faulting".into());
            Err(HarnessError::msg("fails"))
        });
        let (opened, root, _) = open_root(
            backend,
            vec![first.clone(), faulting.clone(), waiter.clone()],
            OpenOptions::default(),
        );
        let first_id = start(&root, &first, false);
        let faulting_id = start(&root, &faulting, false);
        *on.lock().unwrap() = vec![first_id, faulting_id];
        let waiter_id = start(&root, &waiter, false);
        opened.harness.resume().unwrap();
        eventually(|| order.lock().unwrap().len() == 3);
        // The waiter records its `wait` before it commits the waiting state.
        eventually(|| js(&opened.harness.get_task(waiter_id).unwrap().unwrap().state)["status"] == "waiting");
        // Settle, then check nothing else ran: a slow machine can only let this pass wrongly.
        flush();
        let mut sorted = order.lock().unwrap().clone();
        sorted.sort();
        assert_eq!(sorted, vec!["faulting", "first", "wait"]);
        let state = js(&opened.harness.get_task(waiter_id).unwrap().unwrap().state);
        assert_matches(
            &state,
            json!({ "status": "waiting", "on": [first_id.0, faulting_id.0] }),
        );
        gate.resolve();
        opened.harness.wait_for_task(waiter_id, &bg()).unwrap();
        assert_eq!(order.lock().unwrap().last().unwrap(), "resume");
        assert_eq!(*outcomes.lock().unwrap(), vec!["completed", "faulted"]);
        opened.harness.close().unwrap();
    });
}

// #10549
#[test]
fn harness_tasks_stamp_started_at_at_first_run_and_ended_at_when_terminal() {
    each_backend(|backend| {
        let clock = Arc::new(Mutex::new(1_000u64));
        let gate = Deferred::new();
        let on = Arc::new(Mutex::new(Vec::new()));
        let waiter = waiter_task("test.timed-waiter", on.clone(), Arc::default(), Arc::default());
        let held = gated("test.timed-held", gate.clone());
        let now_clock = clock.clone();
        let options = OpenOptions {
            now: Some(Arc::new(move || *now_clock.lock().unwrap())),
            ..OpenOptions::default()
        };
        let (opened, root, _) = open_root(backend, vec![waiter.clone(), held.clone()], options);
        let held_id = start(&root, &held, false);
        *on.lock().unwrap() = vec![held_id];
        let waiter_id = start(&root, &waiter, false);
        let times = || -> Vec<(TaskId, Option<u64>, Option<u64>)> {
            let inspection = opened.harness.inspect().unwrap();
            inspection
                .tasks
                .iter()
                .map(|task| (task.record.id, task.record.started_at, task.record.ended_at))
                .collect()
        };
        assert_eq!(times(), vec![(held_id, None, None), (waiter_id, None, None)]);
        *clock.lock().unwrap() = 2_000;
        opened.harness.resume().unwrap();
        eventually(|| opened.harness.get_task(waiter_id).unwrap().unwrap().state.status() == TaskStatus::Waiting);
        assert_eq!(
            times(),
            vec![(held_id, Some(2_000), None), (waiter_id, Some(2_000), None)]
        );
        // The waiter runs again at 5_000; its start stays the first run.
        *clock.lock().unwrap() = 5_000;
        gate.resolve();
        let settled_waiter = opened.harness.wait_for_task(waiter_id, &bg()).unwrap();
        let settled_held = opened.harness.get_task(held_id).unwrap().unwrap();
        assert_eq!(
            (settled_held.started_at, settled_held.ended_at),
            (Some(2_000), Some(5_000))
        );
        assert_eq!(
            (settled_waiter.started_at, settled_waiter.ended_at),
            (Some(2_000), Some(5_000))
        );
        opened.harness.close().unwrap();
    });
}

#[test]
fn harness_tasks_keep_one_registry_snapshot_per_phase_and_refresh_it_at_the_boundary() {
    each_backend(|backend| {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let phase_gate = Deferred::new();
        let entered = Deferred::new();
        let (s1, s2, g, e) = (seen.clone(), seen.clone(), phase_gate.clone(), entered.clone());
        let snapshots = Task::define("test.snapshots", 1, |_| json!({ "phase": "a" }))
            .phase("a", move |_, runtime| {
                let before = runtime.registry();
                e.resolve();
                g.wait();
                let now = runtime.registry();
                s1.lock()
                    .unwrap()
                    .push(json!({ "phase": "a", "tools": now.tools(), "same": Arc::ptr_eq(&now, &before) }));
                runtime.commit(&runtime.ctx(), |_, _| Ok(running(json!({ "phase": "b" }))))
            })
            .phase("b", move |_, runtime| {
                s2.lock()
                    .unwrap()
                    .push(json!({ "phase": "b", "tools": runtime.registry().tools(), "same": true }));
                runtime.commit(&runtime.ctx(), |_, _| Ok(completed(Value::Null)))
            })
            .build();
        let (opened, root, _) = open_root(backend, vec![snapshots.clone()], OpenOptions::default());
        let id = start(&root, &snapshots, false);
        opened.harness.resume().unwrap();
        entered.wait();
        opened
            .registry
            .install(Extension {
                name: "tool:late".into(),
                tasks: vec![],
                tools: vec!["late".into()],
            })
            .unwrap();
        phase_gate.resolve();
        opened.harness.wait_for_task(id, &bg()).unwrap();
        assert_eq!(
            *seen.lock().unwrap(),
            vec![
                json!({ "phase": "a", "tools": [], "same": true }),
                json!({ "phase": "b", "tools": ["late"], "same": true }),
            ]
        );
        opened.harness.close().unwrap();
    });
}

// ─── task runtime ───────────────────────────────────────────────────────────────

#[test]
fn harness_runtime_rejects_operations_after_the_invocation_ends() {
    each_backend(|backend| {
        let captured: Arc<Mutex<Option<TaskRuntime>>> = Arc::default();
        let c = captured.clone();
        let watcher = one_step("test.watcher", move |_, runtime| {
            *c.lock().unwrap() = Some(runtime.clone());
            runtime.commit(&runtime.ctx(), |_, _| Ok(completed(Value::Null)))
        });
        let (opened, root, _) = open_root(backend, vec![watcher.clone()], OpenOptions::default());
        let id = start(&root, &watcher, false);
        opened.harness.resume().unwrap();
        opened.harness.wait_for_task(id, &bg()).unwrap();
        // The step after the phase ends the invocation.
        let runtime = captured.lock().unwrap().clone().unwrap();
        eventually(|| runtime.ended());
        let ended = format!("Task {} invocation has ended", id.0);
        assert_eq!(runtime.commit(&bg(), |_, _| Ok(None)).unwrap_err().message(), ended);
        assert_eq!(runtime.memo("x").unwrap_err().message(), ended);
        assert_eq!(runtime.memo_set("x", json!(1), &bg()).unwrap_err().message(), ended);
        assert_eq!(runtime.sleep(0, &bg()).unwrap_err().message(), ended);
        assert_eq!(runtime.now().unwrap_err().message(), ended);
        assert_eq!(runtime.report(&HarnessError::msg("late")).unwrap_err().message(), ended);
        assert_eq!(runtime.get_task(id).unwrap_err().message(), ended);
        // The handler's own signal is cancelled by now, with the ended error.
        assert_eq!(runtime.signal().reason().unwrap().message(), ended);
        opened.harness.close().unwrap();
    });
}

#[test]
fn harness_runtime_reads_committed_documents_and_forwards_the_clock_and_reports() {
    each_backend(|backend| {
        let notes = DocDefinition::new(
            "test.runtime-notes",
            1,
            DocSemantics::Session,
            |_| json!({ "text": "" }),
        )
        .define()
        .unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (n, s) = (notes.clone(), seen.clone());
        let reader = one_step("test.reader", move |_, runtime| {
            s.lock()
                .unwrap()
                .push(runtime.snapshot(&n, Owner::Session)?.unwrap()["text"].clone());
            s.lock().unwrap().push(json!(runtime.now()?));
            runtime.report(&HarnessError::msg("reported"))?;
            runtime.commit(&runtime.ctx(), |_, _| Ok(completed(Value::Null)))
        });
        let options = OpenOptions {
            now: Some(Arc::new(|| 1234)),
            ..OpenOptions::default()
        };
        let (opened, root, _) = open_root(backend, vec![reader.clone()], options);
        for text in ["one", "two"] {
            opened
                .harness
                .commit(|tx| {
                    tx.doc(&notes, Owner::Session)?
                        .set(&[chord_delta::Seg::Key("text".into())], json!(text))
                        .unwrap();
                    Ok(())
                })
                .unwrap();
        }
        let id = start(&root, &reader, false);
        opened.harness.resume().unwrap();
        opened.harness.wait_for_task(id, &bg()).unwrap();
        assert_eq!(*seen.lock().unwrap(), vec![json!("two"), json!(1234)]);
        assert_eq!(reports(&opened), vec![HarnessError::msg("reported")]);
        opened.harness.close().unwrap();
    });
}

#[test]
fn harness_runtime_orders_commits_against_the_step_one_queued_before_lands_one_after_rejects() {
    each_backend(|backend| {
        let storage = Controlled::new(backend);
        let held: Arc<Mutex<Option<Gate>>> = Arc::default();
        let before: Pending<()> = Arc::default();
        let after: Pending<()> = Arc::default();
        let harness_ref: Arc<Mutex<Option<harness::scheduler::Harness>>> = Arc::default();
        let (st, h, b, a, hr) = (
            storage.clone(),
            held.clone(),
            before.clone(),
            after.clone(),
            harness_ref.clone(),
        );
        let detached = one_step("test.detached", move |_, runtime| {
            // Hold the line, queue a commit without waiting for it, and return; the step queues behind that commit.
            let gate = st.hold_commits();
            *h.lock().unwrap() = Some(gate.clone());
            let harness = hr.lock().unwrap().clone().unwrap();
            let conversation = runtime.conversation_id();
            let blocker = harness.clone();
            spawn(move || {
                blocker.commit(|tx| Ok(tx.append_entry(conversation, EntryDraft::new("blocker")).map(drop)?))
            });
            gate.wait_entered();
            let queued = runtime.clone();
            *b.lock().unwrap() = Some(spawn(move || {
                queued.commit(&bg(), |_, _| Ok(completed(json!("before"))))
            }));
            // `before` is on the line behind the held commit before the handler returns.
            line_reaches(&harness, 2);
            // Started after `before` was queued, so it lands after it: rejected whether it queues before or after the
            // step's own commit.
            let late = runtime.clone();
            *a.lock().unwrap() = Some(spawn(move || late.commit(&bg(), |_, _| Ok(completed(json!("after"))))));
            Ok(())
        });
        let opened = open_tasks(storage.clone(), vec![detached.clone()], OpenOptions::default()).unwrap();
        *harness_ref.lock().unwrap() = Some(opened.harness.clone());
        let root = opened.harness.root().unwrap();
        let id = start(&root, &detached, false);
        opened.harness.resume().unwrap();
        eventually(|| held.lock().unwrap().is_some());
        let gate = held.lock().unwrap().clone().unwrap();
        gate.wait_entered();
        eventually(|| after.lock().unwrap().is_some());
        gate.release();
        before.lock().unwrap().take().unwrap().join().unwrap().unwrap();
        assert_eq!(
            outcome(&opened.harness.wait_for_task(id, &bg()).unwrap()),
            json!({ "status": "completed", "result": "before" })
        );
        let late = after
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .join()
            .unwrap()
            .unwrap_err()
            .message();
        assert!(
            late.contains("invocation has ended") || late.contains("is terminal"),
            "{late}"
        );
        opened.harness.close().unwrap();
    });
}

#[test]
fn harness_runtime_sleeps_until_the_harness_clock_reaches_the_deadline() {
    each_backend(|backend| {
        let clock = Arc::new(Mutex::new(1_000u64));
        let woke = Deferred::new();
        let w = woke.clone();
        let sleeper = one_step("test.sleeper", move |_, runtime| {
            runtime.sleep(900, &runtime.ctx())?;
            runtime.sleep(1_005, &runtime.ctx())?;
            w.resolve();
            runtime.commit(&runtime.ctx(), |_, _| Ok(completed(Value::Null)))
        });
        let c = clock.clone();
        let options = OpenOptions {
            now: Some(Arc::new(move || *c.lock().unwrap())),
            ..OpenOptions::default()
        };
        let (opened, root, _) = open_root(backend, vec![sleeper.clone()], options);
        let id = start(&root, &sleeper, false);
        opened.harness.resume().unwrap();
        // The clock stands still, so real timers keep firing without waking the task. (A settle: a slow machine can
        // only let this check pass wrongly.)
        std::thread::sleep(std::time::Duration::from_millis(30));
        assert!(!woke.is_resolved());
        *clock.lock().unwrap() = 1_005;
        opened.harness.wait_for_task(id, &bg()).unwrap();
        opened.harness.close().unwrap();
    });
}

#[test]
fn harness_runtime_rejects_a_sleep_when_signalled_or_its_own_context_is_cancelled() {
    each_backend(|backend| {
        let results = Arc::new(Mutex::new(Vec::<String>::new()));
        let sleeping = Deferred::new();
        let (r1, r2, s) = (results.clone(), results.clone(), sleeping.clone());
        let far = || {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64
                + 3_600_000
        };
        let signalled = one_step("test.sleep-signalled", move |_, runtime| {
            s.resolve();
            runtime.sleep(far(), &runtime.ctx()).inspect_err(|error| {
                r1.lock().unwrap().push(format!("signalled:{}", error.name()));
            })
        });
        let cancelled = one_step("test.sleep-cancelled", move |_, runtime| {
            let controller = Cancel::new();
            let ctx = Ctx::with_signal(&controller);
            let sleeper = runtime.clone();
            let sleeping = spawn(move || sleeper.sleep(far(), &ctx));
            controller.cancel(HarnessError::msg("stop sleeping"));
            let error = sleeping.join().unwrap().unwrap_err();
            r2.lock().unwrap().push(format!("cancelled:{}", error.message()));
            runtime.commit(&runtime.ctx(), |_, _| Ok(completed(Value::Null)))
        });
        let (opened, root, _) = open_root(
            backend,
            vec![signalled.clone(), cancelled.clone()],
            OpenOptions::default(),
        );
        let signalled_id = start(&root, &signalled, false);
        let cancelled_id = start(&root, &cancelled, false);
        opened.harness.resume().unwrap();
        opened.harness.wait_for_task(cancelled_id, &bg()).unwrap();
        sleeping.wait();
        opened.harness.abort_task(signalled_id, &bg()).unwrap();
        assert_eq!(
            outcome(&opened.harness.wait_for_task(signalled_id, &bg()).unwrap()),
            json!({ "status": "aborted", "reason": "test" })
        );
        assert_eq!(
            *results.lock().unwrap(),
            vec!["cancelled:stop sleeping", "signalled:AbortError"]
        );
        opened.harness.close().unwrap();
    });
}
