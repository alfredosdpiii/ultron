//! An open lane is not a live kernel: the TS session's rlm tool admits a kernel only for a cell (`kernels.use` in
//! session-worker.ts), so `rlm.pool` (the footer's kernel strip) shows 0 of 16 before the first cell. Replays
//! fixtures/idle-pool.json (generate-idle-pool.mjs: the TypeScript tool's `poolStats()` right after creation), then
//! checks admission with the first cell and the capacity refusal at open, with the real runtime.py.

use delegation::lanes::{CellOutcome, Lanes, LanesOptions, PoolLimits};
use kernel::{HostHandler, Kernel, KernelOptions, Status};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const HANG_GUARD: Duration = Duration::from_secs(120);

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/idle-pool.json")).unwrap()
}

fn runtime() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/src/ultron/rlm/runtime.py")
}

fn lanes(dir: &Path, max_live: usize) -> Lanes {
    let cwd = dir.to_path_buf();
    let null: Arc<dyn HostHandler> =
        Arc::new(|_: &str, _: serde_json::Map<String, Value>, _: &kernel::Signal| Ok(Value::Null));
    let (lanes, _ends) = Lanes::new(LanesOptions {
        create: Arc::new(move |_| {
            Kernel::new(KernelOptions::new(cwd.clone(), runtime()), null.clone()).map_err(|e| e.message().to_string())
        }),
        handlers: Arc::new(|_, _, _| {
            Arc::new(|_: &str, _: serde_json::Map<String, Value>, _: &kernel::Signal| Ok(Value::Null))
                as Arc<dyn HostHandler>
        }),
        max_lanes: 4,
        cancel_grace: Duration::from_secs(120),
        // Every kernel pinnable, so a test can fill the pool with pins.
        pool: Some(PoolLimits {
            max_pinned: Some(max_live),
            ..PoolLimits::new(max_live, None)
        }),
    })
    .unwrap();
    lanes
}

fn run(lanes: &Lanes, lane: &str) {
    match lanes.run(lane, "1", HANG_GUARD).unwrap() {
        CellOutcome::Done(Ok(result)) => assert_eq!(result.status, Status::Ok),
        other => panic!("{other:?}"),
    }
}

fn live_lanes(lanes: &Lanes) -> Vec<String> {
    lanes.pool_stats()["lanes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["lane"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn delegation_idle_pool_counts_no_kernel_for_an_open_lane_like_ts() {
    let dir = tempfile::tempdir().unwrap();
    let lanes = lanes(dir.path(), 16);
    // The session opens its root lane at start; before its first cell the pool is what TypeScript reports.
    lanes.open("main").unwrap();
    assert_eq!(lanes.pool_stats(), fixture()["idle"]["stats"]);
    // The first cell admits the lane's kernel; it stays live (idle) after the cell.
    run(&lanes, "main");
    let stats = lanes.pool_stats();
    assert_eq!((&stats["live"], &stats["maxLive"]), (&json!(1), &json!(16)));
    assert_eq!(live_lanes(&lanes), ["main"]);
    lanes.close();
}

#[test]
fn delegation_idle_pool_open_evicts_nothing_and_refuses_only_a_full_pool() {
    let dir = tempfile::tempdir().unwrap();
    let lanes = lanes(dir.path(), 1);
    lanes.open("a").unwrap();
    run(&lanes, "a");
    // Room can be made (a is idle): b opens, and a keeps its kernel until b's first cell needs the room.
    lanes.open("b").unwrap();
    assert_eq!(live_lanes(&lanes), ["a"]);
    assert_eq!(lanes.pool_stats()["evictions"], 0);
    run(&lanes, "b");
    assert_eq!(live_lanes(&lanes), ["b"]);
    assert_eq!(lanes.pool_stats()["evictions"], 1);
    // Every live kernel pinned: a new lane is refused with the pool's capacity error; an open one is not.
    assert!(lanes.pin("b", "instance:x"));
    assert_eq!(
        lanes.open("c").unwrap_err(),
        "RLM kernel capacity exhausted: 1/1 kernels are running or pinned"
    );
    lanes.open("b").unwrap();
    lanes.unpin("b", "instance:x");
    lanes.open("c").unwrap();
    assert_eq!(live_lanes(&lanes), ["b"]);
    lanes.close();
}
