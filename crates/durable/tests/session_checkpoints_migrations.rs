//! Port of upstream `test/session-checkpoints-migrations.test.ts`, run against every backend.
//! Not ported: strict-JSON checks of migration results (a `serde_json::Value` is always strict JSON) and the
//! `documentState` half of the older-shape observer test (Chord replicated state is not ported).

#![allow(clippy::type_complexity)]

mod session_support;

use chord_delta::{Op, path};
use durable::{DocDefinition, DocSemantics, DocumentFork, DocumentHistory, EntryDraft, Owner, SessionError, Storage};
use serde_json::{Value, json};
use session_support::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

fn session_doc(kind: &str, version: u64, initial: Value) -> DocDefinition {
    DocDefinition::new(kind, version, DocSemantics::Session, move |_| initial.clone())
}

fn counter() -> Arc<AtomicUsize> {
    Arc::new(AtomicUsize::new(0))
}

fn touch(t: &TestSession, doc: &durable::Doc) {
    t.session.commit(|tx| tx.doc(doc, Owner::Session).map(drop)).unwrap();
}

fn bump(t: &TestSession, doc: &durable::Doc, field: &str) {
    t.session
        .commit(|tx| {
            let draft = tx.doc(doc, Owner::Session)?;
            let count = draft.get(&path![field])?.unwrap().as_i64().unwrap();
            Ok(draft.set(&path![field], json!(count + 1))?)
        })
        .unwrap();
}

fn set(t: &TestSession, doc: &durable::Doc, field: &str, value: Value) {
    t.session
        .commit(|tx| Ok(tx.doc(doc, Owner::Session)?.set(&path![field], value)?))
        .unwrap();
}

fn read(t: &TestSession, doc: &durable::Doc) -> Option<Arc<Value>> {
    t.session.snapshot(doc, Owner::Session).unwrap()
}

/// `(type, content.kind)` of every document write of one commit.
fn document_writes(writes: &[durable::StorageWrite]) -> Vec<Value> {
    writes
        .iter()
        .map(json)
        .filter(|w| {
            matches!(
                w["type"].as_str(),
                Some("document.create" | "document.change" | "document.retire")
            )
        })
        .collect()
}

fn change_kinds(t: &TestSession) -> Vec<String> {
    t.storage
        .commits()
        .iter()
        .flatten()
        .map(json)
        .filter(|w| w["type"] == "document.change")
        .map(|w| w["content"]["kind"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn durable_session_checkpoints_call_the_predicate_with_its_definition() {
    each_backend(|backend| {
        let doc = session_doc("checkpoint.receiver", 1, json!({ "count": 0 }))
            .checkpoint_when(|definition, value, _, _| {
                Ok(value["count"].as_i64().unwrap()
                    == (definition.initial)(&Value::Null)["count"].as_i64().unwrap() + 2)
            })
            .define()
            .unwrap();
        let t = TestSession::open(backend);
        for count in 0..=2 {
            set(&t, &doc, "count", json!(count));
        }
        assert_eq!(change_kinds(&t), ["delta", "base"]);
    });
}

#[test]
fn durable_session_checkpoints_select_bases_only_for_nonempty_batches_and_pass_the_prepared_revision_and_ops() {
    each_backend(|backend| {
        let calls: Arc<Mutex<Vec<(Value, Vec<Value>)>>> = Arc::default();
        let false_calls: Arc<Mutex<Vec<(Value, Vec<Value>)>>> = Arc::default();
        let (sink, false_sink) = (calls.clone(), false_calls.clone());
        let ops_json = |ops: &[Op]| ops.iter().map(Op::to_json).collect::<Vec<_>>();
        let base_doc = session_doc("checkpoint.base", 1, json!({ "items": [] }))
            .checkpoint_when(move |_, value, ops, _| {
                sink.lock().unwrap().push((value.clone(), ops_json(ops)));
                Ok(true)
            })
            .define()
            .unwrap();
        let delta_doc = session_doc("checkpoint.delta", 1, json!({ "count": 0 }))
            .checkpoint_when(move |_, value, ops, _| {
                false_sink.lock().unwrap().push((value.clone(), ops_json(ops)));
                Ok(false)
            })
            .define()
            .unwrap();
        let default_doc = session_doc("checkpoint.default", 1, json!({ "count": 0 }))
            .define()
            .unwrap();
        let t = TestSession::open(backend);
        t.session
            .commit(|tx| {
                for doc in [&base_doc, &delta_doc, &default_doc] {
                    tx.doc(doc, Owner::Session)?;
                }
                Ok(())
            })
            .unwrap();
        assert!(calls.lock().unwrap().is_empty() && false_calls.lock().unwrap().is_empty());
        assert!(
            document_writes(&t.storage.last_commit())
                .iter()
                .all(|w| w["type"] == "document.create" && w["content"]["kind"] == "base")
        );

        t.session
            .commit(|tx| {
                tx.doc(&base_doc, Owner::Session)?
                    .push(&path!["items"], vec![json!("x")])?;
                tx.doc(&delta_doc, Owner::Session)?.set(&path!["count"], json!(1))?;
                tx.doc(&default_doc, Owner::Session)?.set(&path!["count"], json!(1))?;
                Ok(())
            })
            .unwrap();
        let writes: Vec<Value> = document_writes(&t.storage.last_commit());
        assert_eq!(
            writes.iter().map(|w| w["content"]["kind"].clone()).collect::<Vec<_>>(),
            [json!("base"), json!("delta"), json!("delta")]
        );
        let (calls, false_calls) = (calls.lock().unwrap(), false_calls.lock().unwrap());
        assert_eq!(calls.len(), 1);
        assert_eq!(false_calls.len(), 1);
        let snapshot = read(&t, &base_doc).unwrap();
        let delta_snapshot = read(&t, &delta_doc).unwrap();
        let published = document_changes(&t.last_publication());
        assert_eq!(calls[0].0, *snapshot);
        assert_eq!(calls[0].1, published[0].ops);
        assert_eq!(calls[0].1, [json!(["p", ["items"], 0, 0, ["x"]])]);
        assert_eq!(false_calls[0].0, *delta_snapshot);
        assert_eq!(false_calls[0].1, published[1].ops);
        assert_eq!(Value::Array(false_calls[0].1.clone()), writes[1]["content"]["ops"]);
        assert_eq!(writes[0]["content"]["value"], *snapshot);
    });
}

#[test]
fn durable_session_checkpoints_pass_the_stored_delta_count_since_the_newest_base() {
    each_backend(|backend| {
        let seen: Arc<Mutex<Vec<usize>>> = Arc::default();
        let (v1_seen, v2_seen) = (seen.clone(), seen.clone());
        let v1 = session_doc("checkpoint.deltas-since-base", 1, json!({ "count": 0 }))
            .checkpoint_when(move |_, _, _, info| {
                v1_seen.lock().unwrap().push(info.deltas_since_base);
                Ok(info.deltas_since_base >= 2)
            })
            .define()
            .unwrap();
        let v2 = session_doc("checkpoint.deltas-since-base", 2, json!({ "count": 0 }))
            .migrate(|value, _| Ok(json!({ "count": value["count"] })))
            .checkpoint_when(move |_, _, _, info| {
                v2_seen.lock().unwrap().push(info.deltas_since_base);
                Ok(false)
            })
            .define()
            .unwrap();
        let t = TestSession::open(backend);
        touch(&t, &v1);
        bump(&t, &v1, "count");
        bump(&t, &v1, "count");
        bump(&t, &v1, "count");
        t.session.unload_documents().unwrap();
        bump(&t, &v1, "count");
        assert_eq!(*seen.lock().unwrap(), [0, 1, 2, 0]);
        let commits = t.storage.commits();
        let kinds: Vec<Value> = commits[commits.len() - 4..]
            .iter()
            .map(|writes| json(&writes[0])["content"]["kind"].clone())
            .collect();
        assert_eq!(kinds, [json!("delta"), json!("delta"), json!("base"), json!("delta")]);
        // A required version base resets the count without calling the predicate.
        bump(&t, &v2, "count");
        bump(&t, &v2, "count");
        assert_eq!(*seen.lock().unwrap(), [0, 1, 2, 0, 0]);
        let commits = t.storage.commits();
        assert_matches(
            &json(&commits[commits.len() - 2][0]),
            json!({ "content": { "kind": "base", "version": 2 } }),
        );
    });
}

#[test]
fn durable_session_checkpoints_skip_the_predicate_for_empty_batches_but_call_it_for_structural_no_ops() {
    each_backend(|backend| {
        let calls = counter();
        let counted = calls.clone();
        let doc = session_doc("checkpoint.no-op", 1, json!({ "items": ["a", "b"] }))
            .checkpoint_when(move |_, _, _, _| {
                counted.fetch_add(1, Ordering::SeqCst);
                Ok(false)
            })
            .define()
            .unwrap();
        let t = TestSession::open(backend);
        touch(&t, &doc);
        let commits = t.storage.commit_count();
        t.session
            .commit(|tx| {
                let value = tx.doc(&doc, Owner::Session)?;
                value.push(&path!["items"], vec![json!("x")])?;
                value.pop(&path!["items"])?;
                Ok(())
            })
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(t.storage.commit_count(), commits);
        t.session
            .commit(|tx| {
                let value = tx.doc(&doc, Owner::Session)?;
                let first = value.shift(&path!["items"])?.unwrap();
                value.unshift(&path!["items"], vec![first])?;
                Ok(())
            })
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_matches(
            &json(&t.storage.last_commit()[0]),
            json!({ "type": "document.change", "content": { "kind": "delta" } }),
        );
    });
}

#[test]
fn durable_session_checkpoints_roll_back_every_prepared_document_when_a_predicate_fails() {
    each_backend(|backend| {
        let throw = Arc::new(AtomicBool::new(true));
        let first_calls = counter();
        let counted = first_calls.clone();
        let first = session_doc("checkpoint.rollback.first", 1, json!({ "count": 0 }))
            .checkpoint_when(move |_, _, _, _| {
                counted.fetch_add(1, Ordering::SeqCst);
                Ok(false)
            })
            .define()
            .unwrap();
        let throwing = throw.clone();
        let second = session_doc("checkpoint.rollback.second", 1, json!({ "count": 0 }))
            .checkpoint_when(move |_, _, _, _| {
                if throwing.load(Ordering::SeqCst) {
                    Err("checkpoint failed".into())
                } else {
                    Ok(false)
                }
            })
            .define()
            .unwrap();
        let t = TestSession::open(backend);
        t.session
            .commit(|tx| {
                tx.doc(&first, Owner::Session)?;
                tx.doc(&second, Owner::Session)?;
                Ok(())
            })
            .unwrap();
        let (first_value, second_value) = (read(&t, &first).unwrap(), read(&t, &second).unwrap());
        let (commits, published) = (t.storage.commit_count(), t.publication_count());
        let error = t
            .session
            .commit(|tx| {
                tx.doc(&first, Owner::Session)?.set(&path!["count"], json!(1))?;
                tx.doc(&second, Owner::Session)?.set(&path!["count"], json!(2))?;
                Ok(())
            })
            .unwrap_err();
        assert_eq!(error, SessionError::Error("checkpoint failed".into()));
        assert_eq!(first_calls.load(Ordering::SeqCst), 1);
        assert_eq!(t.storage.commit_count(), commits);
        assert_eq!(t.publication_count(), published);
        assert!(Arc::ptr_eq(&read(&t, &first).unwrap(), &first_value));
        assert!(Arc::ptr_eq(&read(&t, &second).unwrap(), &second_value));
        throw.store(false, Ordering::SeqCst);
        t.session
            .commit(|tx| {
                tx.doc(&first, Owner::Session)?.set(&path!["count"], json!(3))?;
                tx.doc(&second, Owner::Session)?.set(&path!["count"], json!(4))?;
                Ok(())
            })
            .unwrap();
        assert_eq!(*read(&t, &first).unwrap(), json!({ "count": 3 }));
        assert_eq!(*read(&t, &second).unwrap(), json!({ "count": 4 }));
    });
}

#[test]
fn durable_session_checkpoints_persist_repeated_false_decisions_as_deltas_and_replay_the_tail() {
    each_backend(|backend| {
        let calls = counter();
        let counted = calls.clone();
        let doc = session_doc("checkpoint.tail", 1, json!({ "values": [] }))
            .checkpoint_when(move |_, _, _, _| {
                counted.fetch_add(1, Ordering::SeqCst);
                Ok(false)
            })
            .define()
            .unwrap();
        let t = TestSession::open(backend);
        touch(&t, &doc);
        for value in 1..=8 {
            t.session
                .commit(|tx| {
                    Ok(tx
                        .doc(&doc, Owner::Session)?
                        .push(&path!["values"], vec![json!(value)])
                        .map(drop)?)
                })
                .unwrap();
        }
        assert_eq!(change_kinds(&t), vec!["delta"; 8]);
        assert_eq!(calls.load(Ordering::SeqCst), 8);
        t.session.unload_documents().unwrap();
        assert_eq!(*read(&t, &doc).unwrap(), json!({ "values": [1, 2, 3, 4, 5, 6, 7, 8] }));
    });
}

#[test]
fn durable_session_checkpoints_keep_a_prepared_root_replacement_as_a_delta_when_the_predicate_is_false() {
    each_backend(|backend| {
        let initial: serde_json::Map<String, Value> = (0..4_100).map(|i| (format!("field{i}"), json!(0))).collect();
        let doc = session_doc("checkpoint.root-replacement", 1, Value::Object(initial))
            .checkpoint_when(|_, _, _, _| Ok(false))
            .define()
            .unwrap();
        let t = TestSession::open(backend);
        touch(&t, &doc);
        t.session
            .commit(|tx| {
                let value = tx.doc(&doc, Owner::Session)?;
                for index in 0..4_100 {
                    value.set(&path![format!("field{index}")], json!(1))?;
                }
                Ok(())
            })
            .unwrap();
        let write = json(&t.storage.last_commit()[0]);
        assert_eq!(write["type"], "document.change");
        assert_eq!(write["content"]["kind"], "delta");
        let ops = write["content"]["ops"].as_array().unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0][0], "r");
        t.session.unload_documents().unwrap();
        assert_eq!(read(&t, &doc).unwrap()["field4099"], 1);
    });
}

#[test]
fn durable_session_checkpoints_use_ordinary_checkpoint_selection_before_retirement() {
    each_backend(|backend| {
        let calls = counter();
        let counted = calls.clone();
        let doc = session_doc("checkpoint.retire", 1, json!({ "count": 0 }))
            .checkpoint_when(move |_, _, _, _| {
                counted.fetch_add(1, Ordering::SeqCst);
                Ok(true)
            })
            .define()
            .unwrap();
        let t = TestSession::open(backend);
        touch(&t, &doc);
        t.session
            .commit(|tx| {
                tx.doc(&doc, Owner::Session)?.set(&path!["count"], json!(1))?;
                tx.retire_doc(&doc, Owner::Session)
            })
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let writes = document_writes(&t.storage.last_commit());
        assert_eq!(writes.len(), 2);
        assert_matches(
            &writes[0],
            json!({ "type": "document.change", "content": { "kind": "base" } }),
        );
        assert_matches(&writes[1], json!({ "type": "document.retire" }));
    });
}

#[test]
fn durable_session_migrations_migrate_read_only_once_per_cold_load_and_write_nothing() {
    each_backend(|backend| {
        let old = session_doc("migration.read-only", 1, json!({ "count": 2 }))
            .define()
            .unwrap();
        let calls = counter();
        let counted = calls.clone();
        let current = session_doc("migration.read-only", 3, json!({ "count": 0, "labels": [] }))
            .migrate(move |value, from| {
                assert_eq!(from, 1);
                counted.fetch_add(1, Ordering::SeqCst);
                Ok(json!({ "count": value["count"], "labels": ["migrated"] }))
            })
            .define()
            .unwrap();
        let t = TestSession::open(backend);
        touch(&t, &old);
        t.session.unload_documents().unwrap();
        let commits = t.storage.commit_count();
        let first = read(&t, &current).unwrap();
        assert!(Arc::ptr_eq(&read(&t, &current).unwrap(), &first));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(t.storage.commit_count(), commits);
        assert_eq!(*first, json!({ "count": 2, "labels": ["migrated"] }));
        t.session.unload_documents().unwrap();
        let second = read(&t, &current).unwrap();
        assert!(!Arc::ptr_eq(&second, &first));
        assert_eq!(second, first);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(t.storage.commit_count(), commits);
    });
}

#[test]
fn durable_session_migrations_write_the_required_base_on_the_first_transaction_then_deltas() {
    each_backend(|backend| {
        let old = session_doc("migration.transition", 1, json!({ "count": 4 }))
            .define()
            .unwrap();
        let checkpoints = counter();
        let counted = checkpoints.clone();
        let current = session_doc("migration.transition", 3, json!({ "count": 0 }))
            .migrate(|value, from| Ok(json!({ "count": value["count"].as_u64().unwrap() + from - 1 })))
            .checkpoint_when(move |_, _, _, _| {
                counted.fetch_add(1, Ordering::SeqCst);
                Ok(false)
            })
            .define()
            .unwrap();
        let t = TestSession::open(backend);
        touch(&t, &old);
        t.session.unload_documents().unwrap();
        // An observer of the older shape.
        let watch = t.session.watch_doc(&old, Owner::Session).unwrap().unwrap();
        let frames: Arc<Mutex<Vec<(Value, Vec<Value>)>>> = Arc::default();
        let sink = frames.clone();
        watch
            .start(move |value, ops| {
                let value = value.as_deref().cloned().unwrap_or(Value::Null);
                sink.lock()
                    .unwrap()
                    .push((value, ops.iter().map(Op::to_json).collect()));
                Ok(())
            })
            .unwrap();
        assert_eq!(*read(&t, &current).unwrap(), json!({ "count": 4 }));
        let snapshot = read(&t, &current).unwrap();
        // An observer of the new shape: the migration changes nothing it sees.
        let current_watch = t.session.watch_doc(&current, Owner::Session).unwrap().unwrap();
        let current_frames = Arc::new(Mutex::new(0));
        let current_sink = current_frames.clone();
        current_watch
            .start(move |_, _| {
                *current_sink.lock().unwrap() += 1;
                Ok(())
            })
            .unwrap();
        touch(&t, &current);
        assert_matches(
            &json(&t.storage.last_commit()[0]),
            json!({ "type": "document.change", "content": { "kind": "base", "version": 3, "value": { "count": 4 } } }),
        );
        assert_eq!(checkpoints.load(Ordering::SeqCst), 0);
        // The migration-only base is published, so the older-shape watch receives the new value as a root replacement.
        let published = document_changes(&t.last_publication());
        assert_eq!(published.len(), 1);
        assert_eq!(published[0].version, Some(3));
        assert_eq!(**published[0].value.as_ref().unwrap(), json!({ "count": 4 }));
        assert!(published[0].ops.is_empty());
        eventually(|| frames.lock().unwrap().len() == 1);
        settle();
        assert_eq!(
            *frames.lock().unwrap(),
            [(json!({ "count": 4 }), vec![json!(["r", { "count": 4 }])])]
        );
        assert_eq!(*current_frames.lock().unwrap(), 0);
        watch.stop();
        current_watch.stop();
        assert!(Arc::ptr_eq(&read(&t, &current).unwrap(), &snapshot));

        set(&t, &current, "count", json!(7));
        assert_matches(
            &json(&t.storage.last_commit()[0]),
            json!({ "type": "document.change", "content": { "kind": "delta", "version": 3 } }),
        );
        assert_eq!(checkpoints.load(Ordering::SeqCst), 1);
        t.session.unload_documents().unwrap();
        assert_eq!(*read(&t, &current).unwrap(), json!({ "count": 7 }));
    });
}

#[test]
fn durable_session_migrations_roll_migration_and_edits_back_then_coalesce_later_edits_into_one_base() {
    each_backend(|backend| {
        let old = session_doc("migration.rollback", 1, json!({ "count": 1 }))
            .define()
            .unwrap();
        let migrations = counter();
        let counted = migrations.clone();
        let current = session_doc("migration.rollback", 2, json!({ "count": 0, "migrated": false }))
            .migrate(move |value, _| {
                counted.fetch_add(1, Ordering::SeqCst);
                Ok(json!({ "count": value["count"], "migrated": true }))
            })
            .define()
            .unwrap();
        let t = TestSession::open(backend);
        touch(&t, &old);
        t.session.unload_documents().unwrap();
        let commits = t.storage.commit_count();
        let error = t
            .session
            .commit(|tx| {
                tx.doc(&current, Owner::Session)?.set(&path!["count"], json!(8))?;
                Err::<(), _>(SessionError::Error("rollback".into()))
            })
            .unwrap_err();
        assert_eq!(error.to_string(), "rollback");
        assert_eq!(t.storage.commit_count(), commits);
        assert_eq!(*read(&t, &current).unwrap(), json!({ "count": 1, "migrated": true }));
        assert_eq!(migrations.load(Ordering::SeqCst), 1);
        t.session
            .commit(|tx| {
                let value = tx.doc(&current, Owner::Session)?;
                value.set(&path!["count"], json!(9))?;
                value.set(&path!["migrated"], json!(false))?;
                Ok(())
            })
            .unwrap();
        assert_eq!(
            document_writes(&t.storage.last_commit()),
            [
                json!({ "type": "document.change", "id": json(&t.storage.last_commit()[0])["id"], "content": { "kind": "base", "version": 2, "value": { "count": 9, "migrated": false } } })
            ]
        );
        let published = document_changes(&t.last_publication()).remove(0);
        assert!(Arc::ptr_eq(
            published.value.as_ref().unwrap(),
            &read(&t, &current).unwrap()
        ));
        assert_eq!(
            published.ops,
            [json!(["s", ["count"], 9]), json!(["s", ["migrated"], false])]
        );
        assert_eq!(migrations.load(Ordering::SeqCst), 1);
    });
}

#[test]
fn durable_session_migrations_reject_newer_stored_versions_and_older_versions_without_migration() {
    each_backend(|backend| {
        let v2 = session_doc("migration.compatibility", 2, json!({ "count": 2 }))
            .define()
            .unwrap();
        let v1 = session_doc("migration.compatibility", 1, json!({ "count": 1 }))
            .define()
            .unwrap();
        let v3 = session_doc("migration.compatibility", 3, json!({ "count": 3 }))
            .define()
            .unwrap();
        let t = TestSession::open(backend);
        touch(&t, &v2);
        t.session.unload_documents().unwrap();
        let commits = t.storage.commit_count();
        let id = json(&t.storage.last_commit()[0])["record"]["id"].clone();
        let newer = format!("Document {id} (migration.compatibility) has newer version 2 than 1");
        assert_eq!(t.session.snapshot(&v1, Owner::Session).unwrap_err().to_string(), newer);
        assert_eq!(
            t.session
                .commit(|tx| tx.doc(&v1, Owner::Session).map(drop))
                .unwrap_err()
                .to_string(),
            newer
        );
        let missing = format!("Document {id} (migration.compatibility) requires migration from version 2");
        assert_eq!(
            t.session.snapshot(&v3, Owner::Session).unwrap_err().to_string(),
            missing
        );
        assert_eq!(
            t.session
                .commit(|tx| tx.doc(&v3, Owner::Session).map(drop))
                .unwrap_err()
                .to_string(),
            missing
        );
        assert_eq!(t.storage.commit_count(), commits);
    });
}

#[test]
fn durable_session_migrations_persist_a_required_base_before_retirement_without_the_predicate() {
    each_backend(|backend| {
        let old = session_doc("migration.retire", 1, json!({ "count": 1 }))
            .define()
            .unwrap();
        let current = session_doc("migration.retire", 2, json!({ "count": 0 }))
            .migrate(|value, _| Ok(json!({ "count": value["count"] })))
            .checkpoint_when(|_, _, _, _| Err("must not run".into()))
            .define()
            .unwrap();
        let t = TestSession::open(backend);
        touch(&t, &old);
        t.session.unload_documents().unwrap();
        t.session
            .commit(|tx| {
                tx.doc(&current, Owner::Session)?;
                tx.retire_doc(&current, Owner::Session)
            })
            .unwrap();
        let writes = document_writes(&t.storage.last_commit());
        assert_eq!(writes.len(), 2);
        assert_matches(
            &writes[0],
            json!({ "type": "document.change", "content": { "kind": "base", "version": 2, "value": { "count": 1 } } }),
        );
        assert_matches(&writes[1], json!({ "type": "document.retire" }));
    });
}

#[test]
fn durable_session_migrations_leave_unaccessed_older_documents_untouched() {
    each_backend(|backend| {
        let first_v1 = session_doc("migration.lazy.first", 1, json!({ "count": 1 }))
            .define()
            .unwrap();
        let second_v1 = session_doc("migration.lazy.second", 1, json!({ "count": 2 }))
            .define()
            .unwrap();
        let first_v2 = session_doc("migration.lazy.first", 2, json!({ "count": 0 }))
            .migrate(|value, _| Ok(json!({ "count": value["count"] })))
            .define()
            .unwrap();
        let t = TestSession::open(backend);
        t.session
            .commit(|tx| {
                tx.doc(&first_v1, Owner::Session)?;
                tx.doc(&second_v1, Owner::Session)?;
                Ok(())
            })
            .unwrap();
        t.session.unload_documents().unwrap();
        let commits = t.storage.commit_count();
        assert_eq!(*read(&t, &first_v2).unwrap(), json!({ "count": 1 }));
        assert_eq!(t.storage.commit_count(), commits);
        let address = durable::DocumentAddress {
            kind: "migration.lazy.second".into(),
            scope: durable::DocumentScope::Session,
            key: None,
        };
        let record = t
            .storage
            .find_document(&address, durable::DocumentPoint::Current)
            .unwrap()
            .unwrap();
        let stored = t
            .storage
            .document(record.identity.id, durable::DocumentPoint::Current)
            .unwrap()
            .unwrap();
        assert_eq!(stored.version, 1);
    });
}

const REWINDABLE: DocSemantics = DocSemantics::Conversation {
    history: DocumentHistory::Rewindable,
    fork: DocumentFork::AsOf,
};

#[test]
fn durable_session_history_migrates_current_and_historical_values_independently_and_follows_fork_ancestry() {
    each_backend(|backend| {
        let v1 = DocDefinition::new("history.migration", 1, REWINDABLE, |_| json!({ "count": 0 }))
            .define()
            .unwrap();
        let family = DocDefinition::new(
            "history.family",
            1,
            REWINDABLE,
            |seed| json!({ "seed": seed, "count": 0 }),
        )
        .family()
        .define()
        .unwrap();
        let migrations: Arc<Mutex<Vec<u64>>> = Arc::default();
        let recorded = migrations.clone();
        let v3 = DocDefinition::new(
            "history.migration",
            3,
            REWINDABLE,
            |_| json!({ "count": 0, "version": 3 }),
        )
        .migrate(move |value, from| {
            recorded.lock().unwrap().push(from);
            Ok(json!({ "count": value["count"], "version": 3 }))
        })
        .define()
        .unwrap();
        let t = TestSession::open(backend);
        let c = t.create_conversation();
        let owner = Owner::Conversation(c);
        let first = t
            .session
            .commit(|tx| {
                let at = tx.append_entry(c, EntryDraft::new("first"))?.id;
                tx.doc(&v1, owner)?.set(&path!["count"], json!(1))?;
                tx.doc_member(&family, owner, "member", json!("seed"))?
                    .set(&path!["count"], json!(1))?;
                Ok(at)
            })
            .unwrap();
        let second = t
            .session
            .commit(|tx| {
                let at = tx.append_entry(c, EntryDraft::new("second"))?.id;
                tx.doc(&v1, owner)?.set(&path!["count"], json!(2))?;
                Ok(at)
            })
            .unwrap();
        t.session.unload_documents().unwrap();
        let commits = t.storage.commit_count();
        assert_eq!(
            *t.session.snapshot(&v3, owner).unwrap().unwrap(),
            json!({ "count": 2, "version": 3 })
        );
        assert_eq!(*migrations.lock().unwrap(), [1]);
        assert_eq!(t.storage.commit_count(), commits);
        let third = t
            .session
            .commit(|tx| {
                let at = tx.append_entry(c, EntryDraft::new("third"))?.id;
                tx.doc(&v3, owner)?;
                Ok(at)
            })
            .unwrap();
        assert!(t.storage.last_commit().iter().map(json).any(|w| matches(
            &w,
            &json!({ "type": "document.change", "content": { "kind": "base", "version": 3 } })
        )));
        let as_of = |conversation, key: Option<&str>, at| {
            let doc = if key.is_some() { &family } else { &v3 };
            t.session
                .snapshot_as_of(doc, conversation, key, at)
                .map(|value| value.map(|value| (*value).clone()))
        };
        assert_eq!(
            as_of(c, None, first).unwrap(),
            Some(json!({ "count": 1, "version": 3 }))
        );
        assert_eq!(
            as_of(c, None, second).unwrap(),
            Some(json!({ "count": 2, "version": 3 }))
        );
        assert_eq!(
            as_of(c, None, third).unwrap(),
            Some(json!({ "count": 2, "version": 3 }))
        );
        assert_eq!(*migrations.lock().unwrap(), [1, 1, 1]);
        assert_eq!(
            as_of(c, Some("member"), first).unwrap(),
            Some(json!({ "seed": "seed", "count": 1 }))
        );
        let child = t
            .session
            .commit(|tx| {
                Ok(tx
                    .fork_conversation(c, second, durable::ConversationOwnership::Ownerless)?
                    .id)
            })
            .unwrap();
        assert_eq!(
            as_of(child, None, first).unwrap(),
            Some(json!({ "count": 1, "version": 3 }))
        );
        assert_eq!(
            as_of(child, None, second).unwrap(),
            Some(json!({ "count": 2, "version": 3 }))
        );
        assert_eq!(
            as_of(child, Some("member"), first).unwrap(),
            Some(json!({ "seed": "seed", "count": 1 }))
        );
        assert_eq!(
            as_of(child, None, third).unwrap_err().to_string(),
            format!("Entry {} is not visible from conversation {}", third.0, child.0)
        );
    });
}

#[test]
fn durable_session_history_selects_the_incarnation_alive_at_the_entry_commit() {
    each_backend(|backend| {
        let doc = DocDefinition::new("history.incarnation", 1, REWINDABLE, |_| json!({ "value": "initial" }))
            .define()
            .unwrap();
        let t = TestSession::open(backend);
        let c = t.create_conversation();
        let owner = Owner::Conversation(c);
        let entry = |kind: &str, change: &dyn Fn(&mut durable::Tx<'_>) -> Result<(), SessionError>| {
            t.session
                .commit(|tx| {
                    let at = tx.append_entry(c, EntryDraft::new(kind))?.id;
                    change(tx)?;
                    Ok(at)
                })
                .unwrap()
        };
        let before = entry("before", &|_| Ok(()));
        let created = entry("create", &|tx| {
            Ok(tx.doc(&doc, owner)?.set(&path!["value"], json!("old"))?)
        });
        let retired = entry("retire", &|tx| tx.retire_doc(&doc, owner));
        let recreated = entry("recreate", &|tx| {
            Ok(tx.doc(&doc, owner)?.set(&path!["value"], json!("new"))?)
        });
        let as_of = |at| {
            t.session
                .snapshot_as_of(&doc, c, None, at)
                .map(|v| v.map(|v| (*v).clone()))
        };
        assert_eq!(as_of(before).unwrap(), None);
        assert_eq!(as_of(created).unwrap(), Some(json!({ "value": "old" })));
        assert_eq!(as_of(retired).unwrap(), None);
        assert_eq!(as_of(recreated).unwrap(), Some(json!({ "value": "new" })));
        t.session.close().unwrap();
        assert_eq!(as_of(recreated).unwrap_err().to_string(), "Session is closed");
    });
}

fn v1_doc() -> durable::Doc {
    session_doc("cache.versioned", 1, json!({ "name": "first" }))
        .define()
        .unwrap()
}

fn v2_doc() -> durable::Doc {
    session_doc("cache.versioned", 2, json!({ "names": [] }))
        .migrate(|value, _| Ok(json!({ "names": [value["name"]] })))
        .define()
        .unwrap()
}

#[test]
fn durable_session_cache_migrates_a_document_cached_by_an_older_definition_without_unloading() {
    each_backend(|backend| {
        let t = TestSession::open(backend);
        touch(&t, &v1_doc());
        assert_eq!(*read(&t, &v1_doc()).unwrap(), json!({ "name": "first" }));
        // Reloaded extension code accesses the still-cached document with a newer definition.
        assert_eq!(*read(&t, &v2_doc()).unwrap(), json!({ "names": ["first"] }));
        t.session
            .commit(|tx| {
                Ok(tx
                    .doc(&v2_doc(), Owner::Session)?
                    .push(&path!["names"], vec![json!("second")])
                    .map(drop)?)
            })
            .unwrap();
        assert_matches(
            &document_writes(&t.storage.last_commit())[0],
            json!({ "type": "document.change", "content": { "version": 2, "kind": "base", "value": { "names": ["first", "second"] } } }),
        );
        let error = t.session.snapshot(&v1_doc(), Owner::Session).unwrap_err();
        assert!(error.to_string().contains("newer version 2"), "{error}");
    });
}

#[test]
fn durable_session_cache_sends_observers_of_an_older_shape_a_root_replacement() {
    each_backend(|backend| {
        let t = TestSession::open(backend);
        touch(&t, &v1_doc());
        let watch = t.session.watch_doc(&v1_doc(), Owner::Session).unwrap().unwrap();
        let frames: Arc<Mutex<Vec<Vec<Value>>>> = Arc::default();
        let sink = frames.clone();
        watch
            .start(move |_, ops| {
                sink.lock().unwrap().push(ops.iter().map(Op::to_json).collect());
                Ok(())
            })
            .unwrap();
        t.session
            .commit(|tx| {
                Ok(tx
                    .doc(&v2_doc(), Owner::Session)?
                    .push(&path!["names"], vec![json!("second")])
                    .map(drop)?)
            })
            .unwrap();
        eventually(|| !frames.lock().unwrap().is_empty());
        assert_eq!(*watch.value().unwrap(), json!({ "names": ["first", "second"] }));
        assert_eq!(
            *frames.lock().unwrap(),
            [vec![json!(["r", { "names": ["first", "second"] }])]]
        );
        watch.stop();
    });
}

#[test]
fn durable_session_cache_serves_an_older_definition_from_storage_after_an_in_memory_migration() {
    each_backend(|backend| {
        let t = TestSession::open(backend);
        touch(&t, &v1_doc());
        t.session.unload_documents().unwrap();
        assert_eq!(*read(&t, &v2_doc()).unwrap(), json!({ "names": ["first"] }));
        assert_eq!(*read(&t, &v1_doc()).unwrap(), json!({ "name": "first" }));
        set(&t, &v1_doc(), "name", json!("renamed"));
        assert_eq!(*read(&t, &v2_doc()).unwrap(), json!({ "names": ["renamed"] }));
    });
}
