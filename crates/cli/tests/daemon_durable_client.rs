//! `ultron durable` clients through the product (user requirement; literal expectations): `ultron durable -p` and
//! `--mode json` attach to the durable instance's detached server (activating it once), leave it running when they
//! exit, and its scheduled prompts keep firing; its sessions have the Ultron persona on by default; an idle durable
//! server starts no run of its own; a project client keeps its own server as before.

#[path = "daemon_common/mod.rs"]
mod common;

use common::{HANG_GUARD, Home, assert_none_mentioning, provider, sse, text_reply};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const CLOCK_ENV: &str = "__ULTRON_SCHEDULE_CLOCK_FILE";
const T0: i64 = 1_800_000_000_000;
const HOUR: i64 = 3_600_000;
const PERSONALITY: &str = "# Ultron\n\nYou were built to finish things.";

fn tool_reply(code: &str) -> String {
    sse(&[
        json!({"type": "message_start", "message": {"id": "msg_2", "type": "message", "role": "assistant", "model": "claude-sonnet-4-5", "content": [], "stop_reason": null, "usage": {"input_tokens": 10, "output_tokens": 1}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "toolu_1", "name": "rlm", "input": {}}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": json!({"code": code}).to_string()}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 7}}),
        json!({"type": "message_stop"}),
    ])
}

fn wait_until(what: &str, root: &Path, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + HANG_GUARD;
    while !done() {
        assert!(
            Instant::now() < deadline,
            "timed out: {what}\n{}",
            common::hang_report(root, &[])
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn durable_dir(home: &Home) -> PathBuf {
    home.root.join(".ultron/server")
}

/// Generation records of `dir` whose process is alive.
fn generations(dir: &Path) -> Vec<Value> {
    daemon::instance::generations(dir)
        .into_iter()
        .map(|g| json!({"pid": g.pid, "serverId": g.server_id}))
        .collect()
}

fn project_generations(home: &Home) -> usize {
    std::fs::read_dir(home.root.join(".ultron/projects"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| generations(&e.path()).len())
        .sum()
}

fn needles(home: &Home) -> Vec<PathBuf> {
    let mut needles = vec![home.root.clone(), daemon::instance::socket_dir(&durable_dir(home))];
    for entry in std::fs::read_dir(home.root.join(".ultron/projects"))
        .into_iter()
        .flatten()
        .flatten()
    {
        needles.push(daemon::instance::socket_dir(&entry.path()));
    }
    needles
}

fn run(home: &Home, clock: &Path, args: &[&str]) -> (i32, String, String) {
    let output = home
        .instance_command(&home.cwd(), args)
        .env(CLOCK_ENV, clock)
        .output()
        .unwrap();
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn workers(root: &Path) -> usize {
    common::processes_mentioning(root)
        .into_iter()
        .filter(|pid| {
            std::fs::read(format!("/proc/{pid}/cmdline"))
                .is_ok_and(|c| String::from_utf8_lossy(&c).contains("session-worker"))
        })
        .count()
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

fn last_user_text(request: &Value) -> String {
    request["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["role"] == "user")
        .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
        .filter_map(|c| c["text"].as_str().map(str::to_string))
        .rfind(|t| !t.starts_with("[clock]"))
        .unwrap_or_default()
}

#[test]
fn cli_durable_clients_attach_exit_and_leave_the_server_and_its_schedules_running() {
    let provider = provider(
        vec![
            tool_reply("await schedule.every('1h', 'check the build', name='build')"),
            text_reply("Scheduled."),
            text_reply("second answer"),
            text_reply("Build is green."),
            text_reply("project answer"),
        ],
        Duration::ZERO,
    );
    let home = Home::new(provider.port);
    std::fs::write(home.agent().join("personality.md"), PERSONALITY).unwrap();
    let clock = home.root.join("clock");
    std::fs::write(&clock, T0.to_string()).unwrap();

    // The first client activates the durable server, runs its turn there and exits; the server stays.
    let (code, out, err) = run(&home, &clock, &["durable", "-p", "watch the build"]);
    assert_eq!((code, out.as_str()), (0, "Scheduled.\n"), "{err}");
    let first = generations(&durable_dir(&home));
    assert_eq!(first.len(), 1, "{first:?}");
    // A second client (json) attaches to the same generation: no second server.
    let (code, out, err) = run(&home, &clock, &["durable", "--mode", "json", "again"]);
    assert_eq!(code, 0, "{err}");
    let result: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(result["text"], "second answer");
    assert_eq!(result["serverId"], first[0]["serverId"]);
    assert_eq!(generations(&durable_dir(&home)), first);
    // The durable instance's sessions have the persona on by default (no setting says so).
    let requests = provider.requests.lock().unwrap().clone();
    assert!(system_text(&requests[0]).contains(&format!("<persona>\n{PERSONALITY}\n</persona>")));
    assert!(
        !std::fs::read_to_string(home.agent().join("settings.json"))
            .unwrap()
            .contains("persona")
    );

    // Every client is gone; the slot passes and the durable server fires the schedule.
    wait_until("the workers retire", &home.root, || workers(&home.root) == 0);
    std::fs::write(&clock, (T0 + HOUR + 5).to_string()).unwrap();
    wait_until("the fired run", &home.root, || {
        provider.requests.lock().unwrap().len() >= 4
    });
    assert_eq!(last_user_text(&provider.requests.lock().unwrap()[3]), "check the build");
    assert_eq!(generations(&durable_dir(&home)), first);

    // A project client keeps its own server (gone when it exits), without the persona.
    let (code, out, err) = run(&home, &clock, &["--mode", "json", "project"]);
    assert_eq!(code, 0, "{err}");
    let result: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(result["text"], "project answer");
    assert_ne!(result["serverId"], first[0]["serverId"]);
    assert_eq!(project_generations(&home), 0);
    let requests = provider.requests.lock().unwrap().clone();
    assert!(!system_text(&requests[4]).contains("<persona>"));
    assert_eq!(generations(&durable_dir(&home)), first);

    let (code, _, err) = run(&home, &clock, &["durable", "stop"]);
    assert_eq!(code, 0, "{err}");
    assert_none_mentioning(&needles(&home), HANG_GUARD);
}

#[test]
fn cli_durable_server_idle_without_schedules_starts_no_run_of_its_own() {
    let provider = provider(vec![text_reply("only answer")], Duration::ZERO);
    let home = Home::new(provider.port);
    let clock = home.root.join("clock");
    std::fs::write(&clock, T0.to_string()).unwrap();
    // A finished session in the durable store, then a fresh durable server over it.
    let (code, out, err) = run(&home, &clock, &["durable", "-p", "hello"]);
    assert_eq!((code, out.as_str()), (0, "only answer\n"), "{err}");
    let (code, _, err) = run(&home, &clock, &["durable", "stop"]);
    assert_eq!(code, 0, "{err}");
    assert_none_mentioning(&needles(&home), HANG_GUARD);
    let (code, out, err) = run(&home, &clock, &["durable", "start"]);
    assert_eq!(code, 0, "{err}");
    let (id, socket) = out.trim().split_once('\t').unwrap();
    // Started, reachable and answering (its journals read): no worker and no provider request of its own.
    let connection = client::runtime::Connection::connect(Path::new(socket), id).unwrap();
    let listed = connection
        .call(
            connection.server_target(),
            "pi.session-management",
            "describe",
            json!([{}]),
        )
        .unwrap()
        .unwrap();
    assert_eq!(listed.as_array().unwrap().len(), 1);
    connection.disconnect();
    std::fs::write(&clock, (T0 + 24 * HOUR).to_string()).unwrap();
    let (code, out, _) = run(&home, &clock, &["durable", "status"]);
    assert_eq!(code, 0);
    assert!(out.starts_with("durable\t"), "{out}");
    assert_eq!(workers(&home.root), 0);
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    let (code, _, _) = run(&home, &clock, &["durable", "stop"]);
    assert_eq!(code, 0);
    assert_none_mentioning(&needles(&home), HANG_GUARD);
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
}

#[test]
fn cli_durable_refuses_client_worker_options() {
    let home = Home::new(1);
    let clock = home.root.join("clock");
    std::fs::write(&clock, T0.to_string()).unwrap();
    let (code, _, err) = run(
        &home,
        &clock,
        &["durable", "-p", "--model", "anthropic/claude-sonnet-4-5", "x"],
    );
    assert_eq!(code, 1);
    assert!(
        err.contains("`ultron durable` runs on the durable server's worker options"),
        "{err}"
    );
    assert!(generations(&durable_dir(&home)).is_empty());
    assert_none_mentioning(&needles(&home), HANG_GUARD);
}
