//! `AWS_BEDROCK_FORCE_HTTP1=1` (`api/bedrock-converse-stream.ts`: `NodeHttpHandler` in place of the SDK's default
//! `NodeHttp2Handler`): the Converse stream request goes out over HTTP/1.1. Replays the handler of each FORCE_HTTP1
//! case of `crates/ai/fixtures/bedrock-endpoint.json` (`generate-bedrock-endpoint.mjs`) against a 127.0.0.1 TLS
//! server that offers h2 and http/1.1 and reports what the client negotiated (ALPN): `NodeHttp2Handler` is h2,
//! `NodeHttpHandler` http/1.1. The test generates its own CA and server certificate; no proxy, no credentials
//! (`AWS_BEDROCK_SKIP_AUTH=1` in the request env).

use ai::ReqwestTransport;
use ai::bedrock::{self, BedrockOptions};
use ai::net::{ClientConfig, ProxyEnv};
use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio_rustls::rustls;
use tokio_rustls::rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};

/// Only a hang guard: the handshake report arrives as soon as the client connects.
const HANG_GUARD: std::time::Duration = std::time::Duration::from_secs(120);

/// A TLS server offering h2 and http/1.1; each handshake reports the protocol it settled on, then the connection
/// closes. Returns the port, the receiver and the CA certificate (PEM) to trust.
async fn alpn_server() -> (u16, tokio::sync::mpsc::UnboundedReceiver<String>, String) {
    let ca_key = KeyPair::generate().unwrap();
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca = ca_params.self_signed(&ca_key).unwrap();
    let issuer = Issuer::new(ca_params, ca_key);
    let server_key = KeyPair::generate().unwrap();
    let server = CertificateParams::new(vec!["127.0.0.1".to_string()])
        .unwrap()
        .signed_by(&server_key, &issuer)
        .unwrap();
    let mut config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![server.der().clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(server_key.serialize_der())),
        )
        .unwrap();
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else { return };
            let acceptor = acceptor.clone();
            let tx = tx.clone();
            tokio::spawn(async move {
                if let Ok(tls) = acceptor.accept(tcp).await {
                    let alpn = tls
                        .get_ref()
                        .1
                        .alpn_protocol()
                        .map(|p| String::from_utf8_lossy(p).into_owned());
                    let _ = tx.send(alpn.unwrap_or_else(|| "none".into()));
                }
            });
        }
    });
    (port, rx, ca.pem())
}

async fn negotiated(force_http1: Option<&str>) -> String {
    let (port, mut seen, ca) = alpn_server().await;
    let transport = ReqwestTransport::configured(
        ClientConfig {
            proxy: Some(ProxyEnv::default()),
            extra_roots: vec![ca.into_bytes()],
        },
        None,
    )
    .unwrap();
    let model: ai::Model = serde_json::from_value(json!({"id": "amazon.nova-lite-v1:0", "name": "Nova Lite", "api": "bedrock-converse-stream", "provider": "amazon-bedrock", "baseUrl": format!("https://127.0.0.1:{port}"), "reasoning": false, "input": ["text"], "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}, "contextWindow": 300000, "maxTokens": 5000})).unwrap();
    let mut env = json!({"AWS_BEDROCK_SKIP_AUTH": "1", "AWS_REGION": "us-west-2"});
    if let Some(value) = force_http1 {
        env["AWS_BEDROCK_FORCE_HTTP1"] = json!(value);
    }
    let options: BedrockOptions = serde_json::from_value(json!({"maxRetries": 0, "env": env})).unwrap();
    let context: ai::TranscriptContext =
        serde_json::from_value(json!({"messages": [{"role": "user", "content": "go", "timestamp": 0}]})).unwrap();
    let mut sink = |_: &ai::AssistantMessageEvent| {};
    let _ = bedrock::stream(&model, &context, &options, &transport, &mut sink).await;
    tokio::time::timeout(HANG_GUARD, seen.recv()).await.unwrap().unwrap()
}

fn fixture_handler(name: &str) -> String {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/bedrock-endpoint.json")).unwrap();
    let case = fixture["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == name)
        .unwrap();
    case["request"]["handler"].as_str().unwrap().to_string()
}

fn protocol_of(handler: &str) -> &'static str {
    match handler {
        "NodeHttp2Handler" => "h2",
        "NodeHttpHandler" => "http/1.1",
        other => panic!("unknown handler {other}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ai_bedrock_force_http1_negotiates_http1_like_typescript() {
    let handler = fixture_handler("AWS_BEDROCK_FORCE_HTTP1=1 uses HTTP/1.1");
    assert_eq!(negotiated(Some("1")).await, protocol_of(&handler));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ai_bedrock_default_and_non_one_values_negotiate_h2_like_typescript() {
    let handler = fixture_handler("AWS_BEDROCK_FORCE_HTTP1=true does not");
    assert_eq!(negotiated(Some("true")).await, protocol_of(&handler));
    let handler = fixture_handler("default endpoint for the region");
    assert_eq!(negotiated(None).await, protocol_of(&handler));
}

/// `bedrock::configured_endpoint_url` (the SDK's `getEndpointFromConfig`) for every fixture case whose client sets no
/// endpoint (standard base URL, a configured region): the recorded request went to the configured endpoint, or to
/// the region's default one when there is none.
#[test]
fn ai_bedrock_configured_endpoint_url_matches_typescript() {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/bedrock-endpoint.json")).unwrap();
    let mut checked = 0;
    for case in fixture["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let vars = case["env"].as_object().unwrap();
        if !vars.contains_key("AWS_REGION") || case["baseUrl"] != "https://bedrock-runtime.us-east-1.amazonaws.com" {
            continue;
        }
        checked += 1;
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config");
        let credentials = dir.path().join("credentials");
        std::fs::write(&config, case["config"].as_str().unwrap_or("")).unwrap();
        std::fs::write(&credentials, case["credentials"].as_str().unwrap_or("")).unwrap();
        let files_env: ai::env::ProviderEnv = [
            ("AWS_CONFIG_FILE".to_string(), config.display().to_string()),
            (
                "AWS_SHARED_CREDENTIALS_FILE".to_string(),
                credentials.display().to_string(),
            ),
        ]
        .into();
        let lookup = |key: &str| vars.get(key).and_then(Value::as_str).map(str::to_string);
        let got =
            bedrock::configured_endpoint_url(&lookup, &|| ai::aws_credentials::load_shared_files(Some(&files_env)));
        let url = case["request"]["url"].as_str().unwrap();
        let expected = url
            .starts_with("http://127.0.0.1")
            .then(|| url.split("/model/").next().unwrap().to_string());
        assert_eq!(got, expected, "{name}");
    }
    assert_eq!(checked, 15);
}
