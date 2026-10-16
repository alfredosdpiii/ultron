//! `/trust`, `/debug` and `/reload` in a real terminal (tmux) against the Rust server and session worker
//! (client-tui-pi-commands.ts `showTrustSelector` / `writeDebugLog`, slash-commands-provider.ts `/reload`): the trust
//! choice reaches the profile's trust.json through the worker, the debug log lands in the agent dir with the worker's
//! facts, and the plugin reload answers.

mod tui_harness;

use std::os::unix::fs::PermissionsExt;
use tui_harness::{Mock, Tui};

#[test]
fn cli_interactive_trust_debug_and_reload_work_through_the_worker() {
    let mock = Mock::start(Vec::new(), None);
    let tui = Tui::start(
        mock.port,
        100,
        30,
        &["--model", "anthropic/claude-sonnet-4-5"],
        &[("ULTRON_AUTO_NAME", "off"), ("ULTRON_SPLASH", "off")],
    );
    tui.wait("Session:");
    let agent = tui.root.join("agent");
    let cwd = tui.root.join("cwd").canonicalize().unwrap();

    tui.send_text("/trust");
    tui.send_keys(&["Enter"]);
    tui.wait("Project trust");
    tui.wait("Saved decision: none");
    tui.send_keys(&["Down", "Down", "Enter"]);
    tui.wait("Saved trust decision: untrusted. Restart ultron for this to take effect.");
    let trust: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(agent.join("trust.json")).unwrap()).unwrap();
    assert_eq!(trust[cwd.display().to_string()], false, "{trust}");
    // Reopened, the saved decision is shown and preselected.
    tui.send_text("/trust");
    tui.send_keys(&["Enter"]);
    // (The harness shows the temp root as `<TMP>`.)
    tui.wait("Saved decision: untrusted (<TMP>/cwd)");
    tui.send_keys(&["Escape"]);
    // The next keys must not join the escape (Alt+/): wait for the selector to close first.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while tui.capture().contains("Project trust") {
        assert!(std::time::Instant::now() < deadline, "the trust selector did not close");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    tui.send_text("/debug");
    tui.send_keys(&["Enter"]);
    tui.wait("Debug log written");
    let log_path = agent.join("ultron-debug.log");
    let log = std::fs::read_to_string(&log_path).unwrap();
    assert!(log.starts_with("Debug output at "), "{log}");
    assert!(log.contains("=== Session worker ==="), "{log}");
    assert!(log.contains("\"model\": \"anthropic/claude-sonnet-4-5\""), "{log}");
    assert!(log.contains("Server transport: unix"), "{log}");
    assert!(log.contains("=== All rendered lines with visible widths ==="), "{log}");
    assert!(!log.contains("test-key"), "no credentials in the log");
    assert_eq!(
        std::fs::metadata(&log_path).unwrap().permissions().mode() & 0o777,
        0o600
    );

    tui.send_text("/reload");
    tui.send_keys(&["Enter"]);
    tui.wait("Reloaded plugins.");
    assert!(!tui.capture().contains("not available in the Rust build"));
}
