//! The session's MCP gateway (`Config::mcp_gateway`) reaches HTTP servers through the proxies of the session's own
//! environment (TS: the worker's `configureHttpDispatcher`, undici's `EnvHttpProxyAgent`), never through this test
//! process's: the server is under `.invalid`, so only the 127.0.0.1 fake proxy can answer.

#[path = "../../mcp/tests/support/fake_proxy.rs"]
mod fake_proxy;

use fake_proxy::FakeProxy;
use serde_json::json;
use session_core::RunEnv;
use std::path::Path;
use std::sync::Arc;

#[test]
fn session_core_mcp_gateway_reaches_http_servers_through_the_session_proxy() {
    let proxy = FakeProxy::start();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let (home, cwd) = (root.join("home"), root.join("project"));
    let agent = home.join(".ultron/agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(
        agent.join("mcp.json"),
        json!({"mcpServers": {"remote": {
            "url": "http://mcp.invalid/mcp",
            "headers": {"Authorization": "Bearer fixture-token"},
        }}})
        .to_string(),
    )
    .unwrap();
    let vars: Vec<(String, String)> = vec![
        ("HOME".into(), home.display().to_string()),
        ("ULTRON_CODING_AGENT_DIR".into(), agent.display().to_string()),
        ("ULTRON_SERVER_DIR".into(), root.join("server").display().to_string()),
        ("HTTP_PROXY".into(), proxy.url()),
    ];
    let env = RunEnv {
        cwd,
        home,
        var: Arc::new(move |key| vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())),
        docs: Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/docs"),
    };
    let config = session_core::compose::Config::from_env(&env);
    let gateway = config.mcp_gateway();
    let params = json!({ "server": "remote" });
    let result = gateway.execute(params.as_object().unwrap(), None).unwrap();
    gateway.close();
    let text = mcp::gateway::Gateway::text_of(&result);
    assert!(text.contains("echo"), "{text}");
    assert_eq!(proxy.next().as_deref(), Some("POST http://mcp.invalid/mcp HTTP/1.1"));
}
