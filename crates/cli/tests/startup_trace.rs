//! ULTRON_STARTUP_TRACE milestones (experimental/startup-trace.ts `traceStartup`): a `-p` run appends one JSON line
//! `{at, origin, pid, label}` per milestone to the named file, from every process of the start: the CLI's
//! `cli.native`, `cli.list-sessions`, `cli.server-started` (main.ts, native-command.ts) and the session worker's
//! `worker.harness`, `worker.resources-loaded`, `worker.harness-create`, `worker.host`, `worker.ready`
//! (session-worker.ts), each in that order. Without the variable nothing is written.

mod common;

use common::{Naming, Sandbox, anthropic_text, mock_server};
use serde_json::Value;

fn run(trace: Option<&std::path::Path>) -> (Sandbox, String) {
    let (port, _) = mock_server(vec![anthropic_text("done")], Naming::Fail);
    let sandbox = Sandbox::new(port);
    let mut command = sandbox.ultron();
    command.args(["--model", "anthropic/claude-sonnet-4-5", "-p", "hi"]);
    command
        .env("ULTRON_HINDSIGHT_URL", "off")
        .env("PI_OFFLINE", "1")
        .env("ULTRON_LOKI", "off");
    if let Some(trace) = trace {
        command.env("ULTRON_STARTUP_TRACE", trace);
    }
    let output = command.stdin(std::process::Stdio::null()).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    (sandbox, String::from_utf8(output.stdout).unwrap())
}

#[test]
fn cli_startup_trace_records_the_cli_and_worker_milestones_like_typescript() {
    let dir = tempfile::tempdir().unwrap();
    let trace = dir.path().join("trace.jsonl");
    let (_sandbox, stdout) = run(Some(&trace));
    assert_eq!(stdout.trim(), "done");
    let lines: Vec<Value> = std::fs::read_to_string(&trace)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    for line in &lines {
        let keys: Vec<&str> = line.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, ["at", "origin", "pid", "label"], "{line}");
        assert!(
            line["at"].as_f64().unwrap() >= line["origin"].as_f64().unwrap(),
            "{line}"
        );
    }
    let labels = |prefix: &str| -> Vec<String> {
        lines
            .iter()
            .filter_map(|l| l["label"].as_str())
            .filter(|l| l.starts_with(prefix))
            .map(str::to_string)
            .collect()
    };
    assert_eq!(
        labels("cli."),
        ["cli.native", "cli.list-sessions", "cli.server-started"]
    );
    assert_eq!(
        labels("worker."),
        [
            "worker.entry",
            "worker.harness",
            "worker.resources-loaded",
            "worker.harness-create",
            "worker.host",
            "worker.ready"
        ]
    );
    assert_eq!(
        labels("server."),
        [
            "server.coordinator",
            "server.coordinator-ready",
            "server.connected",
            "server.spawn-worker"
        ]
    );
    assert_eq!(labels("coordinator."), ["coordinator.entry"]);
    // The CLI's lines are its own process's.
    let cli_pids: Vec<&Value> = lines
        .iter()
        .filter(|l| l["label"].as_str().unwrap().starts_with("cli."))
        .map(|l| &l["pid"])
        .collect();
    assert!(cli_pids.windows(2).all(|w| w[0] == w[1]));
}

#[test]
fn cli_startup_trace_is_off_without_the_variable() {
    let dir = tempfile::tempdir().unwrap();
    let (_sandbox, stdout) = run(None);
    assert_eq!(stdout.trim(), "done");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}
