//! The Radius relay host against `fixtures/radius-relay.json`, recorded from the TypeScript `RadiusRelayHost` by
//! `fixtures/generate-radius-relay.mjs`: the same gateway scripts run against a fake gateway on 127.0.0.1, and the
//! sockets (path, subprotocol, authorization, every message the relay sent, its close), statuses, `Radius: …` lines,
//! retry delays, accepted connections and handler events must match. Plus the TypeScript unit tests of the relay
//! envelope and credential resolution, with their literal expectations.
//!
//! Differences the replay bridges: the gateway is this fake (plain `ws:` on loopback; the recorded `wss:` scheme is
//! checked by `relay_web_socket_url`'s own test); before a server-side send or close the replay pings the relay and
//! drops that pong from the record (the TypeScript fake socket handles messages synchronously, the real one does not).

mod support;

use protocol::websocket::Message;
use serde_json::{Value, json};
use server::radius_relay::{
    AuthInput, HostControl, RadiusRelayAuthResolver, RadiusRelayHost, RelayAccept, RelayAuth, RelayAuthResolver,
    RelayConnection, RelayConnectionHandler, RelayHostOptions, RelayHostStatus, RetryDelays, Sleep,
    encode_relay_data_frame, normalize_radius_gateway_url, parse_host_control_message, parse_relay_data_frame,
    relay_status_line, relay_web_socket_url,
};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use support::gateway::{Gateway, GatewaySocket, HANG_GUARD, hex, unhex};

static FIXTURE: LazyLock<Value> =
    LazyLock::new(|| serde_json::from_str(include_str!("../fixtures/radius-relay.json")).expect("fixture parses"));

const SERVER_ID: &str = "00000000-0000-4000-8000-000000000001";
const CONNECTION_ID: &str = "00000000-0000-4000-8000-000000000002";

/// The scripted credentials; once they run out the lookup blocks until the test ends (as the TypeScript stub's
/// promise stays pending).
struct ScriptedAuth {
    results: Mutex<Vec<Value>>,
    gateway: String,
    exhausted: Mutex<Receiver<()>>,
}

impl RelayAuthResolver for ScriptedAuth {
    fn resolve(&self, _required: bool) -> Result<Option<RelayAuth>, String> {
        let next = {
            let mut results = self.results.lock().unwrap();
            (!results.is_empty()).then(|| results.remove(0))
        };
        let Some(next) = next else {
            let _ = self.exhausted.lock().unwrap().recv();
            return Err("aborted".into());
        };
        if let Some(error) = next.get("error").and_then(Value::as_str) {
            return Err(error.into());
        }
        if next.get("none").is_some() {
            return Ok(None);
        }
        let gateway = match next.get("gateway").and_then(Value::as_str) {
            // The recorded gateway's path, on the fake gateway.
            Some(g) if g.starts_with("http://") || g.starts_with("https://") => {
                let rest = g.split_once("://").unwrap().1;
                let path = rest.find('/').map_or("", |i| &rest[i..]);
                format!("{}{path}", self.gateway)
            }
            Some(other) => other.to_string(),
            None => self.gateway.clone(),
        };
        Ok(Some(RelayAuth {
            gateway,
            token: next["token"].as_str().unwrap().to_string(),
        }))
    }
}

#[derive(Default)]
struct Server {
    connections: Mutex<Vec<RelayConnection>>,
    accepted: Mutex<Vec<usize>>,
    events: Mutex<Vec<Value>>,
    changed: Condvar,
}

struct Handler {
    index: usize,
    server: Arc<Server>,
}

impl Handler {
    fn push(&self, event: Value) {
        self.server.events.lock().unwrap().push(event);
        self.server.changed.notify_all();
    }
}

impl RelayConnectionHandler for Handler {
    fn on_data(&mut self, chunk: &[u8]) {
        self.push(json!({"connection": self.index, "data": hex(chunk)}));
    }
    fn on_close(&mut self) {
        self.push(json!({"connection": self.index, "close": true}));
    }
    fn on_error(&mut self, error: &str) {
        self.push(json!({"connection": self.index, "error": error}));
    }
}

fn accept(server: Arc<Server>, refuse: bool) -> RelayAccept {
    Arc::new(move |connection: RelayConnection| -> Box<dyn RelayConnectionHandler> {
        let index = {
            let mut connections = server.connections.lock().unwrap();
            connections.push(connection.clone());
            connections.len() - 1
        };
        server.accepted.lock().unwrap().push(index);
        server.changed.notify_all();
        if refuse {
            let _ = connection.close(None);
        }
        Box::new(Handler {
            index,
            server: server.clone(),
        })
    })
}

fn connection(server: &Server, index: usize) -> RelayConnection {
    let guard = server.connections.lock().unwrap();
    let (guard, _) = server
        .changed
        .wait_timeout_while(guard, HANG_GUARD, |c| c.len() <= index)
        .unwrap();
    guard[index].clone()
}

fn status_value(status: &RelayHostStatus) -> Value {
    match status {
        RelayHostStatus::Retrying(error) => json!({"status": "retrying", "error": error}),
        other => json!({"status": other.name()}),
    }
}

struct Outcome {
    sockets: Vec<Value>,
    statuses: Vec<Value>,
    lines: Vec<String>,
    delays: Vec<u64>,
    accepted: Vec<usize>,
    events: Vec<Value>,
}

fn run_host(scenario: &Value) -> Outcome {
    let gateway = Gateway::start();
    let (release, exhausted): (Sender<()>, Receiver<()>) = channel();
    let auth = Arc::new(ScriptedAuth {
        results: Mutex::new(scenario["auth"].as_array().unwrap().clone()),
        gateway: gateway.origin(),
        exhausted: Mutex::new(exhausted),
    });
    let server = Arc::new(Server::default());
    let statuses = Arc::new(Mutex::new(Vec::new()));
    let delays = Arc::new(Mutex::new(Vec::new()));
    let recorded_delays = delays.clone();
    let sleep: Sleep = Arc::new(move |ms| {
        recorded_delays.lock().unwrap().push(ms);
        Box::pin(async {})
    });
    let recorded_statuses = statuses.clone();
    let host = RadiusRelayHost::start(RelayHostOptions {
        server_id: SERVER_ID.into(),
        accept: accept(server.clone(), scenario["refuse"] == true),
        auth,
        on_status: Some(Arc::new(move |status: &RelayHostStatus| {
            recorded_statuses.lock().unwrap().push(status.clone())
        })),
        delays: RetryDelays::default(),
        sleep: Some(sleep),
    });
    let mut sockets: Vec<GatewaySocket> = Vec::new();
    for step in scenario["steps"].as_array().unwrap() {
        if let Some(connect) = step.get("connect") {
            sockets.push(gateway.accept(connect.get("fail").is_some()));
            continue;
        }
        let current = sockets.last_mut().expect("a connected socket");
        if let Some(text) = step.get("text").and_then(Value::as_str) {
            current.send(Message::Text(text.into()));
        } else if let Some(binary) = step.get("binary").and_then(Value::as_str) {
            current.send(Message::Binary(unhex(binary)));
        } else if let Some(close) = step.get("close") {
            current.close(close[0].as_u64().unwrap() as u16, close[1].as_str().unwrap());
        } else if step.get("drop").is_some() {
            current.drop_connection();
        } else if let Some(send) = step.get("serverSend") {
            current.sync(false);
            let index = send[0].as_u64().unwrap() as usize;
            if let Err(error) = connection(&server, index).send(&unhex(send[1].as_str().unwrap())) {
                server.events.lock().unwrap().push(json!({"sendError": error}));
            }
        } else if let Some(close) = step.get("serverClose") {
            current.sync(false);
            let index = close[0].as_u64().unwrap() as usize;
            let last = close[1].as_str().map(unhex);
            connection(&server, index).close(last.as_deref()).unwrap();
        } else if step.get("sync").is_some() {
            current.sync(true);
        }
    }
    host.close();
    for socket in &sockets {
        socket.wait_done();
    }
    drop(release);
    let statuses = statuses.lock().unwrap().clone();
    let mut previous = String::new();
    let lines = statuses
        .iter()
        .filter_map(|status| relay_status_line(&mut previous, status))
        .collect();
    Outcome {
        sockets: sockets.iter().map(GatewaySocket::snapshot).collect(),
        statuses: statuses.iter().map(status_value).collect(),
        lines,
        delays: delays.lock().unwrap().clone(),
        accepted: server.accepted.lock().unwrap().clone(),
        events: server.events.lock().unwrap().clone(),
    }
}

fn host_differences(scenario: &Value) -> Vec<String> {
    let name = scenario["name"].as_str().unwrap();
    let rust = run_host(scenario);
    let mut diffs = Vec::new();
    let mut check = |what: &str, expected: &Value, actual: &Value| {
        if expected != actual {
            diffs.push(format!("{name}: {what}\n  expected {expected}\n  actual   {actual}"));
        }
    };
    let expected_sockets: Vec<Value> = scenario["sockets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            json!({"path": s["path"], "protocol": s["protocol"], "authorization": s["authorization"], "sent": s["sent"], "closed": s["closed"]})
        })
        .collect();
    check("sockets", &json!(expected_sockets), &json!(rust.sockets));
    check("statuses", &scenario["statuses"], &json!(rust.statuses));
    check("status lines", &scenario["statusLines"], &json!(rust.lines));
    check("delays", &scenario["delays"], &json!(rust.delays));
    check("accepted", &scenario["accepted"], &json!(rust.accepted));
    check("events", &scenario["events"], &json!(rust.events));
    diffs
}

#[test]
fn server_radius_relay_host_replays_recorded_typescript_scenarios() {
    let mut failures = Vec::new();
    let scenarios: Vec<&Value> = FIXTURE["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["kind"] == "host")
        .collect();
    assert_eq!(scenarios.len(), 25);
    for scenario in scenarios {
        failures.extend(host_differences(scenario));
    }
    assert!(
        failures.is_empty(),
        "{} differences:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

// ------------------------------------------------------------------------------------- ported literal expectations

/// "matches the Radius multiplexing envelope".
#[test]
fn server_radius_relay_data_frame_round_trips() {
    let payload = [0u8, 1, 2, 255];
    let frame = encode_relay_data_frame(CONNECTION_ID, &payload).unwrap();
    assert_eq!(hex(&frame), "010100000000000040008000000000000002000102ff");
    assert_eq!(
        parse_relay_data_frame(&frame),
        Some((CONNECTION_ID.to_string(), payload.to_vec()))
    );
    assert_eq!(
        encode_relay_data_frame("not-a-uuid", &payload).unwrap_err(),
        "Invalid Radius relay connection ID"
    );
    assert_eq!(parse_relay_data_frame(&frame[..17]), None);
    let mut wrong_type = frame.clone();
    wrong_type[1] = 2;
    assert_eq!(parse_relay_data_frame(&wrong_type), None);
    let mut not_v4 = frame.clone();
    not_v4[8] = 0x10;
    assert_eq!(parse_relay_data_frame(&not_v4), None);
}

#[test]
fn server_radius_relay_control_messages_parse_like_typescript() {
    assert_eq!(
        parse_host_control_message(r#"{"version":1,"type":"ping"}"#),
        Ok(HostControl::Ping)
    );
    assert_eq!(
        parse_host_control_message(&format!(
            r#"{{"version":1,"type":"connection_close","connection_id":"{CONNECTION_ID}","code":4999}}"#
        )),
        Ok(HostControl::ConnectionClose(CONNECTION_ID.into(), Some(4999)))
    );
    assert_eq!(
        parse_host_control_message(r#"{"version":1.0,"type":"pong","extra":true}"#),
        Ok(HostControl::Pong)
    );
    assert_eq!(
        parse_host_control_message(r#"{"type":"ping"}"#).unwrap_err(),
        "Unsupported Radius relay control version"
    );
    assert_eq!(
        parse_host_control_message("null").unwrap_err(),
        "Invalid Radius relay control message"
    );
}

/// `relayWebSocketUrl`: https maps to wss, http to ws, the server path replaces any gateway path.
#[test]
fn server_radius_relay_web_socket_url_matches_typescript() {
    assert_eq!(
        relay_web_socket_url("https://radius.example.test/base/", SERVER_ID).unwrap(),
        format!("wss://radius.example.test/v1/session-relays/{SERVER_ID}/connect")
    );
    assert_eq!(
        relay_web_socket_url("http://127.0.0.1:9", SERVER_ID).unwrap(),
        format!("ws://127.0.0.1:9/v1/session-relays/{SERVER_ID}/connect")
    );
    assert_eq!(
        relay_web_socket_url("ftp://radius.example.test", SERVER_ID).unwrap_err(),
        "Unsupported Radius gateway protocol: ftp:"
    );
    assert_eq!(
        normalize_radius_gateway_url("radius.example.test/"),
        "https://radius.example.test"
    );
    assert_eq!(normalize_radius_gateway_url("HTTP://x.test//"), "HTTP://x.test");
}

fn resolver(input: Option<AuthInput>, offline: bool, stored: Option<&str>) -> RadiusRelayAuthResolver {
    let stored = stored.map(str::to_string);
    RadiusRelayAuthResolver::new(
        input,
        "radius.example.test",
        offline,
        Arc::new(move || Ok(stored.clone())),
    )
}

/// `radius-auth.ts`: offline mode, an explicit token (trimmed, never empty) before the stored credential, and the
/// required-credential message.
#[test]
fn server_radius_relay_auth_resolution_matches_typescript() {
    let auth = |token: &str| RelayAuth {
        gateway: "https://radius.example.test".into(),
        token: token.into(),
    };
    assert_eq!(resolver(None, true, Some("s")).resolve(false), Ok(None));
    assert_eq!(
        resolver(None, true, Some("s")).resolve(true).unwrap_err(),
        "Radius relay connections are unavailable in offline mode"
    );
    assert_eq!(
        resolver(Some(AuthInput::Token("  t \n".into())), false, Some("s")).resolve(true),
        Ok(Some(auth("t")))
    );
    assert_eq!(
        resolver(Some(AuthInput::Token(" ".into())), false, Some("s"))
            .resolve(false)
            .unwrap_err(),
        "Radius authentication token must not be empty"
    );
    assert_eq!(resolver(None, false, Some("s")).resolve(false), Ok(Some(auth("s"))));
    assert_eq!(resolver(None, false, Some("")).resolve(false), Ok(None));
    assert_eq!(
        resolver(None, false, None).resolve(true).unwrap_err(),
        "Radius authentication is required; start Pi and run /login radius, then retry"
    );
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("token");
    std::fs::write(&file, "from-file\n").unwrap();
    assert_eq!(
        resolver(Some(AuthInput::File(file)), false, None).resolve(true),
        Ok(Some(auth("from-file")))
    );
    let missing = dir.path().join("missing");
    assert_eq!(
        resolver(Some(AuthInput::File(missing.clone())), false, None)
            .resolve(true)
            .unwrap_err(),
        format!("ENOENT: no such file or directory, open '{}'", missing.display())
    );
}

/// The server command's `Radius: …` lines: `connecting` is never printed, a repeated description once.
#[test]
fn server_radius_relay_status_lines_print_once_per_change() {
    let mut previous = String::new();
    let lines: Vec<Option<String>> = [
        RelayHostStatus::Connecting,
        RelayHostStatus::NotAuthenticated,
        RelayHostStatus::NotAuthenticated,
        RelayHostStatus::Connected,
        RelayHostStatus::Retrying("x".into()),
        RelayHostStatus::Retrying("x".into()),
        RelayHostStatus::Retrying("y".into()),
    ]
    .iter()
    .map(|s| relay_status_line(&mut previous, s))
    .collect();
    assert_eq!(
        lines,
        vec![
            None,
            Some("Radius: not connected; local only".into()),
            None,
            Some("Radius: connected".into()),
            Some("Radius: reconnecting: x".into()),
            None,
            Some("Radius: reconnecting: y".into()),
        ]
    );
}

/// undici checks the subprotocol during the handshake: a gateway that selects another one is a failed connection.
#[test]
fn server_radius_relay_rejects_a_different_subprotocol() {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).unwrap();
            head.push(byte[0]);
        }
        let head = String::from_utf8(head).unwrap();
        let key = head
            .split("\r\n")
            .find_map(|l| l.strip_prefix("sec-websocket-key: "))
            .unwrap()
            .to_string();
        let response = format!(
            "HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\nconnection: Upgrade\r\nsec-websocket-accept: {}\r\nsec-websocket-protocol: other\r\n\r\n",
            protocol::websocket::accept_key(&key)
        );
        stream.write_all(response.as_bytes()).unwrap();
    });
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let result = runtime.block_on(server::radius_ws::connect(
        &format!("ws://127.0.0.1:{port}/v1/x"),
        "pi-session-relay.host.v1",
        "Bearer t",
        1024,
    ));
    assert_eq!(result.err().as_deref(), Some("Radius WebSocket connection failed"));
    server.join().unwrap();
}
