//! A11: approval binds scope, revision, policy and action, and is rechecked on every use.
//! Ported from packages/coding-agent/test/ultron-grants.test.ts (all nine cases) with literal expectations. The TS
//! tests send `grants.*` and `background.start` through `NativeRlmHost` with waiting lanes; here the same requests
//! go through `g3_support::Host`: the real `kernel_apis::GrantsModule` over task identity and lineage from the real
//! `harness` task supervisor (owner lanes `ultron.background-job.<id>`). The pure `checkGrant` case evaluates a
//! stored document through `GrantsModule::handle` with an explicit caller, task list and clock (Rust has no separate
//! `checkGrant`; the check is the handler's).

mod g3_support;

use g3_support::{Host, JournalStore, Tasks, lane_name, rejects};
use kernel_apis::grants::TaskLink;
use kernel_apis::{DocumentStore, GrantsModule, MemoryDocumentStore};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

const REV_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const REV_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn binding() -> Value {
    json!({"scope": "repo:/work/app", "revision": REV_A, "policy": "apply-policy@3", "action": "apply-patch"})
}

fn with(extra: Value) -> Value {
    let mut out = binding();
    for (key, value) in extra.as_object().unwrap() {
        out[key] = value.clone();
    }
    out
}

struct Setup {
    host: Host,
    clock: Arc<AtomicI64>,
    owner: String,
    owner_lane: String,
}

fn setup(store: Option<Arc<MemoryDocumentStore>>, enforce: Option<bool>) -> Setup {
    let clock = Arc::new(AtomicI64::new(1_000_000));
    let now = clock.clone();
    let module = GrantsModule::new(
        store.unwrap_or_default(),
        Arc::new(move || now.load(Ordering::SeqCst)),
        enforce.unwrap_or(false),
    );
    let tasks = Tasks::new(JournalStore::default(), clock.clone());
    let host = Host::new(tasks, vec![Arc::new(module)]);
    let owner = host.call("background.start", json!({"prompt": "owner"})).unwrap();
    let owner = owner["id"].as_str().unwrap().to_string();
    let owner_lane = lane_name("background-job@1", &owner);
    Setup {
        host,
        clock,
        owner,
        owner_lane,
    }
}

impl Setup {
    fn issue(&self, lane: &str, ttl: i64) -> Value {
        self.host
            .call_in(lane, "grants.issue", with(json!({"ttl_ms": ttl})))
            .unwrap()
    }
    fn check(&self, lane: &str, id: &Value, change: Value) -> Value {
        let mut payload = with(change);
        payload["id"] = id.clone();
        self.host.call_in(lane, "grants.check", payload).unwrap()
    }
}

#[test]
fn acceptance_a11_issues_a_grant_owned_by_the_calling_lane_and_allows_a_matching_check() {
    let s = setup(None, None);
    let grant = s.issue(&s.owner_lane, 60_000);
    for (key, value) in binding().as_object().unwrap() {
        assert_eq!(&grant[key], value);
    }
    assert_eq!(grant["owner_task_id"], s.owner.as_str());
    assert_eq!(grant["expires_at"], 1_060_000);
    assert_eq!(grant["revoked_at"], Value::Null);
    let result = s.check(&s.owner_lane, &grant["id"], json!({}));
    assert_eq!(
        (&result["allowed"], &result["reasons"], &result["enforced"]),
        (&json!(true), &json!([]), &json!(false))
    );
    let listed = s.host.call_in(&s.owner_lane, "grants.list", json!({})).unwrap();
    let ids: Vec<_> = listed["grants"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["id"].clone())
        .collect();
    assert_eq!(ids, vec![grant["id"].clone()]);
    assert_eq!(listed["enforced"], false);
}

#[test]
fn acceptance_a11_revocation_denies_later_checks_and_only_the_owner_lineage_may_revoke() {
    let s = setup(None, None);
    let grant = s.issue(&s.owner_lane, 60_000);
    let stranger = s.host.call("background.start", json!({"prompt": "stranger"})).unwrap();
    let stranger_lane = lane_name("background-job@1", stranger["id"].as_str().unwrap());
    rejects(
        s.host
            .call_in(&stranger_lane, "grants.revoke", json!({"id": grant["id"]})),
        "owner or one of its ancestors",
    );
    // The root is an ancestor of every task.
    let revoked = s.host.call("grants.revoke", json!({"id": grant["id"]})).unwrap();
    assert_eq!(revoked["revoked_at"], 1_000_000);
    let result = s.check(&s.owner_lane, &grant["id"], json!({}));
    assert_eq!(result["allowed"], false);
    assert!(result["reasons"].to_string().contains("revoked"));
}

#[test]
fn acceptance_a11_expiry_is_rechecked_against_the_clock_on_every_check() {
    let s = setup(None, None);
    let grant = s.issue(&s.owner_lane, 1_000);
    s.clock.fetch_add(999, Ordering::SeqCst);
    assert_eq!(s.check(&s.owner_lane, &grant["id"], json!({}))["allowed"], true);
    s.clock.fetch_add(1, Ordering::SeqCst);
    let expired = s.check(&s.owner_lane, &grant["id"], json!({}));
    assert_eq!(expired["allowed"], false);
    assert!(expired["reasons"].to_string().contains("expired"));
}

fn only_reason(result: &Value, text: &str) {
    let reasons = result["reasons"].as_array().unwrap();
    assert_eq!(reasons.len(), 1, "{result}");
    assert!(reasons[0].as_str().unwrap().contains(text), "{result}");
}

#[test]
fn acceptance_a11_a_changed_base_policy_scope_or_action_is_denied() {
    let s = setup(None, None);
    let grant = s.issue(&s.owner_lane, 60_000);
    let id = &grant["id"];
    let changed_base = s.check(&s.owner_lane, id, json!({"revision": REV_B}));
    assert_eq!(changed_base["allowed"], false);
    only_reason(&changed_base, "Base changed");
    only_reason(
        &s.check(&s.owner_lane, id, json!({"policy": "apply-policy@4"})),
        "Policy changed",
    );
    only_reason(
        &s.check(&s.owner_lane, id, json!({"scope": "repo:/work/other"})),
        "Scope differs",
    );
    only_reason(&s.check(&s.owner_lane, id, json!({"action": "push"})), "Action differs");
    assert_eq!(s.check(&s.owner_lane, id, json!({}))["allowed"], true);
    assert_eq!(
        s.check(&s.owner_lane, &json!("grant-nope"), json!({}))["reasons"],
        json!(["Unknown grant"])
    );
}

#[test]
fn acceptance_a11_descendants_of_the_owner_may_use_a_grant_siblings_and_the_root_may_not() {
    let s = setup(None, None);
    let child = s
        .host
        .call_in(&s.owner_lane, "background.start", json!({"prompt": "child"}))
        .unwrap();
    let sibling = s.host.call("background.start", json!({"prompt": "sibling"})).unwrap();
    let child_lane = lane_name("background-job@1", child["id"].as_str().unwrap());
    let sibling_lane = lane_name("background-job@1", sibling["id"].as_str().unwrap());
    let grant = s.issue(&s.owner_lane, 60_000);
    assert_eq!(grant["owner_task_id"], s.owner.as_str());

    assert_eq!(s.check(&child_lane, &grant["id"], json!({}))["allowed"], true);
    let from_sibling = s.check(&sibling_lane, &grant["id"], json!({}));
    assert_eq!(from_sibling["allowed"], false);
    only_reason(&from_sibling, "Wrong owner");
    assert_eq!(s.check("main", &grant["id"], json!({}))["allowed"], false);
    let sibling_list = s.host.call_in(&sibling_lane, "grants.list", json!({})).unwrap();
    assert_eq!(sibling_list["grants"], json!([]));

    // A root-owned grant covers every task.
    let root_grant = s.issue("main", 60_000);
    assert_eq!(root_grant["owner_task_id"], Value::Null);
    assert_eq!(s.check(&sibling_lane, &root_grant["id"], json!({}))["allowed"], true);
}

#[test]
fn acceptance_a11_rejects_a_forged_owner_and_other_unknown_or_malformed_fields() {
    let s = setup(None, None);
    let lane = &s.owner_lane;
    let call = |kind: &str, payload: Value| s.host.call_in(lane, kind, payload);
    rejects(
        call("grants.issue", with(json!({"ttl_ms": 60_000, "owner": "someone-else"}))),
        "Unknown payload field: owner",
    );
    rejects(
        call("grants.issue", with(json!({"ttl_ms": 60_000, "owner_task_id": null}))),
        "Unknown payload field: owner_task_id",
    );
    rejects(call("grants.issue", with(json!({"ttl_ms": 0}))), "ttl_ms");
    rejects(
        call("grants.issue", with(json!({"policy": "no-version", "ttl_ms": 5}))),
        "id@version",
    );
    rejects(
        call("grants.check", with(json!({"id": "x", "owner": null}))),
        "Unknown payload field: owner",
    );
    rejects(call("grants.list", json!({"all": true})), "Unknown payload field");
}

#[test]
fn acceptance_a11_enforce_defaults_to_false_and_is_reported_opting_in_reports_true() {
    let off = setup(None, None);
    let off_grant = off.issue(&off.owner_lane, 60_000);
    let denied = off.check("main", &off_grant["id"], json!({"revision": REV_B}));
    assert_eq!(
        (&denied["allowed"], &denied["enforced"]),
        (&json!(false), &json!(false))
    );

    let on = setup(None, Some(true));
    let on_grant = on.issue(&on.owner_lane, 60_000);
    let allowed = on.check(&on.owner_lane, &on_grant["id"], json!({}));
    assert_eq!(
        (&allowed["allowed"], &allowed["enforced"]),
        (&json!(true), &json!(true))
    );
}

#[test]
fn acceptance_a11_grants_and_revocations_survive_a_new_module_instance() {
    let store = Arc::new(MemoryDocumentStore::default());
    let first = setup(Some(store.clone()), None);
    let kept = first.issue("main", 60_000);
    let dropped = first.issue("main", 60_000);
    first.host.call("grants.revoke", json!({"id": dropped["id"]})).unwrap();

    let second = setup(Some(store), None);
    let listed = second.host.call("grants.list", json!({})).unwrap();
    let pairs: Vec<_> = listed["grants"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| json!([g["id"], !g["revoked_at"].is_null()]))
        .collect();
    assert_eq!(pairs, vec![json!([kept["id"], false]), json!([dropped["id"], true])]);
    assert_eq!(second.check("main", &kept["id"], json!({}))["allowed"], true);
    assert_eq!(second.check("main", &dropped["id"], json!({}))["allowed"], false);
}

#[test]
fn acceptance_a11_a_check_is_a_pure_evaluation_that_reports_every_failing_binding() {
    let mut grant = with(
        json!({"id": "g1", "owner_task_id": "t1", "issued_at": 0, "expires_at": 100,
        "revoked_at": 50, "revoked_by": "root"}),
    );
    let evaluate = |grant: &Value, now: i64, caller: &str, tasks: &[(&str, Option<&str>)], change: Value| {
        let store = Arc::new(MemoryDocumentStore::new(Some(json!({"version": 1, "grants": [grant]}))));
        let module = GrantsModule::new(store.clone(), Arc::new(move || now), false);
        let links: Vec<TaskLink> = tasks
            .iter()
            .map(|(id, parent)| TaskLink {
                id: id.to_string(),
                parent_id: parent.map(str::to_string),
            })
            .collect();
        let mut payload = with(change);
        payload["id"] = "g1".into();
        let before = store.read().unwrap();
        let result = module
            .handle("grants.check", payload.as_object().unwrap(), Some(caller), &links)
            .unwrap();
        // Pure: the check leaves the stored document as it was.
        assert_eq!(store.read().unwrap(), before);
        result
    };
    let result = evaluate(
        &grant,
        200,
        "t2",
        &[("t1", None), ("t2", None)],
        json!({"revision": REV_B}),
    );
    assert_eq!(result["allowed"], false);
    assert_eq!(result["reasons"].as_array().unwrap().len(), 4);
    grant["revoked_at"] = Value::Null;
    let allowed = evaluate(&grant, 10, "t3", &[("t1", None), ("t3", Some("t1"))], json!({}));
    assert_eq!(allowed["allowed"], true);
}
