//! Scheduled prompts through the product (a Rust-only feature; literal expectations): the real server and session
//! worker processes against a scripted provider on loopback. A cell creates `schedule.every("1h", ...)`; once the
//! worker has retired, the test clock (`__ULTRON_SCHEDULE_CLOCK_FILE`, read by the server's timer and the worker) moves
//! past the slot and the server starts the session's worker and runs the prompt as an `ultron.schedule` message. A
//! restarted server reads the schedule back from the journal and fires the slots it missed once. `ultron schedule
//! list` reads the journals, `ultron schedule cancel` goes through the running server, and without a server it
//! writes the journal itself.

#[path = "daemon_common/mod.rs"]
mod common;

use client::runtime::Connection;
use common::{HANG_GUARD, Home, SERVER_ID, ServerProcess, assert_no_processes, provider, sse, text_reply};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

const CLOCK_ENV: &str = "__ULTRON_SCHEDULE_CLOCK_FILE";
const T0: i64 = 1_800_000_000_000;
const HOUR: i64 = 3_600_000;

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

fn start(home: &Home, clock: &Path) -> ServerProcess {
    let child = home
        .command(&[
            "__internal",
            "server",
            home.server_dir().to_str().unwrap(),
            SERVER_ID,
            home.sessions().to_str().unwrap(),
            r#"{"model":"anthropic/claude-sonnet-4-5"}"#,
        ])
        .env("__ULTRON_SERVER_KEEP_ALIVE", "1")
        .env(CLOCK_ENV, clock)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let socket = home.server_dir().join(format!("{SERVER_ID}.sock"));
    wait_until("server start", &home.root, || {
        Connection::connect(&socket, SERVER_ID).is_ok()
    });
    ServerProcess {
        child,
        server_id: SERVER_ID.into(),
        socket,
    }
}

/// Session workers of this test (their options name the temp root).
fn workers(root: &Path) -> usize {
    common::processes_mentioning(root)
        .into_iter()
        .filter(|pid| {
            std::fs::read(format!("/proc/{pid}/cmdline"))
                .is_ok_and(|c| String::from_utf8_lossy(&c).contains("session-worker"))
        })
        .count()
}

fn session_file(sessions: &Path) -> PathBuf {
    std::fs::read_dir(sessions)
        .unwrap()
        .flatten()
        .flat_map(|d| std::fs::read_dir(d.path()).unwrap().flatten())
        .map(|f| f.path())
        .find(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .expect("the session file exists")
}

/// The `ultron.schedule` custom messages the journal holds: `(name, run)`.
fn fired(sessions: &Path) -> Vec<(String, u64)> {
    let text = std::fs::read_to_string(session_file(sessions)).unwrap();
    text.lines()
        .skip(1)
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .flat_map(|t| t.as_array().cloned().unwrap_or_else(|| vec![t]))
        .filter(|w| w["kind"] == "entry" && w["type"] == "message" && w["message"]["customType"] == "ultron.schedule")
        .map(|e| {
            let details = &e["message"]["details"];
            (
                details["name"].as_str().unwrap_or_default().to_string(),
                details["run"].as_u64().unwrap_or(0),
            )
        })
        .collect()
}

fn user_texts(body: &Value) -> Vec<String> {
    body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["role"] == "user")
        .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
        .filter_map(|c| c["text"].as_str().map(str::to_string))
        .filter(|t| !t.starts_with("[clock]"))
        .collect()
}

fn cli(home: &Home, clock: &Path, args: &[&str]) -> (i32, String, String) {
    let output = home.command(args).env(CLOCK_ENV, clock).output().unwrap();
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn cli_daemon_schedules_fire_restart_collapse_and_cancel_through_the_product() {
    let provider = provider(
        vec![
            tool_reply("sid = await schedule.every('1h', 'check the build', name='build')\nprint(sid)"),
            text_reply("Scheduled."),
            text_reply("Build is green."),
            text_reply("Missed checks done."),
        ],
        Duration::ZERO,
    );
    let home = Home::new(provider.port);
    let clock = home.root.join("clock");
    std::fs::write(&clock, T0.to_string()).unwrap();
    let mut server = start(&home, &clock);
    let connection = Connection::connect(&server.socket, SERVER_ID).unwrap();
    let created = connection
        .call(
            connection.server_target(),
            "pi.session-management",
            "create",
            json!([{}]),
        )
        .unwrap()
        .unwrap();
    let id = created["sessionId"].as_str().unwrap().to_string();
    connection
        .call(
            connection.server_target(),
            "pi.session-management",
            "attach",
            json!([id]),
        )
        .unwrap();
    connection
        .wait_attachment(|a| a.is_some_and(|a| a.session_id == id))
        .unwrap();
    let session = connection.session_target().unwrap();
    let answered = connection
        .call(
            session,
            "pi.agent-controller",
            "prompt",
            json!([{"message": "watch the build", "images": null}]),
        )
        .unwrap()
        .unwrap();
    assert_eq!(answered["error"], Value::Null, "{answered}");
    connection.disconnect();
    // The worker retires without its client; the schedule stays with the session.
    wait_until("the worker retires", &home.root, || workers(&home.root) == 0);
    assert_eq!(provider.requests.lock().unwrap().len(), 2);

    // The slot passes: the server starts the worker and the prompt runs.
    std::fs::write(&clock, (T0 + HOUR + 5_000).to_string()).unwrap();
    wait_until("the fired run", &home.root, || {
        provider.requests.lock().unwrap().len() >= 3
    });
    let third = provider.requests.lock().unwrap()[2].clone();
    assert_eq!(user_texts(&third).last().unwrap(), "check the build");
    wait_until("the fired run's entry", &home.root, || {
        fired(&home.sessions()) == [("build".to_string(), 1)]
    });
    wait_until("the worker retires again", &home.root, || workers(&home.root) == 0);

    // A restarted server finds the schedule in the journal; four missed slots fire once.
    server.stop();
    assert_no_processes(&home.root, HANG_GUARD);
    std::fs::write(&clock, (T0 + 5 * HOUR + 1).to_string()).unwrap();
    let server = start(&home, &clock);
    wait_until("the missed run", &home.root, || {
        provider.requests.lock().unwrap().len() >= 4
    });
    wait_until("the missed run's entry", &home.root, || {
        fired(&home.sessions()).len() == 2
    });
    assert_eq!(
        fired(&home.sessions()),
        [("build".to_string(), 1), ("build".to_string(), 2)]
    );
    let fourth = provider.requests.lock().unwrap()[3].clone();
    assert_eq!(user_texts(&fourth).last().unwrap(), "check the build");

    // `ultron schedule list` and `cancel` (through the running server).
    let (code, out, err) = cli(&home, &clock, &["schedule", "list"]);
    assert_eq!(code, 0, "{err}");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "ID\tNAME\tNEXT\tRUNS\tREPEAT\tSTATE\tSESSION\tCWD\tERROR");
    let row: Vec<&str> = lines[1].split('\t').collect();
    assert_eq!(
        row[1..7],
        [
            "build",
            "2027-01-15T14:00:00.000Z",
            "2",
            "every 1h",
            "active",
            id.as_str()
        ]
    );
    let schedule_id = row[0].to_string();
    let (code, out, err) = cli(&home, &clock, &["schedule", "cancel", "build"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(out.trim(), format!("Cancelled build ({schedule_id}) of session {id}"));
    let (_, out, _) = cli(&home, &clock, &["schedule", "list"]);
    assert!(out.lines().nth(1).unwrap().contains("\tcancelled\t"), "{out}");
    let (code, _, err) = cli(&home, &clock, &["schedule", "cancel", "build"]);
    assert_eq!((code, err.trim()), (1, "Error: No active schedule build"));
    // Usage errors; any other `ultron schedule ...` stays a prompt.
    let (code, _, err) = cli(&home, &clock, &["schedule", "cancel"]);
    assert_eq!(
        (code, err.trim()),
        (2, "Usage: ultron schedule list | wake | cancel <id|name> (see --help)")
    );
    drop(server);
    assert_no_processes(&home.root, HANG_GUARD);
    assert_eq!(provider.requests.lock().unwrap().len(), 4);
}

#[test]
fn cli_daemon_schedules_cancel_without_a_server_writes_the_journal() {
    let provider = provider(
        vec![
            tool_reply("await schedule.at(3600, 'say hi', name='hello')"),
            text_reply("Scheduled."),
        ],
        Duration::ZERO,
    );
    let home = Home::new(provider.port);
    let clock = home.root.join("clock");
    std::fs::write(&clock, T0.to_string()).unwrap();
    let (code, _, err) = cli(&home, &clock, &["-p", "schedule a greeting"]);
    assert_eq!(code, 0, "{err}");
    assert_no_processes(&home.root, HANG_GUARD);
    // (`list` would start the instance's server for the active schedule: cancel first.)
    let (code, out, err) = cli(&home, &clock, &["schedule", "cancel", "hello"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.starts_with("Cancelled hello (sch-"), "{out}");
    let (_, out, _) = cli(&home, &clock, &["schedule", "list"]);
    let row: Vec<String> = out.lines().nth(1).unwrap().split('\t').map(str::to_string).collect();
    assert_eq!(row[1..6], ["hello", "-", "0", "once", "cancelled"]);
    assert_eq!(row.last().unwrap(), "-");
    assert_no_processes(&home.root, HANG_GUARD);
}

/// The instance's server: the generation records under the server dir.
fn generations(home: &Home) -> usize {
    std::fs::read_dir(home.server_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("generation-"))
        .count()
}

/// `{error, at, due}` the server kept for the session's schedule `id`.
fn kept_error(home: &Home, session: &str, id: &str) -> Option<Value> {
    let text = std::fs::read_to_string(home.server_dir().join("schedule-errors.json")).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    Some(value[session][id].clone()).filter(|v| !v.is_null())
}

#[test]
fn cli_daemon_schedules_list_wakes_the_instance_and_a_failed_delivery_stays_due() {
    let provider = provider(
        vec![
            tool_reply("await schedule.at(3600, 'say hi', name='hello')"),
            text_reply("Scheduled."),
        ],
        Duration::ZERO,
    );
    let home = Home::new(provider.port);
    let clock = home.root.join("clock");
    std::fs::write(&clock, T0.to_string()).unwrap();
    let (code, _, err) = cli(&home, &clock, &["-p", "schedule a greeting"]);
    assert_eq!(code, 0, "{err}");
    assert_no_processes(&home.root, HANG_GUARD);
    // The slot passes with no server running. `list` brings the instance up: a server that retires at once when
    // idle (no grace), started without a key, so the fire cannot start a run.
    std::fs::write(&clock, (T0 + HOUR + 5).to_string()).unwrap();
    let no_key = |args: &[&str]| {
        let output = home
            .command(args)
            .env(CLOCK_ENV, &clock)
            .env("__ULTRON_SERVER_IDLE_GRACE_MS", "0")
            .env_remove("ANTHROPIC_API_KEY")
            .output()
            .unwrap();
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    };
    let (code, out, err) = no_key(&["schedule", "list"]);
    assert_eq!(code, 0, "{err}");
    let row: Vec<String> = out.lines().nth(1).unwrap().split('\t').map(str::to_string).collect();
    let (id, session) = (row[0].clone(), row[6].clone());
    assert_eq!(generations(&home), 1, "list started the instance's server");
    wait_until("the failed delivery", &home.root, || {
        kept_error(&home, &session, &id).is_some()
    });
    let kept = kept_error(&home, &session, &id).unwrap();
    assert_eq!(
        (kept["at"].clone(), kept["due"].clone()),
        (json!(T0 + HOUR + 5), json!(T0 + HOUR))
    );
    let error = kept["error"].as_str().unwrap().to_string();
    assert!(error.contains("No API key found for anthropic"), "{error}");
    // Not counted: still due on its slot, the error shown.
    let (_, out, _) = no_key(&["schedule", "list"]);
    let row: Vec<String> = out.lines().nth(1).unwrap().split('\t').map(str::to_string).collect();
    assert_eq!(row[1..6], ["hello", "2027-01-15T09:00:00.000Z", "0", "once", "active"]);
    assert_eq!(row[8], error);
    assert!(fired(&home.sessions()).is_empty());
    // The worker the fire started retires; the server, idle but for the pending schedule, stays and retries.
    wait_until("the worker retires", &home.root, || workers(&home.root) == 0);
    std::fs::write(&clock, (T0 + HOUR + 5 + 60_000).to_string()).unwrap();
    wait_until("the retry", &home.root, || {
        kept_error(&home, &session, &id).is_some_and(|e| e["at"] == json!(T0 + HOUR + 5 + 60_000))
    });
    assert_eq!(generations(&home), 1);
    // Cancelled: nothing is pending and the server retires.
    let (code, out, err) = no_key(&["schedule", "cancel", "hello"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(out.trim(), format!("Cancelled hello ({id}) of session {session}"));
    assert_no_processes(&home.root, HANG_GUARD);
    let (_, out, _) = no_key(&["schedule", "list"]);
    let row: Vec<String> = out.lines().nth(1).unwrap().split('\t').map(str::to_string).collect();
    assert_eq!(
        (row[3].as_str(), row[5].as_str(), row[8].as_str()),
        ("0", "cancelled", "-")
    );
    assert_eq!(provider.requests.lock().unwrap().len(), 2);
    assert_no_processes(&home.root, HANG_GUARD);
}
