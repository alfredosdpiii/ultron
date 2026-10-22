//! `ultron durable` session selectors (Rust-only fix, literal expectations): `--session <id>` and `-c` select among
//! the durable instance's own sessions, from whatever directory the client runs, the same way in print, JSON and
//! RPC mode. The durable server's sessions record its directory (the home directory), never the client's: before,
//! the client kept only the sessions of its own directory, so `--session <id>` (how herdr resumes a durable pane)
//! failed with "No native session found matching".

#[path = "daemon_common/mod.rs"]
mod common;

use common::{HANG_GUARD, Home, assert_none_mentioning, provider, text_reply};
use serde_json::Value;
use std::io::Write;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

fn needles(home: &Home) -> Vec<PathBuf> {
    vec![
        home.root.clone(),
        daemon::instance::socket_dir(&home.root.join(".ultron/server")),
    ]
}

fn run(home: &Home, args: &[&str]) -> (i32, String, String) {
    let output = home.instance_command(&home.cwd(), args).output().unwrap();
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// `ultron durable --mode rpc <args>`: the session id `get_state` reports (stdin closed after the one command).
fn rpc_session(home: &Home, args: &[&str]) -> String {
    let mut argv = vec!["durable", "--mode", "rpc"];
    argv.extend_from_slice(args);
    let mut child = home
        .instance_command(&home.cwd(), &argv)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, r#"{{"id": "s", "type": "get_state"}}"#).unwrap();
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let state = stdout
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["id"] == "s")
        .unwrap_or_else(|| {
            panic!(
                "no get_state answer: {stdout}\n{}",
                String::from_utf8_lossy(&output.stderr)
            )
        });
    assert_eq!(state["success"], true, "{state}");
    state["data"]["sessionId"].as_str().unwrap().to_string()
}

fn json_session(home: &Home, args: &[&str]) -> (String, String) {
    let mut argv = vec!["durable", "--mode", "json"];
    argv.extend_from_slice(args);
    let (code, out, err) = run(home, &argv);
    assert_eq!(code, 0, "{err}");
    let result: Value = serde_json::from_str(out.trim()).unwrap_or_else(|_| panic!("{out}"));
    (
        result["sessionId"].as_str().unwrap().to_string(),
        result["text"].as_str().unwrap().to_string(),
    )
}

#[test]
fn cli_durable_session_and_continue_select_the_instances_sessions_in_every_mode() {
    let replies = (0..8).map(|i| text_reply(&format!("answer {i}"))).collect();
    let provider = provider(replies, Duration::ZERO);
    let home = Home::new(provider.port);
    // An older session started from the caller's directory.
    let (local, _) = json_session(&home, &["zero"]);
    // A newer one started from another directory.
    let elsewhere = home.root.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let output = home
        .instance_command(&elsewhere, &["durable", "--mode", "json", "first"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let first: Value = serde_json::from_str(String::from_utf8_lossy(&output.stdout).trim()).unwrap();
    let id = first["sessionId"].as_str().unwrap().to_string();
    assert_ne!(id, local);
    // `-c`: the instance's most recently active session, the same in every mode.
    assert_eq!(rpc_session(&home, &["-c"]), id);
    let (code, out, err) = run(&home, &["durable", "-p", "-c", "go on"]);
    assert_eq!((code, out.as_str()), (0, "answer 2\n"), "{err}");
    assert_eq!(json_session(&home, &["-c", "and on"]), (id.clone(), "answer 3".into()));
    // `--session <id>` (as herdr resumes a durable pane) from the caller's directory: that session, by id or prefix.
    let (code, out, err) = run(&home, &["durable", "-p", "--session", &id, "again"]);
    assert_eq!((code, out.as_str()), (0, "answer 4\n"), "{err}");
    assert_eq!(
        json_session(&home, &["--session", &id[..id.len() - 4], "more"]),
        (id.clone(), "answer 5".into())
    );
    assert_eq!(rpc_session(&home, &["--session", &id]), id);
    // The local one is still selected by id.
    assert_eq!(rpc_session(&home, &["--session", &local]), local);
    // An unknown one is still refused.
    let (code, _, err) = run(&home, &["durable", "-p", "--session", "ffffffff", "x"]);
    assert_eq!(code, 1);
    assert!(err.contains("No native session found matching 'ffffffff'"), "{err}");
    let output = home
        .instance_command(&home.cwd(), &["durable", "stop"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_none_mentioning(&needles(&home), HANG_GUARD);
}
