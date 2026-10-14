//! A21: replay does not duplicate external effects; uncertainty stays explicit. Both cases of
//! packages/coding-agent/test/ultron-a21-lost-response-replay.test.ts with their literal expectations, over the Rust
//! typed-agent host (`delegation::agents::Agents` through `agents_support`): its task journal, its durable definition
//! registry and its usage ledger are the real ones, each on a durable document with owner fencing (`durable()` of
//! the TS test: a newer owner fences every older one, and `crash_on_write` kills an owner at a chosen write).
//!
//! The TS payment-like service is an HTTP server reached through `fetch`; here it is an in-process service with the
//! same contract (every request recorded, each idempotency key applied once, and in lose-response mode the effect
//! is committed while the reply is held until the service closes or the caller aborts). The deterministic adapter
//! calls it with the task's abort signal, as the TS adapter passes `signal` to `fetch`.

mod agents_support;

use agents_support::{Fixture, Options, adapter, fixture_with_stores, lock, wait_for};
use delegation::task_store::TaskStore;
use delegation::usage::{UsageLedger, UsageOptions};
use kernel::Signal;
use serde_json::{Value, json};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// The hang guard of every wait (never a timing expectation).
const HANG: Duration = Duration::from_secs(120);

/// Kills an owner on a write: `(next document, the owner's write count)`.
type Crash = Arc<dyn Fn(&Value, usize) -> bool + Send + Sync>;

#[derive(Default)]
struct DurableState {
    document: Option<Value>,
    generation: u64,
}

/// One durable document with owner fencing (`durable()`); clones share it.
#[derive(Clone, Default)]
struct Durable(Arc<Mutex<DurableState>>);

impl Durable {
    fn document(&self) -> Option<Value> {
        lock(&self.0).document.clone()
    }

    /// `open({crashOnWrite})`: a new owner; every earlier one is fenced.
    fn open(&self, crash: Option<Crash>) -> Owner {
        let mut state = lock(&self.0);
        state.generation += 1;
        Owner {
            durable: self.clone(),
            mine: state.generation,
            writes: 0,
            dead: false,
            crash,
        }
    }
}

struct Owner {
    durable: Durable,
    mine: u64,
    writes: usize,
    dead: bool,
    crash: Option<Crash>,
}

impl TaskStore for Owner {
    fn read(&mut self) -> Result<Option<Value>, String> {
        Ok(self.durable.document())
    }
    fn write(&mut self, next: &Value) -> Result<(), String> {
        self.writes += 1;
        if !self.dead && self.crash.as_ref().is_some_and(|crash| crash(next, self.writes)) {
            self.dead = true;
        }
        let mut state = lock(&self.durable.0);
        if self.dead || self.mine != state.generation {
            return Err("owner is gone".into());
        }
        state.document = Some(next.clone());
        Ok(())
    }
}

#[derive(Default)]
struct ServiceState {
    requests: Vec<(String, u64)>,
    effects: Vec<(String, Value)>,
    lose_response: bool,
    closed: bool,
}

/// A disposable payment-like service (`fakeService()`): it records every request and applies each idempotency key
/// once; in lose-response mode the effect is committed but the reply never reaches the caller.
#[derive(Clone, Default)]
struct Service(Arc<(Mutex<ServiceState>, Condvar)>);

impl Service {
    fn post(&self, key: &str, amount: u64, signal: &Signal) -> Result<Value, String> {
        let mut state = lock(&self.0.0);
        state.requests.push((key.into(), amount));
        let effect = match state.effects.iter().find(|(known, _)| known == key) {
            Some((_, effect)) => effect.clone(),
            None => {
                let effect = json!({ "charge": format!("charge-{}", state.effects.len() + 1), "amount": amount });
                state.effects.push((key.into(), effect.clone()));
                effect
            }
        };
        if !state.lose_response {
            return Ok(effect);
        }
        // The reply is held: the caller sees only a hang, ended by the service closing or its own abort.
        loop {
            if state.closed {
                return Err("socket hang up".into());
            }
            if signal.is_aborted() {
                return Err("This operation was aborted".into());
            }
            state = self
                .0
                .1
                .wait_timeout(state, Duration::from_millis(10))
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }

    fn set_lose_response(&self, lose: bool) {
        lock(&self.0.0).lose_response = lose;
    }

    fn request_keys(&self) -> Vec<String> {
        lock(&self.0.0).requests.iter().map(|(key, _)| key.clone()).collect()
    }

    fn effect_count(&self) -> usize {
        lock(&self.0.0).effects.len()
    }

    fn close(&self) {
        lock(&self.0.0).closed = true;
        self.0.1.notify_all();
    }
}

fn definition() -> Value {
    json!({
        "id": "charge-card",
        "version": "1",
        "strategy": "deterministic",
        "instructions": "Charge the card once using the supplied idempotency key.",
        "inputSchema": {
            "type": "object",
            "properties": { "amount": { "type": "integer" }, "idempotency_key": { "type": "string" } },
            "required": ["amount", "idempotency_key"],
            "additionalProperties": false,
        },
        "outputSchema": {
            "type": "object",
            "properties": { "charge": { "type": "string" }, "amount": { "type": "integer" } },
            "required": ["charge", "amount"],
        },
        "maxRepairs": 0,
        "inputDescription": "{amount, idempotency_key}",
        "outputDescription": "{charge, amount}",
    })
}

/// The service, the three durable stores, the adapter call count, and every owner opened (closed at the end).
struct Setup {
    service: Service,
    tasks: Durable,
    usage: Durable,
    definitions: Durable,
    adapter_calls: Arc<AtomicUsize>,
    owners: RefCell<Vec<Rc<Fixture>>>,
}

impl Setup {
    fn new() -> Setup {
        Setup {
            service: Service::default(),
            tasks: Durable::default(),
            usage: Durable::default(),
            definitions: Durable::default(),
            adapter_calls: Arc::default(),
            owners: RefCell::default(),
        }
    }

    /// `owner(taskOptions)`: a new host over the same stores; it fences every earlier owner.
    fn owner(&self, crash: Option<Crash>) -> Rc<Fixture> {
        let usage = UsageLedger::new(Box::new(self.usage.open(None)), UsageOptions::default()).unwrap();
        let (service, calls) = (self.service.clone(), self.adapter_calls.clone());
        let options = Options {
            usage: Some(usage),
            deterministic: Some(adapter(move |_, input, _, signal| {
                calls.fetch_add(1, Ordering::SeqCst);
                let amount = input["amount"].as_u64().unwrap();
                let key = input["idempotency_key"].as_str().unwrap();
                service.post(key, amount, signal)
            })),
            ..Options::default()
        };
        let fixture = Rc::new(fixture_with_stores(
            options,
            Box::new(self.tasks.open(crash)),
            Box::new(self.definitions.open(None)),
        ));
        self.owners.borrow_mut().push(fixture.clone());
        fixture
    }

    fn adapter_calls(&self) -> usize {
        self.adapter_calls.load(Ordering::SeqCst)
    }
}

impl Drop for Setup {
    fn drop(&mut self) {
        self.service.close();
        for owner in self.owners.borrow_mut().drain(..).rev() {
            owner.agents.close();
        }
    }
}

fn charge(key: &str, amount: u64) -> Value {
    json!({
        "definition": "charge-card@1",
        "input": { "amount": amount, "idempotency_key": key },
        "key": format!("task:{key}"),
    })
}

fn interrupted() -> Value {
    json!({
        "status": "interrupted",
        "error": "Owner ended; automatic replay is disabled",
        "verification": "unverified",
    })
}

fn task_states(document: &Value) -> Vec<(String, String)> {
    document["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| (t["id"].as_str().unwrap().into(), t["state"].as_str().unwrap().into()))
        .collect()
}

#[test]
fn acceptance_a21_a_lost_response_followed_by_an_owner_crash_is_not_replayed_and_the_same_key_returns_the_durable_interrupted_result()
 {
    let fixture = Setup::new();
    let first = fixture.owner(None);
    first
        .call("agents.register", json!({ "definition": definition() }))
        .unwrap();
    fixture.service.set_lose_response(true);
    let spawned = first.call("agents.spawn", charge("order-1", 500)).unwrap();
    let id = spawned["id"].as_str().unwrap().to_string();
    wait_for("the charge request", HANG, || fixture.service.request_keys().len() == 1);
    assert_eq!(
        task_states(&fixture.tasks.document().unwrap()),
        [(id.clone(), "running".to_string())]
    );

    // Owner crash: the effect happened, its response never arrived, and nothing terminal was committed.
    fixture.service.set_lose_response(false);
    let restarted = fixture.owner(None);
    let inspected = restarted.call("agents.inspect", json!({ "id": id })).unwrap();
    assert_eq!(inspected["state"], "interrupted", "{inspected}");
    assert_eq!(inspected["result"], interrupted(), "{inspected}");
    // Restart alone replays nothing.
    assert_eq!(fixture.service.request_keys().len(), 1);

    // Re-invoking with the same key is a lookup of the durable outcome, not a second attempt.
    let again = restarted.call("agents.invoke", charge("order-1", 500)).unwrap();
    assert_eq!(again, interrupted());
    assert_eq!(restarted.call("agents.result", json!({ "id": id })).unwrap(), again);
    let reused = restarted.call("agents.invoke", charge("order-1", 900)).unwrap_err();
    assert!(reused.contains("Idempotency key reused"), "{reused}");
    assert_eq!(fixture.service.request_keys().len(), 1);
    assert_eq!(fixture.service.effect_count(), 1);
    assert_eq!(fixture.adapter_calls(), 1);

    // The uncertainty is explicit in accounting too: the ended owner's reservation settles as unknown.
    let status = restarted.call("agents.status", json!({})).unwrap();
    assert_eq!(status["usage"]["activeReservations"], 0, "{status}");
    let ledger = fixture.usage.document().unwrap();
    let roots = ledger["roots"].as_object().unwrap();
    let calls = roots.values().next().unwrap()["calls"].as_array().unwrap();
    assert_eq!(calls.len(), 1, "{ledger}");
    assert_eq!(calls[0]["kind"], "task");
    assert_eq!(calls[0]["requestKey"], "task:order-1");
    assert_eq!(calls[0]["status"], "unknown");
}

#[test]
fn acceptance_a21_crashes_before_launch_after_the_effects_response_and_after_commit_never_duplicate_the_effect() {
    let fixture = Setup::new();
    let setup_owner = fixture.owner(None);
    setup_owner
        .call("agents.register", json!({ "definition": definition() }))
        .unwrap();

    // Boundary 1: admission committed, owner dies before the task is marked running.
    let before_launch = fixture.owner(Some(Arc::new(|next: &Value, _| {
        next["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|task| task["state"] == "running")
    })));
    let spawned = before_launch.call("agents.spawn", charge("order-2", 500)).unwrap();
    // The owner's end is observed through its own result (TS: a 20 ms pause): its journal refuses further work
    // (`task-store.ts` DURABILITY_ERROR) and the launch never happened.
    let ended = before_launch.call("agents.result", json!({ "id": spawned["id"] }));
    assert_eq!(
        ended,
        Err("Task store durability is uncertain; reopen the owner".to_string())
    );
    assert!(fixture.service.request_keys().is_empty());
    assert_eq!(fixture.adapter_calls(), 0);

    // Boundary 2: the service responded, but the owner died before committing the terminal result.
    let before_commit = fixture.owner(Some(Arc::new(|next: &Value, _| {
        next["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|task| task["key"] == "task:order-3" && task["state"] == "completed")
    })));
    let lost = before_commit
        .call("agents.invoke", charge("order-3", 500))
        .unwrap_or_else(|error| json!({ "status": format!("rejected: {error}") }));
    assert_ne!(lost["status"], "succeeded", "{lost}");
    assert_eq!(fixture.service.request_keys(), ["order-3"]);

    // Boundary 3: everything committed, but the caller lost the response and retries.
    let committed = fixture.owner(None);
    let success = committed.call("agents.invoke", charge("order-4", 500)).unwrap();
    assert_eq!(success["status"], "succeeded", "{success}");
    assert_eq!(success["value"], json!({ "charge": "charge-2", "amount": 500 }));

    let restarted = fixture.owner(None);
    let replayed = |key: &str| restarted.call("agents.invoke", charge(key, 500)).unwrap();
    assert_eq!(replayed("order-2")["status"], "interrupted");
    assert_eq!(replayed("order-3")["status"], "interrupted");
    assert_eq!(replayed("order-4"), success);
    assert_eq!(fixture.service.request_keys(), ["order-3", "order-4"]);
    assert_eq!(fixture.adapter_calls(), 2);

    // Retrying an uncertain effect is an explicit new request; the service's own key still dedupes it.
    let mut retry = charge("order-3", 500);
    retry["key"] = json!("task:order-3:retry-1");
    let explicit_retry = restarted.call("agents.invoke", retry).unwrap();
    assert_eq!(explicit_retry["status"], "succeeded", "{explicit_retry}");
    assert_eq!(explicit_retry["value"], json!({ "charge": "charge-1", "amount": 500 }));
    assert_eq!(fixture.service.effect_count(), 2);
    let status = restarted.call("agents.status", json!({})).unwrap();
    let states: Vec<&Value> = status["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|task| &task["state"])
        .collect();
    assert_eq!(
        states,
        [
            &json!("interrupted"),
            &json!("interrupted"),
            &json!("completed"),
            &json!("completed")
        ]
    );
}
