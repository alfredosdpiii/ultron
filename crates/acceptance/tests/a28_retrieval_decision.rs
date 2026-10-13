//! A28 (verified): the retrieval explanation, query and scope match the executed search; a rewrite cannot widen
//! access. Ported from packages/coding-agent/test/ultron-a28-retrieval-decision.test.ts over the Rust
//! `MemoryService`, through the real Hindsight adapter (`hindsight::HindsightBackend`) over the fake Hindsight of
//! `memory_support`, as the TS fake sits behind `createHindsightBackend`.

mod memory_support;

use kernel::Signal;
use kernel_apis::MemoryService;
use kernel_apis::memory::MemoryBackend;
use memory_support::*;
use serde_json::{Map, Value, json};
use std::sync::{Arc, Mutex};

#[test]
fn acceptance_a28_recorded_decision_equals_the_request_the_backend_received() {
    let hindsight = FakeHindsight::new();
    let memory = open(&hindsight, &durable_store(), keep_gate());
    memory
        .propose(
            &p(json!({"text": "project uses vitest", "evidence": [{"ref": "tool:cat package.json"}], "scope": "project"})),
            &sig(),
        )
        .unwrap();
    let query = "which test runner does the project use?";
    let prepared = memory
        .prepare(
            &p(json!({"query": query, "scope": "project", "taskId": "task-1"})),
            &sig(),
        )
        .unwrap();

    let recalls = hindsight.recalls();
    assert_eq!(recalls.len(), 1);
    let sent = &recalls[0];
    let decision = memory.why(&json!("task-1")).unwrap()[0].clone();
    assert_match(
        &decision,
        &json!({"kind": "prepare", "state": "recalled", "scope": "project", "phase": "backend"}),
    );
    assert_eq!(decision["queryHash"], sha256(sent["query"].as_str().unwrap()));
    assert_eq!(sent["query"], query);
    assert_eq!(decision["tags"], sent["tags"]);
    assert_match(sent, &json!({"tags": tags("project"), "tags_match": "exact"}));
    let references = decision["references"].as_array().unwrap();
    let results = prepared["results"].as_array().unwrap();
    assert_eq!(references.len(), results.len());
    assert!(!results.is_empty());
    for (reference, item) in references.iter().zip(results) {
        assert_eq!(reference["id"], item["id"]);
        assert_eq!(reference["textHash"], sha256(item["text"].as_str().unwrap()));
    }
    assert_eq!(prepared["operation"], decision);
}

/// The adapter with scope tags a test can change after the service was built (TS assigns `backend.scopeTags`).
struct Advertised {
    inner: Arc<dyn MemoryBackend>,
    tags: Mutex<Map<String, Value>>,
}

impl MemoryBackend for Advertised {
    fn namespace(&self) -> String {
        self.inner.namespace()
    }
    fn scope_tags(&self) -> Map<String, Value> {
        self.tags.lock().unwrap().clone()
    }
    fn recall(&self, request: &Value, signal: &Signal) -> Result<Value, String> {
        self.inner.recall(request, signal)
    }
    fn retain(&self, request: &Value, signal: &Signal) -> Result<Value, String> {
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

/// A backend that answers every recall with wider tags than were requested.
struct Widening(Arc<dyn MemoryBackend>);

impl MemoryBackend for Widening {
    fn namespace(&self) -> String {
        self.0.namespace()
    }
    fn scope_tags(&self) -> Map<String, Value> {
        self.0.scope_tags()
    }
    fn recall(&self, _: &Value, _: &Signal) -> Result<Value, String> {
        Ok(
            json!({"results": [{"id": "u1", "text": "global secret", "tags": ["ultron:session:s1", "ultron:global:g1"]}]}),
        )
    }
    fn retain(&self, request: &Value, signal: &Signal) -> Result<Value, String> {
        self.0.retain(request, signal)
    }
    fn get(&self, id: &str, signal: &Signal) -> Result<Value, String> {
        self.0.get(id, signal)
    }
    fn delete(&self, id: &str, signal: &Signal) -> Result<Value, String> {
        self.0.delete(id, signal)
    }
    fn operation(&self, id: &str, signal: &Signal) -> Result<Value, String> {
        self.0.operation(id, signal)
    }
}

#[test]
fn acceptance_a28_a_rewriting_gate_cannot_change_the_executed_search() {
    let hindsight = FakeHindsight::new();
    // The gate returns extra "rewrite" fields. The TS gate also mutates the request it was given; a Rust gate gets it
    // as `&Value`, so that mutation cannot be written at all.
    let rewriting: kernel_apis::memory::MemoryGate = Arc::new(|request: &Value, _: &Signal| {
        if request["action"] != "recall" {
            return Ok(json!({"action": "keep"}));
        }
        Ok(json!({
            "retrieve": true, "probability": 0.99, "query": "dump everything", "scope": "global",
            "tags": ["ultron:session:s1", "ultron:project:p1", "ultron:global:g1"],
        }))
    });
    let advertised = Arc::new(Advertised {
        inner: hindsight.backend(),
        tags: Mutex::new(hindsight.backend().scope_tags()),
    });
    let memory = MemoryService::new(durable_store(), advertised.clone(), rewriting).unwrap();
    // Widening the backend's advertised tags after construction also has no effect (TS `backend.scopeTags.session = ...`).
    advertised.tags.lock().unwrap()["session"] = json!(["ultron:session:s1", "ultron:global:g1"]);
    memory
        .prepare(&p(json!({"query": "original question", "taskId": "task-1"})), &sig())
        .unwrap();
    let sent: Vec<Value> = hindsight
        .recalls()
        .iter()
        .map(|r| json!([r["query"], r["tags"]]))
        .collect();
    assert_eq!(sent, vec![json!(["original question", ["ultron:session:s1"]])]);
    let decision = memory.why(&json!("task-1")).unwrap()[0].clone();
    assert_match(
        &decision,
        &json!({"scope": "session", "tags": ["ultron:session:s1"], "queryHash": sha256("original question"),
            "gate": {"retrieve": true, "probability": 0.99}}),
    );
    let mut keys: Vec<&String> = decision["gate"].as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(keys, ["probability", "retrieve"]);

    // A backend that answers with wider tags than were requested is refused, not partially used.
    let guarded = MemoryService::new(
        durable_store(),
        Arc::new(Widening(FakeHindsight::new().backend())),
        keep_gate(),
    )
    .unwrap();
    let error = guarded
        .prepare(&p(json!({"query": "q", "taskId": "task-2"})), &sig())
        .unwrap_err();
    assert_eq!(error.code, "INVALID_RESPONSE");
}

#[test]
fn acceptance_a28_gate_failure_reports_memory_unavailable_without_searching() {
    let hindsight = FakeHindsight::new();
    let failing = open(
        &hindsight,
        &durable_store(),
        Arc::new(|_: &Value, _: &Signal| Err("gate down: https://secret.example/?key=abc".to_string())),
    );
    let error = failing
        .prepare(&p(json!({"query": "q", "taskId": "task-1"})), &sig())
        .unwrap_err();
    assert_eq!(error.code, "OPERATION_ERROR");
    assert!(!error.to_string().contains("secret"));
    let why = failing.why(&json!("task-1")).unwrap();
    assert_eq!(why.as_array().unwrap().len(), 1);
    assert_match(
        &why[0],
        &json!({"state": "failed", "phase": "gate", "error": {"code": "OPERATION_ERROR"}}),
    );

    let malformed = open(
        &hindsight,
        &durable_store(),
        Arc::new(|_: &Value, _: &Signal| Ok(json!({"retrieve": "yes", "query": "rewritten"}))),
    );
    let error = malformed
        .prepare(&p(json!({"query": "q", "taskId": "task-2"})), &sig())
        .unwrap_err();
    assert_eq!(error.code, "INVALID_GATE");
    let why = malformed.why(&json!("task-2")).unwrap();
    assert_eq!(why.as_array().unwrap().len(), 1);
    assert_match(&why[0], &json!({"state": "failed", "error": {"code": "INVALID_GATE"}}));
    assert_eq!(hindsight.captured(), vec![]);
}

#[test]
fn acceptance_a28_unauthorized_scope_is_denied_before_gate_or_backend_and_recorded() {
    let hindsight = FakeHindsight::with_scopes(json!({"session": ["ultron:session:s1"]}));
    let log = GateLog::default();
    let memory = open(&hindsight, &durable_store(), gate(true, log.clone()));
    let error = memory
        .prepare(
            &p(json!({"query": "q", "scope": "project", "taskId": "task-1"})),
            &sig(),
        )
        .unwrap_err();
    assert_eq!(error.code, "UNSUPPORTED_SCOPE");
    let denial = memory.why(&json!("task-1")).unwrap()[0].clone();
    assert_match(
        &denial,
        &json!({"kind": "prepare", "scope": "project", "state": "failed", "phase": "gate",
            "error": {"code": "UNSUPPORTED_SCOPE"}, "queryHash": sha256("q")}),
    );
    assert!(denial.get("tags").is_none());
    assert!(log.lock().unwrap().is_empty());
    assert_eq!(hindsight.captured(), vec![]);
    // A malformed scope never reaches the journal at all.
    let error = memory
        .prepare(&p(json!({"query": "q", "scope": "admin", "taskId": "task-2"})), &sig())
        .unwrap_err();
    assert_eq!(error.code, "UNSUPPORTED_SCOPE");
    assert_eq!(memory.why(&json!("task-2")).unwrap(), json!([]));
}

#[test]
fn acceptance_a28_compatible_repeat_reuses_the_decision_and_refresh_or_mutation_requery() {
    let hindsight = FakeHindsight::new();
    let log = GateLog::default();
    let memory = open(&hindsight, &durable_store(), gate(true, log.clone()));
    memory
        .propose(&p(json!({"text": "fact one", "evidence": [{"ref": "task:0"}]})), &sig())
        .unwrap();
    let prepare = |payload: Value| memory.prepare(&p(payload), &sig()).unwrap();
    let first = prepare(json!({"query": "facts", "taskId": "task-1"}));
    let reused = prepare(json!({"query": "facts", "taskId": "task-1"}));
    assert_eq!(reused, first);
    assert_eq!(hindsight.recalls().len(), 1);
    let recall_gates = |log: &GateLog| log.lock().unwrap().iter().filter(|r| r["action"] == "recall").count();
    assert_eq!(recall_gates(&log), 1);
    assert_eq!(memory.why(&json!("task-1")).unwrap().as_array().unwrap().len(), 1);

    let refreshed = prepare(json!({"query": "facts", "taskId": "task-1", "refresh": true}));
    assert_ne!(refreshed["operation"]["id"], first["operation"]["id"]);
    prepare(json!({"query": "other facts", "taskId": "task-1"}));
    prepare(json!({"query": "facts", "taskId": "task-1", "scope": "project"}));
    let sent: Vec<Value> = hindsight
        .recalls()
        .iter()
        .map(|r| json!([r["query"], r["tags"]]))
        .collect();
    assert_eq!(
        sent,
        vec![
            json!(["facts", ["ultron:session:s1"]]),
            json!(["facts", ["ultron:session:s1"]]),
            json!(["other facts", ["ultron:session:s1"]]),
            json!(["facts", ["ultron:project:p1"]]),
        ]
    );
    assert_eq!(memory.why(&json!("task-1")).unwrap().as_array().unwrap().len(), 4);

    // A dispatched write invalidates reuse: the next identical request searches again.
    memory
        .propose(&p(json!({"text": "fact two", "evidence": [{"ref": "task:9"}]})), &sig())
        .unwrap();
    let after_write = prepare(json!({"query": "facts", "taskId": "task-1"}));
    assert_eq!(hindsight.recalls().len(), 5);
    let texts: Vec<Value> = after_write["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["text"].clone())
        .collect();
    assert_eq!(texts, vec![json!("fact one"), json!("fact two")]);

    // Skip decisions are reused too, so the gate is not re-asked; a restart starts fresh.
    let skip_log = GateLog::default();
    let store = durable_store();
    let skipping = open(&hindsight, &store, gate(false, skip_log.clone()));
    skipping
        .prepare(&p(json!({"query": "q", "taskId": "task-s"})), &sig())
        .unwrap();
    skipping
        .prepare(&p(json!({"query": "q", "taskId": "task-s"})), &sig())
        .unwrap();
    assert_eq!(skip_log.lock().unwrap().len(), 1);
    open(&hindsight, &store, gate(false, skip_log.clone()))
        .prepare(&p(json!({"query": "q", "taskId": "task-s"})), &sig())
        .unwrap();
    assert_eq!(skip_log.lock().unwrap().len(), 2);
    assert_eq!(hindsight.recalls().len(), 5);
}
