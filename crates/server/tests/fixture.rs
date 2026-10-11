//! Replays `fixtures/server.json`, recorded by driving the TypeScript `Server` with scripted client bytes,
//! against `Connection`: every sent frame, close (with its final frame) and reported error must match.

use protocol::{DEFAULT_MAX_FRAME_LENGTH, ProtocolError, Value};
use serde_json::Value as Json;
use server::{Connection, HostError, Output, ServerConfig, ServerServices, ServiceCall};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex"))
        .collect()
}

/// The same service table as the fixture generator's TS host.
struct TestServices;

impl ServerServices for TestServices {
    fn invoke(&mut self, call: &ServiceCall) -> Result<Option<Value>, HostError> {
        match call.service_id.as_str() {
            "test.echo" => Ok(Some(Value::map([
                ("echo", Value::Array(call.args.clone())),
                ("member", Value::text(call.member.as_str())),
            ]))),
            "test.void" => Ok(None),
            "test.fail" => Err(HostError::Coded(ProtocolError::new(
                "session_not_found",
                call.args.first().and_then(Value::as_str).unwrap_or("nope"),
            ))),
            "test.big" => {
                let Some(Value::Int(n)) = call.args.first() else {
                    unreachable!("fixture passes a count")
                };
                Ok(Some(Value::text("x".repeat(*n as usize))))
            }
            other => Err(HostError::Internal(format!(
                "Unsupported test server service {other}.{}",
                call.member
            ))),
        }
    }
}

fn event(output: &Output) -> Json {
    match output {
        Output::Send(frame) => serde_json::json!({ "send": hex(frame) }),
        Output::Close(frame) => serde_json::json!({ "close": frame.as_deref().map(hex) }),
    }
}

#[test]
fn server_connection_scripts_match_typescript() {
    let fixture: Json = serde_json::from_str(include_str!("../fixtures/server.json")).expect("fixture");
    let server_id = fixture["serverId"].as_str().expect("server id");
    let scripts = fixture["scripts"].as_array().expect("scripts");
    assert!(scripts.len() >= 20);
    for script in scripts {
        let max = script["max"].as_u64().map_or(DEFAULT_MAX_FRAME_LENGTH, |m| m as usize);
        let mut connection = Connection::new(ServerConfig::new(server_id, max).expect("config"), TestServices);
        for result in script["results"].as_array().expect("results") {
            connection.reported.clear();
            let outputs = match result["op"].as_str().expect("op") {
                "close" => {
                    connection.transport_closed();
                    Vec::new()
                }
                "timeout" => connection.handshake_timeout(),
                bytes => connection.receive(&unhex(bytes)),
            };
            let events: Vec<Json> = outputs.iter().map(event).collect();
            assert_eq!(
                Json::Array(events),
                result["events"],
                "{}: {}",
                script["name"],
                result["op"]
            );
            let reported: Vec<Json> = connection.reported.iter().map(|m| Json::String(m.clone())).collect();
            assert_eq!(Json::Array(reported), result["reported"], "{}", script["name"]);
        }
    }
}
