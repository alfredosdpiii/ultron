//! `executionMode: "sequential"` and `onUpdate` for REPL tool calls, ported from
//! packages/coding-agent/test/ultron-extension-tools.test.ts ("sequential extension tools from the REPL") with its
//! literal expectations: a gated runner whose calls log `start <name>` and end when the test opens their gate;
//! `seq_*` tools are sequential. Where TypeScript sleeps 30 ms before checking that a call has not started, these
//! tests check the call's queued preview and the start log once the preceding call has started.

use kernel::Signal;
use kernel_apis::tool_calls::{QUEUED_PREVIEW, ToolUpdate};
use kernel_apis::{MemoryDocumentStore, ToolCalls, ToolInfo, ToolRunner};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const HANG: Duration = Duration::from_secs(120);

#[derive(Default)]
struct Gated {
    log: Mutex<Vec<String>>,
    /// Open gates by tool name (a call ends once its gate is open).
    open: Mutex<HashMap<String, usize>>,
    changed: Condvar,
    /// Progress texts each call reports before it waits (`onUpdate`).
    updates: Vec<String>,
}

impl Gated {
    fn open(&self, name: &str) {
        *self.open.lock().unwrap().entry(name.to_string()).or_default() += 1;
        self.changed.notify_all();
    }

    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
}

impl ToolRunner for Gated {
    fn tools(&self) -> Vec<ToolInfo> {
        ["seq_a", "seq_b", "par_a", "par_b"]
            .into_iter()
            .map(|name| ToolInfo {
                name: name.into(),
                label: None,
                description: name.into(),
                parameters: json!({"type": "object", "properties": {}}),
                execution_mode: name.starts_with("seq").then(|| "sequential".to_string()),
            })
            .collect()
    }

    fn execute(&self, name: &str, id: &str, params: &Map<String, Value>, cancel: &Signal) -> Result<Value, String> {
        self.execute_with_updates(name, id, params, cancel, &|_: &Value| {})
    }

    fn execute_with_updates(
        &self,
        name: &str,
        _id: &str,
        _params: &Map<String, Value>,
        cancel: &Signal,
        on_update: ToolUpdate<'_>,
    ) -> Result<Value, String> {
        self.log.lock().unwrap().push(format!("start {name}"));
        for text in &self.updates {
            on_update(&json!({"content": [{"type": "text", "text": text}]}));
        }
        let mut open = self.open.lock().unwrap();
        loop {
            if let Some(count) = open.get_mut(name).filter(|c| **c > 0) {
                *count -= 1;
                break;
            }
            if cancel.is_aborted() {
                return Err("aborted".into());
            }
            open = self.changed.wait_timeout(open, Duration::from_millis(20)).unwrap().0;
        }
        drop(open);
        self.log.lock().unwrap().push(format!("end {name}"));
        Ok(json!({"content": [{"type": "text", "text": name}]}))
    }
}

fn until(what: &str, check: impl Fn() -> bool) {
    let start = Instant::now();
    while !check() {
        assert!(start.elapsed() < HANG, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn record(calls: &ToolCalls, id: &str) -> Value {
    calls.list().into_iter().find(|r| r["id"] == id).unwrap()
}

fn setup(updates: Vec<String>) -> (Arc<Gated>, ToolCalls) {
    let runner = Arc::new(Gated {
        updates,
        ..Gated::default()
    });
    let calls = ToolCalls::new(
        Some(runner.clone() as Arc<dyn ToolRunner>),
        Arc::new(MemoryDocumentStore::default()),
    );
    (runner, calls)
}

#[test]
fn kernel_apis_sequential_tool_waits_for_lane_calls_and_runs_alone() {
    let (runner, calls) = setup(Vec::new());
    let par_a = calls.start("main", None, "par_a", Map::new()).unwrap();
    let seq_a = calls.start("main", None, "seq_a", Map::new()).unwrap();
    let seq_b = calls.start("main", None, "seq_b", Map::new()).unwrap();
    let par_b = calls.start("main", None, "par_b", Map::new()).unwrap();
    // Another lane is not held up by this lane's sequential call.
    let other = calls.start("child", None, "par_b", Map::new()).unwrap();
    until("two calls started", || runner.log().len() == 2);
    let mut started = runner.log();
    started.sort();
    assert_eq!(started, ["start par_a", "start par_b"]);
    assert_eq!(record(&calls, &seq_a)["preview"], QUEUED_PREVIEW);
    assert!(QUEUED_PREVIEW.starts_with("queued:"));
    runner.open("par_b"); // the other lane's call
    until("the other lane's call ended", || {
        record(&calls, &other)["status"] == "completed"
    });
    assert!(!runner.log().contains(&"start seq_a".to_string()));
    runner.open("par_a");
    until("seq_a started", || runner.log().contains(&"start seq_a".to_string()));
    assert!(!runner.log().contains(&"start seq_b".to_string()));
    assert_eq!(record(&calls, &seq_b)["preview"], QUEUED_PREVIEW);
    assert!(record(&calls, &seq_a).get("preview").is_none());
    runner.open("seq_a");
    until("seq_b started", || runner.log().contains(&"start seq_b".to_string()));
    assert_eq!(runner.log().iter().filter(|l| *l == "start par_b").count(), 1);
    runner.open("seq_b");
    until("par_b started", || {
        runner.log().iter().filter(|l| *l == "start par_b").count() == 2
    });
    runner.open("par_b");
    for id in [&par_a, &seq_a, &seq_b, &par_b, &other] {
        until("every call ended", || record(&calls, id)["status"] != "running");
    }
    let statuses: Vec<Value> = [&par_a, &seq_a, &seq_b, &par_b, &other]
        .iter()
        .map(|id| record(&calls, id)["status"].clone())
        .collect();
    assert_eq!(statuses, vec![json!("completed"); 5]);
    assert_eq!(record(&calls, &seq_a)["preview"], "seq_a");
    // The order the lane ran its calls in: par_a alone (par_b waits behind seq_a), seq_a, seq_b, then par_b.
    let main: Vec<String> = runner
        .log()
        .into_iter()
        .filter(|l| l.starts_with("start"))
        .skip(2)
        .collect();
    assert_eq!(main, ["start seq_a", "start seq_b", "start par_b"]);
    calls.close();
}

#[test]
fn kernel_apis_call_cancelled_while_queued_never_starts() {
    let (runner, calls) = setup(Vec::new());
    let seq_a = calls.start("main", None, "seq_a", Map::new()).unwrap();
    let seq_b = calls.start("main", None, "seq_b", Map::new()).unwrap();
    let par_a = calls.start("main", None, "par_a", Map::new()).unwrap();
    until("seq_a started", || runner.log().len() == 1);
    assert_eq!(runner.log(), ["start seq_a"]);
    calls.cancel(&seq_b, "cancelled");
    assert_eq!(record(&calls, &seq_b)["status"], "cancelled");
    assert!(record(&calls, &seq_b).get("preview").is_none());
    runner.open("seq_a");
    until("par_a started", || runner.log().contains(&"start par_a".to_string()));
    runner.open("par_a");
    for id in [&seq_a, &par_a] {
        until("the calls ended", || record(&calls, id)["status"] == "completed");
    }
    assert!(!runner.log().contains(&"start seq_b".to_string()));
    calls.close();
}

/// `onUpdate(partial)`: the running call's latest progress text is its preview (`resultText`, previewed); the
/// result replaces it at the end.
#[test]
fn kernel_apis_tool_progress_becomes_the_running_preview() {
    let (runner, calls) = setup(vec!["step 1".into(), "step  2\n of 3".into()]);
    let id = calls.start("main", None, "par_a", Map::new()).unwrap();
    until("the progress arrived", || {
        record(&calls, &id)["preview"] == "step 2 of 3"
    });
    assert_eq!(record(&calls, &id)["status"], "running");
    runner.open("par_a");
    until("the call ended", || record(&calls, &id)["status"] == "completed");
    assert_eq!(record(&calls, &id)["preview"], "par_a");
    calls.close();
}
