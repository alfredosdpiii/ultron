//! Port of upstream `test/session-watches.test.ts` ("Session document watches"), run against every backend.
//! Listeners run on the watch's delivery thread; "never inline" becomes "never on the starting thread".
//! Not ported: commit Context values and context-cancelled acquisition (the Rust Session takes no Context).

#![allow(clippy::type_complexity)]

mod session_support;

use chord_delta::{Op, apply_immutable, path};
use durable::{DocDefinition, DocSemantics, Owner, Watch, WatchEnd};
use serde_json::{Value, json};
use session_support::*;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(10);

fn state_doc() -> durable::Doc {
    DocDefinition::new(
        "watch.state",
        1,
        DocSemantics::Session,
        |_| json!({ "value": 0, "items": ["a", "b"], "retained": { "label": "stable" } }),
    )
    .define()
    .unwrap()
}

fn create_state(backend: Backend) -> TestSession {
    let t = TestSession::open(backend);
    t.session
        .commit(|tx| tx.doc(&state_doc(), Owner::Session).map(drop))
        .unwrap();
    t
}

fn set_state(t: &TestSession, value: i64) {
    t.session
        .commit(|tx| {
            Ok(tx
                .doc(&state_doc(), Owner::Session)?
                .set(&path!["value"], json!(value))?)
        })
        .unwrap();
}

fn watch(t: &TestSession) -> Watch {
    t.session.watch_doc(&state_doc(), Owner::Session).unwrap().unwrap()
}

fn number(value: &durable::Observed) -> i64 {
    value.as_ref().map_or(-1, |value| value["value"].as_i64().unwrap())
}

fn ops_json(ops: &[Op]) -> Vec<Value> {
    ops.iter().map(Op::to_json).collect()
}

/// A listener that records `(value, ops)` and blocks on its first delivery until released.
fn blocking_listener(
    deliveries: Arc<Mutex<Vec<(durable::Observed, Vec<Value>)>>>,
) -> (
    impl FnMut(&durable::Observed, &[Op]) -> Result<(), String> + Send,
    mpsc::Receiver<()>,
    mpsc::Sender<()>,
) {
    let (entered_tx, entered) = mpsc::channel();
    let (release, release_rx) = mpsc::channel::<()>();
    let listener = move |value: &durable::Observed, ops: &[Op]| {
        deliveries.lock().unwrap().push((value.clone(), ops_json(ops)));
        if deliveries.lock().unwrap().len() == 1 {
            entered_tx.send(()).unwrap();
            release_rx.recv_timeout(WAIT).unwrap();
        }
        Ok(())
    };
    (listener, entered, release)
}

#[test]
fn durable_session_watches_never_create_an_absent_document() {
    each_backend(|backend| {
        let t = TestSession::open(backend);
        let commits = t.storage.commit_count();
        assert!(t.session.watch_doc(&state_doc(), Owner::Session).unwrap().is_none());
        assert_eq!(t.storage.commit_count(), commits);
        assert_eq!(t.storage.mint_count(), 0);
    });
}

#[test]
fn durable_session_watches_keep_the_acquisition_revision_until_start_and_deliver_exact_committed_frames() {
    each_backend(|backend| {
        let t = create_state(backend);
        let watch = watch(&t);
        let initial = watch.value().unwrap();
        set_state(&t, 1);
        let first = document_changes(&t.last_publication()).remove(0);
        set_state(&t, 2);
        let second = document_changes(&t.last_publication()).remove(0);
        assert!(Arc::ptr_eq(watch.value().as_ref().unwrap(), &initial));

        let deliveries = Arc::new(Mutex::new(Vec::new()));
        let sink = deliveries.clone();
        let starter = std::thread::current().id();
        watch
            .start(move |value, ops| {
                assert_ne!(std::thread::current().id(), starter);
                sink.lock().unwrap().push((value.clone(), ops_json(ops)));
                Ok(())
            })
            .unwrap();
        eventually(|| deliveries.lock().unwrap().len() == 2);
        let deliveries = deliveries.lock().unwrap();
        assert_eq!(deliveries.iter().map(|(v, _)| number(v)).collect::<Vec<_>>(), [1, 2]);
        assert!(Arc::ptr_eq(
            deliveries[0].0.as_ref().unwrap(),
            first.value.as_ref().unwrap()
        ));
        assert_eq!(deliveries[0].1, first.ops);
        assert!(Arc::ptr_eq(
            deliveries[1].0.as_ref().unwrap(),
            second.value.as_ref().unwrap()
        ));
        assert_eq!(deliveries[1].1, second.ops);
        assert_eq!(initial["value"], 0);
        assert_eq!(watch.stop(), WatchEnd::Stopped);
    });
}

#[test]
fn durable_session_watches_serialize_callbacks_and_buffer_frames_committed_while_one_is_in_flight() {
    each_backend(|backend| {
        let t = create_state(backend);
        let watch = watch(&t);
        let values = Arc::new(Mutex::new(Vec::new()));
        let active = Arc::new(Mutex::new((0, 0)));
        let (sink, counter) = (values.clone(), active.clone());
        let (entered_tx, entered) = mpsc::channel();
        let (release, release_rx) = mpsc::channel::<()>();
        watch
            .start(move |value, _| {
                {
                    let mut active = counter.lock().unwrap();
                    active.0 += 1;
                    active.1 = active.1.max(active.0);
                }
                sink.lock().unwrap().push(number(value));
                if sink.lock().unwrap().len() == 1 {
                    entered_tx.send(()).unwrap();
                    release_rx.recv_timeout(WAIT).unwrap();
                }
                counter.lock().unwrap().0 -= 1;
                Ok(())
            })
            .unwrap();
        set_state(&t, 1);
        entered.recv_timeout(WAIT).unwrap();
        for value in 2..=20 {
            set_state(&t, value);
        }
        assert_eq!(*values.lock().unwrap(), [1]);
        release.send(()).unwrap();
        eventually(|| values.lock().unwrap().len() == 20);
        assert_eq!(active.lock().unwrap().1, 1);
        assert_eq!(*values.lock().unwrap(), (1..=20).collect::<Vec<_>>());
        watch.stop();
    });
}

#[test]
fn durable_session_watches_allow_a_listener_to_initiate_a_later_session_commit() {
    each_backend(|backend| {
        let t = create_state(backend);
        let watch = watch(&t);
        let values = Arc::new(Mutex::new(Vec::new()));
        let (sink, session) = (values.clone(), t.session.clone());
        watch
            .start(move |value, _| {
                let current = number(value);
                sink.lock().unwrap().push(current);
                if current == 1 {
                    session
                        .commit(|tx| Ok(tx.doc(&state_doc(), Owner::Session)?.set(&path!["value"], json!(2))?))
                        .map_err(|error| error.to_string())?;
                }
                Ok(())
            })
            .unwrap();
        set_state(&t, 1);
        eventually(|| values.lock().unwrap().len() == 2);
        assert_eq!(*values.lock().unwrap(), [1, 2]);
        watch.stop();
    });
}

#[test]
fn durable_session_watches_collapse_101_pending_commits_to_one_root_replacement() {
    each_backend(|backend| {
        let t = create_state(backend);
        let watch = watch(&t);
        for value in 1..=101 {
            set_state(&t, value);
        }
        let deliveries = Arc::new(Mutex::new(Vec::new()));
        let sink = deliveries.clone();
        watch
            .start(move |value, ops| {
                sink.lock().unwrap().push((value.clone(), ops_json(ops)));
                Ok(())
            })
            .unwrap();
        eventually(|| !deliveries.lock().unwrap().is_empty());
        settle();
        let deliveries = deliveries.lock().unwrap();
        assert_eq!(deliveries.len(), 1);
        assert_eq!(number(&deliveries[0].0), 101);
        let current = watch.value().unwrap();
        assert_eq!(deliveries[0].1, [json!(["r", *current])]);
        watch.stop();
    });
}

#[test]
fn durable_session_watches_never_fold_the_in_flight_frame_into_an_overflow_reset() {
    each_backend(|backend| {
        let t = create_state(backend);
        let watch = watch(&t);
        let deliveries = Arc::new(Mutex::new(Vec::new()));
        let (listener, entered, release) = blocking_listener(deliveries.clone());
        watch.start(listener).unwrap();
        set_state(&t, 1);
        entered.recv_timeout(WAIT).unwrap();
        for value in 2..=102 {
            set_state(&t, value);
        }
        release.send(()).unwrap();
        eventually(|| deliveries.lock().unwrap().len() == 2);
        settle();
        let deliveries = deliveries.lock().unwrap();
        assert_eq!(deliveries.len(), 2);
        assert_eq!(number(&deliveries[0].0), 1);
        assert_eq!(number(&deliveries[1].0), 102);
        assert_eq!(deliveries[1].1, [json!(["r", *watch.value().unwrap()])]);
        watch.stop();
    });
}

#[test]
fn durable_session_watches_fold_retirement_into_an_overflow_reset_and_then_close() {
    each_backend(|backend| {
        let t = create_state(backend);
        let watch = watch(&t);
        for value in 1..=100 {
            set_state(&t, value);
        }
        t.session
            .commit(|tx| tx.retire_doc(&state_doc(), Owner::Session))
            .unwrap();
        let deliveries = Arc::new(Mutex::new(Vec::new()));
        let sink = deliveries.clone();
        watch
            .start(move |value, ops| {
                sink.lock().unwrap().push((value.clone(), ops_json(ops)));
                Ok(())
            })
            .unwrap();
        assert_eq!(watch.closed(WAIT), Some(WatchEnd::Retired));
        let deliveries = deliveries.lock().unwrap();
        assert_eq!(deliveries.len(), 1);
        assert_eq!(deliveries[0], (None, vec![json!(["r", null])]));
    });
}

#[test]
fn durable_session_watches_deliver_replayable_structural_no_op_commits() {
    each_backend(|backend| {
        let t = create_state(backend);
        let watch = watch(&t);
        let initial = watch.value().unwrap();
        t.session
            .commit(|tx| {
                let state = tx.doc(&state_doc(), Owner::Session)?;
                let first = state.shift(&path!["items"])?.unwrap();
                state.unshift(&path!["items"], vec![first])?;
                Ok(())
            })
            .unwrap();
        let newest = t.session.snapshot(&state_doc(), Owner::Session).unwrap().unwrap();
        assert!(!Arc::ptr_eq(&newest, &initial));
        assert_eq!(newest, initial);
        let batches = Arc::new(Mutex::new(Vec::new()));
        let (sink, expected) = (batches.clone(), newest.clone());
        watch
            .start(move |value, ops| {
                assert!(Arc::ptr_eq(value.as_ref().unwrap(), &expected));
                sink.lock().unwrap().push(ops_json(ops));
                Ok(())
            })
            .unwrap();
        eventually(|| !batches.lock().unwrap().is_empty());
        settle();
        assert_eq!(batches.lock().unwrap().len(), 1);
        assert!(!batches.lock().unwrap()[0].is_empty());
        watch.stop();
    });
}

#[test]
fn durable_session_watches_keep_earlier_immutable_revisions_stable() {
    each_backend(|backend| {
        let t = create_state(backend);
        let watch = watch(&t);
        let initial = watch.value().unwrap();
        let delivered = Arc::new(Mutex::new(None));
        let sink = delivered.clone();
        watch
            .start(move |value, _| {
                *sink.lock().unwrap() = value.clone();
                Ok(())
            })
            .unwrap();
        set_state(&t, 1);
        eventually(|| delivered.lock().unwrap().is_some());
        let delivered = delivered.lock().unwrap().clone().unwrap();
        assert!(Arc::ptr_eq(&delivered, watch.value().as_ref().unwrap()));
        assert!(!Arc::ptr_eq(&delivered, &initial));
        assert_eq!(initial["value"], 0);
        watch.stop();
    });
}

#[test]
fn durable_session_watches_deliver_retirement_and_do_not_follow_recreation() {
    each_backend(|backend| {
        let t = create_state(backend);
        let old = watch(&t);
        let values = Arc::new(Mutex::new(Vec::new()));
        let sink = values.clone();
        old.start(move |value, _| {
            sink.lock().unwrap().push(value.clone());
            Ok(())
        })
        .unwrap();
        t.session
            .commit(|tx| {
                tx.retire_doc(&state_doc(), Owner::Session)?;
                Ok(tx.doc(&state_doc(), Owner::Session)?.set(&path!["value"], json!(10))?)
            })
            .unwrap();
        assert_eq!(old.closed(WAIT), Some(WatchEnd::Retired));
        assert_eq!(*values.lock().unwrap(), [None]);
        assert_eq!(old.value(), None);
        let replacement = watch(&t);
        assert_eq!(number(&replacement.value()), 10);
        set_state(&t, 11);
        settle();
        assert_eq!(old.value(), None);
        replacement.stop();
    });
}

#[test]
fn durable_session_watches_discard_retirement_buffered_before_start_on_session_close() {
    each_backend(|backend| {
        let t = create_state(backend);
        let watch = watch(&t);
        let baseline = watch.value().unwrap();
        t.session
            .commit(|tx| tx.retire_doc(&state_doc(), Owner::Session))
            .unwrap();
        t.session.close().unwrap();
        assert_eq!(watch.closed(WAIT), Some(WatchEnd::SessionClosed));
        assert!(Arc::ptr_eq(watch.value().as_ref().unwrap(), &baseline));
        assert_eq!(watch.start(|_, _| Ok(())).unwrap_err().to_string(), "Watch is stopped");
    });
}

#[test]
fn durable_session_watches_discard_retirement_behind_an_in_flight_callback_on_session_close() {
    each_backend(|backend| {
        let t = create_state(backend);
        let watch = watch(&t);
        let deliveries = Arc::new(Mutex::new(Vec::new()));
        let (listener, entered, release) = blocking_listener(deliveries.clone());
        watch.start(listener).unwrap();
        set_state(&t, 1);
        entered.recv_timeout(WAIT).unwrap();
        t.session
            .commit(|tx| tx.retire_doc(&state_doc(), Owner::Session))
            .unwrap();
        t.session.close().unwrap();
        assert_eq!(watch.closed(WAIT), Some(WatchEnd::SessionClosed));
        release.send(()).unwrap();
        settle();
        let values: Vec<i64> = deliveries.lock().unwrap().iter().map(|(v, _)| number(v)).collect();
        assert_eq!(values, [1]);
    });
}

#[test]
fn durable_session_watches_support_idempotent_stop_and_reject_repeated_or_late_start() {
    each_backend(|backend| {
        let t = create_state(backend);
        let started = watch(&t);
        started.start(|_, _| Ok(())).unwrap();
        assert_eq!(
            started.start(|_, _| Ok(())).unwrap_err().to_string(),
            "Watch is already started"
        );
        assert_eq!(started.stop(), WatchEnd::Stopped);
        assert_eq!(started.stop(), WatchEnd::Stopped);
        let stopped = watch(&t);
        stopped.stop();
        assert_eq!(
            stopped.start(|_, _| Ok(())).unwrap_err().to_string(),
            "Watch is stopped"
        );
    });
}

#[test]
fn durable_session_watches_cancel_future_delivery_without_aborting_an_in_flight_callback() {
    each_backend(|backend| {
        let t = create_state(backend);
        let watch = watch(&t);
        let deliveries = Arc::new(Mutex::new(Vec::new()));
        let (listener, entered, release) = blocking_listener(deliveries.clone());
        watch.start(listener).unwrap();
        set_state(&t, 1);
        entered.recv_timeout(WAIT).unwrap();
        watch.cancel();
        assert_eq!(watch.closed(WAIT), Some(WatchEnd::Cancelled));
        set_state(&t, 2);
        release.send(()).unwrap();
        settle();
        assert_eq!(deliveries.lock().unwrap().len(), 1);
    });
}

#[test]
fn durable_session_watches_stop_on_session_close_without_joining_an_in_flight_callback() {
    each_backend(|backend| {
        let t = create_state(backend);
        let watch = watch(&t);
        let deliveries = Arc::new(Mutex::new(Vec::new()));
        let (listener, entered, release) = blocking_listener(deliveries.clone());
        watch.start(listener).unwrap();
        set_state(&t, 1);
        entered.recv_timeout(WAIT).unwrap();
        t.session.close().unwrap();
        assert_eq!(watch.closed(WAIT), Some(WatchEnd::SessionClosed));
        release.send(()).unwrap();
    });
}

#[test]
fn durable_session_watches_settle_listener_failure_on_only_the_affected_watch() {
    each_backend(|backend| {
        let t = create_state(backend);
        let failed = watch(&t);
        let healthy = watch(&t);
        failed.start(|_, _| Err("listener failed".into())).unwrap();
        let calls = Arc::new(Mutex::new(0));
        let counter = calls.clone();
        healthy
            .start(move |_, _| {
                *counter.lock().unwrap() += 1;
                Ok(())
            })
            .unwrap();
        set_state(&t, 1);
        let end = failed.closed(WAIT).unwrap();
        assert_eq!(end.reason(), "listener_error");
        assert_eq!(end, WatchEnd::ListenerError("listener failed".into()));
        eventually(|| *calls.lock().unwrap() == 1);
        assert_eq!(healthy.end(), None);
        healthy.stop();
    });
}

#[test]
fn durable_session_watches_hydrate_migration_without_writing_and_observe_the_later_exact_edit() {
    each_backend(|backend| {
        let old = DocDefinition::new("watch.migration", 1, DocSemantics::Session, |_| json!({ "value": 3 }))
            .define()
            .unwrap();
        let current = DocDefinition::new(
            "watch.migration",
            2,
            DocSemantics::Session,
            |_| json!({ "value": 0, "migrated": false }),
        )
        .migrate(|value, _| Ok(json!({ "value": value["value"], "migrated": true })))
        .define()
        .unwrap();
        let t = TestSession::open(backend);
        t.session.commit(|tx| tx.doc(&old, Owner::Session).map(drop)).unwrap();
        t.session.unload_documents().unwrap();
        let commits = t.storage.commit_count();
        let watch = t.session.watch_doc(&current, Owner::Session).unwrap().unwrap();
        assert_eq!(*watch.value().unwrap(), json!({ "value": 3, "migrated": true }));
        assert_eq!(t.storage.commit_count(), commits);
        let values = Arc::new(Mutex::new(Vec::new()));
        let sink = values.clone();
        watch
            .start(move |value, _| {
                sink.lock().unwrap().push(number(value));
                Ok(())
            })
            .unwrap();
        t.session
            .commit(|tx| tx.doc(&current, Owner::Session).map(drop))
            .unwrap();
        settle();
        assert!(values.lock().unwrap().is_empty());
        t.session
            .commit(|tx| Ok(tx.doc(&current, Owner::Session)?.set(&path!["value"], json!(4))?))
            .unwrap();
        eventually(|| !values.lock().unwrap().is_empty());
        assert_eq!(*values.lock().unwrap(), [4]);
        watch.stop();
    });
}

#[test]
fn durable_session_watches_replay_every_delivered_exact_operation_batch_from_the_acquisition_revision() {
    each_backend(|backend| {
        let t = create_state(backend);
        let watch = watch(&t);
        let replica = Arc::new(Mutex::new((*watch.value().unwrap()).clone()));
        let (shared, delivered) = (replica.clone(), Arc::new(Mutex::new(0)));
        let count = delivered.clone();
        watch
            .start(move |value, ops| {
                let mut replica = shared.lock().unwrap();
                *replica = apply_immutable(&replica, ops).map_err(|error| error.to_string())?;
                assert_eq!(*replica, **value.as_ref().unwrap());
                *count.lock().unwrap() += 1;
                Ok(())
            })
            .unwrap();
        t.session
            .commit(|tx| {
                let state = tx.doc(&state_doc(), Owner::Session)?;
                state.set(&path!["value"], json!(3))?;
                state.splice(&path!["items"], 0, 1, Vec::new())?;
                Ok(())
            })
            .unwrap();
        eventually(|| *delivered.lock().unwrap() == 1);
        assert_eq!(*replica.lock().unwrap(), *watch.value().unwrap());
        assert_eq!(
            *replica.lock().unwrap(),
            json!({ "value": 3, "items": ["b"], "retained": { "label": "stable" } })
        );
        watch.stop();
    });
}

#[test]
fn durable_session_watches_continue_after_the_tracker_cache_unloads() {
    each_backend(|backend| {
        let t = create_state(backend);
        let watch = watch(&t);
        let baseline = watch.value().unwrap();
        let reads = t.storage.document_reads();
        t.session.unload_documents().unwrap();
        let reloaded = t.session.snapshot(&state_doc(), Owner::Session).unwrap().unwrap();
        assert_eq!(reloaded, baseline);
        assert!(!Arc::ptr_eq(&reloaded, &baseline));
        assert!(t.storage.document_reads() > reads);
        watch.start(|_, _| Ok(())).unwrap();
        set_state(&t, 7);
        eventually(|| number(&watch.value()) == 7);
        watch.stop();
    });
}

#[test]
fn durable_session_watches_end_with_the_failure_when_storage_fails_the_session() {
    each_backend(|backend| {
        let t = create_state(backend);
        let watch = watch(&t);
        let failure = durable::StorageError::Failed("disk vanished".into());
        t.storage.fail_next_commit(failure.clone());
        t.session
            .commit(|tx| Ok(tx.doc(&state_doc(), Owner::Session)?.set(&path!["value"], json!(1))?))
            .unwrap_err();
        assert_eq!(
            watch.closed(WAIT),
            Some(WatchEnd::SessionFailed(durable::SessionError::Storage(failure)))
        );
    });
}
