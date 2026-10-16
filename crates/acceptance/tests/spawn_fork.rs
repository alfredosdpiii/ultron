//! `rlm.spawn(brief, name=..., fork=True)`, ported from packages/coding-agent/test/ultron-spawn-fork.test.ts, end to
//! end through the `ultron` binary in RPC mode against a scripted provider: the forked child's conversation starts as
//! its parent's finished conversation (without the cell still running, which spawned it) and its brief says its
//! kernel is new; without fork the child sees only its brief; fork is refused when it must be a boolean.

mod common;

use common::{Install, Provider, Reply, Request, rpc_tool_results};

const PARENT_MARKER: &str = "PARENT_CONTEXT_7d21";
const CHILD_INPUT: &str = "Child brief CHILD_INPUT_12";

fn is_child(request: &Request) -> bool {
    request.users().iter().any(|u| u.contains(CHILD_INPUT))
}

/// Runs a root that remembers the marker, then spawns the child with `kwargs` (Python keyword text); returns the
/// child's first request and the spawn cell's tool output.
fn spawn(kwargs: &str) -> (Request, String) {
    let cell = format!(
        "h = await rlm.spawn({CHILD_INPUT:?}, name='helper'{kwargs})\nprint('ANCHOR', repr(h.parent_branch_anchor))"
    );
    let provider = Provider::start(move |request| {
        if is_child(request) {
            return Reply::Text("child finished".into());
        }
        let last = request.users().last().cloned().unwrap_or_default();
        if last.ends_with("Spawn the helper") {
            let last_role = request.body["messages"].as_array().unwrap().last().unwrap()["role"].clone();
            if last_role == "tool" {
                return Reply::Text("DELEGATED".into());
            }
            return Reply::Rlm(cell.clone());
        }
        Reply::Text("root acknowledged".into())
    });
    let install = Install::new(&provider);
    let mut client = install.rpc(&["--no-session"], &[]);
    client
        .prompt_and_wait(&format!("Remember {PARENT_MARKER}."), common::HANG)
        .unwrap();
    client.prompt_and_wait("Spawn the helper", common::HANG).unwrap();
    common::wait_until("the child's request", || provider.requests().iter().any(is_child));
    let child = provider.requests().into_iter().find(is_child).unwrap();
    let tools = rpc_tool_results(&client.get_messages().unwrap());
    let anchor = tools
        .iter()
        .find_map(|t| t.lines().find_map(|l| l.strip_prefix("ANCHOR ")).map(str::to_string))
        .unwrap_or_else(|| panic!("no anchor in {tools:?}"));
    (child, anchor)
}

#[test]
fn acceptance_spawn_fork_child_starts_on_the_parents_finished_conversation() {
    let (child, anchor) = spawn(", fork=True");
    assert_ne!(anchor, "''", "a forked child names where it starts");
    let raw = &child.raw;
    assert!(raw.contains(PARENT_MARKER), "{raw}");
    assert!(raw.contains("root acknowledged"), "{raw}");
    assert!(raw.contains("CHILD_INPUT_12"));
    assert!(raw.contains("Your REPL kernel is new"));
    // The cell that spawned it is still running: its call is not in the child's conversation.
    assert!(!raw.contains("fork=True"), "{raw}");
    let messages = child.body["messages"].as_array().unwrap();
    assert!(
        !messages.iter().any(|m| m["role"] == "tool"),
        "no tool result without its call: {messages:?}"
    );
}

#[test]
fn acceptance_spawn_without_fork_the_child_sees_only_its_brief() {
    let (child, anchor) = spawn("");
    assert_eq!(anchor, "''");
    assert!(!child.raw.contains(PARENT_MARKER));
    assert!(!child.raw.contains("Your REPL kernel is new"));
    assert!(!child.raw.contains("root acknowledged"));
}

#[test]
fn acceptance_spawn_fork_must_be_a_boolean() {
    let provider = Provider::start(|request| {
        let last_role = request.body["messages"].as_array().unwrap().last().unwrap()["role"].clone();
        if last_role == "tool" {
            return Reply::Text("done".into());
        }
        Reply::Rlm(
            "try:\n    await rlm.spawn('x', name='c', fork='yes')\nexcept Exception as e:\n    print('ERR', e)".into(),
        )
    });
    let install = Install::new(&provider);
    let mut client = install.rpc(&["--no-session"], &[]);
    client.prompt_and_wait("Try a fork", common::HANG).unwrap();
    let text = rpc_tool_results(&client.get_messages().unwrap()).join("\n");
    assert!(text.contains("ERR rlm.spawn fork must be True or False"), "{text}");
}
