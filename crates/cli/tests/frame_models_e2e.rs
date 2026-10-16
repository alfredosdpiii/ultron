//! Through the shipped `ultron -p` (the session's composed host stack): a frame naming another model of the catalog
//! (`rlm.infer(model=...)`) calls that model, with its provider's key, through the same provider dispatch as the
//! session (TS: the frame lane's `setModel`); and `rlm.spawn(model=...)` naming a model the session does not know is
//! refused instead of running the child on the root's model (A04). Private HOME and agent/server dirs, a scripted
//! Anthropic server on 127.0.0.1.

mod common;

use common::{Naming, Sandbox, anthropic_text, anthropic_tool, assert_private_home, mock_server, tool_result_text};

const MODEL: &str = "anthropic/claude-sonnet-4-5";

#[test]
fn cli_frame_naming_another_catalog_model_calls_it_and_spawn_refuses_an_unknown_model() {
    let other = ai::models::builtin_models("anthropic")
        .find(|m| m.id.contains("haiku"))
        .expect("a built-in anthropic haiku model")
        .id
        .clone();
    let code = format!(
        "try:\n    await rlm.spawn('x', name='c', model='nope/not-a-model')\nexcept Exception as e:\n    print('refused:', e)\nr = await rlm.infer('Say hi', model='anthropic/{other}')\nprint('frame:', r)"
    );
    let (port, seen) = mock_server(
        vec![
            anthropic_tool("toolu_f", &code),
            anthropic_text("hello from the frame model"),
            anthropic_text("done"),
        ],
        Naming::Fail,
    );
    let sandbox = Sandbox::new(port);
    assert_private_home(&sandbox.root);
    std::fs::write(
        sandbox.agent_dir.join("settings.json"),
        r#"{"sessionNaming":{"enabled":false}}"#,
    )
    .unwrap();
    let output = sandbox.ultron().args(["-p", "--model", MODEL, "go"]).output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "done\n", "{stderr}");
    let requests = seen.lock().unwrap().requests.clone();
    assert_eq!(requests.len(), 3, "{requests:#?}");
    // The frame's request went to the named model, with the frame's own system prompt and the session's key.
    assert_eq!(requests[1]["body"]["model"], other.as_str());
    assert!(
        requests[1]["body"]["system"]
            .to_string()
            .contains("You are an inference frame inside Ultron"),
        "{}",
        requests[1]["body"]
    );
    assert_eq!(requests[0]["body"]["model"], "claude-sonnet-4-5");
    let result = tool_result_text(&requests[2]["body"], "toolu_f").unwrap();
    assert_eq!(
        result,
        "refused: model must be provider/model naming a known model, not nope/not-a-model\nframe: hello from the frame model"
    );
}
