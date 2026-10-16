//! The Radius client transport and reconnector against the client and reconnect scenarios of
//! `crates/server/fixtures/radius-relay.json` (recorded from the TypeScript `createRadiusClientTransportFactory` and
//! `RadiusClientReconnect` by `crates/server/fixtures/generate-radius-relay.mjs`), with a fake gateway on 127.0.0.1;
//! plus the client TUI's recovery statuses (`client-tui.ts` `#handleConnectionState`, `#handleAttachmentState`).

#[path = "../../server/tests/support/mod.rs"]
mod support;

use client::radius::{
    AttachmentState, ByteTransportHandlers, Cancellation, ConnectionState, RadiusClientReconnect, ReconnectClient,
    RecoveryView, attachment_state_view, connect_radius_client_transport, connection_state_view,
};
use protocol::websocket::Message;
use serde_json::{Value, json};
use server::radius_relay::{RelayAuth, RelayAuthResolver};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use support::gateway::{Gateway, HANG_GUARD, hex, unhex};

static FIXTURE: LazyLock<Value> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../../server/fixtures/radius-relay.json")).expect("fixture parses")
});

const SERVER_ID: &str = "00000000-0000-4000-8000-000000000001";

fn scenarios(kind: &str) -> Vec<&'static Value> {
    FIXTURE["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["kind"] == kind)
        .collect()
}

struct FixedAuth(String);

impl RelayAuthResolver for FixedAuth {
    fn resolve(&self, _required: bool) -> Result<Option<RelayAuth>, String> {
        Ok(Some(RelayAuth {
            gateway: self.0.clone(),
            token: "secret".into(),
        }))
    }
}

struct Events(Sender<Value>);

impl ByteTransportHandlers for Events {
    fn on_data(&mut self, chunk: &[u8]) {
        let _ = self.0.send(json!({ "data": hex(chunk) }));
    }
    fn on_close(&mut self) {
        let _ = self.0.send(json!({ "close": true }));
    }
    fn on_error(&mut self, error: &str) {
        let _ = self.0.send(json!({ "error": error }));
    }
}

fn run_client(scenario: &Value) -> Value {
    let gateway = Gateway::start();
    let (tx, rx): (Sender<Value>, Receiver<Value>) = channel();
    let auth = FixedAuth(gateway.origin());
    let steps = scenario["steps"].as_array().unwrap();
    let refuse = steps[0]["connect"].get("fail").is_some();
    let (opened_tx, opened_rx) = channel();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let _ = opened_tx.send(connect_radius_client_transport(SERVER_ID, &auth, Box::new(Events(tx))));
        });
        let mut socket = gateway.accept(refuse);
        let opened = opened_rx.recv_timeout(HANG_GUARD).expect("the transport settled");
        let (transport, open_error) = match opened {
            Ok(transport) => (Some(transport), Value::Null),
            Err(error) => (None, json!(error)),
        };
        let mut events = Vec::new();
        let next_event = |events: &mut Vec<Value>| {
            events.push(rx.recv_timeout(HANG_GUARD).expect("a transport event"));
        };
        for step in &steps[1..] {
            let transport = transport.as_ref().expect("an open transport");
            if let Some(chunk) = step.get("clientSend").and_then(Value::as_str) {
                if let Err(error) = transport.send(&unhex(chunk)) {
                    events.push(json!({ "sendError": error }));
                }
            } else if step.get("clientClose").is_some() {
                transport.close();
            } else if let Some(binary) = step.get("binary").and_then(Value::as_str) {
                let open = !transport.is_closed();
                socket.send(Message::Binary(unhex(binary)));
                if open {
                    next_event(&mut events);
                }
            } else if let Some(text) = step.get("text").and_then(Value::as_str) {
                socket.send(Message::Text(text.into()));
                next_event(&mut events);
            } else if let Some(close) = step.get("close") {
                socket.close(close[0].as_u64().unwrap() as u16, close[1].as_str().unwrap());
                next_event(&mut events);
            } else if step.get("drop").is_some() {
                socket.drop_connection();
                next_event(&mut events);
            }
        }
        drop(transport);
        socket.wait_done();
        // Nothing arrives after the transport is gone.
        events.extend(rx.try_iter());
        let sockets = if refuse {
            vec![]
        } else {
            let s = socket.snapshot();
            vec![
                json!({"sent": s["sent"], "closed": s["closed"], "path": s["path"], "protocol": s["protocol"], "authorization": s["authorization"]}),
            ]
        };
        json!({ "sockets": sockets, "openError": open_error, "events": events })
    })
}

#[test]
fn client_radius_transport_replays_recorded_typescript_scenarios() {
    let mut failures = Vec::new();
    let all = scenarios("client");
    assert_eq!(all.len(), 6);
    for scenario in all {
        let name = scenario["name"].as_str().unwrap();
        let rust = run_client(scenario);
        let expected_sockets: Vec<Value> = scenario["sockets"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|_| scenario["openError"].is_null())
            .map(|s| json!({"sent": s["sent"], "closed": s["closed"], "path": s["path"], "protocol": s["protocol"], "authorization": s["authorization"]}))
            .collect();
        for (what, expected, actual) in [
            ("sockets", json!(expected_sockets), rust["sockets"].clone()),
            ("openError", scenario["openError"].clone(), rust["openError"].clone()),
            ("events", scenario["events"].clone(), rust["events"].clone()),
        ] {
            if expected != actual {
                failures.push(format!("{name}: {what}\n  expected {expected}\n  actual   {actual}"));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

// ---------------------------------------------------------------------------------------------- the reconnector

struct FakeClient {
    state: Mutex<(bool, ConnectionState)>,
    reconnects: Mutex<Vec<String>>,
    log: Arc<Mutex<Vec<String>>>,
    reconnector: OnceLock<Arc<RadiusClientReconnect>>,
}

impl ReconnectClient for FakeClient {
    fn connected(&self) -> bool {
        self.state.lock().unwrap().0
    }
    fn connection_state(&self) -> ConnectionState {
        self.state.lock().unwrap().1
    }
    fn disconnect(&self, reason: &str) {
        self.log.lock().unwrap().push(format!("disconnect: {reason}"));
        *self.state.lock().unwrap() = (false, ConnectionState::Disconnected);
        if let Some(reconnector) = self.reconnector.get() {
            reconnector.connection_state_changed(ConnectionState::Disconnected);
        }
    }
    fn reconnect(&self) -> Result<(), String> {
        self.log.lock().unwrap().push("reconnect".into());
        let outcome = {
            let mut outcomes = self.reconnects.lock().unwrap();
            if outcomes.is_empty() {
                "ok".to_string()
            } else {
                outcomes.remove(0)
            }
        };
        if outcome != "ok" {
            return Err(outcome);
        }
        *self.state.lock().unwrap() = (true, ConnectionState::Connected);
        if let Some(reconnector) = self.reconnector.get() {
            reconnector.connection_state_changed(ConnectionState::Connected);
        }
        Ok(())
    }
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect()
}

fn run_reconnect(scenario: &Value) -> (Vec<String>, Vec<u64>) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let client = Arc::new(FakeClient {
        state: Mutex::new((true, ConnectionState::Connected)),
        reconnects: Mutex::new(strings(&scenario["reconnects"])),
        log: log.clone(),
        reconnector: OnceLock::new(),
    });
    let reattaches = Arc::new(Mutex::new(strings(&scenario["reattaches"])));
    let reattach_log = log.clone();
    let delays = Arc::new(Mutex::new(Vec::new()));
    let recorded = delays.clone();
    let reconnector = Arc::new(RadiusClientReconnect::new(
        client.clone(),
        scenario["attached"].as_str().map(str::to_string),
        Arc::new(move |session: &str| {
            reattach_log.lock().unwrap().push(format!("reattach {session}"));
            let mut outcomes = reattaches.lock().unwrap();
            let outcome = if outcomes.is_empty() {
                "ok".to_string()
            } else {
                outcomes.remove(0)
            };
            if outcome == "ok" { Ok(()) } else { Err(outcome) }
        }),
        Some(Arc::new(move |ms, cancel: &Cancellation| {
            recorded.lock().unwrap().push(ms);
            !cancel.is_cancelled()
        })),
    ));
    let _ = client.reconnector.set(reconnector.clone());
    for step in scenario["steps"].as_array().unwrap() {
        match step {
            Value::String(s) if s == "disconnect" => client.disconnect("lost"),
            Value::String(s) if s == "dispose" => reconnector.dispose(),
            other => reconnector.attachment_changed(other["attach"].as_str()),
        }
        reconnector.wait_idle();
    }
    reconnector.dispose();
    let log = log.lock().unwrap().clone();
    let delays = delays.lock().unwrap().clone();
    (log, delays)
}

#[test]
fn client_radius_reconnect_replays_recorded_typescript_scenarios() {
    let mut failures = Vec::new();
    let all = scenarios("reconnect");
    assert_eq!(all.len(), 7);
    for scenario in all {
        let name = scenario["name"].as_str().unwrap();
        let (log, delays) = run_reconnect(scenario);
        if json!(log) != scenario["log"] || json!(delays) != scenario["delays"] {
            failures.push(format!(
                "{name}\n  expected {} {}\n  actual   {} {}",
                scenario["log"],
                scenario["delays"],
                json!(log),
                json!(delays)
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

// ----------------------------------------------------------------------------------------------- TUI statuses

/// `#handleConnectionState` / `#handleAttachmentState` of client-tui.ts (lines 894-924), literally.
#[test]
fn client_radius_recovery_statuses_match_the_typescript_tui() {
    let view = |busy, status, close_lane| {
        Some(RecoveryView {
            busy,
            status,
            close_lane,
        })
    };
    assert_eq!(
        connection_state_view(ConnectionState::Connecting, true),
        view(true, "Reconnecting to Radius…", true)
    );
    assert_eq!(
        connection_state_view(ConnectionState::Disconnected, true),
        view(true, "Radius disconnected; retrying…", true)
    );
    assert_eq!(
        connection_state_view(ConnectionState::Connected, false),
        view(true, "Reattaching Session…", false)
    );
    assert_eq!(connection_state_view(ConnectionState::Connected, true), None);
    assert_eq!(
        attachment_state_view(&AttachmentState::Attaching("s".into()), "s"),
        view(true, "Reattaching Session…", false)
    );
    assert_eq!(
        attachment_state_view(&AttachmentState::Attached("s".into()), "s"),
        view(false, "", false)
    );
    assert_eq!(
        attachment_state_view(&AttachmentState::Attached("other".into()), "s"),
        None
    );
    assert_eq!(attachment_state_view(&AttachmentState::Detached, "s"), None);
}
