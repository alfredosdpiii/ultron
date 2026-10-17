//! One shell-job table for every lane (shell-jobs.ts: the session's single `ShellJobs`, with `laneWorkspace(lane)`
//! giving a worktree subagent's cwd and env): a worktree lane's host runs its commands with the worktree's extra
//! environment, and its jobs join the session host's table, so the root lane lists them, the root turn's abort
//! cancels them, their ends reach the session's `onEnd`, the session's journal records them (with the worktree as
//! their cwd), and they keep running when the worktree lane's host is dropped.
//!
//! Every wait is on a condition with a 120 s hang guard; the only processes signalled are the jobs this test started.

use host::journal::FileJobJournal;
use host::{AllowAll, Host, JobEnd};
use kernel::Signal;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const HANG: Duration = Duration::from_secs(120);
const CHILD_LANE: &str = "ultron.rlm-child.t1";

fn wait_for(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + HANG;
    while !condition() {
        assert!(Instant::now() < deadline, "hang guard: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn call(host: &Host, lane: &str, kind: &str, payload: Value) -> Value {
    let Value::Object(payload) = payload else {
        unreachable!()
    };
    host.handle_lane(lane, kind, payload, &Signal::new())
        .unwrap_or_else(|e| panic!("{kind}: {e}"))
}

fn worktree_env(dir: &Path) -> Vec<(String, String)> {
    vec![
        ("ULTRON_WORKTREE".into(), dir.display().to_string()),
        ("ULTRON_PARENT_REPO".into(), "/repo".into()),
        ("ULTRON_WORKTREE_BRANCH".into(), "ultron/t1".into()),
        ("PWD".into(), dir.display().to_string()),
    ]
}

#[test]
fn host_shared_jobs_a_worktree_hosts_commands_get_its_environment() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let host = Host::new(&dir, dir.join(".jobs"), &dir, Box::new(AllowAll)).with_env(worktree_env(&dir));
    let echo = "echo W=$ULTRON_WORKTREE R=$ULTRON_PARENT_REPO B=$ULTRON_WORKTREE_BRANCH P=$PWD";
    let expected = format!("W={0} R=/repo B=ultron/t1 P={0}\n", dir.display());
    let blocking = call(&host, CHILD_LANE, "bash", json!({ "command": echo }));
    assert_eq!(blocking["output"], expected);
    let job = call(
        &host,
        CHILD_LANE,
        "shell.run",
        json!({ "command": echo, "yield_after": 0 }),
    );
    let done = call(&host, CHILD_LANE, "shell.result", json!({ "id": job["id"] }));
    assert_eq!(done["text"], expected, "{done}");
    // A host without extra environment does not see them.
    let plain = Host::new(&dir, dir.join(".jobs2"), &dir, Box::new(AllowAll));
    assert_eq!(
        call(&plain, "main", "bash", json!({ "command": "echo W=$ULTRON_WORKTREE" }))["output"],
        "W=\n"
    );
}

#[test]
fn host_shared_jobs_a_worktree_lanes_jobs_are_the_sessions() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let worktree = dir.join("wt");
    std::fs::create_dir_all(&worktree).unwrap();
    let session = Host::new(&dir, dir.join(".jobs"), &dir, Box::new(AllowAll))
        .with_journal(Arc::new(FileJobJournal::new(dir.join("jobs.json"))))
        .unwrap();
    session.set_roots(Arc::new(|lane: &str| Some(format!("root-of:{lane}"))));
    let ends: Arc<Mutex<Vec<JobEnd>>> = Arc::default();
    let seen = ends.clone();
    session.on_job_end(Arc::new(move |end| seen.lock().unwrap().push(end)));
    let child = Host::new(&worktree, dir.join(".jobs"), &worktree, Box::new(AllowAll))
        .with_env(worktree_env(&worktree))
        .sharing_jobs(&session);
    // A job that ends on its own and one that runs until its root turn is aborted (bounded: a hang guard).
    let quick = call(
        &child,
        CHILD_LANE,
        "shell.run",
        json!({ "command": "echo quick", "yield_after": 0 }),
    );
    let slow = call(
        &child,
        CHILD_LANE,
        "shell.run",
        json!({ "command": "i=0; while [ $i -lt 2400 ]; do sleep 0.05; i=$((i+1)); done", "yield_after": 0 }),
    );
    wait_for("the quick job's end", || {
        ends.lock().unwrap().iter().any(|end| end.record.id == quick["id"])
    });
    // The quick job's end reached the session's `onEnd`, with the child's lane and root.
    let quick_end = ends
        .lock()
        .unwrap()
        .iter()
        .find(|e| e.record.id == quick["id"])
        .cloned()
        .unwrap();
    assert_eq!(quick_end.record.lane, CHILD_LANE);
    assert_eq!(
        quick_end.record.root_id.as_deref(),
        Some(format!("root-of:{CHILD_LANE}").as_str())
    );
    // The root lane lists both; the child lane's host going away does not stop the running one.
    drop(child);
    let listed: Vec<Value> = call(&session, "main", "shell.list", json!({}))
        .as_array()
        .unwrap()
        .clone();
    let ids: Vec<&Value> = listed.iter().map(|job| &job["id"]).collect();
    assert!(ids.contains(&&quick["id"]) && ids.contains(&&slow["id"]), "{listed:?}");
    assert_eq!(
        call(&session, "main", "shell.get", json!({ "id": slow["id"] }))["status"],
        "running"
    );
    // The session's journal records them, each with the worktree as its cwd.
    let journal: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("jobs.json")).unwrap()).unwrap();
    let records = journal["jobs"]
        .as_array()
        .or_else(|| journal.as_array())
        .cloned()
        .unwrap_or_default();
    let slow_record = records
        .iter()
        .find(|r| r["id"] == slow["id"])
        .unwrap_or_else(|| panic!("{journal}"));
    assert_eq!(slow_record["cwd"], worktree.display().to_string());
    assert_eq!(slow_record["lane"], CHILD_LANE);
    // Esc on the root turn stops it; its end is not announced.
    session.cancel_root(&format!("root-of:{CHILD_LANE}"));
    wait_for("the slow job's end", || {
        ends.lock().unwrap().iter().any(|end| end.record.id == slow["id"])
    });
    let slow_end = ends
        .lock()
        .unwrap()
        .iter()
        .find(|e| e.record.id == slow["id"])
        .cloned()
        .unwrap();
    assert!(slow_end.root_aborted);
    assert_eq!(slow_end.event(), None);
    assert_eq!(
        call(&session, "main", "shell.get", json!({ "id": slow["id"] }))["status"],
        "cancelled"
    );
}

#[test]
fn host_commands_and_jobs_never_see_the_parent_verdict_token() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let env = vec![("ULTRON_PARENT_TOKEN".to_string(), "secret-token".to_string())];
    let host = Host::new(&dir, dir.join(".jobs"), &dir, Box::new(AllowAll)).with_env(env);
    let echo = "echo T=$ULTRON_PARENT_TOKEN";
    assert_eq!(
        call(&host, "main", "bash", json!({ "command": echo }))["output"],
        "T=\n"
    );
    let job = call(&host, "main", "shell.run", json!({ "command": echo, "yield_after": 0 }));
    let done = call(&host, "main", "shell.result", json!({ "id": job["id"] }));
    assert_eq!(done["text"], "T=\n", "{done}");
}
