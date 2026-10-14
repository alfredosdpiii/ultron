//! A server's death against the calls and the tool-list refresh, each order forced, replaying
//! crates/mcp/fixtures/ordering.json (generate-ordering.mjs: the TypeScript gateway over the same scripted transport):
//! - detected: the close is seen before the next call, which reconnects and reports the failed restart;
//! - undetected: the server is dead but its close not yet seen, so a call reads the cached tool list
//!   (`tool_not_found` for a tool the server never had) until the close arrives;
//! - refresh: the server dies while answering the refresh `tools/list_changed` started; the close is seen first
//!   (close listeners run before waiting requests wake), so the refresh failure is the error the status keeps.

use mcp::config::{Scope, ServerEntry};
use mcp::connection::ConnectionEnv;
use mcp::gateway::{Gateway, GatewayOptions};
use mcp::{Error, Incoming, Sink, Transport};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Default)]
struct World {
    restart_fails: bool,
    die_on_list: bool,
    live: Option<Arc<Scripted>>,
}

/// A fake server behind a transport: answers initialize and tools/list; `die` is the close the client sees.
struct Scripted {
    world: Arc<Mutex<World>>,
    sink: Mutex<Option<Sink>>,
    closed: Mutex<bool>,
}

impl Scripted {
    fn die(&self) {
        let sink = {
            let mut closed = self.closed.lock().unwrap();
            if *closed {
                return;
            }
            *closed = true;
            self.sink.lock().unwrap().clone()
        };
        if let Some(sink) = sink {
            sink(Incoming::Closed);
        }
    }

    fn emit(&self, message: Value) {
        if let Some(sink) = self.sink.lock().unwrap().clone() {
            sink(Incoming::Message(message));
        }
    }
}

struct Handle(Arc<Scripted>);

impl Transport for Handle {
    fn start(&self, sink: Sink) -> Result<(), Error> {
        let mut world = self.0.world.lock().unwrap();
        if world.restart_fails {
            return Err(Error::Other("spawn gw ENOENT".into()));
        }
        *self.0.sink.lock().unwrap() = Some(sink);
        world.live = Some(self.0.clone());
        Ok(())
    }

    fn send(&self, message: &Value) -> Result<(), Error> {
        let Some(id) = message.get("id").cloned() else {
            return Ok(());
        };
        let method = message["method"].as_str().unwrap_or_default();
        let server = self.0.clone();
        if method == "tools/list" {
            let mut world = self.0.world.lock().unwrap();
            if world.die_on_list {
                world.die_on_list = false;
                std::thread::spawn(move || server.die());
                return Ok(());
            }
        }
        let result = match method {
            "initialize" => json!({
                "protocolVersion": message["params"]["protocolVersion"],
                "capabilities": { "tools": { "listChanged": true } },
                "serverInfo": { "name": "gw", "version": "1" },
            }),
            "tools/list" => json!({ "tools": [{ "name": "ping", "inputSchema": { "type": "object" } }] }),
            _ => json!({}),
        };
        // Answered from another thread, as a reader thread would.
        std::thread::spawn(move || server.emit(json!({ "jsonrpc": "2.0", "id": id, "result": result })));
        Ok(())
    }

    fn close(&self) {
        self.0.die();
    }
}

fn fixture() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/ordering.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn wait_until(what: &str, ready: impl Fn() -> bool) {
    let start = Instant::now();
    while !ready() {
        assert!(
            start.elapsed() < Duration::from_secs(120),
            "timed out waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn run(name: &str) {
    let fixture = fixture();
    let recorded = fixture["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == name)
        .unwrap()
        .clone();
    let world: Arc<Mutex<World>> = Arc::default();
    let factory_world = world.clone();
    let dir = tempfile::tempdir().unwrap();
    let gateway = Gateway::new(GatewayOptions {
        agent_dir: dir.path().to_path_buf(),
        servers: Some(vec![ServerEntry {
            name: "srv".into(),
            config: json!({ "command": "gw" }).as_object().unwrap().clone(),
            source: "test".into(),
            scope: Scope::Global,
        }]),
        env: ConnectionEnv {
            cwd: dir.path().to_path_buf(),
            transport: Some(Arc::new(move |_: &ServerEntry| {
                let scripted = Arc::new(Scripted {
                    world: factory_world.clone(),
                    sink: Mutex::new(None),
                    closed: Mutex::new(false),
                });
                Ok(Arc::new(Handle(scripted)) as Arc<dyn Transport>)
            })),
            ..ConnectionEnv::default()
        },
        ..GatewayOptions::default()
    });
    let call = |params: &Value| -> Value {
        let result = gateway.execute(params.as_object().unwrap(), None).unwrap();
        json!({ "text": Gateway::text_of(&result), "details": result["details"] })
    };
    let live = || world.lock().unwrap().live.clone().unwrap();
    for (index, expected) in recorded["events"].as_array().unwrap().iter().enumerate() {
        let mut got = json!({ "op": expected["op"] });
        match expected["op"].as_str().unwrap() {
            "call" => {
                got["params"] = expected["params"].clone();
                let result = call(&expected["params"]);
                got["text"] = result["text"].clone();
                got["details"] = result["details"].clone();
            }
            "restartFails" => world.lock().unwrap().restart_fails = true,
            // The close is delivered and handled on this thread before the next step.
            "die" => live().die(),
            "dieUnseen" => {}
            "changeAndDieOnRefresh" => {
                world.lock().unwrap().die_on_list = true;
                live().emit(json!({ "jsonrpc": "2.0", "method": "notifications/tools/list_changed" }));
                // Settled once the refresh's failure is recorded (whatever order would end elsewhere and time out).
                wait_until("the refresh to fail", || {
                    gateway.status()["details"]["servers"][0]["error"]
                        .as_str()
                        .is_some_and(|e| e.starts_with("Failed to refresh tools"))
                });
            }
            other => panic!("unknown op {other}"),
        }
        assert_eq!(&got, expected, "{name} event {index}");
    }
    gateway.close();
}

#[test]
fn mcp_ordering_a_death_seen_before_the_call_reports_the_failed_restart() {
    run("detected");
}

#[test]
fn mcp_ordering_a_death_not_yet_seen_reads_the_cached_tool_list() {
    run("undetected");
}

#[test]
fn mcp_ordering_a_death_during_the_tool_refresh_keeps_the_refresh_failure() {
    run("refresh");
}
