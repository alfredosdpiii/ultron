//! The session worker's settings and control members, `pi.presentation-plugins`, `Models.refresh`,
//! `AgentController.resume` and client cancellation, through the real Rust server and session worker
//! (experimental/services/session-control-provider.ts, services/server.ts, packages/server server.ts `handleCancel` /
//! `handleRequest`, session-worker.ts `onOperationCancel`). Offline: `PI_OFFLINE=1`, a loopback provider.

#[path = "daemon_common/mod.rs"]
mod common;

use client::runtime::{CallError, Connection};
use common::{HANG_GUARD, Home, SERVER_ID};
use protocol::{ClientMessage, DEFAULT_MAX_FRAME_LENGTH, RpcTarget, ServerMessage, ServerMessageDecoder};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::process::{Child, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Server {
    child: Child,
    socket: std::path::PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start(home: &Home) -> Server {
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
        .env("PI_OFFLINE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let socket = home.server_dir().join(format!("{SERVER_ID}.sock"));
    wait_until("server start", || Connection::connect(&socket, SERVER_ID).is_ok());
    Server { child, socket }
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + HANG_GUARD;
    while !done() {
        assert!(Instant::now() < deadline, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn management(connection: &Connection, member: &str, args: Value) -> Result<Option<Value>, CallError> {
    connection.call(connection.server_target(), "pi.session-management", member, args)
}

/// Create a session and attach this connection to it; returns its id.
fn attached(connection: &Connection) -> String {
    let created = management(connection, "create", json!([{}])).unwrap().unwrap();
    let id = created["sessionId"].as_str().unwrap().to_string();
    management(connection, "attach", json!([id])).unwrap();
    connection
        .wait_attachment(|a| a.is_some_and(|a| a.session_id == id))
        .unwrap();
    id
}

fn code(error: CallError) -> String {
    match error {
        CallError::Server(error) => format!("{}: {}", error.code, error.message),
        CallError::Disconnected(message) => format!("disconnected: {message}"),
    }
}

#[test]
fn cli_daemon_presentation_plugins_need_a_prepared_selection_per_client() {
    let home = Home::new(1);
    let server = start(&home);
    let connection = Connection::connect(&server.socket, SERVER_ID).unwrap();
    let plugins = |member: &str, args: Value| {
        connection.call(connection.server_target(), "pi.presentation-plugins", member, args)
    };
    // `reload` before `prepareSession` throws a plain error: the client sees the internal error.
    assert_eq!(
        code(plugins("reload", json!([])).unwrap_err()),
        "internal_error: Internal server error"
    );
    let created = management(&connection, "create", json!([{}])).unwrap().unwrap();
    let id = created["sessionId"].as_str().unwrap();
    assert_eq!(
        code(
            plugins(
                "prepareSession",
                json!([{"sessionId": "00000000-0000-4000-8000-00000000dead", "packagePaths": null}])
            )
            .unwrap_err()
        ),
        "session_not_found: Unknown session: 00000000-0000-4000-8000-00000000dead"
    );
    assert_eq!(
        plugins("prepareSession", json!([{"sessionId": id, "packagePaths": null}])).unwrap(),
        Some(json!({"presentationFacetBundles": []}))
    );
    assert_eq!(
        plugins("reload", json!([])).unwrap(),
        Some(json!({"presentationFacetBundles": []}))
    );
    // The selection belongs to the client that prepared it.
    let other = Connection::connect(&server.socket, SERVER_ID).unwrap();
    assert_eq!(
        code(
            other
                .call(other.server_target(), "pi.presentation-plugins", "reload", json!([]))
                .unwrap_err()
        ),
        "internal_error: Internal server error"
    );
}

#[test]
fn cli_daemon_session_control_settings_trust_debug_and_models_through_the_worker() {
    let home = Home::new(1);
    let server = start(&home);
    let connection = Connection::connect(&server.socket, SERVER_ID).unwrap();
    attached(&connection);
    let session = connection.session_target().unwrap();
    let control = |member: &str, args: Value| {
        connection
            .call(session.clone(), "ultron.session-control", member, args)
            .map_err(code)
    };
    let read = control("readSettings", json!([])).unwrap().unwrap();
    assert_eq!(read["cwd"], json!(home.cwd().display().to_string()));
    assert_eq!(read["agentDir"], json!(home.agent().display().to_string()));
    assert_eq!(read["values"]["hideThinkingBlock"], false);
    assert_eq!(read["savedTrust"], Value::Null);
    assert_eq!(
        control("setSetting", json!(["hideThinkingBlock", true])).unwrap(),
        Some(json!({"applied": "saved"}))
    );
    assert_eq!(
        control("setSetting", json!(["followUpMode", "all"])).unwrap(),
        Some(json!({"applied": "live"}))
    );
    assert_eq!(
        control("getSettings", json!([])).unwrap().unwrap()["followUpMode"],
        "all"
    );
    assert_eq!(
        control("setSetting", json!(["steeringMode", "sometimes"])).unwrap_err(),
        "internal_error: Internal server error"
    );
    let settings: Value =
        serde_json::from_str(&std::fs::read_to_string(home.agent().join("settings.json")).unwrap()).unwrap();
    assert_eq!(settings["hideThinkingBlock"], true);
    assert_eq!(settings["followUpMode"], "all");
    // `/trust`: saved to the profile's trust.json, read back as the saved decision.
    let cwd = home.cwd().display().to_string();
    control("setProjectTrust", json!([[{"path": cwd, "decision": false}]])).unwrap();
    let read = control("readSettings", json!([])).unwrap().unwrap();
    assert_eq!(read["savedTrust"], json!({"path": cwd, "decision": false}));
    let info = control("debugInfo", json!([])).unwrap().unwrap();
    assert_eq!(info["model"], "anthropic/claude-sonnet-4-5");
    assert_ne!(info["pid"], json!(server.child.id()));
    assert_eq!(info["cwd"], json!(cwd));
    assert_eq!(info["environment"]["PI_OFFLINE"], "1");
    assert!(info["kernelPool"].is_object());
    let available = control("reloadAuth", json!([null])).unwrap().unwrap()["availableModels"]
        .as_u64()
        .unwrap();
    assert!(available > 0, "the stored Anthropic key makes models available");
    control("setLabel", json!(["missing-entry", ""])).ok();
    // Pi extensions (`session_before_fork`) are not supported by ultron-rs: nothing cancels a fork.
    assert_eq!(
        control("beforeFork", json!(["e", "before"])).unwrap(),
        Some(json!({"cancelled": false}))
    );
    // `Models.refresh` (offline): the catalogue and the refresh status are published on `pi.models`.
    let models = connection.subscribe(session.clone(), "pi.models").unwrap();
    connection
        .call(session.clone(), "pi.models", "refresh", json!([]))
        .unwrap();
    wait_until("the refresh is published", || {
        connection
            .state(&models, "state")
            .is_some_and(|s| s["refresh"]["status"] == "done")
    });
    let state = connection.state(&models, "state").unwrap();
    assert_eq!(
        state["catalog"]["availableModels"].as_array().unwrap().len() as u64,
        available
    );
    assert_eq!(state["configuration"]["model"]["modelId"], "claude-sonnet-4-5");
    // `Models.select` (models-provider.ts): the session's model, saved as the default (`setDefaultModelAndProvider`).
    let selected = connection
        .call(
            session.clone(),
            "pi.models",
            "select",
            json!([{"provider": "anthropic", "modelId": "claude-haiku-4-5"}]),
        )
        .unwrap()
        .unwrap();
    assert_eq!(selected["id"], "claude-haiku-4-5");
    let settings: Value =
        serde_json::from_str(&std::fs::read_to_string(home.agent().join("settings.json")).unwrap()).unwrap();
    assert_eq!(
        (&settings["defaultProvider"], &settings["defaultModel"]),
        (&json!("anthropic"), &json!("claude-haiku-4-5"))
    );
    assert_eq!(settings["followUpMode"], "all");
    // Nothing interrupted, nothing queued: nothing to resume.
    assert_eq!(
        connection
            .call(session.clone(), "pi.agent-controller", "resume", json!([]))
            .unwrap(),
        Some(
            json!({"accepted": false, "operationId": null, "error": {"code": "nothing_to_resume",
            "message": "Lane \"main\" has no active operation to resume"}})
        )
    );
}

#[test]
fn cli_daemon_cancel_stops_waiting_and_aborts_the_bash_it_ran() {
    let home = Home::new(1);
    let server = start(&home);
    let connection = Connection::connect(&server.socket, SERVER_ID).unwrap();
    attached(&connection);
    let session = connection.session_target().unwrap();
    // A long poll, cancelled: the server answers `cancelled` instead of waiting for the worker.
    let poll = connection
        .start(session.clone(), "ultron.extension-ui", "poll", json!([0, 60_000]))
        .unwrap();
    connection.cancel(session.clone(), &poll).unwrap();
    assert_eq!(
        code(connection.wait(&poll).unwrap_err()),
        "cancelled: RPC request cancelled"
    );
    // A cancel for another target, or for no active request, changes nothing.
    connection.cancel(connection.server_target(), "request-999").unwrap();
    // A `!` command whose call is cancelled is aborted in the worker (its call's context is cancelled).
    let marker = home.root.join("bash-started");
    let command = format!("touch {}; sleep 60; echo finished", marker.display());
    let bash = connection
        .start(
            session.clone(),
            "ultron.session-control",
            "bash",
            json!([command, false]),
        )
        .unwrap();
    wait_until("the command started", || marker.exists());
    connection.cancel(session.clone(), &bash).unwrap();
    assert_eq!(
        code(connection.wait(&bash).unwrap_err()),
        "cancelled: RPC request cancelled"
    );
    wait_until("the cancelled command is recorded", || {
        connection
            .call(session.clone(), "ultron.session-ops", "op", json!([{"op": "messages"}]))
            .ok()
            .flatten()
            .and_then(|m| m.as_array().cloned())
            .is_some_and(|m| m.iter().any(|m| m["role"] == "bashExecution" && m["cancelled"] == true))
    });
    // The session still serves calls afterwards.
    assert!(
        connection
            .call(session, "ultron.session-control", "getSettings", json!([]))
            .is_ok()
    );
}

/// A raw protocol connection (the runtime client never reuses a request id).
struct Raw {
    stream: UnixStream,
    decoder: ServerMessageDecoder,
    queued: Vec<ServerMessage>,
}

impl Raw {
    fn send(&mut self, message: &ClientMessage) {
        let frame = protocol::encode_client_message(message, DEFAULT_MAX_FRAME_LENGTH).unwrap();
        self.stream.write_all(&frame).unwrap();
    }

    fn next(&mut self) -> ServerMessage {
        let mut buffer = [0u8; 65536];
        while self.queued.is_empty() {
            let n = self.stream.read(&mut buffer).unwrap();
            assert!(n > 0, "server closed the connection");
            self.queued.extend(self.decoder.push(&buffer[..n]).unwrap());
        }
        self.queued.remove(0)
    }

    fn request(&mut self, id: &str, target: RpcTarget, call: Value) {
        self.send(&ClientMessage::Request {
            id: id.into(),
            target,
            call: client::runtime::to_cbor(&call),
        });
    }

    /// The next response (attachment updates are skipped), as `(id, Ok(result) | Err("code: message"))`.
    fn response(&mut self) -> (String, Result<Option<Value>, String>) {
        loop {
            if let ServerMessage::Response { id, result } = self.next() {
                return (
                    id,
                    result
                        .map(|r| r.as_ref().map(client::runtime::to_json))
                        .map_err(|e| format!("{}: {}", e.code, e.message)),
                );
            }
        }
    }
}

#[test]
fn cli_daemon_an_active_request_id_cannot_be_reused() {
    let home = Home::new(1);
    let server = start(&home);
    let connection = Connection::connect(&server.socket, SERVER_ID).unwrap();
    let created = management(&connection, "create", json!([{}])).unwrap().unwrap();
    let id = created["sessionId"].as_str().unwrap().to_string();
    let stream = UnixStream::connect(&server.socket).unwrap();
    stream.set_read_timeout(Some(HANG_GUARD)).unwrap();
    let mut raw = Raw {
        stream,
        decoder: ServerMessageDecoder::default(),
        queued: Vec::new(),
    };
    raw.send(&ClientMessage::Hello {
        version: protocol::PROTOCOL_VERSION,
    });
    assert!(matches!(raw.next(), ServerMessage::Hello { .. }));
    let server_target = RpcTarget::Server {
        server_id: SERVER_ID.into(),
    };
    raw.request(
        "attach",
        server_target,
        json!({"serviceId": "pi.session-management", "member": "attach", "args": [id]}),
    );
    let attachment = loop {
        match raw.next() {
            ServerMessage::Attachment { attachment: Some(a) } => break a,
            _ => continue,
        }
    };
    assert_eq!(raw.response(), ("attach".to_string(), Ok(None)));
    let session = RpcTarget::Session(attachment);
    let poll = json!({"serviceId": "ultron.extension-ui", "member": "poll", "args": [0, 60_000]});
    raw.request("dup", session.clone(), poll.clone());
    raw.request("dup", session.clone(), poll);
    assert_eq!(
        raw.response(),
        (
            "dup".to_string(),
            Err("invalid_request: Request ID is already active".into())
        )
    );
    // A cancel naming another target leaves the request running; the right one cancels it.
    raw.send(&ClientMessage::Cancel {
        id: "dup".into(),
        target: RpcTarget::Server {
            server_id: SERVER_ID.into(),
        },
    });
    // Still active: reusing its id is refused again.
    raw.request(
        "dup",
        session.clone(),
        json!({"serviceId": "ultron.extension-ui", "member": "poll", "args": [0, 60_000]}),
    );
    assert_eq!(
        raw.response(),
        (
            "dup".to_string(),
            Err("invalid_request: Request ID is already active".into())
        )
    );
    raw.send(&ClientMessage::Cancel {
        id: "dup".into(),
        target: session,
    });
    assert_eq!(
        raw.response(),
        ("dup".to_string(), Err("cancelled: RPC request cancelled".into()))
    );
    let _keep: Arc<Connection> = connection;
}
