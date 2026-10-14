//! Replays fixtures/skills-invoke.json, recorded by fixtures/generate-skills-invoke.mjs: `skills.invoke` of the real
//! `SkillsModule` over the TS test catalog and a task host that records every spawn (the TS `fakeHost`), one
//! admission refused. The replies, the spawned requests (definition, the exact `rlm-child@1` prompt, the idempotency
//! key, the parent) and the journaled invocations must equal the TypeScript module's.

use fancy_regex::Regex;
use kernel::Signal;
use kernel_apis::skills::{SkillSource, SkillsModule};
use kernel_apis::{DocumentStore, MemoryDocumentStore, TaskHost};
use serde_json::{Map, Value, json};
use std::path::Path;
use std::sync::{Arc, Mutex};

fn fixture() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/skills-invoke.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// Random ids numbered by first appearance, as the generator writes them.
fn number_ids(value: &Value) -> Value {
    let text = serde_json::to_string(value).unwrap();
    let pattern = Regex::new(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}").unwrap();
    let mut seen: Vec<String> = Vec::new();
    let mut out = String::new();
    let mut last = 0;
    for found in pattern.find_iter(&text) {
        let found = found.unwrap();
        out.push_str(&text[last..found.start()]);
        let id = found.as_str().to_string();
        let index = match seen.iter().position(|s| *s == id) {
            Some(index) => index,
            None => {
                seen.push(id);
                seen.len() - 1
            }
        };
        out.push_str(&format!("uuid-{}", index + 1));
        last = found.end();
    }
    out.push_str(&text[last..]);
    serde_json::from_str(&out).unwrap()
}

const REVIEW: &str = "---\nname: review\ndescription: Review a diff for correctness bugs\n---\nRead the diff, then report bugs with file and line.";
const DEPLOY: &str = "---\nname: deploy\ndescription: Ship a release\nallowed-tools: [bash, write]\ntools: [\"*\"]\ngrants: { network: true }\npermissions: admin\nmodel: anthropic/claude-opus\n---\nRun the release script. Review the changelog first.";
const PRIVATE: &str = "---\nname: private\ndescription: Rotate review credentials\ndisable-model-invocation: true\n---\nOnly a user may run this.";
const NESTED: &str = "---\nname: nested\ndescription: Nested\n---\n  Body with \"quotes\" & <tags>.\n\n";

/// The TS `fakeHost`: spawns are recorded and answered `task-N`; `fail` refuses the next admission.
#[derive(Default)]
struct Recorder {
    spawned: Mutex<Vec<Value>>,
    fail: Mutex<bool>,
}

impl TaskHost for Recorder {
    fn caller_task_id(&self, lane: &str) -> Option<String> {
        (lane == "ultron.rlm-child.parent-task").then(|| "parent-task".into())
    }
    fn task_lane(&self, _: &str) -> Option<String> {
        None
    }
    fn strategy(&self, _: &str) -> Result<String, String> {
        Ok("rlm".into())
    }
    fn tasks(&self) -> Result<Vec<Value>, String> {
        Ok(Vec::new())
    }
    fn spawn(&self, request: &Value, parent: Option<&str>, _: &Signal) -> Result<Value, String> {
        let mut spawned = self.spawned.lock().unwrap();
        spawned.push(json!({"request": request, "parent": parent}));
        if std::mem::take(&mut *self.fail.lock().unwrap()) {
            return Err("admission refused: budget exhausted".into());
        }
        Ok(json!({"id": format!("task-{}", spawned.len()), "definition": request["definition"], "state": "queued"}))
    }
    fn result(&self, _: &str) -> Result<Value, String> {
        Err("unused".into())
    }
    fn cancel(&self, _: &str, _: &str) -> Result<Value, String> {
        Err("unused".into())
    }
    fn steer(&self, _: &str, _: &str, _: &Signal) -> Result<bool, String> {
        Ok(false)
    }
    fn now(&self) -> i64 {
        0
    }
}

fn source(name: &str, description: &str, file_path: &str, content: &str, disabled: bool) -> SkillSource {
    SkillSource {
        name: name.into(),
        description: description.into(),
        file_path: file_path.into(),
        content: content.into(),
        disable_model_invocation: disabled,
    }
}

#[test]
fn kernel_apis_skills_invoke_spawns_the_pinned_skill_as_the_typescript_module_does() {
    let fixture = fixture();
    let store: Arc<MemoryDocumentStore> = Arc::default();
    let catalog = vec![
        source(
            "review",
            "Review a diff for correctness bugs",
            "/skills/review/SKILL.md",
            REVIEW,
            false,
        ),
        source("deploy", "Ship a release", "/skills/deploy/SKILL.md", DEPLOY, false),
        source(
            "private",
            "Rotate review credentials",
            "/skills/private/SKILL.md",
            PRIVATE,
            true,
        ),
        source("nested", "Nested", "relative.md", NESTED, false),
    ];
    let module = SkillsModule::new(
        store.clone(),
        Arc::new(move || Ok(catalog.clone())),
        Arc::new(|| 1000),
        None,
    );
    let host = Recorder::default();
    let mut results = Vec::new();
    for call in fixture["calls"].as_array().unwrap() {
        let lane = call[0].as_str().unwrap();
        let payload: Map<String, Value> = call[1].as_object().unwrap().clone();
        *host.fail.lock().unwrap() = call.get(2) == Some(&json!("fail"));
        let caller = host.caller_task_id(lane);
        results.push(
            match module.handle_with_host("skills.invoke", &payload, caller.as_deref(), &host, &Signal::new()) {
                Ok(value) => json!({"ok": value}),
                Err(error) => json!({"error": error}),
            },
        );
    }
    let document = store.read().unwrap().unwrap();
    let actual = number_ids(&json!({
        "results": results,
        "spawned": *host.spawned.lock().unwrap(),
        "invocations": document["invocations"],
    }));
    assert_eq!(actual["results"], fixture["results"]);
    assert_eq!(actual["spawned"], fixture["spawned"]);
    assert_eq!(actual["invocations"], fixture["invocations"]);
}
