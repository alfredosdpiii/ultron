//! `/goal` from RPC (Ultron Durable P4 wiring): a `prompt` whose message is `/goal ...`, and the `goal` command, go to
//! the session goal's command (`GoalDriver::command`, the TUI's `/goal`) and answer its text; neither starts a turn.
//! The reply texts are the literals of packages/coding-agent/src/ultron/goal.ts (`goalStatusText`, `/goal clear`,
//! `/goal check`), which kernel-apis ports.

mod common;

use cli::rpc::client::RpcClient;
use common::{HANG, Install, Provider, Reply};
use serde_json::json;

#[test]
fn acceptance_a23_goal_command_over_rpc_answers_the_goals_text_without_a_turn() {
    let provider = Provider::start(|_| Reply::Text("unexpected".into()));
    let install = Install::new(&provider);
    let mut client = install.rpc(&["--no-session"], &[]);
    let text = |response: serde_json::Value| RpcClient::data(&response).unwrap()["text"].clone();
    let status = client.send(json!({"type": "prompt", "message": "/goal"})).unwrap();
    assert_eq!(text(status), json!("No goal. Set one with /goal <objective>."));
    let clear = client
        .send(json!({"type": "prompt", "message": "  /goal clear  "}))
        .unwrap();
    assert_eq!(text(clear), json!("No goal to clear."));
    let check = client.send(json!({"type": "goal", "args": "check npm test"})).unwrap();
    assert_eq!(text(check), json!("Set a goal first: /goal <objective>."));
    // `/goalkeeper` is not `/goal`: it is a prompt like any other.
    assert!(provider.requests().is_empty());
    client.prompt_and_wait("/goalkeeper", HANG).unwrap();
    assert_eq!(provider.requests().len(), 1);
    let (_, code) = client.finish(HANG);
    assert_eq!(code, Some(0));
}
