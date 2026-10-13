//! A18 (verified): ingestion acceptance is distinct from completed retention; forgetting persists.
//! Ported from packages/coding-agent/test/ultron-a18-retention-forget.test.ts over the Rust `MemoryService` with a
//! durable journal reopened as after a restart, through the real Hindsight adapter
//! (`hindsight::HindsightBackend`) over the fake Hindsight of `memory_support`, as the TS fake sits behind `createHindsightBackend`.

mod memory_support;

use kernel::Signal;
use kernel_apis::memory::MemoryBackend;
use kernel_apis::{MemoryDocumentStore, MemoryService};
use memory_support::*;
use serde_json::{Map, Value, json};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn find(list: &Value, matches: impl Fn(&Value) -> bool) -> Value {
    list.as_array().unwrap().iter().find(|op| matches(op)).cloned().unwrap()
}

#[test]
fn acceptance_a18_accepted_stays_distinct_from_stored_through_receipt_failure_and_restart() {
    let hindsight = FakeHindsight::new();
    let store = durable_store();
    hindsight.lock().receipt_state = "pending".into();
    let accepted = open(&hindsight, &store, keep_gate())
        .propose(&p(json!({"text": "fact A", "evidence": [{"ref": "task:1"}]})), &sig())
        .unwrap();
    assert_match(
        &accepted,
        &json!({"state": "accepted", "operationIds": [accepted["id"]]}),
    );
    let memory_id = accepted["memoryId"].as_str().unwrap().to_string();

    // Restart: acceptance is still only acceptance, and no document read is attempted.
    let mut memory = open(&hindsight, &store, keep_gate());
    let got = memory.get(&p(json!({"id": memory_id})), &sig()).unwrap();
    assert_match(&got, &json!({"state": "accepted", "content": null}));
    assert_eq!(hindsight.calls("GET", &format!("/documents/{memory_id}")), 0);

    // The receipt endpoint is down: the read fails, and the claim is neither promoted nor demoted.
    hindsight.lock().fail_next.push("GET /operations/".into());
    assert_eq!(
        memory.get(&p(json!({"id": memory_id})), &sig()).unwrap_err().code,
        "HTTP_ERROR"
    );
    memory = open(&hindsight, &store, keep_gate());
    assert_eq!(
        find(&memory.list().unwrap(), |op| op["id"] == accepted["id"])["state"],
        "accepted"
    );

    // Only a completed receipt establishes retention.
    hindsight
        .lock()
        .operations
        .insert(accepted["id"].as_str().unwrap().into(), "completed".into());
    let got = memory.get(&p(json!({"id": memory_id})), &sig()).unwrap();
    assert_match(&got, &json!({"state": "stored", "content": "fact A"}));
    let reopened = open(&hindsight, &store, keep_gate()).list().unwrap();
    assert_match(
        &find(&reopened, |op| op["id"] == accepted["id"]),
        &json!({"state": "stored", "receipts": [{"id": accepted["id"], "status": "completed"}]}),
    );

    // A failed background ingestion is reported as failed, never as stored.
    hindsight.lock().receipt_state = "failed".into();
    let failed = memory
        .propose(&p(json!({"text": "fact B", "evidence": [{"ref": "task:2"}]})), &sig())
        .unwrap();
    assert_eq!(failed["state"], "accepted");
    let got = open(&hindsight, &store, keep_gate())
        .get(&p(json!({"id": failed["memoryId"]})), &sig())
        .unwrap();
    assert_match(&got, &json!({"state": "failed", "content": null}));

    // A write whose response is lost is unknown: not accepted, not stored, and never replayed.
    hindsight.lock().receipt_state = "completed".into();
    hindsight.lock().fail_next.push("POST /memories".into());
    let error = memory
        .propose(&p(json!({"text": "fact C", "evidence": [{"ref": "task:3"}]})), &sig())
        .unwrap_err();
    assert_eq!(error.code, "HTTP_ERROR");
    let retains_before = hindsight.retains().len();
    memory = open(&hindsight, &store, keep_gate());
    let lost = find(&memory.list().unwrap(), |op| {
        op["kind"] == "propose" && op["state"] == "unknown"
    });
    assert_match(&lost, &json!({"phase": "backend", "error": {"code": "HTTP_ERROR"}}));
    let error = memory
        .correct(
            &p(json!({"id": lost["memoryId"], "text": "fact C'", "evidence": [{"ref": "task:4"}]})),
            &sig(),
        )
        .unwrap_err();
    assert_eq!(error.code, "BUSY");
    assert_eq!(hindsight.retains().len(), retains_before);
}

/// A backend whose `retain` waits for a release before reaching the fake.
struct Held {
    inner: Arc<dyn MemoryBackend>,
    release: Mutex<Receiver<()>>,
}

impl MemoryBackend for Held {
    fn namespace(&self) -> String {
        self.inner.namespace()
    }
    fn scope_tags(&self) -> Map<String, Value> {
        self.inner.scope_tags()
    }
    fn recall(&self, request: &Value, signal: &Signal) -> Result<Value, String> {
        self.inner.recall(request, signal)
    }
    fn retain(&self, request: &Value, signal: &Signal) -> Result<Value, String> {
        let _ = self.release.lock().unwrap().recv();
        self.inner.retain(request, signal)
    }
    fn get(&self, id: &str, signal: &Signal) -> Result<Value, String> {
        self.inner.get(id, signal)
    }
    fn delete(&self, id: &str, signal: &Signal) -> Result<Value, String> {
        self.inner.delete(id, signal)
    }
    fn operation(&self, id: &str, signal: &Signal) -> Result<Value, String> {
        self.inner.operation(id, signal)
    }
}

#[test]
fn acceptance_a18_crash_during_dispatch_reopens_as_unknown_with_no_replay() {
    let hindsight = FakeHindsight::new();
    let store = durable_store();
    let (release, held) = channel();
    let backend = Arc::new(Held {
        inner: hindsight.backend(),
        release: Mutex::new(held),
    });
    let crashed = Arc::new(MemoryService::new(store.clone(), backend, keep_gate()).unwrap());
    let pending = {
        let crashed = crashed.clone();
        std::thread::spawn(move || {
            crashed.propose(
                &p(json!({"text": "in flight", "evidence": [{"ref": "task:1"}]})),
                &Signal::new(),
            )
        })
    };
    // The durable state at the moment the owner died: dispatch has begun.
    let deadline = Instant::now() + Duration::from_secs(120);
    let snapshot = loop {
        let value = stored(&store);
        if value.to_string().contains("\"phase\":\"backend\"") {
            break value;
        }
        assert!(Instant::now() < deadline, "the propose never reached dispatch");
        std::thread::sleep(Duration::from_millis(2));
    };

    let restarted_store = Arc::new(MemoryDocumentStore::new(Some(snapshot)));
    let restarted = open(&hindsight, &restarted_store, keep_gate());
    let op = find(&restarted.list().unwrap(), |op| op["kind"] == "propose");
    assert_match(
        &op,
        &json!({"state": "unknown", "phase": "backend", "error": {"code": "OWNER_ENDED"}}),
    );
    assert_eq!(hindsight.retains().len(), 0);
    release.send(()).unwrap();
    pending.join().unwrap().unwrap();
}

#[test]
fn acceptance_a18_forget_persists_across_restart_and_outranks_a_backend_still_returning_it() {
    let hindsight = FakeHindsight::new();
    let store = durable_store();
    let mut memory = open(&hindsight, &store, keep_gate());
    let kept = memory
        .propose(&p(json!({"text": "keep me", "evidence": [{"ref": "task:1"}]})), &sig())
        .unwrap();
    let doomed = memory
        .propose(
            &p(json!({"text": "secret-ish launcher detail", "evidence": [{"ref": "task:2"}]})),
            &sig(),
        )
        .unwrap();
    let doomed_id = doomed["memoryId"].as_str().unwrap().to_string();
    memory.get(&p(json!({"id": kept["memoryId"]})), &sig()).unwrap();
    memory.get(&p(json!({"id": doomed_id})), &sig()).unwrap();
    let forgotten = memory.forget(&p(json!({"id": doomed_id})), &sig()).unwrap();
    assert_match(
        &forgotten,
        &json!({"kind": "forget", "state": "forgotten", "phase": "backend"}),
    );
    let document_path = format!("/documents/{doomed_id}");
    assert_eq!(hindsight.calls("DELETE", &document_path), 1);

    // The backend misbehaves: its index still returns the deleted unit and the document reappears.
    {
        let mut state = hindsight.lock();
        let metadata = json!({"ultron_operation": doomed["id"], "ultron_evidence_class": "hypothesis"});
        state.stale.push(json!({
            "id": "ghost-unit", "text": "secret-ish launcher detail", "tags": tags("session"), "type": "world",
            "document_id": doomed_id, "metadata": metadata,
        }));
        state.documents.push(Document {
            id: doomed_id.clone(),
            content: "secret-ish launcher detail".into(),
            tags: vec!["ultron:session:s1".into()],
            observation_scopes: vec![vec!["ultron:session:s1".into()]],
            metadata: metadata.as_object().unwrap().clone(),
        });
    }

    // Restart: the local forget record wins everywhere.
    memory = open(&hindsight, &store, keep_gate());
    let gets = hindsight.calls("GET", &document_path);
    assert_eq!(
        memory.get(&p(json!({"id": doomed_id})), &sig()).unwrap_err().code,
        "FORGOTTEN"
    );
    assert_eq!(hindsight.calls("GET", &document_path), gets);
    let error = memory
        .correct(
            &p(json!({"id": doomed_id, "text": "resurrect", "evidence": [{"ref": "task:3"}]})),
            &sig(),
        )
        .unwrap_err();
    assert_eq!(error.code, "FORGOTTEN");
    assert_eq!(
        memory.forget(&p(json!({"id": doomed_id})), &sig()).unwrap_err().code,
        "FORGOTTEN"
    );
    assert_eq!(hindsight.calls("DELETE", &document_path), 1);

    let prepared = memory
        .prepare(&p(json!({"query": "launcher", "taskId": "task-after"})), &sig())
        .unwrap();
    let ids: Vec<Value> = prepared["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["memoryId"].clone())
        .collect();
    assert_eq!(ids, vec![kept["memoryId"].clone()]);
    assert!(!prepared["context"].as_str().unwrap().contains("secret-ish"));
    let why = memory.why(&json!("task-after")).unwrap()[0].clone();
    let excluded = why["excluded"].as_array().unwrap();
    assert_eq!(excluded.len(), 2);
    assert!(excluded[0]["id"].as_str().unwrap().contains(&doomed_id));
    assert_eq!(excluded[0]["memoryId"], doomed_id.as_str());
    assert_eq!(excluded[0]["reason"], "forgotten");
    assert_eq!(
        excluded[1],
        json!({"id": "ghost-unit", "memoryId": doomed_id, "reason": "forgotten"})
    );
    assert!(!stored(&store).to_string().contains("secret-ish"));
}

#[test]
fn acceptance_a18_failed_forget_is_unknown_and_still_withholds_after_restart() {
    let hindsight = FakeHindsight::new();
    let store = durable_store();
    let mut memory = open(&hindsight, &store, keep_gate());
    let item = memory
        .propose(
            &p(json!({"text": "please forget", "evidence": [{"ref": "task:1"}]})),
            &sig(),
        )
        .unwrap();
    let id = item["memoryId"].as_str().unwrap().to_string();
    memory.get(&p(json!({"id": id})), &sig()).unwrap();
    hindsight.lock().fail_next.push("DELETE /documents/".into());
    assert_eq!(
        memory.forget(&p(json!({"id": id})), &sig()).unwrap_err().code,
        "HTTP_ERROR"
    );

    memory = open(&hindsight, &store, keep_gate());
    let forget = find(&memory.list().unwrap(), |op| op["kind"] == "forget");
    assert_match(&forget, &json!({"state": "unknown", "phase": "backend"}));
    // Reporting honesty: the read says unknown with no content.
    let got = memory.get(&p(json!({"id": id})), &sig()).unwrap();
    assert_match(&got, &json!({"state": "unknown", "content": null}));
    let error = memory
        .correct(&p(json!({"id": id, "text": "x", "evidence": [{"ref": "t"}]})), &sig())
        .unwrap_err();
    assert_eq!(error.code, "BUSY");
    // The document still exists in the backend, but the requested forget keeps it out of context.
    assert!(hindsight.has_document(&id));
    let prepared = memory
        .prepare(&p(json!({"query": "anything", "taskId": "task-x"})), &sig())
        .unwrap();
    assert_eq!(prepared["results"], json!([]));
    let excluded = memory.why(&json!("task-x")).unwrap()[0]["excluded"].clone();
    assert_eq!(excluded.as_array().unwrap().len(), 1);
    assert!(excluded[0]["id"].is_string());
    assert_eq!(excluded[0]["memoryId"], id.as_str());
    assert_eq!(excluded[0]["reason"], "forgotten");
    assert_eq!(excluded[0].as_object().unwrap().len(), 3);
}
