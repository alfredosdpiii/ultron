//! A05 (partial): cancellation terminates owned work and rejects late success.
//! Ported from packages/coding-agent/test/ultron-a05-cancellation-process-tree.test.ts: the real runtime.py under the
//! Rust kernel supervisor, and the Rust task supervisor (crates/harness, the task half of NativeRlmHost). As in the
//! TypeScript test, the "lane" is test code: a thread that runs the task's Python in a kernel, aborted through the
//! supervisor's abort hook, then reports a late provider success. Not ported: the ultron-native-host.test.ts cases
//! that need task timeouts, background jobs, deterministic adapters, workflows and the reviewer verdict check.

mod common;

use common::{alive, kernel_in, null_handler, read_pids, run, wait_until};
use harness::supervisor::{SpawnRequest, TaskSupervisor};
use harness::task_store::TaskDocumentStore;
use kernel::{HostHandler, KernelError, Signal, Status};
use serde_json::{Map, Value, json};
use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::Duration;

/// Python cell: a child with a same-group grandchild and a setsid grandchild, the pids written to `pid_file`, then
/// `wait`.
fn process_tree_cell(pid_file: &Path, wait: &str) -> String {
    let path = serde_json::to_string(pid_file).unwrap();
    [
        "import json, os, subprocess, sys",
        "child_code = '''",
        "import subprocess, sys, time",
        "g1 = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(120)'])",
        "g2 = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(120)'], start_new_session=True)",
        "print(g1.pid, g2.pid, flush=True)",
        "time.sleep(120)",
        "'''",
        "proc = subprocess.Popen([sys.executable, '-c', child_code], stdout=subprocess.PIPE, text=True)",
        "grandchild, detached = map(int, proc.stdout.readline().split())",
        &format!(
            "open({path}, 'w').write(json.dumps({{'kernel': os.getpid(), 'child': proc.pid, 'grandchild': grandchild, 'detached': detached}}))"
        ),
        wait,
        "'finished'",
    ]
    .join("\n")
}

fn all_gone(pids: &[u32]) {
    wait_until("the process tree to end", Duration::from_secs(10), || {
        pids.iter().all(|pid| !alive(*pid))
    });
}

/// A gate the test opens once.
#[derive(Default)]
struct Gate(Mutex<bool>, Condvar);

impl Gate {
    fn open(&self) {
        *self.0.lock().unwrap() = true;
        self.1.notify_all();
    }
    fn wait(&self) {
        drop(self.1.wait_while(self.0.lock().unwrap(), |open| !*open).unwrap());
    }
}

fn cancelling_a_cell_kills_its_tree_and_a_late_reply_does_not_revive_it() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("pids.json");
    let gate = Arc::new(Gate::default());
    let (signals, host_signal) = mpsc::channel::<Signal>();
    let late = gate.clone();
    let signals = Mutex::new(signals);
    // Delayed provider: answers only after the test opens the gate, ignoring the abort signal.
    let handler: Arc<dyn HostHandler> = Arc::new(move |_: &str, _: Map<String, Value>, signal: &Signal| {
        let _ = signals.lock().unwrap().send(signal.clone());
        late.wait();
        Ok(json!({"late": "success"}))
    });
    let kernel = Arc::new(kernel_in(dir.path(), handler));
    let signal = Signal::new();
    let (cell_kernel, cell_signal) = (kernel.clone(), signal.clone());
    let code = process_tree_cell(&pid_file, "late = await rlm.host_request('provider.call', {})");
    let running = std::thread::spawn(move || cell_kernel.execute(&code, Some(&cell_signal), None));
    let pids = read_pids(&pid_file, 4);
    let host_signal = host_signal.recv_timeout(Duration::from_secs(20)).unwrap();
    assert!(pids.iter().all(|pid| alive(*pid)));

    signal.abort("user cancelled");
    assert_eq!(running.join().unwrap(), Err(KernelError::new("user cancelled")));
    assert!(host_signal.is_aborted());
    all_gone(&pids);

    // The late provider success arrives after cancellation and must not revive the cell.
    gate.open();
    std::thread::sleep(Duration::from_millis(50));
    let fresh = run(&kernel, "'late' in globals()");
    assert_eq!((fresh.status, fresh.result.as_deref()), (Status::Ok, Some("False")));
    kernel.shutdown();
}

fn shutdown_kills_the_tree_of_a_cell_blocked_in_synchronous_code() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("pids.json");
    let kernel = Arc::new(kernel_in(dir.path(), null_handler()));
    let cell_kernel = kernel.clone();
    let code = process_tree_cell(&pid_file, "import time\ntime.sleep(120)");
    let running = std::thread::spawn(move || cell_kernel.execute(&code, None, None));
    let pids = read_pids(&pid_file, 4);
    kernel.shutdown();
    let error = running.join().unwrap().unwrap_err();
    assert!(error.message().contains("shut down"), "{error}");
    all_gone(&pids);
}

/// A task document store that records every write.
#[derive(Clone, Default)]
struct Store(Arc<Mutex<(Option<Value>, Vec<Value>)>>);

impl Store {
    fn history(&self) -> Vec<Value> {
        self.0.lock().unwrap().1.clone()
    }
}

impl TaskDocumentStore for Store {
    fn read(&mut self) -> Result<Option<Value>, String> {
        Ok(self.0.lock().unwrap().0.clone())
    }
    fn write(&mut self, document: &Value) -> Result<(), String> {
        let mut state = self.0.lock().unwrap();
        state.0 = Some(document.clone());
        state.1.push(document.clone());
        Ok(())
    }
}

/// `(state, result.status)` of task `id` in each written document.
fn task_states(history: &[Value], id: &str) -> Vec<(String, Option<String>)> {
    history
        .iter()
        .filter_map(|document| {
            let task = document["tasks"].as_array()?.iter().find(|t| t["id"] == id)?;
            Some((
                task["state"].as_str()?.to_string(),
                task["result"]["status"].as_str().map(str::to_string),
            ))
        })
        .collect()
}

fn reviewer() -> SpawnRequest {
    let payload = json!({"definition": "security-reviewer@1", "input": {"request": "review"}});
    SpawnRequest::from_payload(payload.as_object().unwrap()).unwrap()
}

fn success() -> Value {
    json!({"status": "succeeded", "value": {"outcome": "no_findings", "findings": []}, "verification": "unverified"})
}

fn cancelling_a_task_kills_its_lane_kernel_tree_and_keeps_the_durable_cancelled_result() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("pids.json");
    let store = Store::default();
    let kernel = Arc::new(kernel_in(dir.path(), null_handler()));
    let lane_signal = Signal::new();
    let supervisor = Arc::new(Mutex::new(TaskSupervisor::new(store.clone())));
    let abort = lane_signal.clone();
    supervisor
        .lock()
        .unwrap()
        .on_abort(Box::new(move |_| abort.abort("lane aborted")));
    let spawned = supervisor.lock().unwrap().spawn(None, &reviewer()).unwrap();
    let id = spawned["id"].as_str().unwrap().to_string();

    // The lane: the task's Python owns a process tree; a provider success arrives after the kernel was killed.
    let provider = Arc::new(Gate::default());
    let (lane_kernel, lane_provider, lane_supervisor, lane_id) =
        (kernel.clone(), provider.clone(), supervisor.clone(), id.clone());
    let code = process_tree_cell(&pid_file, "import asyncio\nawait asyncio.sleep(120)");
    let lane = std::thread::spawn(move || {
        let _ = lane_kernel.execute(&code, Some(&lane_signal), None);
        lane_provider.wait();
        lane_supervisor.lock().unwrap().complete(&lane_id, success())
    });
    let pids = read_pids(&pid_file, 4);
    assert!(pids.iter().all(|pid| alive(*pid)));

    assert!(supervisor.lock().unwrap().cancel(None, &id).unwrap());
    all_gone(&pids);
    provider.open();
    let late = lane.join().unwrap();
    // The late success commits nothing: the first terminal result is what `complete` reports.
    assert_eq!(late.unwrap()["status"], "cancelled");
    let result = supervisor.lock().unwrap().result(None, &id).unwrap().unwrap();
    assert_eq!(result["status"], "cancelled");
    let states = task_states(&store.history(), &id);
    assert_eq!(
        states.last().unwrap(),
        &("cancelled".to_string(), Some("cancelled".to_string()))
    );
    assert!(!states.iter().any(|(state, _)| state == "completed"));

    let mut reopened = TaskSupervisor::new(store.clone());
    let inspected = reopened.inspect(None, &id).unwrap();
    assert_eq!(
        (&inspected["state"], &inspected["result"]["status"]),
        (&json!("cancelled"), &json!("cancelled"))
    );
    reopened.close().unwrap();
    supervisor.lock().unwrap().close().unwrap();
    kernel.shutdown();
}

fn a_delayed_success_after_cancellation_never_overwrites_the_cancelled_result() {
    let store = Store::default();
    let supervisor = Arc::new(Mutex::new(TaskSupervisor::new(store.clone())));
    let aborts = Arc::new(Mutex::new(0));
    let counted = aborts.clone();
    supervisor
        .lock()
        .unwrap()
        .on_abort(Box::new(move |_| *counted.lock().unwrap() += 1));
    let id = supervisor.lock().unwrap().spawn(None, &reviewer()).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let provider = Arc::new(Gate::default());
    let (lane_provider, lane_supervisor, lane_id) = (provider.clone(), supervisor.clone(), id.clone());
    // A lane whose provider answers successfully only after the gate opens, ignoring abort.
    let lane = std::thread::spawn(move || {
        lane_provider.wait();
        lane_supervisor.lock().unwrap().complete(&lane_id, success())
    });
    assert!(supervisor.lock().unwrap().cancel(None, &id).unwrap());
    assert_eq!(*aborts.lock().unwrap(), 1);
    provider.open();
    lane.join().unwrap().unwrap();
    let result = supervisor.lock().unwrap().result(None, &id).unwrap().unwrap();
    assert_eq!(
        (&result["status"], &result["verification"]),
        (&json!("cancelled"), &json!("unverified"))
    );
    let states = task_states(&store.history(), &id);
    assert_eq!(states.iter().filter(|(state, _)| state == "cancelled").count(), 1);
    assert!(!states.iter().any(|(state, _)| state == "completed"));
    supervisor.lock().unwrap().close().unwrap();
}

fn racing_cancellation_against_success_yields_one_consistent_terminal_result() {
    let mut outcomes = HashSet::new();
    for iteration in 0..30u64 {
        let store = Store::default();
        let supervisor = Arc::new(Mutex::new(TaskSupervisor::new(store.clone())));
        let id = supervisor.lock().unwrap().spawn(None, &reviewer()).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let (lane_supervisor, lane_id) = (supervisor.clone(), id.clone());
        let lane = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(iteration % 4));
            lane_supervisor.lock().unwrap().complete(&lane_id, success())
        });
        std::thread::sleep(Duration::from_millis((iteration * 3) % 5));
        let cancelled = supervisor.lock().unwrap().cancel(None, &id).unwrap();
        let result = supervisor.lock().unwrap().result(None, &id).unwrap().unwrap();
        lane.join().unwrap().unwrap();
        let states = task_states(&store.history(), &id);
        let terminal: Vec<_> = states
            .iter()
            .filter(|(state, _)| ["completed", "cancelled", "failed"].contains(&state.as_str()))
            .collect();
        let kinds: HashSet<_> = terminal.iter().map(|(state, _)| state.clone()).collect();
        assert_eq!(kinds.len(), 1, "{states:?}");
        let status = result["status"].as_str().unwrap().to_string();
        assert_eq!(status, if cancelled { "cancelled" } else { "succeeded" });
        assert_eq!(terminal.last().unwrap().1.as_deref(), Some(status.as_str()));
        let again = supervisor.lock().unwrap().result(None, &id).unwrap().unwrap();
        assert_eq!(again["status"], status.as_str());
        outcomes.insert(status);
        supervisor.lock().unwrap().close().unwrap();
    }
    // The race exercises the cancellation side, not only late completion.
    assert!(outcomes.contains("cancelled"));
}

#[test]
fn acceptance_a05_cancellation_kills_owned_work_and_rejects_late_success_ported_part() {
    cancelling_a_cell_kills_its_tree_and_a_late_reply_does_not_revive_it();
    shutdown_kills_the_tree_of_a_cell_blocked_in_synchronous_code();
    cancelling_a_task_kills_its_lane_kernel_tree_and_keeps_the_durable_cancelled_result();
    a_delayed_success_after_cancellation_never_overwrites_the_cancelled_result();
    racing_cancellation_against_success_yields_one_consistent_terminal_result();
}
