//! Ported from packages/client/test/unix.test.ts and unix-transport.test.ts: the `Client` over real sockets in a
//! private temp server dir, against a test peer that runs the server's `Connection` state machine.

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use client::{Client, ClientError, UnixServerRoute, discover_unix_servers};
use protocol::{ProtocolError, RpcTarget, ServiceCall, Value};
use server::{Connection, HostError, Output, ServerConfig, ServerServices};

const A: &str = "00000000-0000-4000-8000-00000000000a";
const B: &str = "00000000-0000-4000-8000-00000000000b";
const C: &str = "00000000-0000-4000-8000-00000000000c";

/// Handshake and probe timeouts that must not fire while the peer answers: a hang guard, so a loaded machine slows
/// these tests down instead of timing a live peer out.
const PATIENT: Duration = Duration::from_secs(120);

struct Echo;

impl ServerServices for Echo {
    fn invoke(&mut self, call: &ServiceCall) -> Result<Option<Value>, HostError> {
        match call.service_id.as_str() {
            "test.echo" => Ok(Some(Value::Array(call.args.clone()))),
            _ => Err(HostError::Coded(ProtocolError::new(
                "service_not_found",
                "no such service",
            ))),
        }
    }
}

fn socket(dir: &Path, server_id: &str) -> PathBuf {
    dir.join(format!("{server_id}.sock"))
}

/// Serve `listen_as`'s protocol at `path` on a background thread: one `Connection` per accepted stream. The thread
/// lives until the test process exits, like a server the test never stops.
fn serve(path: &Path, listen_as: &str) {
    let listener = UnixListener::bind(path).expect("bind");
    let config = ServerConfig::new(listen_as, 1 << 24).expect("config");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let connection = Connection::new(config.clone(), Echo);
            std::thread::spawn(move || drive(stream, connection));
        }
    });
}

fn drive(mut stream: UnixStream, mut connection: Connection<Echo>) {
    let mut buffer = vec![0u8; 64 * 1024];
    while !connection.is_closed() {
        let outputs = match stream.read(&mut buffer) {
            Ok(0) | Err(_) => {
                connection.transport_closed();
                return;
            }
            Ok(n) => connection.receive(&buffer[..n]),
        };
        for output in outputs {
            match output {
                Output::Send(frame) => {
                    if stream.write_all(&frame).is_err() {
                        connection.transport_closed();
                    }
                }
                Output::Close(frame) => {
                    if let Some(frame) = frame {
                        let _ = stream.write_all(&frame);
                    }
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                }
            }
        }
    }
}

fn start(dir: &Path, server_id: &str) -> PathBuf {
    let path = socket(dir, server_id);
    serve(&path, server_id);
    path
}

fn call(service: &str, args: Vec<Value>) -> Value {
    Value::map([
        ("serviceId", Value::text(service)),
        ("member", Value::text("invoke")),
        ("args", Value::Array(args)),
    ])
}

#[test]
fn client_carries_handshake_and_requests_over_a_real_unix_socket() {
    let temp = tempfile::tempdir().expect("temp");
    let socket = start(temp.path(), A);

    let mut client = Client::connect(UnixStream::connect(&socket).expect("connect"), A).expect("handshake");
    let echoed = client.request(
        RpcTarget::Server { server_id: A.into() },
        &call("test.echo", vec![Value::Int(7)]),
    );
    assert_eq!(echoed.expect("echo"), Some(Value::Array(vec![Value::Int(7)])));
    let missing = client.request(RpcTarget::Server { server_id: A.into() }, &call("test.missing", vec![]));
    assert!(
        matches!(missing, Err(ClientError::Server(e)) if e.code == "service_not_found" && e.message == "no such service")
    );
    let wrong = client.request(RpcTarget::Server { server_id: B.into() }, &call("test.echo", vec![]));
    assert!(matches!(wrong, Err(ClientError::Server(e)) if e.code == "wrong_server"));

    assert!(
        matches!(Client::connect(UnixStream::connect(&socket).expect("connect"), B), Err(ClientError::Validation(m)) if m.contains("does not match"))
    );
}

#[test]
fn client_discovery_returns_reachable_servers_in_id_order_and_never_deletes() {
    let temp = tempfile::tempdir().expect("temp");
    let dir = temp.path();
    assert_eq!(
        discover_unix_servers(&dir.join("missing"), Duration::from_secs(1)).expect("missing"),
        vec![]
    );

    start(dir, B);
    start(dir, A);
    std::fs::write(dir.join(format!("{C}.sock")), "not a socket").expect("file");
    std::fs::write(dir.join("readme.txt"), "x").expect("file");
    // A socket named for one server that answers as another is not that server.
    serve(&socket(dir, "00000000-0000-4000-8000-0000000000ee"), A);
    let stale = socket(dir, "00000000-0000-4000-8000-0000000000ff");
    drop(UnixListener::bind(&stale).expect("stale"));

    let routes = discover_unix_servers(dir, PATIENT).expect("discover");
    let expected = [A, B].map(|id| UnixServerRoute {
        server_id: id.into(),
        path: dir.join(format!("{id}.sock")),
    });
    assert_eq!(routes, expected.to_vec());
    assert!(stale.exists(), "discovery never deletes endpoints");
}

#[test]
fn client_discovery_omits_a_silent_endpoint_after_the_probe_timeout() {
    // Alone in its directory, so the short probe timeout (the input) can only ever drop this endpoint.
    let temp = tempfile::tempdir().expect("temp");
    // Accepts connections (backlog) but never answers: times out.
    let silent_path = socket(temp.path(), "00000000-0000-4000-8000-0000000000dd");
    let _silent = UnixListener::bind(&silent_path).expect("silent");
    let routes = discover_unix_servers(temp.path(), Duration::from_millis(150)).expect("discover");
    assert_eq!(routes, vec![]);
    assert!(silent_path.exists(), "discovery never deletes endpoints");
}

#[test]
fn client_sees_truncated_final_frames_as_protocol_errors() {
    let temp = tempfile::tempdir().expect("temp");
    let path = temp.path().join("raw.sock");
    let listener = UnixListener::bind(&path).expect("bind");
    let peer = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut hello = [0u8; 64];
        let _ = stream.read(&mut hello).expect("client hello");
        let reply = protocol::encode_server_message(
            &protocol::ServerMessage::Hello {
                version: 8,
                server_id: A.into(),
            },
            1 << 24,
        )
        .expect("hello");
        stream.write_all(&reply).expect("write");
        let mut request = [0u8; 256];
        let _ = stream.read(&mut request).expect("client request");
        stream.write_all(&[0, 0, 0, 2, 1]).expect("partial");
    });
    let mut client = Client::connect(UnixStream::connect(&path).expect("connect"), A).expect("handshake");
    let result = client.request(RpcTarget::Server { server_id: A.into() }, &call("test.echo", vec![]));
    peer.join().expect("peer");
    assert!(
        matches!(result, Err(ClientError::Validation(m)) if m == "Invalid server protocol framing: Truncated frame at end of stream")
    );
}

#[test]
fn client_discovery_propagates_non_version_handshake_errors() {
    let temp = tempfile::tempdir().expect("temp");
    let path = socket(temp.path(), A);
    let listener = UnixListener::bind(&path).expect("bind");
    let peer = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut hello = [0u8; 64];
        let _ = stream.read(&mut hello).expect("client hello");
        let error = ProtocolError::new("internal_error", "Internal server error");
        let reply = protocol::encode_server_message(&protocol::ServerMessage::HelloError { error }, 1 << 24);
        stream.write_all(&reply.expect("frame")).expect("write");
    });
    let error = discover_unix_servers(temp.path(), PATIENT).expect_err("not a stale endpoint");
    assert_eq!(error.to_string(), "internal_error: Internal server error");
    peer.join().expect("peer");
}
