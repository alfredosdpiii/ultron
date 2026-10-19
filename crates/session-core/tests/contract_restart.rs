//! Delegation contracts survive a restart: a child spawned with `role=` / `contract=` / `hop=` whose worker hands
//! the session over while the child is still running is reattached by the next worker, which still holds its end to
//! the contract the session kept (`ultron.subagent-contracts`): a violating result is still `contract_failed`, a
//! retry hop still re-runs it, a passing one is `verified`. A child spawned without a contract (as every child of a
//! session from before kept contracts) is reattached and ends unchecked, as before. Two in-process actors over one
//! private session file; nothing is killed and no signal is sent. New in Rust: literal expectations.

mod p_session_support;
mod reopen_support;

use p_session_support::{HANG, Script, text, tool, tool_results, user_texts};
use reopen_support::{Home, Worker};
use serde_json::{Value, json};
use session_core::core::Op;
use std::path::Path;
use std::time::{Duration, Instant};

const HANDLE: &str = "handle.txt";
const STARTED: &str = "child-started";
const RELEASE: &str = "release";
const RESULT: &str = "contract-result.json";

fn texts(request: &Value) -> Vec<String> {
    user_texts(request)
        .iter()
        .filter_map(Value::as_str)
        .filter(|t| !t.starts_with("[clock]"))
        .map(str::to_string)
        .collect()
}

/// The first worker's root cell: spawn with `options` and keep the handle in a file (the next worker reads it).
fn spawn_cell(options: &str) -> String {
    format!(
        "h = await rlm.spawn('CHILD: fix status', name='c', {options})\nopen({}, 'w').write(h.rlm_child_id)\nprint('spawned')",
        json!(HANDLE)
    )
}

/// The next worker's root cell: wait for the reattached child (a poll bounded at about 120 s, a hang guard) and
/// write its result.
fn collect_cell() -> String {
    format!(
        "import asyncio, json\nhid = open({}).read()\nr = None\nfor _ in range(12000):\n    try:\n        r = (await rlm.collect([hid]))[0]['result']\n        break\n    except RuntimeError:\n        await asyncio.sleep(0.01)\nopen({}, 'w').write(json.dumps(r))\nprint('collected')",
        json!(HANDLE),
        json!(RESULT)
    )
}

/// A child cell that records `status` and a passed verdict.
fn work_cell(status: &str) -> String {
    format!(
        "from pathlib import Path\nPath('status.txt').write_text({})\nprint(await rlm.finish('passed', 'did the work', evidence=['status.txt:1 written'], changed_files=['status.txt']))",
        json!(format!("{status}\n"))
    )
}

/// The first worker's child: it starts, says so, and is still running its cell when the worker hands over.
fn blocked_cell() -> String {
    format!(
        "import asyncio, os\nopen({}, 'w').write('1')\nfor _ in range(12000):\n    if os.path.exists({}):\n        break\n    await asyncio.sleep(0.01)\nprint('released')",
        json!(STARTED),
        json!(RELEASE)
    )
}

fn is_child(request: &Value) -> bool {
    texts(request).first().is_some_and(|t| t.contains("CHILD: fix status"))
}

/// The first worker: the root spawns, the child blocks in its first cell.
fn first_script(options: &str) -> Script {
    let spawn = spawn_cell(options);
    Box::new(move |request, _| {
        let results = tool_results(request).len();
        if is_child(request) {
            return match results {
                0 => tool("toolu_c0", &blocked_cell()),
                _ => text("(the first worker never sees the child continue)"),
            };
        }
        match results {
            0 => tool("toolu_root", &spawn),
            _ => text("spawned"),
        }
    })
}

/// The next worker: the reattached child (its interrupted cell's result first) records `first`; a retry of it
/// records `retry`; the root collects.
fn next_script(first: &'static str, retry: &'static str) -> Script {
    Box::new(move |request, _| {
        let results = tool_results(request).len();
        let all = texts(request);
        if is_child(request) {
            let retried = all.first().is_some_and(|t| t.contains("[Contract] Attempt 1"));
            return match (retried, results) {
                (false, 1) => tool("toolu_c1", &work_cell(first)),
                (true, 0) => tool("toolu_r1", &work_cell(retry)),
                _ => text("child done"),
            };
        }
        let collecting = all.last().is_some_and(|t| t.starts_with("ROOT2"));
        match (collecting, results) {
            (true, 1) => tool("toolu_collect", &collect_cell()),
            _ => text("root done"),
        }
    })
}

fn wait_file(path: &Path) {
    let end = Instant::now() + HANG;
    while !path.exists() {
        assert!(Instant::now() < end, "timed out waiting for {}", path.display());
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn prompt(worker: &Worker, message: &str) -> Value {
    worker
        .call(Op::PromptAndWait {
            message: message.into(),
            images: Vec::new(),
        })
        .unwrap()
        .unwrap()
}

struct Restarted {
    result: Value,
    kept: Vec<Value>,
    child_briefs: Vec<String>,
}

/// Spawn with `options` in one worker, hand over while the child runs, collect it in the next.
fn across_restart(options: &str, first: &'static str, retry: &'static str) -> Restarted {
    let home = Home::new(None);
    let worker = home.open(first_script(options), &[], None);
    let ended = prompt(&worker, "ROOT1: delegate with a contract");
    assert_eq!(ended["error"], Value::Null, "{ended}");
    wait_file(&home.cwd.join(STARTED));
    worker.hand_over();
    // Lets the first worker's cell end if its kernel outlived the hand-over.
    std::fs::write(home.cwd.join(RELEASE), "1").unwrap();

    let next = home.open(next_script(first, retry), &[], None);
    let ended = prompt(&next, "ROOT2: collect the child");
    assert_eq!(ended["error"], Value::Null, "{ended}");
    let written = std::fs::read_to_string(home.cwd.join(RESULT)).unwrap_or_else(|_| {
        let seen: Vec<(Vec<String>, Vec<Value>)> =
            next.requests().iter().map(|r| (texts(r), tool_results(r))).collect();
        panic!("the root cell wrote no result: {seen:#?}")
    });
    let result: Value = serde_json::from_str(&written).unwrap();
    assert!(!result.is_null(), "the reattached child never ended");
    let child_briefs = next
        .requests()
        .iter()
        .filter(|r| is_child(r))
        .filter_map(|r| texts(r).first().cloned())
        .collect();
    drop(next);
    Restarted {
        result,
        kept: home.values("ultron.subagent-contracts"),
        child_briefs,
    }
}

#[test]
fn session_core_w17e_a_violating_child_reattached_after_a_restart_is_still_contract_failed() {
    let r = across_restart(
        r#"role='fixer', contract={'checks': [{'run': 'grep -q fixed status.txt'}]}"#,
        "wrong",
        "never",
    );
    let child = &r.result;
    assert_eq!(child["status"], "failed", "{child}");
    assert_eq!(child["check"]["contract"]["verdict"], "contract_failed", "{child}");
    assert_eq!(
        child["check"]["contract"]["failed"],
        json!(["run \"grep -q fixed status.txt\": exit 1"]),
        "{child}"
    );
    assert_eq!(
        child["error"], "Contract failed: run \"grep -q fixed status.txt\": exit 1",
        "{child}"
    );
    assert_eq!(child["check"]["contract"]["hop"], "report", "{child}");
    // The child's own claim stays beside the host's verdict.
    assert_eq!(child["verdict"]["status"], "passed", "{child}");
    // The session kept the contract as the spawn described it, once.
    assert_eq!(
        r.kept,
        vec![json!({
            "role": "fixer",
            "checks": [{"run": "grep -q fixed status.txt", "expect": {"exit": 0}, "timeout_ms": 120000}],
            "returns": null,
            "hop": "report",
        })]
    );
}

#[test]
fn session_core_w17e_a_retry_hop_still_reruns_a_reattached_child_and_its_passing_result_is_verified() {
    let r = across_restart(
        r#"contract={'checks': [{'run': 'grep fixed status.txt', 'expect': {'stdout_contains': 'fixed'}}], 'returns': {'type': 'object'}}, hop={'retry': 1}"#,
        "wrong",
        "fixed",
    );
    let child = &r.result;
    assert_eq!(child["status"], "succeeded", "{child}");
    assert_eq!(child["check"]["contract"]["verdict"], "verified", "{child}");
    assert_eq!(child["check"]["contract"]["attempts"], 2, "{child}");
    assert_eq!(child["check"]["contract"]["hop"], json!({"retry": 1}), "{child}");
    assert_eq!(
        child["check"]["contract"]["previous"],
        json!([["run \"grep fixed status.txt\": exit 1; stdout does not contain \"fixed\""]]),
        "{child}"
    );
    // The retry's brief is the reattached child's with the failure appended.
    let retry = r
        .child_briefs
        .iter()
        .find(|b| b.contains("[Contract] Attempt 1"))
        .unwrap_or_else(|| panic!("no retry brief in {:?}", r.child_briefs));
    assert!(
        retry.starts_with("Contract: when you finish, the host itself runs these checks"),
        "{retry}"
    );
}

#[test]
fn session_core_w17e_a_child_without_a_kept_contract_is_reattached_and_ends_unchecked() {
    let r = across_restart("timeout_ms=600000", "wrong", "never");
    let child = &r.result;
    assert_eq!(child["status"], "succeeded", "{child}");
    assert_eq!(child["check"]["contract"], Value::Null, "{child}");
    assert_eq!(r.kept, Vec::<Value>::new());
}
