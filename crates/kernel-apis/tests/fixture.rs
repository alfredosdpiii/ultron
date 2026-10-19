//! Replays crates/kernel-apis/fixtures/<area>.json, recorded by fixtures/generate.mjs from the TypeScript host
//! (NativeRlmHost with the skills / hints modules, or the worker services for memory) serving the real
//! runtime.py through kernel.ts: the same cells run through crates/kernel with `KernelApis` and must print, return
//! and fail the same, and leave the same durable state (module documents, the code-skills directory, the backend
//! calls). Ids, timestamps, durations and temp paths are normalized on both sides by the same rules.

mod common;

use common::{FIXED_NOW, Made, assert_area, fixed, inner, runtime, tree};
use kernel::Signal;
use kernel_apis::memory::MemoryBackend;
use kernel_apis::util::lock;
use kernel_apis::{
    CodeSkills, CodeSkillsOptions, DocumentStore, HintsModule, KernelApis, MemoryApi, MemoryDocumentStore,
    MemoryService, SkillSource, SkillsModule, worker_gate,
};
use serde_json::{Map, Value, json};
use std::path::Path;
use std::sync::atomic::AtomicI64;
use std::sync::{Arc, Mutex};

/// generate.mjs CATALOG.
fn catalog() -> Vec<SkillSource> {
    let source = |name: &str, description: &str, content: &str, disabled: bool| SkillSource {
        name: name.into(),
        description: description.into(),
        file_path: format!("/skills/{name}/SKILL.md"),
        content: content.into(),
        disable_model_invocation: disabled,
    };
    vec![
        source(
            "deploy-notes",
            "Write deployment notes for a release",
            "---\nname: deploy-notes\ndescription: Write deployment notes for a release\nallowed-tools: [bash]\nmodel: big\n---\nCollect the changelog and write release notes for the deploy.\n",
            false,
        ),
        source(
            "slug-helper",
            "Make URL slugs",
            "---\nname: slug-helper\ndescription: Make URL slugs\n---\nUse slugify on titles. Release slugs are lowercase.\n",
            false,
        ),
        source(
            "hidden",
            "Internal only",
            "---\nname: hidden\ndescription: Internal only\ndisable-model-invocation: true\n---\nRelease internals.\n",
            true,
        ),
    ]
}

fn skills_host(dir: &Path) -> Made {
    let skills_dir = dir.join("skills");
    std::fs::create_dir_all(&skills_dir).unwrap();
    let store = Arc::new(MemoryDocumentStore::default());
    let mut options = CodeSkillsOptions::new(&skills_dir, runtime());
    options.test_timeout_ms = 4000;
    let module = SkillsModule::new(
        store.clone(),
        Arc::new(|| Ok(catalog())),
        fixed(FIXED_NOW),
        Some(CodeSkills::new(options)),
    );
    let describe_dir = skills_dir.clone();
    Made {
        handler: KernelApis::new(inner(Arc::new(AtomicI64::new(FIXED_NOW)))).with(Arc::new(module)),
        env: vec![(
            "ULTRON_CODE_SKILLS_DIR".into(),
            skills_dir.to_string_lossy().into_owned(),
        )],
        state: Box::new(move || {
            let describe = CodeSkills::new(CodeSkillsOptions::new(&describe_dir, runtime())).describe();
            json!({"module": store.read().unwrap(), "files": tree(&describe_dir), "describe": describe})
        }),
    }
}

/// generate.mjs `fakeHindsight`: a lagging index (old entries of a document stay recallable, deletes leave them).
#[derive(Default)]
struct FakeHindsight {
    calls: Mutex<Vec<Value>>,
    entries: Mutex<Vec<Value>>,
    documents: Mutex<Vec<(String, Value)>>,
    operations: Mutex<Vec<(String, String)>>,
}

impl MemoryBackend for FakeHindsight {
    fn namespace(&self) -> String {
        "fake://bank".into()
    }

    fn scope_tags(&self) -> Map<String, Value> {
        json!({"session": ["ultron:session:s1"], "project": ["ultron:project:p1"]})
            .as_object()
            .unwrap()
            .clone()
    }

    fn recall(&self, request: &Value, _: &Signal) -> Result<Value, String> {
        lock(&self.calls).push(json!(["recall", request]));
        let tags = request["tags"].as_array().unwrap();
        let query = request["query"].as_str().unwrap().to_lowercase();
        let results: Vec<Value> = lock(&self.entries)
            .iter()
            .filter(|entry| {
                let own = entry["tags"].as_array().unwrap();
                own.len() == tags.len()
                    && own.iter().all(|tag| tags.contains(tag))
                    && (query == "*" || entry["text"].as_str().unwrap().to_lowercase().contains(&query))
            })
            .map(|entry| {
                json!({"id": entry["id"], "text": entry["text"], "tags": entry["tags"], "document_id": entry["document_id"],
                    "type": "world", "metadata": entry["metadata"]})
            })
            .collect();
        Ok(json!({"results": results}))
    }

    fn retain(&self, request: &Value, _: &Signal) -> Result<Value, String> {
        lock(&self.calls).push(json!(["retain", request]));
        let item = &request["items"][0];
        let mut entries = lock(&self.entries);
        let id = format!("entry-{}", entries.len() + 1);
        entries.push(json!({"id": id, "text": item["content"], "tags": item["tags"], "document_id": item["document_id"], "metadata": item["metadata"]}));
        let document_id = item["document_id"].as_str().unwrap().to_string();
        let document = json!({"id": document_id, "original_text": item["content"], "tags": item["tags"]});
        let mut documents = lock(&self.documents);
        match documents.iter_mut().find(|(key, _)| *key == document_id) {
            Some(slot) => slot.1 = document,
            None => documents.push((document_id, document)),
        }
        let content = item["content"].as_str().unwrap();
        let status = if content.contains("pending") {
            "pending"
        } else {
            "completed"
        };
        lock(&self.operations).push((request["operation_id"].as_str().unwrap().into(), status.into()));
        if content.contains("sync") {
            return Ok(json!({"success": true, "async": false}));
        }
        Ok(json!({"success": true, "async": true, "operation_id": request["operation_id"]}))
    }

    fn get(&self, id: &str, _: &Signal) -> Result<Value, String> {
        lock(&self.calls).push(json!(["get", id]));
        Ok(lock(&self.documents)
            .iter()
            .find(|(key, _)| key == id)
            .map(|(_, document)| document.clone())
            .unwrap_or_else(|| json!({"id": id, "original_text": null, "tags": []})))
    }

    fn delete(&self, id: &str, _: &Signal) -> Result<Value, String> {
        lock(&self.calls).push(json!(["delete", id]));
        lock(&self.documents).retain(|(key, _)| key != id);
        Ok(json!({"success": true, "document_id": id}))
    }

    fn operation(&self, id: &str, _: &Signal) -> Result<Value, String> {
        lock(&self.calls).push(json!(["operation", id]));
        let status = lock(&self.operations)
            .iter()
            .rev()
            .find(|(key, _)| key == id)
            .map(|(_, status)| status.clone())
            .unwrap_or_else(|| "not_found".into());
        Ok(json!({"operation_id": id, "status": status}))
    }
}

fn memory_host(_: &Path) -> Made {
    let store = Arc::new(MemoryDocumentStore::default());
    let backend = Arc::new(FakeHindsight::default());
    let service = MemoryService::new(store.clone(), backend.clone(), worker_gate()).unwrap();
    Made {
        handler: KernelApis::new(inner(Arc::new(AtomicI64::new(FIXED_NOW))))
            .with(Arc::new(MemoryApi { service: Some(service) })),
        env: Vec::new(),
        state: Box::new(move || json!({"journal": store.read().unwrap(), "backend": *lock(&backend.calls)})),
    }
}

fn hints_host(_: &Path) -> Made {
    let store = Arc::new(MemoryDocumentStore::default());
    Made {
        handler: KernelApis::new(inner(Arc::new(AtomicI64::new(FIXED_NOW))))
            .with(Arc::new(HintsModule::new(store.clone()))),
        env: Vec::new(),
        state: Box::new(move || store.read().unwrap().unwrap_or(Value::Null)),
    }
}

#[test]
fn kernel_apis_skills_cells_match_typescript_host() {
    assert_area("skills", &skills_host);
}

#[test]
fn kernel_apis_memory_cells_match_typescript_host() {
    assert_area("memory", &memory_host);
}

#[test]
fn kernel_apis_hints_cells_match_typescript_host() {
    assert_area("hints", &hints_host);
}
