//! `/mcp` and the MCP notices through the real server and session worker, as the TUI and RPC clients reach them:
//! `pi.legacy-extension-commands` lists and runs `/mcp` (session-worker.ts `legacyExtensionCommands`), and its
//! notices and dialogs reach the client through `ultron.extension-ui` (the worker's `ExtensionUIBridge`): an eager
//! server that needs sign-in announces it when the session starts, `/mcp status` notifies, and `/mcp` without
//! arguments opens the menu (pick a server, then an action) for a client that serves dialogs. `--no-extensions`
//! leaves MCP out of the session (session-worker.ts skips the built-in MCP extension).

#[path = "daemon_common/mod.rs"]
mod common;

use client::runtime::Connection;
use common::{HANG_GUARD, Home, provider, start_server, text_reply};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn python() -> String {
    std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into())
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../mcp/fixtures").join(name)
}

struct OAuthServer(Child, String);

impl Drop for OAuthServer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn oauth_server() -> OAuthServer {
    let mut child = Command::new(python())
        .arg(fixture("fake_oauth_server.py"))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    OAuthServer(child, format!("http://127.0.0.1:{}", line.trim()))
}

fn write_mcp_json(home: &Home, oauth: &OAuthServer) {
    let config = json!({ "mcpServers": {
        "beta": { "command": python(), "args": [fixture("fake_gateway_server.py"), "beta", "-"] },
        "issues": { "url": format!("{}/mcp", oauth.1), "lifecycle": "eager" },
    }});
    std::fs::write(home.agent().join("mcp.json"), config.to_string()).unwrap();
}

fn attach(connection: &Connection) -> protocol::RpcTarget {
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
    connection.session_target().unwrap()
}

/// A client serving the extension UI: polls with its cursor and keeps every request it received.
struct UiClient<'a> {
    connection: &'a Connection,
    session: protocol::RpcTarget,
    cursor: Option<u64>,
    seen: Vec<Value>,
}

impl UiClient<'_> {
    fn poll(&mut self) {
        let polled = self
            .connection
            .call(
                self.session.clone(),
                "ultron.extension-ui",
                "poll",
                json!([self.cursor, 1000]),
            )
            .unwrap()
            .unwrap();
        self.cursor = polled["cursor"].as_u64();
        for item in polled["requests"].as_array().unwrap() {
            self.seen.push(item["request"].clone());
        }
    }

    /// Polls until a request matches; returns it.
    fn wait_for(&mut self, what: &str, matches: impl Fn(&Value) -> bool) -> Value {
        let start = Instant::now();
        loop {
            if let Some(found) = self.seen.iter().find(|r| matches(r)) {
                let found = found.clone();
                self.seen.retain(|r| r != &found);
                return found;
            }
            assert!(
                start.elapsed() < HANG_GUARD,
                "timed out waiting for {what}: {:?}",
                self.seen
            );
            self.poll();
        }
    }
}

fn notify_containing(text: &'static str) -> impl Fn(&Value) -> bool {
    move |r: &Value| r["method"] == "notify" && r["message"].as_str().is_some_and(|m| m.contains(text))
}

#[test]
fn cli_daemon_mcp_command_and_notices_reach_the_client() {
    let oauth = oauth_server();
    let provider = provider(Vec::new(), Duration::ZERO);
    let home = Home::new(provider.port);
    write_mcp_json(&home, &oauth);
    let server = start_server(&home, common::SERVER_ID);
    let connection = Connection::connect(&server.socket, &server.server_id).unwrap();
    let session = attach(&connection);
    let mut ui = UiClient {
        connection: &connection,
        session: session.clone(),
        cursor: None,
        seen: Vec::new(),
    };
    // The eager server's background connect finds that it needs sign-in (index.ts session_start).
    let notice = ui.wait_for("the sign-in notice", notify_containing("issues"));
    assert_eq!(
        notice,
        json!({"type": "extension_ui_request", "id": notice["id"], "method": "notify",
            "message": "MCP server \"issues\" requires sign-in. Run /mcp to sign in.", "notifyType": "warning"})
    );
    let listed = connection
        .call(session.clone(), "pi.legacy-extension-commands", "list", json!([]))
        .unwrap()
        .unwrap();
    assert_eq!(listed[0]["name"], "mcp");
    assert_eq!(listed[0]["sourceInfo"]["path"], "<inline:mcp>");
    let ran = connection
        .call(
            session.clone(),
            "pi.legacy-extension-commands",
            "run",
            json!(["mcp", "status"]),
        )
        .unwrap()
        .unwrap();
    assert_eq!(ran, json!({"notifications": []}));
    let status = ui.wait_for("the status notice", notify_containing("MCP: "));
    assert_eq!(
        status["message"],
        "MCP: 0/2 servers connected\nbeta: not connected\nissues: needs sign-in"
    );
    // The menu: a server, then an action; the client answers both dialogs.
    let menu = connection
        .start(
            session.clone(),
            "pi.legacy-extension-commands",
            "run",
            json!(["mcp", ""]),
        )
        .unwrap();
    let servers = ui.wait_for("the server menu", |r| r["method"] == "select");
    assert_eq!(servers["title"], "MCP servers");
    assert_eq!(
        servers["options"],
        json!(["beta \u{2014} not connected", "issues \u{2014} needs sign-in"])
    );
    connection
        .call(
            session.clone(),
            "ultron.extension-ui",
            "respond",
            json!([servers["id"], {"value": "beta \u{2014} not connected"}]),
        )
        .unwrap();
    let actions = ui.wait_for("the action menu", |r| r["method"] == "select");
    assert_eq!(actions["title"], "beta (not connected)");
    assert_eq!(actions["options"], json!(["Connect", "Disable"]));
    connection
        .call(
            session.clone(),
            "ultron.extension-ui",
            "respond",
            json!([actions["id"], {"value": "Connect"}]),
        )
        .unwrap();
    let connected = ui.wait_for("the connect notice", notify_containing("Connected to"));
    assert_eq!(connected["message"], "Connected to beta (2 tools).");
    assert_eq!(connection.wait(&menu).unwrap(), Some(json!({"notifications": []})));
    let unknown = connection
        .call(session, "pi.legacy-extension-commands", "run", json!(["nope", ""]))
        .unwrap_err();
    // A plain error of a service crosses the wire as an internal error, as TypeScript's `Unknown extension command`.
    assert!(format!("{unknown:?}").contains("Internal server error"), "{unknown:?}");
}

/// The system prompt of the session's first request: whether the runtime guide names the MCP servers.
fn guide_names_mcp(args: &[&str]) -> bool {
    let oauth = oauth_server();
    let provider = provider(vec![text_reply("done")], Duration::ZERO);
    let home = Home::new(provider.port);
    write_mcp_json(&home, &oauth);
    let mut command_args = vec!["-p", "--no-session"];
    command_args.extend(args);
    command_args.push("hello");
    let output = home.run(&command_args);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let requests = provider.requests.lock().unwrap();
    let system = requests
        .iter()
        .find(|r| r["system"].is_array() || r["system"].is_string())
        .map(|r| r["system"].to_string())
        .unwrap_or_default();
    let _ = Arc::strong_count(&provider.requests);
    system.contains("MCP servers (beta, issues)")
}

/// `--no-extensions` is ignored (Pi extensions are not supported by ultron-rs), so it no longer leaves the built-in
/// MCP gateway out.
#[test]
fn cli_daemon_no_extensions_flag_keeps_mcp() {
    assert!(guide_names_mcp(&[]));
    assert!(guide_names_mcp(&["--no-extensions"]));
}

/// RPC mode relays the worker's extension UI as Pi's `extension_ui_request` lines (rpc-native.ts `uiLoop`): the eager
/// server's sign-in notice reaches the RPC client, and `get_commands` names `/mcp`.
#[test]
fn cli_rpc_mode_relays_mcp_notices_as_extension_ui_requests() {
    use cli::rpc::client::{RpcClient, RpcClientOptions};
    let oauth = oauth_server();
    let provider = provider(Vec::new(), Duration::ZERO);
    let home = Home::new(provider.port);
    write_mcp_json(&home, &oauth);
    let mut client = RpcClient::start(&RpcClientOptions {
        program: PathBuf::from(env!("CARGO_BIN_EXE_ultron")),
        program_args: Vec::new(),
        cwd: Some(home.cwd()),
        env_clear: true,
        env: vec![
            ("PATH".into(), std::env::var("PATH").unwrap_or_default()),
            ("HOME".into(), home.root.display().to_string()),
            ("ULTRON_CODING_AGENT_DIR".into(), home.agent().display().to_string()),
            ("ULTRON_SERVER_DIR".into(), home.server_dir().display().to_string()),
            ("ANTHROPIC_API_KEY".into(), "test-key".into()),
        ],
        provider: Some("anthropic".into()),
        model: Some("claude-sonnet-4-5".into()),
        args: vec!["--no-session".into()],
    })
    .unwrap();
    let notice = client
        .wait_for(
            &|line| line["type"] == "extension_ui_request" && line["method"] == "notify",
            HANG_GUARD,
        )
        .unwrap();
    assert_eq!(
        notice["message"],
        "MCP server \"issues\" requires sign-in. Run /mcp to sign in."
    );
    assert_eq!(notice["notifyType"], "warning");
    let commands = client.get_commands().unwrap();
    assert!(commands.to_string().contains("\"name\":\"mcp\""), "{commands}");
    client.stop();
}
