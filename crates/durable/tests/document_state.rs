//! `Session::document_state`: replays fixtures/document-state.json (upstream `Session.documentState` driven by
//! step scripts, recorded by fixtures/generate-document-state.mjs) against the Rust Session on every backend,
//! comparing what each step observed and every subscriber's deliveries.
//!
//! Upstream subscribers with a synchronous callback run inside the publishing commit; Rust subscribers run on their
//! own threads, so the replay waits for every unblocked subscriber to go idle after each commit (and at `flush`)
//! before the next step. A blocked subscriber's first callback waits for its `release` step.

mod session_support;

use chord_delta::path;
use durable::{Doc, DocDefinition, DocSemantics, DocumentState, Owner, StateSubscription};
use serde_json::{Value, json};
use session_support::*;
use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

const HANG_GUARD: Duration = Duration::from_secs(120);

fn docs() -> HashMap<&'static str, Doc> {
    let session_doc = |kind: &str, version: u64, initial: Value| {
        DocDefinition::new(kind, version, DocSemantics::Session, move |_| initial.clone())
    };
    HashMap::from([
        (
            "notes",
            session_doc("state.notes", 1, json!({ "text": "", "count": 0 }))
                .define()
                .unwrap(),
        ),
        (
            "v1",
            session_doc("cache.versioned", 1, json!({ "name": "first" }))
                .define()
                .unwrap(),
        ),
        (
            "v2",
            session_doc("cache.versioned", 2, json!({ "names": [] }))
                .migrate(|value, _| Ok(json!({ "names": [value["name"]] })))
                .define()
                .unwrap(),
        ),
    ])
}

/// A one-shot gate: `wait` blocks until `open`; `entered` reports that a waiter arrived.
#[derive(Clone, Default)]
struct Latch(Arc<(Mutex<(bool, bool)>, Condvar)>);

impl Latch {
    fn wait(&self) {
        let (lock, cv) = &*self.0;
        let mut state = lock.lock().unwrap();
        state.1 = true;
        cv.notify_all();
        let _guard = cv.wait_timeout_while(state, HANG_GUARD, |s| !s.0).unwrap();
    }

    fn wait_entered(&self) {
        let (lock, cv) = &*self.0;
        let state = lock.lock().unwrap();
        let (state, _) = cv.wait_timeout_while(state, HANG_GUARD, |s| !s.1).unwrap();
        assert!(state.1, "the blocked subscriber never ran");
    }

    fn open(&self) {
        let (lock, cv) = &*self.0;
        lock.lock().unwrap().0 = true;
        cv.notify_all();
    }
}

/// One subscriber's deliveries as `{kind, sequence, value}`.
type Received = Arc<Mutex<Vec<Value>>>;

struct Sub {
    subscription: StateSubscription,
    gate: Option<Latch>,
    blocked: bool,
}

fn observed(value: &durable::Observed) -> Value {
    value.as_deref().cloned().unwrap_or(Value::Null)
}

/// Run one scenario's steps; (trace, deliveries) as the generator records them.
fn run(t: &TestSession, steps: &[Value]) -> (Vec<Value>, HashMap<String, Received>) {
    let docs = docs();
    let mut states: HashMap<String, DocumentState> = HashMap::new();
    let mut subs: HashMap<String, Sub> = HashMap::new();
    let mut deliveries: HashMap<String, Received> = HashMap::new();
    let mut trace = Vec::new();
    let idle = |subs: &HashMap<String, Sub>| {
        for sub in subs.values().filter(|sub| !sub.blocked) {
            assert!(sub.subscription.wait_idle(HANG_GUARD), "a subscriber never went idle");
        }
    };
    for step in steps {
        let s = |key: &str| step[key].as_str().unwrap_or_default().to_string();
        let doc = || &docs[step["doc"].as_str().unwrap()];
        match step["op"].as_str().unwrap() {
            op @ ("set" | "push" | "touch") => {
                t.session
                    .commit(|tx| {
                        let draft = tx.doc(doc(), Owner::Session)?;
                        match op {
                            "set" => draft.set(&path![s("field").as_str()], step["value"].clone())?,
                            "push" => draft
                                .push(&path![s("field").as_str()], vec![step["value"].clone()])
                                .map(drop)?,
                            _ => {}
                        }
                        Ok(())
                    })
                    .unwrap();
                idle(&subs);
            }
            "retire" => {
                t.session.commit(|tx| tx.retire_doc(doc(), Owner::Session)).unwrap();
                idle(&subs);
            }
            "state" => match t.session.document_state(doc(), Owner::Session).unwrap() {
                None => trace.push(json!({ "state": s("name"), "value": "undefined" })),
                Some(state) => {
                    states.insert(s("name"), state);
                }
            },
            "value" => trace.push(json!({ "state": s("state"), "value": observed(&states[&s("state")].value()) })),
            op @ ("subscribe" | "subscribeBlocked") => {
                let received: Received = Arc::default();
                let gate = (op == "subscribeBlocked").then(Latch::default);
                let (sink, listener_gate) = (received.clone(), gate.clone());
                let subscription = states[&s("state")].subscribe(move |value, delivery| {
                    let first = {
                        let mut received = sink.lock().unwrap();
                        received.push(json!({
                            "kind": delivery.kind.as_str(),
                            "sequence": delivery.sequence,
                            "value": observed(value),
                        }));
                        received.len() == 1
                    };
                    if first && let Some(gate) = &listener_gate {
                        gate.wait();
                    }
                });
                if let Some(gate) = &gate {
                    // Upstream's hydration callback starts inside `subscribe`.
                    gate.wait_entered();
                }
                let blocked = gate.is_some();
                subs.insert(
                    s("name"),
                    Sub {
                        subscription,
                        gate,
                        blocked,
                    },
                );
                deliveries.insert(s("name"), received);
                idle(&subs);
            }
            "release" => {
                let sub = subs.get_mut(&s("name")).unwrap();
                sub.gate.as_ref().unwrap().open();
                sub.blocked = false;
            }
            "unsubscribe" => subs[&s("name")].subscription.unsubscribe(),
            "dispose" => states[&s("state")].dispose(),
            "close" => t.session.close().unwrap(),
            "stateError" => trace.push(match t.session.document_state(doc(), Owner::Session) {
                Ok(_) => json!({ "error": null }),
                Err(error) => json!({ "error": error.to_string() }),
            }),
            "flush" => idle(&subs),
            other => panic!("unknown step {other}"),
        }
    }
    idle(&subs);
    (trace, deliveries)
}

#[test]
fn durable_document_state_replays_upstream_scenarios() {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/document-state.json")).unwrap();
    let scenarios = fixture["scenarios"].as_array().unwrap();
    assert_eq!(scenarios.len(), 9);
    each_backend(|backend| {
        for scenario in scenarios {
            let name = scenario["name"].as_str().unwrap();
            let t = TestSession::open(backend);
            let (trace, deliveries) = run(&t, scenario["steps"].as_array().unwrap());
            assert_eq!(Value::Array(trace), scenario["trace"], "{name}: trace");
            let want = scenario["deliveries"].as_object().unwrap();
            assert_eq!(deliveries.len(), want.len(), "{name}: subscribers");
            for (sub, expected) in want {
                let got = Value::Array(deliveries[sub].lock().unwrap().clone());
                assert_eq!(&got, expected, "{name}: deliveries of {sub}");
            }
        }
    });
}

/// `documentState` of a family member: absent until created, then hydrated and following that member only.
#[test]
fn durable_document_state_member_follows_one_member() {
    each_backend(|backend| {
        let t = TestSession::open(backend);
        let family = DocDefinition::new("state.family", 1, DocSemantics::Session, |_| json!({ "n": 0 }))
            .family()
            .define()
            .unwrap();
        assert!(
            t.session
                .document_state_member(&family, Owner::Session, "a")
                .unwrap()
                .is_none()
        );
        for key in ["a", "b"] {
            t.session
                .commit(|tx| tx.doc_member(&family, Owner::Session, key, json!(null)).map(drop))
                .unwrap();
        }
        let state = t
            .session
            .document_state_member(&family, Owner::Session, "a")
            .unwrap()
            .unwrap();
        t.session
            .commit(|tx| {
                Ok(tx
                    .doc_member(&family, Owner::Session, "b", json!(null))?
                    .set(&path!["n"], json!(5))?)
            })
            .unwrap();
        assert_eq!(*state.value().unwrap(), json!({ "n": 0 }));
        t.session
            .commit(|tx| {
                Ok(tx
                    .doc_member(&family, Owner::Session, "a", json!(null))?
                    .set(&path!["n"], json!(1))?)
            })
            .unwrap();
        assert_eq!(*state.value().unwrap(), json!({ "n": 1 }));
    });
}
