//! The pool over real kernels (`impl PoolableKernel for kernel::Kernel`), ported from
//! packages/coding-agent/test/ultron-a43-resource-release.test.ts with its literal expectations: snapshot before
//! eviction records the non-restorable names and a recreated kernel restores the rest; a failed snapshot refuses the
//! eviction and keeps the kernel's state.

use delegation::kernel_pool::{KernelPool, KernelPoolOptions};
use kernel::{Kernel, KernelOptions, Signal, Status};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

fn runtime() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/src/ultron/rlm/runtime.py")
}

type PathFor = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

fn pool(max_live: usize, idle_ttl_ms: f64, path_for: PathFor, created: Arc<AtomicUsize>) -> KernelPool<Kernel> {
    let key = vec![3u8; 32];
    let create_path = path_for.clone();
    KernelPool::new(KernelPoolOptions {
        create: Box::new(move |lane| {
            created.fetch_add(1, Ordering::SeqCst);
            let mut options = KernelOptions::new(env!("CARGO_MANIFEST_DIR"), runtime());
            options.snapshot_key = Some(key.clone());
            options.snapshot_path = create_path(lane).map(PathBuf::from);
            Kernel::new(
                options,
                Arc::new(|_: &str, _: Map<String, Value>, _: &Signal| Ok(Value::Null)),
            )
            .unwrap()
        }),
        max_live,
        idle_ttl_ms,
        max_pinned: None,
        snapshot_path: Some(Box::new(move |lane| path_for(lane))),
        now: Box::new(|| 1000.0),
    })
    .unwrap()
}

fn execute(pool: &Mutex<KernelPool<Kernel>>, lane: &str, code: &str) -> kernel::ExecutionResult {
    let mut pool = pool.lock().unwrap();
    let lease = pool.acquire(lane).unwrap();
    let result = pool.kernel(lease).unwrap().execute(code, None, None).unwrap();
    pool.release(lease);
    result
}

#[test]
fn delegation_pool_snapshots_a_real_kernel_before_eviction_and_the_recreated_kernel_restores_it() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().to_path_buf();
    let created = Arc::new(AtomicUsize::new(0));
    let pool = Mutex::new(pool(
        2,
        60_000.0,
        Arc::new(move |lane| Some(root.join(format!("{lane}.snap")).display().to_string())),
        created.clone(),
    ));
    execute(&pool, "worker", "total = 41\nhelper = lambda x: x + 1");
    let record = pool.lock().unwrap().evict("worker", "explicit");
    let path = directory.path().join("worker.snap").display().to_string();
    assert_eq!(
        record,
        json!({ "lane": "worker", "reason": "explicit", "at": 1000, "evicted": true, "restorable": "partial",
                "nonRestorable": ["helper"], "snapshotPath": path })
    );
    assert!(pool.lock().unwrap().live("worker").is_none());
    let restored = execute(&pool, "worker", "(total + 1, 'helper' in globals())");
    assert_eq!(restored.status, Status::Ok);
    assert_eq!(restored.result.as_deref(), Some("(42, False)"));
    assert_eq!(created.load(Ordering::SeqCst), 2);
    pool.lock().unwrap().close();
}

#[test]
fn delegation_pool_failed_real_snapshot_refuses_eviction_and_keeps_state() {
    let created = Arc::new(AtomicUsize::new(0));
    let pool = Mutex::new(pool(
        1,
        0.0,
        Arc::new(|_| Some("/dev/null/cannot/write.snap".into())),
        created,
    ));
    // The configured path cannot exist, so startup restore finds nothing ("missing").
    execute(&pool, "only", "value = 'precious'");
    let swept = pool.lock().unwrap().sweep();
    assert_eq!(swept.len(), 1);
    assert_eq!(swept[0]["lane"], "only");
    assert_eq!(swept[0]["evicted"], false);
    assert_eq!(swept[0]["refused"], "snapshot-failed");
    assert!(
        swept[0]["error"].as_str().is_some_and(|error| !error.is_empty()),
        "{}",
        swept[0]
    );
    // Capacity cannot evict it either: the new lane gets a capacity error, not state loss.
    assert_eq!(
        pool.lock().unwrap().acquire("other").unwrap_err(),
        "RLM kernel capacity exhausted: 1/1 kernels are running or pinned"
    );
    assert_eq!(execute(&pool, "only", "value").result.as_deref(), Some("'precious'"));
    pool.lock().unwrap().close();
}
