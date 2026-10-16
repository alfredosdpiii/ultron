//! Guard statistics in `rlm.pool` (session-worker.ts `poolStats`: `guards: fileHooks.stats()`): TypeScript has one
//! set of file hooks for the whole session, so a worktree subagent's checks count with the root's. In Rust each
//! worktree subagent has a host (and file hooks) of its own; the pool reports the sum, also after the subagent's
//! lane is gone, and a cell's own `rlm.pool` request carries the same guards as the inspected pool.

mod p_session_support;

use p_session_support::{HANG, Setup, actor, text, tool};
use serde_json::{Value, json};
use session_core::core::Op;
use std::sync::{Arc, Mutex};
use std::time::Instant;

const POOL_CELL: &str =
    "p = await rlm.host_request('rlm.pool', {})\nprint([(g['name'], g['checks'], g['blocked']) for g in p['guards']])";

fn first_user_text(body: &Value) -> String {
    let content = &body["messages"][0]["content"];
    match content {
        Value::String(text) => text.clone(),
        _ => content[0]["text"].as_str().unwrap_or_default().to_string(),
    }
}

fn git(cwd: &std::path::Path, home: &std::path::Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn tool_result(body: &Value, id: &str) -> Option<String> {
    body["messages"].as_array()?.iter().find_map(|m| {
        m["content"].as_array()?.iter().find_map(|b| {
            (b["type"] == "tool_result" && b["tool_use_id"] == id).then(|| match &b["content"] {
                Value::String(text) => text.clone(),
                other => other[0]["text"].as_str().unwrap_or_default().to_string(),
            })
        })
    })
}

#[test]
fn session_core_pool_guards_add_worktree_subagents_and_reach_the_cell() {
    let parent = [
        tool("toolu_root_write", "print(await write('root.md', 'root'))"),
        tool(
            "toolu_spawn",
            "h = await rlm.spawn('look around', name='scout', worktree=True)\nprint('spawned')",
        ),
        text("waiting"),
        tool("toolu_pool", POOL_CELL),
        tool(
            "toolu_merge",
            "print([r['status'] for r in (await rlm.merge([h]))['results']])",
        ),
        text("parent done"),
    ];
    let child = [
        tool("toolu_child_write", "print(await write('child.md', 'child'))"),
        text("child done"),
    ];
    let counters = Arc::new(Mutex::new((0usize, 0usize)));
    let script = Box::new(move |body: &Value, _| {
        let mut counters = counters.lock().unwrap();
        if first_user_text(body).contains("look around") {
            counters.1 += 1;
            child.get(counters.1 - 1).cloned().unwrap_or_else(|| text("(no more)"))
        } else {
            counters.0 += 1;
            parent.get(counters.0 - 1).cloned().unwrap_or_else(|| text("(no more)"))
        }
    });
    let a = actor(script, Setup::default());
    git(&a.cwd, &a.root, &["init", "-q"]);
    std::fs::write(a.cwd.join("README.md"), "repo\n").unwrap();
    git(&a.cwd, &a.root, &["add", "README.md"]);
    git(
        &a.cwd,
        &a.root,
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "init",
        ],
    );
    let end = a.prompt("go");
    assert!(end["error"].is_null(), "{end}");
    // The subagent's end re-invokes the root, whose next turn asks for the pool from its cell.
    let deadline = Instant::now() + HANG;
    let asked = loop {
        if let Some(text) = a.requests().iter().find_map(|r| tool_result(r, "toolu_pool")) {
            break text;
        }
        assert!(
            Instant::now() < deadline,
            "the root never asked for the pool: {:?}",
            a.requests().len()
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    // Each write was checked by Loki: the root's in the session's host, the subagent's in its worktree's.
    assert_eq!(asked, "[('Loki', 2, 0)]");
    let inspected = a
        .call(Op::Inspect {
            request: "rlm.pool".into(),
            payload: json!({}),
        })
        .unwrap()
        .unwrap();
    let guards = inspected["guards"].as_array().expect("the inspected pool has guards");
    assert_eq!(guards.len(), 1, "{inspected}");
    assert_eq!(
        (&guards[0]["name"], &guards[0]["checks"], &guards[0]["blocked"]),
        (&json!("Loki"), &json!(2), &json!(0))
    );
    // The merge and the subagent's model are in the session's counters (`onMerge`, `childModel`).
    let deadline = Instant::now() + HANG;
    let merged = loop {
        if let Some(text) = a.requests().iter().find_map(|r| tool_result(r, "toolu_merge")) {
            break text;
        }
        assert!(Instant::now() < deadline, "the root never merged");
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    assert_eq!(merged, "['merged']");
    a.call(Op::Describe).unwrap();
    let stats = a
        .writes()
        .iter()
        .rev()
        .find(|w| w["namespace"] == "ultron.module" && w["key"] == "stats" && w["op"] == "set")
        .map(|w| w["value"].clone())
        .unwrap();
    let values = |key: &str| stats[key].as_object().unwrap().values().cloned().collect::<Vec<_>>();
    assert_eq!(values("merges"), [json!("merged")], "{stats}");
    assert_eq!(values("childModels"), [json!("anthropic/claude-sonnet-4-5")], "{stats}");
    assert_eq!(
        stats["merges"].as_object().unwrap().keys().collect::<Vec<_>>(),
        stats["childModels"].as_object().unwrap().keys().collect::<Vec<_>>()
    );
    assert_eq!(stats["guards"]["Loki"]["checks"], 2, "{stats}");
    assert_eq!(stats["cells"]["subagents"]["count"], 1, "{stats}");
}
