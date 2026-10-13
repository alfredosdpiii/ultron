//! Server-to-client requests over an in-memory transport (client.ts `handleRequest`).

use mcp::{CancelToken, Client, ClientOptions, Error, Incoming, Sink, Transport};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

/// Answers `initialize` and records everything else the client sends.
#[derive(Default)]
struct Fake {
    sink: Mutex<Option<Sink>>,
    sent: Mutex<Vec<Value>>,
}

impl Transport for Fake {
    fn start(&self, sink: Sink) -> Result<(), Error> {
        *self.sink.lock().unwrap() = Some(sink);
        Ok(())
    }

    fn send(&self, message: &Value) -> Result<(), Error> {
        if message["method"] == "initialize" {
            let result = json!({ "protocolVersion": "2025-11-25", "capabilities": {}, "serverInfo": { "name": "s", "version": "1" } });
            let sink = self.sink.lock().unwrap().clone().unwrap();
            let id = message["id"].clone();
            std::thread::spawn(move || {
                sink(Incoming::Message(
                    json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                ))
            });
        } else {
            self.sent.lock().unwrap().push(message.clone());
        }
        Ok(())
    }

    fn close(&self) {}
}

fn ask(fake: &Fake, request: Value, expected_id: &Value) -> Value {
    let sink = fake.sink.lock().unwrap().clone().unwrap();
    sink(Incoming::Message(request));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while std::time::Instant::now() < deadline {
        if let Some(reply) = fake.sent.lock().unwrap().iter().find(|m| &m["id"] == expected_id) {
            return reply.clone();
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("no reply to {expected_id}");
}

#[test]
fn mcp_client_answers_server_requests_with_handler_results_and_errors() {
    let fake = Arc::new(Fake::default());
    let client = Client::new(ClientOptions {
        name: "c".into(),
        version: "1".into(),
        ..ClientOptions::default()
    });
    client.connect(fake.clone()).unwrap();
    client.set_request_handler("x/null", Arc::new(|_, _: &CancelToken| Ok(Value::Null)));
    client.set_request_handler(
        "x/echo",
        Arc::new(|p: Option<&Value>, _: &CancelToken| Ok(json!({ "got": p }))),
    );
    client.set_request_handler(
        "x/mcp",
        Arc::new(|_, _: &CancelToken| {
            Err(Error::Mcp {
                code: 7,
                message: "nope".into(),
                data: Some(json!([1])),
            })
        }),
    );
    client.set_request_handler(
        "x/other",
        Arc::new(|_, _: &CancelToken| Err(Error::Other("boom".into()))),
    );
    let cases = [
        ("x/null", json!(1), json!({ "jsonrpc": "2.0", "id": 1, "result": {} })),
        (
            "x/echo",
            json!("e"),
            json!({ "jsonrpc": "2.0", "id": "e", "result": { "got": null } }),
        ),
        (
            "x/mcp",
            json!(2),
            json!({ "jsonrpc": "2.0", "id": 2, "error": { "code": 7, "message": "nope", "data": [1] } }),
        ),
        (
            "x/other",
            json!(3),
            json!({ "jsonrpc": "2.0", "id": 3, "error": { "code": -32603, "message": "boom" } }),
        ),
        (
            "roots/list",
            json!(4),
            json!({ "jsonrpc": "2.0", "id": 4, "error": { "code": -32601, "message": "Method not found: roots/list" } }),
        ),
    ];
    for (method, id, expected) in cases {
        let reply = ask(&fake, json!({ "jsonrpc": "2.0", "id": id, "method": method }), &id);
        assert_eq!(
            serde_json::to_string(&reply).unwrap(),
            serde_json::to_string(&expected).unwrap()
        );
    }
    client.close();
}
