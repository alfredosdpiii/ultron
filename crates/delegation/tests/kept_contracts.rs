//! Contracts a session keeps for its subagents (`Delegation::contract_of`, `SpawnContract::from_described`,
//! `Reattached`): a kept contract reads back as the spawn wrote it, and a host reopened over the journal holds a
//! reattached child to it; a kept contract it cannot read fails the child. The session-level behaviour across a
//! restart is in session-core/tests/contract_restart.rs. New in Rust: literal expectations.

use delegation::host::{ChildRunner, Delegation, DelegationOptions, Reattached};
use delegation::spawn_contract::SpawnContract;
use delegation::task_store::TaskStore;
use kernel::Signal;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const HANG: Duration = Duration::from_secs(120);

/// One task journal document two hosts open in turn (as two workers of one session).
#[derive(Clone, Default)]
struct Shared(Arc<Mutex<Option<Value>>>);

impl TaskStore for Shared {
    fn read(&mut self) -> Result<Option<Value>, String> {
        Ok(self.0.lock().unwrap().clone())
    }
    fn write(&mut self, document: &Value) -> Result<(), String> {
        *self.0.lock().unwrap() = Some(document.clone());
        Ok(())
    }
}

fn host(store: &Shared, runner: ChildRunner) -> Delegation {
    let mut options = DelegationOptions::new(runner);
    options.store = Box::new(store.clone());
    Delegation::new(options).0
}

#[test]
fn delegation_w17e_a_kept_contract_reads_back_as_the_spawn_wrote_it() {
    for spawn in [
        json!({"role": "fixer", "contract": {"checks": [
            {"run": "make", "expect": "exit 2", "timeout_ms": 5},
            {"run": "y", "expect": {"stdout_contains": "ok"}},
            {"changed_files_within": ["src/**"]},
            {"no_new_dependencies": true},
            {"files_exist": ["a.txt"]},
        ], "returns": {"type": "object"}}, "hop": {"retry": 2}}),
        json!({"contract": {"checks": [{"run": "t"}]}, "hop": {"review": "look again"}}),
        json!({"role": "planner"}),
    ] {
        let contract = SpawnContract::parse(spawn.as_object().unwrap()).unwrap().unwrap();
        let kept = contract.describe();
        assert_eq!(SpawnContract::from_described(&kept).unwrap(), contract, "{kept}");
    }
    assert_eq!(
        SpawnContract::from_described(&json!({"checks": [], "bogus": 1})).unwrap_err(),
        "Unknown kept contract field: bogus"
    );
    assert_eq!(
        SpawnContract::from_described(&json!({"checks": [{"run": ""}], "hop": "report"})).unwrap_err(),
        "contract.checks[0].run must be a nonempty string of at most 4000 characters"
    );
}

/// Spawn `options` in a first host whose runner never ends (the worker goes away), then reopen the journal in a
/// second host that reattaches the child with `kept`; the child replies `reply` there. Returns the child's result and
/// what the first host said its contract was.
fn reattached(
    options: Value,
    kept: impl FnOnce(Option<Value>) -> Option<Value>,
    reply: &str,
) -> (Value, Option<Value>) {
    let store = Shared::default();
    let started = Arc::new(Mutex::new(false));
    let seen = started.clone();
    let first = host(
        &store,
        Arc::new(move |run| {
            *seen.lock().unwrap() = true;
            run.signal.wait(HANG);
            Err("the first worker went away".into())
        }),
    );
    let spawned = first
        .handle("rlm.spawn", json!({"prompt": "fix it", "kwargs": options}), "main")
        .unwrap();
    let id = spawned["rlm_child_id"].as_str().unwrap().to_string();
    let described = first.contract_of(&id);
    // The journal as the first worker left it: the child unfinished.
    let unfinished = vec![id.clone()];
    let second = host(&store, Arc::new(|_| Err("no retry run in this test".into())));
    let reply = reply.to_string();
    let kept = kept(described.clone());
    let resumed = second
        .restore(&unfinished, |_| {
            Some(Reattached {
                waiter: Box::new({
                    let reply = reply.clone();
                    move |_: Signal| Ok(reply)
                }),
                contract: kept.clone(),
                run: None,
            })
        })
        .unwrap();
    assert_eq!(resumed, vec![id.clone()]);
    let (_, result) = second.wait(&[id], Some(HANG)).pop().unwrap();
    drop(first);
    (result.unwrap(), described)
}

#[test]
fn delegation_w17e_a_reattached_child_is_held_to_its_kept_contract() {
    let options = json!({"name": "c", "role": "fixer", "contract": {"checks": [{"files_exist": ["never-written.txt"]}]}, "hop": {"retry": 1}});
    let (result, described) = reattached(options, |kept| kept, "done");
    assert_eq!(
        described,
        Some(
            json!({"role": "fixer", "checks": [{"files_exist": ["never-written.txt"]}], "returns": null, "hop": {"retry": 1}})
        )
    );
    assert_eq!(result["status"], "failed", "{result}");
    assert_eq!(result["check"]["contract"]["verdict"], "contract_failed", "{result}");
    // No run to repeat: the failure is reported after the one attempt.
    assert_eq!(result["check"]["contract"]["attempts"], 1, "{result}");
    assert_eq!(result["check"]["contract"]["hop"], json!({"retry": 1}), "{result}");
    assert_eq!(
        result["error"], "Contract failed: files_exist [\"never-written.txt\"]: missing: never-written.txt",
        "{result}"
    );
}

#[test]
fn delegation_w17e_a_reattached_child_without_a_kept_contract_ends_unchecked() {
    let (result, described) = reattached(json!({"name": "c"}), |_| None, "done");
    assert_eq!(described, None);
    assert_eq!(result["status"], "succeeded", "{result}");
    assert_eq!(result["check"]["contract"], Value::Null, "{result}");
}

#[test]
fn delegation_w17e_an_unreadable_kept_contract_fails_the_reattached_child() {
    let (result, _) = reattached(json!({"name": "c"}), |_| Some(json!({"checks": "nope"})), "done");
    assert_eq!(result["status"], "failed", "{result}");
    assert_eq!(
        result["error"],
        "Contract failed: the session's kept contract is unreadable: contract.checks must be a list of at most 16 checks",
        "{result}"
    );
}
