//! Migration of the pre-durable task journal (`ultron.tasks/root`, written by `task-store.ts` and its two Rust
//! ports) into the durable task table. Inputs are the documents the TypeScript journal stored, recorded in
//! `fixtures/task-store.json` (`generate.mjs`): every stored document imports one terminal `ultron.child` task per
//! record (unfinished records as `interrupted`, never replayed), a second import writes nothing, and every document
//! the old journal refused to open is refused with the same error.

mod chat_support;
mod support;

use chat_support::*;
use durable::{TaskRecord, TaskState};
use harness::chat::children::child_report;
use harness::chat::migrate::import_task_journal;
use serde_json::{Value, json};
use support::*;

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/task-store.json")).unwrap()
}

fn imported(chat: &harness::chat::ChatHarness, ids: &[durable::TaskId]) -> Vec<TaskRecord> {
    ids.iter().map(|id| chat.get_task(*id).unwrap().unwrap()).collect()
}

/// The status a legacy state imports as, and the result it reports.
fn expected(task: &Value) -> (&'static str, Value) {
    let interrupted = json!({
        "status": "interrupted",
        "error": "Owner ended; automatic replay is disabled",
        "verification": "unverified",
    });
    match task["state"].as_str().unwrap() {
        "completed" => ("completed", task["result"].clone()),
        "cancelled" => ("aborted", task["result"].clone()),
        "failed" | "interrupted" => ("failed", task["result"].clone()),
        _ => ("failed", interrupted),
    }
}

#[test]
fn harness_chat_migrate_imports_every_stored_journal_document_as_terminal_child_tasks_once() {
    let scenarios = fixture()["scenarios"].as_object().unwrap().clone();
    let mut documents = 0;
    for (name, scenario) in &scenarios {
        for step in scenario["steps"].as_array().unwrap() {
            let Some(stored) = step.get("stored").filter(|stored| stored["tasks"].is_array()) else {
                continue;
            };
            if harness::task_store::parse_document(stored).is_err() {
                continue;
            }
            documents += 1;
            let (chat, root) = open_chat(Controlled::new(Backend::Memory), &Setup::new());
            let ids = root.commit(|tx| import_task_journal(tx, root.id, stored)).unwrap();
            let tasks = stored["tasks"].as_array().unwrap();
            assert_eq!(ids.len(), tasks.len(), "{name}");
            for (record, task) in imported(&chat, &ids).iter().zip(tasks) {
                let (status, result) = expected(task);
                let TaskState::Terminal { outcome } = &record.state else {
                    panic!("{name}: an imported task is terminal");
                };
                assert_eq!(js(outcome)["status"], status, "{name}");
                assert_eq!(record.kind, "ultron.child");
                assert!(record.background);
                let mut legacy = task.clone();
                legacy.as_object_mut().unwrap().remove("result");
                assert_eq!(record.input["legacy"], legacy, "{name}");
                assert_eq!(
                    child_report(record).unwrap(),
                    json!({ "id": record.id.0.to_string(), "result": result }),
                    "{name}"
                );
            }
            // Importing again writes nothing.
            assert!(
                root.commit(|tx| import_task_journal(tx, root.id, stored))
                    .unwrap()
                    .is_empty()
            );
            // Imported records are history: nothing runs, and the conversation is idle.
            chat.wait_for_idle(&bg()).unwrap();
            assert!(chat.inspect().unwrap().tasks.is_empty());
            chat.close().unwrap();
        }
    }
    assert!(documents > 50, "{documents} documents");
}

#[test]
fn harness_chat_migrate_refuses_the_documents_the_old_journal_refused_with_its_error() {
    let scenarios = fixture()["scenarios"].as_object().unwrap().clone();
    let mut refused = 0;
    for (name, scenario) in &scenarios {
        let steps = scenario["steps"].as_array().unwrap();
        let Some(seed) = steps.iter().find(|step| step["op"] == "seed") else {
            continue;
        };
        let opened = steps
            .iter()
            .filter(|step| step["op"] != "seed")
            .find_map(|step| step["outcome"]["error"].as_str());
        let Some(error) = opened.filter(|error| error.contains("task document") || error.starts_with("Cannot resume"))
        else {
            continue;
        };
        refused += 1;
        let (chat, root) = open_chat(Controlled::new(Backend::Memory), &Setup::new());
        let result = root.commit(|tx| import_task_journal(tx, root.id, &seed["value"]));
        assert_eq!(
            result.err().map(|error| error.message()).as_deref(),
            Some(error),
            "{name}"
        );
        chat.close().unwrap();
    }
    assert!(refused >= 20, "{refused} refused documents");
}

#[test]
fn harness_chat_migrate_keeps_imported_results_across_reopen_and_reports_them_to_results() {
    for backend in [Backend::Sqlite, Backend::Jsonl] {
        let storage = Controlled::new(backend);
        let lifecycle = &fixture()["scenarios"]["lifecycle"]["steps"];
        let stored = lifecycle.as_array().unwrap().last().unwrap()["stored"].clone();
        let (chat, root) = open_chat(storage.clone(), &Setup::new());
        let ids = root.commit(|tx| import_task_journal(tx, root.id, &stored)).unwrap();
        chat.close().unwrap();
        let (chat, _) = open_chat(storage.reopen(), &Setup::new());
        let reports: Vec<Value> = imported(&chat, &ids).iter().filter_map(child_report).collect();
        assert_eq!(
            reports
                .iter()
                .map(|report| report["result"]["status"].clone())
                .collect::<Vec<_>>(),
            [json!("succeeded"), json!("failed")]
        );
        assert_eq!(reports[0]["result"]["value"], json!({ "n": [1, 2] }));
        assert_eq!(reports[1]["result"]["error"], "boom");
        chat.close().unwrap();
    }
}
