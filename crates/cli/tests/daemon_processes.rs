//! Process tests of the native architecture: the `ultron` client, its server generation, the coordinator and session
//! worker processes, each with private ULTRON_SERVER_DIR / ULTRON_CODING_AGENT_DIR / HOME temp dirs. Every test
//! checks that no process it started is left running.

#[path = "daemon_common/mod.rs"]
mod common;

use client::runtime::{ClientCommand, ClientResult, Connection, run_client};
use common::{Home, assert_no_processes, processes_mentioning, provider, start_server, text_reply};
use protocol::RpcTarget;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

/// The user texts of a provider request body, in order.
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

fn assistant_texts(body: &Value) -> Vec<String> {
    body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["role"] == "assistant")
        .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
        .filter_map(|c| c["text"].as_str().map(str::to_string))
        .collect()
}

fn json_line(output: &std::process::Output) -> Value {
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn cli_daemon_reattach_after_client_exit_resumes_the_session() {
    let provider = provider(
        vec![text_reply("One."), text_reply("Two."), text_reply("Three.")],
        Duration::ZERO,
    );
    let home = Home::new(provider.port);
    let model = "anthropic/claude-sonnet-4-5";
    let first = json_line(&home.run(&["--mode", "json", "--model", model, "first"]));
    let id = first["sessionId"].as_str().unwrap().to_string();
    assert_eq!(first["text"], "One.");
    // The first client exited: a new process resumes the session by ID, then with -c.
    let second = json_line(&home.run(&["--mode", "json", "--model", model, "--session-id", &id, "second"]));
    assert_eq!(
        (second["sessionId"].as_str(), second["text"].as_str()),
        (Some(id.as_str()), Some("Two."))
    );
    let third = json_line(&home.run(&["--mode", "json", "--model", model, "-c", "third"]));
    assert_eq!(
        (third["sessionId"].as_str(), third["text"].as_str()),
        (Some(id.as_str()), Some("Three."))
    );
    let requests = provider.requests.lock().unwrap().clone();
    assert_eq!(user_texts(&requests[1]), ["first", "second"]);
    assert_eq!(assistant_texts(&requests[1]), ["One."]);
    assert_eq!(user_texts(&requests[2]), ["first", "second", "third"]);
    assert_eq!(assistant_texts(&requests[2]), ["One.", "Two."]);
    assert_no_processes(&home.root, Duration::from_secs(120));
}

/// Set only in the copy of this test binary that [`cli_daemon_test_clients_ignore_the_launchers_open_stdin`] runs.
const INNER: &str = "ULTRON_W6D_OPEN_STDIN_INNER";

/// The gate condition behind four tests that hung in about half of the full runs (W6D-01): `cargo test` launched
/// with a stdin that never reaches EOF (a pipe or socket its launcher holds open). This binary runs its own inner
/// test with exactly that stdin; the inner test's print client, made by `Home::command`, must still reach the
/// provider and finish.
#[test]
fn cli_daemon_test_clients_ignore_the_launchers_open_stdin() {
    let mut inner = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "cli_daemon_print_client_spawned_under_an_open_stdin_reaches_the_provider",
            "--test-threads=1",
            "--nocapture",
        ])
        .env(INNER, "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // Held open (never written) until the inner run exits: `wait_with_output` would close it first.
    let held = inner.stdin.take();
    let output = inner.wait_with_output().unwrap();
    drop(held);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("test result: ok. 1 passed"),
        "inner run ({}):\n{stdout}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The inner half of [`cli_daemon_test_clients_ignore_the_launchers_open_stdin`]; passes trivially elsewhere.
#[test]
fn cli_daemon_print_client_spawned_under_an_open_stdin_reaches_the_provider() {
    if std::env::var_os(INNER).is_none() {
        return;
    }
    let own = std::fs::read_link("/proc/self/fd/0").unwrap();
    assert!(own.to_string_lossy().starts_with("pipe:"), "this run's stdin: {own:?}");
    let provider = provider(vec![text_reply("Fine.")], Duration::ZERO);
    let home = Home::new(provider.port);
    let (stderr, stderr_path) = home.stderr_file("client");
    // Spawned like the four tests that hung, without setting stdin: `Home::command` must have set it.
    let mut client = home
        .command(&["--mode", "json", "--model", "anthropic/claude-sonnet-4-5", "hello"])
        .stdout(std::process::Stdio::piped())
        .stderr(stderr)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + common::HANG_GUARD;
    while provider.requests.lock().unwrap().is_empty() {
        if let Some(status) = client.try_wait().unwrap() {
            common::exited_early("its first request", status, &stderr_path);
        }
        if Instant::now() >= deadline {
            common::hang("the first request under an open stdin", &home.root, &mut client);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let output = client.wait_with_output().unwrap();
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["text"], "Fine.");
    assert_no_processes(&home.root, common::HANG_GUARD);
}

/// What the gate's hung clients were doing, shown by the hang report: a print client whose stdin is an open pipe
/// reads it to its end before anything else (`readPipedStdin`), so no request leaves while the pipe stays open; once
/// it closes, the piped text leads the prompt.
#[test]
fn cli_daemon_print_client_reads_an_open_stdin_to_its_end_before_its_first_request() {
    let provider = provider(vec![text_reply("Read.")], Duration::ZERO);
    let home = Home::new(provider.port);
    let (stderr, stderr_path) = home.stderr_file("client");
    let mut client = home
        .command(&["--mode", "json", "--model", "anthropic/claude-sonnet-4-5", "hello"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(stderr)
        .spawn()
        .unwrap();
    let pid = client.id();
    let reading = || {
        std::fs::read_dir(format!("/proc/{pid}/task"))
            .into_iter()
            .flatten()
            .flatten()
            .any(|t| std::fs::read_to_string(t.path().join("wchan")).is_ok_and(|w| w.contains("pipe_read")))
    };
    let deadline = Instant::now() + common::HANG_GUARD;
    while !reading() {
        if let Some(status) = client.try_wait().unwrap() {
            common::exited_early("reading its stdin", status, &stderr_path);
        }
        if Instant::now() >= deadline {
            common::hang("the client reading its stdin", &home.root, &mut client);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let report = common::hang_report(&home.root, &[pid]);
    let client_section = report
        .split("pid ")
        .find(|s| s.starts_with(&format!("{pid}:")))
        .unwrap();
    assert!(client_section.contains("fds 0=pipe:"), "{report}");
    assert!(client_section.contains("pipe_read"), "{report}");
    assert!(report.contains("client.stderr ---"), "{report}");
    assert!(provider.requests.lock().unwrap().is_empty());
    let mut stdin = client.stdin.take().unwrap();
    std::io::Write::write_all(&mut stdin, b"piped text\n").unwrap();
    drop(stdin);
    let output = client.wait_with_output().unwrap();
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["text"], "Read.");
    let requests = provider.requests.lock().unwrap().clone();
    assert_eq!(user_texts(&requests[0]), ["piped text\n\nhello"]);
    assert_no_processes(&home.root, common::HANG_GUARD);
}

#[test]
fn cli_daemon_busy_worker_survives_a_killed_client_and_is_adopted() {
    // The first reply takes long enough to kill the client mid-run.
    let provider = provider(
        vec![text_reply("Slow."), text_reply("Next.")],
        Duration::from_millis(1500),
    );
    let home = Home::new(provider.port);
    let model = "anthropic/claude-sonnet-4-5";
    let (stderr, stderr_path) = home.stderr_file("client");
    let mut client = home
        .command(&["--mode", "json", "--model", model, "slow"])
        .stderr(stderr)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(120);
    while provider.requests.lock().unwrap().is_empty() {
        // A client that exited cannot send it: fail now, with its stderr.
        if let Some(status) = client.try_wait().unwrap() {
            common::exited_early("its first request", status, &stderr_path);
        }
        if Instant::now() >= deadline {
            common::hang("the first request never arrived", &home.root, &mut client);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    client.kill().unwrap();
    client.wait().unwrap();
    // The worker keeps running detached (its server is gone) and commits the turn.
    let sessions = home.sessions();
    let file = loop {
        let found = std::fs::read_dir(&sessions)
            .unwrap()
            .flatten()
            .flat_map(|d| std::fs::read_dir(d.path()).unwrap().flatten())
            .map(|f| f.path())
            .find(|p| std::fs::read_to_string(p).is_ok_and(|t| t.contains("Slow.")));
        if let Some(found) = found {
            break found;
        }
        assert!(Instant::now() < deadline, "the detached worker did not commit its turn");
        std::thread::sleep(Duration::from_millis(50));
    };
    let header: Value = serde_json::from_str(std::fs::read_to_string(&file).unwrap().lines().next().unwrap()).unwrap();
    let id = header["id"].as_str().unwrap();
    let next = json_line(&home.run(&["--mode", "json", "--model", model, "--session-id", id, "next"]));
    assert_eq!(next["text"], "Next.");
    let requests = provider.requests.lock().unwrap().clone();
    assert_eq!(user_texts(&requests[1]), ["slow", "next"]);
    assert_eq!(assistant_texts(&requests[1]), ["Slow."]);
    assert_no_processes(&home.root, Duration::from_secs(120));
}

/// The worker process serving `session_id` (its options argument names the session file).
fn worker_pid(home: &Home, session_id: &str) -> Option<u32> {
    processes_mentioning(&home.root).into_iter().find(|pid| {
        std::fs::read(format!("/proc/{pid}/cmdline"))
            .map(|c| {
                let text = String::from_utf8_lossy(&c).into_owned();
                text.contains("session-worker") && text.contains(session_id)
            })
            .unwrap_or(false)
    })
}

fn transcript_roles(connection: &Connection, subscription: &str) -> Vec<String> {
    connection
        .state(subscription, "state")
        .map(|s| {
            s["snapshot"]["transcript"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|e| e["message"]["role"].as_str().unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn cli_daemon_worker_crash_restarts_with_committed_state() {
    let provider = provider(vec![text_reply("Before."), text_reply("After.")], Duration::ZERO);
    let home = Home::new(provider.port);
    let server = start_server(&home, common::SERVER_ID);
    let route = server.route();
    let connection = Connection::connect(&route.path, &route.server_id).unwrap();
    let target = connection.server_target();
    let created = connection
        .call(target.clone(), "pi.session-management", "create", json!([{}]))
        .unwrap()
        .unwrap();
    let id = created["sessionId"].as_str().unwrap().to_string();
    let attach = |connection: &Connection| {
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
        let transcript = connection.subscribe(session.clone(), "pi.transcript").unwrap();
        (session, transcript)
    };
    let (session, transcript) = attach(&connection);
    let prompt = |connection: &Connection, session: &protocol::RpcTarget, text: &str| {
        connection
            .call(
                session.clone(),
                "pi.agent-controller",
                "prompt",
                json!([{"message": text, "images": null}]),
            )
            .unwrap()
            .unwrap()
    };
    assert_eq!(prompt(&connection, &session, "before")["accepted"], true);
    assert_eq!(transcript_roles(&connection, &transcript), ["user", "assistant"]);
    let pid = worker_pid(&home, &id).expect("the worker runs");
    // The worker crashes: the server drops the attachment.
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(pid as i32).unwrap(),
        rustix::process::Signal::KILL,
    )
    .unwrap();
    connection.wait_attachment(|a| a.is_none()).unwrap();
    connection.forget(&transcript);
    // Reattaching starts a new worker that recovers what the session file committed.
    let (session, transcript) = attach(&connection);
    let restarted = worker_pid(&home, &id).expect("a new worker runs");
    assert_ne!(restarted, pid);
    assert_eq!(transcript_roles(&connection, &transcript), ["user", "assistant"]);
    assert_eq!(prompt(&connection, &session, "after")["accepted"], true);
    assert_eq!(
        transcript_roles(&connection, &transcript),
        ["user", "assistant", "user", "assistant"]
    );
    let requests = provider.requests.lock().unwrap().clone();
    assert_eq!(user_texts(&requests[1]), ["before", "after"]);
    assert_eq!(assistant_texts(&requests[1]), ["Before."]);
    connection.disconnect();
    drop(server);
    assert_no_processes(&home.root, Duration::from_secs(120));
}

#[test]
fn cli_daemon_two_clients_share_one_server_and_session() {
    let provider = provider(vec![text_reply("Shared.")], Duration::ZERO);
    let home = Home::new(provider.port);
    let server = start_server(&home, common::SERVER_ID);
    let route = server.route();
    let a = Connection::connect(&route.path, &route.server_id).unwrap();
    let b = Connection::connect(&route.path, &route.server_id).unwrap();
    let directory = b.subscribe(b.server_target(), "pi.session-directory").unwrap();
    let (listed_tx, listed_rx) = std::sync::mpsc::channel();
    b.listen(
        &directory,
        Box::new(move |_, value| {
            let _ = listed_tx.send(value["sessions"][0]["sessionId"].clone());
        }),
    );
    let created = a
        .call(a.server_target(), "pi.session-management", "create", json!([{}]))
        .unwrap()
        .unwrap();
    let id = created["sessionId"].as_str().unwrap().to_string();
    // B's directory replica learns about A's session: its update reaches B's connection on its own, and may arrive
    // after A's create response.
    loop {
        let listed = listed_rx
            .recv_timeout(Duration::from_secs(120))
            .expect("B's directory lists A's session");
        if listed == json!(id) {
            break;
        }
    }
    b.unlisten(&directory);
    assert_eq!(
        b.state(&directory, "state").unwrap()["sessions"][0]["sessionId"],
        json!(id)
    );
    let mut transcripts = Vec::new();
    for connection in [&a, &b] {
        connection
            .call(
                connection.server_target(),
                "pi.session-management",
                "attach",
                json!([id]),
            )
            .unwrap();
        connection
            .wait_attachment(|x| x.is_some_and(|x| x.session_id == id))
            .unwrap();
        transcripts.push(
            connection
                .subscribe(connection.session_target().unwrap(), "pi.transcript")
                .unwrap(),
        );
    }
    assert_ne!(
        a.attachment().unwrap().attachment_id,
        b.attachment().unwrap().attachment_id
    );
    let (seen, events) = std::sync::mpsc::channel();
    b.listen(
        &transcripts[1],
        Box::new(move |_, value| {
            let _ = seen.send(value["event"].clone());
        }),
    );
    let response = a
        .call(
            a.session_target().unwrap(),
            "pi.agent-controller",
            "prompt",
            json!([{"message": "together", "images": null}]),
        )
        .unwrap()
        .unwrap();
    assert_eq!(response["accepted"], true);
    // B saw A's run from start to end, and both replicas hold the same transcript.
    let mut types = Vec::new();
    while let Ok(event) = events.recv_timeout(Duration::from_secs(120)) {
        types.push(event["type"].as_str().unwrap_or_default().to_string());
        if event["type"] == "run_end" {
            break;
        }
    }
    assert_eq!(types.first().map(String::as_str), Some("run_start"));
    assert_eq!(types.last().map(String::as_str), Some("run_end"));
    assert_eq!(
        a.state(&transcripts[0], "state").unwrap()["snapshot"],
        b.state(&transcripts[1], "state").unwrap()["snapshot"]
    );
    assert_eq!(transcript_roles(&b, &transcripts[1]), ["user", "assistant"]);
    // A client on the same server can also take the run's result through runClient.
    let listed = run_client(&ClientCommand::default(), &route).unwrap();
    assert_eq!(listed, ClientResult::List(vec![(route.server_id.clone(), id.clone())]));
    a.disconnect();
    b.disconnect();
    drop(server);
    assert_no_processes(&home.root, Duration::from_secs(120));
}

#[test]
fn cli_daemon_attach_to_unknown_session_and_detached_calls_fail_like_typescript() {
    let provider = provider(vec![], Duration::ZERO);
    let home = Home::new(provider.port);
    let server = start_server(&home, common::SERVER_ID);
    let route = server.route();
    let connection = Connection::connect(&route.path, &route.server_id).unwrap();
    let error = connection
        .call(
            connection.server_target(),
            "pi.session-management",
            "attach",
            json!(["missing"]),
        )
        .unwrap_err();
    assert_eq!(
        error,
        client::runtime::CallError::Server(protocol::ProtocolError::new(
            "session_not_found",
            "Unknown session: missing"
        ))
    );
    let fake = protocol::RpcTarget::Session(protocol::SessionTarget {
        server_id: route.server_id.clone(),
        session_id: "s".into(),
        attachment_id: "a".into(),
    });
    let error = connection.call(fake, "pi.transcript", "x", json!([])).unwrap_err();
    assert_eq!(
        error,
        client::runtime::CallError::Server(protocol::ProtocolError::new(
            "session_not_attached",
            "Session is not attached to this client"
        ))
    );
    let wrong = protocol::RpcTarget::Server {
        server_id: "00000000-0000-4000-8000-0000000000bb".into(),
    };
    let error = connection
        .call(wrong, "pi.session-management", "detach", json!([]))
        .unwrap_err();
    assert_eq!(
        error,
        client::runtime::CallError::Server(protocol::ProtocolError::new(
            "wrong_server",
            "Request was addressed to another server"
        ))
    );
    let error = connection
        .call(connection.server_target(), "pi.nothing", "x", json!([]))
        .unwrap_err();
    assert_eq!(
        error,
        client::runtime::CallError::Server(protocol::ProtocolError::new(
            "service_not_allowed",
            "Remote service pi.nothing is not allowlisted"
        ))
    );
    connection.disconnect();
    drop(server);
    assert_no_processes(&home.root, Duration::from_secs(120));
}

#[test]
fn cli_daemon_new_server_adopts_a_busy_orphan_worker() {
    let provider = provider(vec![text_reply("Detached.")], Duration::from_millis(2500));
    let home = Home::new(provider.port);
    let mut first = start_server(&home, common::SERVER_ID);
    let route = first.route();
    let connection = Connection::connect(&route.path, &route.server_id).unwrap();
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
    connection.wait_attachment(|a| a.is_some()).unwrap();
    let session: RpcTarget = connection.session_target().unwrap();
    let prompting = connection.start(
        session,
        "pi.agent-controller",
        "prompt",
        json!([{"message": "long", "images": null}]),
    );
    assert!(prompting.is_ok());
    let deadline = Instant::now() + Duration::from_secs(120);
    while provider.requests.lock().unwrap().is_empty() {
        assert!(Instant::now() < deadline, "the run never reached the provider");
        std::thread::sleep(Duration::from_millis(20));
    }
    let worker = worker_pid(&home, &id).expect("the worker runs");
    // The server generation dies mid-run; its busy worker keeps working, orphaned behind the coordinator.
    first.stop();
    let second = start_server(&home, common::SERVER_ID);
    let route = second.route();
    let adopter = Connection::connect(&route.path, &route.server_id).unwrap();
    adopter
        .call(adopter.server_target(), "pi.session-management", "attach", json!([id]))
        .unwrap();
    adopter.wait_attachment(|a| a.is_some()).unwrap();
    assert_eq!(
        worker_pid(&home, &id),
        Some(worker),
        "the orphan was adopted, not replaced"
    );
    let transcript = adopter
        .subscribe(adopter.session_target().unwrap(), "pi.transcript")
        .unwrap();
    while transcript_roles(&adopter, &transcript) != ["user", "assistant"] {
        assert!(Instant::now() < deadline, "the adopted worker did not finish its run");
        std::thread::sleep(Duration::from_millis(50));
    }
    connection.disconnect();
    adopter.disconnect();
    drop(second);
    drop(first);
    assert_no_processes(&home.root, Duration::from_secs(120));
}

/// `pi.session-management.describe` carries `allMessagesText` (what Pi's selector searches): the text of every user
/// and assistant message, for a session file the server lists (recorded in session-journal's all-messages fixture).
#[test]
fn cli_daemon_describe_lists_all_messages_text() {
    let fixture: Value =
        serde_json::from_str(include_str!("../../session-journal/fixtures/all-messages.json")).unwrap();
    let case = fixture
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "later-messages-are-searched")
        .unwrap();
    let provider = provider(vec![], Duration::ZERO);
    let home = Home::new(provider.port);
    let folder = home.sessions().join("--work-project--");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(
        folder.join("2026-09-21T14-13-20-000Z_11111111-aaaa.jsonl"),
        case["nativeText"].as_str().unwrap(),
    )
    .unwrap();
    let server = start_server(&home, common::SERVER_ID);
    let route = server.route();
    let connection = Connection::connect(&route.path, &route.server_id).unwrap();
    let listed = connection
        .call(
            connection.server_target(),
            "pi.session-management",
            "describe",
            json!([{}]),
        )
        .unwrap()
        .unwrap();
    let listing = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["sessionId"] == "11111111-aaaa")
        .unwrap_or_else(|| panic!("{listed}"));
    assert_eq!(listing["firstMessage"], "fix the parser");
    assert_eq!(listing["allMessagesText"], case["allMessagesText"]);
    assert_eq!(listing["messageCount"], 4);
    connection.disconnect();
    drop(server);
    assert_no_processes(&home.root, Duration::from_secs(120));
}
