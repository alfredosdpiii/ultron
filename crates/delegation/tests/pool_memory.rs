//! `rlm.pool` reports each live kernel's process-tree memory and the tree cap (session-worker.ts `poolStats`:
//! `memoryBytes` per lane and in all, `memoryCapBytes`), against the real runtime.py.

use delegation::lanes::{CellOutcome, Lanes, LanesOptions};
use kernel::{HostHandler, Kernel, KernelOptions, Status, TreeMemoryOptions};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

fn runtime() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/src/ultron/rlm/runtime.py")
}

fn lanes(dir: &Path, tree_memory: Option<u64>) -> Lanes {
    let cwd = dir.to_path_buf();
    let null: Arc<dyn HostHandler> =
        Arc::new(|_: &str, _: serde_json::Map<String, Value>, _: &kernel::Signal| Ok(Value::Null));
    let (lanes, _ends) = Lanes::new(LanesOptions {
        create: Arc::new(move |_| {
            let mut options = KernelOptions::new(cwd.clone(), runtime());
            options.tree_memory = tree_memory.map(|cap| TreeMemoryOptions {
                max_mb: Some(cap),
                backend: Some("watchdog".into()),
                ..TreeMemoryOptions::default()
            });
            Kernel::new(options, null.clone()).map_err(|e| e.message().to_string())
        }),
        handlers: Arc::new(|_, _, _| {
            Arc::new(|_: &str, _: serde_json::Map<String, Value>, _: &kernel::Signal| Ok(Value::Null))
                as Arc<dyn HostHandler>
        }),
        max_lanes: 4,
        cancel_grace: Duration::from_secs(120),
        pool: None,
    })
    .unwrap();
    lanes
}

fn run(lanes: &Lanes, lane: &str) {
    lanes.open(lane).unwrap();
    match lanes
        .run(lane, "data = bytearray(8 * 1024 * 1024)\n1", Duration::from_secs(120))
        .unwrap()
    {
        CellOutcome::Done(Ok(result)) => assert_eq!(result.status, Status::Ok),
        other => panic!("{other:?}"),
    }
}

#[test]
fn delegation_pool_reports_kernel_tree_memory_and_cap() {
    let dir = tempfile::tempdir().unwrap();
    let lanes = lanes(dir.path(), Some(512));
    // Nothing runs yet: no reading.
    let idle = lanes.pool_stats();
    assert!(idle.get("memoryBytes").is_none(), "{idle}");
    run(&lanes, "main");
    run(&lanes, "side");
    let stats = lanes.pool_stats();
    let per_lane: Vec<u64> = stats["lanes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|lane| lane["memoryBytes"].as_u64().unwrap())
        .collect();
    assert_eq!(per_lane.len(), 2);
    // Each kernel holds its 8 MiB buffer.
    assert!(per_lane.iter().all(|bytes| *bytes >= 8 * 1024 * 1024), "{stats}");
    assert_eq!(stats["memoryBytes"].as_u64().unwrap(), per_lane.iter().sum::<u64>());
    assert_eq!(stats["memoryCapBytes"], 512 * 1024 * 1024);
    lanes.close();
}

#[test]
fn delegation_pool_without_a_tree_cap_reports_memory_only() {
    let dir = tempfile::tempdir().unwrap();
    let lanes = lanes(dir.path(), None);
    run(&lanes, "main");
    let stats = lanes.pool_stats();
    assert!(stats["memoryBytes"].as_u64().unwrap() > 0, "{stats}");
    assert!(stats.get("memoryCapBytes").is_none(), "{stats}");
    lanes.close();
}
