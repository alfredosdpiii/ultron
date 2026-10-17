//! Bedrock through a proxy (`api/bedrock-converse-stream.ts`: `resolveHttpProxyUrlForTarget(model.baseUrl,
//! options.env)` names one, so the client's handler is NodeHttpHandler, HTTP/1.1, instead of NodeHttp2Handler),
//! replayed from `crates/ai/fixtures/bedrock-proxy.json` (`generate-bedrock-proxy.mjs`) against a 127.0.0.1 TLS
//! server that offers h2 and http/1.1 and reports what the client negotiated (ALPN). The proxy variables are the
//! request env's; the connection itself is the transport's (here direct), so only the protocol choice is observed.
//! A proxy that cannot be used (SOCKS, an unparsable URL) is an unhandled rejection in TypeScript (the stream never
//! ends); here it ends the message with that rejection's text.

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

async fn run(env: &Value) -> (ai::AssistantMessage, Option<String>) {
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
    let mut env = env.clone();
    env["AWS_BEDROCK_SKIP_AUTH"] = json!("1");
    let options: BedrockOptions = serde_json::from_value(json!({"maxRetries": 0, "env": env})).unwrap();
    let context: ai::TranscriptContext =
        serde_json::from_value(json!({"messages": [{"role": "user", "content": "go", "timestamp": 0}]})).unwrap();
    let mut sink = |_: &ai::AssistantMessageEvent| {};
    let message = bedrock::stream(&model, &context, &options, &transport, &mut sink).await;
    // The message ended: a handshake it made has been reported already.
    let alpn = seen.try_recv().ok();
    (message, alpn)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ai_bedrock_proxy_uses_http1_like_typescript() {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/bedrock-proxy.json")).unwrap();
    let cases = fixture["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 9);
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let (message, alpn) = tokio::time::timeout(HANG_GUARD, run(&case["env"])).await.unwrap();
        match case["handler"].as_str() {
            Some(handler) => {
                let expected = match handler {
                    "NodeHttp2Handler" => "h2",
                    "NodeHttpHandler" => "http/1.1",
                    other => panic!("unknown handler {other}"),
                };
                assert_eq!(alpn.as_deref(), Some(expected), "{name}: {:?}", message.error_message);
            }
            None => {
                assert_eq!(alpn, None, "{name}");
                assert_eq!(
                    message.error_message.as_ref().map(|m| json!(m)),
                    Some(case["unhandled"].clone()),
                    "{name}"
                );
            }
        }
    }
}
