//! `ultron server` and `ultron client --connect radius://<server id>` (PI_EXPERIMENTAL=1) end to end through a fake
//! Radius gateway on 127.0.0.1 that bridges clients to the server's relay host (`radius_bridge`), never the real
//! gateway: the server command's `Server:` / `Socket:` / `Radius: …` lines (`runServerCommand`), its relay's
//! upgrade (path, host subprotocol, the `--auth-token` bearer) and its `1000 Pi server stopped` close on SIGTERM;
//! the client's listing, prompted turn and attachment over the relay (`runClient` over
//! `createRadiusClientTransportFactory`); offline mode and missing credentials; a detached server's relay; and the
//! client TUI recovering a dropped Radius connection with client-tui.ts's statuses.

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
    assert_eq!(server.line(), "Radius: connected");
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
    assert_eq!(offline.line(), "Radius: not connected; local only");
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
    assert_eq!(unauthenticated.line(), "Radius: not connected; local only");
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
    // `ultron durable` activates a detached `__internal` server; its relay uses the stored credential (here
    // RADIUS_API_KEY) on PI_RADIUS_GATEWAY.
    let durable = tmp.0.join("durable");
    let (out, err, code) = run(&mut ultron(
        &tmp.0,
        &bridge.origin(),
        &["durable"],
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
    assert_eq!(server.line(), "Radius: connected");
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
