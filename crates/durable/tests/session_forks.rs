//! Port of upstream `test/session-forks.test.ts` ("Session conversation document forks"), run against every backend.

#![allow(clippy::type_complexity)]

mod session_support;

use chord_delta::path;
use durable::{
    ConversationOwnership, DocDefinition, DocSemantics, DocumentAddress, DocumentFork, DocumentHistory, DocumentPoint,
    DocumentScope, EntryDraft, EntryQuery, Owner, SessionEnd, SessionError, Storage, StorageError, TaskDefinition,
    TaskOptions,
};
use serde_json::{Value, json};
use session_support::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

fn semantics(history: DocumentHistory, fork: DocumentFork) -> DocSemantics {
    DocSemantics::Conversation { history, fork }
}

const AS_OF: DocSemantics = DocSemantics::Conversation {
    history: DocumentHistory::Rewindable,
    fork: DocumentFork::AsOf,
};
const CURRENT: DocSemantics = DocSemantics::Conversation {
    history: DocumentHistory::Latest,
    fork: DocumentFork::Current,
};

fn value_doc(kind: &str, semantics: DocSemantics, initial: &'static str) -> durable::Doc {
    DocDefinition::new(kind, 1, semantics, move |_| json!({ "value": initial }))
        .define()
        .unwrap()
}

fn seeded_family(kind: &str, semantics: DocSemantics) -> durable::Doc {
    DocDefinition::new(kind, 1, semantics, |seed| json!({ "value": seed }))
        .family()
        .define()
        .unwrap()
}

fn set_value(draft: chord_delta::Draft, value: Value) -> Result<(), SessionError> {
    Ok(draft.set(&path!["value"], value)?)
}

fn fork(t: &TestSession, parent: durable::ConversationId, at: durable::EntryId) -> durable::ConversationRecord {
    t.session
        .commit(|tx| tx.fork_conversation(parent, at, ConversationOwnership::Ownerless))
        .unwrap()
}

fn point(tx: &mut durable::Tx<'_>, conversation: durable::ConversationId, kind: &str) -> durable::EntryId {
    tx.append_entry(conversation, EntryDraft::new(kind)).unwrap().id
}

fn read(t: &TestSession, doc: &durable::Doc, conversation: durable::ConversationId) -> Option<Value> {
    t.session
        .snapshot(doc, Owner::Conversation(conversation))
        .unwrap()
        .map(|v| (*v).clone())
}

fn read_member(t: &TestSession, doc: &durable::Doc, conversation: durable::ConversationId, key: &str) -> Option<Value> {
    t.session
        .snapshot_member(doc, Owner::Conversation(conversation), key)
        .unwrap()
        .map(|v| (*v).clone())
}

fn address(kind: &str, conversation_id: durable::ConversationId) -> DocumentAddress {
    DocumentAddress {
        kind: kind.into(),
        scope: DocumentScope::Conversation { conversation_id },
        key: None,
    }
}

#[test]
fn durable_session_forks_copy_as_of_and_current_bases_while_leaving_initial_documents_absent() {
    each_backend(|backend| {
        let as_of = value_doc("fork.policies.as-of", AS_OF, "as-initial");
        let current = value_doc("fork.policies.current", CURRENT, "current-initial");
        let initial = value_doc(
            "fork.policies.initial",
            semantics(DocumentHistory::Latest, DocumentFork::Initial),
            "fresh",
        );
        let as_of_family = seeded_family("fork.policies.as-of-family", AS_OF);
        let current_family = seeded_family("fork.policies.current-family", CURRENT);
        let t = TestSession::open(backend);
        let parent = t.create_conversation();
        let p = Owner::Conversation(parent);
        let fork_at = t
            .session
            .commit(|tx| {
                let at = point(tx, parent, "fork-point");
                set_value(tx.doc(&as_of, p)?, json!("as-at-fork"))?;
                set_value(tx.doc(&current, p)?, json!("current-at-fork"))?;
                set_value(tx.doc(&initial, p)?, json!("parent-only"))?;
                set_value(
                    tx.doc_member(&as_of_family, p, "a", json!("unused"))?,
                    json!("family-as-a"),
                )?;
                set_value(
                    tx.doc_member(&as_of_family, p, "b", json!("unused"))?,
                    json!("family-as-b"),
                )?;
                set_value(
                    tx.doc_member(&current_family, p, "a", json!("unused"))?,
                    json!("family-current-a"),
                )?;
                Ok(at)
            })
            .unwrap();
        t.session
            .commit(|tx| {
                set_value(tx.doc(&as_of, p)?, json!("as-after-fork"))?;
                set_value(tx.doc(&current, p)?, json!("current-when-copied"))?;
                set_value(
                    tx.doc_member(&as_of_family, p, "a", json!("unused"))?,
                    json!("family-as-after"),
                )?;
                set_value(
                    tx.doc_member(&current_family, p, "a", json!("unused"))?,
                    json!("family-current-when-copied"),
                )
            })
            .unwrap();
        let reads = t.storage.document_reads();
        let child = fork(&t, parent, fork_at);
        assert_eq!(t.storage.document_reads(), reads);

        assert_eq!(read(&t, &as_of, child.id), Some(json!({ "value": "as-at-fork" })));
        assert_eq!(
            read(&t, &current, child.id),
            Some(json!({ "value": "current-when-copied" }))
        );
        assert_eq!(read(&t, &initial, child.id), None);
        assert_eq!(
            read_member(&t, &as_of_family, child.id, "a"),
            Some(json!({ "value": "family-as-a" }))
        );
        assert_eq!(
            read_member(&t, &as_of_family, child.id, "b"),
            Some(json!({ "value": "family-as-b" }))
        );
        assert_eq!(
            read_member(&t, &current_family, child.id, "a"),
            Some(json!({ "value": "family-current-when-copied" }))
        );

        let admitted: Vec<Value> = t.storage.last_commit().iter().map(json).collect();
        let copies: Vec<&Value> = admitted.iter().filter(|w| w["type"] == "document.copy").collect();
        assert_eq!(copies.len(), 5);
        assert!(
            copies
                .iter()
                .all(|w| w["record"]["scope"] == json!({ "kind": "conversation", "conversationId": child.id.0 }))
        );
        let publication = t.last_publication();
        let copied = document_copy_changes(&publication);
        assert_eq!(copied.len(), 5);
        for (record, conversation_id, source) in &copied {
            assert_eq!(record.created_at, publication.seq);
            assert_eq!(*conversation_id, child.id);
            let write = copies
                .iter()
                .find(|w| w["record"]["id"] == record.identity.id.0)
                .unwrap();
            assert_eq!(*source, write["source"]);
        }

        let parent_record = t
            .storage
            .find_document(&address("fork.policies.as-of", parent), DocumentPoint::Current);
        let child_record = t
            .storage
            .find_document(&address("fork.policies.as-of", child.id), DocumentPoint::Current);
        assert_ne!(
            parent_record.unwrap().unwrap().identity.id,
            child_record.unwrap().unwrap().identity.id
        );

        t.session
            .commit(|tx| {
                set_value(
                    tx.doc(&as_of, Owner::Conversation(child.id))?,
                    json!("child-independent"),
                )
            })
            .unwrap();
        assert_eq!(read(&t, &as_of, parent), Some(json!({ "value": "as-after-fork" })));
        assert_eq!(
            read(&t, &as_of, child.id),
            Some(json!({ "value": "child-independent" }))
        );
        t.session
            .commit(|tx| set_value(tx.doc(&initial, Owner::Conversation(child.id))?, json!("child-created")))
            .unwrap();
        assert_eq!(read(&t, &initial, child.id), Some(json!({ "value": "child-created" })));
    });
}

#[test]
fn durable_session_forks_use_final_state_of_the_fork_entry_commit_and_exclude_later_same_commit_entries() {
    each_backend(|backend| {
        let doc = value_doc("fork.same-commit", AS_OF, "initial");
        let t = TestSession::open(backend);
        let parent = t.create_conversation();
        let (fork_at, excluded) = t
            .session
            .commit(|tx| {
                let fork_at = point(tx, parent, "included");
                set_value(
                    tx.doc(&doc, Owner::Conversation(parent))?,
                    json!("final-state-of-commit"),
                )?;
                Ok((fork_at, point(tx, parent, "excluded")))
            })
            .unwrap();
        let child = fork(&t, parent, fork_at);
        assert_eq!(
            read(&t, &doc, child.id),
            Some(json!({ "value": "final-state-of-commit" }))
        );
        let query = EntryQuery {
            conversation_id: child.id,
            min_entry_id: None,
            max_entry_id: None,
            order: None,
        };
        let visible = t.session.commit(|tx| tx.scan_entries(&query, 10, None)).unwrap();
        let ids: Vec<_> = visible.items.iter().map(|entry| entry.id).collect();
        assert!(ids.contains(&fork_at));
        assert!(!ids.contains(&excluded));
    });
}

#[test]
fn durable_session_forks_select_the_entry_owning_ancestor_for_as_of_and_the_parent_for_current() {
    each_backend(|backend| {
        let as_of = value_doc("fork.ancestry.as-of", AS_OF, "initial");
        let current = value_doc("fork.ancestry.current", CURRENT, "initial");
        let t = TestSession::open(backend);
        let root = t.create_conversation();
        let inherited = t
            .session
            .commit(|tx| {
                let at = point(tx, root, "root");
                set_value(tx.doc(&as_of, Owner::Conversation(root))?, json!("root-at-entry"))?;
                set_value(tx.doc(&current, Owner::Conversation(root))?, json!("root-current"))?;
                Ok(at)
            })
            .unwrap();
        let parent = fork(&t, root, inherited);
        let parent_entry = t
            .session
            .commit(|tx| {
                let at = point(tx, parent.id, "parent");
                set_value(
                    tx.doc(&as_of, Owner::Conversation(parent.id))?,
                    json!("parent-at-own-entry"),
                )?;
                set_value(
                    tx.doc(&current, Owner::Conversation(parent.id))?,
                    json!("parent-current"),
                )?;
                Ok(at)
            })
            .unwrap();
        let inherited_fork = fork(&t, parent.id, inherited);
        assert_eq!(
            read(&t, &as_of, inherited_fork.id),
            Some(json!({ "value": "root-at-entry" }))
        );
        assert_eq!(
            read(&t, &current, inherited_fork.id),
            Some(json!({ "value": "parent-current" }))
        );
        let own_fork = fork(&t, parent.id, parent_entry);
        assert_eq!(
            read(&t, &as_of, own_fork.id),
            Some(json!({ "value": "parent-at-own-entry" }))
        );
        assert_eq!(
            read(&t, &current, own_fork.id),
            Some(json!({ "value": "parent-current" }))
        );
    });
}

#[test]
fn durable_session_forks_copy_the_stored_value_and_version_without_migrating() {
    each_backend(|backend| {
        let v1 = DocDefinition::new("fork.stored-version", 1, AS_OF, |_| json!({ "count": 1 }))
            .define()
            .unwrap();
        let migrations = Arc::new(AtomicUsize::new(0));
        let counted = migrations.clone();
        let v3 = DocDefinition::new(
            "fork.stored-version",
            3,
            AS_OF,
            |_| json!({ "count": 0, "migrated": false }),
        )
        .migrate(move |value, from| {
            assert_eq!(from, 1);
            counted.fetch_add(1, Ordering::SeqCst);
            Ok(json!({ "count": value["count"], "migrated": true }))
        })
        .define()
        .unwrap();
        let t = TestSession::open(backend);
        let parent = t.create_conversation();
        let fork_at = t
            .session
            .commit(|tx| {
                let at = point(tx, parent, "point");
                tx.doc(&v1, Owner::Conversation(parent))?;
                Ok(at)
            })
            .unwrap();
        t.session.unload_documents().unwrap();
        assert_eq!(read(&t, &v3, parent), Some(json!({ "count": 1, "migrated": true })));
        assert_eq!(migrations.load(Ordering::SeqCst), 1);
        let child = fork(&t, parent, fork_at);
        assert_eq!(migrations.load(Ordering::SeqCst), 1);
        let record = t
            .storage
            .find_document(&address("fork.stored-version", child.id), DocumentPoint::Current)
            .unwrap()
            .unwrap();
        let stored = t
            .storage
            .document(record.identity.id, DocumentPoint::Current)
            .unwrap()
            .unwrap();
        assert_eq!(
            (stored.version, Value::Object(stored.value)),
            (1, json!({ "count": 1 }))
        );
        assert_eq!(read(&t, &v3, child.id), Some(json!({ "count": 1, "migrated": true })));
        assert_eq!(migrations.load(Ordering::SeqCst), 2);
    });
}

#[test]
fn durable_session_forks_coalesce_a_typed_migration_and_override_into_the_copied_creation_base() {
    each_backend(|backend| {
        let v1 = DocDefinition::new("fork.override", 1, AS_OF, |_| json!({ "count": 2 }))
            .define()
            .unwrap();
        let checkpoints = Arc::new(AtomicUsize::new(0));
        let counted = checkpoints.clone();
        let v2 = DocDefinition::new("fork.override", 2, AS_OF, |_| json!({ "count": 0, "migrated": false }))
            .migrate(|value, _| Ok(json!({ "count": value["count"], "migrated": true })))
            .checkpoint_when(move |_, _, _, _| {
                counted.fetch_add(1, Ordering::SeqCst);
                Ok(false)
            })
            .define()
            .unwrap();
        let t = TestSession::open(backend);
        let parent = t.create_conversation();
        let fork_at = t
            .session
            .commit(|tx| {
                let at = point(tx, parent, "point");
                tx.doc(&v1, Owner::Conversation(parent))?;
                Ok(at)
            })
            .unwrap();
        let reads = t.storage.document_reads();
        let child = t
            .session
            .commit(|tx| {
                let created = tx.fork_conversation(parent, fork_at, ConversationOwnership::Ownerless)?;
                tx.doc(&v2, Owner::Conversation(created.id))?
                    .set(&path!["count"], json!(9))?;
                Ok(created)
            })
            .unwrap();
        assert_eq!(t.storage.document_reads(), reads + 1);
        let creates: Vec<Value> = t
            .storage
            .last_commit()
            .iter()
            .map(json)
            .filter(|w| w["type"] == "document.create")
            .collect();
        assert_eq!(creates.len(), 1);
        assert_eq!(
            creates[0]["content"],
            json!({ "kind": "base", "version": 2, "value": { "count": 9, "migrated": true } })
        );
        assert_eq!(checkpoints.load(Ordering::SeqCst), 0);
        let documents = document_changes(&t.last_publication());
        assert_eq!(documents.iter().map(|d| d.version).collect::<Vec<_>>(), [Some(2)]);
        assert_eq!(read(&t, &v2, child.id), Some(json!({ "count": 9, "migrated": true })));
        let parent_record = t
            .storage
            .find_document(&address("fork.override", parent), DocumentPoint::Current)
            .unwrap()
            .unwrap();
        let parent_stored = t
            .storage
            .document(parent_record.identity.id, DocumentPoint::Current)
            .unwrap();
        assert_eq!(parent_stored.unwrap().version, 1);
    });
}

#[test]
fn durable_session_forks_copy_the_incarnation_alive_at_the_fork_point() {
    each_backend(|backend| {
        let doc = value_doc("fork.incarnations", AS_OF, "initial");
        let t = TestSession::open(backend);
        let parent = t.create_conversation();
        let p = Owner::Conversation(parent);
        let old_at = t
            .session
            .commit(|tx| {
                let at = point(tx, parent, "old");
                set_value(tx.doc(&doc, p)?, json!("old"))?;
                Ok(at)
            })
            .unwrap();
        let retired_at = t
            .session
            .commit(|tx| {
                let at = point(tx, parent, "retired");
                tx.retire_doc(&doc, p)?;
                Ok(at)
            })
            .unwrap();
        let new_at = t
            .session
            .commit(|tx| {
                let at = point(tx, parent, "new");
                set_value(tx.doc(&doc, p)?, json!("new"))?;
                Ok(at)
            })
            .unwrap();
        let old_child = fork(&t, parent, old_at);
        let empty_child = fork(&t, parent, retired_at);
        let new_child = fork(&t, parent, new_at);
        assert_eq!(read(&t, &doc, old_child.id), Some(json!({ "value": "old" })));
        assert_eq!(read(&t, &doc, empty_child.id), None);
        assert_eq!(read(&t, &doc, new_child.id), Some(json!({ "value": "new" })));
    });
}

#[test]
fn durable_session_forks_reject_invisible_fork_points_before_admission_and_remain_usable() {
    each_backend(|backend| {
        let t = TestSession::open(backend);
        let root = t.create_conversation();
        let (visible, hidden) = t
            .session
            .commit(|tx| Ok((point(tx, root, "visible"), point(tx, root, "hidden"))))
            .unwrap();
        let parent = fork(&t, root, visible);
        let (commits, published) = (t.storage.commit_count(), t.publication_count());
        let error = t
            .session
            .commit(|tx| tx.fork_conversation(parent.id, hidden, ConversationOwnership::Ownerless))
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("Entry {} is not visible from conversation {}", hidden.0, parent.id.0)
        );
        assert_eq!(t.storage.commit_count(), commits);
        assert_eq!(t.publication_count(), published);
        let independent = t
            .session
            .commit(|tx| tx.create_conversation(ConversationOwnership::Ownerless))
            .unwrap();
        assert_eq!(t.storage.conversation(independent.id).unwrap(), Some(independent));
    });
}

#[test]
fn durable_session_forks_reject_duplicate_as_of_and_current_selections_before_admission() {
    each_backend(|backend| {
        let as_of = value_doc("fork.duplicate-policy", AS_OF, "as-of");
        let current = value_doc("fork.duplicate-policy", CURRENT, "current");
        let t = TestSession::open(backend);
        let parent = t.create_conversation();
        let p = Owner::Conversation(parent);
        let old_at = t
            .session
            .commit(|tx| {
                let at = point(tx, parent, "old");
                tx.doc(&as_of, p)?;
                Ok(at)
            })
            .unwrap();
        t.session.commit(|tx| tx.retire_doc(&as_of, p)).unwrap();
        t.session.commit(|tx| tx.doc(&current, p).map(drop)).unwrap();
        let commits = t.storage.commit_count();
        let error = t
            .session
            .commit(|tx| tx.fork_conversation(parent, old_at, ConversationOwnership::Ownerless))
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Fork selects multiple source documents for fork.duplicate-policy"
        );
        assert_eq!(t.storage.commit_count(), commits);
        let independent = t
            .session
            .commit(|tx| tx.create_conversation(ConversationOwnership::Ownerless))
            .unwrap();
        assert_eq!(t.storage.conversation(independent.id).unwrap(), Some(independent));
    });
}

#[test]
fn durable_session_forks_reject_current_and_as_of_source_writes_in_the_fork_transaction() {
    each_backend(|backend| {
        let current = value_doc("fork.same-transaction-current", CURRENT, "committed");
        let as_of = value_doc("fork.same-transaction-as-of", AS_OF, "committed");
        let t = TestSession::open(backend);
        let parent = t.create_conversation();
        let p = Owner::Conversation(parent);
        let fork_at = t
            .session
            .commit(|tx| {
                let at = point(tx, parent, "point");
                tx.doc(&current, p)?;
                tx.doc(&as_of, p)?;
                Ok(at)
            })
            .unwrap();
        let commits = t.storage.commit_count();
        let attempts: [&dyn Fn(&mut durable::Tx<'_>) -> Result<(), SessionError>; 3] = [
            &|tx| {
                set_value(tx.doc(&current, p)?, json!("before-fork"))?;
                tx.fork_conversation(parent, fork_at, ConversationOwnership::Ownerless)
                    .map(drop)
            },
            &|tx| {
                tx.fork_conversation(parent, fork_at, ConversationOwnership::Ownerless)?;
                set_value(tx.doc(&current, p)?, json!("after-fork"))
            },
            &|tx| {
                set_value(tx.doc(&as_of, p)?, json!("as-of-write"))?;
                tx.fork_conversation(parent, fork_at, ConversationOwnership::Ownerless)
                    .map(drop)
            },
        ];
        for attempt in attempts {
            let error = t.session.commit(attempt).unwrap_err();
            assert!(
                error.to_string().starts_with("Cannot change fork source document"),
                "{error}"
            );
        }
        assert_eq!(t.storage.commit_count(), commits);
        assert_eq!(read(&t, &current, parent), Some(json!({ "value": "committed" })));
        let child = fork(&t, parent, fork_at);
        assert_eq!(read(&t, &current, child.id), Some(json!({ "value": "committed" })));
    });
}

#[test]
fn durable_session_forks_roll_every_copied_base_back_when_later_assembly_fails() {
    each_backend(|backend| {
        let copied = value_doc("fork.rollback.copied", AS_OF, "copied");
        let fail = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let failing = fail.clone();
        let failure = DocDefinition::new(
            "fork.rollback.failure",
            1,
            DocSemantics::Session,
            |_| json!({ "count": 0 }),
        )
        .checkpoint_when(move |_, _, _, _| {
            if failing.load(Ordering::SeqCst) {
                Err("checkpoint failed".into())
            } else {
                Ok(false)
            }
        })
        .define()
        .unwrap();
        let t = TestSession::open(backend);
        let parent = t.create_conversation();
        let fork_at = t
            .session
            .commit(|tx| {
                let at = point(tx, parent, "point");
                tx.doc(&copied, Owner::Conversation(parent))?;
                tx.doc(&failure, Owner::Session)?;
                Ok(at)
            })
            .unwrap();
        let (commits, published) = (t.storage.commit_count(), t.publication_count());
        let mut child = None;
        let error = t
            .session
            .commit(|tx| {
                child = Some(
                    tx.fork_conversation(parent, fork_at, ConversationOwnership::Ownerless)?
                        .id,
                );
                Ok(tx.doc(&failure, Owner::Session)?.set(&path!["count"], json!(1))?)
            })
            .unwrap_err();
        assert_eq!(error.to_string(), "checkpoint failed");
        assert_eq!(t.storage.commit_count(), commits);
        assert_eq!(t.publication_count(), published);
        assert_eq!(t.storage.conversation(child.unwrap()).unwrap(), None);
        fail.store(false, Ordering::SeqCst);
        t.session
            .commit(|tx| Ok(tx.doc(&failure, Owner::Session)?.set(&path!["count"], json!(2))?))
            .unwrap();
        assert_eq!(
            *t.session.snapshot(&failure, Owner::Session).unwrap().unwrap(),
            json!({ "count": 2 })
        );
        assert_eq!(read(&t, &copied, parent), Some(json!({ "value": "copied" })));
    });
}

#[test]
fn durable_session_forks_fail_the_session_on_a_rejected_fork_commit_which_leaves_no_effect() {
    each_backend(|backend| {
        let doc = value_doc("fork.storage-rejected", AS_OF, "source");
        let t = TestSession::open(backend);
        let parent = t.create_conversation();
        let fork_at = t
            .session
            .commit(|tx| {
                let at = point(tx, parent, "point");
                tx.doc(&doc, Owner::Conversation(parent))?;
                Ok(at)
            })
            .unwrap();
        let rejection = StorageError::Failed("copy rejected".into());
        t.storage.fail_next_commit(rejection.clone());
        let mut rejected_child = None;
        let error = t
            .session
            .commit(|tx| {
                rejected_child = Some(
                    tx.fork_conversation(parent, fork_at, ConversationOwnership::Ownerless)?
                        .id,
                );
                Ok(())
            })
            .unwrap_err();
        assert_eq!(error, SessionError::Storage(rejection.clone()));
        let failed = t
            .session
            .commit(|tx| tx.create_conversation(ConversationOwnership::Ownerless))
            .unwrap_err();
        assert_eq!(failed.name(), "SessionFailed");
        assert_eq!(failed.cause(), Some(&SessionError::Storage(rejection.clone())));
        // The failed Session closed its Storage; reopened, it holds nothing of the batch.
        assert_eq!(
            t.session.wait_closed(),
            SessionEnd::Failed(SessionError::Storage(rejection))
        );
        t.storage.reopen();
        assert_eq!(t.storage.conversation(rejected_child.unwrap()).unwrap(), None);
    });
}

#[test]
fn durable_session_forks_retire_a_copied_document_and_recreate_the_address_in_the_fork_transaction() {
    each_backend(|backend| {
        let doc = value_doc("fork.retire-recreate", AS_OF, "fresh");
        let t = TestSession::open(backend);
        let parent = t.create_conversation();
        let fork_at = t
            .session
            .commit(|tx| {
                let at = point(tx, parent, "point");
                set_value(tx.doc(&doc, Owner::Conversation(parent))?, json!("copied"))?;
                Ok(at)
            })
            .unwrap();
        let child = t
            .session
            .commit(|tx| {
                let created = tx.fork_conversation(parent, fork_at, ConversationOwnership::Ownerless)?;
                tx.retire_doc(&doc, Owner::Conversation(created.id))?;
                set_value(tx.doc(&doc, Owner::Conversation(created.id))?, json!("replacement"))?;
                Ok(created)
            })
            .unwrap();
        let kinds = types(&t.storage.last_commit());
        for kind in ["document.copy", "document.create", "document.retire"] {
            assert_eq!(kinds.iter().filter(|k| *k == kind).count(), 1, "{kind}");
        }
        assert_eq!(read(&t, &doc, child.id), Some(json!({ "value": "replacement" })));
    });
}

#[test]
fn durable_session_forks_copy_only_conversation_documents() {
    each_backend(|backend| {
        let copied = value_doc("fork.scope.conversation", CURRENT, "conversation");
        let session_only = value_doc("fork.scope.session", DocSemantics::Session, "session");
        let task_only = value_doc("fork.scope.task", DocSemantics::Task, "task");
        let work = TaskDefinition::new("fork.scope.work", 1, |_| json!({ "phase": "start" }));
        let t = TestSession::open(backend);
        let parent = t.create_conversation();
        let (fork_at, task_id) = t
            .session
            .commit(|tx| {
                let at = point(tx, parent, "point");
                let options = TaskOptions {
                    conversation_id: Some(parent),
                    ..TaskOptions::default()
                };
                let task_id = tx.create_task(&work, Value::Null, options)?;
                tx.doc(&copied, Owner::Conversation(parent))?;
                tx.doc(&session_only, Owner::Session)?;
                tx.doc(&task_only, Owner::Task(task_id))?;
                Ok((at, task_id))
            })
            .unwrap();
        let child = fork(&t, parent, fork_at);
        let copies: Vec<Value> = t
            .storage
            .last_commit()
            .iter()
            .map(json)
            .filter(|w| w["type"] == "document.copy")
            .map(|w| w["record"]["kind"].clone())
            .collect();
        assert_eq!(copies, [json!("fork.scope.conversation")]);
        let published: Vec<String> = document_copy_changes(&t.last_publication())
            .into_iter()
            .map(|(record, ..)| record.identity.kind)
            .collect();
        assert_eq!(published, ["fork.scope.conversation"]);
        assert_eq!(read(&t, &copied, child.id), Some(json!({ "value": "conversation" })));
        assert_eq!(
            t.session
                .snapshot(&session_only, Owner::Session)
                .unwrap()
                .map(|v| (*v).clone()),
            Some(json!({ "value": "session" }))
        );
        assert_eq!(
            t.session
                .snapshot(&task_only, Owner::Task(task_id))
                .unwrap()
                .map(|v| (*v).clone()),
            Some(json!({ "value": "task" }))
        );
    });
}

#[test]
fn durable_session_forks_copy_every_family_member_across_storage_scan_pages() {
    each_backend(|backend| {
        let family = seeded_family("fork.pagination", CURRENT);
        let t = TestSession::open(backend);
        let parent = t.create_conversation();
        let fork_at = t.session.commit(|tx| Ok(point(tx, parent, "point"))).unwrap();
        t.session
            .commit(|tx| {
                for index in 0..260 {
                    tx.doc_member(
                        &family,
                        Owner::Conversation(parent),
                        &format!("member-{index}"),
                        json!(index),
                    )?;
                }
                Ok(())
            })
            .unwrap();
        let child = fork(&t, parent, fork_at);
        assert_eq!(
            types(&t.storage.last_commit())
                .iter()
                .filter(|k| *k == "document.copy")
                .count(),
            260
        );
        assert_eq!(
            read_member(&t, &family, child.id, "member-0"),
            Some(json!({ "value": 0 }))
        );
        assert_eq!(
            read_member(&t, &family, child.id, "member-259"),
            Some(json!({ "value": 259 }))
        );
    });
}
