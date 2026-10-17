//! The footer's kernel strip reads `rlm.pool`: a session composed as the worker composes it (`Runtime::stack`, its
//! root lane opened) reports no live kernel before its first cell, as the TypeScript session does
//! (crates/delegation/fixtures/idle-pool.json, recorded from `createUltronRlmTool(...).poolStats()`).

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

#[test]
fn session_core_idle_session_reports_no_live_kernel_before_its_first_cell_like_ts() {
    let dir = tempfile::tempdir().unwrap();
    let stack = stack(dir.path(), 16);
    let expected: Value = serde_json::from_str(include_str!("../../delegation/fixtures/idle-pool.json")).unwrap();
    // The pool's own fields; the session adds its guard statistics (Loki), which the fixture's bare tool has not.
    let mut stats = stack.pool_stats();
    assert!(stats.as_object_mut().unwrap().remove("guards").is_some(), "{stats}");
    assert_eq!(stats, expected["idle"]["stats"]);
    assert!(matches!(
        stack.lanes.run("main", "1", HANG_GUARD).unwrap(),
        CellOutcome::Done(Ok(_))
    ));
    assert_eq!(stack.pool_stats()["live"], json!(1));
    stack.close();
}
