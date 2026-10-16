//! Replays fixtures/budget_line.json (fixtures/generate-budget-line.mjs, the TypeScript session worker in RPC mode):
//! each `rlm` result ends with the budget line of the root turn it is charged to (`[budget left for this request:
//! N model turns, M tokens]`) under a turn cap, an explicit token cap, or both, from the environment or the
//! `rootBudget` settings; without a cap that shows, the result is unchanged.

mod p_session_support;

use p_session_support::{Setup, actor, in_order, text, tool, tool_results};
use serde_json::{Value, json};
use std::sync::LazyLock;

static FIXTURE: LazyLock<Value> =
    LazyLock::new(|| serde_json::from_str(include_str!("../fixtures/budget_line.json")).expect("fixture parses"));

fn budget_case(name: &str) {
    let case = &FIXTURE["cases"][name];
    let vars = case["env"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (&*Box::leak(k.clone().into_boxed_str()), v.as_str().unwrap().to_string()))
        .collect();
    let settings = (!case["settings"].is_null()).then(|| case["settings"].clone());
    let a = actor(
        in_order(vec![
            tool("toolu_1", "print('one')"),
            tool("toolu_2", "x = 2"),
            text("done"),
        ]),
        Setup {
            settings,
            vars,
            ..Setup::default()
        },
    );
    a.prompt("count");
    let requests = a.requests();
    assert_eq!(json!(requests.len()), case["requests"], "{name}: requests");
    assert_eq!(
        json!(tool_results(requests.last().unwrap())),
        case["toolResults"],
        "{name}: tool results"
    );
}

#[test]
fn session_core_budget_line_shows_turns_left_under_a_turn_cap_like_typescript() {
    budget_case("turn-cap");
}

#[test]
fn session_core_budget_line_shows_turns_and_tokens_under_both_caps_like_typescript() {
    budget_case("turn-and-token-caps");
}

#[test]
fn session_core_budget_line_shows_tokens_under_an_explicit_token_cap_like_typescript() {
    budget_case("token-cap-only");
}

#[test]
fn session_core_budget_line_reads_the_root_budget_settings_like_typescript() {
    budget_case("settings-turn-cap");
}

#[test]
fn session_core_budget_line_is_absent_without_caps_like_typescript() {
    budget_case("no-caps");
}
