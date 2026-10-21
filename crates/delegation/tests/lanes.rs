//! Process tests for the new lane and wait behavior, against the real runtime.py: a root's `rlm.collect` never holds
//! a cell (host collect mode), a cell parked on a waiting collect is detached at once and frees the turn, a root opens
//! more lanes, and a cell outliving its tool call is detached (its end arrives as an event) or cancelled (a parked
//! cell keeps its kernel and variables; a busy one has its kernel killed).

use delegation::host::{ChildRunner, CollectMode, Delegation, DelegationOptions, DetachedEnd};
use delegation::lanes::{CellEnd, CellOutcome, Lanes, LanesOptions};
use delegation::task_store::MemoryStore;
use kernel::{ExecutionResult, Kernel, KernelOptions, Status};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

fn runtime() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/src/ultron/rlm/runtime.py")
}

/// Children started so far, and which gates are open.
type GateState = Mutex<(usize, Vec<bool>)>;

/// Children that each wait until the test opens their gate (by spawn order), then reply `reply-<n>`.
#[derive(Clone, Default)]
struct Gates(Arc<(GateState, Condvar)>);

impl Gates {
    fn open(&self, index: usize) {
        let mut state = self.0.0.lock().unwrap();
        if state.1.len() <= index {
            state.1.resize(index + 1, false);
        }
        state.1[index] = true;
        self.0.1.notify_all();
    }
    fn runner(&self) -> ChildRunner {
        let gates = self.clone();
        Arc::new(move |run| {
            let index = {
                let mut state = gates.0.0.lock().unwrap();
                state.0 += 1;
                state.0 - 1
            };
            let mut state = gates.0.0.lock().unwrap();
            while !state.1.get(index).copied().unwrap_or(false) {
                if run.signal.is_aborted() {
                    return Err("aborted".into());
                }
                state = gates.0.1.wait_timeout(state, Duration::from_millis(50)).unwrap().0;
            }
            Ok(format!("reply-{index}"))
        })
    }
}

struct Root {
    _dir: tempfile::TempDir,
    host: Delegation,
    events: Receiver<DetachedEnd>,
    lanes: Lanes,
    ends: Receiver<CellEnd>,
    gates: Gates,
}

/// A hang guard, never a timing expectation: a loaded machine must slow a test down, never fail it.
const HANG_GUARD: Duration = Duration::from_secs(120);

fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + HANG_GUARD;
    while !ready() {
        assert!(Instant::now() < deadline, "{what}: not reached in {HANG_GUARD:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A root whose cancelled cells get a grace period no test step can outlast, so "ended without the grace period"
/// holds however slow the machine.
fn root(collect: CollectMode, max_lanes: usize) -> Root {
    root_with_grace(collect, max_lanes, HANG_GUARD)
}

fn root_with_grace(collect: CollectMode, max_lanes: usize, cancel_grace: Duration) -> Root {
    let dir = tempfile::tempdir().unwrap();
    let gates = Gates::default();
    let (host, events) = Delegation::new(DelegationOptions {
        store: Box::new(MemoryStore::default()),
        collect,
        runner: gates.runner(),
        spawn_depth: 3,
        child_model: None,
        workspace: None,
        session_id: "session".into(),
        worktree_setup: None,
        keep_worktrees: false,
        now: Box::new(|| 0.0),
    });
    let cwd = dir.path().to_path_buf();
    let private = dir.path().join("agent");
    let handlers_host = host.clone();
    let (lanes, ends) = Lanes::new(LanesOptions {
        create: Arc::new(move |_lane| {
            let mut options = KernelOptions::new(cwd.clone(), runtime());
            options.env = vec![
                ("ULTRON_CODING_AGENT_DIR".into(), private.display().to_string()),
                ("ULTRON_SERVER_DIR".into(), private.display().to_string()),
            ];
            let null: Arc<dyn kernel::HostHandler> = Arc::new(
                |_: &str, _: serde_json::Map<String, serde_json::Value>, _: &kernel::Signal| {
                    Ok(serde_json::Value::Null)
                },
            );
            Kernel::new(options, null).map_err(|e| e.message().to_string())
        }),
        handlers: Arc::new(move |lane, signal, park| Arc::new(handlers_host.cell_handler(lane, signal, park))),
        max_lanes,
        cancel_grace,
        pool: None,
    })
    .unwrap();
    lanes.open("main").unwrap();
    Root {
        _dir: dir,
        host,
        events,
        lanes,
        ends,
        gates,
    }
}

/// A cell budget no ordinary cell outlasts, however slow the machine (it is an input: a cell past it is detached).
const BUDGET: Duration = HANG_GUARD;

fn done(outcome: CellOutcome) -> ExecutionResult {
    match outcome {
        CellOutcome::Done(Ok(result)) => result,
        other => panic!("expected a finished cell, got {other:?}"),
    }
}

fn ok(lanes: &Lanes, lane: &str, code: &str) -> ExecutionResult {
    let result = done(lanes.run(lane, code, BUDGET).unwrap());
    assert_eq!(result.status, Status::Ok, "{code}: {result:?}");
    result
}

fn running(outcome: CellOutcome) -> (String, bool) {
    match outcome {
        CellOutcome::Running { cell, parked } => (cell, parked),
        other => panic!("expected a running cell, got {other:?}"),
    }
}

#[test]
fn delegation_host_collect_never_holds_a_cell() {
    let r = root(CollectMode::Host, 2);
    ok(&r.lanes, "main", "h = await rlm.spawn('brief', name='a')");
    // Returns within the budget (`ok`), though the child never ends until its gate opens.
    let result = ok(
        &r.lanes,
        "main",
        "try:\n    await rlm.collect([h])\nexcept RuntimeError as e:\n    print(e)",
    );
    let id = r.host.running("main").pop().unwrap();
    assert_eq!(
        result.stdout,
        format!(
            "rlm.collect: 1 subagent(s) still running ({id}). The host waits for them, not this cell: do other work, or end your turn; each end arrives as a child_done event, then collect again.\n"
        )
    );
    // The non-waiting form reports the running child.
    let poll = ok(&r.lanes, "main", "await rlm.host_request('rlm.results', {})");
    assert_eq!(poll.result.unwrap(), format!("{{'results': [], 'running': ['{id}']}}"));
    // The same lane keeps serving cells while the child runs.
    assert_eq!(ok(&r.lanes, "main", "6 * 7").result.unwrap(), "42");
    // The host waits (between turns), and the end arrives as an event.
    assert_eq!(r.host.running("main"), vec![id.clone()]);
    r.gates.open(0);
    let event = r.events.recv_timeout(HANG_GUARD).unwrap();
    assert_eq!(event.task["id"], id.as_str());
    assert!(!event.awaited);
    assert!(r.host.running("main").is_empty());
    let collected = ok(&r.lanes, "main", "(await rlm.collect([h]))[0]['result']['value']");
    assert_eq!(collected.result.unwrap(), "'reply-0'");
    r.lanes.close();
}

#[test]
fn delegation_parked_collect_detaches_and_other_lanes_run() {
    let r = root(CollectMode::Wait, 2);
    ok(&r.lanes, "main", "x = 41\nh = await rlm.spawn('brief', name='a')");
    let started = Instant::now();
    let (cell, parked) = running(
        r.lanes
            .run(
                "main",
                "res = await rlm.collect([h])\nprint(res[0]['result']['value'], x + 1)",
                BUDGET,
            )
            .unwrap(),
    );
    assert!(parked, "a cell waiting on children is detached at once");
    assert!(started.elapsed() < BUDGET, "detached before its budget");
    // The parked lane refuses another cell; the root opens a second lane, which runs at once with its own state.
    assert_eq!(
        r.lanes.run("main", "1", BUDGET).unwrap_err(),
        format!("REPL lane main is still running {cell}: cancel it, wait for its end, or run in another lane")
    );
    assert_eq!(r.lanes.run("side", "1", BUDGET).unwrap_err(), "Unknown REPL lane: side");
    r.lanes.open("side").unwrap();
    assert_eq!(ok(&r.lanes, "side", "'x' in globals()").result.unwrap(), "False");
    // A cell can list lanes and cells; the parked one shows as parked and detached.
    let cells = ok(&r.lanes, "side", "(await rlm.host_request('rlm.cells', {}))['cells']");
    assert_eq!(
        cells.result.unwrap(),
        format!(
            "[{{'cell': '{cell}', 'lane': 'main', 'parked': True, 'detached': True}}, {{'cell': 'cell-6', 'lane': 'side', 'parked': False, 'detached': False}}]"
        )
    );
    assert_eq!(
        r.lanes.open("third").unwrap_err(),
        "REPL lane limit reached: 2 of 2 lanes are open; close one first"
    );
    // The child ends; the parked cell completes and its end arrives as an event, not as a tool result.
    r.gates.open(0);
    let end = r.ends.recv_timeout(HANG_GUARD).unwrap();
    assert_eq!((end.cell.as_str(), end.lane.as_str()), (cell.as_str(), "main"));
    assert_eq!(end.outcome.unwrap().stdout, "reply-0 42\n");
    let event = r.events.recv_timeout(HANG_GUARD).unwrap();
    assert!(event.awaited, "the child's end was awaited by the parked collect");
    // The lane is free again, its state intact.
    assert_eq!(ok(&r.lanes, "main", "x").result.unwrap(), "41");
    r.lanes.close_lane("side").unwrap();
    r.lanes.close();
}

#[test]
fn delegation_cancel_parked_cell_keeps_kernel_state() {
    let r = root(CollectMode::Wait, 2);
    ok(&r.lanes, "main", "x = 7\nh = await rlm.spawn('brief', name='a')");
    let pid = r.lanes.kernel("main").unwrap().pid().unwrap();
    let (cell, parked) = running(r.lanes.run("main", "await rlm.collect([h])", BUDGET).unwrap());
    assert!(parked);
    let started = Instant::now();
    let outcome = r.lanes.cancel(&cell, "user pressed escape").unwrap().unwrap();
    assert!(started.elapsed() < HANG_GUARD, "the wait ends without the grace period");
    assert_eq!(outcome.status, Status::Error);
    let error = outcome.error.unwrap();
    assert_eq!(
        (error.ename.as_str(), error.evalue.as_str()),
        ("RuntimeError", "rlm.collect was cancelled: user pressed escape")
    );
    // No CellEnd event for a cancelled cell: the canceller has its outcome.
    assert!(r.ends.recv_timeout(Duration::from_millis(200)).is_err());
    assert_eq!(r.lanes.kernel("main").unwrap().pid(), Some(pid), "same kernel process");
    assert_eq!(ok(&r.lanes, "main", "x").result.unwrap(), "7");
    // The child itself keeps running; it was not the cell's to cancel.
    assert_eq!(r.host.running("main").len(), 1);
    r.gates.open(0);
    r.events.recv_timeout(HANG_GUARD).unwrap();
    r.lanes.close();
}

#[test]
fn delegation_cell_outliving_its_budget_detaches_or_cancels() {
    // A short grace: the busy cell below is killed once it passes.
    let r = root_with_grace(CollectMode::Host, 1, Duration::from_secs(1));
    let go = r._dir.path().join("go");
    ok(
        &r.lanes,
        "main",
        &format!("import os, time\ny = 5\ngo = {:?}", go.display().to_string()),
    );
    // Over budget: the caller detaches it; its end arrives on the channel. The cell runs until the test creates
    // `go`, so it is still running when detached.
    let (cell, parked) = running(
        r.lanes
            .run(
                "main",
                "while not os.path.exists(go):\n    time.sleep(0.01)\nprint('slept')",
                Duration::from_millis(50),
            )
            .unwrap(),
    );
    assert!(!parked);
    assert_eq!(r.lanes.detach(&cell).unwrap(), None);
    std::fs::write(&go, "").unwrap();
    let end = r.ends.recv_timeout(HANG_GUARD).unwrap();
    assert_eq!(end.cell, cell);
    assert_eq!(end.outcome.unwrap().stdout, "slept\n");
    // Detaching a cell that already ended hands back its outcome instead.
    std::fs::remove_file(&go).unwrap();
    let (cell, _) = running(
        r.lanes
            .run(
                "main",
                "while not os.path.exists(go):\n    time.sleep(0.01)\n'late'",
                Duration::from_millis(20),
            )
            .unwrap(),
    );
    std::fs::write(&go, "").unwrap();
    wait_until("the late cell ends", || r.lanes.list()[0]["cell"].is_null());
    let late = r.lanes.detach(&cell).unwrap().unwrap().unwrap();
    assert_eq!(late.result.unwrap(), "'late'");
    // Over budget and cancelled: a cell not parked on the host is ended by killing its kernel; the next cell starts
    // a fresh kernel without the earlier state.
    let pid = r.lanes.kernel("main").unwrap().pid().unwrap();
    let (cell, _) = running(
        r.lanes
            .run("main", "time.sleep(3600)", Duration::from_millis(50))
            .unwrap(),
    );
    let (lanes, id) = (r.lanes.clone(), cell.clone());
    let started = Instant::now();
    let cancelled = std::thread::spawn(move || lanes.cancel(&id, "stop"))
        .join()
        .unwrap()
        .unwrap();
    // Killed after the grace period, not after the cell's hour-long sleep.
    assert!(started.elapsed() < HANG_GUARD);
    assert_eq!(cancelled.unwrap_err(), "stop");
    assert!(r.ends.recv_timeout(Duration::from_millis(100)).is_err());
    assert_eq!(ok(&r.lanes, "main", "'y' in globals()").result.unwrap(), "False");
    assert_ne!(r.lanes.kernel("main").unwrap().pid(), Some(pid));
    assert_eq!(r.lanes.cancel("cell-404", "x").unwrap_err(), "Unknown cell: cell-404");
    r.lanes.close();
}

#[test]
fn delegation_cell_cancels_another_lanes_cell_but_not_itself() {
    let r = root(CollectMode::Wait, 2);
    r.lanes.open("side").unwrap();
    ok(&r.lanes, "main", "h = await rlm.spawn('brief', name='a')");
    let (cell, _) = running(r.lanes.run("main", "await rlm.collect([h])", BUDGET).unwrap());
    let reply = ok(
        &r.lanes,
        "side",
        &format!("await rlm.host_request('rlm.cell.cancel', {{'cell': '{cell}'}})"),
    );
    assert_eq!(reply.result.unwrap(), "{'cancelled': True, 'status': 'error'}");
    let own = ok(
        &r.lanes,
        "side",
        "try:\n    await rlm.host_request('rlm.cell.cancel', {'cell': 'cell-4'})\nexcept RuntimeError as e:\n    print(e)",
    );
    assert_eq!(own.stdout, "a cell cannot cancel itself\n");
    r.gates.open(0);
    r.lanes.close();
}

/// `CollectMode::InCell` (a lane nothing wakes, session_core::wake): the collect blocks its cell until the children
/// end, never parked (the cell is not detached, so its result is the tool result), bounded by `timeout_ms`; the mode
/// is picked per lane.
#[test]
fn delegation_in_cell_collect_holds_the_cell_unparked_and_honours_timeout_ms() {
    let r = root(CollectMode::Wait, 2);
    r.host.set_lane_collect(Arc::new(|lane: &str| {
        if lane == "main" {
            CollectMode::InCell
        } else {
            CollectMode::Host
        }
    }));
    assert_eq!(r.host.collect_mode("main"), CollectMode::InCell);
    assert_eq!(r.host.collect_mode("other"), CollectMode::Host);
    ok(&r.lanes, "main", "x = 41\nh = await rlm.spawn('brief', name='a')");
    let id = r.host.running("main").pop().unwrap();
    // A bounded wait ends with the children still running named, and they keep running.
    let timed_out = ok(
        &r.lanes,
        "main",
        "try:\n    await rlm.collect([h], timeout_ms=50)\nexcept RuntimeError as e:\n    print(e)",
    );
    assert_eq!(
        timed_out.stdout,
        format!(
            "rlm.collect: timed out after 50 ms with 1 subagent(s) still running ({id}); they keep running. `await rlm.results()` returns the results of those that ended and the ids still running; collect again to wait more.\n"
        )
    );
    assert_eq!(r.host.running("main"), vec![id.clone()]);
    let started = r
        .lanes
        .start_routed(
            "main",
            "res = await rlm.collect([h])\nprint(res[0]['result']['value'], x + 1)",
        )
        .unwrap();
    // While it waits the cell is neither parked nor detached.
    r.lanes.open("side").unwrap();
    let cells = ok(&r.lanes, "side", "(await rlm.host_request('rlm.cells', {}))['cells']");
    assert_eq!(
        cells.result.unwrap(),
        format!(
            "[{{'cell': '{}', 'lane': 'main', 'parked': False, 'detached': False}}, {{'cell': 'cell-4', 'lane': 'side', 'parked': False, 'detached': False}}]",
            started.cell
        )
    );
    r.gates.open(0);
    // Its result comes back as the cell's own (no CellEnd event).
    let result = done(r.lanes.wait(&started.cell, BUDGET).unwrap());
    assert_eq!(result.stdout, "reply-0 42\n");
    assert!(r.ends.try_recv().is_err());
    assert_eq!(r.lanes.detached_running("main"), 0);
    r.lanes.close_lane("side").unwrap();
    r.lanes.close();
}

/// `timeout_ms` must be a nonnegative number (0: no bound, as runtime.py sends by default).
#[test]
fn delegation_in_cell_collect_refuses_a_bad_timeout() {
    let r = root(CollectMode::InCell, 1);
    ok(&r.lanes, "main", "h = await rlm.spawn('brief', name='a')");
    let refused = ok(
        &r.lanes,
        "main",
        "try:\n    await rlm.host_request('rlm.collect', {'selectors': [h.rlm_child_id], 'timeout_ms': -1})\nexcept RuntimeError as e:\n    print(e)",
    );
    assert_eq!(
        refused.stdout,
        "rlm.collect timeout_ms must be a nonnegative number of milliseconds\n"
    );
    r.gates.open(0);
    let collected = ok(
        &r.lanes,
        "main",
        "(await rlm.collect([h], timeout_ms=0))[0]['result']['value']",
    );
    assert_eq!(collected.result.unwrap(), "'reply-0'");
    r.lanes.close();
}
