//! A38 (partial): nested context excludes unauthorized and contaminating history.
//! Ported from packages/coding-agent/test/ultron-a38-reviewer-context.test.ts, end to end through the `ultron`
//! binary in RPC mode against a scripted provider: the root conversation holds a secret and a speculation, the root
//! spawns an RLM child (`rlm.spawn`); the child's requests carry only its own prompt, and the child's kernel is its own
//! Python process without the root's variables. The root keeps its history on its next turn.
//!
//! Not ported: the security-reviewer half of the first test (`agents.invoke` of `security-reviewer@1` needs the
//! definition registry), and ultron-a38-query-scope.test.ts (`background.start`, `agents.spawn`, `agents.status`,
//! `agents.inspect/result/cancel` and `background.*` have no Rust host).

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
fn acceptance_a38_child_lanes_receive_only_their_own_prompt_and_kernel_ported_part() {
    let provider = Provider::start(|request| {
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
                "secret = {SECRET:?}\nimport os\nprint('ROOT_PID', os.readlink('/proc/self/ns/pid') + ':' + str(os.getpid()))\nh = await rlm.spawn('Child task CHILD_INPUT_58', name='helper')"
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
    // The child's end continues the root's run (child_done) before the run settles.
    let tools = rpc_tool_results(&client.get_messages().unwrap());
    let root_pid: String = tools
        .iter()
        .find_map(|t| t.lines().find_map(|l| l.strip_prefix("ROOT_PID ")).map(str::to_string))
        .expect("root pid");

    client.prompt_and_wait("Second root turn", timeout).unwrap();
    let roots: Vec<Request> = provider.requests().into_iter().filter(|r| !is_child(r)).collect();
    let second = roots.last().unwrap();
    assert!(second.raw.contains(SECRET));
    assert!(second.raw.contains("root acknowledged"));

    let children: Vec<Request> = provider.requests().into_iter().filter(is_child).collect();
    assert_eq!(children.len(), 2);
    // Exactly one message on the child's first request: the task prompt built from its own explicit input.
    assert_eq!(conversation(&children[0]).len(), 1);
    for request in &children {
        assert!(request.raw.contains("CHILD_INPUT_58"));
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
