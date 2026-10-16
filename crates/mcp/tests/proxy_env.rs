//! The MCP crate's HTTP clients take their proxies from the injected environment (`ConnectionEnv::proxy`, undici's
//! `EnvHttpProxyAgent` semantics through `ai::net::ProxyEnv`), as the TypeScript gateway's requests go through the
//! worker's `configureHttpDispatcher`: the streamable HTTP transport, OAuth discovery during sign-in, and the token
//! refresh a connection does on its own. The servers are under `.invalid` (they never resolve), so only the
//! 127.0.0.1 fake proxy can answer; `NO_PROXY` hosts go direct; this process's own environment is never consulted.

#[path = "support/fake_proxy.rs"]
mod fake_proxy;

use ai::net::ProxyEnv;
use fake_proxy::FakeProxy;
use mcp::config::{Scope, ServerEntry};
use mcp::connection::{Connection, ConnectionEnv};
use mcp::http::{HttpOptions, HttpTransport};
use mcp::oauth::flow::StateStore;
use mcp::oauth::store::CredentialStore;
use mcp::{Client, ClientOptions, Transport};
use serde_json::{Value, json};
use std::sync::Arc;

fn proxy_env(vars: &[(&str, String)]) -> ProxyEnv {
    let vars: Vec<(String, String)> = vars.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
    ProxyEnv::from_lookup(&move |k: &str| vars.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()))
}

fn entry(url: &str, headers: Option<Value>) -> ServerEntry {
    let mut config = json!({ "url": url });
    if let Some(headers) = headers {
        config["headers"] = headers;
    }
    ServerEntry {
        name: "remote".into(),
        config: config.as_object().unwrap().clone(),
        source: "test".into(),
        scope: Scope::Global,
    }
}

fn env(root: &std::path::Path, proxy: Option<ProxyEnv>) -> ConnectionEnv {
    ConnectionEnv {
        cwd: root.to_path_buf(),
        home: root.to_path_buf(),
        client_version: "1.0.0".into(),
        proxy,
        ..ConnectionEnv::default()
    }
}

fn connect(transport: HttpTransport) -> Result<(), mcp::Error> {
    let client = Client::new(ClientOptions {
        name: "proxy-test".into(),
        version: "1.0.0".into(),
        ..ClientOptions::default()
    });
    let result = client.connect(Arc::new(transport) as Arc<dyn Transport>).map(drop);
    client.close();
    result
}

/// A connection (`ConnectionEnv::proxy` from the run's `HTTP_PROXY`) initializes and lists tools through the proxy,
/// in absolute form (the decided deviation from undici's `proxyTunnel: true`).
#[test]
fn mcp_proxy_connection_reaches_an_http_server_through_the_injected_proxy() {
    let proxy = FakeProxy::start();
    let tmp = tempfile::tempdir().unwrap();
    let env = env(tmp.path(), Some(proxy_env(&[("HTTP_PROXY", proxy.url())])));
    // An Authorization header: no OAuth, so the only requests are the transport's.
    let connection = Connection::new(
        entry(
            "http://mcp.invalid/mcp",
            Some(json!({ "Authorization": "Bearer fixture-token" })),
        ),
        env,
    );
    connection.get_client().unwrap();
    let tools: Vec<Value> = connection.tools();
    assert_eq!(tools.len(), 1, "{tools:?}");
    assert_eq!(tools[0]["name"], "echo");
    connection.close();
    assert_eq!(proxy.next().as_deref(), Some("POST http://mcp.invalid/mcp HTTP/1.1"));
}

/// `https_proxy` serves `https:` servers through a CONNECT tunnel; a refused tunnel fails the connection.
#[test]
fn mcp_proxy_https_server_is_tunnelled_with_connect() {
    let proxy = FakeProxy::start();
    let transport = HttpTransport::new(HttpOptions {
        url: "https://mcp.invalid/mcp".into(),
        proxy: Some(proxy_env(&[("https_proxy", proxy.url())])),
        ..HttpOptions::default()
    });
    assert!(connect(transport).is_err());
    assert_eq!(proxy.next().as_deref(), Some("CONNECT mcp.invalid:443 HTTP/1.1"));
}

/// A `NO_PROXY` host is reached directly; the proxy sees nothing.
#[test]
fn mcp_proxy_no_proxy_server_goes_direct() {
    let proxy = FakeProxy::start();
    let server = FakeProxy::start();
    let transport = HttpTransport::new(HttpOptions {
        url: format!("http://127.0.0.1:{}/mcp", server.port),
        proxy: Some(proxy_env(&[
            ("HTTP_PROXY", proxy.url()),
            ("NO_PROXY", "localhost, 127.0.0.1".into()),
        ])),
        ..HttpOptions::default()
    });
    connect(transport).unwrap();
    assert_eq!(server.next().as_deref(), Some("POST /mcp HTTP/1.1"));
    assert_eq!(proxy.drain(), Vec::<String>::new(), "the proxy saw a NO_PROXY request");
}

/// Sign-in discovery (`ultron mcp login`, `/mcp login`) goes through the connection's proxies.
#[test]
fn mcp_proxy_oauth_sign_in_discovers_through_the_injected_proxy() {
    let proxy = FakeProxy::start();
    let tmp = tempfile::tempdir().unwrap();
    let env = env(tmp.path(), Some(proxy_env(&[("HTTP_PROXY", proxy.url())])));
    let connection = Connection::new(entry("http://mcp.invalid/mcp", None), env);
    let settings = connection.oauth_settings().unwrap();
    let credentials = Arc::new(CredentialStore::new(&tmp.path().join("agent"), None));
    struct NoBrowser;
    impl mcp::oauth::signin::SignInPrompt for NoBrowser {
        fn show_authorization_url(&self, url: &str) {
            panic!("the sign-in reached the browser step: {url}");
        }
        fn prompt_for_redirect_url(&self, _: &mcp::CancelToken) -> Option<String> {
            None
        }
    }
    // Every discovery and registration request gets a 404, so the sign-in fails before any browser step.
    let result = mcp::oauth::signin::sign_in(
        "http://mcp.invalid/mcp",
        &credentials.for_server("remote", "http://mcp.invalid/mcp"),
        &settings,
        None,
        &NoBrowser,
    );
    assert!(result.is_err());
    let seen = proxy.drain();
    assert!(
        seen.contains(&"GET http://mcp.invalid/.well-known/oauth-protected-resource/mcp HTTP/1.1".to_string()),
        "{seen:?}"
    );
    assert!(seen.iter().all(|line| line.contains("http://mcp.invalid/")), "{seen:?}");
}

/// The token refresh a connection does on its own (an expired access token with a refresh token) discovers the
/// authorization server through the proxy, then the request goes out with the old token, as TypeScript does.
#[test]
fn mcp_proxy_connection_token_refresh_goes_through_the_injected_proxy() {
    let proxy = FakeProxy::start();
    let tmp = tempfile::tempdir().unwrap();
    let agent = tmp.path().join("agent");
    let credentials = Arc::new(CredentialStore::new(&agent, None));
    credentials
        .for_server("remote", "http://mcp.invalid/mcp")
        .save(&json!({
            "tokens": { "access_token": "old-access", "refresh_token": "fixture-refresh", "token_type": "Bearer" },
            "tokensExpireAt": 1,
            "clientInformation": { "client_id": "fixture-client" },
        }))
        .unwrap();
    let env = ConnectionEnv {
        credentials: Some(credentials),
        ..env(tmp.path(), Some(proxy_env(&[("HTTP_PROXY", proxy.url())])))
    };
    let connection = Connection::new(entry("http://mcp.invalid/mcp", None), env);
    connection.get_client().unwrap();
    connection.close();
    let lines = proxy
        .until("POST http://mcp.invalid/mcp HTTP/1.1")
        .unwrap_or_else(|lines| panic!("no initialize through the proxy: {lines:?}"));
    assert_eq!(
        lines.first().map(String::as_str),
        Some("GET http://mcp.invalid/.well-known/oauth-protected-resource/mcp HTTP/1.1"),
        "{lines:?}"
    );
}

/// A client built from an injected environment without proxy variables connects directly whatever this process's
/// own environment names: this test runs itself again as a child whose environment points at a proxy.
#[test]
fn mcp_proxy_injected_environment_ignores_the_process_environment() {
    if let Ok(port) = std::env::var("MCP_PROXY_CHILD_SERVER") {
        let transport = HttpTransport::new(HttpOptions {
            url: format!("http://127.0.0.1:{port}/mcp"),
            proxy: Some(ProxyEnv::default()),
            ..HttpOptions::default()
        });
        connect(transport).unwrap();
        return;
    }
    let server = FakeProxy::start();
    let proxy = FakeProxy::start();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "mcp_proxy_injected_environment_ignores_the_process_environment",
            "--test-threads=1",
        ])
        .env("MCP_PROXY_CHILD_SERVER", server.port.to_string())
        .env("HTTP_PROXY", proxy.url())
        .env("http_proxy", proxy.url())
        .env("ALL_PROXY", proxy.url())
        .env_remove("NO_PROXY")
        .env_remove("no_proxy")
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(server.next().as_deref(), Some("POST /mcp HTTP/1.1"));
    assert_eq!(
        proxy.drain(),
        Vec::<String>::new(),
        "the process environment's proxy was used"
    );
}
