//! A36 / A40: invocation scratch is isolated while declared instance state persists; retained instances accept new
//! invocations without rewriting old terminal outcomes.
//! Ported from packages/coding-agent/test/ultron-instances.test.ts with literal expectations. The three
//! retained-instance cases run the real `kernel_apis::InstancesModule` over the Rust typed-agent host
//! (`delegation::agents::Agents`, through `agents_support`) as the TS cases run `createInstanceModule` over
//! `NativeRlmHost`: `rlm-child@1` runs on its named task lane, `identity@1` deterministically, `instances.invoke`
//! reuses the lane (resetting its scratch first, `before_lane_reuse`), and a restart is a new host and module over
//! the same task journal and instance document. Lanes are faked as the TS `fakeHarness` fakes them: each remembers
//! its prompts and replies `reply <n>`, and prompts can be held. The reset case runs the real runtime.py under the
//! Rust kernel supervisor (`Kernel::reset_scratch`).

mod agents_support;
mod common;

use agents_support::{Fixture, Memory, Options, fixture, lock};
use common::{kernel_in, null_handler, value};
use kernel::{HostHandler, Signal, Status};
use kernel_apis::{ApiModule, InstancesModule, KernelApis, MemoryDocumentStore};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};

/// The TS `fakeHarness`: every lane remembers its prompts and replies `reply <count>`; prompts can be held.
#[derive(Clone, Default)]
struct FakeLanes {
    prompts: Arc<Mutex<HashMap<String, Vec<String>>>>,
    hold: Arc<(Mutex<bool>, Condvar)>,
}

impl FakeLanes {
    fn prompts(&self, lane: &str) -> Vec<String> {
        lock(&self.prompts).get(lane).cloned().unwrap_or_default()
    }
    fn hold_prompts(&self) {
        *lock(&self.hold.0) = true;
    }
    fn release(&self) {
        *lock(&self.hold.0) = false;
        self.hold.1.notify_all();
    }
}

/// The task journal and instance document a restarted owner reopens.
#[derive(Clone, Default)]
struct Stores {
    tasks: Memory,
    instances: Arc<MemoryDocumentStore>,
}

struct Built {
    fixture: Fixture,
    module: Arc<InstancesModule>,
    resets: Arc<Mutex<Vec<String>>>,
    stores: Stores,
}

/// Requests outside the typed-agent host (none are expected).
struct Unused;

impl HostHandler for Unused {
    fn handle(&self, kind: &str, _: Map<String, Value>, _: &Signal) -> Result<Value, String> {
        Err(format!("unexpected host request: {kind}"))
    }
}

fn build(fake: &FakeLanes, stores: Option<Stores>) -> Built {
    let stores = stores.unwrap_or_default();
    let resets: Arc<Mutex<Vec<String>>> = Arc::default();
    let (lanes, reset_log) = (fake.clone(), resets.clone());
    let fixture = fixture(Options {
        store: Some(stores.tasks.clone()),
        script: Some(Arc::new(move |lane, prompt, _, _| {
            let count = {
                let mut prompts = lock(&lanes.prompts);
                let seen = prompts.entry(lane.to_string()).or_default();
                seen.push(prompt.to_string());
                seen.len()
            };
            let (held, released) = &*lanes.hold;
            let mut held = lock(held);
            while *held {
                held = released.wait(held).unwrap();
            }
            Ok(format!("reply {count}"))
        })),
        before_lane_reuse: Some(Arc::new(move |lane: &str| {
            lock(&reset_log).push(lane.to_string());
            Ok(())
        })),
        ..Default::default()
    });
    let module = Arc::new(InstancesModule::new(stores.instances.clone(), None));
    Built {
        fixture,
        module,
        resets,
        stores,
    }
}

impl Built {
    /// One request from `lane`, served as that lane's kernel requests are: `instances.*` by the module over the
    /// typed-agent host, everything else by the host.
    fn call_as(&self, kind: &str, payload: Value, lane: &str) -> Result<Value, String> {
        let inner: Arc<dyn HostHandler> = Arc::new(self.fixture.agents.handler(lane, Arc::new(Unused)));
        let apis = KernelApis::new(inner)
            .with_host(Arc::new(self.fixture.agents.clone()), lane)
            .with(self.module.clone() as Arc<dyn ApiModule>);
        apis.handle(kind, payload.as_object().cloned().unwrap_or_default(), &Signal::new())
    }

    fn call(&self, kind: &str, payload: Value) -> Result<Value, String> {
        self.call_as(kind, payload, "main")
    }
}

/// `toMatchObject` for one level of fields.
fn matches(actual: &Value, expected: Value) {
    for (key, value) in expected.as_object().unwrap() {
        assert_eq!(&actual[key], value, "{key} of {actual}");
    }
}

#[test]
fn acceptance_a36_continue_on_the_same_lane_with_fresh_scratch_and_never_rewrite_the_earlier_result() {
    let fake = FakeLanes::default();
    let b = build(&fake, None);
    let first = b
        .call(
            "agents.spawn",
            json!({"definition": "rlm-child@1", "input": {"prompt": "one"}}),
        )
        .unwrap();
    let first = first["id"].as_str().unwrap().to_string();
    let first_result = b.call("agents.result", json!({"id": first})).unwrap();
    matches(&first_result, json!({"status": "succeeded", "value": "reply 1"}));

    let instance = b.call("instances.retain", json!({"task_id": first})).unwrap();
    let lane = instance["lane"].as_str().unwrap().to_string();
    assert_eq!(lane, format!("ultron.rlm-child.{first}"));
    matches(
        &b.call("instances.retain", json!({"task_id": first})).unwrap(),
        json!({"id": instance["id"]}),
    );

    let second = b
        .call(
            "instances.invoke",
            json!({"id": instance["id"], "input": {"prompt": "two"}}),
        )
        .unwrap();
    matches(
        &b.call("agents.result", json!({"id": second["task_id"]})).unwrap(),
        json!({"status": "succeeded", "value": "reply 2"}),
    );
    // Same lane, continued conversation; scratch reset only for the reuse.
    assert_eq!(fake.prompts(&lane), ["one", "two"]);
    assert_eq!(*lock(&b.resets), [lane.as_str()]);
    // The first invocation's durable outcome is untouched.
    assert_eq!(b.call("agents.result", json!({"id": first})).unwrap(), first_result);
    b.fixture.agents.close();
}

#[test]
fn acceptance_a36_reject_concurrent_use_closed_instances_non_parents_and_laneless_tasks() {
    let fake = FakeLanes::default();
    let b = build(&fake, None);
    let first = b
        .call(
            "agents.spawn",
            json!({"definition": "rlm-child@1", "input": {"prompt": "one"}}),
        )
        .unwrap();
    let first = first["id"].as_str().unwrap().to_string();
    b.call("agents.result", json!({"id": first})).unwrap();
    let instance = b.call("instances.retain", json!({"task_id": first})).unwrap();

    fake.hold_prompts();
    b.call(
        "instances.invoke",
        json!({"id": instance["id"], "input": {"prompt": "slow"}}),
    )
    .unwrap();
    let busy = b
        .call(
            "instances.invoke",
            json!({"id": instance["id"], "input": {"prompt": "again"}}),
        )
        .unwrap_err();
    assert!(busy.contains("busy"), "{busy}");
    fake.release();

    let deterministic = b
        .call("agents.spawn", json!({"definition": "identity@1", "input": {"a": 1}}))
        .unwrap();
    b.call("agents.result", json!({"id": deterministic["id"]})).unwrap();
    let laneless = b
        .call("instances.retain", json!({"task_id": deterministic["id"]}))
        .unwrap_err();
    assert!(laneless.contains("Only RLM tasks"), "{laneless}");
    // A task that is not the origin's parent cannot retain it.
    assert!(
        b.call_as(
            "instances.retain",
            json!({"task_id": first}),
            &format!("ultron.rlm-child.{first}")
        )
        .is_err()
    );

    b.call("instances.close", json!({"id": instance["id"]})).unwrap();
    let closed = b
        .call(
            "instances.invoke",
            json!({"id": instance["id"], "input": {"prompt": "x"}}),
        )
        .unwrap_err();
    assert!(closed.contains("closed"), "{closed}");
    let forged = b
        .call("instances.retain", json!({"task_id": first, "owner": "forged"}))
        .unwrap_err();
    assert!(forged.contains("Unknown payload field"), "{forged}");
    b.fixture.agents.close();
}

#[test]
fn acceptance_a36_an_instance_survives_an_owner_restart_and_keeps_its_lane() {
    let fake = FakeLanes::default();
    let first = build(&fake, None);
    let origin = first
        .call(
            "agents.spawn",
            json!({"definition": "rlm-child@1", "input": {"prompt": "one"}}),
        )
        .unwrap();
    first.call("agents.result", json!({"id": origin["id"]})).unwrap();
    let instance = first
        .call("instances.retain", json!({"task_id": origin["id"]}))
        .unwrap();
    let lane = instance["lane"].as_str().unwrap().to_string();
    first.fixture.agents.close();

    let restarted = build(&fake, Some(first.stores.clone()));
    let next = restarted
        .call(
            "instances.invoke",
            json!({"id": instance["id"], "input": {"prompt": "after restart"}}),
        )
        .unwrap();
    matches(
        &restarted.call("agents.result", json!({"id": next["task_id"]})).unwrap(),
        json!({"status": "succeeded"}),
    );
    assert_eq!(fake.prompts(&lane), ["one", "after restart"]);
    let got = restarted.call("instances.get", json!({"id": instance["id"]})).unwrap();
    let invocations = got["invocations"].as_array().unwrap();
    assert_eq!(invocations.len(), 1);
    assert_eq!(invocations[0]["task_id"], next["task_id"]);
    restarted.fixture.agents.close();
}

#[test]
fn acceptance_a36_reset_clears_scratch_and_restores_bindings_while_declared_state_persists() {
    let dir = tempfile::tempdir().unwrap();
    let kernel = kernel_in(dir.path(), null_handler());
    value(
        &kernel,
        "scratch = 'first call'\nstate['calls'] = state.get('calls', 0) + 1\nagents = None",
    );
    let reset = kernel.reset_scratch().unwrap();
    assert_eq!(reset.status, Status::Ok, "{reset:?}");
    value(&kernel, "state['calls'] = state.get('calls', 0) + 1");
    assert_eq!(
        value(&kernel, "('scratch' in globals(), state['calls'], agents is not None)"),
        "(False, 2, True)"
    );
    kernel.shutdown();
}

const RETAIN_CELL: &str = r#"import json
t = await agents.spawn("rlm-child@1", {"prompt": "INSTANCE ONE: remember the word KIWI"})
r1 = await agents.result(t.id)
inst = await instances.retain(t.id)
nxt = await instances.invoke(inst["id"], {"prompt": "INSTANCE TWO: which word?"})
r2 = await agents.result(nxt["task_id"])
again = await agents.result(t.id)
got = await instances.get(inst["id"])
print('RESULT ' + json.dumps({"lane": inst["lane"], "id": t.id, "r1": r1["value"], "r2": r2["value"], "same": again == r1, "pinned": inst["pinned"], "invocations": [i["task_id"] for i in got["invocations"]], "next": nxt["task_id"]}))"#;

/// Through the binary over RPC: a retained `rlm-child@1` instance continues its durable child conversation on the
/// same lane, its kernel pinned in the session's pool, and the first invocation's result stays as it was.
#[test]
fn acceptance_a36_end_to_end_a_retained_instance_continues_its_lane_conversation() {
    use common::{Install, Provider, Reply, Request};
    let first_user = |request: &Request| {
        request
            .users()
            .into_iter()
            .find(|u| !u.starts_with("[clock]"))
            .unwrap_or_default()
    };
    let provider = Provider::start(move |request| {
        let first = first_user(request);
        if first.starts_with("INSTANCE ONE") {
            let continued = request.users().iter().any(|u| u.starts_with("INSTANCE TWO"));
            return Reply::Text(if continued { "KIWI, still" } else { "KIWI noted" }.into());
        }
        if first.starts_with("A36") {
            return match request.tool_results().len() {
                0 => Reply::Rlm(RETAIN_CELL.into()),
                _ => Reply::Text("A36 COMPLETE".into()),
            };
        }
        panic!("Unscripted request: {first}");
    });
    let install = Install::new(&provider);
    let mut client = install.rpc(&["--no-session"], &[("ULTRON_HINDSIGHT_URL", "off")]);
    client
        .prompt_and_wait("A36: retain a child and ask it again", common::HANG)
        .unwrap();
    let results: Vec<Value> = common::rpc_tool_results(&client.get_messages().unwrap())
        .iter()
        .flat_map(|text| text.lines().map(str::to_string).collect::<Vec<_>>())
        .filter_map(|line| {
            line.strip_prefix("RESULT ")
                .map(|json| serde_json::from_str(json).unwrap())
        })
        .collect();
    assert_eq!(results.len(), 1, "{results:?}");
    let r = &results[0];
    let id = r["id"].as_str().unwrap();
    assert_eq!(r["lane"], format!("ultron.rlm-child.{id}"));
    assert_eq!((&r["r1"], &r["r2"]), (&json!("KIWI noted"), &json!("KIWI, still")));
    assert_eq!(r["same"], true);
    assert_eq!(r["pinned"], true);
    assert_eq!(r["invocations"], json!([r["next"]]));
    // The second invocation is the same conversation's next turn: its request carries the first exchange.
    let children: Vec<Request> = provider
        .requests()
        .into_iter()
        .filter(|r| first_user(r).starts_with("INSTANCE ONE"))
        .collect();
    assert_eq!(children.len(), 2, "{children:?}");
    let second = &children[1];
    assert!(second.raw.contains("KIWI noted"));
    assert_eq!(second.users().iter().filter(|u| !u.starts_with("[clock]")).count(), 2);
    let (_, code) = client.finish(common::HANG);
    assert_eq!(code, Some(0));
}
