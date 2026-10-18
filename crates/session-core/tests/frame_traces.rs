//! Frame traces and the `rlm.frames` index are values of the session (inference.ts `createSessionFrameStore`:
//! `ultron.rlm.frames/<id>` and `ultron.rlm.frames/index`, handed to the inference runtime by session-worker.ts):
//! a resumed session lists and reads the frames of earlier cells, a tree fork copies them, and a branch fork starts
//! without them (fork-policy.ts `projectForkCurrentStateWrite`: every non-reserved namespace is tree-only).
//! Through the session actor over a private home and a scripted loopback model.

mod p_session_support;
mod reopen_support;

use p_session_support::{text, tool, tool_results};
use reopen_support::Home;
use serde_json::Value;
use session_core::core::Op;
use session_journal::{ForkOptions, SessionRepo, SystemClock};

const INFER: &str = "r = await rlm.infer('what is six times seven')\nprint('INFERRED', r)";
const LIST: &str = "fs = await rlm.frames()\nprint('LISTED', [(f['id'], f['task'], f['status']) for f in fs['frames']] if isinstance(fs, dict) else fs)\nif isinstance(fs, dict) and fs['frames']:\n    t = await rlm.frames(fs['frames'][0]['id'])\n    print('TRACE', t['task'], t['status'], 'forty-two' in t['value'])";

fn prompt(w: &reopen_support::Worker, text: &str) -> Value {
    w.call(Op::PromptAndWait {
        message: text.into(),
        images: Vec::new(),
    })
    .unwrap()
    .unwrap()
}

/// The root runs `cell` once per prompt and then answers; a frame's request (its system prompt says it is a frame)
/// gets "forty-two".
fn script(cell: &'static str) -> p_session_support::Script {
    Box::new(move |request, _| {
        if request["system"]
            .to_string()
            .contains("You are an inference frame inside Ultron")
        {
            return text("forty-two");
        }
        let last = request["messages"]
            .as_array()
            .and_then(|m| m.last())
            .cloned()
            .unwrap_or_default();
        let answered = last["content"]
            .as_array()
            .is_some_and(|blocks| blocks.iter().any(|b| b["type"] == "tool_result"));
        if answered { text("done") } else { tool("toolu_1", cell) }
    })
}

fn last_result(w: &reopen_support::Worker) -> String {
    let requests = w.requests();
    tool_results(requests.last().unwrap())
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

/// A session whose first cell ran one frame; returns the frame's id.
fn inferred(home: &Home) -> String {
    let w = home.open(script(INFER), &[], None);
    prompt(&w, "infer");
    let result = last_result(&w);
    w.close();
    assert!(result.contains("INFERRED forty-two"), "{result}");
    let id = home
        .writes()
        .iter()
        .rev()
        .find(|w| w["namespace"] == "ultron.rlm.frames" && w["key"] == "index" && w["op"] == "set")
        .and_then(|w| w["value"][0]["id"].as_str().map(str::to_string))
        .expect("the index is a session value");
    assert!(id.starts_with("frame-"), "{id}");
    id
}

fn listed(home: &Home) -> String {
    let w = home.open(script(LIST), &[], None);
    prompt(&w, "list");
    let result = last_result(&w);
    w.close();
    result
}

/// Fork the only session with `options`, then keep only the fork.
fn fork_only(home: &Home, options: &ForkOptions) {
    let repo = SessionRepo::new(&home.sessions(), SystemClock::shared());
    let source = repo.list(None).into_iter().next().unwrap();
    drop(repo.fork(&source, options, None).unwrap());
    std::fs::remove_file(&source.path).unwrap();
}

#[test]
fn session_core_frame_traces_survive_a_resume() {
    let home = Home::new(None);
    let id = inferred(&home);
    let result = listed(&home);
    assert!(
        result.contains(&format!("LISTED [('{id}', 'what is six times seven', 'complete')]")),
        "{result}"
    );
    assert!(
        result.contains("TRACE what is six times seven complete True"),
        "{result}"
    );
}

#[test]
fn session_core_frame_traces_are_copied_by_a_tree_fork() {
    let home = Home::new(None);
    let id = inferred(&home);
    fork_only(&home, &ForkOptions::Tree);
    let result = listed(&home);
    assert!(result.contains(&format!("LISTED [('{id}',")), "{result}");
}

#[test]
fn session_core_frame_traces_are_not_copied_by_a_branch_fork() {
    let home = Home::new(None);
    inferred(&home);
    fork_only(
        &home,
        &ForkOptions::Branch {
            branch: "main".into(),
            entry_id: None,
            any_tree_entry: false,
            before: false,
        },
    );
    assert!(home.values("ultron.rlm.frames").is_empty());
    let result = listed(&home);
    assert!(result.contains("LISTED []"), "{result}");
}
