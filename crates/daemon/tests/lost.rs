//! Ported from packages/coding-agent/test/experimental-lost-server.test.ts.

use daemon::lost::{describe, friendly_status, is_lost};

/// `clientErrorMessage`.
fn client_error_message(message: &str) -> String {
    describe(message).unwrap_or_else(|| message.to_string())
}

#[test]
fn daemon_lost_bare_binding_error_becomes_a_clear_message() {
    assert_eq!(
        describe("Remote service pi.agent-controller binding is closed").as_deref(),
        Some(
            "The Ultron server stopped (Remote service pi.agent-controller binding is closed); your session is saved — run `ultron -c` to resume"
        )
    );
    assert_eq!(
        friendly_status("Error: Remote service pi.agent-controller binding is closed"),
        "Error: The Ultron server stopped (Remote service pi.agent-controller binding is closed); your session is saved — run `ultron -c` to resume"
    );
}

#[test]
fn daemon_lost_recognizes_transport_and_worker_loss_only() {
    assert!(is_lost("Byte transport closed"));
    assert!(is_lost("Session worker s-1 disconnected unexpectedly"));
    assert!(is_lost("Session worker 0199 exited unexpectedly (signal 9)"));
    assert!(is_lost("Ultron server lost its coordinator during a worker operation"));
    assert!(is_lost("write EPIPE"));
    assert!(!is_lost("EPIPEX"));
    assert!(!is_lost("Model not found: test/none"));
    assert_eq!(
        client_error_message("Model not found: test/none"),
        "Model not found: test/none"
    );
    assert_eq!(
        friendly_status("Error: Model not found: test/none"),
        "Error: Model not found: test/none"
    );
}

#[test]
fn daemon_lost_clear_message_is_not_wrapped_twice() {
    let once = client_error_message("Unix connection is closed");
    assert_eq!(client_error_message(&once), once);
    assert_eq!(friendly_status(&format!("Error: {once}")), format!("Error: {once}"));
}
