//! Shared by the memory rows (A16-A18, A28-A30): the Rust port of `test/ultron-fake-hindsight.ts`.
//!
//! As in TypeScript, every request goes through the real Hindsight adapter (`hindsight::HindsightBackend`, the port of
//! `createHindsightBackend`) and the fake is its HTTP transport (the captured `fetch`): it sees the exact wire
//! method, path (bank prefix stripped) and JSON body, and answers with the same statuses (503 for an injected
//! failure, 404 for an unknown document or endpoint) that the adapter maps to its error codes.
#![allow(dead_code)]

use hindsight::{HindsightBackend, HindsightOptions, HttpRequest, HttpResponse, Transport, TransportError};
use kernel::Signal;
use kernel_apis::memory::{MemoryBackend, MemoryGate};
use kernel_apis::{DocumentStore, MemoryDocumentStore, MemoryService};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

pub fn scopes() -> Value {
    json!({"session": ["ultron:session:s1"], "project": ["ultron:project:p1"], "global": ["ultron:global:g1"]})
}

pub fn tags(scope: &str) -> Value {
    scopes()[scope].clone()
}

#[derive(Clone, Debug, PartialEq)]
pub struct Captured {
    pub method: String,
    pub path: String,
    pub body: Option<Value>,
}

#[derive(Clone, Debug)]
pub struct Document {
    pub id: String,
    pub content: String,
    pub tags: Vec<String>,
    pub observation_scopes: Vec<Vec<String>>,
    pub metadata: Map<String, Value>,
}

#[derive(Default)]
pub struct State {
    pub captured: Vec<Captured>,
    /// Insertion-ordered, like a JS Map.
    pub documents: Vec<Document>,
    /// Units that survive deletion: a stale index that still returns forgotten or replaced text.
    pub stale: Vec<Value>,
    pub observations: Vec<Value>,
    pub operations: HashMap<String, String>,
    pub receipt_state: String,
    pub fail_next: Vec<String>,
    pub merge_across_scopes: bool,
    pub leak_observations: bool,
    counter: u32,
}

/// In-process Hindsight 0.9.2 fake: observations form per explicit observation scope with exactly its tags.
#[derive(Clone)]
pub struct FakeHindsight {
    pub state: Arc<Mutex<State>>,
    pub scope_tags: Arc<Mutex<Value>>,
}

fn same_set(left: &Value, right: &Value) -> bool {
    let (Some(left), Some(right)) = (left.as_array(), right.as_array()) else {
        return false;
    };
    left.len() == right.len() && left.iter().all(|tag| right.contains(tag))
}

impl FakeHindsight {
    pub fn new() -> Self {
        Self::with_scopes(scopes())
    }

    pub fn with_scopes(scope_tags: Value) -> Self {
        let state = State {
            receipt_state: "completed".into(),
            ..State::default()
        };
        FakeHindsight {
            state: Arc::new(Mutex::new(state)),
            scope_tags: Arc::new(Mutex::new(scope_tags)),
        }
    }

    pub fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    pub fn captured(&self) -> Vec<Captured> {
        self.lock().captured.clone()
    }

    pub fn calls(&self, method: &str, suffix: &str) -> usize {
        self.captured()
            .iter()
            .filter(|c| c.method == method && c.path.ends_with(suffix))
            .count()
    }

    pub fn recalls(&self) -> Vec<Value> {
        self.captured()
            .into_iter()
            .filter(|c| c.path.ends_with("/memories/recall"))
            .map(|c| c.body.unwrap())
            .collect()
    }

    pub fn retains(&self) -> Vec<Value> {
        self.captured()
            .into_iter()
            .filter(|c| c.method == "POST" && c.path.ends_with("/memories"))
            .map(|c| c.body.unwrap())
            .collect()
    }

    pub fn has_document(&self, id: &str) -> bool {
        self.lock().documents.iter().any(|d| d.id == id)
    }

    /// Hindsight's background consolidation, run on demand.
    pub fn consolidate(&self) {
        let mut state = self.lock();
        state.observations.clear();
        let mut groups: BTreeMap<String, (Vec<String>, Vec<String>)> = BTreeMap::new();
        let mut order = Vec::new();
        let all: Vec<String> = {
            let mut all = Vec::new();
            for document in &state.documents {
                for tag in &document.tags {
                    if !all.contains(tag) {
                        all.push(tag.clone());
                    }
                }
            }
            all
        };
        for document in &state.documents {
            let tags = if state.merge_across_scopes {
                all.clone()
            } else {
                document.observation_scopes[0].clone()
            };
            let mut sorted = tags.clone();
            sorted.sort();
            let key = sorted.join("\u{0}");
            if !groups.contains_key(&key) {
                order.push(key.clone());
            }
            groups
                .entry(key)
                .or_insert_with(|| (tags, Vec::new()))
                .1
                .push(document.content.clone());
        }
        for key in order {
            let (tags, texts) = groups.remove(&key).unwrap();
            state.counter += 1;
            let id = format!("observation-{}", state.counter);
            state.observations.push(json!({
                "id": id, "text": format!("Consolidated: {}", texts.join(" | ")), "tags": tags,
                "type": "observation", "document_id": null, "metadata": null,
            }));
        }
    }

    fn units(state: &State) -> Vec<Value> {
        let mut units: Vec<Value> = state
            .documents
            .iter()
            .map(|d| {
                json!({
                    "id": format!("unit-{}-{}", d.id, d.metadata.get("ultron_operation").and_then(Value::as_str).unwrap_or("x")),
                    "text": d.content, "tags": d.tags, "type": "world", "document_id": d.id, "metadata": d.metadata,
                })
            })
            .collect();
        units.extend(state.stale.iter().cloned());
        units.extend(state.observations.iter().cloned());
        units
    }

    /// The fake server's handling of one adapter request.
    fn fetch(&self, method: &str, path: &str, body: Option<&Value>) -> (u16, Value) {
        let mut state = self.lock();
        state.captured.push(Captured {
            method: method.into(),
            path: path.into(),
            body: body.cloned(),
        });
        let key = format!("{method} {path}");
        if let Some(index) = state.fail_next.iter().position(|f| key.starts_with(f.as_str())) {
            state.fail_next.remove(index);
            return (503, json!({"detail": "unavailable"}));
        }
        if method == "POST" && path == "/memories/recall" {
            let wanted = &body.unwrap()["tags"];
            let leak = state.leak_observations;
            let results: Vec<Value> = Self::units(&state)
                .into_iter()
                .filter(|u| same_set(&u["tags"], wanted) || (leak && u["type"] == "observation"))
                .collect();
            return (200, json!({"results": results}));
        }
        if method == "POST" && path == "/memories" {
            let body = body.unwrap();
            let item = &body["items"][0];
            let id = item["document_id"].as_str().unwrap().to_string();
            assert_eq!(item["update_mode"], "replace", "fake supports replace only");
            let strings = |v: &Value| -> Vec<String> {
                v.as_array()
                    .unwrap()
                    .iter()
                    .map(|t| t.as_str().unwrap().to_string())
                    .collect()
            };
            let document = Document {
                id: id.clone(),
                content: item["content"].as_str().unwrap().into(),
                tags: strings(&item["tags"]),
                observation_scopes: item["observation_scopes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(strings)
                    .collect(),
                metadata: item["metadata"].as_object().cloned().unwrap_or_default(),
            };
            match state.documents.iter_mut().find(|d| d.id == id) {
                Some(existing) => *existing = document,
                None => state.documents.push(document),
            }
            let operation_id = body["operation_id"].as_str().unwrap().to_string();
            let receipt = state.receipt_state.clone();
            state.operations.insert(operation_id.clone(), receipt);
            return (
                200,
                json!({"success": true, "async": true, "operation_id": operation_id}),
            );
        }
        if let Some(id) = path.strip_prefix("/operations/")
            && method == "GET"
        {
            let status = state.operations.get(id).cloned().unwrap_or_else(|| "not_found".into());
            return (200, json!({"operation_id": id, "status": status}));
        }
        if let Some(id) = path.strip_prefix("/documents/") {
            let found = state.documents.iter().position(|d| d.id == id);
            if method == "GET" {
                return match found {
                    Some(index) => {
                        let d = &state.documents[index];
                        (200, json!({"id": id, "original_text": d.content, "tags": d.tags}))
                    }
                    None => (404, json!({"detail": "not found"})),
                };
            }
            if method == "DELETE" {
                if let Some(index) = found {
                    state.documents.remove(index);
                }
                return (
                    200,
                    json!({"success": true, "document_id": id, "memory_units_deleted": usize::from(found.is_some())}),
                );
            }
        }
        (404, json!({"detail": "unsupported"}))
    }

    /// `hindsight.backend(scopeTags, bankId)`: the real adapter over this fake.
    pub fn backend_for(&self, scope_tags: Value, bank_id: &str) -> Arc<dyn MemoryBackend> {
        let mut options = HindsightOptions::new(
            "http://hindsight.test",
            bank_id,
            scope_tags.as_object().unwrap().clone(),
        );
        options.transport = Some(Arc::new(self.clone()));
        Arc::new(HindsightBackend::new(options).unwrap())
    }

    /// The adapter with the fake's scope tags and bank `bank-1`.
    pub fn backend(&self) -> Arc<dyn MemoryBackend> {
        let tags = self.scope_tags.lock().unwrap().clone();
        self.backend_for(tags, "bank-1")
    }
}

/// `decodeURIComponent` of a path segment.
fn decode(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            out.push(u8::from_str_radix(&segment[i + 1..i + 3], 16).unwrap());
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

impl Transport for FakeHindsight {
    fn send(
        &self,
        request: HttpRequest,
        _: usize,
        _: std::time::Duration,
        _: &Signal,
    ) -> Result<HttpResponse, TransportError> {
        let path = request.url.strip_prefix("http://hindsight.test").unwrap();
        // `url.pathname.replace(/^\/v1\/default\/banks\/[^/]+/, "")`
        let path = match path.strip_prefix("/v1/default/banks/") {
            Some(rest) => rest.find('/').map_or("", |at| &rest[at..]),
            None => path,
        };
        let path = match path.split_once('/') {
            Some((_, rest)) if path.starts_with("/documents/") || path.starts_with("/operations/") => {
                let (kind, id) = rest.split_once('/').unwrap();
                format!("/{kind}/{}", decode(id))
            }
            _ => path.to_string(),
        };
        let body: Option<Value> = request.body.as_ref().map(|b| serde_json::from_slice(b).unwrap());
        let (status, value) = self.fetch(&request.method, &path, body.as_ref());
        Ok(HttpResponse {
            status,
            body: if (200..300).contains(&status) {
                value.to_string().into_bytes()
            } else {
                Vec::new()
            },
        })
    }
}

pub type GateLog = Arc<Mutex<Vec<Value>>>;

/// `gate(recall, log)`: records each request; recalls run when `recall`, writes are kept.
pub fn gate(recall: bool, log: GateLog) -> MemoryGate {
    Arc::new(move |request: &Value, _: &Signal| {
        log.lock().unwrap().push(request.clone());
        Ok(if request["action"] == "recall" {
            json!({"retrieve": recall, "probability": if recall { 0.9 } else { 0.1 }})
        } else {
            json!({"action": "keep"})
        })
    })
}

pub fn keep_gate() -> MemoryGate {
    gate(true, GateLog::default())
}

/// A durable journal shared by every service instance that reopens it.
pub fn durable_store() -> Arc<MemoryDocumentStore> {
    Arc::new(MemoryDocumentStore::default())
}

pub fn stored(store: &MemoryDocumentStore) -> Value {
    store.read().unwrap().unwrap_or(Value::Null)
}

pub fn open(hindsight: &FakeHindsight, store: &Arc<MemoryDocumentStore>, memory_gate: MemoryGate) -> MemoryService {
    MemoryService::new(store.clone(), hindsight.backend(), memory_gate).unwrap()
}

/// A payload map from a JSON object literal.
pub fn p(value: Value) -> Map<String, Value> {
    value.as_object().unwrap().clone()
}

pub fn sig() -> Signal {
    Signal::new()
}

pub fn sha256(text: &str) -> String {
    kernel_apis::util::sha256_hex(text.as_bytes())
}

/// Asserts every field of `want` equals the field of `got` (vitest `toMatchObject`, one level of objects deep
/// recursion included).
pub fn assert_match(got: &Value, want: &Value) {
    match (got, want) {
        (Value::Object(g), Value::Object(w)) => {
            for (key, value) in w {
                assert_match(g.get(key).unwrap_or(&Value::Null), value);
                if value.is_null() {
                    continue;
                }
                assert!(g.contains_key(key), "missing {key} in {got}");
            }
        }
        (Value::Array(g), Value::Array(w)) => {
            assert_eq!(g.len(), w.len(), "{got} vs {want}");
            for (g, w) in g.iter().zip(w) {
                assert_match(g, w);
            }
        }
        _ => assert_eq!(got, want),
    }
}
