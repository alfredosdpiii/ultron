//! Replays crates/kernel-apis/fixtures/goal.json, recorded by fixtures/generate-goal.mjs from the TypeScript
//! GoalDriver (ultron/goal.ts) served by NativeRlmHost to the real runtime.py: the same cells (model `goal.*` calls,
//! the person's `/goal` commands, task-end events and calls from other lanes) run through crates/kernel with the Rust
//! `GoalDriver`, with the same scripted worker (checks, job starts and stops), and must print the same and leave the
//! same goal document and worker calls. Plus the pure functions over the recorded table, and ported TS tests.

mod common;

use common::{FIXED_NOW, Made, fixed, fixture, run_scenario};
use kernel::{HostHandler, Signal};
use kernel_apis::goal::{GoalCheckResult, GoalDriver, GoalJobs, check_signature, goal_brief, goal_status_text};
use kernel_apis::util::lock;
use kernel_apis::{DocumentStore, KernelApis, MemoryDocumentStore};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

fn result_of(value: &Value) -> GoalCheckResult {
    GoalCheckResult {
        exit_code: value["exit_code"].as_i64(),
        timed_out: value["timed_out"] == true,
        output: value["output"].as_str().unwrap().to_string(),
    }
}

fn scripted(output: &str, exit_code: Option<i64>, timed_out: bool) -> GoalCheckResult {
    GoalCheckResult {
        exit_code,
        timed_out,
        output: output.into(),
    }
}

/// generate-goal.mjs CHECKS and its fake worker.
#[derive(Default)]
struct Worker {
    calls: Mutex<Vec<Value>>,
    used: Mutex<HashMap<String, usize>>,
    jobs: Mutex<u64>,
}

impl Worker {
    fn checks(command: &str) -> Vec<GoalCheckResult> {
        let fail = scripted("1 failed in 0.31s", Some(1), false);
        let pass = scripted("3 passed in 0.12s", Some(0), false);
        match command {
            "pytest -q" => vec![fail.clone(), fail, scripted("2 failed in 0.40s", Some(1), false), pass],
            "bench" => {
                let mut list = vec![scripted("slow: 812ms at 0x7f3a", Some(1), false); 10];
                list.push(scripted("slow: 640ms", Some(1), false));
                list
            }
            "long-output" => vec![scripted(&format!("{}é end", "y".repeat(2500)), Some(2), false)],
            "hang" => vec![scripted("", None, true)],
            _ => vec![pass],
        }
    }
}

impl GoalJobs for Worker {
    fn run_check(&self, command: &str, timeout_seconds: u64) -> Result<GoalCheckResult, String> {
        lock(&self.calls).push(json!(["check", command, timeout_seconds]));
        let list = Self::checks(command);
        let mut used = lock(&self.used);
        let count = used.entry(command.to_string()).or_default();
        let result = list[(*count).min(list.len() - 1)].clone();
        *count += 1;
        Ok(result)
    }

    fn start_job(&self, prompt: &str, key: &str, usage_root: &str, _: &Signal) -> Result<String, String> {
        lock(&self.calls).push(json!(["start", key, usage_root, prompt]));
        if prompt.contains("unstartable") {
            return Err("no capacity for a background job".into());
        }
        let mut jobs = lock(&self.jobs);
        *jobs += 1;
        Ok(format!("task-{jobs}"))
    }

    fn stop_job(&self, task_id: &str, reason: &str) -> Result<(), String> {
        lock(&self.calls).push(json!(["stop", task_id, reason]));
        Ok(())
    }

    fn lane_of_job(&self, task_id: &str) -> Option<String> {
        Some(format!("ultron.background-job.{task_id}"))
    }
}

fn goal_host(_: &Path) -> Made {
    let store = Arc::new(MemoryDocumentStore::default());
    let worker = Arc::new(Worker::default());
    let driver = Arc::new(GoalDriver::new(store.clone(), worker.clone(), fixed(FIXED_NOW)));
    let test = driver.clone();
    let inner: Arc<dyn HostHandler> =
        Arc::new(
            move |kind: &str, payload: Map<String, Value>, signal: &Signal| match kind {
                "test.goal.command" => Ok(json!({"text": test.command(payload["args"].as_str().unwrap(), signal)?})),
                "test.goal.ended" => {
                    test.task_ended(payload["id"].as_str().unwrap(), payload.get("result"))?;
                    Ok(Value::Null)
                }
                "test.goal.as" => test.handle_request(
                    payload["type"].as_str().unwrap(),
                    payload["payload"].as_object().unwrap(),
                    payload["lane"].as_str().unwrap(),
                ),
                _ => Err(format!("Ultron RLM host request is not wired: {kind}")),
            },
        );
    Made {
        handler: KernelApis::new(inner).with(driver),
        env: Vec::new(),
        state: Box::new(move || json!({"document": store.read().unwrap(), "calls": *lock(&worker.calls)})),
    }
}

fn goal_ids(value: &Value) -> Value {
    let text = serde_json::to_string(value).unwrap();
    let pattern = fancy_regex::Regex::new("goal-[0-9a-f]{8}").unwrap();
    serde_json::from_str(&pattern.replace_all(&text, "goal-ID")).unwrap()
}

#[test]
fn kernel_apis_goal_cells_match_typescript_host() {
    let fixture = fixture("goal");
    for scenario in fixture["scenarios"].as_array().unwrap() {
        let actual = goal_ids(&run_scenario(scenario, &goal_host));
        for (index, expected) in scenario["cells"].as_array().unwrap().iter().enumerate() {
            assert_eq!(actual["cells"][index], *expected, "{} / cell {index}", scenario["name"]);
        }
        assert_eq!(
            actual["state"]["calls"], scenario["state"]["calls"],
            "{} / calls",
            scenario["name"]
        );
        assert_eq!(
            actual["state"]["document"], scenario["state"]["document"],
            "{} / document",
            scenario["name"]
        );
        assert_eq!(actual, *scenario, "{}", scenario["name"]);
    }
}

#[test]
fn kernel_apis_goal_pure_functions_match_typescript() {
    let pure = &fixture("goal")["pure"];
    for case in pure["signatures"].as_array().unwrap() {
        assert_eq!(
            check_signature(&result_of(&case["result"])),
            case["signature"],
            "{case}"
        );
    }
    for case in pure["briefs"].as_array().unwrap() {
        assert_eq!(goal_brief(&case["goal"]), case["brief"], "{case}");
    }
    for case in pure["statuses"].as_array().unwrap() {
        let goal = Some(&case["goal"]).filter(|goal| !goal.is_null());
        assert_eq!(goal_status_text(goal), case["text"], "{case}");
    }
}

/// ultron-goal.test.ts "check signatures ignore durations and addresses, not counts or exit codes".
#[test]
fn kernel_apis_goal_check_signature_ignores_durations_and_addresses() {
    let fail = |output: &str| scripted(output, Some(1), false);
    assert_eq!(
        check_signature(&fail("1 failed in 0.31s at 0x7f00")),
        check_signature(&fail("1 failed in 12.5s at 0x1234"))
    );
    assert_ne!(check_signature(&fail("1 failed")), check_signature(&fail("2 failed")));
    assert_ne!(
        check_signature(&scripted("1 failed in 0.31s", Some(2), false)),
        check_signature(&fail("1 failed in 0.31s"))
    );
    let brief = goal_brief(&json!({"revision": 3, "objective": "o", "check": null, "last_answer": null}));
    assert!(brief.contains("There is no check command"));
}

/// ultron-goal.test.ts: a goal whose job was running loads paused after a restart; clear and status follow.
#[test]
fn kernel_apis_goal_restart_pauses_a_running_job() {
    let store = Arc::new(MemoryDocumentStore::default());
    let signal = Signal::new();
    let first = GoalDriver::new(store.clone(), Arc::new(Worker::default()), fixed(FIXED_NOW));
    first.command("ship the parser", &signal).unwrap();
    assert_eq!(first.get().unwrap().unwrap()["job"], "task-1");
    let again = GoalDriver::new(store, Arc::new(Worker::default()), fixed(FIXED_NOW));
    let goal = again.get().unwrap().unwrap();
    assert_eq!((goal["status"].as_str(), &goal["job"]), (Some("paused"), &Value::Null));
    assert_eq!(
        goal["reason"],
        "Ultron restarted while the goal job ran; /goal resume continues"
    );
    assert_eq!(again.command("clear", &signal).unwrap(), "Goal cleared.");
    assert_eq!(
        again.command("", &signal).unwrap(),
        "No goal. Set one with /goal <objective>."
    );
}
