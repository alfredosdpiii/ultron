//! Port of upstream `test/session-tables.test.ts` ("Session transaction tables"), run against every backend.
//! Not ported: non-strict JSON rejection (a `serde_json::Value` is always strict JSON).

mod session_support;

use chord_delta::path;
use durable::{
    ConversationId, ConversationOwnership, ConversationQuery, ConversationRecord, DocDefinition, DocSemantics,
    DocumentFork, DocumentHistory, DocumentPoint, DocumentQuery, DocumentScope, EntryDraft, EntryHead, EntryId,
    EntryQuery, Owner, SessionError, Storage, TaskDefinition, TaskId, TaskOptions, TaskOutcome, TaskOwnership,
    TaskQuery, TaskRecord, TaskState,
};
use serde_json::{Value, json};
use session_support::*;

fn work_task() -> TaskDefinition {
    TaskDefinition::new("test.work", 1, |_| json!({ "phase": "start" }))
}

fn progress_doc() -> durable::Doc {
    DocDefinition::new("test.progress", 1, DocSemantics::Task, |_| json!({ "lines": [] }))
        .define()
        .unwrap()
}

fn step_doc() -> durable::Doc {
    DocDefinition::new("test.step", 1, DocSemantics::Task, |_| json!({ "lines": [] }))
        .family()
        .define()
        .unwrap()
}

fn notes_doc() -> durable::Doc {
    DocDefinition::new(
        "test.notes",
        1,
        DocSemantics::Conversation {
            history: DocumentHistory::Rewindable,
            fork: DocumentFork::AsOf,
        },
        |_| json!({ "text": "" }),
    )
    .define()
    .unwrap()
}

fn in_conversation(conversation_id: ConversationId) -> TaskOptions {
    TaskOptions {
        conversation_id: Some(conversation_id),
        ..TaskOptions::default()
    }
}

fn terminal(task: &TaskRecord) -> TaskRecord {
    TaskRecord {
        id: task.id,
        conversation_id: task.conversation_id,
        kind: task.kind.clone(),
        version: task.version,
        input: task.input.clone(),
        owner: None,
        background: task.background,
        abort_requested: task.abort_requested,
        abort_reason: None,
        abandon_on_restart: false,
        started_at: None,
        ended_at: None,
        state: TaskState::Terminal {
            outcome: TaskOutcome::Completed {
                result: json!({ "ok": true }),
            },
        },
        memos: None,
    }
}

fn pending(id: TaskId, conversation_id: ConversationId, path: &str, abort_requested: bool) -> TaskRecord {
    TaskRecord {
        id,
        conversation_id,
        kind: "test.work".into(),
        version: 1,
        input: json!({ "path": path }),
        owner: None,
        background: false,
        abort_requested,
        abort_reason: None,
        abandon_on_restart: false,
        started_at: None,
        ended_at: None,
        state: TaskState::Pending {
            checkpoint: json!({ "phase": "start" }),
        },
        memos: None,
    }
}

fn create_task(t: &TestSession, conversation_id: ConversationId, with_document: bool) -> TaskId {
    t.session
        .commit(|tx| {
            let task_id = tx.create_task(&work_task(), json!({ "path": "a" }), in_conversation(conversation_id))?;
            if with_document {
                tx.doc(&progress_doc(), Owner::Task(task_id))?
                    .push(&path!["lines"], vec![json!("started")])?;
            }
            Ok(task_id)
        })
        .unwrap()
}

fn entries_query(conversation_id: ConversationId) -> EntryQuery {
    EntryQuery {
        conversation_id,
        min_entry_id: None,
        max_entry_id: None,
        order: None,
    }
}

#[test]
fn durable_session_tables_allow_table_reads_only_before_the_first_table_write() {
    each_backend(|backend| {
        let t = TestSession::open(backend);
        let c = t.create_conversation();
        t.session
            .commit(|tx| {
                assert_eq!(
                    tx.conversation(c)?,
                    Some(ConversationRecord {
                        id: c,
                        parent: None,
                        owner: None
                    })
                );
                let page = tx.scan_conversations(&ConversationQuery::default(), 1, None)?;
                assert_eq!(
                    serde_json::to_value(&page).unwrap(),
                    json!({ "items": [{ "id": c.0 }] })
                );
                let tasks = TaskQuery {
                    conversation_id: Some(c),
                    ..TaskQuery::default()
                };
                assert!(tx.scan_tasks(&tasks, 10, None)?.items.is_empty());
                assert!(tx.scan_entries(&entries_query(c), 10, None)?.items.is_empty());
                tx.append_entry(c, EntryDraft::new("note"))?;
                assert_eq!(
                    tx.conversation(c).unwrap_err(),
                    SessionError::ReadAfterWrite("conversation")
                );
                let error = tx.task(TaskId(1)).unwrap_err();
                assert_eq!(
                    error.to_string(),
                    "Tx.task() cannot read tables after the first table write"
                );
                assert_eq!(error.name(), "ReadAfterWrite");
                assert_eq!(tx.entry(EntryId(1)).unwrap_err().name(), "ReadAfterWrite");
                assert_eq!(
                    tx.scan_conversations(&ConversationQuery::default(), 10, None)
                        .unwrap_err()
                        .name(),
                    "ReadAfterWrite"
                );
                assert_eq!(
                    tx.scan_entries(&entries_query(c), 10, None).unwrap_err().name(),
                    "ReadAfterWrite"
                );
                // Document access remains available after table writes.
                tx.doc(&notes_doc(), Owner::Conversation(c))?
                    .set(&path!["text"], json!("after write"))?;
                Ok(())
            })
            .unwrap();
        assert_eq!(
            *t.session
                .snapshot(&notes_doc(), Owner::Conversation(c))
                .unwrap()
                .unwrap(),
            json!({ "text": "after write" })
        );
    });
}

#[test]
fn durable_session_tables_pass_caller_selected_limits_and_cursors_through_table_scans() {
    each_backend(|backend| {
        let t = TestSession::open(backend);
        let ids = [
            t.create_conversation(),
            t.create_conversation(),
            t.create_conversation(),
        ];
        t.session
            .commit(|tx| {
                let first = tx.scan_conversations(&ConversationQuery::default(), 2, None)?;
                assert_eq!(first.items.iter().map(|c| c.id).collect::<Vec<_>>(), ids[..2]);
                assert!(first.next.is_some());
                let second = tx.scan_conversations(&ConversationQuery::default(), 2, first.next.as_ref())?;
                assert_eq!(second.items.iter().map(|c| c.id).collect::<Vec<_>>(), ids[2..]);
                assert!(second.next.is_none());
                Ok(())
            })
            .unwrap();
    });
}

#[test]
fn durable_session_tables_treat_set_task_as_the_first_table_write() {
    each_backend(|backend| {
        let t = TestSession::open(backend);
        let c = t.create_conversation();
        let task_id = create_task(&t, c, false);
        t.session
            .commit(|tx| {
                let task = tx.task(task_id)?.unwrap();
                tx.set_task(task)?;
                assert_eq!(tx.task(task_id).unwrap_err(), SessionError::ReadAfterWrite("task"));
                Ok(())
            })
            .unwrap();
    });
}

#[test]
fn durable_session_tables_create_conversations_entries_and_tasks_with_minted_ids() {
    each_backend(|backend| {
        let t = TestSession::open(backend);
        let (conversation, first, headed, task) = t
            .session
            .commit(|tx| {
                let conversation = tx.create_conversation(ConversationOwnership::Ownerless)?;
                let first = tx.append_entry(
                    conversation.id,
                    EntryDraft {
                        data: Some(json!("one")),
                        ..EntryDraft::new("note")
                    },
                )?;
                let headed = tx.append_entry(
                    conversation.id,
                    EntryDraft {
                        head: Some(EntryHead::SelfId),
                        ..EntryDraft::new("summary")
                    },
                )?;
                let options = TaskOptions {
                    background: true,
                    ..in_conversation(conversation.id)
                };
                let task = tx.create_task(&work_task(), json!({ "path": "x" }), options)?;
                Ok((conversation, first, headed, task))
            })
            .unwrap();
        let ids = std::collections::HashSet::from([conversation.id.0, first.id.0, headed.id.0, task.0]);
        assert_eq!(ids.len(), 4);
        assert_eq!(headed.head, Some(headed.id));
        assert_eq!(
            serde_json::to_value(&first).unwrap(),
            json!({ "id": first.id.0, "conversationId": conversation.id.0, "kind": "note", "data": "one" })
        );
        assert_eq!(t.storage.entry(headed.id).unwrap().unwrap().entry, headed);
        assert_eq!(
            serde_json::to_value(t.storage.task(task).unwrap().unwrap()).unwrap(),
            json!({
                "id": task.0,
                "conversationId": conversation.id.0,
                "kind": "test.work",
                "version": 1,
                "input": { "path": "x" },
                "background": true,
                "abortRequested": false,
                "state": { "status": "pending", "checkpoint": { "phase": "start" } },
            })
        );
        let changes = t.last_publication().changes;
        assert_eq!(changes.len(), 4);
        let admitted = t.storage.last_commit();
        for change in &changes {
            let durable::CommitChange::Table(write) = change else {
                panic!("table change")
            };
            assert!(admitted.contains(write));
        }
        let mut kinds: Vec<String> = changes
            .iter()
            .map(|change| match change {
                durable::CommitChange::Table(write) => json(write)["type"].as_str().unwrap().to_owned(),
                _ => panic!("table change"),
            })
            .collect();
        kinds.sort();
        assert_eq!(kinds, ["conversation", "entry", "entry", "task"]);
        assert!(
            changes.contains(&durable::CommitChange::Table(durable::StorageWrite::Conversation {
                value: conversation.clone()
            }))
        );
        assert!(
            changes.contains(&durable::CommitChange::Table(durable::StorageWrite::Entry {
                value: first
            }))
        );
        assert!(
            changes.contains(&durable::CommitChange::Table(durable::StorageWrite::Entry {
                value: headed
            }))
        );

        let error = t
            .session
            .commit(|tx| tx.create_task(&work_task(), json!({ "path": "x" }), TaskOptions::default()))
            .unwrap_err();
        assert_eq!(error.to_string(), "Tx.createTask() requires options.conversationId");
        let error = t
            .session
            .commit(|tx| tx.append_entry(ConversationId(12345), EntryDraft::new("note")))
            .unwrap_err();
        assert_eq!(error.to_string(), "Conversation 12345 does not exist");
    });
}

#[test]
fn durable_session_tables_create_a_conversation_with_an_explicitly_staged_task_owner() {
    each_backend(|backend| {
        let t = TestSession::open(backend);
        let parent = t.create_conversation();
        let (supervisor, child) = t
            .session
            .commit(|tx| {
                let options = TaskOptions {
                    background: true,
                    ..in_conversation(parent)
                };
                let supervisor = tx.create_task(&work_task(), json!({ "path": "background" }), options)?;
                let child = tx.create_conversation(ConversationOwnership::Task(supervisor))?;
                Ok((supervisor, child))
            })
            .unwrap();
        assert_eq!(
            serde_json::to_value(child.owner).unwrap(),
            json!({ "conversationId": parent.0, "taskId": supervisor.0 })
        );
        assert_eq!(t.storage.conversation(child.id).unwrap(), Some(child.clone()));
        let error = t
            .session
            .commit(|tx| {
                let task = tx.task(supervisor)?.unwrap();
                tx.set_task(TaskRecord {
                    conversation_id: child.id,
                    ..task
                })
            })
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("Task {} cannot change conversations", supervisor.0)
        );
        assert_eq!(t.storage.task(supervisor).unwrap().unwrap().conversation_id, parent);
    });
}

#[test]
fn durable_session_tables_reject_missing_terminal_and_abort_marked_conversation_owners_atomically() {
    each_backend(|backend| {
        let t = TestSession::open(backend);
        let parent = t.create_conversation();
        let mut moved = None;
        let error = t
            .session
            .commit(|tx| {
                let task_id = tx.create_task(&work_task(), json!({ "path": "move" }), in_conversation(parent))?;
                moved = Some(task_id);
                tx.set_task(pending(task_id, ConversationId(998), "move", false))
            })
            .unwrap_err();
        assert!(error.to_string().contains("cannot change conversations"));
        assert_eq!(t.storage.task(moved.unwrap()).unwrap(), None);

        let error = t
            .session
            .commit(|tx| tx.create_conversation(ConversationOwnership::Task(TaskId(999))))
            .unwrap_err();
        assert_eq!(error.to_string(), "Conversation owner task 999 does not exist");

        let mut rejected_child = None;
        let error = t
            .session
            .commit(|tx| {
                let supervisor =
                    tx.create_task(&work_task(), json!({ "path": "aborting" }), in_conversation(parent))?;
                rejected_child = Some(tx.create_conversation(ConversationOwnership::Task(supervisor))?.id);
                tx.set_task(pending(supervisor, parent, "aborting", true))
            })
            .unwrap_err();
        assert!(error.to_string().ends_with("is abort-marked"), "{error}");
        assert_eq!(t.storage.conversation(rejected_child.unwrap()).unwrap(), None);

        let error = t
            .session
            .commit(|tx| {
                let supervisor =
                    tx.create_task(&work_task(), json!({ "path": "terminal" }), in_conversation(parent))?;
                tx.create_conversation(ConversationOwnership::Task(supervisor))?;
                tx.set_task(terminal(&pending(supervisor, parent, "terminal", false)))
            })
            .unwrap_err();
        assert!(error.to_string().ends_with("is terminal"), "{error}");

        let terminal_owner = create_task(&t, parent, false);
        t.session
            .commit(|tx| {
                let task = tx.task(terminal_owner)?.unwrap();
                tx.set_task(terminal(&task))
            })
            .unwrap();
        let error = t
            .session
            .commit(|tx| tx.create_conversation(ConversationOwnership::Task(terminal_owner)))
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("Conversation owner task {} is terminal", terminal_owner.0)
        );
    });
}

#[test]
fn durable_session_tables_omit_absent_entry_data() {
    each_backend(|backend| {
        let t = TestSession::open(backend);
        let c = t.create_conversation();
        let omitted = t
            .session
            .commit(|tx| tx.append_entry(c, EntryDraft::new("omitted")))
            .unwrap();
        assert!(serde_json::to_value(&omitted).unwrap().get("data").is_none());
        let stored = t.storage.entry(omitted.id).unwrap().unwrap().entry;
        assert!(serde_json::to_value(stored).unwrap().get("data").is_none());
        let null = t
            .session
            .commit(|tx| {
                tx.append_entry(
                    c,
                    EntryDraft {
                        data: Some(Value::Null),
                        ..EntryDraft::new("null")
                    },
                )
            })
            .unwrap();
        assert_eq!(t.storage.entry(null.id).unwrap().unwrap().entry.data, Some(Value::Null));
    });
}

#[test]
fn durable_session_tables_replace_task_records_completely() {
    each_backend(|backend| {
        let t = TestSession::open(backend);
        let c = t.create_conversation();
        let task_id = create_task(&t, c, false);
        t.session
            .commit(|tx| {
                let task = tx.task(task_id)?.unwrap();
                tx.set_task(TaskRecord {
                    state: TaskState::Running {
                        checkpoint: json!({ "phase": "next", "step": 2 }),
                    },
                    memos: Some(json!({ "choice": "b" }).as_object().unwrap().clone()),
                    ..task
                })
            })
            .unwrap();
        assert_matches(
            &serde_json::to_value(t.storage.task(task_id).unwrap().unwrap()).unwrap(),
            json!({
                "state": { "status": "running", "checkpoint": { "phase": "next", "step": 2 } },
                "memos": { "choice": "b" },
            }),
        );
    });
}

#[test]
fn durable_session_tables_create_a_task_and_then_its_document_without_read_after_write() {
    each_backend(|backend| {
        let t = TestSession::open(backend);
        let c = t.create_conversation();
        let task_id = t
            .session
            .commit(|tx| {
                let task_id = tx.create_task(&work_task(), json!({ "path": "a" }), in_conversation(c))?;
                // Validation uses the candidate task record, not a caller table read.
                tx.doc(&progress_doc(), Owner::Task(task_id))?
                    .push(&path!["lines"], vec![json!("created")])?;
                tx.doc_member(&step_doc(), Owner::Task(task_id), "one", Value::Null)?
                    .push(&path!["lines"], vec![json!("step")])?;
                Ok(task_id)
            })
            .unwrap();
        assert_eq!(
            *t.session
                .snapshot(&progress_doc(), Owner::Task(task_id))
                .unwrap()
                .unwrap(),
            json!({ "lines": ["created"] })
        );
        let publication = t.last_publication();
        assert!(publication.changes.iter().any(|change| matches!(
            change,
            durable::CommitChange::Table(durable::StorageWrite::Task { value }) if value.id == task_id
        )));
        let documents = document_changes(&publication);
        assert_eq!(documents.len(), 2);
        // Task documents derive their conversation from the task record.
        assert!(documents.iter().all(|document| document.conversation_id == Some(c)));

        t.session
            .commit(|tx| {
                tx.create_conversation(ConversationOwnership::Ownerless)?;
                tx.doc(&progress_doc(), Owner::Task(task_id))?
                    .push(&path!["lines"], vec![json!("committed task")])?;
                Ok(())
            })
            .unwrap();
        assert_eq!(document_changes(&t.last_publication())[0].conversation_id, Some(c));
    });
}

#[test]
fn durable_session_tables_reject_task_documents_after_a_terminal_candidate() {
    each_backend(|backend| {
        let t = TestSession::open(backend);
        let c = t.create_conversation();
        let task_id = create_task(&t, c, true);
        t.session
            .commit(|tx| {
                let task = tx.task(task_id)?.unwrap();
                let progress = tx.doc(&progress_doc(), Owner::Task(task_id))?;
                tx.set_task(terminal(&task))?;
                let expected = format!("Task {} is terminal", task_id.0);
                assert_eq!(
                    tx.doc(&progress_doc(), Owner::Task(task_id)).unwrap_err().to_string(),
                    expected
                );
                assert_eq!(
                    tx.doc_member(&step_doc(), Owner::Task(task_id), "late", Value::Null)
                        .unwrap_err()
                        .to_string(),
                    expected
                );
                assert!(
                    tx.set_task(task)
                        .unwrap_err()
                        .to_string()
                        .contains("terminal candidate")
                );
                progress.push(&path!["lines"], vec![json!("final")])?;
                Ok(())
            })
            .unwrap();
        let error = t
            .session
            .commit(|tx| tx.doc(&progress_doc(), Owner::Task(task_id)).map(drop))
            .unwrap_err();
        assert_eq!(error.to_string(), format!("Task {} is terminal", task_id.0));
    });
}

#[test]
fn durable_session_tables_retire_task_documents_at_terminal_settlement() {
    each_backend(|backend| {
        let t = TestSession::open(backend);
        let c = t.create_conversation();
        let task_id = create_task(&t, c, true);
        t.session
            .commit(|tx| {
                tx.doc_member(&step_doc(), Owner::Task(task_id), "committed", Value::Null)?
                    .push(&path!["lines"], vec![json!("x")])?;
                Ok(())
            })
            .unwrap();
        let published = t.publication_count();
        t.session
            .commit(|tx| {
                let task = tx.task(task_id)?.unwrap();
                tx.doc_member(&step_doc(), Owner::Task(task_id), "new", Value::Null)?
                    .push(&path!["lines"], vec![json!("created then retired")])?;
                tx.set_task(terminal(&task))
            })
            .unwrap();
        let writes: Vec<Value> = t.storage.last_commit().iter().map(json).collect();
        assert_eq!(writes.len(), 5);
        let of = |kind: &str| writes.iter().filter(|w| w["type"] == kind).cloned().collect::<Vec<_>>();
        assert_eq!(of("task").len(), 1);
        let creations = of("document.create");
        let retirements = of("document.retire");
        assert_eq!(creations.len(), 1);
        assert_eq!(retirements.len(), 3);
        assert!(retirements.iter().any(|r| r["id"] == creations[0]["record"]["id"]));
        assert_eq!(t.publication_count(), published + 1);
        let publication = t.last_publication();
        let tasks = publication
            .changes
            .iter()
            .filter(|change| matches!(change, durable::CommitChange::Table(durable::StorageWrite::Task { .. })))
            .count();
        assert_eq!(tasks, 1);
        let documents = document_changes(&publication);
        assert_eq!(
            documents.iter().map(|d| d.value.clone()).collect::<Vec<_>>(),
            [None, None, None]
        );
        assert!(
            documents
                .iter()
                .all(|d| d.ops.is_empty() && d.conversation_id == Some(c))
        );
        assert_eq!(t.session.snapshot(&progress_doc(), Owner::Task(task_id)).unwrap(), None);
        assert_eq!(
            t.session
                .snapshot_member(&step_doc(), Owner::Task(task_id), "committed")
                .unwrap(),
            None
        );
        let alive = t
            .storage
            .scan_documents(
                &DocumentQuery {
                    scope: DocumentScope::Task { task_id },
                    at: DocumentPoint::Current,
                    kind: None,
                },
                10,
                None,
            )
            .unwrap();
        assert!(alive.items.is_empty());
        let error = t
            .session
            .commit(|tx| {
                let task = tx.task(task_id)?.unwrap();
                tx.set_task(terminal(&task))
            })
            .unwrap_err();
        assert_eq!(error.to_string(), format!("Task {} is already terminal", task_id.0));
    });
}

#[test]
fn durable_session_tables_validate_document_owners() {
    each_backend(|backend| {
        let t = TestSession::open(backend);
        let error = t
            .session
            .commit(|tx| tx.doc(&progress_doc(), Owner::Task(TaskId(4242))).map(drop))
            .unwrap_err();
        assert_eq!(error.to_string(), "Task 4242 does not exist");
        let error = t
            .session
            .commit(|tx| {
                tx.doc(&notes_doc(), Owner::Conversation(ConversationId(4242)))
                    .map(drop)
            })
            .unwrap_err();
        assert_eq!(error.to_string(), "Conversation 4242 does not exist");
    });
}

#[test]
fn durable_session_tables_attribute_entries_and_default_task_conversations_to_the_commit_scope() {
    each_backend(|backend| {
        let t = TestSession::open(backend);
        let c = t.create_conversation();
        let owner = create_task(&t, c, false);
        let scope = durable::TxScope {
            conversation_id: Some(c),
            task_id: Some(owner),
        };
        let (entry, child) = t
            .session
            .commit_with(scope, |tx| {
                let entry = tx.append_entry(c, EntryDraft::new("by-task"))?;
                let child = tx.create_task(&work_task(), Value::Null, TaskOptions::default())?;
                Ok((entry, child))
            })
            .unwrap();
        assert_eq!(entry.by_task_id, Some(owner));
        assert_eq!(t.storage.task(child).unwrap().unwrap().conversation_id, c);
        let error = t
            .session
            .commit(|tx| {
                let options = TaskOptions {
                    ownership: TaskOwnership::Task(owner),
                    background: true,
                    ..TaskOptions::default()
                };
                tx.create_task(&work_task(), Value::Null, options)
            })
            .unwrap_err();
        assert_eq!(error.to_string(), "A child task cannot be background");
        assert_eq!(error.name(), "TypeError");
        let other = t.create_conversation();
        let error = t
            .session
            .commit(|tx| {
                let options = TaskOptions {
                    ownership: TaskOwnership::Task(owner),
                    conversation_id: Some(other),
                    ..TaskOptions::default()
                };
                tx.create_task(&work_task(), Value::Null, options)
            })
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("A child task lives in its owner's conversation {}", c.0)
        );
    });
}

/// Not an upstream test: the Tx dedup lookup used by the harness's submission admission.
#[test]
fn durable_session_tables_find_submissions_by_request_id_within_their_conversation() {
    each_backend(|backend| {
        let t = TestSession::open(backend);
        let c = t.create_conversation();
        let other = t.create_conversation();
        let created = t
            .session
            .commit(|tx| {
                tx.create_submission(durable::SubmissionCreate {
                    conversation_id: c,
                    request_id: Some("r1".into()),
                    kind: durable::SubmissionType::Input,
                    status: durable::SubmissionStatus::Queued,
                    entry: None,
                    answer: None,
                    reason: None,
                    detail: None,
                })
            })
            .unwrap();
        t.session
            .commit(|tx| {
                let found = tx.submission_by_request(c, "r1")?.unwrap();
                assert_eq!(
                    serde_json::to_value(&found).unwrap(),
                    json!({ "id": created.id.0, "conversationId": c.0, "requestId": "r1", "type": "input", "status": "queued" })
                );
                assert_eq!(tx.submission_by_request(other, "r1")?, None);
                assert_eq!(tx.submission_by_request(c, "r2")?, None);
                tx.append_entry(c, EntryDraft::new("note"))?;
                assert_eq!(
                    tx.submission_by_request(c, "r1").unwrap_err(),
                    SessionError::ReadAfterWrite("submissionByRequest")
                );
                Ok(())
            })
            .unwrap();
    });
}
