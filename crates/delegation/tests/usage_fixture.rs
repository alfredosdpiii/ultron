//! Replays fixtures/usage.json (recorded by fixtures/generate-usage.mjs from usage.ts): every scripted ledger
//! session must return the same results and errors and leave the same stored document after every step, as JSON
//! text (key order included); invalid stored documents fail with the same messages; the env limits and the budget
//! line match.

use delegation::task_store::TaskStore;
use delegation::usage::{
    Measurement, Remaining, Reservation, ReserveRequest, UsageLedger, UsageLimits, UsageOptions, budget_line,
    limits_from_env,
};
use serde_json::{Map, Value, json};
use std::path::Path;
use std::sync::{Arc, Mutex};

fn fixture() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/usage.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

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

fn float(value: &Value) -> Option<f64> {
    match value {
        Value::String(text) if text == "NaN" => Some(f64::NAN),
        other => other.as_f64(),
    }
}

fn text(value: &Value) -> Option<String> {
    value.as_str().map(String::from)
}

fn limits(options: &Value) -> UsageLimits {
    let limits = &options["limits"];
    let integer = |key: &str| limits[key].as_u64();
    UsageLimits {
        max_admitted_tasks: integer("maxAdmittedTasks"),
        max_wall_ms: integer("maxWallMs"),
        max_cost_usd: limits["maxCostUsd"].as_f64(),
        max_total_tokens: integer("maxTotalTokens"),
        max_total_turns: integer("maxTotalTurns"),
        default_max_total_tokens: integer("defaultMaxTotalTokens"),
    }
}

fn request(value: &Value) -> ReserveRequest {
    ReserveRequest {
        kind: value["kind"].as_str().unwrap_or_default().into(),
        root_id: text(&value["rootId"]),
        task_id: text(&value["taskId"]),
        parent_task_id: text(&value["parentTaskId"]),
        request_key: text(&value["requestKey"]),
        budget_id: text(&value["budgetId"]),
        timeout_ms: float(&value["timeoutMs"]),
        deadline_at: float(&value["deadlineAt"]),
        model_backed: value["modelBacked"].as_bool(),
    }
}

fn measurement(value: &Value) -> Option<Measurement> {
    value.is_object().then(|| Measurement {
        input_tokens: float(&value["inputTokens"]),
        output_tokens: float(&value["outputTokens"]),
        total_tokens: float(&value["totalTokens"]),
        cost: float(&value["cost"]),
        wall_ms: float(&value["wallMs"]),
    })
}

fn remaining(value: &Remaining) -> Value {
    json!({ "turnsLeft": value.turns_left, "tokensLeft": value.tokens_left, "tokenCap": value.token_cap })
}

fn outcome(result: Result<Value, String>) -> Value {
    match result {
        Ok(value) => json!({ "ok": value }),
        Err(error) => json!({ "error": error }),
    }
}

#[test]
fn delegation_usage_sessions_match_the_typescript_ledger() {
    for session in fixture()["sessions"].as_array().unwrap() {
        let name = session["name"].as_str().unwrap();
        let options = &session["options"];
        let now = Arc::new(Mutex::new(1_000.0));
        let clock = now.clone();
        let counter = Arc::new(Mutex::new(0));
        let refused = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = refused.clone();
        let document = Arc::new(Mutex::new(Some(session["initial"].clone()).filter(|v| !v.is_null())));
        let ledger = UsageLedger::new(
            Box::new(Shared(document.clone())),
            UsageOptions {
                limits: limits(options),
                detailed_roots: options["detailedRoots"].as_u64().map_or(50, |n| n as usize),
                now: Arc::new(move || *clock.lock().unwrap()),
                on_refused: Some(Arc::new(move |reason: &str| sink.lock().unwrap().push(reason.into()))),
                ids: Box::new(move || {
                    let mut n = counter.lock().unwrap();
                    *n += 1;
                    format!("id-{n}")
                }),
                ..UsageOptions::default()
            },
        )
        .unwrap();
        let mut reservations: Vec<Reservation> = Vec::new();
        for op in session["ops"].as_array().unwrap() {
            let step = op["step"].as_array().unwrap();
            let args = &step[1..];
            let root = |i: usize| args.get(i).and_then(Value::as_str);
            let got = match step[0].as_str().unwrap() {
                "advance" => {
                    let mut now = now.lock().unwrap();
                    *now += args[0].as_f64().unwrap();
                    Ok(json!(*now as i64))
                }
                "reserve" => ledger.reserve(&request(&args[0])).map(|r| {
                    reservations.push(r.clone());
                    r.0
                }),
                "settle" => {
                    let reservation = &reservations[args[0].as_u64().unwrap() as usize];
                    let status = args[1]["status"].as_str();
                    ledger
                        .settle(reservation, status, measurement(&args[1]["usage"]).as_ref())
                        .map(|()| Value::Null)
                }
                "status" => ledger.status(root(0)),
                "recordTurn" => ledger
                    .record_turn(root(0), float(&args[1]["totalTokens"]), float(&args[1]["cost"]))
                    .map(|()| Value::Null),
                "exhaustion" => ledger
                    .budget_exhaustion(root(0))
                    .map(|e| e.map_or(Value::Null, |e| json!({ "kind": e.kind, "reason": e.reason }))),
                "remaining" => ledger.remaining(root(0)).map(|r| remaining(&r)),
                "reconcile" => {
                    let active: Vec<String> = args[0].as_array().unwrap().iter().filter_map(text).collect();
                    ledger.reconcile(&active).map(|()| Value::Null)
                }
                "import" => {
                    let calls: Vec<(f64, Measurement, Option<String>)> = args[2]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|c| {
                            (
                                c["at"].as_f64().unwrap(),
                                measurement(&c["usage"]).unwrap(),
                                text(&c["status"]),
                            )
                        })
                        .collect();
                    ledger
                        .import_history(root(0).unwrap(), root(1).unwrap(), &calls)
                        .map(|()| Value::Null)
                }
                "ready" => ledger.ready().map(|()| Value::Null),
                other => panic!("unknown op {other}"),
            };
            let mut want: Map<String, Value> = op.as_object().unwrap().clone();
            want.remove("step");
            let stored = want.remove("document").unwrap();
            assert_eq!(
                outcome(got).to_string(),
                Value::Object(want).to_string(),
                "{name}: {}",
                op["step"]
            );
            let ours = document.lock().unwrap().clone().unwrap_or(Value::Null);
            assert_eq!(
                ours.to_string(),
                stored.to_string(),
                "{name}: document after {}",
                op["step"]
            );
        }
        assert_eq!(json!(*refused.lock().unwrap()), session["refused"], "{name}: refusals");
    }
}

#[test]
fn delegation_usage_invalid_documents_fail_like_typescript() {
    for case in fixture()["invalid"].as_array().unwrap() {
        let document = Arc::new(Mutex::new(Some(case["document"].clone())));
        let ledger = UsageLedger::new(Box::new(Shared(document)), UsageOptions::default()).unwrap();
        let got = outcome(ledger.ready().map(|()| Value::Null));
        let mut want = case.clone();
        want.as_object_mut().unwrap().remove("document");
        assert_eq!(got, want, "{}", case["document"]);
    }
}

#[test]
fn delegation_usage_limits_from_env_and_budget_line_match() {
    let fixture = fixture();
    for case in fixture["env"].as_array().unwrap() {
        let vars = case["vars"].clone();
        let got = limits_from_env(&|key| vars[key].as_str().map(String::from), &case["settings"]);
        let mut want = Map::new();
        let mut put = |key: &str, value: Option<Value>| {
            if let Some(value) = value {
                want.insert(key.into(), value);
            }
        };
        put("maxAdmittedTasks", got.max_admitted_tasks.map(Value::from));
        put("maxWallMs", got.max_wall_ms.map(Value::from));
        put("maxCostUsd", got.max_cost_usd.map(|c| json!(c)));
        put("maxTotalTokens", got.max_total_tokens.map(Value::from));
        put("defaultMaxTotalTokens", got.default_max_total_tokens.map(Value::from));
        put("maxTotalTurns", got.max_total_turns.map(Value::from));
        let expected: Map<String, Value> = case["limits"].as_object().unwrap().clone();
        assert_eq!(Value::Object(want), Value::Object(expected), "{}", case["vars"]);
    }
    for case in fixture["lines"].as_array().unwrap() {
        let r = &case["remaining"];
        let remaining = Remaining {
            turns_left: r["turnsLeft"].as_u64(),
            tokens_left: r["tokensLeft"].as_u64(),
            token_cap: r["tokenCap"].as_u64(),
        };
        let line = budget_line(&remaining, case["explicit"].as_bool().unwrap());
        assert_eq!(json!(line), case["line"], "{r}");
    }
}
