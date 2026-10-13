//! The runtime's wrappers for the lane, cell, results and recovery host requests, called from cells of the real
//! runtime.py (crates/kernel) and served by the Rust handlers ([`Lanes::request`], [`Delegation`]): `rlm.lanes()`,
//! `rlm.lane.open()` / `close()`, `rlm.cells()`, `rlm.cell.cancel()`, `rlm.results()` (never waits),
//! `rlm.collect()` in host collect mode, and `rlm.recovery()`.

use delegation::host::{ChildRunner, CollectMode, Delegation, DelegationOptions, DetachedEnd};
use delegation::lanes::{CellOutcome, Lanes, LanesOptions};
use delegation::task_store::MemoryStore;
use kernel::{ContinuityPolicy, ExecutionResult, Kernel, KernelOptions, Status};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

fn runtime() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/src/ultron/rlm/runtime.py")
}

/// Children started so far, and which gates are open.
type GateState = Mutex<(usize, Vec<bool>)>;

/// Children wait until the test opens their gate (by spawn order), then reply `reply-<n>`.
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
    dir: tempfile::TempDir,
    events: Receiver<DetachedEnd>,
    lanes: Lanes,
    gates: Gates,
}

/// A root with a "main" lane. `continuity`: each lane kernel checkpoints after every cell (to `<lane>.snapshot`)
/// and is restored when it dies.
fn root(collect: CollectMode, max_lanes: usize, continuity: bool) -> Root {
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
    let (lanes, _ends) = Lanes::new(LanesOptions {
        create: Arc::new(move |lane| {
            let mut options = KernelOptions::new(cwd.clone(), runtime());
            options.env = vec![
                ("ULTRON_CODING_AGENT_DIR".into(), private.display().to_string()),
                ("ULTRON_SERVER_DIR".into(), private.display().to_string()),
            ];
            if continuity {
                options.snapshot_key = Some(vec![7; 32]);
                options.snapshot_path = Some(cwd.join(format!("{lane}.snapshot")));
                options.continuity = Some(ContinuityPolicy::default());
            }
            let null: Arc<dyn kernel::HostHandler> = Arc::new(
                |_: &str, _: serde_json::Map<String, serde_json::Value>, _: &kernel::Signal| {
                    Ok(serde_json::Value::Null)
                },
            );
            Kernel::new(options, null).map_err(|e| e.message().to_string())
        }),
        handlers: Arc::new(move |lane, signal, park| Arc::new(host.cell_handler(lane, signal, park))),
        max_lanes,
        // No test step can outlast it, so "ended without the grace period" holds however slow the machine.
        cancel_grace: HANG_GUARD,
    })
    .unwrap();
    lanes.open("main").unwrap();
    Root {
        dir,
        events,
        lanes,
        gates,
    }
}

/// A hang guard, never a timing expectation: a loaded machine must slow a test down, never fail it.
const HANG_GUARD: Duration = Duration::from_secs(120);

/// A cell budget no ordinary cell outlasts, however slow the machine (it is an input: a cell past it is detached).
const BUDGET: Duration = HANG_GUARD;

fn ok(lanes: &Lanes, lane: &str, code: &str) -> ExecutionResult {
    let result = match lanes.run(lane, code, BUDGET).unwrap() {
        CellOutcome::Done(Ok(result)) => result,
        other => panic!("expected a finished cell, got {other:?}"),
    };
    assert_eq!(result.status, Status::Ok, "{code}: {result:?}");
    result
}

fn value(lanes: &Lanes, lane: &str, code: &str) -> String {
    ok(lanes, lane, code).result.unwrap_or_default()
}

/// What a cell's call raised: `<ExceptionType>: <message>`.
fn raised(lanes: &Lanes, lane: &str, call: &str) -> String {
    let code = format!("try:\n    {call}\nexcept Exception as e:\n    print(f'{{type(e).__name__}}: {{e}}')");
    ok(lanes, lane, &code).stdout.trim_end().to_string()
}

fn parked_cell(lanes: &Lanes, lane: &str, code: &str) -> String {
    match lanes.run(lane, code, BUDGET).unwrap() {
        CellOutcome::Running { cell, parked: true } => cell,
        other => panic!("expected a parked cell, got {other:?}"),
    }
}

#[test]
fn delegation_rlm_lane_wrappers_open_list_and_close_lanes() {
    let r = root(CollectMode::Wait, 2, false);
    assert_eq!(
        value(&r.lanes, "main", "[(l['lane'], l['cell']) for l in await rlm.lanes()]"),
        "[('main', 'cell-1')]"
    );
    let pid = r.lanes.kernel("main").unwrap().pid().unwrap();
    assert_eq!(
        value(&r.lanes, "main", "[l['pid'] for l in await rlm.lanes()]"),
        format!("[{pid}]")
    );
    assert_eq!(
        value(&r.lanes, "main", "await rlm.lane.open('side')"),
        "{'opened': True}"
    );
    // Idempotent; the new lane has no kernel until its first cell.
    assert_eq!(
        value(&r.lanes, "main", "await rlm.lane.open('side')"),
        "{'opened': True}"
    );
    assert_eq!(
        value(&r.lanes, "main", "await rlm.lanes()"),
        format!("[{{'lane': 'main', 'cell': 'cell-5', 'pid': {pid}}}, {{'lane': 'side', 'cell': None, 'pid': None}}]")
    );
    assert_eq!(
        raised(&r.lanes, "main", "await rlm.lane.open('third')"),
        "RuntimeError: REPL lane limit reached: 2 of 2 lanes are open; close one first"
    );
    assert_eq!(
        raised(&r.lanes, "main", "await rlm.lane.open('  ')"),
        "RuntimeError: lane must be a nonempty string"
    );
    assert_eq!(
        raised(&r.lanes, "main", "await rlm.lane.close('nope')"),
        "RuntimeError: Unknown REPL lane: nope"
    );
    // A lane running a cell refuses to close: the side lane's own cell closes "side" only after it ends.
    assert_eq!(
        raised(&r.lanes, "side", "await rlm.lane.close('side')"),
        "RuntimeError: REPL lane side is running cell-9; cancel it first"
    );
    assert_eq!(
        value(&r.lanes, "main", "await rlm.lane.close('side')"),
        "{'closed': True}"
    );
    assert_eq!(
        value(&r.lanes, "main", "[l['lane'] for l in await rlm.lanes()]"),
        "['main']"
    );
    r.lanes.close();
}

#[test]
fn delegation_rlm_cell_wrappers_list_and_cancel_a_parked_cell() {
    let r = root(CollectMode::Wait, 2, false);
    r.lanes.open("side").unwrap();
    ok(&r.lanes, "main", "x = 7\nh = await rlm.spawn('brief', name='a')");
    let cell = parked_cell(&r.lanes, "main", "await rlm.collect([h])");
    assert_eq!(
        value(&r.lanes, "side", "await rlm.cells()"),
        format!(
            "[{{'cell': '{cell}', 'lane': 'main', 'parked': True, 'detached': True}}, {{'cell': 'cell-3', 'lane': 'side', 'parked': False, 'detached': False}}]"
        )
    );
    assert_eq!(
        raised(&r.lanes, "side", "await rlm.lane.close('main')"),
        format!("RuntimeError: REPL lane main is running {cell}; cancel it first")
    );
    let started = Instant::now();
    assert_eq!(
        value(&r.lanes, "side", &format!("await rlm.cell.cancel('{cell}')")),
        "{'cancelled': True, 'status': 'error'}"
    );
    assert!(
        started.elapsed() < HANG_GUARD,
        "a parked cell ends without the grace period"
    );
    assert_eq!(
        raised(&r.lanes, "side", "await rlm.cell.cancel('cell-6')"),
        "RuntimeError: a cell cannot cancel itself"
    );
    assert_eq!(
        raised(&r.lanes, "side", "await rlm.cell.cancel('cell-404')"),
        "RuntimeError: Unknown cell: cell-404"
    );
    // Only the asking cell still runs; the cancelled lane kept its kernel and variables.
    assert_eq!(
        value(&r.lanes, "side", "[(c['cell'], c['lane']) for c in await rlm.cells()]"),
        "[('cell-8', 'side')]"
    );
    assert_eq!(value(&r.lanes, "main", "x"), "7");
    r.gates.open(0);
    r.events.recv_timeout(HANG_GUARD).unwrap();
    r.lanes.close();
}

#[test]
fn delegation_rlm_results_never_waits_and_host_collect_raises_naming_running_children() {
    let r = root(CollectMode::Host, 1, false);
    ok(
        &r.lanes,
        "main",
        "ha = await rlm.spawn('one', name='a')\nhb = await rlm.spawn('two', name='b')",
    );
    let (a, b) = (value(&r.lanes, "main", "ha.id"), value(&r.lanes, "main", "hb.id"));
    let (a, b) = (a.trim_matches('\''), b.trim_matches('\''));
    // Neither call waits: each returns within the budget (`value` refuses a detached cell) while the children run.
    assert_eq!(
        value(&r.lanes, "main", "await rlm.results()"),
        format!("{{'results': [], 'running': ['{a}', '{b}']}}")
    );
    // A SpawnHandle or an id selects one child.
    assert_eq!(
        value(&r.lanes, "main", "(await rlm.results(hb))['running']"),
        format!("['{b}']")
    );
    assert_eq!(
        value(&r.lanes, "main", &format!("(await rlm.results(['{a}']))['running']")),
        format!("['{a}']")
    );
    assert_eq!(
        raised(&r.lanes, "main", "await rlm.collect()"),
        format!(
            "RuntimeError: rlm.collect: 2 subagent(s) still running ({a}, {b}). The host waits for them, not this cell: do other work, or end your turn; each end arrives as a child_done event, then collect again."
        )
    );
    r.gates.open(0);
    let end = r.events.recv_timeout(HANG_GUARD).unwrap();
    assert_eq!(end.task["id"], a);
    assert_eq!(
        value(
            &r.lanes,
            "main",
            "res = await rlm.results([ha, hb])\n([(x['id'], x['result']['value']) for x in res['results']], res['running'])"
        ),
        format!("([('{a}', 'reply-0')], ['{b}'])")
    );
    // Every selected child ended: collect returns at once; one still running fails it.
    assert_eq!(
        value(
            &r.lanes,
            "main",
            "[x['result']['value'] for x in await rlm.collect([ha])]"
        ),
        "['reply-0']"
    );
    assert_eq!(
        raised(&r.lanes, "main", "await rlm.collect([ha, hb])"),
        format!(
            "RuntimeError: rlm.collect: 1 subagent(s) still running ({b}). The host waits for them, not this cell: do other work, or end your turn; each end arrives as a child_done event, then collect again."
        )
    );
    r.gates.open(1);
    r.events.recv_timeout(HANG_GUARD).unwrap();
    assert_eq!(
        value(&r.lanes, "main", "[x['result']['value'] for x in await rlm.collect()]"),
        "['reply-0', 'reply-1']"
    );
    assert_eq!(value(&r.lanes, "main", "(await rlm.results())['running']"), "[]");
    r.lanes.close();
}

fn kill(pid: u32) {
    let pid = rustix::process::Pid::from_raw(i32::try_from(pid).unwrap()).unwrap();
    rustix::process::kill_process(pid, rustix::process::Signal::KILL).unwrap();
}

fn wait_until(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + HANG_GUARD;
    while !ready() {
        assert!(Instant::now() < deadline, "condition not reached in {HANG_GUARD:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn delegation_rlm_recovery_reports_the_lane_kernels_restart() {
    let r = root(CollectMode::Host, 2, true);
    assert_eq!(value(&r.lanes, "main", "(await rlm.recovery()) is None"), "True");
    ok(&r.lanes, "main", "total = 41\nhelper = lambda x: x");
    let kernel = r.lanes.kernel("main").unwrap();
    wait_until(|| {
        kernel
            .checkpoint()
            .is_some_and(|c| c.saved.iter().chain(&c.skipped).any(|n| n == "helper"))
    });
    let old = kernel.pid().unwrap();
    kill(old);
    wait_until(|| kernel.is_running() && kernel.pid().is_some_and(|pid| pid != old));
    let first = ok(&r.lanes, "main", "total + 1");
    assert_eq!(first.result.as_deref(), Some("42"));
    assert!(
        first.stderr.starts_with("[RLM kernel restarted after: "),
        "{}",
        first.stderr
    );
    let snapshot = r.dir.path().join("main.snapshot").display().to_string();
    assert_eq!(
        value(
            &r.lanes,
            "main",
            "rec = await rlm.recovery()\n(rec['snapshotPath'], rec['restored'], rec['lost'], rec['interruptedCell'], rec['unsavedCells'], 'restoreError' in rec)"
        ),
        format!(
            "('{snapshot}', ['_', 'state', 'total'], [{{'name': 'helper', 'reason': 'unsupported type builtins.function'}}], False, 0, False)"
        )
    );
    // Still available after the notice was delivered, and per lane: another lane's kernel has none.
    assert!(value(&r.lanes, "main", "(await rlm.recovery())['reason']").starts_with("'RLM kernel exited (SIGKILL)"));
    r.lanes.open("side").unwrap();
    assert_eq!(value(&r.lanes, "side", "(await rlm.recovery()) is None"), "True");
    r.lanes.close();
}
