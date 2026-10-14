//! A29: the memory inspector shows the evidence that was injected, with no new search and no other task's scope, and
//! refuses requests that would search or change state. Ported from packages/coding-agent/test/
//! ultron-memory-inspection.test.ts: the first case over the Rust `MemoryService` with the same in-memory backend,
//! the second over the session inspector (`session_core::inspect`, TS `createSessionControl().inspect`) with a
//! recording runtime, as the TS case records its `inspect` option. The same inspector answers RPC `inspect` and the
//! TUI's `/memory`, `/agents`, ... commands (crates/daemon worker, `Op::Inspect`).

use kernel::Signal;
use kernel_apis::memory::MemoryBackend;
use kernel_apis::{MemoryDocumentStore, MemoryService};
use serde_json::{Map, Value, json};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Calls {
    recall: usize,
    get: usize,
}

struct Backend(Arc<Mutex<Calls>>);

impl MemoryBackend for Backend {
    fn namespace(&self) -> String {
        "fake://bank".into()
    }
    fn scope_tags(&self) -> Map<String, Value> {
        json!({"session": ["ultron:session:s1"], "project": ["ultron:project:p1"], "global": ["ultron:global:g1"]})
            .as_object()
            .unwrap()
            .clone()
    }
    fn supports(&self, call: &str) -> bool {
        // The TS backend object defines only recall and get.
        matches!(call, "recall" | "get")
    }
    fn recall(&self, request: &Value, _: &Signal) -> Result<Value, String> {
        self.0.lock().unwrap().recall += 1;
        let scope = if request["tags"]
            .as_array()
            .unwrap()
            .contains(&json!("ultron:project:p1"))
        {
            "project"
        } else {
            "session"
        };
        Ok(
            json!({"results": [{"id": format!("{scope}-fact"), "text": format!("{scope} evidence"), "tags": request["tags"]}]}),
        )
    }
    fn retain(&self, _: &Value, _: &Signal) -> Result<Value, String> {
        unreachable!()
    }
    fn get(&self, _: &str, _: &Signal) -> Result<Value, String> {
        self.0.lock().unwrap().get += 1;
        Ok(Value::Null)
    }
    fn delete(&self, _: &str, _: &Signal) -> Result<Value, String> {
        unreachable!()
    }
    fn operation(&self, _: &str, _: &Signal) -> Result<Value, String> {
        unreachable!()
    }
}

#[test]
fn acceptance_a29_why_shows_injected_evidence_without_a_new_search_or_other_scopes_ported_part() {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let memory = MemoryService::new(
        Arc::new(MemoryDocumentStore::default()),
        Arc::new(Backend(calls.clone())),
        Arc::new(|_: &Value, _: &Signal| Ok(json!({"retrieve": true}))),
    )
    .unwrap();
    let payload = |value: Value| value.as_object().unwrap().clone();
    let injected = memory
        .prepare(
            &payload(json!({"query": "launcher path", "taskId": "task-a"})),
            &Signal::new(),
        )
        .unwrap();
    memory
        .prepare(
            &payload(json!({"query": "secret project detail", "taskId": "task-b", "scope": "project"})),
            &Signal::new(),
        )
        .unwrap();
    let searches = calls.lock().unwrap().recall;
    assert_eq!(searches, 2);

    let why = memory.why(&json!("task-a")).unwrap();
    let after = calls.lock().unwrap();
    assert_eq!((after.recall, after.get), (searches, 0));
    let why = why.as_array().unwrap();
    assert_eq!(why.len(), 1);
    let references: Vec<Value> = injected["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| json!({"id": item["id"], "textHash": kernel_apis::util::sha256_hex(item["text"].as_str().unwrap().as_bytes())}))
        .collect();
    assert_eq!(
        references,
        vec![json!({"id": "session-fact", "textHash": kernel_apis::util::sha256_hex(b"session evidence")})]
    );
    for (key, value) in [
        ("taskId", json!("task-a")),
        ("scope", json!("session")),
        ("state", json!("recalled")),
        ("tags", json!(["ultron:session:s1"])),
        ("references", Value::Array(references)),
    ] {
        assert_eq!(why[0][key], value, "{key}");
    }
    // The recorded query is a hash; neither the other task's scope nor its query leaks.
    let text = serde_json::to_string(&why).unwrap();
    assert!(!text.contains("project"));
    assert!(!text.contains("launcher path"));
    assert!(!text.contains("secret project detail"));
}

#[test]
fn acceptance_a29_the_inspector_refuses_requests_that_would_search_or_change_state() {
    let requests = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = requests.clone();
    let runtime = move |kind: &str, _: Map<String, Value>, _: &Signal| -> Result<Value, String> {
        seen.lock().unwrap().push(kind.to_string());
        Ok(json!([]))
    };
    let runtime: &dyn kernel::HostHandler = &runtime;
    let signal = Signal::new();
    for request in [
        "memory.prepare",
        "memory.forget",
        "memory.get",
        "agents.spawn",
        "refinements.activate",
    ] {
        let refused = session_core::inspect::inspect(Some(runtime), request, json!({}), &signal);
        assert_eq!(refused, Err(format!("Not an inspection request: {request}")));
    }
    assert_eq!(
        session_core::inspect::inspect(Some(runtime), "memory.why", json!({"taskId": "task-a"}), &signal),
        Ok(json!([]))
    );
    assert_eq!(*requests.lock().unwrap(), ["memory.why"]);
    // Without a runtime, and with a payload that is not an object (TS: "Inspection payload must be an object").
    assert_eq!(
        session_core::inspect::inspect(None, "memory.why", json!({}), &signal),
        Err("This Session has no Ultron runtime to inspect".into())
    );
    assert_eq!(
        session_core::inspect::inspect(Some(runtime), "memory.list", json!([1]), &signal),
        Err("Inspection payload must be an object".into())
    );
    assert_eq!(*requests.lock().unwrap(), ["memory.why"]);
}
