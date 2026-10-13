//! A31 (partial): skill selection is version-pinned, explainable, and cannot grant capabilities.
//! Ported from packages/coding-agent/test/ultron-skills.test.ts with literal expectations: the real
//! `kernel_apis::SkillsModule` over the same three-skill catalog, a changeable loader and a fixed clock, called with
//! the caller identity the TS `fakeHost` derives from the lane (`ultron.rlm-child.parent-task` → `parent-task`).
//! Ported whole: list, select, why, version-pinned load (including the stale `skills.invoke`), disabled invocation,
//! strict payloads, durable state. Ported in part: capability frontmatter (the load report; not the spawned child's
//! request) and the concurrency race (loads, selects and refreshes; not the invokes).
//! Not ported: every case that needs `skills.invoke` to spawn the skill's `rlm-child@1` task — Rust's
//! `skills.invoke` validates and then fails ("needs task spawning", crates/kernel-apis/src/skills.rs), so "invoke
//! through the real host keeps the child's model and tools and links the parent" and the spawned-request halves of
//! the capability and concurrency cases have no Rust counterpart.

use kernel::Signal;
use kernel_apis::skills::{SkillSource, SkillsModule};
use kernel_apis::{DocumentStore, MemoryDocumentStore};
use serde_json::{Map, Value, json};
use std::sync::{Arc, Mutex};

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

struct Setup {
    skills: Catalog,
    module: SkillsModule,
    store: Arc<MemoryDocumentStore>,
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
    Setup { skills, module, store }
}

const PARENT_LANE: &str = "ultron.rlm-child.parent-task";

impl Setup {
    fn call_as(&self, kind: &str, payload: Value, lane: &str) -> Result<Value, String> {
        let caller = (lane == PARENT_LANE).then_some("parent-task");
        let payload: Map<String, Value> = payload.as_object().cloned().unwrap_or_default();
        self.module.handle(kind, &payload, caller, &Signal::new())
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
fn acceptance_a31_capability_granting_frontmatter_is_reported_and_never_reaches_the_spawned_child_ported_part() {
    let s = setup(None);
    let loaded = s.ok("skills.load", json!({"name": "deploy"}));
    assert_eq!(
        loaded["ignored_capability_requests"],
        json!({"allowed-tools": ["bash", "write"], "tools": ["*"], "grants": {"network": true},
            "permissions": "admin", "model": "anthropic/claude-opus"})
    );
}

#[test]
fn acceptance_a31_concurrent_loads_and_selects_each_observe_one_catalog_snapshot_while_content_changes_ported_part() {
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
