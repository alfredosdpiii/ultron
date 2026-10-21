//! `ultron durable` with a client's own session options (user requirement; literal expectations): `--model`,
//! `--thinking`, `--system-prompt` and `--no-session` reach the worker the client's session starts, as they would
//! with a foreground server; the stored lane keeps the model for a later client without flags; a client whose
//! options differ from those of a session's running worker is refused with what to do. The durable server, its
//! workers and the clients run under a private HOME against a scripted Anthropic server on loopback.

#[path = "daemon_common/mod.rs"]
mod common;

use cli::rpc::client::{RpcClient, RpcClientOptions};
use common::{HANG_GUARD, Home, assert_none_mentioning, provider, text_reply};
use serde_json::Value;
use std::path::PathBuf;
use std::time::Duration;

fn run(home: &Home, args: &[&str]) -> (i32, String, String) {
    let output = home.instance_command(&home.cwd(), args).output().unwrap();
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn system_text(request: &Value) -> String {
    match &request["system"] {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn durable_sessions(home: &Home) -> usize {
    daemon::catalog::list(&home.sessions()).len()
}

/// Session workers still running under the test's root.
fn workers(home: &Home) -> usize {
    common::processes_mentioning(&home.root)
        .into_iter()
        .filter(|pid| {
            std::fs::read(format!("/proc/{pid}/cmdline"))
                .is_ok_and(|c| String::from_utf8_lossy(&c).contains("session-worker"))
        })
        .count()
}

/// Wait (hang guard only) until the last client's session worker retired: the next one starts on its own options.
fn workers_retired(home: &Home) {
    let deadline = std::time::Instant::now() + HANG_GUARD;
    while workers(home) > 0 {
        assert!(std::time::Instant::now() < deadline, "the session worker never retired");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn needles(home: &Home) -> Vec<PathBuf> {
    vec![
        home.root.clone(),
        daemon::instance::socket_dir(&home.root.join(".ultron/server")),
    ]
}

#[test]
fn cli_durable_clients_run_their_sessions_on_their_own_options() {
    let replies = (0..5).map(|i| text_reply(&format!("answer {i}"))).collect();
    let provider = provider(replies, Duration::ZERO);
    let home = Home::new(provider.port);
    // A new session on the client's model, thinking level and system prompt.
    let (code, out, err) = run(
        &home,
        &[
            "durable",
            "--mode",
            "json",
            "--model",
            "anthropic/claude-haiku-4-5",
            "--thinking",
            "off",
            "--system-prompt",
            "Custom durable system",
            "hello",
        ],
    );
    assert_eq!(code, 0, "{err}");
    let result: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(result["text"], "answer 0");
    let id = result["sessionId"].as_str().unwrap().to_string();
    let first = provider.requests.lock().unwrap()[0].clone();
    assert_eq!(first["model"], "claude-haiku-4-5");
    assert!(
        system_text(&first).starts_with("Custom durable system"),
        "{}",
        system_text(&first)
    );
    assert_eq!(first["thinking"]["type"], "disabled", "--thinking off: {first}");
    let sessions = durable_sessions(&home);
    assert_eq!(sessions, 1);
    workers_retired(&home);
    // A later client without flags: the session's stored model, the default prompt (flags belong to a run).
    let (code, out, err) = run(&home, &["durable", "-p", "--session-id", &id, "again"]);
    assert_eq!((code, out.as_str()), (0, "answer 1\n"), "{err}");
    let second = provider.requests.lock().unwrap()[1].clone();
    assert_eq!(second["model"], "claude-haiku-4-5");
    assert!(!system_text(&second).contains("Custom durable system"));
    workers_retired(&home);
    // `--no-session`: the session lasts for the run only.
    let (code, out, err) = run(
        &home,
        &[
            "durable",
            "-p",
            "--no-session",
            "--model",
            "anthropic/claude-sonnet-4-5",
            "once",
        ],
    );
    assert_eq!((code, out.as_str()), (0, "answer 2\n"), "{err}");
    assert_eq!(provider.requests.lock().unwrap()[2]["model"], "claude-sonnet-4-5");
    assert_eq!(durable_sessions(&home), sessions);
    workers_retired(&home);
    // An RPC client keeps the session's worker running on its options.
    let command = home.instance_command(&home.cwd(), &[]);
    let env = command
        .get_envs()
        .filter_map(|(k, v)| Some((k.to_str()?.to_string(), v?.to_str()?.to_string())))
        .collect();
    let options = RpcClientOptions {
        program: env!("CARGO_BIN_EXE_ultron").into(),
        cwd: Some(home.cwd()),
        env_clear: true,
        env,
        program_args: vec!["durable".to_string()],
        args: ["--session-id", &id, "--thinking", "low"].map(String::from).to_vec(),
        ..Default::default()
    };
    let mut client = RpcClient::start(&options).unwrap();
    let state = client.get_state().unwrap();
    assert_eq!(
        (&state["thinkingLevel"], &state["sessionId"]),
        (&serde_json::json!("low"), &serde_json::json!(id))
    );
    // Other options for the running worker: refused, saying why and what to do.
    let (code, _, err) = run(
        &home,
        &["durable", "-p", "--session-id", &id, "--thinking", "high", "x"],
    );
    assert_eq!(code, 1, "{err}");
    assert!(
        err.contains("is already open on this server with other session options")
            && err.contains("leave the flags out"),
        "{err}"
    );
    // The same options, or none: the client attaches to the running worker.
    let (code, out, err) = run(
        &home,
        &["durable", "-p", "--session-id", &id, "--thinking", "low", "same"],
    );
    assert_eq!((code, out.as_str()), (0, "answer 3\n"), "{err}");
    let (code, out, err) = run(&home, &["durable", "-p", "--session-id", &id, "plain"]);
    assert_eq!((code, out.as_str()), (0, "answer 4\n"), "{err}");
    assert_eq!(provider.requests.lock().unwrap().len(), 5);
    let (_, code) = client.finish(HANG_GUARD);
    assert_eq!(code, Some(0));
    let (code, _, err) = run(&home, &["durable", "stop"]);
    assert_eq!(code, 0, "{err}");
    assert_none_mentioning(&needles(&home), HANG_GUARD);
}
