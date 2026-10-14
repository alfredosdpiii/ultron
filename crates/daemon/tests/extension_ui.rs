//! The worker's extension UI bridge, ported from packages/coding-agent/test/ultron-extension-ui-bridge.test.ts with
//! its literal expectations: nobody serving, Pi's defaults; a serving client gets Pi-format requests in order and its
//! answers settle the dialogs; timeouts and cancellation; a worker started for an interactive client queues UI for
//! its first poll, and one that never arrives leaves dialogs with their defaults.

use daemon::extension_ui::{BridgeUi, ExtensionUiBridge};
use mcp::gateway::CommandUi;
use serde_json::{Map, Value, json};
use std::sync::Arc;
use std::time::Duration;

fn fields(value: Value) -> Map<String, Value> {
    value.as_object().unwrap().clone()
}

fn methods(polled: &Value) -> Vec<String> {
    polled["requests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["request"]["method"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn daemon_extension_ui_without_a_client_dialogs_take_defaults_and_notifications_drop() {
    let bridge = ExtensionUiBridge::new(false);
    let ui = BridgeUi(bridge.clone());
    assert!(!bridge.serving());
    assert_eq!(ui.select("t", &["a".into()]), None);
    assert_eq!(
        bridge.dialog(
            fields(json!({"method": "confirm", "title": "t", "message": "m"})),
            None,
            &|| false
        ),
        None
    );
    assert_eq!(ui.input("t", "", &mcp::CancelToken::new()), None);
    ui.notify("dropped", "info");
    assert_eq!(bridge.poll(None, Duration::ZERO)["requests"], json!([]));
    // Not kept for a later client either: nothing was queued from the start.
    assert_eq!(bridge.poll(Some(0), Duration::ZERO)["requests"], json!([]));
}

#[test]
fn daemon_extension_ui_a_serving_client_gets_requests_in_order_and_answers_them() {
    let bridge = ExtensionUiBridge::new(false);
    let cursor = bridge.poll(None, Duration::ZERO)["cursor"].as_u64();
    let ui = BridgeUi(bridge.clone());
    std::thread::scope(|scope| {
        let waiting = scope.spawn(|| bridge.poll(cursor, Duration::from_secs(120)));
        ui.notify("hello", "warning");
        let first = waiting.join().unwrap();
        let picked = scope.spawn(|| ui.select("Pick", &["a".into(), "b".into()]));
        // The dialog is offered to the next poll after the notification.
        let after = first["cursor"].as_u64();
        let second = bridge.poll(after, Duration::from_secs(120));
        assert_eq!(methods(&first), ["notify"]);
        assert_eq!(
            first["requests"][0]["request"],
            json!({"type": "extension_ui_request", "method": "notify", "message": "hello", "notifyType": "warning",
                "id": first["requests"][0]["request"]["id"]})
        );
        let select = second["requests"][0]["request"].clone();
        assert_eq!(
            (&select["method"], &select["title"], &select["options"]),
            (&json!("select"), &json!("Pick"), &json!(["a", "b"]))
        );
        // Delivered requests are not repeated; the open dialog is still offered to a client that starts serving now.
        assert_eq!(
            bridge.poll(second["cursor"].as_u64(), Duration::ZERO)["requests"],
            json!([])
        );
        let fresh = bridge.poll(None, Duration::ZERO);
        assert_eq!(fresh["requests"][0]["request"]["id"], select["id"]);
        bridge.respond(select["id"].as_str().unwrap(), json!({"value": "b"}));
        assert_eq!(picked.join().unwrap().as_deref(), Some("b"));
        let confirmed = scope.spawn(|| {
            bridge.dialog(
                fields(json!({"method": "confirm", "title": "Sure?", "message": "really"})),
                None,
                &|| false,
            )
        });
        let third = bridge.poll(second["cursor"].as_u64(), Duration::from_secs(120));
        bridge.respond(
            third["requests"][0]["request"]["id"].as_str().unwrap(),
            json!({"cancelled": true}),
        );
        assert_eq!(confirmed.join().unwrap(), None);
    });
}

#[test]
fn daemon_extension_ui_dialogs_honour_timeout_and_cancel_and_settle_once_the_client_leaves() {
    let bridge = ExtensionUiBridge::new(false);
    bridge.poll(None, Duration::ZERO);
    assert_eq!(
        bridge.dialog(
            fields(json!({"method": "select", "title": "t", "options": ["a"], "timeout": 50})),
            Some(Duration::from_millis(50)),
            &|| false
        ),
        None
    );
    let cancel = mcp::CancelToken::new();
    cancel.cancel(None);
    assert_eq!(BridgeUi(bridge.clone()).input("t", "", &cancel), None);
    // Nobody polls again: after the serving grace the open dialog gets its default instead of hanging.
    assert_eq!(
        bridge.dialog(
            fields(json!({"method": "confirm", "title": "t", "message": "m"})),
            None,
            &|| false
        ),
        None
    );
    assert!(!bridge.serving());
}

#[test]
fn daemon_extension_ui_an_expected_client_gets_what_was_queued_before_its_first_poll() {
    let bridge = ExtensionUiBridge::new(true);
    assert!(bridge.serving());
    bridge.emit(fields(
        json!({"method": "setStatus", "statusKey": "ext", "statusText": "ready"}),
    ));
    std::thread::scope(|scope| {
        let ui = BridgeUi(bridge.clone());
        let picked = scope.spawn(move || ui.select("Early", &["a".into(), "b".into()]));
        let start = std::time::Instant::now();
        while bridge.open_dialogs() == 0 {
            assert!(start.elapsed() < Duration::from_secs(120), "the dialog never opened");
            std::thread::sleep(Duration::from_millis(5));
        }
        let first = bridge.poll(None, Duration::from_secs(1));
        assert_eq!(methods(&first), ["setStatus", "select"]);
        bridge.respond(
            first["requests"][1]["request"]["id"].as_str().unwrap(),
            json!({"value": "a"}),
        );
        assert_eq!(picked.join().unwrap().as_deref(), Some("a"));
    });
    bridge.close();
    let _ = Arc::strong_count(&bridge);
}

#[test]
fn daemon_extension_ui_an_expected_client_that_never_arrives_leaves_dialogs_with_defaults() {
    let bridge = ExtensionUiBridge::expecting(Some(Duration::from_millis(50)));
    assert_eq!(
        bridge.dialog(
            fields(json!({"method": "confirm", "title": "Anyone?", "message": "m"})),
            None,
            &|| false
        ),
        None
    );
    assert!(!bridge.serving());
    bridge.close();
}

#[test]
fn daemon_extension_ui_buffered_notifications_are_dropped_once_the_client_stops_polling() {
    let bridge = ExtensionUiBridge::new(false);
    let cursor = bridge.poll(None, Duration::ZERO)["cursor"].as_u64();
    BridgeUi(bridge.clone()).notify("kept while serving", "info");
    assert_eq!(bridge.buffered_events(), 1);
    // Nobody polls: after the serving grace the buffered notification is gone, so a client that comes back later
    // does not receive it.
    let start = std::time::Instant::now();
    while bridge.buffered_events() > 0 {
        assert!(
            start.elapsed() < Duration::from_secs(120),
            "the notification was never dropped"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!bridge.serving());
    assert_eq!(bridge.poll(cursor, Duration::ZERO)["requests"], json!([]));
    bridge.close();
}
