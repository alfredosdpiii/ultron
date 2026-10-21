//! A resumed session whose stored model no longer resolves opens (the TypeScript lane opens on any identity), its
//! prompt fails with `model_unavailable` before any request (generation.ts `prepareGeneration`), and selecting a
//! model fixes it: the next prompt runs on the selected one. Print mode and RPC, against a scripted Anthropic server
//! on loopback; the session is a legacy (v3) file whose model is no longer in the catalog.

mod common;

use cli::rpc::client::{RpcClient, RpcClientOptions};
use common::{Naming, Sandbox, anthropic_text, mock_server, normalize_journal, session_files};
use serde_json::{Value, json};

/// A hang guard only: every wait ends on an event.
const HANG_GUARD: std::time::Duration = std::time::Duration::from_secs(120);

/// A legacy (v3) session in `sandbox` whose model is `anthropic/claude-retired-1` (not in the catalog).
fn retired_session(sandbox: &Sandbox) -> std::path::PathBuf {
    let cwd = sandbox.cwd.display().to_string();
    let lines = [
        json!({"type": "session", "version": 3, "id": "0193a000-0000-7000-8000-00000000aaaa", "timestamp": "2026-01-01T00:00:00.000Z", "cwd": cwd}),
        json!({"type": "model_change", "id": "a1", "parentId": null, "timestamp": "2026-01-01T00:00:01.000Z", "provider": "anthropic", "modelId": "claude-retired-1"}),
        json!({"type": "thinking_level_change", "id": "a2", "parentId": "a1", "timestamp": "2026-01-01T00:00:01.000Z", "thinkingLevel": "low"}),
        json!({"type": "message", "id": "a3", "parentId": "a2", "timestamp": "2026-01-01T00:00:02.000Z", "message": {"role": "user", "content": [{"type": "text", "text": "old question"}], "timestamp": 1767225602000_i64}}),
        json!({"type": "message", "id": "a4", "parentId": "a3", "timestamp": "2026-01-01T00:00:03.000Z", "message": {"role": "assistant", "content": [{"type": "text", "text": "old answer"}], "api": "anthropic-messages", "provider": "anthropic", "model": "claude-sonnet-4-5", "usage": {"input": 1, "output": 2, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 3, "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}}, "stopReason": "stop", "timestamp": 1767225603000_i64}}),
    ];
    let path = sandbox
        .sessions_dir()
        .join("--v3--/2026-01-01T00-00-00-000Z_0193a000-0000-7000-8000-00000000aaaa.jsonl");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(&path, text).unwrap();
    path
}

fn quiet_sandbox(port: u16) -> Sandbox {
    let sandbox = Sandbox::new(port);
    std::fs::write(
        sandbox.agent_dir.join("settings.json"),
        r#"{"sessionNaming":{"enabled":false}}"#,
    )
    .unwrap();
    sandbox
}

/// `ultron -p -c --model ...` on it: exit 1 with the configuration error, no request, the prompt kept, and the run's
/// journal as the harness writes a configuration failure: `assistant.ready` naming the stored model, then the
/// failed result with the error code, message and the model's identity.
#[test]
fn cli_print_continue_with_a_retired_model_fails_with_model_unavailable() {
    let (port, seen) = mock_server(vec![anthropic_text("unused")], Naming::Fail);
    let sandbox = quiet_sandbox(port);
    retired_session(&sandbox);
    let output = sandbox
        .ultron()
        .stdin(std::process::Stdio::null())
        .args(["-p", "--continue", "--model", "anthropic/claude-sonnet-4-5", "again"])
        .output()
        .unwrap();
    assert_eq!(
        (
            output.status.code(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned()
        ),
        (
            Some(1),
            String::new(),
            "Error: The configured model is unavailable in this process\n".to_string()
        )
    );
    assert!(seen.lock().unwrap().requests.is_empty());
    let files = session_files(&sandbox.sessions_dir());
    assert_eq!(files.len(), 1);
    let journal = normalize_journal(&std::fs::read_to_string(&files[0]).unwrap());
    let transactions = journal["transactions"].as_array().unwrap();
    let records = |i: usize| transactions[i].as_array().unwrap().clone();
    let last = records(transactions.len() - 1);
    let result = last.iter().find(|r| r["namespace"] == "pi.result").unwrap();
    let mut result_value = result["value"].clone();
    let operation = result_value["operationId"].clone();
    result_value["operationId"] = Value::Null;
    result_value["fromTipId"] = Value::Null;
    result_value["tipId"] = Value::Null;
    assert_eq!(
        result_value,
        json!({"operationId": null, "kind": "run", "status": "failed",
            "error": {"code": "model_unavailable", "message": "The configured model is unavailable in this process",
                "details": {"provider": "anthropic", "modelId": "claude-retired-1"}},
            "fromTipId": null, "tipId": null, "startedAt": 0, "endedAt": 0})
    );
    assert_eq!(
        last.iter()
            .map(|r| format!("{} {}", r["op"].as_str().unwrap(), r["namespace"].as_str().unwrap()))
            .collect::<Vec<_>>(),
        [
            "delete pi.op.meta",
            "delete pi.op.state",
            "set pi.result",
            "set pi.lane.state"
        ]
    );
    // The transaction before: the generation's `assistant.ready`, its context on the stored model.
    let ready = records(transactions.len() - 2);
    let state = &ready.iter().find(|r| r["namespace"] == "pi.op.state").unwrap()["value"];
    assert_eq!(state["at"], "assistant.ready");
    assert_eq!(state["nextAttempt"], 1);
    assert_eq!(
        state["generationContext"]["configuration"],
        json!({"model": {"provider": "anthropic", "modelId": "claude-retired-1"}, "thinkingLevel": "low", "activeToolNames": ["rlm"]})
    );
    assert_eq!(
        ready.iter().find(|r| r["namespace"] == "pi.op.state").unwrap()["key"],
        operation
    );
    // The prompt is the branch's last message; no assistant answer follows it.
    let text = std::fs::read_to_string(&files[0]).unwrap();
    let (_, messages) = common::transcript(&text);
    let last_message = messages.last().unwrap();
    assert_eq!(
        (&last_message["role"], &last_message["content"][0]["text"]),
        (&json!("user"), &json!("again"))
    );
}

#[test]
fn cli_resume_with_an_unavailable_model_opens_and_select_model_fixes_it() {
    let (port, seen) = mock_server(vec![anthropic_text("fixed")], Naming::Fail);
    let sandbox = quiet_sandbox(port);
    retired_session(&sandbox);
    let command = sandbox.ultron();
    let env = command
        .get_envs()
        .filter_map(|(k, v)| Some((k.to_str()?.to_string(), v?.to_str()?.to_string())))
        .collect();
    let options = RpcClientOptions {
        program: env!("CARGO_BIN_EXE_ultron").into(),
        cwd: Some(sandbox.cwd.clone()),
        env_clear: true,
        env,
        args: ["--mode", "rpc", "-c", "--model", "anthropic/claude-sonnet-4-5"]
            .map(String::from)
            .to_vec(),
        ..Default::default()
    };
    let mut client = RpcClient::start(&options).unwrap();
    let state = client.get_state().unwrap();
    assert_eq!(
        (&state["model"]["provider"], &state["model"]["id"]),
        (&json!("anthropic"), &json!("claude-retired-1"))
    );
    client.mark();
    let response = client.send(json!({"type": "prompt", "message": "again"})).unwrap();
    let error = match RpcClient::data(&response) {
        Err(error) => error,
        // An accepted prompt reports the failure as the run ends.
        Ok(_) => {
            let ended = client
                .wait_for(
                    &|line| line["type"] == "agent_settled" || line["type"] == "error",
                    HANG_GUARD,
                )
                .unwrap();
            ended.to_string()
        }
    };
    assert!(
        error.contains("The configured model is unavailable in this process"),
        "{error}"
    );
    assert!(
        seen.lock().unwrap().requests.is_empty(),
        "no request on the unavailable model"
    );
    let selected = client
        .send(json!({"type": "set_model", "provider": "anthropic", "modelId": "claude-sonnet-4-5"}))
        .unwrap();
    assert_eq!(RpcClient::data(&selected).unwrap()["id"], "claude-sonnet-4-5");
    client.prompt_and_wait("again", HANG_GUARD).unwrap();
    assert_eq!(client.get_last_assistant_text().unwrap().as_deref(), Some("fixed"));
    let requests = seen.lock().unwrap().requests.clone();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["body"]["model"], "claude-sonnet-4-5");
    let (_, code) = client.finish(HANG_GUARD);
    assert_eq!(code, Some(0));
}
