//! The session's persona toggle and keep-going mode from RPC (resources::keep_going): the `persona` and
//! `keep_going` commands, and a `prompt` whose whole message is `/persona ...` or `/keepgoing ...`, answer the
//! session's status without a turn; any other prompt is a turn (Rust-only, literal expectations).

mod common;

use cli::rpc::client::RpcClient;
use common::{HANG, Install, Provider, Reply};
use serde_json::json;

#[test]
fn acceptance_w22_persona_and_keep_going_over_rpc_answer_the_session_status_without_a_turn() {
    let provider = Provider::start(|_| Reply::Text("hello".into()));
    let install = Install::new(&provider);
    let mut client = install.rpc(&["--no-session"], &[]);
    let data = |response: serde_json::Value| RpcClient::data(&response).unwrap().clone();
    let on = data(client.send(json!({"type": "persona", "args": "on"})).unwrap());
    assert_eq!(
        on,
        json!({"text": "Persona is on for this session; keep-going off", "persona": "on", "effective": true, "keepGoing": null})
    );
    let going = data(
        client
            .send(json!({"type": "prompt", "message": "/keepgoing on"}))
            .unwrap(),
    );
    assert_eq!(going["keepGoing"]["reason"], "/keepgoing on");
    assert_eq!(going["keepGoing"]["continues"], 0);
    let off = data(client.send(json!({"type": "keep_going", "args": "off"})).unwrap());
    assert_eq!(off["text"], "Persona is on for this session; keep-going off");
    let auto = data(
        client
            .send(json!({"type": "prompt", "message": " /persona auto "}))
            .unwrap(),
    );
    assert_eq!(auto["persona"], "auto");
    let usage = client.send(json!({"type": "persona", "args": "jarvis"})).unwrap();
    assert_eq!(usage["success"], false, "{usage}");
    assert_eq!(usage["error"], "Usage: /persona on|off|auto (this session)");
    assert!(provider.requests().is_empty(), "no toggle started a turn");
    // `/personality` is not `/persona`: a prompt like any other.
    client.prompt_and_wait("/personality", HANG).unwrap();
    assert_eq!(provider.requests().len(), 1);
    let (_, code) = client.finish(HANG);
    assert_eq!(code, Some(0));
}
