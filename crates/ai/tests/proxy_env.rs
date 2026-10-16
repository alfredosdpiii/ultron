//! Proxies as the TypeScript HTTP stack picks them (undici's `EnvHttpProxyAgent`): `fixtures/proxy-env.json`, recorded
//! by `fixtures/generate-proxy-env.mjs`, replayed through `ai::net::ProxyEnv::proxy_for`; then a client built from an
//! injected environment talking to a 127.0.0.1 fake proxy.

use ai::Transport;
use ai::net::{ClientConfig, ProxyEnv};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::mpsc;

/// Only a hang guard: an event that is due arrives at once.
const HANG_GUARD: std::time::Duration = std::time::Duration::from_secs(120);

fn lookup(env: &Value) -> impl Fn(&str) -> Option<String> + use<> {
    let env = env.clone();
    move |name: &str| env.get(name).and_then(Value::as_str).map(str::to_string)
}

#[test]
fn ai_net_proxy_choice_matches_undici_env_http_proxy_agent() {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/proxy-env.json")).unwrap();
    let cases = fixture["cases"].as_array().unwrap();
    assert!(cases.len() >= 200);
    let mut differences = Vec::new();
    for case in cases {
        let proxy = ProxyEnv::from_lookup(&lookup(&case["env"]));
        let url = reqwest::Url::parse(case["url"].as_str().unwrap()).unwrap();
        let chosen = proxy.proxy_for(&url);
        if chosen != case["proxy"].as_str() {
            differences.push(format!(
                "{} with {}: rust {chosen:?}, undici {}",
                case["url"], case["env"], case["proxy"]
            ));
        }
    }
    assert!(differences.is_empty(), "{}", differences.join("\n"));
}

/// A one-shot proxy on 127.0.0.1: it reports each request line it gets and answers `answer`.
fn fake_proxy(answer: &'static str) -> (u16, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            let _ = reader.read_line(&mut line);
            // The rest of the head.
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).unwrap_or(0) == 0 || header == "\r\n" {
                    break;
                }
            }
            let _ = stream.write_all(answer.as_bytes());
            let _ = stream.flush();
            if tx.send(line.trim_end().to_string()).is_err() {
                return;
            }
        }
    });
    (port, rx)
}

fn get(transport: &ai::ReqwestTransport, url: &str) -> Result<u16, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        transport
            .send(ai::HttpRequest {
                method: "GET".into(),
                url: url.into(),
                headers: Default::default(),
                body: String::new(),
            })
            .await
            .map(|r| r.status)
    })
}

const OK: &str = "HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok";
const REFUSED: &str = "HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";

#[test]
fn ai_net_injected_proxy_carries_http_and_tunnels_https() {
    let (port, seen) = fake_proxy(OK);
    let proxy = format!("http://127.0.0.1:{port}");
    let env = serde_json::json!({ "HTTP_PROXY": proxy });
    let transport = ai::ReqwestTransport::for_env(&lookup(&env), None);
    // `.invalid` never resolves: only the proxy can answer.
    assert_eq!(get(&transport, "http://upstream.invalid/x?y=1"), Ok(200));
    assert_eq!(
        seen.recv_timeout(HANG_GUARD).unwrap(),
        "GET http://upstream.invalid/x?y=1 HTTP/1.1"
    );

    let (port, seen) = fake_proxy(REFUSED);
    let env = serde_json::json!({ "https_proxy": format!("http://127.0.0.1:{port}") });
    let transport = ai::ReqwestTransport::for_env(&lookup(&env), None);
    assert!(get(&transport, "https://upstream.invalid/v1").is_err());
    assert_eq!(
        seen.recv_timeout(HANG_GUARD).unwrap(),
        "CONNECT upstream.invalid:443 HTTP/1.1"
    );
}

#[test]
fn ai_net_no_proxy_entries_go_direct() {
    let (proxy_port, proxy_seen) = fake_proxy(OK);
    let (server_port, server_seen) = fake_proxy(OK);
    let env = serde_json::json!({
        "HTTP_PROXY": format!("http://127.0.0.1:{proxy_port}"),
        "NO_PROXY": "localhost, 127.0.0.1",
    });
    let transport = ai::ReqwestTransport::for_env(&lookup(&env), None);
    assert_eq!(
        get(&transport, &format!("http://127.0.0.1:{server_port}/direct")),
        Ok(200)
    );
    assert_eq!(server_seen.recv_timeout(HANG_GUARD).unwrap(), "GET /direct HTTP/1.1");
    assert!(proxy_seen.try_recv().is_err(), "the proxy saw a NO_PROXY request");
}

/// A client from an environment without proxy variables connects directly, whatever this process's own
/// environment holds (no lookup of its own): this test runs itself again as a child whose environment names a proxy.
#[test]
fn ai_net_client_config_ignores_the_process_environment() {
    if let Ok(port) = std::env::var("AI_NET_CHILD_SERVER") {
        let config = ClientConfig::from_lookup(&|_| None);
        assert_eq!(config.proxy, Some(ProxyEnv::default()));
        let transport = ai::ReqwestTransport::configured(config, None).unwrap();
        assert_eq!(get(&transport, &format!("http://127.0.0.1:{port}/plain")), Ok(200));
        return;
    }
    let (server_port, server_seen) = fake_proxy(OK);
    let (proxy_port, proxy_seen) = fake_proxy(OK);
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "ai_net_client_config_ignores_the_process_environment",
            "--test-threads=1",
        ])
        .env("AI_NET_CHILD_SERVER", server_port.to_string())
        .env("HTTP_PROXY", format!("http://127.0.0.1:{proxy_port}"))
        .env("http_proxy", format!("http://127.0.0.1:{proxy_port}"))
        .env("ALL_PROXY", format!("http://127.0.0.1:{proxy_port}"))
        .env_remove("NO_PROXY")
        .env_remove("no_proxy")
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(
        server_seen.recv_timeout(HANG_GUARD).ok().as_deref(),
        Some("GET /plain HTTP/1.1")
    );
    assert!(
        proxy_seen.try_recv().is_err(),
        "the process environment's proxy was used"
    );
}
