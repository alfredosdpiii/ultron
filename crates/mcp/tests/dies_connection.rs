//! Decision rows 351/411/495 (`describe dies_x` answered `tool_not_found` once, never reproduced): the orderings
//! that could reach it, made deterministic. A server that exits before answering `initialize` must report
//! `connect_failed` (the text recorded from TypeScript in fixtures/gateway.json, main scenario event 17) whether the
//! describe comes first, after a failed connect, while another call lists every server, after `start`, or from many
//! threads at once.

use mcp::connection::ConnectionEnv;
use mcp::gateway::{Gateway, GatewayOptions};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;

fn python() -> String {
    std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into())
}

const EXPECTED: &str = "MCP server \"dies\" could not be connected: MCP server \"dies\" failed to connect: MCP connection closed\nfatal: cannot start";

fn gateway(root: &Path) -> Arc<Gateway> {
    let agent = root.join("agent");
    let cwd = root.join("project");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    let server = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/fake_gateway_server.py");
    let stdio = |variant: &str| json!({"command": python(), "args": [server.display().to_string(), variant, "-"]});
    let config = json!({"mcpServers": {"dies": stdio("dies"), "beta-srv": stdio("beta"), "plain": stdio("plain")}});
    std::fs::write(agent.join("mcp.json"), config.to_string()).unwrap();
    Gateway::new(GatewayOptions {
        agent_dir: agent,
        project_trusted: false,
        servers: None,
        env: ConnectionEnv {
            cwd,
            home: root.to_path_buf(),
            client_version: mcp::gateway::client_version(),
            ..ConnectionEnv::default()
        },
        open_url: None,
    })
}

fn describe(gateway: &Gateway) -> Value {
    let params = json!({"describe": "dies_x"});
    gateway.execute(params.as_object().unwrap(), None).unwrap()
}

fn assert_connect_failed(result: &Value, when: &str) {
    assert_eq!(
        result["details"],
        json!({"server": "dies", "error": "connect_failed"}),
        "{when}: {result}"
    );
    assert_eq!(result["content"][0]["text"], EXPECTED, "{when}");
}

#[test]
fn mcp_dies_connection_reports_connect_failed_first_and_after_a_failed_connect() {
    let tmp = tempfile::tempdir().unwrap();
    let gateway = gateway(tmp.path());
    assert_connect_failed(&describe(&gateway), "first");
    assert_connect_failed(&describe(&gateway), "again");
    gateway.close();
}

#[test]
fn mcp_dies_connection_reports_connect_failed_while_every_server_is_listed() {
    let tmp = tempfile::tempdir().unwrap();
    let gateway = gateway(tmp.path());
    std::thread::scope(|scope| {
        let listing = scope.spawn(|| gateway.execute(json!({}).as_object().unwrap(), None).unwrap());
        let described = describe(&gateway);
        listing.join().unwrap();
        assert_connect_failed(&described, "during a listing");
    });
    gateway.start(None);
    gateway.settle();
    assert_connect_failed(&describe(&gateway), "after start");
    gateway.close();
}

#[test]
fn mcp_dies_connection_reports_connect_failed_to_concurrent_describes() {
    let tmp = tempfile::tempdir().unwrap();
    let gateway = gateway(tmp.path());
    let results: Vec<Value> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..12).map(|_| scope.spawn(|| describe(&gateway))).collect();
        workers.into_iter().map(|w| w.join().unwrap()).collect()
    });
    for (i, result) in results.iter().enumerate() {
        assert_connect_failed(result, &format!("thread {i}"));
    }
    gateway.close();
}
