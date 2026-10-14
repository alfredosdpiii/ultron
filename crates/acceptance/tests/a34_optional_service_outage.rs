//! A34: optional-service failure preserves RLM operation without bypassing required checks or replaying effects.
//! Port of packages/coding-agent/test/ultron-a34-optional-service-outage.test.ts with its literal expectations: one
//! real runtime.py kernel cell (`crates/kernel`) against the Rust stack with every optional service down at once:
//! Hindsight (a memory backend whose every call fails, behind the real `MemoryService`), the refinement store (the
//! local services over session values that refuse `ultron.local`), the refinement hook of the typed-agent host, the
//! skills module, and a workflow step; the typed-agent host, usage ledger, progress, goals and release gates stay up.

mod agents_support;
mod common;
mod services_support;

use agents_support::{Memory, adapter};
use common::{kernel_in, run};
use delegation::usage::UsageLedger;
use kernel::{HostHandler, Signal, Status};
use kernel_apis::memory::MemoryBackend;
use kernel_apis::{
    ApiModule, MemoryApi, MemoryDocumentStore, MemoryService, ProgressModule, Request, SchedulesModule, worker_gate,
};
use serde_json::{Map, Value, json};
use services::ReleaseGateModule;
use services::local::{LocalOptions, LocalServices, LocalServicesModule};
use services_support::{Down, Host, Setup, build};
use std::sync::{Arc, Mutex};

/// Hindsight is down: every backend call fails, and is recorded.
struct HindsightDown(Arc<Mutex<Vec<String>>>);

impl HindsightDown {
    fn down(&self, name: &str) -> Result<Value, String> {
        self.0.lock().unwrap().push(name.into());
        Err("Hindsight unavailable".into())
    }
}

impl MemoryBackend for HindsightDown {
    fn namespace(&self) -> String {
        "fake://hindsight-down".into()
    }
    fn scope_tags(&self) -> Map<String, Value> {
        json!({ "session": ["ultron:session:a34"], "project": ["ultron:project:a34"], "global": ["ultron:global:a34"] })
            .as_object()
            .unwrap()
            .clone()
    }
    fn recall(&self, _: &Value, _: &Signal) -> Result<Value, String> {
        self.down("recall")
    }
    fn retain(&self, _: &Value, _: &Signal) -> Result<Value, String> {
        self.down("retain")
    }
    fn get(&self, _: &str, _: &Signal) -> Result<Value, String> {
        self.down("get")
    }
    fn delete(&self, _: &str, _: &Signal) -> Result<Value, String> {
        self.down("delete")
    }
    fn operation(&self, _: &str, _: &Signal) -> Result<Value, String> {
        self.down("operation")
    }
}

/// The skills module is down.
struct BrokenSkills;

impl ApiModule for BrokenSkills {
    fn prefixes(&self) -> &[&str] {
        &["skills."]
    }
    fn handle(&self, _: &Request<'_>) -> Result<Value, String> {
        Err("skill module unavailable".into())
    }
}

fn deterministic_definition(id: &str) -> Value {
    json!({
        "id": id, "version": "1", "strategy": "deterministic", "instructions": format!("Deterministic {id}."),
        "inputSchema": {}, "outputSchema": {}, "maxRepairs": 0, "inputDescription": "Any JSON value",
        "outputDescription": "Any JSON value",
    })
}

const CELL: &str = r#"
import json
data = list(range(1, 100001))
answer = sum(x * x for x in data) % 1000003
outcomes = {}
async def attempt(name, pending):
    try:
        outcomes[name] = {"ok": True, "value": await pending}
    except Exception as error:
        outcomes[name] = {"ok": False, "error": str(error)[:300]}

await attempt("memory_recall", memory.prepare("prior fixes", task_id="a34-root"))
await attempt("memory_propose", memory.propose("lesson", [{"ref": "task:a34"}]))
await attempt("refinements", refinements.list())
await attempt("skills", rlm.host_request("skills.list", {}))

nodes = [
    {"id": "effect", "definition": "external-effect@1", "input": {"order": "A34-1"}, "key": "wf:A34-1:effect"},
    {"id": "flaky", "definition": "flaky-step@1", "input": {"step": 2}, "dependsOn": ["effect"]},
    {"id": "summary", "definition": "identity@1", "inputFrom": "flaky", "dependsOn": ["flaky"]},
]
first_run = await workflows.run(nodes)
second_run = await workflows.run(nodes)

child = await agents.spawn("rlm-child@1", {"prompt": "summarize the computed answer"})
child_result = await child.result()
unnamed = await progress.reassess(child.id, claim="complete")
outage_verified = await progress.reassess(child.id, claim="complete", verifier="outage-check@1")
goal = await goals.create("ship A34", ["check-pass@1", "outage-check@1"])
goal_after = await goals.verify(goal["id"])
await gates.define("a34-release", [{"name": "unit", "required": True}, {"name": "memory-regression", "required": True}], "a" * 64)
gate = await gates.compare(
    "a34-release",
    {"variant": "base", "fixture_hash": "a" * 64, "results": {"unit": "passed", "memory-regression": "passed"}},
    {"variant": "cand", "fixture_hash": "a" * 64, "results": {"unit": "passed"}},
)
print(json.dumps({
    "answer": answer,
    "outcomes": outcomes,
    "first_run": first_run,
    "second_run": second_run,
    "child": child_result,
    "unnamed": unnamed["claim"],
    "outage_verified": outage_verified["claim"],
    "goal": {"state": goal_after["state"], "status": goal_after["verification"]["status"]},
    "gate": {"decision": gate["decision"], "reasons": gate["reasons"]},
}))
"#;

struct Stores {
    tasks: Memory,
    usage: Memory,
    definitions: Memory,
}

fn build_host(stores: &Stores, effects: &Arc<Mutex<Vec<String>>>, calls: &Arc<Mutex<Vec<String>>>) -> Host {
    let memory = MemoryService::new(
        Arc::new(MemoryDocumentStore::default()),
        Arc::new(HindsightDown(calls.clone())),
        worker_gate(),
    )
    .unwrap();
    let local = LocalServices::new(Box::new(Down("refinement store unavailable")), LocalOptions::default());
    let effects = effects.clone();
    build(Setup {
        tasks: Box::new(stores.tasks.clone()),
        definitions: Box::new(stores.definitions.clone()),
        usage: Some(UsageLedger::new(Box::new(stores.usage.clone()), Default::default()).unwrap()),
        script: Some(Arc::new(|_, _, _| Ok("summary without optional services".into()))),
        refinements: Some(Arc::new(|_: &str| Err("refinement service unavailable".into()))),
        deterministic: Some(adapter(move |id, input, _, _| match id {
            "external-effect" => {
                let mut effects = effects.lock().unwrap();
                effects.push(serde_json::to_string(input).unwrap());
                Ok(json!({ "receipt": format!("receipt-{}", effects.len()) }))
            }
            "flaky-step" => Err("workflow step service unavailable".into()),
            "outage-check" => Err("verifier dependency (memory) unavailable".into()),
            "check-pass" => Ok(json!({ "passed": true })),
            _ => Ok(input.clone()),
        })),
        modules: vec![
            Arc::new(ProgressModule::new(Arc::new(MemoryDocumentStore::default()))),
            Arc::new(SchedulesModule::new(Arc::new(MemoryDocumentStore::default()), None)),
            Arc::new(ReleaseGateModule::new(Arc::new(MemoryDocumentStore::default()), None)),
            Arc::new(BrokenSkills),
            Arc::new(MemoryApi { service: Some(memory) }),
            Arc::new(LocalServicesModule {
                services: Arc::new(local),
            }),
        ],
        ..Setup::default()
    })
}

fn has(got: &Value, want: Value) {
    for (key, value) in want.as_object().unwrap() {
        assert_eq!(&got[key], value, "{key} of {got}");
    }
}

#[test]
fn acceptance_a34_combined_hindsight_refinement_module_and_workflow_outage_in_one_real_rlm_kernel_run() {
    let effects: Arc<Mutex<Vec<String>>> = Arc::default();
    let backend_calls: Arc<Mutex<Vec<String>>> = Arc::default();
    let stores = Stores {
        tasks: Memory::default(),
        usage: Memory::default(),
        definitions: Memory::default(),
    };
    let host = build_host(&stores, &effects, &backend_calls);
    for id in ["external-effect", "flaky-step", "outage-check", "check-pass"] {
        host.call("agents.register", json!({ "definition": deterministic_definition(id) }))
            .unwrap();
    }
    let kernel = kernel_in(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")),
        host.apis.clone() as Arc<dyn HostHandler>,
    );
    let execution = run(&kernel, CELL);
    assert_eq!(execution.status, Status::Ok, "{execution:?}");
    let report: Value = serde_json::from_str(execution.stdout.trim().lines().last().unwrap()).unwrap();

    // Self-contained RLM work completes over the full data with every optional service down.
    let expected = (1..=100_000u128).map(|x| x * x).sum::<u128>() % 1_000_003;
    assert_eq!(report["answer"], json!(expected as u64));

    // Each optional service fails closed and visibly; none is silently treated as success.
    let outcomes = &report["outcomes"];
    assert_eq!(outcomes["memory_recall"]["ok"], false);
    assert_eq!(outcomes["memory_propose"]["ok"], false);
    assert_eq!(outcomes["refinements"]["ok"], false);
    assert!(
        outcomes["refinements"]["error"]
            .as_str()
            .unwrap()
            .contains("refinement store unavailable")
    );
    assert_eq!(outcomes["skills"]["ok"], false);
    assert!(outcomes["skills"]["error"].as_str().unwrap().contains("skill module"));
    // The recall and the write both reach the down backend and fail visibly; neither counts as success.
    assert_eq!(*backend_calls.lock().unwrap(), ["recall", "retain"]);

    // The workflow records its failure and skip; the explicit rerun does not repeat the keyed effect.
    for run in [&report["first_run"], &report["second_run"]] {
        has(
            &run["effect"],
            json!({ "status": "succeeded", "value": { "receipt": "receipt-1" } }),
        );
        has(
            &run["flaky"],
            json!({ "status": "failed", "error": "workflow step service unavailable" }),
        );
        assert_eq!(run["summary"]["status"], "skipped");
        assert_eq!(run["summary"].as_object().unwrap().len(), 2, "{}", run["summary"]);
        assert!(run["summary"]["reason"].as_str().unwrap().contains("did not succeed"));
    }
    assert_eq!(*effects.lock().unwrap(), [json!({ "order": "A34-1" }).to_string()]);

    // A model-backed child still runs without its refinement service.
    has(
        &report["child"],
        json!({ "status": "succeeded", "value": "summary without optional services" }),
    );
    let prompts: Vec<String> = host.prompts.lock().unwrap().iter().map(|(_, p)| p.clone()).collect();
    assert_eq!(prompts, ["summarize the computed answer"]);

    // Required verification still refuses success.
    assert_eq!(report["unnamed"]["decision"], "unverified");
    has(
        &report["outage_verified"],
        json!({ "decision": "unverified", "verifier_status": "failed" }),
    );
    assert!(
        report["outage_verified"]["reason"]
            .as_str()
            .unwrap()
            .contains("verifier dependency (memory) unavailable")
    );
    assert_eq!(report["goal"], json!({ "state": "active", "status": "unachieved" }));
    assert_eq!(report["gate"]["decision"], "blocked");

    // The kernel remains usable after the combined outage.
    let after = run(&kernel, "answer + 1");
    assert_eq!(after.status, Status::Ok);
    assert_eq!(
        after.result.as_deref(),
        Some((expected as u64 + 1).to_string().as_str())
    );
    kernel.shutdown();
    host.close();

    // After an owner restart, the same keyed workflow still does not replay the effect.
    let restarted = build_host(&stores, &effects, &backend_calls);
    let rerun = restarted
        .call(
            "workflows.run",
            json!({ "nodes": [{ "id": "effect", "definition": "external-effect@1", "input": { "order": "A34-1" },
                                "key": "wf:A34-1:effect" }] }),
        )
        .unwrap();
    assert_eq!(rerun["effect"]["status"], "succeeded");
    assert_eq!(effects.lock().unwrap().len(), 1);
    let status = restarted.call("agents.status", json!({})).unwrap();
    let external = status["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|task| task["definition"] == "external-effect@1")
        .count();
    assert_eq!(external, 1);
    restarted.close();
}
