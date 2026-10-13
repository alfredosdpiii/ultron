//! A30 (partial): correction and deletion propagate to managed memory views without changing trusted instructions.
//! Ported from packages/coding-agent/test/ultron-a30-forget.test.ts: the worker's `memory.*` dispatch (Rust:
//! `KernelApis` + `MemoryApi` + the worker gate over the session's file store, as `session_core::compose` builds it)
//! with the same fake Hindsight bank (a `MemoryBackend` object in both). Not ported from the first case: the
//! rendered `/memory list` view (`formatInspection` of inspection-commands.ts) and the `refinements.propose` refusal
//! (refinement service); Rust has neither.

use kernel::{HostHandler, Signal};
use kernel_apis::memory::MemoryBackend;
use kernel_apis::{FileDocumentStore, KernelApis, MemoryApi, MemoryService, worker_gate};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const AGENTS_MD: &str = "# Project rules\n\nAlways run the tests.\r\nTrailing bytes: é—\n";
const FORGET_TEXT: &str = "The deploy password hint is PURPLE-ELEPHANT-42";
const OLD_CLAIM: &str = "The release branch is release/1.x";
const NEW_CLAIM: &str = "The release branch is release/2.x";

#[derive(Default)]
struct Bank {
    /// Insertion-ordered documents `(id, original_text, tags)`.
    documents: Vec<(String, String, Value)>,
    deleted: Vec<(String, String, Value)>,
    recall: usize,
    retain: usize,
    get: usize,
    delete: Vec<String>,
    stale_index: bool,
}

/// The A30 fake bank; `stale_index` models a recall index that still returns deleted documents.
#[derive(Clone, Default)]
struct Hindsight(Arc<Mutex<Bank>>);

impl Hindsight {
    fn bank(&self) -> std::sync::MutexGuard<'_, Bank> {
        self.0.lock().unwrap()
    }
}

impl MemoryBackend for Hindsight {
    fn namespace(&self) -> String {
        "fake://a30".into()
    }
    fn scope_tags(&self) -> Map<String, Value> {
        json!({"session": ["ultron:session:a30"], "project": ["ultron:project:a30"], "global": ["ultron:global:a30"]})
            .as_object()
            .unwrap()
            .clone()
    }
    fn supports(&self, call: &str) -> bool {
        call != "operation"
    }
    fn recall(&self, request: &Value, _: &Signal) -> Result<Value, String> {
        let mut bank = self.bank();
        bank.recall += 1;
        let mut visible = bank.documents.clone();
        if bank.stale_index {
            visible.extend(bank.deleted.iter().cloned());
        }
        let results: Vec<Value> = visible
            .into_iter()
            .filter(|(_, _, tags)| *tags == request["tags"])
            .map(|(id, text, tags)| json!({"id": format!("unit-{id}"), "document_id": id, "text": text, "tags": tags}))
            .collect();
        Ok(json!({"results": results}))
    }
    fn retain(&self, request: &Value, _: &Signal) -> Result<Value, String> {
        let mut bank = self.bank();
        bank.retain += 1;
        let item = &request["items"][0];
        let id = item["document_id"].as_str().unwrap().to_string();
        let document = (
            id.clone(),
            item["content"].as_str().unwrap().to_string(),
            item["tags"].clone(),
        );
        match bank.documents.iter_mut().find(|d| d.0 == id) {
            Some(existing) => *existing = document,
            None => bank.documents.push(document),
        }
        Ok(json!({"success": true, "async": false}))
    }
    fn get(&self, id: &str, _: &Signal) -> Result<Value, String> {
        let mut bank = self.bank();
        bank.get += 1;
        Ok(bank.documents.iter().find(|d| d.0 == id).map_or(
            Value::Null,
            |(id, text, tags)| json!({"id": id, "original_text": text, "tags": tags}),
        ))
    }
    fn delete(&self, id: &str, _: &Signal) -> Result<Value, String> {
        let mut bank = self.bank();
        bank.delete.push(id.into());
        if let Some(index) = bank.documents.iter().position(|d| d.0 == id) {
            let document = bank.documents.remove(index);
            bank.deleted.push(document);
        }
        Ok(json!({"success": true, "document_id": id}))
    }
    fn operation(&self, _: &str, _: &Signal) -> Result<Value, String> {
        unreachable!()
    }
}

/// The worker's memory dispatch over the session's durable module store (`rlm-modules/<session>/memory.json`).
fn services(store: &Path, bank: &Hindsight) -> impl Fn(&str, Value) -> Result<Value, String> {
    let service = MemoryService::new(
        Arc::new(FileDocumentStore::new(store)),
        Arc::new(bank.clone()),
        worker_gate(),
    )
    .unwrap();
    let refuse: Arc<dyn HostHandler> =
        Arc::new(|kind: &str, _: Map<String, Value>, _: &Signal| Err(format!("Unknown host request: {kind}")));
    let worker = KernelApis::new(refuse).with(Arc::new(MemoryApi { service: Some(service) }));
    move |kind: &str, payload: Value| worker.handle(kind, payload.as_object().unwrap().clone(), &Signal::new())
}

fn evidence() -> Value {
    json!([{"ref": "user:statement"}])
}

/// The newest claim or forget operation on `memory_id`.
fn latest(list: &Value, memory_id: &Value) -> Value {
    list.as_array()
        .unwrap()
        .iter()
        .rfind(|op| op["memoryId"] == *memory_id && op["kind"] != "get" && op["phase"] == "backend")
        .cloned()
        .unwrap_or(Value::Null)
}

struct Fixture {
    _root: tempfile::TempDir,
    cwd: PathBuf,
    store: PathBuf,
}

fn fixture() -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("AGENTS.md"), AGENTS_MD).unwrap();
    let store = root.path().join("sessions/rlm-modules/a30/memory.json");
    Fixture {
        _root: root,
        cwd,
        store,
    }
}

fn listing(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn acceptance_a30_forgotten_and_corrected_memories_leave_every_view_ported_part() {
    let f = fixture();
    let agents_before = std::fs::read(f.cwd.join("AGENTS.md")).unwrap();
    let files_before = listing(&f.cwd);
    let bank = Hindsight::default();
    let call = services(&f.store, &bank);

    let forget_me = call("memory.propose", json!({"text": FORGET_TEXT, "evidence": evidence()})).unwrap();
    let correct_me = call("memory.propose", json!({"text": OLD_CLAIM, "evidence": evidence()})).unwrap();
    assert_eq!(forget_me["state"], "stored");
    assert_eq!(correct_me["state"], "stored");
    let before = call("memory.prepare", json!({"query": "project facts", "taskId": "before"})).unwrap();
    let context = before["context"].as_str().unwrap();
    assert!(context.contains(FORGET_TEXT) && context.contains(OLD_CLAIM));

    // Forget: the backend delete runs and its completion is recorded, not merely requested.
    let forgotten = call("memory.forget", json!({"id": forget_me["memoryId"]})).unwrap();
    assert_eq!(
        (&forgotten["kind"], &forgotten["state"], &forgotten["memoryId"]),
        (&json!("forget"), &json!("forgotten"), &forget_me["memoryId"])
    );
    assert_eq!(bank.bank().delete, vec![forget_me["memoryId"].as_str().unwrap()]);
    assert!(
        !bank
            .bank()
            .documents
            .iter()
            .any(|d| forget_me["memoryId"] == d.0.as_str())
    );

    // Correct: the old claim is superseded in place; history links, it does not rewrite.
    let corrected = call(
        "memory.correct",
        json!({"id": correct_me["memoryId"], "text": NEW_CLAIM, "evidence": [{"ref": "user:correction"}]}),
    )
    .unwrap();
    assert_eq!(
        (&corrected["kind"], &corrected["state"], &corrected["memoryId"]),
        (&json!("correct"), &json!("stored"), &correct_me["memoryId"])
    );
    assert_ne!(corrected["textHash"], correct_me["textHash"]);

    // Later prepare results carry neither the forgotten text nor the superseded claim.
    let after = call("memory.prepare", json!({"query": "project facts", "taskId": "after"})).unwrap();
    let context = after["context"].as_str().unwrap();
    assert!(!context.contains(FORGET_TEXT) && !context.contains(OLD_CLAIM) && context.contains(NEW_CLAIM));
    let why = call("memory.why", json!({"taskId": "after"})).unwrap();
    let ids: Vec<Value> = why[0]["references"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].clone())
        .collect();
    assert_eq!(
        ids,
        vec![json!(format!("unit-{}", correct_me["memoryId"].as_str().unwrap()))]
    );

    // get: forgotten is refused; corrected shows only the new content.
    let error = call("memory.get", json!({"id": forget_me["memoryId"]})).unwrap_err();
    assert!(error.contains("FORGOTTEN"), "{error}");
    let got = call("memory.get", json!({"id": correct_me["memoryId"]})).unwrap();
    assert_eq!(
        (&got["state"], &got["content"], &got["operation"]["kind"]),
        (&json!("stored"), &json!(NEW_CLAIM), &json!("correct"))
    );

    // list: forgotten is marked, content never appears.
    let list = call("memory.list", json!({})).unwrap();
    let last = latest(&list, &forget_me["memoryId"]);
    assert_eq!((&last["kind"], &last["state"]), (&json!("forget"), &json!("forgotten")));
    let last = latest(&list, &correct_me["memoryId"]);
    assert_eq!((&last["kind"], &last["state"]), (&json!("correct"), &json!("stored")));
    for view in [
        list.to_string(),
        why.to_string(),
        std::fs::read_to_string(&f.store).unwrap(),
    ] {
        assert!(!view.contains(FORGET_TEXT));
        assert!(!view.contains("PURPLE-ELEPHANT"));
        assert!(!view.contains(OLD_CLAIM));
    }
    assert_eq!(std::fs::read(f.cwd.join("AGENTS.md")).unwrap(), agents_before);
    assert_eq!(listing(&f.cwd), files_before);
}

#[test]
fn acceptance_a30_a_lagging_backend_index_cannot_reinject_a_forgotten_memory() {
    let f = fixture();
    let bank = Hindsight::default();
    let call = services(&f.store, &bank);
    let secret = call("memory.propose", json!({"text": FORGET_TEXT, "evidence": evidence()})).unwrap();
    call("memory.propose", json!({"text": NEW_CLAIM, "evidence": evidence()})).unwrap();
    call("memory.forget", json!({"id": secret["memoryId"]})).unwrap();

    bank.bank().stale_index = true;
    let prepared = call("memory.prepare", json!({"query": "facts", "taskId": "stale"})).unwrap();
    assert_eq!(bank.bank().recall, 1);
    assert!(!prepared["context"].as_str().unwrap().contains(FORGET_TEXT));
    let texts: Vec<Value> = prepared["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["text"].clone())
        .collect();
    assert_eq!(texts, vec![json!(NEW_CLAIM)]);
}

#[test]
fn acceptance_a30_forgetting_persists_across_a_new_service_instance_on_the_same_store() {
    let f = fixture();
    let agents_before = std::fs::read(f.cwd.join("AGENTS.md")).unwrap();
    let bank = Hindsight::default();
    let first = services(&f.store, &bank);
    let secret = first("memory.propose", json!({"text": FORGET_TEXT, "evidence": evidence()})).unwrap();
    first("memory.forget", json!({"id": secret["memoryId"]})).unwrap();

    // A fresh worker service (as after a restart) over the same session store and bank.
    bank.bank().stale_index = true;
    let second = services(&f.store, &bank);
    let list = second("memory.list", json!({})).unwrap();
    let last = latest(&list, &secret["memoryId"]);
    assert_eq!((&last["kind"], &last["state"]), (&json!("forget"), &json!("forgotten")));
    for (kind, payload) in [
        ("memory.get", json!({"id": secret["memoryId"]})),
        (
            "memory.correct",
            json!({"id": secret["memoryId"], "text": "resurrect", "evidence": [{"ref": "user:x"}]}),
        ),
        ("memory.forget", json!({"id": secret["memoryId"]})),
    ] {
        let error = second(kind, payload).unwrap_err();
        assert!(error.contains("FORGOTTEN"), "{kind}: {error}");
    }
    let prepared = second("memory.prepare", json!({"query": "facts", "taskId": "restart"})).unwrap();
    assert!(!prepared["context"].as_str().unwrap().contains(FORGET_TEXT));
    // Only the original delete reached the backend; refused retries do not re-dispatch.
    assert_eq!(bank.bank().delete, vec![secret["memoryId"].as_str().unwrap()]);
    assert_eq!(bank.bank().retain, 1);
    assert_eq!(std::fs::read(f.cwd.join("AGENTS.md")).unwrap(), agents_before);
}
