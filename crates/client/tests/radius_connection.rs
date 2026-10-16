//! A client connection over the Radius relay (`Client.connect` with `createRadiusClientTransportFactory`), against
//! a fake gateway on 127.0.0.1: the handshake runs over the relay's binary messages, the connection's own
//! disconnect closes the relay (`1000 Pi client closed`) while the connection is still held, and the gateway
//! closing the relay ends the connection.

#[path = "../../server/tests/support/mod.rs"]
mod support;

use client::runtime::Connection;
use protocol::websocket::Message;
use protocol::{DEFAULT_MAX_FRAME_LENGTH, PROTOCOL_VERSION, ServerMessage, encode_server_message};
use server::radius_relay::{RelayAuth, RelayAuthResolver};
use support::gateway::{Gateway, HANG_GUARD};

const SERVER_ID: &str = "00000000-0000-4000-8000-000000000001";

struct FixedAuth(String);

impl RelayAuthResolver for FixedAuth {
    fn resolve(&self, _required: bool) -> Result<Option<RelayAuth>, String> {
        Ok(Some(RelayAuth {
            gateway: self.0.clone(),
            token: "secret".into(),
        }))
    }
}

fn hello() -> Vec<u8> {
    encode_server_message(
        &ServerMessage::Hello {
            version: PROTOCOL_VERSION,
            server_id: SERVER_ID.into(),
        },
        DEFAULT_MAX_FRAME_LENGTH,
    )
    .unwrap()
}

fn connect(gateway: &Gateway) -> (std::sync::Arc<Connection>, support::gateway::GatewaySocket) {
    let auth = FixedAuth(gateway.origin());
    std::thread::scope(|scope| {
        let connecting = scope.spawn(|| Connection::connect_radius(SERVER_ID, &auth));
        let mut socket = gateway.accept(false);
        socket.send(Message::Binary(hello()));
        (connecting.join().unwrap().expect("handshaken over the relay"), socket)
    })
}

fn wait_closed(connection: &Connection) {
    let deadline = std::time::Instant::now() + HANG_GUARD;
    while !connection.is_closed() {
        assert!(std::time::Instant::now() < deadline, "the connection never closed");
        std::thread::yield_now();
    }
}

#[test]
fn client_radius_connection_disconnects_its_relay_while_held() {
    let gateway = Gateway::start();
    let (connection, socket) = connect(&gateway);
    let snapshot = socket.snapshot();
    assert_eq!(snapshot["protocol"], "pi-session-relay.client.v1");
    assert_eq!(snapshot["authorization"], "Bearer secret");
    assert_eq!(snapshot["path"], format!("/v1/session-relays/{SERVER_ID}/connect"));
    // The client's hello went out as one binary message.
    assert_eq!(snapshot["sent"].as_array().unwrap().len(), 1);
    assert!(!connection.is_closed());
    connection.disconnect();
    socket.wait_done();
    assert_eq!(
        socket.snapshot()["closed"],
        serde_json::json!({"code": 1000, "reason": "Pi client closed"})
    );
    wait_closed(&connection);
}

#[test]
fn client_radius_connection_ends_when_the_gateway_closes() {
    let gateway = Gateway::start();
    let (connection, mut socket) = connect(&gateway);
    socket.close(1001, "going away");
    wait_closed(&connection);
    assert!(
        connection
            .call(connection.server_target(), "x", "y", serde_json::json!([]))
            .is_err()
    );
}

#[test]
fn client_radius_connection_rejects_a_bad_server_id_before_connecting() {
    let gateway = Gateway::start();
    let error = Connection::connect_radius("not-an-id", &FixedAuth(gateway.origin()))
        .err()
        .unwrap();
    assert_eq!(error.to_string(), "serverId must be a canonical lowercase UUIDv4");
}

/// `RadiusClientReconnect`'s delays as a one-shot client reopens its relay: recorded, not waited out; `stop_at`
/// (1-based) answers `false` on that call.
fn recorded_sleep(
    delays: std::sync::Arc<std::sync::Mutex<Vec<u64>>>,
    stop_at: Option<usize>,
) -> client::radius::ReconnectSleep {
    std::sync::Arc::new(move |ms, _cancel: &client::radius::Cancellation| {
        let mut delays = delays.lock().unwrap();
        delays.push(ms);
        stop_at != Some(delays.len())
    })
}

fn sent_call(socket: &support::gateway::GatewaySocket, index: usize) -> serde_json::Value {
    let sent = socket.snapshot()["sent"][index]["binary"].as_str().unwrap().to_string();
    let mut decoder = protocol::ClientMessageDecoder::default();
    match decoder.push(&support::gateway::unhex(&sent)).unwrap().remove(0) {
        protocol::ClientMessage::Request { id, call, .. } => {
            serde_json::json!({"id": id, "call": client::runtime::to_json(&call)})
        }
        other => panic!("not a request: {other:?}"),
    }
}

#[test]
fn client_radius_reopen_retries_with_backoff_and_reattaches_the_session() {
    let gateway = Gateway::start();
    let delays = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut reopen = client::radius::RadiusReopen::new(SERVER_ID, std::sync::Arc::new(FixedAuth(gateway.origin())));
    reopen.sleep = Some(recorded_sleep(delays.clone(), None));
    let (connection, socket) = std::thread::scope(|scope| {
        let reopening = scope.spawn(|| reopen.reopen("sess-1"));
        // The first attempt is refused; the second connects and re-attaches the Session.
        gateway.accept(true);
        let mut socket = gateway.accept(false);
        socket.send(Message::Binary(hello()));
        let attached = protocol::SessionTarget {
            server_id: SERVER_ID.into(),
            session_id: "sess-1".into(),
            attachment_id: "attachment-1".into(),
        };
        for message in [
            ServerMessage::Attachment {
                attachment: Some(attached),
            },
            ServerMessage::Response {
                id: "request-1".into(),
                result: Ok(None),
            },
        ] {
            socket.send(Message::Binary(
                encode_server_message(&message, DEFAULT_MAX_FRAME_LENGTH).unwrap(),
            ));
        }
        (reopening.join().unwrap().expect("reopened"), socket)
    });
    // 1 s before the first attempt, doubled after its failure (`CLIENT_RETRY_INITIAL_MS`, `retryMs * 2`).
    assert_eq!(delays.lock().unwrap().clone(), vec![1_000, 2_000]);
    assert_eq!(connection.attachment().map(|a| a.session_id), Some("sess-1".into()));
    assert_eq!(
        sent_call(&socket, 1),
        serde_json::json!({"id": "request-1", "call": {"serviceId": "pi.session-management", "member": "attach", "args": ["sess-1"]}})
    );
    assert!(!connection.is_closed());
    connection.disconnect();
    socket.wait_done();
}

#[test]
fn client_radius_reopen_gives_up_with_the_last_error() {
    let gateway = Gateway::start();
    let delays = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut reopen = client::radius::RadiusReopen::new(SERVER_ID, std::sync::Arc::new(FixedAuth(gateway.origin())));
    // Stopped before the third attempt: the second attempt's error is the answer.
    reopen.sleep = Some(recorded_sleep(delays.clone(), Some(3)));
    let error = std::thread::scope(|scope| {
        let reopening = scope.spawn(|| reopen.reopen("sess-1"));
        gateway.accept(true);
        gateway.accept(true);
        reopening.join().unwrap().err().unwrap()
    });
    assert_eq!(error, "Radius WebSocket connection failed");
    assert_eq!(delays.lock().unwrap().clone(), vec![1_000, 2_000, 4_000]);
    // Past its deadline it stops before any attempt (an attempt would be refused at once: nothing listens on
    // port 1, and the sleep stops it at its second delay).
    let expired_delays = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut expired =
        client::radius::RadiusReopen::new(SERVER_ID, std::sync::Arc::new(FixedAuth("http://127.0.0.1:1".into())));
    expired.timeout = std::time::Duration::ZERO;
    expired.sleep = Some(recorded_sleep(expired_delays.clone(), Some(2)));
    assert_eq!(expired.reopen("sess-1").err().unwrap(), "Radius reconnect stopped");
    assert_eq!(expired_delays.lock().unwrap().clone(), Vec::<u64>::new());
    assert_eq!(client::radius::REOPEN_TIMEOUT, std::time::Duration::from_secs(120));
}
