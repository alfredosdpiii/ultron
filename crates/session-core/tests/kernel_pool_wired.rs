//! A43, ported from packages/coding-agent/test/ultron-a43-kernel-pool-wired.test.ts: the session's REPL lanes run
//! on the kernel pool. The stack is composed as the session worker composes it (`Runtime::stack`), with the pool
//! bounded to one live kernel (`rlm.maxLiveKernels`): a second lane evicts the idle first one after a snapshot, and the
//! first lane's state comes back in a new process. `rlm.pool` reports the pool; with every kernel running or pinned a
//! new lane is refused with the pool's capacity error.

use delegation::lanes::CellOutcome;
use serde_json::{Value, json};
use session_core::RunEnv;
use session_core::compose::{Runtime, Services, Stack};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

const HANG_GUARD: Duration = Duration::from_secs(120);

fn stack(root: &Path, max_live: u64) -> Arc<Stack> {
    let home = root.join("home");
    std::fs::create_dir_all(home.join(".ultron/agent")).unwrap();
    std::fs::write(
        home.join(".ultron/agent/settings.json"),
        json!({"sandbox": {"mode": "off"}, "rlm": {"maxLiveKernels": max_live}}).to_string(),
    )
    .unwrap();
    let cwd = root.join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let vars: Vec<(String, String)> = vec![
        ("HOME".into(), home.display().to_string()),
        (
            "ULTRON_CODING_AGENT_DIR".into(),
            home.join(".ultron/agent").display().to_string(),
        ),
        ("ULTRON_SERVER_DIR".into(), root.join("server").display().to_string()),
        ("ULTRON_HINDSIGHT_URL".into(), "off".into()),
    ];
    let env = RunEnv {
        cwd,
        home,
        var: Arc::new(move |key| vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())),
        docs: config::paths::normalize(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/docs")),
    };
    let runtime = Runtime::new(&env, "print", root.join("sessions"), Services::default()).unwrap();
    let model = ai::models::builtin_models("anthropic").next().unwrap().clone();
    runtime.stack(&model, None, "a43").unwrap()
}

/// One cell on `lane` (opened when needed), its result text.
fn run(stack: &Stack, lane: &str, code: &str) -> String {
    stack.lanes.open(lane).unwrap();
    match stack.lanes.run(lane, code, HANG_GUARD).unwrap() {
        CellOutcome::Done(Ok(result)) => {
            assert_eq!(result.status, kernel::Status::Ok, "{result:?}");
            format!("{}{}", result.stdout, result.result.unwrap_or_default())
        }
        other => panic!("cell did not end: {other:?}"),
    }
}

#[test]
fn session_core_a43_a_second_lane_evicts_the_idle_first_one_after_a_snapshot_and_the_first_lanes_state_comes_back() {
    let dir = tempfile::tempdir().unwrap();
    let stack = stack(dir.path(), 1);
    assert!(
        run(&stack, "lane-a", "state['answer'] = 42\nimport os\nos.getpid()")
            .parse::<u32>()
            .is_ok()
    );
    let first_pid = run(&stack, "lane-a", "os.getpid()");
    // Only one kernel may live: lane A is snapshotted and evicted to make room.
    assert_eq!(run(&stack, "lane-b", "'answer' in state"), "False");
    assert!(stack.lanes.kernel("lane-a").is_none());
    // A new process restores lane A's declared state.
    let back = run(&stack, "lane-a", "import os\n(state['answer'], os.getpid())");
    assert!(back.starts_with("(42, "), "{back}");
    assert_ne!(run(&stack, "lane-a", "os.getpid()"), first_pid);

    // `rlm.pool` from a cell: the lane's own kernel is the one live kernel, leased by that cell.
    let pool: Value = serde_json::from_str(&run(
        &stack,
        "lane-a",
        "import json\nprint(json.dumps(await rlm.host_request('rlm.pool', {})))",
    ))
    .unwrap();
    assert_eq!((&pool["live"], &pool["maxLive"]), (&json!(1), &json!(1)));
    assert_eq!(pool["lanes"][0]["lane"], "lane-a");
    assert_eq!(pool["lanes"][0]["running"], 1);
    // Lane A, then lane B made room (main never ran a cell, so it never held a kernel).
    assert_eq!(pool["evictions"], 2);
    let refused = run(
        &stack,
        "lane-a",
        "try:\n    await rlm.host_request('rlm.pool', {'extra': 1})\nexcept Exception as e:\n    print(e)",
    );
    assert!(refused.contains("Unknown payload field: extra"), "{refused}");
    stack.close();
}

#[test]
fn session_core_a43_a_pinned_lane_is_never_evicted_and_a_full_pool_refuses_a_new_lane() {
    let dir = tempfile::tempdir().unwrap();
    let stack = stack(dir.path(), 2);
    run(&stack, "lane-a", "state['kept'] = 1");
    // The instances module pins a retained lane through the typed-agent host.
    assert!(stack.lanes.pin("lane-a", "instance:x"));
    run(&stack, "lane-b", "1");
    // Lane A stays pinned, lane B is idle and makes room for lane C (main never held a kernel).
    run(&stack, "lane-c", "1");
    assert!(stack.lanes.kernel("lane-a").is_some());
    assert!(stack.lanes.kernel("lane-b").is_none());
    // With lane C held by a running cell and lane A pinned, nothing can be evicted.
    let CellOutcome::Running { cell, .. } = stack
        .lanes
        .run("lane-c", "import time\ntime.sleep(600)", Duration::ZERO)
        .unwrap()
    else {
        panic!("the cell ended at once");
    };
    let refused = stack.lanes.open("lane-d").unwrap_err();
    assert_eq!(
        refused,
        "RLM kernel capacity exhausted: 2/2 kernels are running or pinned"
    );
    stack.lanes.cancel(&cell, "test over").unwrap().ok();
    stack.lanes.unpin("lane-a", "instance:x");
    stack.lanes.open("lane-d").unwrap();
    stack.close();
}
