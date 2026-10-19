//! A busy lane never makes `rlm` unusable (2026-10-19: a hung `git push` held lane main and every later call was
//! refused). `Lanes::start_routed` runs a cell in a side lane while its lane is busy (the side-lane note names the
//! busy cell), detaches the busy cell so its end arrives as an event of its lane, waits for a cell being cancelled
//! instead, and goes back to the lane (closing idle side lanes) once it is free. `rlm.cancel(id)` cancels another
//! lane's cell. Against the real runtime.py; every wait is on a condition, with a 120 s hang guard.

use delegation::host::{ChildRunner, CollectMode, Delegation, DelegationOptions};
use delegation::lanes::{CellEnd, CellOutcome, Lanes, LanesOptions, Started, side_lane_note};
use delegation::task_store::MemoryStore;
use kernel::{ExecutionResult, Kernel, KernelOptions, Status};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const HANG_GUARD: Duration = Duration::from_secs(120);

fn runtime() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/src/ultron/rlm/runtime.py")
}

/// A child gate the test opens: spawned children wait until then.
#[derive(Clone, Default)]
struct Gate(Arc<(Mutex<bool>, Condvar)>);

impl Gate {
    fn open(&self) {
        *self.0.0.lock().unwrap() = true;
        self.0.1.notify_all();
    }
    fn runner(&self) -> ChildRunner {
        let gate = self.clone();
        Arc::new(move |run| {
            let mut open = gate.0.0.lock().unwrap();
            while !*open {
                if run.signal.is_aborted() {
                    return Err("aborted".into());
                }
                open = gate.0.1.wait_timeout(open, Duration::from_millis(50)).unwrap().0;
            }
            Ok("reply".into())
        })
    }
}

struct Root {
    dir: tempfile::TempDir,
    lanes: Lanes,
    ends: Receiver<CellEnd>,
    gate: Gate,
    /// The lane each cell handler was made for, in order.
    handler_lanes: Arc<Mutex<Vec<String>>>,
}

fn root(max_lanes: usize, cancel_grace: Duration) -> Root {
    let dir = tempfile::tempdir().unwrap();
    let gate = Gate::default();
    let (host, _events) = Delegation::new(DelegationOptions {
        store: Box::new(MemoryStore::default()),
        collect: CollectMode::Wait,
        runner: gate.runner(),
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
    let handler_lanes: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = handler_lanes.clone();
    let (lanes, ends) = Lanes::new(LanesOptions {
        create: Arc::new(move |_lane| {
            let mut options = KernelOptions::new(cwd.clone(), runtime());
            options.env = vec![
                ("ULTRON_CODING_AGENT_DIR".into(), private.display().to_string()),
                ("ULTRON_SERVER_DIR".into(), private.display().to_string()),
            ];
            let null: Arc<dyn kernel::HostHandler> =
                Arc::new(|_: &str, _: Map<String, Value>, _: &kernel::Signal| Ok(Value::Null));
            Kernel::new(options, null).map_err(|e| e.message().to_string())
        }),
        handlers: Arc::new(move |lane, signal, park| {
            seen.lock().unwrap().push(lane.to_string());
            Arc::new(host.cell_handler(lane, signal, park))
        }),
        max_lanes,
        cancel_grace,
        pool: None,
    })
    .unwrap();
    lanes.open("main").unwrap();
    Root {
        dir,
        lanes,
        ends,
        gate,
        handler_lanes,
    }
}

impl Root {
    fn path(&self, name: &str) -> String {
        self.dir.path().join(name).display().to_string()
    }

    /// Start a cell on `lane` through the router.
    fn start(&self, lane: &str, code: &str) -> Started {
        self.lanes.start_routed(lane, code).unwrap()
    }

    /// Start a cell through the router and wait for its end.
    fn run(&self, lane: &str, code: &str) -> (Started, ExecutionResult) {
        let started = self.start(lane, code);
        match self.lanes.wait(&started.cell, HANG_GUARD).unwrap() {
            CellOutcome::Done(Ok(result)) => (started, result),
            other => panic!("expected a finished cell, got {other:?}"),
        }
    }

    /// A cell that runs until the test creates `go` (it writes `began` first).
    fn blocking(&self, name: &str) -> String {
        format!(
            "import os, time\nopen({began:?}, 'w').close()\nwhile not os.path.exists({go:?}):\n    time.sleep(0.01)\nprint('{name} done')",
            began = self.path(&format!("{name}.began")),
            go = self.path(&format!("{name}.go")),
        )
    }

    fn release(&self, name: &str) {
        std::fs::write(self.path(&format!("{name}.go")), "").unwrap();
    }

    fn began(&self, name: &str) {
        let path = self.path(&format!("{name}.began"));
        wait_until(&format!("{name} begins"), || Path::new(&path).exists());
    }
}

fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + HANG_GUARD;
    while !ready() {
        assert!(Instant::now() < deadline, "{what}: not reached in {HANG_GUARD:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn lane_names(lanes: &Lanes) -> Vec<String> {
    lanes
        .list()
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["lane"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn delegation_busy_lane_runs_the_next_cell_in_a_side_lane_with_the_note_and_goes_back_once_free() {
    let r = root(4, HANG_GUARD);
    let (_, first) = r.run("main", "x = 1\nimport os\nos.getpid()");
    let main_pid = first.result.unwrap();
    let before = Instant::now();
    let stuck = r.start("main", &r.blocking("stuck"));
    assert_eq!((stuck.lane.as_str(), stuck.note.as_deref()), ("main", None));
    // The stuck cell's own call is still waiting for it when the next call comes.
    let (lanes, id) = (r.lanes.clone(), stuck.cell.clone());
    let waiter = std::thread::spawn(move || lanes.wait(&id, HANG_GUARD));
    r.began("stuck");

    let (side, result) = r.run("main", "import os\n('x' in globals(), os.getpid())");
    assert_eq!(side.lane, "main-side-1");
    let note = side.note.clone().unwrap();
    // `started Ns ago`: the stuck cell's age when the call came, a whole number of seconds.
    let secs: u64 = note
        .strip_prefix(&format!("[lane main is still running {} (started ", stuck.cell))
        .and_then(|rest| rest.split_once("s ago)"))
        .map(|(secs, _)| secs.parse().unwrap())
        .unwrap();
    assert!(secs <= before.elapsed().as_secs(), "{note}");
    assert_eq!(note, side_lane_note("main", &stuck.cell, secs, "main-side-1"));
    assert_eq!(
        note,
        format!(
            "[lane main is still running {cell} (started {secs}s ago); this ran in lane main-side-1; await rlm.cancel(\"{cell}\") stops it]",
            cell = stuck.cell
        )
    );
    // Its own kernel, without main's variables; its host requests are main's.
    let side_result = result.result.unwrap();
    assert!(side_result.starts_with("(False, "), "{side_result}");
    assert!(!side_result.contains(&main_pid), "{side_result} vs {main_pid}");
    assert_eq!(r.handler_lanes.lock().unwrap().last().unwrap(), "main");

    // The stuck cell's waiting call returned as detached: its end is an event of lane main.
    assert_eq!(
        waiter.join().unwrap().unwrap(),
        CellOutcome::Running {
            cell: stuck.cell.clone(),
            parked: true
        }
    );
    // While main is still busy, the next call reuses the idle side lane (its variables kept).
    let (again, _) = r.run("main", "y = 2");
    assert_eq!(again.lane, "main-side-1");
    let (again, result) = r.run("main", "y");
    assert_eq!(
        (again.lane.as_str(), result.result.as_deref()),
        ("main-side-1", Some("2"))
    );

    r.release("stuck");
    let end = r.ends.recv_timeout(HANG_GUARD).unwrap();
    assert_eq!((end.cell.as_str(), end.lane.as_str()), (stuck.cell.as_str(), "main"));
    assert_eq!(end.outcome.unwrap().stdout, "stuck done\n");
    // Main is free: calls go back to it (its variables intact) and the idle side lane is closed.
    let (back, result) = r.run("main", "x");
    assert_eq!((back.lane.as_str(), back.note.as_deref()), ("main", None));
    assert_eq!(result.result.unwrap(), "1");
    assert_eq!(lane_names(&r.lanes), ["main"]);
    r.lanes.close();
}

#[test]
fn delegation_a_side_lane_busy_too_opens_another_and_its_detached_end_is_its_lanes() {
    let r = root(4, HANG_GUARD);
    let stuck = r.start("main", &r.blocking("stuck"));
    r.began("stuck");
    let side = r.start("main", &r.blocking("side"));
    assert_eq!(side.lane, "main-side-1");
    r.began("side");
    let third = r.start("main", "3");
    assert_eq!(third.lane, "main-side-2");
    assert!(third.note.unwrap().contains(&format!("still running {}", stuck.cell)));
    assert_eq!(
        r.lanes.wait(&third.cell, HANG_GUARD).unwrap(),
        CellOutcome::Done(Ok(ok_result("3")))
    );
    // A side lane's cell that outlives its call is detached; its end is reported as lane main's.
    assert_eq!(
        r.lanes.wait(&side.cell, Duration::ZERO).unwrap(),
        CellOutcome::Running {
            cell: side.cell.clone(),
            parked: false
        }
    );
    assert_eq!(r.lanes.detach(&side.cell).unwrap(), None);
    r.release("side");
    let end = r.ends.recv_timeout(HANG_GUARD).unwrap();
    assert_eq!((end.cell.as_str(), end.lane.as_str()), (side.cell.as_str(), "main"));
    r.release("stuck");
    let end = r.ends.recv_timeout(HANG_GUARD).unwrap();
    assert_eq!((end.cell.as_str(), end.lane.as_str()), (stuck.cell.as_str(), "main"));
    let (back, _) = r.run("main", "1");
    assert_eq!(back.lane, "main");
    assert_eq!(lane_names(&r.lanes), ["main"]);
    r.lanes.close();
}

fn ok_result(value: &str) -> ExecutionResult {
    ExecutionResult {
        status: Status::Ok,
        stdout: String::new(),
        stderr: String::new(),
        result: Some(value.into()),
        error: None,
    }
}

#[test]
fn delegation_rlm_cancel_from_a_side_lane_stops_the_stuck_cell_and_frees_its_lane() {
    // A short grace: the stuck cell is not parked on the host, so its kernel is killed once it passes.
    let r = root(4, Duration::from_secs(1));
    r.run("main", "z = 3");
    let stuck = r.start("main", "import time\ntime.sleep(3600)");
    let side = r.start("main", "1");
    assert_eq!(side.lane, "main-side-1");
    r.lanes.wait(&side.cell, HANG_GUARD).unwrap();
    let (side, cancelled) = r.run("main", &format!("await rlm.cancel({:?})", stuck.cell));
    assert_eq!(side.lane, "main-side-1");
    assert_eq!(
        cancelled.result.unwrap(),
        "{'cancelled': True, 'status': 'error', 'error': 'cancelled from another cell'}"
    );
    // A cancelled cell's end is the canceller's, not an event.
    assert!(r.ends.try_recv().is_err());
    let (back, result) = r.run("main", "'z' in globals()");
    assert_eq!((back.lane.as_str(), back.note), ("main", None));
    // Its kernel was killed: main starts over.
    assert_eq!(result.result.unwrap(), "False");
    assert_eq!(lane_names(&r.lanes), ["main"]);
    // A cell cannot cancel itself, through either name.
    let own = r.run(
        "main",
        "cells = await rlm.cells()\ntry:\n    await rlm.cancel(cells[0]['cell'])\nexcept RuntimeError as e:\n    print(e)",
    );
    assert_eq!(own.1.stdout, "a cell cannot cancel itself\n");
    r.lanes.close();
}

#[test]
fn delegation_a_cell_being_cancelled_is_waited_for_not_routed_around() {
    let r = root(4, HANG_GUARD);
    r.run("main", "h = await rlm.spawn('brief', name='a')");
    // The cell survives its cancellation (it catches the raised collect) and runs until `go`.
    let code = format!(
        "import os, time\ntry:\n    await rlm.collect([h])\nexcept RuntimeError:\n    open({caught:?}, 'w').close()\n    while not os.path.exists({go:?}):\n        time.sleep(0.01)\n'ended'",
        caught = r.path("caught"),
        go = r.path("go"),
    );
    let cell = r.start("main", &code);
    let (lanes, id) = (r.lanes.clone(), cell.cell.clone());
    let cancel = std::thread::spawn(move || lanes.cancel(&id, "esc"));
    let caught = r.path("caught");
    wait_until("the cancelled cell caught it", || Path::new(&caught).exists());
    // The next call waits for the cancelled cell to end, then runs in main.
    let lanes = r.lanes.clone();
    let next = std::thread::spawn(move || lanes.start_routed("main", "2"));
    std::fs::write(r.path("go"), "").unwrap();
    let outcome = cancel.join().unwrap().unwrap().unwrap();
    assert_eq!(outcome.result.unwrap(), "'ended'");

    let next = next.join().unwrap().unwrap();
    assert_eq!((next.lane.as_str(), next.note), ("main", None));
    assert_eq!(
        r.lanes.wait(&next.cell, HANG_GUARD).unwrap(),
        CellOutcome::Done(Ok(ok_result("2")))
    );
    r.gate.open();
    r.lanes.close();
}

#[test]
fn delegation_no_room_for_a_side_lane_says_how_to_stop_the_busy_cell() {
    let r = root(1, HANG_GUARD);
    let stuck = r.start("main", &r.blocking("stuck"));
    r.began("stuck");
    assert_eq!(
        r.lanes.start_routed("main", "1").unwrap_err(),
        format!(
            "REPL lane main is still running {cell} and no side lane could open (REPL lane limit reached: 1 of 1 lanes are open): await rlm.cancel(\"{cell}\") stops it",
            cell = stuck.cell
        )
    );
    // Nothing was detached: the stuck cell's call still gets its result.
    r.release("stuck");
    match r.lanes.wait(&stuck.cell, HANG_GUARD).unwrap() {
        CellOutcome::Done(Ok(result)) => assert_eq!(result.stdout, "stuck done\n"),
        other => panic!("{other:?}"),
    }
    assert!(r.ends.try_recv().is_err());
    // The plain `run` still refuses a busy lane.
    let again = r.start("main", &r.blocking("again"));
    r.began("again");
    assert_eq!(
        r.lanes.run("main", "1", HANG_GUARD).unwrap_err(),
        format!(
            "REPL lane main is still running {}: cancel it, wait for its end, or run in another lane",
            again.cell
        )
    );
    r.release("again");
    r.lanes.wait(&again.cell, HANG_GUARD).unwrap();
    r.lanes.close();
}

#[test]
fn delegation_a_cancelled_cells_waiting_call_gets_its_outcome_too() {
    let r = root(4, HANG_GUARD);
    let cell = r.start("main", &r.blocking("slow"));
    r.began("slow");
    // Its own call waits for it (as the rlm tool does while it cancels the cell on an abort).
    let (lanes, id) = (r.lanes.clone(), cell.cell.clone());
    let waiter = std::thread::spawn(move || lanes.wait(&id, HANG_GUARD));
    let (lanes, id) = (r.lanes.clone(), cell.cell.clone());
    let cancel = std::thread::spawn(move || lanes.cancel(&id, "esc"));
    // `rlm.cells` shows it being cancelled; it ends within its grace (here: when the test lets it).
    wait_until("the cell is being cancelled", || {
        r.lanes.request("rlm.cells", Map::new(), None).unwrap()["cells"][0]["cancelling"] == true
    });
    r.release("slow");
    let outcome = cancel.join().unwrap().unwrap().unwrap();
    assert_eq!(outcome.stdout, "slow done\n");
    match waiter.join().unwrap().unwrap() {
        CellOutcome::Done(Ok(result)) => assert_eq!(result.stdout, "slow done\n"),
        other => panic!("{other:?}"),
    }
    assert!(
        r.lanes.request("rlm.cells", Map::new(), None).unwrap()["cells"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    r.lanes.close();
}
