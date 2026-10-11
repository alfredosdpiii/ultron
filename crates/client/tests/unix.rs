//! Ported from packages/server/test/unix.test.ts, packages/client/test/unix.test.ts and
//! unix-transport.test.ts: a Rust `UnixServer` and `Client` over real sockets in a private temp server dir.

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::time::Duration;

use client::{Client, ClientError, UnixServerRoute, discover_unix_servers};
use protocol::{ProtocolError, RpcTarget, ServiceCall, Value};
use server::{HostError, ServerConfig, ServerServices, UnixServer, unix_socket_path};

const A: &str = "00000000-0000-4000-8000-00000000000a";
const B: &str = "00000000-0000-4000-8000-00000000000b";
const C: &str = "00000000-0000-4000-8000-00000000000c";

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

fn start(dir: &Path, server_id: &str) -> UnixServer {
    let path = unix_socket_path(server_id, dir).expect("path");
    let config = ServerConfig::new(server_id, 1 << 24).expect("config");
    UnixServer::start(&path, config, Duration::from_millis(200), || Echo).expect("start")
}

fn call(service: &str, args: Vec<Value>) -> Value {
    Value::map([
        ("serviceId", Value::text(service)),
        ("member", Value::text("invoke")),
        ("args", Value::Array(args)),
    ])
}

#[test]
fn client_unix_socket_path_is_derived_from_the_server_id() {
    let dir = Path::new("/srv/ultron");
    assert_eq!(unix_socket_path(A, dir), Ok(dir.join(format!("{A}.sock"))));
    assert_eq!(
        unix_socket_path("server-1", dir),
        Err("Unix serverId must be a canonical lowercase UUIDv4".into())
    );
}

#[test]
fn client_carries_handshake_and_requests_over_a_real_unix_socket() {
    let temp = tempfile::tempdir().expect("temp");
    let dir = temp.path().join("nested/servers");
    let server = start(&dir, A);
    let socket = dir.join(format!("{A}.sock"));
    assert_eq!(
        std::fs::metadata(&dir).expect("dir").permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::symlink_metadata(&socket).expect("socket").permissions().mode() & 0o777,
        0o600
    );

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
    server.close().expect("close");
    assert!(!socket.exists(), "the server removes its own socket");
}

#[test]
fn client_server_handshake_timeout_sends_a_final_hello_error() {
    let temp = tempfile::tempdir().expect("temp");
    let server = start(temp.path(), A);
    let mut stream = UnixStream::connect(server.path()).expect("connect");
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).expect("server closes");
    let mut decoder = protocol::ServerMessageDecoder::default();
    let messages = decoder.push(&bytes).expect("frames");
    assert_eq!(
        messages,
        vec![protocol::ServerMessage::HelloError {
            error: ProtocolError::new("invalid_request", "Handshake timeout")
        }]
    );
}

#[test]
fn client_server_refuses_live_listeners_and_regular_files_but_replaces_stale_sockets() {
    let temp = tempfile::tempdir().expect("temp");
    let live = start(temp.path(), A);
    let path = live.path().to_owned();
    let config = ServerConfig::new(A, 1 << 24).expect("config");
    let error = UnixServer::start(&path, config.clone(), Duration::from_secs(1), || Echo)
        .err()
        .expect("live");
    assert_eq!(
        error.to_string(),
        format!("Unix listener is already running: {}", path.display())
    );
    assert!(path.exists(), "a live listener's socket is never unlinked");
    drop(live);

    let file = temp.path().join(format!("{B}.sock"));
    std::fs::write(&file, "keep").expect("file");
    let config_b = ServerConfig::new(B, 1 << 24).expect("config");
    let error = UnixServer::start(&file, config_b, Duration::from_secs(1), || Echo)
        .err()
        .expect("file");
    assert_eq!(
        error.to_string(),
        format!("Refusing to remove non-socket Unix listener path: {}", file.display())
    );
    assert_eq!(std::fs::read_to_string(&file).expect("still there"), "keep");

    drop(UnixListener::bind(&path).expect("stale bind"));
    assert!(path.exists(), "a dropped listener leaves a stale socket");
    let server = UnixServer::start(&path, config, Duration::from_secs(1), || Echo).expect("replaces stale socket");
    Client::connect(UnixStream::connect(&path).expect("connect"), A).expect("handshake");
    server.close().expect("close");
}

#[test]
fn client_server_close_keeps_a_replacement_inode() {
    let temp = tempfile::tempdir().expect("temp");
    let server = start(temp.path(), A);
    let path = server.path().to_owned();
    std::fs::remove_file(&path).expect("unlink");
    std::fs::write(&path, "replacement").expect("replace");
    server.close().expect("close");
    assert_eq!(std::fs::read_to_string(&path).expect("kept"), "replacement");
}

#[test]
fn client_discovery_returns_reachable_servers_in_id_order_and_never_deletes() {
    let temp = tempfile::tempdir().expect("temp");
    let dir = temp.path();
    assert_eq!(
        discover_unix_servers(&dir.join("missing"), Duration::from_secs(1)).expect("missing"),
        vec![]
    );

    let b = start(dir, B);
    let a = start(dir, A);
    std::fs::write(dir.join(format!("{C}.sock")), "not a socket").expect("file");
    std::fs::write(dir.join("readme.txt"), "x").expect("file");
    // A socket named for one server that answers as another is not that server.
    let impostor = unix_socket_path("00000000-0000-4000-8000-0000000000ee", dir).expect("path");
    let config = ServerConfig::new(A, 1 << 24).expect("config");
    let _impostor = UnixServer::start(&impostor, config, Duration::from_secs(1), || Echo).expect("start");
    let stale = unix_socket_path("00000000-0000-4000-8000-0000000000ff", dir).expect("path");
    drop(UnixListener::bind(&stale).expect("stale"));
    // Accepts connections (backlog) but never answers: times out.
    let silent_path = unix_socket_path("00000000-0000-4000-8000-0000000000dd", dir).expect("path");
    let _silent = UnixListener::bind(&silent_path).expect("silent");

    let routes = discover_unix_servers(dir, Duration::from_millis(150)).expect("discover");
    let expected = [A, B].map(|id| UnixServerRoute {
        server_id: id.into(),
        path: dir.join(format!("{id}.sock")),
    });
    assert_eq!(routes, expected.to_vec());
    assert!(
        stale.exists() && silent_path.exists(),
        "discovery never deletes endpoints"
    );
    drop((a, b));
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
    let path = unix_socket_path(A, temp.path()).expect("path");
    let listener = UnixListener::bind(&path).expect("bind");
    let peer = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut hello = [0u8; 64];
        let _ = stream.read(&mut hello).expect("client hello");
        let error = ProtocolError::new("internal_error", "Internal server error");
        let reply = protocol::encode_server_message(&protocol::ServerMessage::HelloError { error }, 1 << 24);
        stream.write_all(&reply.expect("frame")).expect("write");
    });
    let error = discover_unix_servers(temp.path(), Duration::from_secs(1)).expect_err("not a stale endpoint");
    assert_eq!(error.to_string(), "internal_error: Internal server error");
    peer.join().expect("peer");
}
