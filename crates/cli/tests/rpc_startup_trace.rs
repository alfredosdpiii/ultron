//! `ULTRON_STARTUP_TRACE` in RPC mode (experimental/startup-trace.ts, rpc-native.ts:56-94, native-command.ts): the binary appends its
//! milestones to the named file as `{at, origin, pid, label}` JSON lines; unset, nothing is written. The trace file
//! lives in the test's private directory.

mod common;
mod tui_harness;

use serde_json::Value;
use std::io::Write;
use std::process::Stdio;

fn run_rpc(sandbox: &common::Sandbox, trace: Option<&std::path::Path>) -> u32 {
    let mut command = sandbox.ultron();
    command
        .args(["--mode", "rpc", "--model", "anthropic/claude-sonnet-4-5"])
        .env("PI_OFFLINE", "1")
        .env("ULTRON_HINDSIGHT_URL", "off")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(trace) = trace {
        command.env("ULTRON_STARTUP_TRACE", trace);
    }
    let mut child = command.spawn().unwrap();
    let pid = child.id();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"{\"type\":\"get_state\",\"id\":\"s\"}\n").unwrap();
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("\"command\":\"get_state\""), "{stdout}");
    pid
}

#[test]
fn cli_rpc_startup_trace_appends_milestones_as_json_lines() {
    let (port, _) = common::mock_server(Vec::new(), common::Naming::Fail);
    let sandbox = common::Sandbox::new(port);
    let trace = sandbox.root.join("trace").join("startup.jsonl");
    std::fs::create_dir_all(trace.parent().unwrap()).unwrap();
    // A line already there (another process of the start) is kept: the file is appended to.
    std::fs::write(&trace, "{\"label\":\"cli.native\"}\n").unwrap();
    let pid = run_rpc(&sandbox, Some(&trace));
    let lines: Vec<Value> = std::fs::read_to_string(&trace)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines[0]["label"], "cli.native");
    let mine: Vec<&str> = lines
        .iter()
        .filter(|l| l["pid"] == pid)
        .map(|l| l["label"].as_str().unwrap())
        .collect();
    // The CLI process's milestones in TS order (main.ts, native-command.ts, rpc-native.ts:56-94, server.ts,
    // session-worker-manager.ts): its foreground server starts first, the client runtime opens after it, services are
    // ready before the session is selected, and attaching the selected session spawns its worker.
    assert_eq!(
        mine,
        [
            "cli.native",
            "cli.list-sessions",
            "server.coordinator",
            "server.coordinator-ready",
            "server.connected",
            "cli.server-started",
            "rpc.open",
            "rpc.services-ready",
            "rpc.session-selected",
            "server.spawn-worker",
            "rpc.attached"
        ]
    );
    let ours: Vec<&Value> = lines
        .iter()
        .filter(|l| l["pid"] == pid && l["label"].as_str().unwrap().starts_with("rpc."))
        .collect();
    for line in &ours {
        let keys: Vec<&str> = line.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, ["at", "origin", "pid", "label"]);
        let (at, origin) = (line["at"].as_f64().unwrap(), line["origin"].as_f64().unwrap());
        assert!(origin > 1.6e12 && origin <= at, "{line}");
    }
    for pair in ours.windows(2) {
        assert!(pair[0]["at"].as_f64() <= pair[1]["at"].as_f64(), "{pair:?}");
        assert_eq!(pair[0]["origin"], pair[1]["origin"], "one time origin per process");
    }

    // Unset: no trace is written anywhere in the sandbox.
    let before = std::fs::read_to_string(&trace).unwrap();
    run_rpc(&sandbox, None);
    assert_eq!(std::fs::read_to_string(&trace).unwrap(), before);
}

#[test]
fn cli_rpc_startup_trace_line_matches_json_stringify() {
    assert_eq!(
        client::startup_trace::trace_line(1_729_000_000_123.456, 1_729_000_000_000.5, 7, "rpc.open"),
        "{\"at\":1729000000123.456,\"origin\":1729000000000.5,\"pid\":7,\"label\":\"rpc.open\"}\n"
    );
}

/// The `rpc.*` milestones are RPC mode's only (rpc-native.ts): the TUI opens its session through the same link and
/// traces none of them.
#[test]
fn cli_rpc_startup_trace_milestones_are_not_traced_by_the_tui() {
    let (port, _) = common::mock_server(Vec::new(), common::Naming::Fail);
    let dir = tempfile::tempdir().unwrap();
    let trace = dir.path().join("startup.jsonl");
    let trace_text = trace.display().to_string();
    let tui = tui_harness::Tui::start(
        port,
        100,
        30,
        &["--model", "anthropic/claude-sonnet-4-5"],
        &[("ULTRON_STARTUP_TRACE", &trace_text), ("ULTRON_HINDSIGHT_URL", "off")],
    );
    tui.wait("ultron v");
    // The session's worker is ready only after the link opened and attached its session.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while !std::fs::read_to_string(&trace)
        .unwrap_or_default()
        .contains("\"worker.ready\"")
    {
        assert!(std::time::Instant::now() < deadline, "no worker.ready in the trace");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    tui.send_text("/quit");
    tui.send_keys(&["Enter"]);
    tui.wait("<exited>");
    let labels: Vec<String> = std::fs::read_to_string(&trace)
        .unwrap()
        .lines()
        .map(|l| {
            serde_json::from_str::<Value>(l).unwrap()["label"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert!(labels.iter().any(|l| l == "cli.server-started"), "{labels:?}");
    assert!(!labels.iter().any(|l| l.starts_with("rpc.")), "{labels:?}");
}
