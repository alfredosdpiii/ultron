//! The client's singleton service replicas against a fake server over a socket pair: Chord's consumer side
//! (packages/chord/src/services/consumer.ts:579-608, state.ts:146-188) and the wire decoders (state-codec.ts).
//! Ported from packages/chord/test/services.test.ts ("clears replicated state after a %s operation sequence") and
//! state.test.ts ("clears a replica when an adopted update is invalid"), plus the `unavailable`/`replaced` updates.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};

use client::runtime::{Connection, to_cbor, to_json};
use protocol::{
    ClientMessage, ClientMessageDecoder, DEFAULT_MAX_FRAME_LENGTH, PROTOCOL_VERSION, ServerMessage,
    encode_server_message,
};
use serde_json::{Value as Json, json};

const SERVER: &str = "00000000-0000-4000-8000-00000000000a";

struct Fake {
    stream: UnixStream,
    decoder: ClientMessageDecoder,
    queued: Vec<ClientMessage>,
}

impl Fake {
    fn next(&mut self) -> ClientMessage {
        let mut buffer = [0u8; 65536];
        while self.queued.is_empty() {
            let n = self.stream.read(&mut buffer).expect("read");
            assert!(n > 0, "client closed");
            self.queued = self.decoder.push(&buffer[..n]).expect("decode");
        }
        self.queued.remove(0)
    }

    fn send(&mut self, message: ServerMessage) {
        let frame = encode_server_message(&message, DEFAULT_MAX_FRAME_LENGTH).expect("encode");
        self.stream.write_all(&frame).expect("write");
    }

    fn request(&mut self) -> (String, Json) {
        match self.next() {
            ClientMessage::Request { id, call, .. } => (id, to_json(&call)),
            other => panic!("expected a request, got {other:?}"),
        }
    }

    fn respond(&mut self, id: &str, result: Json) {
        self.send(ServerMessage::Response {
            id: id.into(),
            result: Ok(Some(to_cbor(&result))),
        });
    }

    fn update(&mut self, subscription: &str, update: Json) {
        self.send(ServerMessage::ServiceUpdate {
            subscription_id: subscription.into(),
            update: to_cbor(&update),
        });
    }
}

fn connect() -> (Arc<Connection>, Fake) {
    let (client, server) = UnixStream::pair().expect("pair");
    let mut fake = Fake {
        stream: server,
        decoder: ClientMessageDecoder::default(),
        queued: Vec::new(),
    };
    let handshake = std::thread::spawn(move || {
        assert_eq!(
            fake.next(),
            ClientMessage::Hello {
                version: PROTOCOL_VERSION
            }
        );
        fake.send(ServerMessage::Hello {
            version: PROTOCOL_VERSION,
            server_id: SERVER.into(),
        });
        fake
    });
    let read = client.try_clone().expect("clone");
    let connection = Connection::connect_over(Box::new(read), Box::new(client), SERVER).expect("connect");
    (connection, handshake.join().expect("fake"))
}

fn models_snapshot(service: &str, ops: Json) -> Json {
    json!({"serviceId": service, "mode": "singleton", "instances": [{"members": [
        {"name": "select", "kind": "method"},
        {"name": "state", "kind": "state", "sequence": 0, "ops": ops},
    ]}]})
}

/// Subscribe `service`, answering with `snapshot`; returns the subscription id or the subscribe error.
fn subscribe(connection: &Connection, fake: &mut Fake, service: &str, snapshot: Json) -> Result<String, String> {
    let (subscription, request) = connection
        .start_subscribe(connection.server_target(), service)
        .expect("start");
    let (id, call) = fake.request();
    assert_eq!(call["serviceId"], "$chord.service");
    assert_eq!(call["args"], json!([subscription, service, "singleton"]));
    fake.respond(&id, snapshot);
    connection
        .finish_subscribe(&subscription, &request)
        .map(|()| subscription)
        .map_err(|e| e.to_string())
}

fn subscribe_models(connection: &Connection, fake: &mut Fake) -> String {
    subscribe(
        connection,
        fake,
        "pi.models",
        models_snapshot("pi.models", json!([["r", {"selected": null, "revision": 0}]])),
    )
    .expect("subscribe")
}

/// Send `updates` on `subscription`, then answer a round trip: the client's reader handles messages in order, so
/// every update has been applied when the round trip returns.
fn deliver(connection: &Connection, fake: &mut Fake, subscription: &str, updates: &[Json]) -> Result<(), String> {
    let id = connection
        .start(connection.server_target(), "test.sync", "ping", json!([]))
        .map_err(|e| e.to_string())?;
    let (request, _) = fake.request();
    assert_eq!(request, id);
    for update in updates {
        fake.update(subscription, update.clone());
    }
    fake.respond(&id, Json::Null);
    connection.wait(&id).map(|_| ()).map_err(|e| e.to_string())
}

fn revision(connection: &Connection, subscription: &str) -> Option<Json> {
    connection.state(subscription, "state").map(|s| s["revision"].clone())
}

#[test]
fn client_replica_duplicate_or_gap_sequence_clears_the_state_and_reports_it() {
    for sequence in [0, 2] {
        let (connection, mut fake) = connect();
        let models = subscribe_models(&connection, &mut fake);
        assert_eq!(revision(&connection, &models), Some(json!(0)));
        let update = json!({"type": "state", "member": "state", "sequence": sequence,
            "ops": [["r", {"selected": null, "revision": sequence}]]});
        deliver(&connection, &mut fake, &models, &[update]).expect("connection stays up");
        assert_eq!(connection.state(&models, "state"), None, "sequence {sequence}");
        assert_eq!(
            connection.take_replica_errors(&models),
            ["Replicated state update sequence has a gap"]
        );
        assert!(!connection.is_closed());
        // A cleared replica refuses later updates until it is hydrated again.
        let next = json!({"type": "state", "member": "state", "sequence": 1, "ops": [["s", ["revision"], 1]]});
        deliver(&connection, &mut fake, &models, &[next]).expect("still up");
        assert_eq!(
            connection.take_replica_errors(&models),
            ["Replicated state received an update before hydration"]
        );
        assert_eq!(connection.state(&models, "state"), None);
    }
}

#[test]
fn client_replica_applies_consecutive_updates_and_tells_the_listener() {
    let (connection, mut fake) = connect();
    let models = subscribe_models(&connection, &mut fake);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    connection.listen(
        &models,
        Box::new(move |member, value| {
            sink.lock()
                .unwrap()
                .push((member.to_string(), value["revision"].clone()))
        }),
    );
    let updates =
        [1, 2].map(|n| json!({"type": "state", "member": "state", "sequence": n, "ops": [["s", ["revision"], n]]}));
    deliver(&connection, &mut fake, &models, &updates).expect("deliver");
    assert_eq!(revision(&connection, &models), Some(json!(2)));
    assert_eq!(
        *seen.lock().unwrap(),
        [("state".to_string(), json!(1)), ("state".to_string(), json!(2))]
    );
    assert!(connection.take_replica_errors(&models).is_empty());
}

#[test]
fn client_replica_update_that_does_not_apply_clears_only_that_replica() {
    let (connection, mut fake) = connect();
    let values = subscribe(
        &connection,
        &mut fake,
        "test.values",
        models_snapshot("test.values", json!([["r", {"values": [1, 2]}]])),
    )
    .expect("values");
    let models = subscribe_models(&connection, &mut fake);
    let bad = json!({"type": "state", "member": "state", "sequence": 1, "ops": [["m", ["values"], [0]]]});
    deliver(&connection, &mut fake, &values, &[bad]).expect("connection stays up");
    assert_eq!(connection.state(&values, "state"), None);
    assert_eq!(connection.take_replica_errors(&values).len(), 1);
    // The other subscription keeps its replica and keeps updating.
    assert_eq!(revision(&connection, &models), Some(json!(0)));
    let good = json!({"type": "state", "member": "state", "sequence": 1, "ops": [["s", ["revision"], 7]]});
    deliver(&connection, &mut fake, &models, &[good]).expect("deliver");
    assert_eq!(revision(&connection, &models), Some(json!(7)));
    assert!(connection.take_replica_errors(&models).is_empty());
}

#[test]
fn client_replica_unavailable_clears_and_replaced_rehydrates() {
    let (connection, mut fake) = connect();
    let models = subscribe_models(&connection, &mut fake);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    connection.listen(
        &models,
        Box::new(move |_, value| sink.lock().unwrap().push(value["revision"].clone())),
    );
    deliver(&connection, &mut fake, &models, &[json!({"type": "unavailable"})]).expect("unavailable");
    assert_eq!(connection.state(&models, "state"), None);
    assert!(connection.take_replica_errors(&models).is_empty());

    let replaced = json!({"type": "replaced", "snapshot": {"members": [
        {"name": "select", "kind": "method"},
        {"name": "state", "kind": "state", "sequence": 5, "ops": [["r", {"selected": "m", "revision": 5}]]},
    ]}});
    // The replacement starts a fresh decoder and sequence: the next update is sequence 6.
    let next = json!({"type": "state", "member": "state", "sequence": 6, "ops": [["s", ["revision"], 6]]});
    deliver(&connection, &mut fake, &models, &[replaced, next]).expect("replaced");
    assert_eq!(revision(&connection, &models), Some(json!(6)));
    assert_eq!(connection.state(&models, "state").unwrap()["selected"], "m");
    assert_eq!(*seen.lock().unwrap(), [json!(5), json!(6)]);
    assert!(connection.take_replica_errors(&models).is_empty());
}

#[test]
fn client_replica_state_update_after_unavailable_breaks_the_stream() {
    let (connection, mut fake) = connect();
    let models = subscribe_models(&connection, &mut fake);
    // `unavailable` resets the decoders: a state update without a replacement names an unknown state.
    let update = json!({"type": "state", "member": "state", "sequence": 1, "ops": [["s", ["revision"], 1]]});
    let result = deliver(
        &connection,
        &mut fake,
        &models,
        &[json!({"type": "unavailable"}), update],
    );
    assert_eq!(result, Err("Unknown service state state".into()));
    assert!(connection.is_closed());
}

#[test]
fn client_replica_replacement_errors_are_reported_on_the_replica() {
    let (connection, mut fake) = connect();
    let models = subscribe_models(&connection, &mut fake);
    let addressed = json!({"type": "replaced", "snapshot": {"instance": {"key": "k", "generation": 1}, "members": [
        {"name": "state", "kind": "state", "sequence": 0, "ops": [["r", {"revision": 9}]]},
    ]}});
    deliver(&connection, &mut fake, &models, &[addressed]).expect("addressed");
    assert_eq!(
        connection.take_replica_errors(&models),
        ["Singleton replacement has an instance address"]
    );
    assert_eq!(
        revision(&connection, &models),
        Some(json!(0)),
        "the replica keeps its value"
    );

    let not_base = json!({"type": "replaced", "snapshot": {"members": [
        {"name": "select", "kind": "method"},
        {"name": "state", "kind": "state", "sequence": 0, "ops": [["s", ["revision"], 3]]},
    ]}});
    deliver(&connection, &mut fake, &models, &[not_base]).expect("not base");
    assert_eq!(
        connection.take_replica_errors(&models),
        ["Replicated state snapshot is not a base operation batch"]
    );
    assert_eq!(connection.state(&models, "state"), None);

    let dropped = json!({"type": "replaced", "snapshot": {"members": [{"name": "select", "kind": "method"}]}});
    deliver(&connection, &mut fake, &models, &[dropped]).expect("dropped");
    assert_eq!(
        connection.take_replica_errors(&models),
        ["Unknown remote service member pi.models.state"]
    );

    let kind = json!({"type": "replaced", "snapshot": {"members": [{"name": "state", "kind": "method"}]}});
    deliver(&connection, &mut fake, &models, &[kind]).expect("kind");
    assert_eq!(
        connection.take_replica_errors(&models),
        ["Remote service member pi.models.state changed kind"]
    );
    assert!(!connection.is_closed());
}

#[test]
fn client_replica_subscribe_rejects_bad_snapshots() {
    let (connection, mut fake) = connect();
    let not_base = subscribe(
        &connection,
        &mut fake,
        "pi.models",
        models_snapshot("pi.models", json!([["s", ["revision"], 1]])),
    );
    assert_eq!(
        not_base,
        Err("Replicated state snapshot is not a base operation batch".into())
    );
    let two = json!({"serviceId": "pi.models", "mode": "singleton", "instances": [{"members": []}, {"members": []}]});
    assert_eq!(
        subscribe(&connection, &mut fake, "pi.models", two),
        Err("Remote service pi.models returned an invalid singleton snapshot".into())
    );
    let other = models_snapshot("pi.other", json!([["r", {}]]));
    assert_eq!(
        subscribe(&connection, &mut fake, "pi.models", other),
        Err("Remote service pi.models returned an invalid singleton snapshot".into())
    );
    let keyed = json!({"serviceId": "pi.models", "mode": "keyed", "instances": [{"members": []}]});
    assert_eq!(
        subscribe(&connection, &mut fake, "pi.models", keyed),
        Err("Remote service pi.models returned an invalid singleton snapshot".into())
    );
    let duplicate = json!({"serviceId": "pi.models", "mode": "singleton", "instances": [{"members": [
        {"name": "select", "kind": "method"}, {"name": "select", "kind": "method"}]}]});
    assert_eq!(
        subscribe(&connection, &mut fake, "pi.models", duplicate),
        Err("Remote service has invalid member descriptions".into())
    );
    assert!(!connection.is_closed());
}

#[test]
fn client_replica_updates_before_the_snapshot_are_checked_in_order() {
    let (connection, mut fake) = connect();
    let (subscription, request) = connection
        .start_subscribe(connection.server_target(), "pi.models")
        .expect("start");
    let (id, _) = fake.request();
    // An update queued before the snapshot was read, with a gap: reported once the replica is installed.
    fake.update(
        &subscription,
        json!({"type": "state", "member": "state", "sequence": 3, "ops": [["s", ["revision"], 3]]}),
    );
    fake.respond(
        &id,
        models_snapshot("pi.models", json!([["r", {"selected": null, "revision": 0}]])),
    );
    // Wait for the reader to see the response; the update before it is then queued for the subscription.
    let ping = connection
        .start(connection.server_target(), "test.sync", "ping", json!([]))
        .expect("ping");
    let (ping_id, _) = fake.request();
    fake.respond(&ping_id, Json::Null);
    connection.wait(&ping).expect("ping");
    connection.finish_subscribe(&subscription, &request).expect("finish");
    assert_eq!(connection.state(&subscription, "state"), None);
    assert_eq!(
        connection.take_replica_errors(&subscription),
        ["Replicated state update sequence has a gap"]
    );
}
