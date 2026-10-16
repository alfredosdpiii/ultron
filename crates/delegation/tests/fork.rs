//! `rlm.spawn(fork=True)` in the subagent host, ported from packages/coding-agent/test/ultron-spawn-fork.test.ts:
//! `forkPoint` with its literal cases, and the host's handling (the anchor reaches the runner and the reply, the
//! brief leads with the fork note, refusals).

use delegation::fork::{FORK_BRIEF_NOTE, fork_point};
use delegation::host::{ChildRunner, Delegation, DelegationOptions};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

fn assistant_calling(id: &str) -> Value {
    json!({ "role": "assistant", "content": [
        { "type": "text", "text": "running a cell" },
        { "type": "toolCall", "id": id, "name": "rlm", "arguments": { "code": "await rlm.spawn(...)" } },
    ], "stopReason": "toolUse" })
}

fn result(id: &str) -> Value {
    json!({ "role": "toolResult", "toolCallId": id, "toolName": "rlm", "content": [], "isError": false })
}

fn point(entries: &[(&str, Value)]) -> Option<String> {
    let entries: Vec<(String, Vec<Value>)> = entries
        .iter()
        .map(|(id, message)| (id.to_string(), vec![message.clone()]))
        .collect();
    fork_point(entries.iter().map(|(id, m)| (id.clone(), m.as_slice())))
}

#[test]
fn delegation_fork_point_stops_before_a_call_still_waiting_for_its_result() {
    assert_eq!(point(&[]), None);
    let done = vec![
        ("u", json!({ "role": "user", "content": "hi" })),
        ("a1", assistant_calling("c1")),
        ("r1", result("c1")),
    ];
    assert_eq!(point(&done).as_deref(), Some("r1"));
    let mut running = done.clone();
    running.push(("a2", assistant_calling("c2")));
    assert_eq!(point(&running).as_deref(), Some("r1"));
    // An entry without messages (a reset, a system change) is a fork point when nothing waits.
    let entries: Vec<(String, Vec<Value>)> = vec![("s".into(), vec![]), ("a".into(), vec![assistant_calling("c")])];
    assert_eq!(
        fork_point(entries.iter().map(|(id, m)| (id.clone(), m.as_slice()))).as_deref(),
        Some("s")
    );
}

/// The prompt and fork anchor of each child run.
type Runs = Arc<Mutex<Vec<(String, Option<String>)>>>;

fn host(with_hook: Option<Option<&'static str>>) -> (Delegation, Runs) {
    let runs: Runs = Arc::default();
    let seen = runs.clone();
    let runner: ChildRunner = Arc::new(move |run| {
        seen.lock().unwrap().push((run.prompt.clone(), run.fork_at.clone()));
        Ok("child finished".into())
    });
    let (delegation, _ends) = Delegation::new(DelegationOptions::new(runner));
    if let Some(anchor) = with_hook {
        delegation.set_fork_point(Arc::new(move |lane: &str| {
            assert_eq!(lane, "main");
            Ok(anchor.map(str::to_string))
        }));
    }
    (delegation, runs)
}

fn spawn(delegation: &Delegation, kwargs: Value) -> Result<Value, String> {
    let mut kwargs = kwargs;
    kwargs["name"] = json!("helper");
    delegation.handle(
        "rlm.spawn",
        json!({ "prompt": "Child brief CHILD_INPUT_12", "kwargs": kwargs }),
        "main",
    )
}

#[test]
fn delegation_forked_child_starts_at_its_callers_fork_point_with_the_fork_note() {
    let (delegation, runs) = host(Some(Some("root:entry-7")));
    let child = spawn(&delegation, json!({ "fork": true })).unwrap();
    assert_eq!(child["parent_branch_anchor"], "root:entry-7");
    let id = child["rlm_child_id"].as_str().unwrap().to_string();
    let ended = delegation.wait(&[id], None);
    assert_eq!(ended[0].1.as_ref().unwrap()["status"], "succeeded");
    let (prompt, fork_at) = runs.lock().unwrap()[0].clone();
    assert_eq!(prompt, format!("{FORK_BRIEF_NOTE}\n\nChild brief CHILD_INPUT_12"));
    assert_eq!(fork_at.as_deref(), Some("root:entry-7"));
    // Without fork the child gets only its brief.
    let plain = spawn(&delegation, json!({})).unwrap();
    assert_eq!(plain["parent_branch_anchor"], "");
    delegation.wait(&[plain["rlm_child_id"].as_str().unwrap().to_string()], None);
    let (prompt, fork_at) = runs.lock().unwrap()[1].clone();
    assert_eq!((prompt.as_str(), fork_at), ("Child brief CHILD_INPUT_12", None));
}

#[test]
fn delegation_fork_is_refused_on_an_empty_lane_and_must_be_a_boolean() {
    let (delegation, runs) = host(Some(None));
    assert_eq!(
        spawn(&delegation, json!({ "fork": true })),
        Err("rlm.spawn fork=True: your lane has no finished conversation to fork yet".into())
    );
    assert_eq!(
        spawn(&delegation, json!({ "fork": "yes" })),
        Err("fork must be True or False".into())
    );
    // A host whose children cannot start on a conversation (external processes) refuses fork.
    let (external, _) = host(None);
    assert_eq!(
        spawn(&external, json!({ "fork": true })),
        Err("rlm.spawn fork=True is not available when subagents run as external processes".into())
    );
    assert!(runs.lock().unwrap().is_empty());
    assert!(delegation.tasks().unwrap().is_empty(), "a refused fork admits nothing");
}
