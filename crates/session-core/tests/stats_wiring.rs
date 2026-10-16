//! Where the session's runtime counters come from (session-worker.ts wiring of `SessionStatsRecorder`): steers sent
//! to the root (`steerRoot` -> `sessionStats.nudge`) and work the usage ledger refuses at a root's limit
//! (`onRefused` -> `usageLimitBlock`) reach the stored `ultron.module/stats` document.

mod p_session_support;

use p_session_support::{Setup, actor, in_order, text, tool, user_texts};
use serde_json::Value;
use session_core::core::Op;

fn stored_stats(a: &p_session_support::Actor) -> Value {
    a.call(Op::Describe).unwrap();
    a.writes()
        .iter()
        .rev()
        .find(|w| w["namespace"] == "ultron.module" && w["key"] == "stats" && w["op"] == "set")
        .map(|w| w["value"].clone())
        .expect("the counters are stored")
}

#[test]
fn session_core_stats_count_the_steers_sent_to_the_root() {
    let a = actor(
        in_order(vec![
            tool("toolu_1", "x = 1"),
            tool("toolu_2", "x = 2"),
            tool("toolu_3", "x = 3"),
            text("done"),
        ]),
        Setup {
            vars: vec![("ULTRON_TOOL_ROUNDS_NUDGE", "2".into())],
            ..Setup::default()
        },
    );
    assert!(a.prompt("go")["error"].is_null());
    // The steers the root was sent, as its last request carries them.
    let steers = user_texts(a.requests().last().unwrap())
        .iter()
        .filter(|t| t.as_str().is_some_and(|t| t.starts_with("[Ultron] You have used")))
        .count();
    assert!(steers >= 1, "the brake steered the root");
    let stats = stored_stats(&a);
    assert_eq!(stats["nudges"]["toolRounds"], steers as u64, "{stats}");
    assert_eq!(stats["cells"]["root"]["count"], 3);
}

#[test]
fn session_core_stats_count_work_refused_at_a_root_limit() {
    let cell = "try:\n    await rlm.infer('q')\nexcept Exception as e:\n    print('refused:', e)";
    let a = actor(
        in_order(vec![tool("toolu_1", cell), text("done")]),
        Setup {
            vars: vec![("ULTRON_MAX_TOTAL_TURNS", "1".into())],
            ..Setup::default()
        },
    );
    assert!(a.prompt("go")["error"].is_null());
    let results = p_session_support::tool_results(a.requests().last().unwrap());
    assert!(results[0].to_string().contains("refused:"), "{results:?}");
    assert!(stored_stats(&a)["usageLimitBlocks"].as_u64().unwrap() >= 1);
}
