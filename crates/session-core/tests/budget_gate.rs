//! Per-root tree limits on every lane loop (session-worker.ts `before_tool` / `before_request` / `transform_context`
//! budget hooks; ported from test/ultron-tree-budget.test.ts): once a root's tree reached its turn or token limit,
//! each lane run still going gets one final request with tool choice "none" and the `[Ultron]` note, then nothing
//! more; a cost cap allows nothing more at all. Requests are counted at a scripted provider.

mod p_session_support;

use p_session_support::{Setup, actor, in_order, text, tool, tool_results, user_texts};
use serde_json::Value;
use session_core::core::Op;

const NOTE: &str =
    "Tools are off for your last reply: answer now with what you have, and say what remains unknown or unfinished.";

/// A request sent with tools off: a lane's one final request after its tree's limit.
fn is_final(request: &Value) -> bool {
    request["tool_choice"]["type"] == "none"
}

/// The first user text of a request (clock lines skipped).
fn first_user(request: &Value) -> String {
    user_texts(request)
        .iter()
        .filter_map(Value::as_str)
        .find(|text| !text.starts_with("[clock]"))
        .unwrap_or_default()
        .to_string()
}

fn is_child(request: &Value) -> bool {
    first_user(request).starts_with("CHILD")
}

fn shape(requests: &[Value]) -> String {
    requests
        .iter()
        .map(|r| {
            format!(
                "{}{}",
                if is_child(r) { "C" } else { "R" },
                if is_final(r) { "*" } else { "" }
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A model that keeps calling its REPL (ignoring tool choice); past 40 requests it answers, so a run the limits fail
/// to stop ends (and the test fails) instead of looping.
fn looping(index: usize, code: &str) -> String {
    if index >= 40 {
        text("RUNAWAY")
    } else {
        tool(&format!("toolu_{index}"), code)
    }
}

fn stored_stats(a: &p_session_support::Actor) -> Value {
    a.call(Op::Describe).unwrap();
    a.writes()
        .iter()
        .rev()
        .find(|w| w["namespace"] == "ultron.module" && w["key"] == "stats" && w["op"] == "set")
        .map(|w| w["value"].clone())
        .expect("the counters are stored")
}

fn results_text(request: &Value) -> String {
    tool_results(request)
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn session_core_budget_gate_turn_limit_grants_one_final_request_then_ends_the_run() {
    // A model that never stops calling its REPL (and ignores tool choice).
    let a = actor(
        Box::new(|_, index| looping(index, &format!("print('round {index}')"))),
        Setup {
            vars: vec![
                ("ULTRON_MAX_TOTAL_TURNS", "2".into()),
                ("ULTRON_TOOL_ROUNDS_NUDGE", "0".into()),
                ("ULTRON_ASYNC_EVENTS", "off".into()),
            ],
            ..Setup::default()
        },
    );
    let ended = a.prompt("go");
    let requests = a.requests();
    // Two turns with tools, then the one final request; its tool call is refused and ends the run.
    assert_eq!(shape(&requests), "R R R*", "{ended}");
    let last = requests.last().unwrap();
    let reason_start = "Usage turn limit reached for root turn:";
    let notes: Vec<String> = user_texts(last)
        .iter()
        .filter_map(|t| t.as_str().map(str::to_string))
        .filter(|t| t.starts_with("[Ultron] "))
        .collect();
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(
        notes[0].starts_with(&format!("[Ultron] {reason_start}")),
        "{}",
        notes[0]
    );
    assert!(
        notes[0].ends_with(&format!(
            ": 2 of 2 model turns used (max_total_turns, ULTRON_MAX_TOTAL_TURNS). No further model work is admitted for this root; answer with what you have. {NOTE}"
        )),
        "{}",
        notes[0]
    );
    // The second cell never ran: its result is the limit text and the note.
    let results = results_text(last);
    assert!(results.contains("round 0"), "{results}");
    assert!(!results.contains("round 1"), "{results}");
    assert!(results.contains(reason_start) && results.contains(NOTE), "{results}");
    // The final request's note is never stored in the transcript.
    let transcript = a.writes().iter().map(Value::to_string).collect::<String>();
    assert!(
        !transcript.contains("[Ultron] Usage turn limit"),
        "the note is not stored"
    );
    // Both refused cells are counted; the grant of the final request is not.
    assert_eq!(stored_stats(&a)["usageLimitBlocks"], 2);
    // The next prompt is a root turn of its own, with a fresh budget.
    a.prompt("again");
    let requests = a.requests();
    assert!(!is_final(&requests[3]), "a new root turn is not spent");
}

#[test]
fn session_core_budget_gate_final_request_answer_ends_the_run_with_it() {
    let a = actor(
        in_order(vec![
            tool("toolu_1", "x = 1"),
            tool("toolu_2", "x = 2"),
            text("ANSWERED"),
        ]),
        Setup {
            vars: vec![
                ("ULTRON_MAX_TOTAL_TURNS", "2".into()),
                ("ULTRON_TOOL_ROUNDS_NUDGE", "0".into()),
            ],
            ..Setup::default()
        },
    );
    let ended = a.prompt("go");
    assert!(ended["error"].is_null(), "{ended}");
    assert_eq!(shape(&a.requests()), "R R R*");
    assert_eq!(a.last_text(), "ANSWERED");
}

#[test]
fn session_core_budget_gate_token_limit_is_a_turn_like_limit() {
    // Each scripted response reports 10 input + 7 output tokens: the second crosses 30.
    let a = actor(
        Box::new(|_, index| looping(index, "x = 1")),
        Setup {
            vars: vec![
                ("ULTRON_MAX_TOTAL_TOKENS", "30".into()),
                ("ULTRON_TOOL_ROUNDS_NUDGE", "0".into()),
            ],
            ..Setup::default()
        },
    );
    a.prompt("go");
    let requests = a.requests();
    assert_eq!(shape(&requests), "R R R*");
    let results = results_text(requests.last().unwrap());
    assert!(
        results.contains("tokens used (max_total_tokens, ULTRON_MAX_TOTAL_TOKENS)"),
        "{results}"
    );
}

#[test]
fn session_core_budget_gate_cost_cap_stops_at_once() {
    // claude-sonnet-4-5 at $3/M input and $15/M output: one response of 10 + 7 tokens costs $0.000135.
    let a = actor(
        Box::new(|_, index| looping(index, "print('ran')")),
        Setup {
            vars: vec![
                ("ULTRON_MAX_COST_USD", "0.0001".into()),
                ("ULTRON_TOOL_ROUNDS_NUDGE", "0".into()),
            ],
            ..Setup::default()
        },
    );
    a.prompt("go");
    // A cost cap is money: no final request, nothing after the response that reached it.
    let requests = a.requests();
    assert_eq!(shape(&requests), "R");
    let transcript = a.writes().iter().map(Value::to_string).collect::<String>();
    assert!(
        transcript.contains("Usage cost cap reached for root turn:")
            && transcript.contains(": spent $0.000135 of $0.0001 (ULTRON_MAX_COST_USD)."),
        "the refused cell's result is the cap's text"
    );
    assert!(!transcript.contains(NOTE), "no final answer at a cost cap");
    assert!(!transcript.contains("\\nran"), "the cell never ran");
    assert_eq!(stored_stats(&a)["usageLimitBlocks"], 1);
}

/// Spawn a runaway child and wait in the cell until it ended (the host does not let `rlm.collect` wait, so the cell
/// polls it; the bound is a hang guard of about 120 s).
const SPAWN_AND_WAIT: &str = "import asyncio\nh = await rlm.spawn(\"CHILD: loop\", name=\"runaway\")\nr = None\nfor _ in range(12000):\n    try:\n        r = (await rlm.collect([h]))[0]\n        break\n    except RuntimeError:\n        await asyncio.sleep(0.01)\nprint(\"CHILD RESULT\", r)";

#[test]
fn session_core_budget_gate_counts_a_childs_turns_against_the_root() {
    // The root spawns a sub-agent that never stops calling its REPL, then answers.
    let a = actor(
        Box::new(|request, index| {
            if is_child(request) {
                return looping(index, "print('child round')");
            }
            if tool_results(request).is_empty() {
                return tool("toolu_r", SPAWN_AND_WAIT);
            }
            text("ROOT ANSWERED")
        }),
        Setup {
            vars: vec![
                ("ULTRON_MAX_TOTAL_TURNS", "5".into()),
                ("ULTRON_TOOL_ROUNDS_NUDGE", "0".into()),
                ("ULTRON_ASYNC_EVENTS", "off".into()),
            ],
            ..Setup::default()
        },
    );
    a.prompt("ROOT: delegate");
    // Five turns with tools (the root's spawn and four child rounds), then one final request per lane.
    let requests = a.requests();
    assert_eq!(shape(&requests), "R C C C C C* R*");
    // The child answered its final request with a tool call (this scripted model ignores tool choice), so it failed
    // with the limit text.
    let results = results_text(requests.last().unwrap());
    assert!(results.contains("'status': 'failed'"), "{results}");
    assert!(
        results.contains("'error': 'Usage turn limit reached for root turn:")
            && results.contains(" of 5 model turns used (max_total_turns, ULTRON_MAX_TOTAL_TURNS)."),
        "{results}"
    );
    assert_eq!(a.last_text(), "ROOT ANSWERED");
}

/// The system prompt of a request (Anthropic's `system` blocks or string).
fn system(request: &Value) -> String {
    match &request["system"] {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks.iter().filter_map(|b| b["text"].as_str()).collect(),
        _ => String::new(),
    }
}

/// Provider-reported tokens of one scripted response (10 input + 5 or 7 output).
fn reported(reply: &str) -> u64 {
    if reply.contains("tool_use") { 17 } else { 15 }
}

#[test]
fn session_core_budget_gate_a_frames_responses_count_once_in_the_tree() {
    let cell = "import json\nv = await rlm.infer(\"the number\", context=[\"forty-two\"], contract={\"type\": \"integer\"})\ns = await agents.status()\nprint(\"SPEND\", json.dumps(s[\"spend\"]), \"VALUE\", v)";
    let replies = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = replies.clone();
    let a = actor(
        Box::new(move |request, _| {
            let reply = if system(request).starts_with("You are an inference frame") {
                text("42")
            } else if tool_results(request).is_empty() {
                tool("toolu_r", cell)
            } else {
                text("ROOT SAW IT")
            };
            seen.lock().unwrap().push(reply.clone());
            reply
        }),
        Setup {
            vars: vec![("ULTRON_MAX_TOTAL_TOKENS", "1000000".into())],
            ..Setup::default()
        },
    );
    a.prompt("ROOT: infer");
    let requests = a.requests();
    let results = results_text(requests.last().unwrap());
    let start = results.find("SPEND ").expect("the cell printed the spend") + "SPEND ".len();
    let end = results[start..].find(" VALUE 42").expect("the frame answered") + start;
    let spend: Value = serde_json::from_str(&results[start..end].replace("\\\"", "\"")).unwrap();
    // Everything before the final answer: the root's first turn and the frame, each counted exactly once.
    let replies = replies.lock().unwrap();
    let before = &replies[..replies.len() - 1];
    assert_eq!(spend["turns"], 2, "{spend}");
    assert_eq!(
        spend["tokens"],
        before.iter().map(|r| reported(r)).sum::<u64>(),
        "{spend}"
    );
}
