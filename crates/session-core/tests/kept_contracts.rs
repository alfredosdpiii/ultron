//! Contracts of persistent children (wave 41, crate::kept_contracts): `rlm.spawn(brief, name=..., keep=True,
//! contract=..., hop=...)` from the root REPL of a real session (the session actor, its durable runtime and kernels)
//! against a scripted provider on loopback. Each answer of the kept child is held to its contract as a one-shot
//! child's end is: the first when `rlm.collect` reports it, each `rlm.ask` answer as it arrives; a retry hop asks it
//! again with its failures, a review hop spawns the reviewer, `returns` reads the verdict of that answer's own
//! `rlm.finish`. New in Rust: literal expectations.

mod p_session_support;

use p_session_support::{Setup, actor, text, tool, tool_results, user_texts};
use serde_json::{Value, json};

const RESULT: &str = "kept-result.json";
const BRIEF: &str = "KEPT: fix status";

fn texts(request: &Value) -> Vec<String> {
    user_texts(request)
        .iter()
        .filter_map(Value::as_str)
        .filter(|t| !t.starts_with("[clock]"))
        .map(str::to_string)
        .collect()
}

/// Whether the request's last message carries a tool result (the model answers it).
fn answering(request: &Value) -> bool {
    request["messages"]
        .as_array()
        .and_then(|m| m.last())
        .and_then(|m| m["content"].as_array())
        .is_some_and(|blocks| blocks.iter().any(|b| b["type"] == "tool_result"))
}

/// A child cell writing `status` to status.txt (and `extra.txt` when asked), then a passed verdict.
fn work(status: &str, extra: bool, finish: &str) -> String {
    let extra = if extra {
        "\nPath('extra.txt').write_text('x')"
    } else {
        ""
    };
    format!(
        "from pathlib import Path\nPath('status.txt').write_text({}){extra}\n{finish}",
        json!(format!("{status}\n"))
    )
}

const FINISH: &str =
    "print(await rlm.finish('passed', 'did it', evidence=['status.txt:1'], changed_files=['status.txt']))";

/// The root cell: spawn the kept child, collect its first answer twice, ask it `questions`, write everything.
fn root_cell(options: &str, questions: &[&str], reviewer: bool) -> String {
    let mut code = format!(
        "import asyncio, json\nh = await rlm.spawn({}, name='k', keep=True, {options})\nout = {{}}\nout['first'] = (await rlm.collect([h]))[0]['result']\nout['again'] = (await rlm.collect([h]))[0]['result']\nout['asks'] = []\n",
        json!(BRIEF)
    );
    for question in questions {
        code.push_str(&format!("out['asks'].append(await rlm.ask(h, {}))\n", json!(question)));
    }
    if reviewer {
        code.push_str("rid = out['first']['check']['contract']['hop']['review']['rlm_child_id']\nfor _ in range(12000):\n    try:\n        out['reviewer'] = (await rlm.collect([rid]))[0]['result']\n        break\n    except RuntimeError:\n        await asyncio.sleep(0.01)\n");
    }
    code.push_str(&format!(
        "open({}, 'w').write(json.dumps(out))\nprint('written')",
        json!(RESULT)
    ));
    code
}

struct Run {
    out: Value,
    child_texts: Vec<String>,
    child_results: Vec<Value>,
}

/// A session whose root runs [`root_cell`]; `child(prompt)` is the kept child's cell for its latest prompt (the brief
/// or a question), `None` for a plain answer.
fn run(
    options: &str,
    questions: &[&str],
    reviewer: bool,
    child: impl Fn(&str) -> Option<String> + Send + 'static,
) -> Run {
    let cell = root_cell(options, questions, reviewer);
    let script = Box::new(move |request: &Value, _| {
        let all = texts(request);
        let first = all.first().cloned().unwrap_or_default();
        if first.starts_with("ROOT") {
            return match answering(request) {
                false => tool("toolu_root", &cell),
                true => text("root done"),
            };
        }
        if first.contains("[Contract review]") {
            return text("reviewed");
        }
        if answering(request) {
            return text("answered");
        }
        let last = all.last().cloned().unwrap_or_default();
        match child(&last) {
            Some(code) => tool("toolu_k", &code),
            None => text("answered without a cell"),
        }
    });
    let a = actor(script, Setup::default());
    let ended = a.prompt("ROOT: keep a child under contract");
    assert_eq!(ended["error"], Value::Null, "{ended}");
    let written = std::fs::read_to_string(a.cwd.join(RESULT)).unwrap_or_else(|_| {
        let results: Vec<Value> = a.requests().iter().flat_map(tool_results).collect();
        panic!("the root cell wrote no result: {results:#?}")
    });
    let child: Vec<Value> = a
        .requests()
        .into_iter()
        .filter(|r| {
            texts(r)
                .first()
                .is_some_and(|t| t.contains(BRIEF) && !t.contains("[Contract review]"))
        })
        .collect();
    Run {
        out: serde_json::from_str(&written).unwrap(),
        child_texts: child.iter().flat_map(texts).collect(),
        child_results: child.iter().flat_map(tool_results).collect(),
    }
}

const GREP: &str = r#"contract={'checks': [{'run': 'grep -q fixed status.txt'}]}"#;

#[test]
fn session_core_w18s_kept_child_each_answer_is_held_to_its_contract() {
    let r = run(GREP, &["Q-GOOD", "Q-BAD"], false, |prompt| {
        Some(match prompt {
            p if p.contains(BRIEF) => work("wrong", false, FINISH),
            "Q-GOOD" => work("fixed", false, FINISH),
            _ => work("broken", false, FINISH),
        })
    });
    let first = &r.out["first"];
    assert_eq!(first["status"], "failed", "{first}");
    assert_eq!(
        first["error"], "Contract failed: run \"grep -q fixed status.txt\": exit 1",
        "{first}"
    );
    assert_eq!(first["check"]["contract"]["verdict"], "contract_failed", "{first}");
    assert_eq!(first["check"]["contract"]["hop"], "report", "{first}");
    assert_eq!(first["check"]["contract"]["attempts"], 1, "{first}");
    // The child's own claim stays beside the host's verdict.
    assert_eq!(first["verdict"]["status"], "passed", "{first}");
    // Checked once: the next collect reports the same checked answer.
    assert_eq!(r.out["again"], *first);
    let good = &r.out["asks"][0];
    assert_eq!(good["status"], "succeeded", "{good}");
    assert_eq!(good["value"], "answered", "{good}");
    assert_eq!(good["check"]["contract"]["verdict"], "verified", "{good}");
    assert!(good["id"].is_string(), "{good}");
    let bad = &r.out["asks"][1];
    assert_eq!(bad["status"], "failed", "{bad}");
    assert_eq!(
        bad["error"], "Contract failed: run \"grep -q fixed status.txt\": exit 1",
        "{bad}"
    );
    // The brief is led by what the host will check.
    assert!(
        r.child_texts[0].starts_with("Contract: when you finish, the host itself runs these checks in your working directory: {\"run\":\"grep -q fixed status.txt\""),
        "{:?}",
        r.child_texts
    );
}

#[test]
fn session_core_w18s_kept_child_retry_hop_asks_it_again_with_its_failures() {
    let r = run(&format!("{GREP}, hop={{'retry': 1}}"), &[], false, |prompt| {
        Some(match prompt {
            p if p.starts_with("[Contract] Attempt 1") => work("fixed", false, FINISH),
            _ => work("wrong", false, FINISH),
        })
    });
    let first = &r.out["first"];
    assert_eq!(first["status"], "succeeded", "{first}");
    assert_eq!(first["check"]["contract"]["verdict"], "verified", "{first}");
    assert_eq!(first["check"]["contract"]["attempts"], 2, "{first}");
    assert_eq!(first["check"]["contract"]["hop"], json!({"retry": 1}), "{first}");
    assert_eq!(
        first["check"]["contract"]["previous"],
        json!([["run \"grep -q fixed status.txt\": exit 1"]]),
        "{first}"
    );
    let note = r
        .child_texts
        .iter()
        .find(|t| t.starts_with("[Contract] Attempt 1"))
        .unwrap_or_else(|| panic!("no retry question in {:?}", r.child_texts));
    assert_eq!(
        note,
        "[Contract] Attempt 1 did not pass the host's contract checks:\n- run \"grep -q fixed status.txt\": exit 1\nFix these in your working directory, then call rlm.finish again; the host re-runs every check when you end."
    );
}

#[test]
fn session_core_w18s_kept_child_returns_reads_each_answers_own_verdict() {
    let options = r#"contract={'returns': {'type': 'object', 'required': ['n']}}"#;
    let r = run(options, &["Q-NO-FINISH"], false, |prompt| {
        if !prompt.contains(BRIEF) {
            return None;
        }
        Some(
            "try:\n    await rlm.finish('passed', 'x', evidence=['data.txt:1 counted'], outputs={})\nexcept Exception as e:\n    print('REJECTED', str(e).splitlines()[1])\nprint(await rlm.finish('passed', 'x', evidence=['data.txt:1 counted'], outputs={'n': 1}))"
                .to_string(),
        )
    });
    let rejected = r
        .child_results
        .iter()
        .map(Value::to_string)
        .find(|t| t.contains("REJECTED"))
        .unwrap_or_default();
    assert!(
        rejected.contains("REJECTED - outputs: must have required properties n (contract.returns)"),
        "{rejected}"
    );
    let first = &r.out["first"];
    assert_eq!(first["check"]["contract"]["verdict"], "verified", "{first}");
    assert_eq!(first["verdict"]["outputs"], json!({"n": 1}), "{first}");
    // The question's answer had no rlm.finish of its own: the first answer's verdict does not count for it.
    let ask = &r.out["asks"][0];
    assert_eq!(ask["status"], "failed", "{ask}");
    assert_eq!(
        ask["check"]["contract"]["failed"],
        json!(["returns: the child ended without rlm.finish, so its outputs were never given"]),
        "{ask}"
    );
}

#[test]
fn session_core_w18s_kept_child_review_hop_spawns_the_reviewer() {
    let r = run(
        &format!("{GREP}, hop={{'review': 'Check the status fix.'}}"),
        &[],
        true,
        |_| Some(work("wrong", false, FINISH)),
    );
    let first = &r.out["first"];
    assert_eq!(first["status"], "failed", "{first}");
    let reviewer = first["check"]["contract"]["hop"]["review"]["rlm_child_id"]
        .as_str()
        .unwrap();
    assert_eq!(
        first["check"]["contract"]["hop"]["review"]["name"], "k-review",
        "{first}"
    );
    assert_eq!(
        first["error"],
        format!("Contract failed: run \"grep -q fixed status.txt\": exit 1 (reviewer {reviewer} spawned)"),
        "{first}"
    );
    assert_eq!(r.out["reviewer"]["value"], "reviewed", "{}", r.out);
}

#[test]
fn session_core_w18s_kept_child_workspace_checks_compare_with_its_spawn() {
    let options = r#"contract={'checks': [{'changed_files_within': ['status.txt']}, {'no_new_dependencies': True}]}"#;
    let r = run(options, &["Q-MORE"], false, |prompt| {
        Some(match prompt {
            p if p.contains(BRIEF) => work("fixed", false, FINISH),
            _ => work("fixed", true, FINISH),
        })
    });
    let first = &r.out["first"];
    assert_eq!(first["check"]["contract"]["verdict"], "verified", "{first}");
    assert_eq!(
        first["check"]["contract"]["checks"][0]["evidence"], "1 changed file(s), all within [\"status.txt\"]",
        "{first}"
    );
    // Cumulative since the spawn: the question's answer added extra.txt.
    let more = &r.out["asks"][0];
    assert_eq!(
        more["check"]["contract"]["failed"],
        json!(["changed_files_within [\"status.txt\"]: changed outside [\"status.txt\"]: extra.txt"]),
        "{more}"
    );
}
