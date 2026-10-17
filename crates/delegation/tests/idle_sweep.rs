//! The session sweeps idle kernels on a timer (session-worker.ts: `setInterval(() => kernels.sweep(), 60_000)`
//! with a 30 minute idle TTL): an idle unpinned kernel past its TTL is evicted at the next sweep, without any lane
//! being leased. The pool's clock is injected and the sweeps are driven by ticks the test sends; each sweep reports
//! what it evicted, which the test waits for (a 120 s hang guard only).

use delegation::lanes::{CellOutcome, Lanes, LanesOptions, PoolLimits};
use kernel::{HostHandler, Kernel, KernelOptions, Status};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const HANG_GUARD: Duration = Duration::from_secs(120);
const TTL_MS: f64 = 30.0 * 60.0 * 1000.0;

fn runtime() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/src/ultron/rlm/runtime.py")
}

fn lanes(dir: &Path, now: Arc<Mutex<f64>>) -> Lanes {
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
        pool: Some(PoolLimits {
            now: Some(Arc::new(move || *now.lock().unwrap())),
            ..PoolLimits::new(4, None)
        }),
    })
    .unwrap();
    lanes
}

fn live(lanes: &Lanes) -> Vec<String> {
    lanes.pool_stats()["lanes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["lane"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn delegation_idle_sweep_evicts_kernels_past_their_ttl_on_the_timer() {
    let dir = tempfile::tempdir().unwrap();
    let now = Arc::new(Mutex::new(1_000.0));
    let lanes = lanes(dir.path(), now.clone());
    for lane in ["a", "b"] {
        lanes.open(lane).unwrap();
        match lanes.run(lane, "x = 1", HANG_GUARD).unwrap() {
            CellOutcome::Done(Ok(result)) => assert_eq!(result.status, Status::Ok),
            other => panic!("{other:?}"),
        }
    }
    // "b" is used again later, so only "a" is idle for the whole TTL.
    *now.lock().unwrap() = 1_000.0 + TTL_MS / 2.0;
    lanes.run("b", "x = 2", HANG_GUARD).unwrap();
    let (tick, ticks) = channel();
    let (swept, sweeps) = channel();
    let sweeper = lanes.sweep_on(ticks, Some(swept));
    // Not idle long enough: nothing goes.
    *now.lock().unwrap() = 1_000.0 + TTL_MS - 1.0;
    tick.send(()).unwrap();
    assert_eq!(sweeps.recv_timeout(HANG_GUARD).unwrap(), Vec::<String>::new());
    assert_eq!(live(&lanes), ["a", "b"]);
    // At the TTL the idle kernel is evicted by the sweep alone; the other stays.
    *now.lock().unwrap() = 1_000.0 + TTL_MS;
    tick.send(()).unwrap();
    assert_eq!(sweeps.recv_timeout(HANG_GUARD).unwrap(), ["a"]);
    assert_eq!(live(&lanes), ["b"]);
    // Its next cell gets a kernel again.
    lanes.run("a", "x = 3", HANG_GUARD).unwrap();
    drop(tick);
    sweeper.join().unwrap();
    lanes.close();
}
