//! Replays `fixtures/google-vertex-adc.json`, recorded from the TypeScript google-vertex provider in application default
//! credentials mode by `fixtures/generate-vertex-adc.mjs`: the same credential files (in a fresh temp directory, which
//! is also HOME) and the same canned token, metadata and Vertex responses go through the Rust port, which must send the
//! same requests, emit the same events and return the same final message.
//!
//! Normalization, each mirroring something upstream does outside the provider: a form body's `content-type` is dropped
//! (`fetch` derives it from the `URLSearchParams` body, so the recording never sees it); a JWT assertion is compared by
//! its decoded header and payload with `iat`/`exp` checked for a one-hour lifetime (the generator pins the clock, this
//! test does not), and its signature is checked separately at the pinned time. A self-signed bearer JWT (a service
//! account outside the default universe) is compared the same way, and an AWS subject token (a SigV4-signed request)
//! by its JSON with the date and signature blanked, its exact bytes checked separately at the pinned time.

use ai::google_auth::{AwsCredentials, aws_subject_token, self_signed_jwt, service_account_assertion, sha256_hex};
use ai::google_vertex::{self, GoogleVertexOptions};
use ai::http::{BoxFuture, HeaderMap};
use ai::json::normalize_numbers;
use ai::options::pi_user_agent;
use ai::{
    AssistantMessageEvent, Body, HttpRequest, HttpResponse, Model, SimpleStreamOptions, TranscriptContext, Transport,
};
use serde_json::{Map, Value, json};
use std::sync::{Arc, LazyLock, Mutex};

static FIXTURE: LazyLock<Value> =
    LazyLock::new(|| serde_json::from_str(include_str!("../fixtures/google-vertex-adc.json")).expect("fixture parses"));

enum Canned {
    Fail,
    Respond {
        status: u16,
        headers: HeaderMap,
        chunks: Vec<Vec<u8>>,
    },
}

struct FixtureBody(std::vec::IntoIter<Vec<u8>>);

impl Body for FixtureBody {
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, String>> {
        let next = self.0.next();
        Box::pin(async move { Ok(next) })
    }
}

/// A request and the client identity it went out with.
type Sent = (HttpRequest, Option<Value>);

#[derive(Clone, Default)]
struct FixtureTransport {
    queue: Arc<Mutex<Vec<Canned>>>,
    /// Each request with the client identity (sha256 of the certificate and key files) it was sent with.
    requests: Arc<Mutex<Vec<Sent>>>,
    identity: Option<Value>,
}

impl Transport for FixtureTransport {
    fn with_client_identity(&self, cert: &[u8], key: &[u8]) -> Result<Arc<dyn Transport>, String> {
        let digest = |bytes: &[u8]| sha256_hex(&String::from_utf8_lossy(bytes));
        Ok(Arc::new(FixtureTransport {
            identity: Some(json!({"certSha256": digest(cert), "keySha256": digest(key)})),
            ..self.clone()
        }))
    }

    fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, String>> {
        self.requests.lock().unwrap().push((request, self.identity.clone()));
        let canned = {
            let mut queue = self.queue.lock().unwrap();
            if queue.is_empty() { None } else { Some(queue.remove(0)) }
        };
        Box::pin(async move {
            match canned.ok_or("unexpected request")? {
                Canned::Fail => Err("connection refused".into()),
                Canned::Respond {
                    status,
                    headers,
                    chunks,
                } => Ok(HttpResponse {
                    status,
                    headers,
                    body: Box::new(FixtureBody(chunks.into_iter())),
                }),
            }
        })
    }
}

fn canned(response: &Value) -> Canned {
    if response.get("fail").is_some() {
        return Canned::Fail;
    }
    let headers = match response.get("headers").and_then(Value::as_object) {
        Some(headers) => headers
            .iter()
            .map(|(k, v)| (k.to_lowercase(), v.as_str().unwrap().to_string()))
            .collect(),
        None => [("content-type".to_string(), "text/event-stream".to_string())].into(),
    };
    Canned::Respond {
        status: response.get("status").and_then(Value::as_u64).unwrap_or(200) as u16,
        headers,
        chunks: response["chunks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c.as_str().unwrap().as_bytes().to_vec())
            .collect(),
    }
}

fn pem(der: &str) -> String {
    let lines: Vec<&str> = der
        .as_bytes()
        .chunks(64)
        .map(|c| std::str::from_utf8(c).unwrap())
        .collect();
    let label = ["PRIVATE", "KEY"].join(" ");
    format!("-----BEGIN {label}-----\n{}\n-----END {label}-----\n", lines.join("\n"))
}

fn cert_pem(der: &str) -> String {
    let lines: Vec<&str> = der
        .as_bytes()
        .chunks(64)
        .map(|c| std::str::from_utf8(c).unwrap())
        .collect();
    format!(
        "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
        lines.join("\n")
    )
}

/// The generator's `<dir>`, `<private-key>` and certificate placeholders, filled in.
fn fill(value: &Value, dir: &str) -> Value {
    let certificate = |name: &str| FIXTURE["certificates"][name].as_str().unwrap();
    match value {
        Value::String(s) => json!(
            s.replace("<dir>", dir)
                .replace("<private-key>", &pem(FIXTURE["privateKeyDer"].as_str().unwrap()))
                .replace("<leaf-cert>", &cert_pem(certificate("leaf")))
                .replace("<other-cert>", &cert_pem(certificate("other")))
                .replace("<leaf-key>", &pem(certificate("leafKey")))
        ),
        Value::Array(items) => Value::Array(items.iter().map(|v| fill(v, dir)).collect()),
        Value::Object(map) => Value::Object(map.iter().map(|(k, v)| (k.clone(), fill(v, dir))).collect()),
        other => other.clone(),
    }
}

fn scrub(value: &Value, dir: &str) -> Value {
    serde_json::from_str(&serde_json::to_string(value).unwrap().replace(dir, "<dir>")).unwrap()
}

fn url_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' => {
                out.push(u8::from_str_radix(&text[i + 1..i + 3], 16).unwrap());
                i += 2;
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8(out).unwrap()
}

fn b64url_json(part: &str) -> Value {
    let standard = part.replace('-', "+").replace('_', "/");
    let padded = format!("{standard}{}", "=".repeat((4 - standard.len() % 4) % 4));
    serde_json::from_slice(&ai::jsutil::atob(&padded).unwrap()).unwrap()
}

/// A JWT assertion as the generator records it.
fn decompose(assertion: &str) -> Value {
    let parts: Vec<&str> = assertion.split('.').collect();
    json!({"header": b64url_json(parts[0]), "payload": b64url_json(parts[1]), "signatureSha256": sha256_hex(parts[2])})
}

/// A decoded JWT's `iat`/`exp` and signature depend on the clock: check the lifetime, then blank them.
fn unclock_jwt(jwt: &mut Value) {
    let payload = &mut jwt["payload"];
    let (iat, exp) = (payload["iat"].as_i64().unwrap(), payload["exp"].as_i64().unwrap());
    assert_eq!(exp - iat, 3600, "JWT lifetime");
    payload["iat"] = json!("<iat>");
    payload["exp"] = json!("<exp>");
    jwt["signatureSha256"] = json!("<signature>");
}

/// An AWS subject token's `x-amz-date`, credential scope date and signature depend on the clock.
fn unclock_aws(request: &mut Value) {
    for header in request["headers"].as_array_mut().unwrap() {
        let value = header["value"].as_str().unwrap().to_string();
        match header["key"].as_str().unwrap() {
            "x-amz-date" => {
                assert_eq!(value.len(), 16, "amz date {value}");
                header["value"] = json!("<amz-date>");
            }
            "authorization" => {
                let (head, _) = value.split_once(", Signature=").unwrap();
                let (credential, rest) = head.split_once('/').unwrap();
                let (_, scope) = rest.split_once('/').unwrap();
                header["value"] = json!(format!("{credential}/<day>/{scope}, Signature=<signature>"));
            }
            _ => {}
        }
    }
}

fn unclock(request: &mut Value) {
    if let Some(jwt) = request["headers"]
        .get_mut("authorization")
        .and_then(|a| a.get_mut("bearerJwt"))
    {
        unclock_jwt(jwt);
    }
    let Some(form) = request["body"].get_mut("form").and_then(Value::as_array_mut) else {
        return;
    };
    for pair in form {
        if pair[0] == "assertion" {
            unclock_jwt(&mut pair[1]);
        }
        if let Some(aws) = pair[1].get_mut("awsRequest") {
            unclock_aws(aws);
        }
    }
}

/// `decodeURIComponent`.
fn uri_decode(text: &str) -> String {
    url_decode(&text.replace('+', "%2B"))
}

fn record_request((request, identity): &Sent) -> Value {
    let user_agent = pi_user_agent();
    let form = request
        .headers
        .get("content-type")
        .is_some_and(|ct| ct.contains("x-www-form-urlencoded"));
    let headers: Map<String, Value> = request
        .headers
        .iter()
        .filter(|(k, _)| !(form && k.as_str() == "content-type"))
        .map(|(k, v)| {
            let jwt = v.strip_prefix("Bearer ").filter(|t| t.split('.').count() == 3);
            match jwt {
                Some(jwt) if k == "authorization" => (k.clone(), json!({ "bearerJwt": decompose(jwt) })),
                _ => (k.clone(), json!(v.replace(&user_agent, "<pi-user-agent>"))),
            }
        })
        .collect();
    let body = if request.body.is_empty() {
        Value::Null
    } else if form {
        let pairs: Vec<Value> = request
            .body
            .split('&')
            .map(|pair| {
                let (k, v) = pair.split_once('=').unwrap();
                let (k, v) = (url_decode(k), url_decode(v));
                if k == "assertion" {
                    json!([k, decompose(&v)])
                } else if k == "subject_token" && v.starts_with("%7B") {
                    json!([k, { "awsRequest": serde_json::from_str::<Value>(&uri_decode(&v)).unwrap() }])
                } else {
                    json!([k, v])
                }
            })
            .collect();
        json!({ "form": pairs })
    } else {
        serde_json::from_str(&request.body).unwrap()
    };
    let mut recorded = json!({"url": request.url, "method": request.method, "headers": headers, "body": body});
    if let Some(identity) = identity {
        recorded["clientIdentity"] = identity.clone();
    }
    recorded
}

const SCRATCH_KEYS: &[&str] = &["index", "partialJson", "partialArgs", "customInput", "streamIndex"];

fn normalize_event(event: &mut Value, now: &Value) {
    for key in ["partial", "message", "error"] {
        if let Some(message) = event.get_mut(key) {
            normalize_message(message, now);
        }
    }
}

fn normalize_message(message: &mut Value, now: &Value) {
    if let Some(object) = message.as_object_mut() {
        object.insert("timestamp".into(), now.clone());
        if let Some(Value::Array(content)) = object.get_mut("content") {
            for block in content {
                if let Some(block) = block.as_object_mut() {
                    for scratch in SCRATCH_KEYS {
                        block.shift_remove(*scratch);
                    }
                }
            }
        }
    }
}

struct Outcome {
    requests: Vec<Value>,
    events: Vec<Value>,
    result: Value,
}

/// Provider env lookups fall back to the process env; with these set, a scenario could read the machine's real
/// credentials. Refuse to run rather than read them.
fn assert_isolated_process_env() {
    for name in [
        "GOOGLE_APPLICATION_CREDENTIALS",
        "google_application_credentials",
        "GOOGLE_CLOUD_PROJECT",
        "GCLOUD_PROJECT",
        "GOOGLE_CLOUD_LOCATION",
        "GOOGLE_CLOUD_QUOTA_PROJECT",
        "GCE_METADATA_HOST",
        "GCE_METADATA_IP",
        "METADATA_SERVER_DETECTION",
        "GOOGLE_VERTEX_BASE_URL",
        "GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES",
        "GOOGLE_API_CERTIFICATE_CONFIG",
        "CLOUDSDK_CONFIG",
        "AWS_REGION",
        "AWS_DEFAULT_REGION",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
    ] {
        assert!(
            std::env::var_os(name).is_none(),
            "{name} is set in the test process; unset it so the Vertex ADC replay cannot reach real credentials"
        );
    }
}

fn run(scenario: &Value) -> Outcome {
    assert_isolated_process_env();
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().to_str().unwrap().to_string();
    for (path, content) in scenario["files"].as_object().unwrap() {
        let file = temp.path().join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let text = match fill(content, &dir) {
            Value::String(text) => text,
            other => serde_json::to_string_pretty(&other).unwrap(),
        };
        std::fs::write(file, text).unwrap();
    }
    for path in scenario["executables"].as_array().into_iter().flatten() {
        use std::os::unix::fs::PermissionsExt;
        let file = temp.path().join(path.as_str().unwrap());
        std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut options = fill(&scenario["options"], &dir);
    let mut env = options.get("env").cloned().unwrap_or_else(|| json!({}));
    for (name, value) in scenario["processEnv"].as_object().unwrap() {
        if let Value::String(value) = fill(value, &dir) {
            env[name] = json!(value);
        }
    }
    options["env"] = env;
    let model: Model = serde_json::from_value(scenario["model"].clone()).unwrap();
    let context: TranscriptContext = serde_json::from_value(scenario["context"].clone()).unwrap();
    let transport = FixtureTransport::default();
    *transport.queue.lock().unwrap() = scenario["responses"].as_array().unwrap().iter().map(canned).collect();
    let mut events = Vec::new();
    let mut sink = |event: &AssistantMessageEvent| events.push(serde_json::to_value(event).unwrap());
    // IO for the pluggable-auth executable's pipe and child.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let message = runtime.block_on(async {
        if scenario["fn"] == "streamSimple" {
            let options: SimpleStreamOptions = serde_json::from_value(options).unwrap();
            google_vertex::stream_simple(&model, &context, &options, &transport, &mut sink)
                .await
                .unwrap()
        } else {
            let options: GoogleVertexOptions = serde_json::from_value(options).unwrap();
            google_vertex::stream(&model, &context, &options, &transport, &mut sink).await
        }
    });
    let requests = transport.requests.lock().unwrap().iter().map(record_request).collect();
    Outcome {
        requests: requests_scrubbed(requests, &dir),
        events: events.iter().map(|e| scrub(e, &dir)).collect(),
        result: scrub(&serde_json::to_value(message).unwrap(), &dir),
    }
}

fn requests_scrubbed(requests: Vec<Value>, dir: &str) -> Vec<Value> {
    requests.iter().map(|r| scrub(r, dir)).collect()
}

fn differences(scenario: &Value) -> Vec<String> {
    let name = scenario["name"].as_str().unwrap();
    let now = FIXTURE["now"].clone();
    let rust = run(scenario);
    let mut diffs = Vec::new();
    let mut check = |what: &str, expected: &Value, actual: &Value| {
        let (expected, actual) = (normalize_numbers(expected.clone()), normalize_numbers(actual.clone()));
        if expected != actual {
            diffs.push(format!("{name}: {what}\n  expected {expected}\n  actual   {actual}"));
        }
    };
    let expected_requests = scenario["requests"].as_array().unwrap();
    check(
        "request count",
        &json!(expected_requests.len()),
        &json!(rust.requests.len()),
    );
    for (i, (expected, actual)) in expected_requests.iter().zip(&rust.requests).enumerate() {
        let (mut expected, mut actual) = (expected.clone(), actual.clone());
        unclock(&mut expected);
        unclock(&mut actual);
        for part in ["url", "method", "headers", "body", "clientIdentity"] {
            check(&format!("request {i} {part}"), &expected[part], &actual[part]);
        }
    }
    let expected_events = scenario["events"].as_array().unwrap();
    check(
        "event types",
        &json!(expected_events.iter().map(|e| e["type"].clone()).collect::<Vec<_>>()),
        &json!(rust.events.iter().map(|e| e["type"].clone()).collect::<Vec<_>>()),
    );
    for (i, (expected, actual)) in expected_events.iter().zip(&rust.events).enumerate() {
        let (mut expected, mut actual) = (expected.clone(), actual.clone());
        normalize_event(&mut expected, &now);
        normalize_event(&mut actual, &now);
        check(&format!("event {i}"), &expected, &actual);
    }
    let (mut expected, mut actual) = (scenario["result"].clone(), rust.result);
    normalize_message(&mut expected, &now);
    normalize_message(&mut actual, &now);
    check("result", &expected, &actual);
    diffs
}

#[test]
fn ai_google_vertex_adc_replays_recorded_typescript_scenarios() {
    let mut failures = Vec::new();
    for scenario in FIXTURE["scenarios"].as_array().unwrap() {
        failures.extend(differences(scenario));
    }
    assert!(
        failures.is_empty(),
        "{} differences:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// gtoken's RS256 assertion at the generator's pinned clock is byte-identical (PKCS#1 v1.5 is deterministic).
#[test]
fn ai_google_auth_service_account_assertion_matches_typescript_signature() {
    let scenario = FIXTURE["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "adc/service-account-key-file-global")
        .unwrap();
    let recorded = &scenario["requests"][0]["body"]["form"][1][1];
    let iat = recorded["payload"]["iat"].as_i64().unwrap();
    let assertion = service_account_assertion(
        "bot@sa-project.iam.gserviceaccount.com",
        &pem(FIXTURE["privateKeyDer"].as_str().unwrap()),
        iat,
    )
    .unwrap();
    assert_eq!(&decompose(&assertion), recorded);
}

#[test]
fn ai_google_auth_rejects_an_undecodable_private_key_like_node() {
    assert_eq!(
        service_account_assertion("a@b", "not a key", 0).unwrap_err(),
        "error:1E08010C:DECODER routines::unsupported"
    );
}

fn scenario(name: &str) -> &'static Value {
    FIXTURE["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == name)
        .unwrap()
}

/// `JWTAccess`'s self-signed token at the generator's pinned clock is byte-identical, with and without a key id.
#[test]
fn ai_google_auth_self_signed_jwt_matches_typescript_signature() {
    for (name, kid) in [
        ("universe/service-account-self-signed-jwt", Some("kid-1")),
        ("universe/service-account-self-signed-jwt-adc-no-key-id", None),
    ] {
        let recorded = &scenario(name)["requests"][0]["headers"]["authorization"]["bearerJwt"];
        let iat = recorded["payload"]["iat"].as_i64().unwrap();
        let jwt = self_signed_jwt(
            "bot@sa-project.iam.gserviceaccount.com",
            &pem(FIXTURE["privateKeyDer"].as_str().unwrap()),
            kid,
            iat,
        )
        .unwrap();
        assert_eq!(&decompose(&jwt), recorded, "{name}");
    }
}

/// The SigV4-signed GetCallerIdentity subject token at the generator's pinned clock is byte-identical (HMAC is
/// deterministic), with and without a session token.
#[test]
fn ai_google_auth_aws_subject_token_matches_typescript_signature() {
    let now = FIXTURE["now"].as_i64().unwrap();
    let audience = "//iam.googleapis.com/projects/123456/locations/global/workloadIdentityPools/pool/providers/prov";
    for (name, region, key, secret, token) in [
        (
            "wif/aws-env-credentials",
            "us-east-2",
            "AKIDEXAMPLE",
            "fake-secret-key",
            Some("fake-session"),
        ),
        (
            "wif/aws-env-default-region-no-token",
            "eu-west-1",
            "AKIDEXAMPLE",
            "fake-secret-key",
            None,
        ),
        (
            "wif/aws-metadata-imdsv2",
            "us-west-2",
            "AKIDMETA",
            "meta-secret",
            Some("meta-token"),
        ),
    ] {
        let sts = scenario(name)["requests"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["url"] == "https://sts.googleapis.com/v1/token")
            .unwrap();
        let recorded = &sts["body"]["form"][4][1]["awsRequest"];
        let credentials = AwsCredentials {
            access_key_id: key.into(),
            secret_access_key: secret.into(),
            token: token.map(str::to_string),
        };
        let url = format!("https://sts.{region}.amazonaws.com?Action=GetCallerIdentity&Version=2011-06-15");
        let encoded = aws_subject_token(&url, region, &credentials, audience, now).unwrap();
        let decoded: Value = serde_json::from_str(&uri_decode(&encoded)).unwrap();
        assert_eq!(&decoded, recorded, "{name}");
    }
}
