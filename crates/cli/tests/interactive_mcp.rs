//! `/mcp` in the real TUI (tmux) over the real server and session worker (client-tui.ts `#serveExtensionUI` with
//! slash-commands-provider.ts `directExtensionCommand`): `/mcp status` shows the worker's notice in the status line,
//! and `/mcp` opens the server menu, then the action menu, whose Connect answer connects the server.

mod tui_harness;

use std::path::Path;
use tui_harness::{Mock, TempDir, Tui};

fn python() -> String {
    std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into())
}

#[test]
fn cli_interactive_mcp_status_and_menu_in_the_tui() {
    let mock = Mock::start(Vec::new(), None);
    let tmp = TempDir::new();
    let agent = tmp.0.join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    let server = Path::new(env!("CARGO_MANIFEST_DIR")).join("../mcp/fixtures/fake_gateway_server.py");
    let config = serde_json::json!({ "mcpServers": {
        "beta": { "command": python(), "args": [server, "beta", "-"] },
    }});
    std::fs::write(agent.join("mcp.json"), config.to_string()).unwrap();
    let tui = Tui::start_at(
        &tmp.0,
        mock.port,
        100,
        30,
        &["--model", "anthropic/claude-sonnet-4-5"],
        &[],
    );
    tui.wait("ultron v");
    tui.send_text("/mcp status");
    tui.send_keys(&["Enter"]);
    tui.wait("MCP: 0/1 servers connected");
    tui.send_text("/mcp");
    tui.send_keys(&["Enter"]);
    tui.wait("MCP servers");
    tui.wait("beta \u{2014} not connected");
    tui.send_keys(&["Enter"]);
    tui.wait("beta (not connected)");
    tui.send_keys(&["Enter"]);
    let screen = tui.wait("Connected to beta (2 tools).");
    assert!(!screen.contains("MCP servers"), "{screen}");
}
