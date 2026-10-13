//! A43 (partial): resource release and eviction respect live references and evidence retention.
//! Ported from the four kernel cases of packages/coding-agent/test/ultron-a43-resource-release.test.ts with their
//! literal expectations: `delegation::kernel_pool::KernelPool` over real kernels (runtime.py under the Rust kernel
//! supervisor) on a controlled clock. A cell runs on its own thread while the pool sweeps, as the TS test leaves its
//! cell promise pending.
//!
//! Not ported: artifact retention, deletion policy and quota (`artifacts.*` of NativeLocalServices has no Rust
//! counterpart), and ultron-a43-kernel-pool-wired.test.ts (the session's REPL lanes, `delegation::lanes::Lanes`, refuse
//! past their lane limit instead of evicting through the pool, and no `rlm.pool` stats exist).

mod common;

use common::runtime;
use delegation::kernel_pool::{KernelPool, KernelPoolOptions, PoolableKernel, snapshot_outcome};
use kernel::{ExecutionResult, Kernel, KernelOptions, Status};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// A pooled real kernel that a cell can keep running on another thread. Snapshot and shutdown are exactly the
/// pool's own `impl PoolableKernel for kernel::Kernel`.
#[derive(Clone)]
struct Shared(Arc<Kernel>);

impl PoolableKernel for Shared {
    fn snapshot(&mut self, path: &str) -> Result<Vec<String>, String> {
        let reply = self
            .0
            .snapshot(Some(std::path::Path::new(path)))
            .map_err(|error| error.to_string())?;
        snapshot_outcome(&serde_json::to_value(&reply).map_err(|error| error.to_string())?)
    }
    fn shutdown(&mut self) {
        self.0.shutdown();
    }
}

/// Where a lane's snapshot goes.
type PathFor = Arc<dyn Fn(&str) -> String + Send + Sync>;

type Created = Arc<Mutex<HashMap<String, Vec<Arc<Kernel>>>>>;

struct Fixture {
    pool: KernelPool<Shared>,
    now: Arc<AtomicU64>,
    created: Created,
}

impl Fixture {
    fn new(max_live: usize, idle_ttl_ms: f64, snapshot: Option<PathFor>) -> Fixture {
        let now = Arc::new(AtomicU64::new(1_000));
        let created: Created = Arc::default();
        let (clock, made, path_for) = (now.clone(), created.clone(), snapshot.clone());
        let pool = KernelPool::new(KernelPoolOptions {
            create: Box::new(move |lane| {
                let mut options = KernelOptions::new(env!("CARGO_MANIFEST_DIR"), runtime());
                options.snapshot_path = path_for.as_ref().map(|f| PathBuf::from(f(lane)));
                let kernel = Arc::new(Kernel::new(options, common::null_handler()).unwrap());
                made.lock()
                    .unwrap()
                    .entry(lane.to_string())
                    .or_default()
                    .push(kernel.clone());
                Shared(kernel)
            }),
            max_live,
            idle_ttl_ms,
            max_pinned: None,
            snapshot_path: snapshot
                .map(|f| -> delegation::kernel_pool::SnapshotPath { Box::new(move |lane: &str| Some(f(lane))) }),
            now: Box::new(move || clock.load(Ordering::SeqCst) as f64),
        })
        .unwrap();
        Fixture { pool, now, created }
    }

    fn advance(&self, ms: u64) {
        self.now.fetch_add(ms, Ordering::SeqCst);
    }

    /// `use(lane, kernel => kernel.execute(code))`.
    fn run(&mut self, lane: &str, code: &str) -> ExecutionResult {
        let lease = self.pool.acquire(lane).unwrap();
        let kernel = self.pool.kernel(lease).unwrap().0.clone();
        let result = kernel.execute(code, None, None).unwrap();
        self.pool.release(lease);
        result
    }

    fn live_lanes(&self) -> Vec<String> {
        let mut lanes: Vec<String> = self.pool.stats()["lanes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|lane| lane["lane"].as_str().unwrap().to_string())
            .collect();
        lanes.sort();
        lanes
    }

    fn kernels(&self, lane: &str) -> Vec<Arc<Kernel>> {
        self.created.lock().unwrap().get(lane).cloned().unwrap_or_default()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.pool.close();
        for kernel in self.created.lock().unwrap().values().flatten() {
            kernel.shutdown();
        }
    }
}

#[test]
fn acceptance_a43_idle_eviction_skips_a_running_cell_and_a_pinned_lane() {
    let mut f = Fixture::new(4, 30_000.0, None);
    let running = f.pool.acquire("running").unwrap();
    let busy = f.pool.kernel(running).unwrap().0.clone();
    let cell = std::thread::spawn(move || {
        busy.execute("import asyncio\nawait asyncio.sleep(0.3)\n'done'", None, None)
            .unwrap()
    });
    f.run("pinned", "kept = 1");
    f.pool.pin("pinned", "checkpoint:pinned@3").unwrap();
    f.run("idle", "scratch = 2");

    f.advance(29_999);
    assert_eq!(f.pool.sweep(), Vec::<Value>::new());
    f.advance(1);
    let swept = f.pool.sweep();
    assert_eq!(swept.len(), 1);
    assert_eq!(
        (
            &swept[0]["lane"],
            &swept[0]["reason"],
            &swept[0]["evicted"],
            &swept[0]["restorable"]
        ),
        (&json!("idle"), &json!("idle"), &json!(true), &json!("none"))
    );
    assert_eq!(f.live_lanes(), ["pinned", "running"]);

    // Explicit eviction is refused, and recorded, while a cell runs or a holder pins the lane.
    let refused = f.pool.evict("running", "explicit");
    assert_eq!(
        (&refused["evicted"], &refused["refused"]),
        (&json!(false), &json!("running"))
    );
    let refused = f.pool.evict("pinned", "explicit");
    assert_eq!(
        (&refused["evicted"], &refused["refused"]),
        (&json!(false), &json!("pinned"))
    );
    let done = cell.join().unwrap();
    assert_eq!((done.status, done.result.as_deref()), (Status::Ok, Some("'done'")));
    assert_eq!(f.run("pinned", "kept").result.as_deref(), Some("1"));

    f.pool.release(running);
    f.pool.unpin("pinned", "checkpoint:pinned@3");
    f.advance(30_000);
    let mut later: Vec<(String, bool)> = f
        .pool
        .sweep()
        .iter()
        .map(|r| (r["lane"].as_str().unwrap().into(), r["evicted"] == true))
        .collect();
    later.sort();
    assert_eq!(later, [("pinned".into(), true), ("running".into(), true)]);
    assert_eq!(f.pool.stats()["live"], 0);
    let log: Vec<Value> = f
        .pool
        .evictions()
        .iter()
        .map(|r| {
            json!([
                r["lane"],
                r["evicted"],
                r.get("refused").cloned().unwrap_or(Value::Null)
            ])
        })
        .collect();
    assert_eq!(
        log,
        [
            json!(["idle", true, null]),
            json!(["running", false, "running"]),
            json!(["pinned", false, "pinned"]),
            json!(["running", true, null]),
            json!(["pinned", true, null]),
        ]
    );
}

#[test]
fn acceptance_a43_capacity_evicts_the_lru_idle_kernel_else_fails_typed() {
    let mut f = Fixture::new(2, 60_000.0, None);
    f.run("first", "1");
    f.advance(10);
    f.run("second", "2");
    f.advance(10);
    f.run("third", "3");
    let evictions = f.pool.evictions().to_vec();
    assert_eq!(evictions.len(), 1);
    assert_eq!(
        (&evictions[0]["lane"], &evictions[0]["reason"], &evictions[0]["evicted"]),
        (&json!("first"), &json!("capacity"), &json!(true))
    );
    assert!(!f.kernels("first")[0].is_running());

    // Every live kernel is running or pinned: no hidden queue, an explicit capacity error.
    f.pool.pin("second", "instance:reviewer").unwrap();
    let lease = f.pool.acquire("third").unwrap();
    assert_eq!(
        f.pool.acquire("fourth").unwrap_err(),
        "RLM kernel capacity exhausted: 2/2 kernels are running or pinned"
    );
    assert_eq!(f.live_lanes(), ["second", "third"]);
    f.pool.release(lease);
    let second = f.kernels("second")[0].clone();
    let invalid = KernelPool::new(KernelPoolOptions {
        create: Box::new(move |_| Shared(second.clone())),
        max_live: 1,
        idle_ttl_ms: 1.0,
        max_pinned: Some(2),
        snapshot_path: None,
        now: Box::new(|| 0.0),
    });
    assert!(invalid.err().is_some_and(|error| error.contains("maxPinned")));
}

#[test]
fn acceptance_a43_snapshot_before_evict_records_non_restorable_names() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().to_path_buf();
    let mut f = Fixture::new(
        2,
        60_000.0,
        Some(Arc::new(move |lane: &str| {
            root.join(format!("{lane}.snap")).display().to_string()
        })),
    );
    f.run("worker", "total = 41\nhelper = lambda x: x + 1");
    let record = f.pool.evict("worker", "explicit");
    assert_eq!(
        (
            &record["evicted"],
            &record["restorable"],
            &record["nonRestorable"],
            &record["snapshotPath"]
        ),
        (
            &json!(true),
            &json!("partial"),
            &json!(["helper"]),
            &json!(directory.path().join("worker.snap").display().to_string())
        )
    );
    assert!(!f.kernels("worker")[0].is_running());
    let restored = f.run("worker", "(total + 1, 'helper' in globals())");
    assert_eq!(
        (restored.status, restored.result.as_deref()),
        (Status::Ok, Some("(42, False)"))
    );
    assert_eq!(f.kernels("worker").len(), 2);
}

#[test]
fn acceptance_a43_failed_snapshot_refuses_eviction_and_keeps_state() {
    let mut f = Fixture::new(
        1,
        0.0,
        Some(Arc::new(|_: &str| "/dev/null/cannot/write.snap".to_string())),
    );
    f.run("only", "value = 'precious'");
    let swept = f.pool.sweep();
    assert_eq!(swept.len(), 1);
    assert_eq!(
        (&swept[0]["lane"], &swept[0]["evicted"], &swept[0]["refused"]),
        (&json!("only"), &json!(false), &json!("snapshot-failed"))
    );
    assert!(swept[0]["error"].is_string());
    // Capacity cannot evict it either, so the new lane gets a capacity error, not state loss.
    assert_eq!(
        f.pool.acquire("other").unwrap_err(),
        "RLM kernel capacity exhausted: 1/1 kernels are running or pinned"
    );
    assert_eq!(f.run("only", "value").result.as_deref(), Some("'precious'"));
}
