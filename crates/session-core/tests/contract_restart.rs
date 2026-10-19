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

/// Whether the request's last message carries a tool result (the model answers it).
fn answering(request: &Value) -> bool {
    request["messages"]
        .as_array()
        .and_then(|m| m.last())
        .and_then(|m| m["content"].as_array())
        .is_some_and(|blocks| blocks.iter().any(|b| b["type"] == "tool_result"))
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
    across_restart_in(options, first, retry, false)
}

/// The session's git: no global or system configuration (HOME is the private home).
const GIT_VARS: [(&str, &str); 2] = [("GIT_CONFIG_GLOBAL", "/dev/null"), ("GIT_CONFIG_NOSYSTEM", "1")];

fn git(home: &Home, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(["-c", "user.email=t@t", "-c", "user.name=t"])
        .args(args)
        .current_dir(&home.cwd)
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("HOME", &home.root)
        .envs(GIT_VARS)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// [`across_restart`]; with `repo` the working directory is a Git repository whose one commit ignores the test's own
/// marker files and Loki's.
fn across_restart_in(options: &str, first: &'static str, retry: &'static str, repo: bool) -> Restarted {
    let home = Home::new(None);
    if repo {
        std::fs::write(home.cwd.join("README.md"), "repo\n").unwrap();
        std::fs::write(
            home.cwd.join(".gitignore"),
            // Loki's own setup files too (it starts in a Git work tree).
            format!("{HANDLE}\n{STARTED}\n{RELEASE}\n{RESULT}\n.loki/\n"),
        )
        .unwrap();
        git(&home, &["init", "-q"]);
        git(&home, &["add", "-A"]);
        git(&home, &["commit", "-q", "-m", "init"]);
    }
    let worker = home.open(first_script(options), &GIT_VARS, None);
    let ended = prompt(&worker, "ROOT1: delegate with a contract");
    assert_eq!(ended["error"], Value::Null, "{ended}");
    wait_file(&home.cwd.join(STARTED));
    worker.hand_over();
    // Lets the first worker's cell end if its kernel outlived the hand-over.
    std::fs::write(home.cwd.join(RELEASE), "1").unwrap();

    let next = home.open(next_script(first, retry), &GIT_VARS, None);
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

/// The kept contract's Git baseline (wave 41): `{"commit": <40 hex>, "status": <blob id>, "dirty": {}}`.
fn assert_clean_baseline(kept: &[Value]) {
    assert_eq!(kept.len(), 1, "{kept:?}");
    let baseline = &kept[0]["baseline"];
    let hex = |v: &Value| {
        v.as_str()
            .is_some_and(|t| t.len() == 40 && t.chars().all(|c| c.is_ascii_hexdigit()))
    };
    assert!(hex(&baseline["commit"]) && hex(&baseline["status"]), "{baseline}");
    assert_eq!(baseline["dirty"], json!({}), "{baseline}");
}

#[test]
fn session_core_w18s_changed_files_within_after_a_restart_compares_with_the_kept_git_baseline() {
    let r = across_restart_in(
        r#"contract={'checks': [{'changed_files_within': ['status.txt']}, {'no_new_dependencies': True}]}"#,
        "fixed",
        "never",
        true,
    );
    let child = &r.result;
    assert_eq!(child["status"], "succeeded", "{child}");
    assert_eq!(child["check"]["contract"]["verdict"], "verified", "{child}");
    let checks = child["check"]["contract"]["checks"].as_array().unwrap();
    assert_eq!(
        checks[0]["evidence"], "1 changed file(s), all within [\"status.txt\"]",
        "{child}"
    );
    assert_eq!(checks[0]["changed"], json!(["status.txt"]), "{child}");
    assert_eq!(
        checks[1]["evidence"], "no dependency manifest or lockfile changed",
        "{child}"
    );
    assert_clean_baseline(&r.kept);
    assert_eq!(
        r.kept[0]["checks"],
        json!([{"changed_files_within": ["status.txt"]}, {"no_new_dependencies": true}])
    );
}

#[test]
fn session_core_w18s_an_out_of_scope_edit_after_a_restart_is_still_contract_failed() {
    let r = across_restart_in(
        r#"contract={'checks': [{'changed_files_within': ['docs/**']}]}"#,
        "wrong",
        "never",
        true,
    );
    let child = &r.result;
    assert_eq!(child["status"], "failed", "{child}");
    assert_eq!(
        child["check"]["contract"]["failed"],
        json!(["changed_files_within [\"docs/**\"]: changed outside [\"docs/**\"]: status.txt"]),
        "{child}"
    );
    assert_clean_baseline(&r.kept);
}

#[test]
fn session_core_w18s_workspace_checks_after_a_restart_outside_git_still_fail_closed() {
    let r = across_restart(
        r#"contract={'checks': [{'changed_files_within': ['status.txt']}]}"#,
        "fixed",
        "never",
    );
    let child = &r.result;
    assert_eq!(child["status"], "failed", "{child}");
    assert_eq!(
        child["check"]["contract"]["failed"],
        json!([
            "changed_files_within [\"status.txt\"]: no working directory was snapshotted when the child started, so its changes are unknown"
        ]),
        "{child}"
    );
    // No baseline is kept outside a Git work tree.
    assert_eq!(r.kept.len(), 1);
    assert_eq!(r.kept[0].get("baseline"), None, "{:?}", r.kept);
}

/// A persistent child (`keep=True`) with workspace checks spawned by one worker and first collected by the next
/// (wave 41, crate::kept_contracts): the next worker has no snapshot of the spawn, so its check compares with the
/// Git baseline the session kept with the contract (`ultron.kept-contracts`).
#[test]
fn session_core_w18s_a_kept_childs_workspace_checks_after_a_restart_use_the_kept_git_baseline() {
    let home = Home::new(None);
    std::fs::write(home.cwd.join("README.md"), "repo\n").unwrap();
    std::fs::write(home.cwd.join(".gitignore"), format!("{HANDLE}\n{RESULT}\n.loki/\n")).unwrap();
    git(&home, &["init", "-q"]);
    git(&home, &["add", "-A"]);
    git(&home, &["commit", "-q", "-m", "init"]);
    let spawn = format!(
        "h = await rlm.spawn('KEPT: fix status', name='k', keep=True, contract={{'checks': [{{'changed_files_within': ['status.txt']}}]}})\nopen({}, 'w').write(h.rlm_child_id)\nprint('spawned')",
        json!(HANDLE)
    );
    let script = move |root_cell: String| -> Script {
        Box::new(move |request, _| {
            let kept = texts(request).first().is_some_and(|t| t.contains("KEPT: fix status"));
            match (kept, answering(request)) {
                (true, false) => tool("toolu_k", &work_cell("fixed")),
                (true, true) => text("kept child answered"),
                (false, false) => tool("toolu_root", &root_cell),
                (false, true) => text("root done"),
            }
        })
    };
    let worker = home.open(script(spawn), &GIT_VARS, None);
    let ended = prompt(&worker, "ROOT1: keep a child");
    assert_eq!(ended["error"], Value::Null, "{ended}");
    // The kept child's cell ended (its verdict is recorded) before the worker hands the session over.
    let end = Instant::now() + HANG;
    while !worker
        .requests()
        .iter()
        .any(|r| answering(r) && texts(r).first().is_some_and(|t| t.contains("KEPT: fix status")))
    {
        assert!(Instant::now() < end, "the kept child never ran its cell");
        std::thread::sleep(Duration::from_millis(10));
    }
    worker.hand_over();
    let collect = format!(
        "import json\nhid = open({}).read()\nr = (await rlm.collect([hid]))[0]['result']\nopen({}, 'w').write(json.dumps(r))\nprint('collected')",
        json!(HANDLE),
        json!(RESULT)
    );
    let next = home.open(script(collect), &GIT_VARS, None);
    let ended = prompt(&next, "ROOT2: collect the kept child");
    assert_eq!(ended["error"], Value::Null, "{ended}");
    let written = std::fs::read_to_string(home.cwd.join(RESULT)).unwrap_or_else(|_| {
        let seen: Vec<Vec<Value>> = next.requests().iter().map(tool_results).collect();
        panic!("the root cell wrote no result: {seen:#?}")
    });
    let result: Value = serde_json::from_str(&written).unwrap();
    assert_eq!(result["check"]["contract"]["verdict"], "verified", "{result}");
    assert_eq!(
        result["check"]["contract"]["checks"][0]["evidence"], "1 changed file(s), all within [\"status.txt\"]",
        "{result}"
    );
    drop(next);
    let kept: Vec<Value> = home.values("ultron.kept-contracts");
    let baseline = &kept[0]["baseline"];
    assert_eq!(baseline["dirty"], json!({}), "{kept:?}");
    assert_eq!(baseline["commit"].as_str().map(str::len), Some(40), "{kept:?}");
}
