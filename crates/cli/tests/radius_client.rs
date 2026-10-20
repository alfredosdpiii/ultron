//! `ultron server` and `ultron client --connect radius://<server id>` (PI_EXPERIMENTAL=1) end to end through a fake
//! Radius gateway on 127.0.0.1 that bridges clients to the server's relay host (`radius_bridge`), never the real
//! gateway: the server command's `Server:` / `Socket:` / `Radius: …` lines (`runServerCommand`), its relay's
//! upgrade (path, host subprotocol, the `--auth-token` bearer) and its `1000 Pi server stopped` close on SIGTERM;
//! the client's listing, prompted turn and attachment over the relay (`runClient` over
//! `createRadiusClientTransportFactory`); offline mode and missing credentials; a detached server's relay; and the
//! client TUI recovering a dropped Radius connection with client-tui.ts's statuses; a one-shot client whose relay
//! drops mid-prompt reconnecting and rejoining its session (`openClientRuntime`'s `RadiusClientReconnect`); and a
//! client without `--connect` over several discovered servers (`runClientOperation`). The server's `Radius: …`
//! lines are the ones the TypeScript command printed (fixtures/radius_server.json).

mod radius_bridge;
mod tui_harness;

use radius_bridge::{Bridge, HANG_GUARD};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use tui_harness::{Mock, TempDir, Tui, text_reply};

const SERVER_ID: &str = "00000000-0000-4000-8000-0000000000aa";
const HOST: &str = "pi-session-relay.host.v1";
const CLIENT: &str = "pi-session-relay.client.v1";

/// A private root: agent dir (models.json at the mock provider, session naming off), server dir, cwd.
fn root(port: u16) -> TempDir {
    let tmp = TempDir::new();
    let agent = tmp.0.join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::create_dir_all(tmp.0.join("cwd")).unwrap();
    std::fs::create_dir_all(tmp.0.join("server")).unwrap();
    std::fs::write(
        agent.join("models.json"),
        format!(r#"{{"providers":{{"anthropic":{{"baseUrl":"http://127.0.0.1:{port}"}}}}}}"#),
    )
    .unwrap();
    std::fs::write(agent.join("settings.json"), r#"{"sessionNaming":{"enabled":false}}"#).unwrap();
    tmp
}

fn ultron(root: &Path, gateway: &str, args: &[&str], env: &[(&str, &str)]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ultron"));
    command
        .args(args)
        .current_dir(root.join("cwd"))
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", root)
        .env("ULTRON_CODING_AGENT_DIR", root.join("agent"))
        .env("ULTRON_SERVER_DIR", root.join("server"))
        .env("ANTHROPIC_API_KEY", "test-key")
        .env("PI_EXPERIMENTAL", "1")
        .env("PI_RADIUS_GATEWAY", gateway)
        .env("NO_COLOR", "1")
        .env("TZ", "UTC");
    for (key, value) in env {
        command.env(key, value);
    }
    command
}

/// `ultron server` as a child of this test, its stdout lines read as they come.
struct ServerProcess {
    child: Child,
    lines: Receiver<String>,
}

impl ServerProcess {
    fn start(root: &Path, gateway: &str, args: &[&str], env: &[(&str, &str)]) -> ServerProcess {
        let mut full = vec!["server"];
        full.extend(args);
        let mut child = ultron(root, gateway, &full, env)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        ServerProcess { child, lines }
    }

    fn line(&self) -> String {
        self.lines
            .recv_timeout(HANG_GUARD)
            .expect("a line from the server command")
    }

    /// SIGTERM this test's own child and wait for it; returns its exit code.
    fn stop(&mut self) -> Option<i32> {
        let pid = rustix::process::Pid::from_raw(self.child.id() as i32).unwrap();
        rustix::process::kill_process(pid, rustix::process::Signal::TERM).unwrap();
        self.child.wait().unwrap().code()
    }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// The first `Radius: …` line the TypeScript `ultron server` printed in a recorded scenario (fixtures/radius_server.mjs).
fn recorded_status(scenario: &str) -> String {
    let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/radius_server.json")).unwrap();
    let cases: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap();
    let case = cases
        .iter()
        .find(|c| c["name"] == scenario)
        .expect("a recorded scenario");
    case["stdout"].as_str().unwrap().lines().nth(2).unwrap().to_string()
}

fn run(command: &mut Command) -> (String, String, Option<i32>) {
    let output = command.stdin(Stdio::null()).output().unwrap();
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
        output.status.code(),
    )
}

#[test]
fn cli_radius_server_relays_a_client_through_the_gateway() {
    let mock = Mock::start(vec![text_reply("Relayed answer"), text_reply("Local answer")], None);
    let tmp = root(mock.port);
    let bridge = Bridge::start();
    let mut server = ServerProcess::start(
        &tmp.0,
        &bridge.origin(),
        &["--server-id", SERVER_ID, "--auth-token", "host-secret"],
        &[],
    );
    assert_eq!(server.line(), format!("Server: {SERVER_ID}"));
    let socket = server.line();
    assert!(
        socket.starts_with("Socket: ") && socket.ends_with(&format!("{SERVER_ID}.sock")),
        "{socket}"
    );
    assert!(Path::new(socket.trim_start_matches("Socket: ")).exists(), "{socket}");
    assert_eq!(server.line(), recorded_status("connected"));
    let host = bridge.wait_upgrades(HOST, 1);
    assert_eq!(host[0].path, format!("/v1/session-relays/{SERVER_ID}/connect"));
    assert_eq!(host[0].authorization, "Bearer host-secret");
    bridge.wait_host(SERVER_ID);

    // A token file's content is trimmed (`RadiusRelayAuthResolver`).
    let token = tmp.0.join("token");
    std::fs::write(&token, "  client-secret\n").unwrap();
    let address = format!("radius://{SERVER_ID}");
    let token_arg = token.display().to_string();
    let client = |args: &[&str]| {
        let mut full = vec!["client", "--connect", &address, "--auth-token-file", &token_arg];
        full.extend(args);
        run(&mut ultron(&tmp.0, &bridge.origin(), &full, &[]))
    };
    let (out, err, code) = client(&[]);
    assert_eq!((out.as_str(), err.as_str(), code), ("", "", Some(0)));
    let (out, err, code) = client(&["Say hello"]);
    assert_eq!((out.as_str(), code), ("Relayed answer\n", Some(0)), "{err}");
    let (listed, _, code) = client(&[]);
    assert_eq!(code, Some(0));
    let session = listed
        .trim_end()
        .strip_prefix(&format!("{SERVER_ID}\t"))
        .unwrap_or_else(|| panic!("one session listed: {listed:?}"))
        .to_string();
    let (out, _, code) = client(&["--session-id", &session]);
    assert_eq!((out, code), (format!("{SERVER_ID}\t{session}\tattached\n"), Some(0)));
    let (out, err, code) = client(&["--session-id", "missing-session"]);
    assert_eq!(
        (out.as_str(), err.as_str(), code),
        (
            "",
            "Error: No discovered server contains session missing-session\n",
            Some(1)
        )
    );
    // Every client offered the client subprotocol with the file's token, at the server's relay path.
    let clients = bridge.wait_upgrades(CLIENT, 5);
    for upgrade in &clients {
        assert_eq!(upgrade.path, format!("/v1/session-relays/{SERVER_ID}/connect"));
        assert_eq!(upgrade.authorization, "Bearer client-secret");
    }
    // Each client closed its relay connection as it left (`1000 Pi client closed`).
    assert!(
        clients
            .iter()
            .all(|u| u.closed.as_deref() == Some("1000 Pi client closed")),
        "{clients:?}"
    );

    // The same server over its Unix socket (`--connect unix://…`), and found by discovery (no `--connect`).
    let unix = format!("unix://{}", socket.trim_start_matches("Socket: "));
    let (out, err, code) = run(&mut ultron(
        &tmp.0,
        &bridge.origin(),
        &["client", "--connect", &unix, "Again"],
        &[],
    ));
    assert_eq!((out.as_str(), code), ("Local answer\n", Some(0)), "{err}");
    let (listed, err, code) = run(&mut ultron(&tmp.0, &bridge.origin(), &["client"], &[]));
    assert_eq!(code, Some(0), "{err}");
    let mut ids: Vec<&str> = listed
        .lines()
        .map(|line| {
            line.strip_prefix(&format!("{SERVER_ID}\t"))
                .expect("this server's session")
        })
        .collect();
    assert_eq!(ids.len(), 2, "{listed}");
    assert!(ids.contains(&session.as_str()));
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(ids, sorted);
    ids.clear();

    // SIGTERM: the server closes its relay (`1000 Pi server stopped`) and exits 0.
    assert_eq!(server.stop(), Some(0));
    assert_eq!(
        bridge.wait_host_closed().closed.as_deref(),
        Some("1000 Pi server stopped")
    );
}

#[test]
fn cli_radius_offline_and_missing_credentials_stay_local() {
    let tmp = root(1);
    let bridge = Bridge::start();
    // Offline: no relay, and a Radius client is refused before any connection.
    let mut offline = ServerProcess::start(
        &tmp.0,
        &bridge.origin(),
        &["--server-id", SERVER_ID],
        &[("PI_OFFLINE", "1")],
    );
    assert_eq!(offline.line(), format!("Server: {SERVER_ID}"));
    offline.line();
    assert_eq!(offline.line(), recorded_status("offline"));
    let address = format!("radius://{SERVER_ID}");
    let (out, err, code) = run(&mut ultron(
        &tmp.0,
        &bridge.origin(),
        &["client", "--connect", &address, "--auth-token", "t"],
        &[("PI_OFFLINE", "1")],
    ));
    assert_eq!(
        (out.as_str(), err.as_str(), code),
        (
            "",
            "Error: Radius relay connections are unavailable in offline mode\n",
            Some(1)
        )
    );
    assert_eq!(offline.stop(), Some(0));

    // No credential stored: the server stays local; a client is told to log in.
    let mut unauthenticated = ServerProcess::start(&tmp.0, &bridge.origin(), &["--server-id", SERVER_ID], &[]);
    assert_eq!(unauthenticated.line(), format!("Server: {SERVER_ID}"));
    unauthenticated.line();
    assert_eq!(unauthenticated.line(), recorded_status("no-credential"));
    let (out, err, code) = run(&mut ultron(
        &tmp.0,
        &bridge.origin(),
        &["client", "--connect", &address],
        &[],
    ));
    assert_eq!(
        (out.as_str(), err.as_str(), code),
        (
            "",
            "Error: Radius authentication is required; start Pi and run /login radius, then retry\n",
            Some(1)
        )
    );
    assert_eq!(unauthenticated.stop(), Some(0));
    // Neither ever reached the gateway.
    assert_eq!(bridge.upgrades(), Vec::new());

    // Validation before any connection (`openClientRuntime`).
    for (args, error) in [
        (
            vec!["client", "--auth-token", "t"],
            "Error: Authentication is only supported for experimental Radius connections\n",
        ),
        (
            vec!["client", "--connect", &address, "--model", "m"],
            "Error: Model selection is only valid when automatically activating a new server\n",
        ),
        (
            vec!["client", "--connect", &address, "-e", "./plugin"],
            "Error: Plugin package paths can only be configured on a local Unix server\n",
        ),
        (
            vec!["client", "--connect", "unix:///tmp/not-a-server.sock"],
            "Error: --connect path must end with <uuidv4-server-id>.sock\n",
        ),
        (
            vec!["client", "--connect", "radius://x"],
            "Error: Radius transport address requires a lowercase UUIDv4 server ID\n",
        ),
    ] {
        let (out, err, code) = run(&mut ultron(&tmp.0, &bridge.origin(), &args, &[]));
        assert_eq!((out.as_str(), err.as_str(), code), ("", error, Some(1)), "{args:?}");
    }
    // Without PI_EXPERIMENTAL the words are the stable CLI's (a prompt), never the experimental commands.
    assert!(
        cli::experimental::run(
            &["server".to_string()],
            &env_without_experimental(),
            &mut Vec::new(),
            &mut Vec::new()
        )
        .is_none()
    );
}

fn env_without_experimental() -> cli::Env {
    cli::Env {
        cwd: PathBuf::from("/"),
        var: std::sync::Arc::new(|_| None),
        docs: PathBuf::new(),
        home: PathBuf::new(),
    }
}

#[test]
fn cli_radius_detached_server_runs_the_relay_with_the_stored_credential() {
    let tmp = root(1);
    let bridge = Bridge::start();
    // `ultron durable start` activates a detached `__internal` server; its relay uses the stored credential (here
    // RADIUS_API_KEY) on PI_RADIUS_GATEWAY.
    let durable = tmp.0.join("durable");
    let (out, err, code) = run(&mut ultron(
        &tmp.0,
        &bridge.origin(),
        &["durable", "start"],
        &[
            ("RADIUS_API_KEY", "stored-key"),
            ("ULTRON_SERVER_DIR", &durable.display().to_string()),
            ("ULTRON_SERVER_ID", SERVER_ID),
        ],
    ));
    assert_eq!(code, Some(0), "{err}");
    assert!(out.starts_with(&format!("{SERVER_ID}\t")), "{out}");
    let host = bridge.wait_upgrades(HOST, 1);
    assert_eq!(
        (host[0].path.as_str(), host[0].authorization.as_str()),
        (
            format!("/v1/session-relays/{SERVER_ID}/connect").as_str(),
            "Bearer stored-key"
        )
    );
    let (_, err, code) = run(&mut ultron(
        &tmp.0,
        &bridge.origin(),
        &["durable", "stop"],
        &[("ULTRON_SERVER_DIR", &durable.display().to_string())],
    ));
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(
        bridge.wait_host_closed().closed.as_deref(),
        Some("1000 Pi server stopped")
    );
}

#[test]
fn cli_radius_client_tui_recovers_a_dropped_connection() {
    let mock = Mock::start(
        vec![
            text_reply("First answer"),
            text_reply("Second answer"),
            text_reply("Third answer"),
        ],
        None,
    );
    let tmp = root(mock.port);
    let bridge = Bridge::start();
    let mut server = ServerProcess::start(
        &tmp.0,
        &bridge.origin(),
        &["--server-id", SERVER_ID, "--auth-token", "host-secret"],
        &[],
    );
    server.line();
    server.line();
    assert_eq!(server.line(), recorded_status("connected"));
    bridge.wait_host(SERVER_ID);
    let address = format!("radius://{SERVER_ID}");
    let models = std::fs::read_to_string(tmp.0.join("agent/models.json")).unwrap();
    let origin = bridge.origin();
    let tui = Tui::start_with_models(
        &tmp.0,
        &models,
        100,
        30,
        &["client", "--connect", &address, "--auth-token", "tok"],
        &[("PI_EXPERIMENTAL", "1"), ("PI_RADIUS_GATEWAY", &origin)],
    );
    tui.wait("Session: <ID>");
    tui.send_text("Say hello");
    tui.send_keys(&["Enter"]);
    tui.wait("First answer");

    // The gateway loses the client and holds its reconnection: the TUI says so and waits.
    bridge.hold_clients(true);
    bridge.drop_clients();
    tui.wait("Radius disconnected; retrying…");
    bridge.wait_held();
    tui.wait("Reconnecting to Radius…");
    // Input waits while recovering.
    tui.send_text("ignored");
    bridge.hold_clients(false);
    let screen = tui.wait("First answer");
    let deadline = std::time::Instant::now() + HANG_GUARD;
    let mut screen = screen;
    while screen.contains("Reconnecting to Radius…") || screen.contains("Reattaching Session…") {
        assert!(
            std::time::Instant::now() < deadline,
            "the TUI never recovered:\n{screen}"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
        screen = tui.capture();
    }
    assert!(!screen.contains("ignored"), "{screen}");
    // The session works again over the new connection.
    tui.send_text("Again");
    tui.send_keys(&["Enter"]);
    tui.wait("Second answer");
    assert_eq!(bridge.wait_upgrades(CLIENT, 2).len(), 2);
    // A second loss is recovered the same way.
    bridge.hold_clients(true);
    bridge.drop_clients();
    bridge.wait_held();
    tui.wait("Reconnecting to Radius…");
    bridge.hold_clients(false);
    let deadline = std::time::Instant::now() + HANG_GUARD;
    let mut screen = tui.capture();
    while screen.contains("Radius") || screen.contains("Reattaching Session…") {
        assert!(
            std::time::Instant::now() < deadline,
            "the TUI never recovered again:\n{screen}"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
        screen = tui.capture();
    }
    tui.send_text("Third");
    tui.send_keys(&["Enter"]);
    tui.wait("Third answer");
    // The server stops first: the TUI's harness waits for every process working in this root to leave.
    assert_eq!(server.stop(), Some(0));
    drop(tui);
}

/// A child's stdout collected as it arrives; waits are on a condition variable.
struct Output {
    text: std::sync::Arc<(std::sync::Mutex<(String, bool)>, std::sync::Condvar)>,
}

impl Output {
    fn of(child: &mut Child) -> Output {
        let mut stdout = child.stdout.take().unwrap();
        let text = std::sync::Arc::new((std::sync::Mutex::new((String::new(), false)), std::sync::Condvar::new()));
        let filling = text.clone();
        std::thread::spawn(move || {
            use std::io::Read;
            let mut buffer = [0u8; 4096];
            loop {
                let n = stdout.read(&mut buffer).unwrap_or(0);
                let (lock, changed) = &*filling;
                let mut state = lock.lock().unwrap();
                if n == 0 {
                    state.1 = true;
                    changed.notify_all();
                    break;
                }
                state.0.push_str(&String::from_utf8_lossy(&buffer[..n]));
                changed.notify_all();
            }
        });
        Output { text }
    }

    /// Wait until the output contains `want` (or it ended); returns it.
    fn wait(&self, want: &str) -> String {
        let (lock, changed) = &*self.text;
        let deadline = std::time::Instant::now() + HANG_GUARD;
        let mut state = lock.lock().unwrap();
        while !state.0.contains(want) && !state.1 {
            let now = std::time::Instant::now();
            assert!(now < deadline, "no {want:?} in {:?}", state.0);
            state = changed.wait_timeout(state, deadline - now).unwrap().0;
        }
        state.0.clone()
    }

    fn all(&self) -> String {
        let (lock, changed) = &*self.text;
        let mut state = lock.lock().unwrap();
        while !state.1 {
            state = changed.wait_timeout(state, HANG_GUARD).unwrap().0;
        }
        state.0.clone()
    }
}

/// An Anthropic reply streaming `first` then `rest`, held after `first` until the mock is released.
fn held_reply(first: &str, rest: &str) -> tui_harness::Reply {
    let start = serde_json::json!({"type": "message_start", "message": {"id": "msg_1", "type": "message", "role": "assistant", "model": "claude-sonnet-4-5", "content": [], "stop_reason": null, "usage": {"input_tokens": 10, "output_tokens": 1}}});
    tui_harness::Reply {
        chunks: tui_harness::sse(&[
            start,
            serde_json::json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": first}}),
            serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": rest}}),
            serde_json::json!({"type": "content_block_stop", "index": 0}),
            serde_json::json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 5}}),
            serde_json::json!({"type": "message_stop"}),
        ]),
        hold_after: Some(3),
    }
}

#[test]
fn cli_radius_client_prompt_survives_a_dropped_relay() {
    // `openClientRuntime` gives the Radius route a `RadiusClientReconnect`: the one-shot client's relay drops while
    // its turn streams; it reconnects (1 s after the loss), re-attaches the Session and rejoins it, and prints the
    // rest of the answer once the turn ends.
    let mock = Mock::start(vec![held_reply("Hello ", "world"), text_reply("Next answer")], None);
    let tmp = root(mock.port);
    let bridge = Bridge::start();
    let mut server = ServerProcess::start(
        &tmp.0,
        &bridge.origin(),
        &["--server-id", SERVER_ID, "--auth-token", "host-secret"],
        &[],
    );
    server.line();
    server.line();
    assert_eq!(server.line(), recorded_status("connected"));
    bridge.wait_host(SERVER_ID);
    let address = format!("radius://{SERVER_ID}");
    let mut client = ultron(
        &tmp.0,
        &bridge.origin(),
        &["client", "--connect", &address, "--auth-token", "tok", "Say hello"],
        &[],
    )
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
    let output = Output::of(&mut client);
    assert_eq!(output.wait("Hello "), "Hello ");
    // The gateway loses the client and holds its reconnection; the turn goes on in the worker meanwhile.
    bridge.hold_clients(true);
    bridge.drop_clients();
    bridge.wait_held();
    bridge.hold_clients(false);
    assert_eq!(bridge.wait_upgrades(CLIENT, 2).len(), 2);
    mock.release();
    let status = client.wait().unwrap();
    let mut errors = String::new();
    std::io::Read::read_to_string(&mut client.stderr.take().unwrap(), &mut errors).unwrap();
    assert_eq!((output.all().as_str(), errors.as_str()), ("Hello world\n", ""));
    assert_eq!(status.code(), Some(0));
    let clients = bridge.wait_upgrades(CLIENT, 2);
    assert!(
        clients
            .iter()
            .all(|u| u.authorization == "Bearer tok" && u.path == format!("/v1/session-relays/{SERVER_ID}/connect")),
        "{clients:?}"
    );
    // The new relay connection closed as the client left; the session is the one the turn ran in.
    let (listed, _, code) = run(&mut ultron(
        &tmp.0,
        &bridge.origin(),
        &["client", "--connect", &address, "--auth-token", "tok"],
        &[],
    ));
    assert_eq!(code, Some(0));
    assert_eq!(listed.lines().count(), 1, "{listed}");
    let clients = bridge.wait_upgrades(CLIENT, 3);
    assert_eq!(clients[1].closed.as_deref(), Some("1000 Pi client closed"));
    assert_eq!(server.stop(), Some(0));
}

const SERVER_B: &str = "00000000-0000-4000-8000-0000000000bb";
const SERVER_C: &str = "00000000-0000-4000-8000-0000000000cc";

#[test]
fn cli_client_without_connect_works_across_several_servers() {
    // experimental-remote-runtime.test.ts "reports missing and ambiguous session selections", and runClientOperation
    // over several discovered servers: the listing spans them, a session is found on its own server, and a prompt
    // without a session is refused.
    let mock = Mock::start(
        vec![
            text_reply("From A"),
            text_reply("From B"),
            text_reply("Again on B"),
            text_reply("Continued"),
        ],
        None,
    );
    let tmp = root(mock.port);
    let bridge = Bridge::start();
    let gateway = bridge.origin();
    let start = |id: &str, sessions: &str| {
        let dir = tmp.0.join(sessions).display().to_string();
        let server = ServerProcess::start(&tmp.0, &gateway, &["--server-id", id, "--session-dir", &dir], &[]);
        assert_eq!(server.line(), format!("Server: {id}"));
        let socket = server.line().trim_start_matches("Socket: ").to_string();
        assert_eq!(server.line(), recorded_status("no-credential"));
        (server, format!("unix://{socket}"))
    };
    let client = |args: &[&str]| run(&mut ultron(&tmp.0, &gateway, args, &[]));
    let (mut a, a_socket) = start(SERVER_ID, "sessions-a");
    let (mut b, b_socket) = start(SERVER_B, "sessions-b");
    let (out, err, code) = client(&["client", "--connect", &a_socket, "One"]);
    assert_eq!((out.as_str(), code), ("From A\n", Some(0)), "{err}");
    let (out, err, code) = client(&["client", "--connect", &b_socket, "Two"]);
    assert_eq!((out.as_str(), code), ("From B\n", Some(0)), "{err}");

    // The listing: every server's sessions, by server ID.
    let (listed, err, code) = client(&["client"]);
    assert_eq!((err.as_str(), code), ("", Some(0)));
    let rows: Vec<(&str, &str)> = listed.lines().map(|l| l.split_once('\t').unwrap()).collect();
    assert_eq!(
        rows.iter().map(|r| r.0).collect::<Vec<_>>(),
        vec![SERVER_ID, SERVER_B],
        "{listed}"
    );
    let (on_a, on_b) = (rows[0].1.to_string(), rows[1].1.to_string());
    // A session is attached on the server that holds it, and prompted there.
    let (out, _, code) = client(&["client", "--session-id", &on_b]);
    assert_eq!((out, code), (format!("{SERVER_B}\t{on_b}\tattached\n"), Some(0)));
    let (out, err, code) = client(&["client", "--session-id", &on_b, "Three"]);
    assert_eq!((out.as_str(), code), ("Again on B\n", Some(0)), "{err}");
    // --continue takes the most recently modified session of any server (B's, just prompted).
    let (out, err, code) = client(&["client", "-c", "Four"]);
    assert_eq!((out.as_str(), code), ("Continued\n", Some(0)), "{err}");
    let (listed_b, _, _) = client(&["client", "--connect", &b_socket]);
    assert_eq!(listed_b, format!("{SERVER_B}\t{on_b}\n"));
    let holds = |dir: &str, text: &str| {
        let mut found = false;
        let mut stack = vec![tmp.0.join(dir)];
        while let Some(path) = stack.pop() {
            if path.is_dir() {
                stack.extend(std::fs::read_dir(&path).unwrap().map(|e| e.unwrap().path()));
            } else if std::fs::read_to_string(&path).is_ok_and(|t| t.contains(text)) {
                found = true;
            }
        }
        found
    };
    assert!(holds("sessions-b", "Four") && !holds("sessions-a", "Four"));
    let (out, _, code) = client(&["client", "--session-id", &on_a]);
    assert_eq!((out, code), (format!("{SERVER_ID}\t{on_a}\tattached\n"), Some(0)));
    for (args, error) in [
        (
            vec!["client", "--session-id", "missing"],
            "Error: No discovered server contains session missing\n".to_string(),
        ),
        (
            vec!["client", "--session-id", "missing", "Hi"],
            "Error: No discovered server contains session missing\n".to_string(),
        ),
        (
            vec!["client", "New"],
            "Error: Client prompt requires exactly one discovered server to create a Session\n".to_string(),
        ),
        (
            vec!["client", "-e", "./plugin"],
            "Error: Plugin selection requires exactly one local server\n".to_string(),
        ),
        (
            vec!["client", "--model", "m"],
            "Error: Model selection is only valid when automatically activating a new server\n".to_string(),
        ),
    ] {
        let (out, err, code) = client(&args);
        assert_eq!(
            (out.as_str(), err.as_str(), code),
            ("", error.as_str(), Some(1)),
            "{args:?}"
        );
    }
    assert_eq!(b.stop(), Some(0));

    // Two servers over one session directory: each of its sessions is on both.
    let (mut c, _) = start(SERVER_C, "sessions-a");
    let (listed, _, code) = client(&["client"]);
    assert_eq!(code, Some(0));
    assert_eq!(listed, format!("{SERVER_ID}\t{on_a}\n{SERVER_C}\t{on_a}\n"));
    let ambiguous = format!("Error: Session {on_a} is available from more than one server\n");
    for args in [
        vec!["client", "--session-id", on_a.as_str()],
        vec!["client", "--session-id", on_a.as_str(), "Hi"],
        vec!["client", "--continue", "Hi"],
    ] {
        let (out, err, code) = client(&args);
        assert_eq!(
            (out.as_str(), err.as_str(), code),
            ("", ambiguous.as_str(), Some(1)),
            "{args:?}"
        );
    }
    assert_eq!(c.stop(), Some(0));
    assert_eq!(a.stop(), Some(0));
    assert_eq!(bridge.upgrades(), Vec::new());
}

/// Waits (bounded by the hang guard) for `path` to exist.
fn wait_for_file(path: &Path) {
    let deadline = std::time::Instant::now() + HANG_GUARD;
    while !path.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[test]
fn cli_client_attaching_while_the_idle_worker_retires_gets_a_new_worker() {
    // The flake of cli_client_without_connect_works_across_several_servers ("Error: Internal server error" on the
    // second client of a session): a client that attaches just as the session's worker retires (it went idle when
    // the previous client left) had its demand refused and the attach failed. The server now waits for the retiring
    // worker to end and starts a new one. The worker is held in its retirement (RETIRE_GATE) until the second
    // client's demand was refused, so the race is taken every time.
    let mock = Mock::start(vec![text_reply("First"), text_reply("Second")], None);
    let tmp = root(mock.port);
    let bridge = Bridge::start();
    let gate = tmp.0.join("gate");
    std::fs::create_dir_all(&gate).unwrap();
    let gate_env = gate.display().to_string();
    let dir = tmp.0.join("sessions").display().to_string();
    let mut server = ServerProcess::start(
        &tmp.0,
        &bridge.origin(),
        &["--server-id", SERVER_B, "--session-dir", &dir],
        &[("__ULTRON_TEST_WORKER_RETIRE_GATE", &gate_env)],
    );
    assert_eq!(server.line(), format!("Server: {SERVER_B}"));
    let socket = format!("unix://{}", server.line().trim_start_matches("Socket: "));
    assert_eq!(server.line(), recorded_status("no-credential"));
    let (out, err, code) = run(&mut ultron(
        &tmp.0,
        &bridge.origin(),
        &["client", "--connect", &socket, "One"],
        &[],
    ));
    assert_eq!((out.as_str(), code), ("First\n", Some(0)), "{err}");
    let (listed, _, _) = run(&mut ultron(
        &tmp.0,
        &bridge.origin(),
        &["client", "--connect", &socket],
        &[],
    ));
    let session = listed.trim().split_once('\t').unwrap().1.to_string();
    // The client left: the idle worker retires and waits at the gate.
    wait_for_file(&gate.join("retiring"));
    let second = ultron(
        &tmp.0,
        &bridge.origin(),
        &["client", "--connect", &socket, "--session-id", &session, "Two"],
        &[],
    )
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
    // Its demand reached the retiring worker and was refused; now the worker may go.
    wait_for_file(&gate.join("rejected"));
    assert_eq!(
        std::fs::read_to_string(gate.join("rejected")).unwrap(),
        "Session worker is retiring"
    );
    std::fs::write(gate.join("go"), "").unwrap();
    let output = second.wait_with_output().unwrap();
    assert_eq!(
        (String::from_utf8_lossy(&output.stdout).as_ref(), output.status.code()),
        ("Second\n", Some(0)),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(server.stop(), Some(0));
}
