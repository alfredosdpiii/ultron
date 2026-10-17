//! `ULTRON_STARTUP_TRACE` in RPC mode (experimental/startup-trace.ts, rpc-native.ts:56-94): the binary appends its
//! milestones to the named file as `{at, origin, pid, label}` JSON lines; unset, nothing is written. The trace file
//! lives in the test's private directory.

mod common;

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
    // The CLI process also traces its own start (cli.native first, its foreground server) before the RPC lines.
    assert_eq!(mine.first(), Some(&"cli.native"), "{mine:?}");
    assert!(mine.contains(&"cli.server-started"), "{mine:?}");
    let ours: Vec<&Value> = lines
        .iter()
        .filter(|l| l["pid"] == pid && l["label"].as_str().unwrap().starts_with("rpc."))
        .collect();
    let labels: Vec<&str> = ours.iter().map(|l| l["label"].as_str().unwrap()).collect();
    assert_eq!(labels, ["rpc.open", "rpc.attached"]);
    for line in &ours {
        let keys: Vec<&str> = line.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, ["at", "origin", "pid", "label"]);
        let (at, origin) = (line["at"].as_f64().unwrap(), line["origin"].as_f64().unwrap());
        assert!(origin > 1.6e12 && origin <= at, "{line}");
    }
    assert!(ours[0]["at"].as_f64() <= ours[1]["at"].as_f64());
    assert_eq!(ours[0]["origin"], ours[1]["origin"], "one time origin per process");

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
