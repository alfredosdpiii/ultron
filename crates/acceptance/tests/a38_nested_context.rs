//! A38: nested context excludes unauthorized and contaminating history.
//! Ported from packages/coding-agent/test/ultron-a38-reviewer-context.test.ts, end to end through the `ultron`
//! binary in RPC mode against a scripted provider: the root conversation holds a secret and a speculation, the root
//! runs a security reviewer (`agents.invoke("security-reviewer@1", ...)`, a typed agent on its own lane) and spawns
//! an RLM child (`rlm.spawn`); the reviewer's and the child's requests carry only their own prompt, and the child's
//! kernel is its own Python process without the root's variables. The root keeps its history on its next turn.
//! The TS test counts the lane's messages without clock lines (its harness installs no clock context); the Rust
//! counterpart drops `[clock]` lines the same way. In the TS product a subagent lane (`ultron.rlm-child.*`) gets one
//! derived clock line and a typed agent none; Rust child conversations get none: neither is history.
//! ultron-a38-query-scope.test.ts runs over the Rust typed-agent host (`agents_support`) as the TS case runs over
//! `NativeRlmHost`, with lanes that work until cancelled.

mod agents_support;
mod common;

use common::{Install, Provider, Reply, Request, rpc_tool_results};
use serde_json::Value;

const SECRET: &str = "ROOT_ONLY_SECRET_9c41";
const SPECULATION: &str = "IMPLEMENTER_SPECULATION_b77e";

/// User messages without the `[clock]` context line the host adds before each prompt.
fn prompts(request: &Request) -> Vec<String> {
    request
        .users()
        .into_iter()
        .filter(|u| !u.starts_with("[clock] "))
        .collect()
}

fn is_child(request: &Request) -> bool {
    prompts(request).first().is_some_and(|u| u.starts_with("Child task"))
}

fn is_reviewer(request: &Request) -> bool {
    prompts(request)
        .first()
        .is_some_and(|u| u.contains("Review the supplied change for security issues"))
}

/// Every message of a request except the system prompt, serialized.
fn conversation(request: &Request) -> Vec<String> {
    request.body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] != "system" && m["role"] != "developer")
        .filter(|m| !m.to_string().contains("[clock] "))
        .map(Value::to_string)
        .collect()
}

#[test]
fn acceptance_a38_reviewer_and_rlm_child_lanes_receive_only_their_own_prompt_and_their_own_kernel() {
    let provider = Provider::start(|request| {
        if is_reviewer(request) {
            return Reply::Text(r#"{"outcome":"no_findings","findings":[]}"#.into());
        }
        if is_child(request) {
            return if request.tool_results().is_empty() {
                Reply::Rlm("import os\nprint('secret' in globals(), os.readlink('/proc/self/ns/pid') + ':' + str(os.getpid()))".into())
            } else {
                Reply::Text("child finished".into())
            };
        }
        let last = prompts(request).last().cloned().unwrap_or_default();
        if last == "Spawn the helper" {
            let last_role = request.body["messages"].as_array().unwrap().last().unwrap()["role"].clone();
            if last_role == "tool" {
                return Reply::Text("DELEGATED".into());
            }
            return Reply::Rlm(format!(
                "secret = {SECRET:?}\nimport os, json\nprint('ROOT_PID', os.readlink('/proc/self/ns/pid') + ':' + str(os.getpid()))\nreview = await agents.invoke('security-reviewer@1', {{'request': 'Review diff REVIEW_INPUT_31'}})\nprint('REVIEW', json.dumps(review, sort_keys=True))\nh = await rlm.spawn('Child task CHILD_INPUT_58', name='helper')"
            ));
        }
        Reply::Text("root acknowledged".into())
    });
    let install = Install::new(&provider);
    let mut client = install.rpc(&["--no-session"], &[]);
    let timeout = common::HANG;
    client
        .prompt_and_wait(&format!("Remember {SECRET}. My guess: {SPECULATION}."), timeout)
        .unwrap();
    let first = provider.roots();
    assert_eq!(conversation(&first[0]).len(), 1);
    assert!(first[0].raw.contains(SECRET));

    client.prompt_and_wait("Spawn the helper", timeout).unwrap();
    // The child's end re-invokes the root as a run of its own (child_done); wait for it to be answered.
    common::wait_until("the child_done run", || {
        client.get_messages().is_ok_and(|m| {
            m.iter().any(|m| m["customType"] == "ultron-runtime-event")
                && m.last().is_some_and(|m| m["role"] == "assistant")
        }) && client.get_state().is_ok_and(|s| s["isStreaming"] == false)
    });
    let tools = rpc_tool_results(&client.get_messages().unwrap());
    let root_pid: String = tools
        .iter()
        .find_map(|t| t.lines().find_map(|l| l.strip_prefix("ROOT_PID ")).map(str::to_string))
        .expect("root pid");

    let review: Value = tools
        .iter()
        .find_map(|t| t.lines().find_map(|l| l.strip_prefix("REVIEW ")).map(str::to_string))
        .map(|json| serde_json::from_str(&json).unwrap())
        .expect("review result");
    assert_eq!(review["status"], "succeeded");
    assert_eq!(
        review["value"],
        serde_json::json!({"outcome": "no_findings", "findings": []})
    );

    client.prompt_and_wait("Second root turn", timeout).unwrap();
    let roots: Vec<Request> = provider
        .requests()
        .into_iter()
        .filter(|r| !is_child(r) && !is_reviewer(r))
        .collect();
    let second = roots.last().unwrap();
    assert!(second.raw.contains(SECRET));
    assert!(second.raw.contains("root acknowledged"));
    // The root's next turn carries neither the reviewer's nor the child's own input.
    assert!(
        !second
            .users()
            .iter()
            .any(|u| u.contains("REVIEW_INPUT_31") && u.contains("Review the supplied"))
    );
    assert!(!second.users().iter().any(|u| u.contains("CHILD_INPUT_58")));

    let reviewers: Vec<Request> = provider.requests().into_iter().filter(is_reviewer).collect();
    assert_eq!(reviewers.len(), 1);
    // Exactly one message: the reviewer's prompt built from its own explicit input.
    assert_eq!(conversation(&reviewers[0]).len(), 1);
    let reviewer = &reviewers[0].raw;
    assert!(reviewer.contains("REVIEW_INPUT_31"));
    for leaked in [SECRET, SPECULATION, "root acknowledged", "CHILD_INPUT_58", "ROOT_PID"] {
        assert!(!reviewer.contains(leaked), "{leaked} reached the reviewer");
    }

    let children: Vec<Request> = provider.requests().into_iter().filter(is_child).collect();
    assert_eq!(children.len(), 2);
    // Exactly one message on the child's first request: the task prompt built from its own explicit input.
    assert_eq!(conversation(&children[0]).len(), 1);
    for request in &children {
        assert!(request.raw.contains("CHILD_INPUT_58"));
        assert!(!request.raw.contains("REVIEW_INPUT_31"));
        assert!(!request.raw.contains(SECRET));
        assert!(!request.raw.contains(SPECULATION));
        assert!(!request.raw.contains("root acknowledged"));
        assert!(!request.raw.contains("ROOT_PID"));
    }
    // Each lane has its own Python kernel: the root's variable is absent and the process differs.
    let child_cell = children[1].tool_results()[0].clone();
    let (seen, child_pid) = child_cell.trim().split_once(' ').expect(&child_cell);
    assert_eq!(seen, "False");
    assert_ne!(child_pid, root_pid);
    assert!(child_pid.starts_with("pid:["), "{child_pid}");
    let (_, code) = client.finish(common::HANG);
    assert_eq!(code, Some(0));
}

#[test]
fn acceptance_a38_a_child_lane_sees_its_own_subtree_only_siblings_are_indistinguishable_from_missing() {
    use serde_json::json;
    use std::sync::Arc;
    let fixture = agents_support::fixture(agents_support::Options {
        script: Some(Arc::new(|_, _, signal, _| agents_support::aborted(signal))),
        ..Default::default()
    });
    let call = |kind: &str, payload: Value, lane: &str| fixture.agents.handle(kind, payload, lane, None);
    let a = call("background.start", json!({"prompt": "implementer"}), "main").unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let b = call("background.start", json!({"prompt": "reviewer"}), "main").unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let as_a = format!("ultron.background-job.{a}");
    agents_support::wait_for("A's lane", std::time::Duration::from_secs(120), || {
        fixture.agents.caller_task(&as_a).is_some()
    });
    let grandchild = call(
        "agents.spawn",
        json!({"definition": "identity@1", "input": {"from": "a"}}),
        &as_a,
    )
    .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    let seen_by_a = call("agents.status", json!({}), &as_a).unwrap();
    let mut ids: Vec<String> = seen_by_a["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_str().unwrap().to_string())
        .collect();
    ids.sort();
    let mut expected = vec![a.clone(), grandchild.clone()];
    expected.sort();
    assert_eq!(ids, expected);
    for request in ["agents.inspect", "agents.result", "agents.cancel"] {
        let error = call(request, json!({"id": b}), &as_a).unwrap_err();
        assert!(error.contains("Unknown Ultron task"), "{request}: {error}");
    }
    for request in ["background.inspect", "background.result", "background.stop"] {
        let error = call(request, json!({"id": b}), &as_a).unwrap_err();
        assert!(error.contains("Unknown background job"), "{request}: {error}");
    }
    let listed = call("background.list", json!({}), &as_a).unwrap();
    let listed = listed.as_array().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["id"], a.as_str());
    // The sibling keeps running: A could not cancel it.
    assert_eq!(
        call("agents.inspect", json!({"id": b}), "main").unwrap()["state"],
        "running"
    );
    // Its own descendant stays fully usable.
    assert_eq!(
        call("agents.result", json!({"id": grandchild}), &as_a).unwrap()["status"],
        "succeeded"
    );
    // The root sees everything.
    let seen_by_root = call("agents.status", json!({}), "main").unwrap();
    assert_eq!(seen_by_root["tasks"].as_array().unwrap().len(), 3);
    fixture.agents.close();
}
