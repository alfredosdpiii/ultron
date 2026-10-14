//! A31: skill selection is version-pinned, explainable, and cannot grant capabilities.
//! Ported from packages/coding-agent/test/ultron-skills.test.ts with literal expectations: the real
//! `kernel_apis::SkillsModule` over the same three-skill catalog, a changeable loader and a fixed clock, called with
//! the caller identity the TS `fakeHost` derives from the lane (`ultron.rlm-child.parent-task` → `parent-task`), its
//! spawns recorded as the TS `fakeHost` records them. "invoke through the real host" runs over the Rust typed-agent
//! host (`agents_support`, `delegation::agents::Agents`) as the TS case runs over `NativeRlmHost`: the module is
//! served for a background job's lane through `KernelApis` with the host as its `TaskHost`.
//! The exact spawned request (the `rlm-child@1` prompt, key and parent) is replayed against the TypeScript module in
//! crates/kernel-apis/tests/skills_invoke.rs.

mod agents_support;

use kernel::{HostHandler, Signal};
use kernel_apis::skills::{SkillSource, SkillsModule};
use kernel_apis::{ApiModule, DocumentStore, KernelApis, MemoryDocumentStore, TaskHost};
use serde_json::{Map, Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// `createHash("sha256").update(text).digest("hex")` (the workspace's sha2, as the module's own versions use).
fn sha(text: &str) -> String {
    kernel_apis::util::sha256_hex(text.as_bytes())
}

const REVIEW: &str = "---\nname: review\ndescription: Review a diff for correctness bugs\n---\nRead the diff, then report bugs with file and line.";
const DEPLOY: &str = "---\nname: deploy\ndescription: Ship a release\nallowed-tools: [bash, write]\ntools: [\"*\"]\ngrants: { network: true }\npermissions: admin\nmodel: anthropic/claude-opus\n---\nRun the release script. Review the changelog first.";
const PRIVATE: &str = "---\nname: private\ndescription: Rotate review credentials\ndisable-model-invocation: true\n---\nOnly a user may run this.";

fn source(name: &str, description: &str, content: &str, disabled: bool) -> SkillSource {
    SkillSource {
        name: name.into(),
        description: description.into(),
        file_path: format!("/skills/{name}/SKILL.md"),
        content: content.into(),
        disable_model_invocation: disabled,
    }
}

/// The TS `catalog()`: an insertion-ordered map of skills the loader copies on every load.
#[derive(Clone)]
struct Catalog(Arc<Mutex<Vec<SkillSource>>>);

impl Catalog {
    fn new() -> Catalog {
        Catalog(Arc::new(Mutex::new(vec![
            source("review", "Review a diff for correctness bugs", REVIEW, false),
            source("deploy", "Ship a release", DEPLOY, false),
            source("private", "Rotate review credentials", PRIVATE, true),
        ])))
    }
    fn set(&self, skill: SkillSource) {
        let mut skills = self.0.lock().unwrap();
        match skills.iter_mut().find(|s| s.name == skill.name) {
            Some(slot) => *slot = skill,
            None => skills.push(skill),
        }
    }
    fn delete(&self, name: &str) {
        self.0.lock().unwrap().retain(|s| s.name != name);
    }
}

/// The TS `fakeHost`: identity from the lane, every spawn recorded and answered `task-N`.
#[derive(Default)]
struct FakeHost {
    spawned: Mutex<Vec<(Value, Option<String>)>>,
}

impl TaskHost for FakeHost {
    fn caller_task_id(&self, lane: &str) -> Option<String> {
        (lane == PARENT_LANE).then(|| "parent-task".into())
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
        spawned.push((request.clone(), parent.map(String::from)));
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

struct Setup {
    skills: Catalog,
    module: SkillsModule,
    store: Arc<MemoryDocumentStore>,
    host: FakeHost,
}

fn setup(store: Option<Arc<MemoryDocumentStore>>) -> Setup {
    let store = store.unwrap_or_default();
    let skills = Catalog::new();
    let loader = skills.clone();
    let module = SkillsModule::new(
        store.clone(),
        Arc::new(move || Ok(loader.0.lock().unwrap().clone())),
        Arc::new(|| 1000),
        None,
    );
    Setup {
        skills,
        module,
        store,
        host: FakeHost::default(),
    }
}

const PARENT_LANE: &str = "ultron.rlm-child.parent-task";

impl Setup {
    fn call_as(&self, kind: &str, payload: Value, lane: &str) -> Result<Value, String> {
        let caller = self.host.caller_task_id(lane);
        let payload: Map<String, Value> = payload.as_object().cloned().unwrap_or_default();
        self.module
            .handle_with_host(kind, &payload, caller.as_deref(), &self.host, &Signal::new())
    }
    fn call(&self, kind: &str, payload: Value) -> Result<Value, String> {
        self.call_as(kind, payload, "main")
    }
    fn ok(&self, kind: &str, payload: Value) -> Value {
        self.call(kind, payload).unwrap()
    }
}

fn rejects(result: Result<Value, String>, text: &str) {
    match result {
        Ok(value) => panic!("expected an error containing {text:?}, got {value}"),
        Err(error) => assert!(error.contains(text), "{error:?} does not contain {text:?}"),
    }
}

#[test]
fn acceptance_a31_lists_skills_with_content_hash_versions_and_invocability() {
    let s = setup(None);
    assert_eq!(
        s.ok("skills.list", json!({}))["skills"],
        json!([
            {"name": "review", "description": "Review a diff for correctness bugs", "version": sha(REVIEW), "invocable": true},
            {"name": "deploy", "description": "Ship a release", "version": sha(DEPLOY), "invocable": true},
            {"name": "private", "description": "Rotate review credentials", "version": sha(PRIVATE), "invocable": false},
        ])
    );
}

#[test]
fn acceptance_a31_select_explains_each_candidate_ranks_deterministically_and_records_the_decision() {
    let s = setup(None);
    let first = s.ok("skills.select", json!({"query": "review the diff"}));
    let second = s.ok("skills.select", json!({"query": "review the diff"}));
    assert_eq!(first["candidates"], second["candidates"]);
    assert_ne!(first["id"], second["id"]);
    let names: Vec<_> = first["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].clone())
        .collect();
    assert_eq!(names, vec![json!("review"), json!("deploy")]);
    assert_eq!(
        first["candidates"][0],
        json!({"name": "review", "version": sha(REVIEW), "score": 3 + 2 + 2 + 1, "matches": [
            {"term": "review", "fields": ["name", "description"]},
            {"term": "diff", "fields": ["description", "body"]},
        ]})
    );
    assert_eq!(
        first["candidates"][1]["matches"],
        json!([{"term": "review", "fields": ["body"]}])
    );
    assert_eq!(
        first["considered"],
        json!({"deploy": sha(DEPLOY), "private": sha(PRIVATE), "review": sha(REVIEW)})
    );
    // Non-invocable skills are considered but never selected, even when they match.
    assert_eq!(
        first["excluded"],
        json!([{"name": "private", "version": sha(PRIVATE), "reason": "not_invocable"}])
    );
    assert_eq!(first["caller_task_id"], Value::Null);
    assert_eq!(
        first["chosen"],
        json!([{"name": "review", "version": sha(REVIEW)}, {"name": "deploy", "version": sha(DEPLOY)}])
    );
    let limited = s.ok("skills.select", json!({"query": "review", "limit": 1}));
    assert_eq!(limited["candidates"].as_array().unwrap().len(), 1);
    let document = s.store.read().unwrap().unwrap();
    let ids: Vec<_> = document["decisions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["id"].clone())
        .collect();
    assert_eq!(
        ids,
        vec![first["id"].clone(), second["id"].clone(), limited["id"].clone()]
    );
}

#[test]
fn acceptance_a31_why_returns_the_original_decision_after_the_catalog_changes() {
    let s = setup(None);
    let decision = s
        .call_as("skills.select", json!({"query": "release"}), PARENT_LANE)
        .unwrap();
    assert_eq!(decision["caller_task_id"], "parent-task");
    let nothing = "---\nname: deploy\n---\nDo nothing.";
    s.skills.set(source("deploy", "Ship nothing", nothing, false));
    let refreshed = s.ok("skills.refresh", json!({}));
    assert_eq!(
        refreshed["changes"],
        json!([{"at": 1000, "generation": 2, "name": "deploy", "from": sha(DEPLOY), "to": sha(nothing)}])
    );
    assert_eq!(s.ok("skills.why", json!({"decision_id": decision["id"]})), decision);
    assert_eq!(
        s.ok("skills.select", json!({"query": "release"}))["candidates"],
        json!([])
    );
    rejects(
        s.call("skills.why", json!({"decision_id": "missing"})),
        "Unknown skill decision",
    );
}

#[test]
fn acceptance_a31_version_pinned_load_succeeds_and_a_stale_version_is_rejected() {
    let s = setup(None);
    let pinned = s.ok("skills.load", json!({"name": "review", "version": sha(REVIEW)}));
    assert_eq!(
        (
            &pinned["name"],
            &pinned["version"],
            &pinned["content"],
            &pinned["context_chars"]
        ),
        (
            &json!("review"),
            &json!(sha(REVIEW)),
            &json!(REVIEW),
            &json!(REVIEW.len())
        )
    );
    assert_eq!(pinned["ignored_capability_requests"], json!({}));
    let changed = format!("{REVIEW}\nAlso check tests.");
    s.skills
        .set(source("review", "Review a diff for correctness bugs", &changed, false));
    s.ok("skills.refresh", json!({}));
    let stale = s
        .call("skills.load", json!({"name": "review", "version": sha(REVIEW)}))
        .unwrap_err();
    let prefix = format!(
        "Stale skill version: review@{} was requested but the catalog has review@",
        sha(REVIEW)
    );
    assert!(stale.starts_with(&prefix), "{stale}");
    rejects(
        s.call(
            "skills.invoke",
            json!({"name": "review", "version": sha(REVIEW), "input": {}}),
        ),
        "Stale skill version",
    );
    assert_eq!(
        s.ok("skills.load", json!({"name": "review"}))["content"],
        changed.as_str()
    );
    s.skills.delete("review");
    s.ok("skills.refresh", json!({}));
    rejects(
        s.call("skills.load", json!({"name": "review", "version": sha(&changed)})),
        "skill removed",
    );
    let document = s.store.read().unwrap().unwrap();
    let loads: Vec<_> = document["loads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| json!({"name": l["name"], "version": l["version"], "caller_task_id": l["caller_task_id"]}))
        .collect();
    assert_eq!(
        loads,
        vec![
            json!({"name": "review", "version": sha(REVIEW), "caller_task_id": null}),
            json!({"name": "review", "version": sha(&changed), "caller_task_id": null}),
        ]
    );
}

#[test]
fn acceptance_a31_capability_granting_frontmatter_is_reported_and_never_reaches_the_spawned_child() {
    let s = setup(None);
    let loaded = s.ok("skills.load", json!({"name": "deploy"}));
    assert_eq!(
        loaded["ignored_capability_requests"],
        json!({"allowed-tools": ["bash", "write"], "tools": ["*"], "grants": {"network": true},
            "permissions": "admin", "model": "anthropic/claude-opus"})
    );
    let invoked = s
        .call_as(
            "skills.invoke",
            json!({"name": "deploy", "version": sha(DEPLOY), "input": {"tag": "v1"}}),
            PARENT_LANE,
        )
        .unwrap();
    assert_eq!(
        (&invoked["task_id"], &invoked["name"], &invoked["version"]),
        (&json!("task-1"), &json!("deploy"), &json!(sha(DEPLOY)))
    );
    let spawned = s.host.spawned.lock().unwrap().clone();
    assert_eq!(spawned.len(), 1);
    let (request, parent) = &spawned[0];
    assert_eq!(parent.as_deref(), Some("parent-task"));
    let mut keys: Vec<&String> = request.as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(keys, ["definition", "input", "key"]);
    assert_eq!(request["definition"], "rlm-child@1");
    let input_keys: Vec<&String> = request["input"].as_object().unwrap().keys().collect();
    assert_eq!(input_keys, ["prompt"]);
    let prompt = request["input"]["prompt"].as_str().unwrap();
    assert!(prompt.contains("Run the release script. Review the changelog first."));
    assert!(prompt.contains(&format!("Skill version: {}", sha(DEPLOY))));
    assert!(prompt.contains(r#"{"tag":"v1"}"#));
    assert!(!prompt.contains("allowed-tools"));
}

#[test]
fn acceptance_a31_invoke_through_the_real_host_keeps_the_childs_model_and_tools_and_links_the_parent() {
    // Every lane prompt waits until its task is cancelled (the TS lane's prompt never resolves).
    let fixture = agents_support::fixture(agents_support::Options {
        script: Some(Arc::new(|_, _, signal, _| agents_support::aborted(signal))),
        ..Default::default()
    });
    let catalog = Catalog::new();
    let skills: Arc<dyn ApiModule> = Arc::new(SkillsModule::new(
        Arc::new(MemoryDocumentStore::default()),
        Arc::new(move || Ok(catalog.0.lock().unwrap().clone())),
        Arc::new(|| 1000),
        None,
    ));
    let parent = fixture.call("background.start", json!({"prompt": "wait"})).unwrap();
    let parent = parent["id"].as_str().unwrap().to_string();
    let child_lane = format!("ultron.background-job.{parent}");
    agents_support::wait_for("the background job's lane", Duration::from_secs(120), || {
        fixture.agents.caller_task(&child_lane).is_some()
    });
    // The module served for the background job's lane, as its kernel's requests are.
    let inner: Arc<dyn HostHandler> = Arc::new(fixture.agents.handler(&child_lane, Arc::new(Unused)));
    let apis = KernelApis::new(inner)
        .with_host(Arc::new(fixture.agents.clone()), child_lane.clone())
        .with(skills);
    let payload = json!({"name": "deploy", "version": sha(DEPLOY), "input": {}});
    let invoked = apis
        .handle("skills.invoke", payload.as_object().unwrap().clone(), &Signal::new())
        .unwrap();
    let task_id = invoked["task_id"].as_str().unwrap().to_string();
    agents_support::wait_for("the skill's lane prompt", Duration::from_secs(120), || {
        agents_support::lock(&fixture.prompts)
            .iter()
            .any(|(_, prompt)| prompt.contains("Run the release script."))
    });
    let tasks = fixture.call("agents.tasks", json!({})).unwrap();
    let task = tasks["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == task_id.as_str())
        .unwrap()
        .clone();
    assert_eq!(
        (&task["definition"], &task["parentId"]),
        (&json!("rlm-child@1"), &json!(parent))
    );
    // The skill's `model` frontmatter never reaches the child's lane.
    assert!(agents_support::lock(&fixture.models).is_empty());
    fixture.agents.close();
}

/// Requests outside the typed-agent host (none are expected).
struct Unused;

impl HostHandler for Unused {
    fn handle(&self, kind: &str, _: Map<String, Value>, _: &Signal) -> Result<Value, String> {
        Err(format!("unexpected host request: {kind}"))
    }
}

#[test]
fn acceptance_a31_concurrent_loads_selects_and_invokes_each_observe_one_catalog_snapshot_while_content_changes() {
    let s = Arc::new(setup(None));
    s.ok("skills.list", json!({}));
    let mut versions = vec![REVIEW.to_string()];
    let mut jobs: Vec<std::thread::JoinHandle<Option<&'static str>>> = Vec::new();
    for round in 1..=20 {
        let content = format!("{REVIEW}\nRevision {round}.");
        versions.push(content.clone());
        {
            let s = s.clone();
            jobs.push(std::thread::spawn(move || {
                s.skills
                    .set(source("review", "Review a diff for correctness bugs", &content, false));
                s.ok("skills.refresh", json!({}));
                None
            }));
        }
        for pinned in [REVIEW.to_string(), versions[round - 1].clone()] {
            let invoke_version = sha(&pinned);
            let invoker = s.clone();
            let s = s.clone();
            jobs.push(std::thread::spawn(move || {
                match s.call("skills.load", json!({"name": "review", "version": sha(&pinned)})) {
                    Ok(loaded) => {
                        assert_eq!(loaded["content"], pinned.as_str());
                        assert_eq!(loaded["version"], sha(&pinned));
                        Some("loaded")
                    }
                    Err(error) => {
                        assert!(error.starts_with("Stale skill version"), "{error}");
                        Some("stale")
                    }
                }
            }));
            jobs.push(std::thread::spawn(move || {
                let invoked = invoker.call(
                    "skills.invoke",
                    json!({"name": "review", "version": invoke_version, "input": {"round": round}}),
                );
                if let Err(error) = invoked {
                    assert!(error.starts_with("Stale skill version"), "{error}");
                }
                None
            }));
        }
        {
            let s = s.clone();
            jobs.push(std::thread::spawn(move || {
                let loaded = s.ok("skills.load", json!({"name": "review"}));
                let content = loaded["content"].as_str().unwrap();
                assert_eq!(loaded["version"], sha(content));
                // Some revision of the review skill, consistent with its own version.
                assert!(content.starts_with(REVIEW), "{content}");
                None
            }));
        }
        {
            let s = s.clone();
            jobs.push(std::thread::spawn(move || {
                let decision = s.ok("skills.select", json!({"query": "review diff"}));
                let chosen = decision["chosen"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|c| c["name"] == "review")
                    .unwrap()
                    .clone();
                assert_eq!(chosen["version"], decision["considered"]["review"]);
                None
            }));
        }
    }
    let outcomes: Vec<_> = jobs.into_iter().filter_map(|job| job.join().unwrap()).collect();
    // The race is real: some pinned loads win against a refresh and some lose.
    assert!(outcomes.contains(&"loaded"));
    assert!(outcomes.contains(&"stale"));
    // Every spawned child got the body of exactly the version its prompt names.
    let spawned = s.host.spawned.lock().unwrap().clone();
    assert!(!spawned.is_empty());
    for (request, _) in &spawned {
        let prompt = request["input"]["prompt"].as_str().unwrap();
        let at = prompt.find("Skill version: ").unwrap() + "Skill version: ".len();
        let version = &prompt[at..at + 64];
        let content = versions.iter().find(|c| sha(c) == version).unwrap();
        let body = &content[content[3..].find("---\n").unwrap() + 3 + 4..];
        assert!(prompt.contains(body.trim()), "{prompt}");
    }
    s.skills.set(source(
        "review",
        "Review a diff for correctness bugs",
        &versions[20],
        false,
    ));
    s.ok("skills.refresh", json!({}));
    assert_eq!(
        s.ok("skills.list", json!({}))["skills"][0]["version"],
        sha(&versions[20])
    );
}

#[test]
fn acceptance_a31_disable_model_invocation_skills_are_listed_but_cannot_be_invoked() {
    let s = setup(None);
    let listed = s.ok("skills.list", json!({}));
    let private = listed["skills"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["name"] == "private")
        .unwrap()
        .clone();
    assert_eq!(private["invocable"], false);
    rejects(
        s.call(
            "skills.invoke",
            json!({"name": "private", "version": sha(PRIVATE), "input": {}}),
        ),
        "disables model invocation",
    );
}

#[test]
fn acceptance_a31_rejects_unknown_payload_fields_forged_identity_and_malformed_arguments() {
    let s = setup(None);
    rejects(
        s.call("skills.list", json!({"verbose": true})),
        "Unknown payload field: verbose",
    );
    rejects(
        s.call("skills.refresh", json!({"force": true})),
        "Unknown payload field",
    );
    rejects(
        s.call("skills.select", json!({"query": "review", "caller_task_id": "root"})),
        "Unknown payload field: caller_task_id",
    );
    rejects(
        s.call("skills.load", json!({"name": "review", "tools": ["bash"]})),
        "Unknown payload field",
    );
    rejects(
        s.call(
            "skills.invoke",
            json!({"name": "review", "version": sha(REVIEW), "input": {}, "model": "a/b"}),
        ),
        "Unknown payload field: model",
    );
    rejects(
        s.call("skills.invoke", json!({"name": "review", "version": sha(REVIEW)})),
        "input",
    );
    rejects(
        s.call("skills.why", json!({"decision_id": "x", "sender": "root"})),
        "Unknown payload field",
    );
    rejects(
        s.call("skills.load", json!({"name": "review", "version": "v1"})),
        "sha256",
    );
    rejects(s.call("skills.select", json!({"query": "  "})), "query");
    rejects(s.call("skills.select", json!({"query": "x", "limit": 0})), "limit");
    rejects(s.call("skills.grant", json!({})), "Unsupported skills request");
    rejects(
        s.call("skills.load", json!({"name": "missing"})),
        "Unknown skill: missing",
    );
}

#[test]
fn acceptance_a31_durable_state_survives_a_new_module_instance_and_records_catalog_changes_across_restarts() {
    let store: Arc<MemoryDocumentStore> = Arc::default();
    let first = setup(Some(store.clone()));
    let decision = first.ok("skills.select", json!({"query": "review"}));
    let second = setup(Some(store.clone()));
    second.skills.set(source("review", "Review", "changed", false));
    assert_eq!(
        second.ok("skills.why", json!({"decision_id": decision["id"]})),
        decision
    );
    second.ok("skills.list", json!({}));
    let document = store.read().unwrap().unwrap();
    assert_eq!(document["generation"], 2);
    let last = document["changes"].as_array().unwrap().last().unwrap().clone();
    assert_eq!((&last["name"], &last["from"]), (&json!("review"), &json!(sha(REVIEW))));
}
