//! `ultron mcp list` and `ultron mcp login` reach HTTP MCP servers through the run's own proxy variables (the
//! injected environment, undici's semantics), never this process's: the server is under `.invalid`, so only the
//! 127.0.0.1 fake proxy can answer. No browser is on the run's PATH and the sign-in fails at discovery.

#[path = "../../mcp/tests/support/fake_proxy.rs"]
mod fake_proxy;

use fake_proxy::FakeProxy;
use std::process::{Command, Stdio};

fn run(base: &std::path::Path, proxy: (&str, String), args: &[&str]) -> std::process::Output {
    let (agent, cwd, bin) = (base.join("agent"), base.join("cwd"), base.join("bin"));
    Command::new(env!("CARGO_BIN_EXE_ultron"))
        .args(args)
        .env_clear()
        .current_dir(&cwd)
        .env("PATH", &bin)
        .env("HOME", base)
        .env("ULTRON_CODING_AGENT_DIR", &agent)
        .env("ULTRON_SERVER_DIR", base.join("server"))
        .env("NO_COLOR", "1")
        .env("PI_OFFLINE", "1")
        .env(proxy.0, proxy.1)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

#[test]
fn cli_mcp_proxy_list_and_login_go_through_the_run_proxy() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    for dir in ["agent", "cwd", "bin"] {
        std::fs::create_dir_all(base.join(dir)).unwrap();
    }
    std::fs::write(
        base.join("agent/mcp.json"),
        serde_json::json!({"mcpServers": {"remote": {"url": "http://mcp.invalid/mcp"}}}).to_string(),
    )
    .unwrap();
    let proxy = FakeProxy::requiring_auth();

    let listed = run(&base, ("HTTP_PROXY", proxy.url()), &["mcp", "list"]);
    // A server that needs sign-in makes `mcp list` exit 1, as TypeScript (fixtures/mcp_login.json).
    assert_eq!(listed.status.code(), Some(1), "{listed:?}");
    let stdout = String::from_utf8_lossy(&listed.stdout);
    assert!(stdout.starts_with("remote: needs sign-in"), "{stdout}");
    assert_eq!(proxy.next().as_deref(), Some("POST http://mcp.invalid/mcp HTTP/1.1"));
    proxy.drain();

    let login = run(&base, ("HTTP_PROXY", proxy.url()), &["mcp", "login", "remote"]);
    assert_ne!(login.status.code(), Some(0), "{login:?}");
    let seen = proxy.drain();
    assert!(
        seen.contains(&"GET http://mcp.invalid/.well-known/oauth-protected-resource/mcp HTTP/1.1".to_string()),
        "{seen:?}"
    );
    assert!(!base.join("bin/opened.log").exists());
}

/// `ALL_PROXY` is not one of undici's variables (reqwest's own lookup would use it): with only `ALL_PROXY` set, both
/// commands reach a 127.0.0.1 server directly and the proxy sees nothing.
#[test]
fn cli_mcp_proxy_all_proxy_is_not_an_undici_variable() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    for dir in ["agent", "cwd", "bin"] {
        std::fs::create_dir_all(base.join(dir)).unwrap();
    }
    let (proxy, server) = (FakeProxy::start(), FakeProxy::requiring_auth());
    let url = format!("http://127.0.0.1:{}/mcp", server.port);
    std::fs::write(
        base.join("agent/mcp.json"),
        serde_json::json!({"mcpServers": {
            "remote": {"url": url},
            // A server with its own Authorization header is probed without OAuth.
            "keyed": {"url": url, "headers": {"Authorization": "Bearer fixture-token"}},
        }})
        .to_string(),
    )
    .unwrap();
    let listed = run(&base, ("ALL_PROXY", proxy.url()), &["mcp", "list"]);
    assert_eq!(listed.status.code(), Some(1), "{listed:?}");
    let stdout = String::from_utf8_lossy(&listed.stdout);
    assert!(stdout.contains("keyed: connected, 1 tool"), "{stdout}");
    assert_eq!(server.next().as_deref(), Some("POST /mcp HTTP/1.1"));
    let login = run(&base, ("ALL_PROXY", proxy.url()), &["mcp", "login", "remote"]);
    assert_ne!(login.status.code(), Some(0), "{login:?}");
    let seen = server.drain();
    assert!(
        seen.contains(&"GET /.well-known/oauth-protected-resource/mcp HTTP/1.1".to_string()),
        "{seen:?}"
    );
    assert_eq!(proxy.drain(), Vec::<String>::new());
}
