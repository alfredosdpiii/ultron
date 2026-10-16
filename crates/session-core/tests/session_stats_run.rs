//! Replays fixtures/session_stats_run.json (fixtures/generate-session-stats-run.mjs, the TypeScript session worker in
//! RPC mode): one root turn whose cells print, run a shell command that writes a file, raise, mask a secret and
//! write through `write()` leaves the same runtime counters in the session file (`ultron.module/stats`): cells, the
//! APIs they name, the kernels' host requests, masked secrets and Loki's checks (times zeroed on both sides).

mod p_session_support;

use p_session_support::{Setup, actor, in_order, text, tool, tool_results};
use serde_json::Value;
use std::sync::LazyLock;

static FIXTURE: LazyLock<Value> =
    LazyLock::new(|| serde_json::from_str(include_str!("../fixtures/session_stats_run.json")).expect("fixture parses"));

/// The last stored stats document, normalized as the generator normalizes it.
fn stored_stats(writes: &[Value]) -> Value {
    let mut stats = writes
        .iter()
        .rev()
        .find(|w| w["kind"] == "value" && w["namespace"] == "ultron.module" && w["key"] == "stats" && w["op"] == "set")
        .map(|w| w["value"].clone())
        .expect("the session file holds the counters");
    stats["since"] = 0.into();
    stats["updatedAt"] = 0.into();
    for guard in stats["guards"].as_object_mut().unwrap().values_mut() {
        guard["ms"] = 0.into();
    }
    stats
}

#[test]
fn session_core_session_stats_of_a_run_match_typescript() {
    let cells: Vec<&str> = FIXTURE["cells"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap())
        .collect();
    let mut replies: Vec<String> = cells
        .iter()
        .enumerate()
        .map(|(i, code)| tool(&format!("toolu_{}", i + 1), code))
        .collect();
    replies.push(text("done"));
    let a = actor(in_order(replies), Setup::default());
    let end = a.prompt("count");
    assert!(end["error"].is_null(), "{end}");
    // The cells did what the recorded ones did.
    let requests = a.requests();
    assert_eq!(
        Value::Array(tool_results(requests.last().unwrap())),
        FIXTURE["toolResults"]
    );
    assert_eq!(stored_stats(&a.writes()), FIXTURE["stats"]);
}
