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
