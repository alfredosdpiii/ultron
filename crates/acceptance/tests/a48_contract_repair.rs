//! A48 (partial): a malformed frame answer is re-asked within budget and returns a valid value; exhaustion yields an
//! Incomplete with evidence and the root turn continues.
//! Ported from the end-to-end case of packages/coding-agent/test/ultron-a47-large-input.test.ts: the `ultron` binary
//! (print mode instead of RPC) against a scripted provider that serves both the root turn and the inference frames.
//! The three host-level cases of ultron-a47-a50-bounded-inference.test.ts are ported in crates/inference/tests/units.rs.
//! Not ported: frame traces (`rlm.frames`, inspected by trace id) have no Rust store yet.

mod common;

use common::{Install, Provider, Reply, stdout_of};
use serde_json::Value;

const LINES: usize = 16_000;

fn log_file() -> String {
    let mut text = String::new();
    for index in 0..LINES {
        let error = index % 13 == 0 || index % 29 == 0;
        text.push_str(&format!(
            "2026-09-26T03:{:02}:00Z id=L{index:06} {}\n",
            index % 60,
            if error {
                "ERROR upstream timeout"
            } else {
                "INFO request served"
            }
        ));
    }
    text
}

const REPAIR_CELL: &str = "import json
h = await rlm.load('big.log')
v = await rlm.infer('REPAIR: how many lines contain ERROR?', h.lines(0, 50), contract=int)
x = await rlm.infer('NEVER: how many lines contain ERROR?', [h.lines(0, 50)], contract=int, max_repairs=1)
print('RESULT ' + json.dumps({'value': v, 'expected': h.lines(0, 50).text.count('ERROR'), 'incomplete': isinstance(x, Incomplete), 'falsy': not x, 'status': x.status, 'trace': x.trace_id, 'last': x.last_outputs, 'spent': x.spent}))";

/// The text of view 1 in a frame's first user message.
fn view_of(message: &str) -> &str {
    let Some(start) = message.find("--- view 1: ") else {
        return "";
    };
    let body = &message[start..];
    let body = &body[body.find("---\n").map_or(0, |i| i + 4)..];
    &body[..body.find("\n--- end of view 1 ---").unwrap_or(body.len())]
}

#[test]
fn acceptance_a48_frame_repair_and_exhaustion_end_to_end_ported_part() {
    let provider = Provider::start(|request| {
        if request.is_frame() {
            let users = request.users();
            let task = &users[0];
            if task.contains("NEVER:") {
                return Reply::Text("I cannot tell.".into());
            }
            if task.contains("REPAIR:") && users.len() == 1 {
                return Reply::Text("There are a few, maybe four?".into());
            }
            return Reply::Text(view_of(task).matches("ERROR").count().to_string());
        }
        if request.tool_results().is_empty() {
            Reply::Rlm(REPAIR_CELL.into())
        } else {
            Reply::Text("DONE".into())
        }
    });
    let install = Install::new(&provider);
    std::fs::write(install.project().join("big.log"), log_file()).unwrap();
    // The root turn continued after the Incomplete and finished normally.
    assert_eq!(stdout_of(&install.prompt("A48: repair and exhaustion", &[])), "DONE\n");

    let roots = provider.roots();
    assert_eq!(roots.len(), 2);
    let line = roots[1].tool_results()[0]
        .lines()
        .find_map(|line| line.strip_prefix("RESULT "))
        .map(str::to_string)
        .expect("a RESULT line");
    let result: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(result["value"], result["expected"]);
    assert!(result["expected"].as_u64().unwrap() > 0);
    assert_eq!(result["incomplete"], true);
    assert_eq!(result["falsy"], true);
    assert_eq!(result["status"], "contract_unmet");
    assert_eq!(result["spent"]["calls"], 2);
    assert_eq!(result["last"], serde_json::json!(["I cannot tell.", "I cannot tell."]));
    assert!(result["trace"].as_str().is_some_and(|t| t.starts_with("frame-")));

    let repaired: Vec<_> = provider
        .frames()
        .into_iter()
        .filter(|r| r.users()[0].contains("REPAIR:"))
        .collect();
    assert_eq!(repaired.len(), 2);
    assert!(
        repaired[1]
            .users()
            .last()
            .unwrap()
            .contains("does not satisfy the contract")
    );
    // Frames are private: no root system prompt, no tools, no root transcript.
    for frame in provider.frames() {
        assert!(!frame.system().contains("rlm"));
        assert!(frame.body["tools"].as_array().is_none_or(Vec::is_empty));
        assert!(!frame.raw.contains("A48: repair"));
    }
}
