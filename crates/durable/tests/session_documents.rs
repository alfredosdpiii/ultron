//! Port of upstream `test/session-documents.test.ts` ("Session document transactions"), run against every backend.
//! Not ported: tests of callbacks that settle with pending acquisitions, of Tx use after settlement, and of non-JSON
//! values; a synchronous callback, the borrow checker, and `serde_json::Value` make those states unrepresentable.

mod session_support;

use chord_delta::path;
use durable::{
    DocDefinition, DocSemantics, DocumentFork, DocumentHistory, Owner, SessionError, StorageError, TaskId, TaskRecord,
    TaskState,
};
use serde_json::{Value, json};
use session_support::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

fn conversation(history: DocumentHistory, fork: DocumentFork) -> DocSemantics {
    DocSemantics::Conversation { history, fork }
}

fn live_value() -> Value {
    json!({ "items": [], "nested": { "count": 0 }, "other": { "label": "x" } })
}

fn live_doc() -> durable::Doc {
    DocDefinition::new(
        "test.live",
        1,
        conversation(DocumentHistory::Latest, DocumentFork::Initial),
        |_| live_value(),
    )
    .define()
    .unwrap()
}

fn counter_doc() -> durable::Doc {
    DocDefinition::new("test.counter", 1, DocSemantics::Session, |_| json!({ "count": 0 }))
        .define()
        .unwrap()
}

fn member_doc(seeds: Arc<std::sync::Mutex<Vec<Value>>>) -> durable::Doc {
    DocDefinition::new("test.member", 1, DocSemantics::Session, move |seed| {
        seeds.lock().unwrap().push(seed.clone());
        json!({ "seed": seed, "hits": 0 })
    })
    .family()
    .define()
    .unwrap()
}

fn setup_live(backend: Backend) -> (TestSession, durable::ConversationId) {
    let t = TestSession::open(backend);
    let c = t.create_conversation();
    t.session
        .commit(|tx| {
            let live = tx.doc(&live_doc(), Owner::Conversation(c))?;
            live.push(&path!["items"], vec![json!("a"), json!("b")])?;
            Ok(())
        })
        .unwrap();
    (t, c)
}

fn snapshot(t: &TestSession, doc: &durable::Doc, owner: Owner) -> Option<Arc<Value>> {
    t.session.snapshot(doc, owner).unwrap()
}

#[test]
fn durable_session_documents_create_an_initial_base_on_first_access_and_adopt_it_after_storage_success() {
    each_backend(|backend| {
        let t = TestSession::open(backend);
        let c = t.create_conversation();
        t.session
            .commit(|tx| {
                tx.doc(&live_doc(), Owner::Conversation(c))?
                    .set(&path!["message"], json!("hello"))?;
                Ok(())
            })
            .unwrap();
        let writes = t.storage.last_commit();
        assert_eq!(writes.len(), 1);
        assert_matches(
            &json(&writes[0]),
            json!({
                "type": "document.create",
                "record": {
                    "kind": "test.live",
                    "scope": { "kind": "conversation", "conversationId": c.0 },
                    "history": "latest",
                    "fork": "initial",
                },
                "content": { "kind": "base", "version": 1, "value": { "message": "hello", "items": [], "nested": { "count": 0 }, "other": { "label": "x" } } },
            }),
        );
        let value = snapshot(&t, &live_doc(), Owner::Conversation(c)).unwrap();
        assert_eq!(
            *value,
            json!({ "message": "hello", "items": [], "nested": { "count": 0 }, "other": { "label": "x" } })
        );
        let publication = t.last_publication();
        let published = &document_changes(&publication)[0];
        assert_eq!(published.record.created_at, publication.seq);
        assert!(Arc::ptr_eq(published.value.as_ref().unwrap(), &value));
        assert_eq!(published.conversation_id, Some(c));
    });
}

#[test]
fn durable_session_documents_never_create_on_snapshot() {
    each_backend(|backend| {
        let t = TestSession::open(backend);
        let c = t.create_conversation();
        let before = t.storage.commit_count();
        let seeds = Arc::default();
        assert_eq!(snapshot(&t, &live_doc(), Owner::Conversation(c)), None);
        assert_eq!(snapshot(&t, &counter_doc(), Owner::Session), None);
        assert_eq!(
            t.session
                .snapshot_member(&member_doc(seeds), Owner::Session, "k")
                .unwrap(),
            None
        );
        assert_eq!(t.storage.commit_count(), before);
        assert_eq!(t.storage.mint_count(), 1);
    });
}

#[test]
fn durable_session_documents_return_shared_immutable_snapshots_and_keep_prior_revisions_stable() {
    each_backend(|backend| {
        let (t, c) = setup_live(backend);
        let owner = Owner::Conversation(c);
        let first = snapshot(&t, &live_doc(), owner).unwrap();
        assert!(Arc::ptr_eq(&snapshot(&t, &live_doc(), owner).unwrap(), &first));
        t.session
            .commit(|tx| Ok(tx.doc(&live_doc(), owner)?.set(&path!["nested", "count"], json!(1))?))
            .unwrap();
        let second = snapshot(&t, &live_doc(), owner).unwrap();
        assert!(!Arc::ptr_eq(&second, &first));
        assert_eq!(first["nested"]["count"], 0);
        assert_eq!(second["nested"]["count"], 1);
    });
}

#[test]
fn durable_session_documents_publish_the_adopted_revision_and_the_admitted_ops() {
    each_backend(|backend| {
        let (t, c) = setup_live(backend);
        let owner = Owner::Conversation(c);
        t.session
            .commit(|tx| {
                let live = tx.doc(&live_doc(), owner)?;
                live.set(&path!["other"], json!({ "label": "y" }))?;
                live.push(&path!["items"], vec![json!("c")])?;
                Ok(())
            })
            .unwrap();
        let value = snapshot(&t, &live_doc(), owner).unwrap();
        let published = document_changes(&t.last_publication()).remove(0);
        assert!(Arc::ptr_eq(published.value.as_ref().unwrap(), &value));
        let admitted = json(&t.storage.last_commit()[0]);
        assert_eq!(admitted["type"], "document.change");
        assert_eq!(admitted["content"]["kind"], "delta");
        assert_eq!(Value::Array(published.ops.clone()), admitted["content"]["ops"]);
        assert_eq!(
            published.ops,
            vec![
                json!(["s", ["other"], { "label": "y" }]),
                json!(["p", ["items"], 2, 0, ["c"]])
            ]
        );
    });
}

#[test]
fn durable_session_documents_suppress_writes_and_publications_for_empty_batches() {
    each_backend(|backend| {
        let (t, c) = setup_live(backend);
        let (commits, published) = (t.storage.commit_count(), t.publication_count());
        t.session
            .commit(|tx| {
                let live = tx.doc(&live_doc(), Owner::Conversation(c))?;
                live.set(&path!["nested", "count"], json!(0))?;
                live.push(&path!["items"], vec![json!("z")])?;
                live.pop(&path!["items"])?;
                Ok(())
            })
            .unwrap();
        assert_eq!(t.storage.commit_count(), commits);
        assert_eq!(t.publication_count(), published);
    });
}

#[test]
fn durable_session_documents_write_and_publish_replayable_nonempty_structural_no_ops() {
    each_backend(|backend| {
        let (t, c) = setup_live(backend);
        let owner = Owner::Conversation(c);
        let before = snapshot(&t, &live_doc(), owner).unwrap();
        t.session
            .commit(|tx| {
                let live = tx.doc(&live_doc(), owner)?;
                let first = live.shift(&path!["items"])?.unwrap();
                live.unshift(&path!["items"], vec![first])?;
                Ok(())
            })
            .unwrap();
        assert_matches(
            &json(&t.storage.last_commit()[0]),
            json!({ "type": "document.change", "content": { "kind": "delta" } }),
        );
        let after = snapshot(&t, &live_doc(), owner).unwrap();
        assert_eq!(after, before);
        assert!(!Arc::ptr_eq(&after, &before));
        assert_eq!(
            document_changes(&t.last_publication())[0].ops,
            vec![json!(["p", ["items"], 0, 1, []]), json!(["p", ["items"], 0, 0, ["a"]])]
        );
    });
}

#[test]
fn durable_session_documents_revoke_escaped_drafts_when_the_callback_settles() {
    each_backend(|backend| {
        let (t, c) = setup_live(backend);
        let owner = Owner::Conversation(c);
        let escaped = t
            .session
            .commit(|tx| {
                let live = tx.doc(&live_doc(), owner)?;
                live.set(&path!["message"], json!("inside"))?;
                Ok(live)
            })
            .unwrap();
        assert_eq!(
            escaped.get(&path!["message"]).unwrap_err().to_string(),
            "Cannot use a settled overlay"
        );
        assert!(escaped.set(&path!["message"], json!("outside")).is_err());
        assert!(escaped.get(&path!["items"]).is_err());
        assert!(escaped.push(&path!["items"], vec![json!("outside")]).is_err());
        assert_eq!(snapshot(&t, &live_doc(), owner).unwrap()["message"], "inside");
        let returned = t.session.commit(|tx| tx.doc(&live_doc(), owner)).unwrap();
        assert!(returned.get(&path!["message"]).is_err());
    });
}

#[test]
fn durable_session_documents_abort_every_change_when_the_callback_fails() {
    each_backend(|backend| {
        let (t, c) = setup_live(backend);
        let owner = Owner::Conversation(c);
        let before = snapshot(&t, &live_doc(), owner).unwrap();
        let commits = t.storage.commit_count();
        let error = t
            .session
            .commit(|tx| {
                tx.doc(&live_doc(), owner)?.set(&path!["message"], json!("lost"))?;
                tx.doc(&counter_doc(), Owner::Session)?.set(&path!["count"], json!(5))?;
                Err::<(), _>(SessionError::Error("callback failed".into()))
            })
            .unwrap_err();
        assert_eq!(error.to_string(), "callback failed");
        assert_eq!(t.storage.commit_count(), commits);
        assert!(Arc::ptr_eq(&snapshot(&t, &live_doc(), owner).unwrap(), &before));
        assert_eq!(snapshot(&t, &counter_doc(), Owner::Session), None);
        t.session
            .commit(|tx| Ok(tx.doc(&live_doc(), owner)?.set(&path!["message"], json!("kept"))?))
            .unwrap();
        assert_eq!(snapshot(&t, &live_doc(), owner).unwrap()["message"], "kept");
    });
}

#[test]
fn durable_session_documents_memoize_duplicate_acquisition_and_initialize_once() {
    each_backend(|backend| {
        let t = TestSession::open(backend);
        let c = t.create_conversation();
        let inits = Arc::new(AtomicUsize::new(0));
        let counted = inits.clone();
        let doc = DocDefinition::new(
            "test.live",
            1,
            conversation(DocumentHistory::Latest, DocumentFork::Initial),
            move |_| {
                counted.fetch_add(1, Ordering::SeqCst);
                live_value()
            },
        )
        .define()
        .unwrap();
        let mints = t.storage.mint_count();
        t.session
            .commit(|tx| {
                let first = tx.doc(&doc, Owner::Conversation(c))?;
                let second = tx.doc(&doc, Owner::Conversation(c))?;
                assert!(first.ptr_eq(&second));
                assert!(tx.doc(&doc, Owner::Conversation(c))?.ptr_eq(&first));
                first.set(&path!["message"], json!("once"))?;
                Ok(())
            })
            .unwrap();
        assert_eq!(inits.load(Ordering::SeqCst), 1);
        assert_eq!(t.storage.mint_count(), mints + 1);
        assert_eq!(types(&t.storage.last_commit()), ["document.create"]);
    });
}

fn increment(draft: &chord_delta::Draft, field: &str) {
    let count = draft.get(&path![field]).unwrap().unwrap().as_i64().unwrap();
    draft.set(&path![field], json!(count + 1)).unwrap();
}

#[test]
fn durable_session_documents_use_the_first_family_seed_and_ignore_seeds_for_existing_members() {
    each_backend(|backend| {
        let t = TestSession::open(backend);
        let seeds = Arc::new(std::sync::Mutex::new(Vec::new()));
        let member = member_doc(seeds.clone());
        t.session
            .commit(|tx| {
                let first = tx.doc_member(&member, Owner::Session, "k", json!("first"))?;
                let second = tx.doc_member(&member, Owner::Session, "k", json!("second"))?;
                assert!(second.ptr_eq(&first));
                increment(&first, "hits");
                Ok(())
            })
            .unwrap();
        t.session
            .commit(|tx| {
                increment(&tx.doc_member(&member, Owner::Session, "k", json!("third"))?, "hits");
                increment(
                    &tx.doc_member(&member, Owner::Session, "other", json!("fourth"))?,
                    "hits",
                );
                Ok(())
            })
            .unwrap();
        assert_eq!(*seeds.lock().unwrap(), [json!("first"), json!("fourth")]);
        let read = |key| {
            t.session
                .snapshot_member(&member, Owner::Session, key)
                .unwrap()
                .map(|v| (*v).clone())
        };
        assert_eq!(read("k"), Some(json!({ "seed": "first", "hits": 2 })));
        assert_eq!(read("other"), Some(json!({ "seed": "fourth", "hits": 1 })));
    });
}

#[test]
fn durable_session_documents_reject_definitions_whose_semantics_or_version_disagree_with_storage() {
    each_backend(|backend| {
        let (t, c) = setup_live(backend);
        let owner = Owner::Conversation(c);
        let rewindable = DocDefinition::new(
            "test.live",
            1,
            conversation(DocumentHistory::Rewindable, DocumentFork::AsOf),
            |_| live_value(),
        )
        .define()
        .unwrap();
        let v2 = DocDefinition::new(
            "test.live",
            2,
            conversation(DocumentHistory::Latest, DocumentFork::Initial),
            |_| live_value(),
        )
        .define()
        .unwrap();
        let semantics = t.session.snapshot(&rewindable, owner).unwrap_err();
        assert!(
            semantics
                .to_string()
                .contains("does not match the supplied definition semantics")
        );
        assert_eq!(semantics.name(), "TypeError");
        let error = t.session.commit(|tx| tx.doc(&rewindable, owner).map(drop)).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("does not match the supplied definition semantics")
        );
        let error = t.session.commit(|tx| tx.doc(&v2, owner).map(drop)).unwrap_err();
        assert!(
            error.to_string().contains("requires migration from version 1"),
            "{error}"
        );
    });
}

#[test]
fn durable_session_documents_roll_back_prepared_documents_when_batch_assembly_fails() {
    each_backend(|backend| {
        let (t, c) = setup_live(backend);
        let owner = Owner::Conversation(c);
        t.session
            .commit(|tx| Ok(tx.doc(&counter_doc(), Owner::Session)?.set(&path!["count"], json!(1))?))
            .unwrap();
        let live = snapshot(&t, &live_doc(), owner).unwrap();
        let counter = snapshot(&t, &counter_doc(), Owner::Session).unwrap();
        let commits = t.storage.commit_count();
        let error = t
            .session
            .commit(|tx| {
                tx.doc(&live_doc(), owner)?.set(&path!["message"], json!("lost"))?;
                tx.doc(&counter_doc(), Owner::Session)?.set(&path!["count"], json!(2))?;
                // Replacing a missing task fails during assembly, after every change was prepared.
                tx.set_task(TaskRecord {
                    id: TaskId(999),
                    conversation_id: c,
                    kind: "missing".into(),
                    version: 1,
                    input: Value::Null,
                    owner: None,
                    background: false,
                    abort_requested: false,
                    abort_reason: None,
                    abandon_on_restart: false,
                    started_at: None,
                    ended_at: None,
                    state: TaskState::Pending {
                        checkpoint: json!({ "phase": "start" }),
                    },
                    memos: None,
                })
            })
            .unwrap_err();
        assert_eq!(error.to_string(), "Task 999 does not exist");
        assert_eq!(t.storage.commit_count(), commits);
        assert!(Arc::ptr_eq(&snapshot(&t, &live_doc(), owner).unwrap(), &live));
        assert!(Arc::ptr_eq(
            &snapshot(&t, &counter_doc(), Owner::Session).unwrap(),
            &counter
        ));
        t.session
            .commit(|tx| {
                tx.doc(&live_doc(), owner)?.set(&path!["message"], json!("next"))?;
                tx.doc(&counter_doc(), Owner::Session)?.set(&path!["count"], json!(3))?;
                Ok(())
            })
            .unwrap();
        assert_eq!(snapshot(&t, &counter_doc(), Owner::Session).unwrap()["count"], 3);
    });
}

#[test]
fn durable_session_documents_fail_the_session_after_a_storage_failure_and_publish_nothing() {
    each_backend(|backend| {
        let (t, c) = setup_live(backend);
        let owner = Owner::Conversation(c);
        let before = snapshot(&t, &live_doc(), owner).unwrap();
        let published = t.publication_count();
        t.storage.fail_next_commit(StorageError::Failed("disk vanished".into()));
        let error = t
            .session
            .commit(|tx| Ok(tx.doc(&live_doc(), owner)?.set(&path!["message"], json!("uncertain"))?))
            .unwrap_err();
        assert_eq!(error.to_string(), "disk vanished");
        assert_eq!(t.publication_count(), published);
        assert_eq!(before.get("message"), None);
        assert_eq!(
            t.session.snapshot(&live_doc(), owner).unwrap_err().name(),
            "SessionFailed"
        );
        let failed = t.session.commit(|_| Ok(())).unwrap_err();
        assert_eq!(failed.name(), "SessionFailed");
        assert_eq!(
            failed.to_string(),
            "Session failed after a storage error; close and reopen it"
        );
        assert_eq!(
            failed.cause(),
            Some(&SessionError::Storage(StorageError::Failed("disk vanished".into())))
        );
        t.session.close().unwrap();
    });
}

#[test]
fn durable_session_documents_keep_the_previous_revision_unchanged_through_storage_settlement() {
    each_backend(|backend| {
        let (t, c) = setup_live(backend);
        let owner = Owner::Conversation(c);
        let before = snapshot(&t, &live_doc(), owner).unwrap();
        let copy = (*before).clone();
        let gate = t.storage.hold_commits();
        let session = t.session.clone();
        let commit = std::thread::spawn(move || {
            session.commit(|tx| {
                let live = tx.doc(&live_doc(), owner)?;
                live.push(&path!["items"], vec![json!("c")])?;
                live.set(&path!["nested", "count"], json!(9))?;
                Ok(())
            })
        });
        gate.wait_entered();
        assert!(Arc::ptr_eq(&snapshot(&t, &live_doc(), owner).unwrap(), &before));
        assert_eq!(*before, copy);
        gate.release();
        commit.join().unwrap().unwrap();
        assert_eq!(
            snapshot(&t, &live_doc(), owner).unwrap()["items"],
            json!(["a", "b", "c"])
        );
        assert_eq!(*before, copy);
    });
}

#[test]
fn durable_session_documents_retire_and_create_a_new_incarnation_at_the_same_address() {
    each_backend(|backend| {
        let (t, c) = setup_live(backend);
        let owner = Owner::Conversation(c);
        let old_id = document_changes(&t.last_publication())[0].record.identity.id;
        t.session
            .commit(|tx| {
                let live = tx.doc(&live_doc(), owner)?;
                live.set(&path!["message"], json!("final"))?;
                tx.retire_doc(&live_doc(), owner)?;
                let replacement = tx.doc(&live_doc(), owner)?;
                assert!(!replacement.ptr_eq(&live));
                replacement.set(&path!["message"], json!("new"))?;
                Ok(())
            })
            .unwrap();
        let writes: Vec<Value> = t.storage.last_commit().iter().map(json).collect();
        assert_eq!(writes.len(), 3);
        assert_matches(&writes[0], json!({ "type": "document.change", "id": old_id.0 }));
        assert_eq!(writes[1], json!({ "type": "document.retire", "id": old_id.0 }));
        assert_matches(
            &writes[2],
            json!({ "type": "document.create", "record": { "kind": "test.live" } }),
        );
        let publication = t.last_publication();
        let changes = document_changes(&publication);
        let (retired, created) = (&changes[0], &changes[1]);
        assert_eq!(retired.record.identity.id, old_id);
        assert_eq!(retired.value, None);
        assert!(retired.ops.is_empty());
        assert_eq!(retired.record.retired_at, Some(publication.seq));
        assert_ne!(created.record.identity.id, old_id);
        assert_eq!(created.record.created_at, publication.seq);
        assert_eq!(created.value.as_ref().unwrap()["message"], "new");
        assert_eq!(created.value.as_ref().unwrap()["items"], json!([]));
        assert!(created.ops.is_empty());
        assert!(Arc::ptr_eq(
            &snapshot(&t, &live_doc(), owner).unwrap(),
            created.value.as_ref().unwrap()
        ));

        t.session.commit(|tx| tx.retire_doc(&live_doc(), owner)).unwrap();
        assert_eq!(snapshot(&t, &live_doc(), owner), None);
        t.session.unload_documents().unwrap();
        assert_eq!(snapshot(&t, &live_doc(), owner), None);
        // Retiring an absent address is a no-op.
        let commits = t.storage.commit_count();
        t.session.commit(|tx| tx.retire_doc(&live_doc(), owner)).unwrap();
        assert_eq!(t.storage.commit_count(), commits);
    });
}

#[test]
fn durable_session_documents_retire_without_acquisition_and_recreate_existing_and_absent_addresses() {
    each_backend(|backend| {
        let (t, c) = setup_live(backend);
        let owner = Owner::Conversation(c);
        let old_id = document_changes(&t.last_publication())[0].record.identity.id;
        t.session.unload_documents().unwrap();
        t.session
            .commit(|tx| {
                tx.retire_doc(&live_doc(), owner)?;
                Ok(tx
                    .doc(&live_doc(), owner)?
                    .set(&path!["message"], json!("replacement"))?)
            })
            .unwrap();
        let writes = t.storage.last_commit();
        assert_eq!(types(&writes), ["document.retire", "document.create"]);
        assert_eq!(json(&writes[0]), json!({ "type": "document.retire", "id": old_id.0 }));

        let member = member_doc(Arc::default());
        t.session
            .commit(|tx| {
                tx.retire_member(&member, Owner::Session, "absent")?;
                Ok(tx
                    .doc_member(&member, Owner::Session, "absent", json!("seed"))?
                    .set(&path!["hits"], json!(1))?)
            })
            .unwrap();
        assert_eq!(types(&t.storage.last_commit()), ["document.create"]);
        assert_eq!(
            *t.session
                .snapshot_member(&member, Owner::Session, "absent")
                .unwrap()
                .unwrap(),
            json!({ "seed": "seed", "hits": 1 })
        );
    });
}

#[test]
fn durable_session_documents_retire_the_acquired_incarnation_after_writing_its_final_content() {
    each_backend(|backend| {
        let (t, c) = setup_live(backend);
        let owner = Owner::Conversation(c);
        let old_id = document_changes(&t.last_publication())[0].record.identity.id;
        t.session
            .commit(|tx| {
                let acquired = tx.doc(&live_doc(), owner)?;
                tx.retire_doc(&live_doc(), owner)?;
                Ok(acquired.set(&path!["message"], json!("final"))?)
            })
            .unwrap();
        let writes: Vec<Value> = t.storage.last_commit().iter().map(json).collect();
        assert_eq!(writes.len(), 2);
        assert_matches(
            &writes[0],
            json!({ "type": "document.change", "id": old_id.0, "content": { "kind": "delta" } }),
        );
        assert_eq!(writes[1], json!({ "type": "document.retire", "id": old_id.0 }));
        assert_eq!(snapshot(&t, &live_doc(), owner), None);

        t.session
            .commit(|tx| Ok(tx.doc(&live_doc(), owner)?.set(&path!["message"], json!("second"))?))
            .unwrap();
        let second_id = document_changes(&t.last_publication())[0].record.identity.id;
        t.session
            .commit(|tx| {
                let old = tx.doc(&live_doc(), owner)?;
                tx.retire_doc(&live_doc(), owner)?;
                let fresh = tx.doc(&live_doc(), owner)?;
                assert!(!fresh.ptr_eq(&old));
                Ok(fresh.set(&path!["message"], json!("third"))?)
            })
            .unwrap();
        let writes = t.storage.last_commit();
        assert_eq!(types(&writes), ["document.retire", "document.create"]);
        assert_eq!(
            json(&writes[0]),
            json!({ "type": "document.retire", "id": second_id.0 })
        );
        assert_eq!(snapshot(&t, &live_doc(), owner).unwrap()["message"], "third");
    });
}

#[test]
fn durable_session_documents_reload_an_unloaded_document_from_storage() {
    each_backend(|backend| {
        let (t, c) = setup_live(backend);
        let owner = Owner::Conversation(c);
        let loaded = snapshot(&t, &live_doc(), owner).unwrap();
        t.session.unload_documents().unwrap();
        let reloaded = snapshot(&t, &live_doc(), owner).unwrap();
        assert!(!Arc::ptr_eq(&reloaded, &loaded));
        assert_eq!(reloaded, loaded);
        t.session
            .commit(|tx| {
                Ok(tx
                    .doc(&live_doc(), owner)?
                    .push(&path!["items"], vec![json!("c")])
                    .map(drop)?)
            })
            .unwrap();
        t.session.unload_documents().unwrap();
        assert_eq!(
            snapshot(&t, &live_doc(), owner).unwrap()["items"],
            json!(["a", "b", "c"])
        );
    });
}

#[test]
fn durable_session_documents_deliver_complete_publications_synchronously_after_adoption() {
    each_backend(|backend| {
        let (t, c) = setup_live(backend);
        let published = t.publication_count();
        let result = t
            .session
            .commit(|tx| {
                tx.doc(&live_doc(), Owner::Conversation(c))?
                    .set(&path!["message"], json!("m"))?;
                Ok("done")
            })
            .unwrap();
        assert_eq!(result, "done");
        assert_eq!(t.publication_count(), published + 1);
    });
}

#[test]
fn durable_session_documents_publish_close_synchronously_and_support_unsubscription() {
    each_backend(|backend| {
        let t = TestSession::open(backend);
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let active = calls.clone();
        t.session
            .subscribe_close(move || active.lock().unwrap().push("active"))
            .unwrap();
        let removed = calls.clone();
        let subscription = t
            .session
            .subscribe_close(move || removed.lock().unwrap().push("removed"))
            .unwrap();
        subscription.unsubscribe();
        t.session.close().unwrap();
        assert_eq!(*calls.lock().unwrap(), ["active"]);
    });
}

#[test]
fn durable_session_documents_settle_admitted_commits_before_close_and_reject_later_admission() {
    each_backend(|backend| {
        let (t, c) = setup_live(backend);
        let owner = Owner::Conversation(c);
        let gate = t.storage.hold_commits();
        let admitted = {
            let session = t.session.clone();
            std::thread::spawn(move || {
                session.commit(|tx| Ok(tx.doc(&live_doc(), owner)?.set(&path!["message"], json!("admitted"))?))
            })
        };
        gate.wait_entered();
        let queued = {
            let session = t.session.clone();
            std::thread::spawn(move || {
                session.commit(|tx| Ok(tx.doc(&live_doc(), owner)?.set(&path!["message"], json!("queued"))?))
            })
        };
        let member = member_doc(Arc::default());
        let admitted_snapshot = {
            let session = t.session.clone();
            std::thread::spawn(move || session.snapshot_member(&member, Owner::Session, "absent"))
        };
        settle();
        let closing = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let began = closing.clone();
        t.session
            .subscribe_close(move || began.store(true, Ordering::SeqCst))
            .unwrap();
        let closed = {
            let session = t.session.clone();
            std::thread::spawn(move || session.close())
        };
        eventually(|| closing.load(Ordering::SeqCst));
        assert_eq!(t.session.commit(|_| Ok(())).unwrap_err(), SessionError::Closed);
        assert_eq!(
            t.session.snapshot(&live_doc(), owner).unwrap_err().to_string(),
            "Session is closed"
        );
        gate.release();
        admitted.join().unwrap().unwrap();
        queued.join().unwrap().unwrap();
        assert_eq!(admitted_snapshot.join().unwrap().unwrap(), None);
        closed.join().unwrap().unwrap();
        let address = durable::DocumentAddress {
            kind: "test.live".into(),
            scope: durable::DocumentScope::Conversation { conversation_id: c },
            key: None,
        };
        let stored = durable::Storage::find_document(&t.storage, &address, durable::DocumentPoint::Current);
        assert!(stored.is_err());
        // Both admitted commits reached Storage before it closed.
        let messages: Vec<Value> = t
            .storage
            .commits()
            .iter()
            .rev()
            .take(2)
            .map(|writes| json(&writes[0])["content"]["ops"].clone())
            .collect();
        assert_eq!(
            messages,
            [
                json!([["s", ["message"], "queued"]]),
                json!([["s", ["message"], "admitted"]])
            ]
        );
    });
}
