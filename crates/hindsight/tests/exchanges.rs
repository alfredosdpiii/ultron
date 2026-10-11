//! Replays fixtures/exchanges.json (recorded from the TypeScript client by fixtures/generate.mjs) against the same
//! fake Hindsight server: each call's result or error code and every request the server received (request line,
//! headers other than the HTTP stack's own, body bytes) must equal TypeScript's.

mod common;

use common::{FakeHindsight, Normalizer, closed_port, error_text, fill, fixture, strings};
use hindsight::{Endpoint, HindsightBackend, HindsightLessons, HindsightOptions, hindsight_url};
use kernel::Signal;
use kernel_apis::{MemoryBackend, MemoryDocumentStore, MemoryService, worker_gate};
use review::ledger::{Fate, Lesson};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

fn options(value: &Value) -> HindsightOptions {
    let mut options = HindsightOptions::new(
        value["baseUrl"].as_str().unwrap(),
        value["bankId"].as_str().unwrap(),
        value["scopeTags"].as_object().unwrap().clone(),
    );
    if let Some(headers) = value["headers"].as_object() {
        options.headers = headers
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
            .collect();
    }
    options.timeout_ms = value["timeoutMs"].as_f64();
    options.max_response_bytes = value["maxResponseBytes"].as_f64();
    options.ensure_bank = value["ensureBank"] == true;
    options
}

fn call(backend: &HindsightBackend, call: &Value) -> Result<Value, String> {
    let signal = Signal::new();
    if call["abortBefore"] == true {
        signal.abort("aborted");
    }
    if let Some(ms) = call["abortAfterMs"].as_u64() {
        let signal = signal.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(ms));
            signal.abort("aborted");
        });
    }
    let arg = &call["arg"];
    let id = arg.as_str().unwrap_or_default();
    match call["op"].as_str().unwrap() {
        "recall" => backend.recall(arg, &signal),
        "retain" => backend.retain(arg, &signal),
        "get" => backend.get(id, &signal),
        "delete" => backend.delete(id, &signal),
        "operation" => backend.operation(id, &signal),
        other => panic!("unknown op {other}"),
    }
}

fn outcome(result: Result<Value, String>) -> Map<String, Value> {
    let mut out = Map::new();
    match result {
        Ok(value) => out.insert("result".into(), value),
        Err(error) => out.insert("error".into(), Value::String(error_text(&error))),
    };
    out
}

fn with(mut base: Map<String, Value>, extra: Map<String, Value>) -> Value {
    base.extend(extra);
    Value::Object(base)
}

#[test]
fn hindsight_backend_requests_and_results_match_typescript() {
    let fixture = fixture();
    let transport = strings(&fixture["transportHeaders"]);
    let server = FakeHindsight::start();
    let closed = closed_port();
    let mut failures = Vec::new();
    for expected in fixture["backend"].as_array().unwrap() {
        server.reset();
        let normalizer = Normalizer::new(server.port, closed);
        let scenario = fill(expected, &server.base(), closed);
        let created = HindsightBackend::new(options(&scenario["options"]));
        let created_record = match &created {
            Ok(backend) => json!({"namespace": backend.namespace(), "scopeTags": backend.scope_tags()}),
            Err(error) => json!({"error": error.code}),
        };
        let mut calls = Vec::new();
        for planned in scenario["calls"].as_array().unwrap() {
            let backend = created.as_ref().expect("calls only on created backends");
            calls.push(with(
                planned.as_object().unwrap().clone(),
                outcome(call(backend, planned)),
            ));
        }
        let slow = scenario["calls"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c.get("abortAfterMs").is_some())
            || scenario["options"]["bankId"]
                .as_str()
                .is_some_and(|b| b.contains("slow"));
        if slow {
            std::thread::sleep(Duration::from_millis(1600));
        }
        let record = normalizer.apply(&json!({
            "name": scenario["name"],
            "options": scenario["options"],
            "created": created_record,
            "calls": calls,
            "requests": server.requests(&transport),
        }));
        // The fixture holds the unfilled options; compare everything else.
        for key in ["created", "calls", "requests"] {
            if record[key] != expected[key] {
                failures.push(format!(
                    "{} / {key}:\n  rust: {}\n  ts:   {}",
                    expected["name"], record[key], expected[key]
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// What reqwest sends, all of it: the client's headers in order, then `accept: */*` and `host` (both stacks add
/// them; undici also adds the fixture's other transport headers), and `content-length` with a body. So the
/// comparison leaves out only headers that reqwest never sends but `host`.
#[test]
fn hindsight_transport_sends_only_client_headers_plus_host() {
    let fixture = fixture();
    let transport = strings(&fixture["transportHeaders"]);
    let server = FakeHindsight::start();
    let mut options = HindsightOptions::new(
        server.base(),
        "ultron",
        json!({"session": ["ultron:session:s1"]}).as_object().unwrap().clone(),
    );
    options.headers = vec![("X-Extra".into(), "  v  ".into()), ("x-extra".into(), "w".into())];
    let backend = HindsightBackend::new(options).unwrap();
    assert_eq!(backend.get("d", &Signal::new()), Err("HTTP_ERROR".into()));
    backend
        .recall(&json!({"query": "q", "tags": ["ultron:session:s1"]}), &Signal::new())
        .unwrap();
    let host = format!("127.0.0.1:{}", server.port);
    let raw = server.raw_requests();
    assert_eq!(
        raw[0]["headers"],
        json!([
            ["content-type", "application/json"],
            ["x-extra", "v, w"],
            ["accept", "*/*"],
            ["host", host]
        ])
    );
    assert_eq!(
        raw[1]["headers"],
        json!([
            ["content-type", "application/json"],
            ["x-extra", "v, w"],
            ["accept", "*/*"],
            ["host", host],
            ["content-length", "42"]
        ])
    );
    assert_eq!(raw[1]["body"], r#"{"query":"q","tags":["ultron:session:s1"]}"#);
    assert!(transport.contains(&"host".to_string()) && !transport.contains(&"accept".to_string()));
}

fn error_or_result(result: Result<Value, String>) -> Map<String, Value> {
    outcome(result)
}

#[test]
fn hindsight_worker_sessions_share_one_bank_like_typescript() {
    let fixture = fixture();
    let expected = &fixture["worker"];
    let transport = strings(&fixture["transportHeaders"]);
    let server = FakeHindsight::start();
    let closed = closed_port();
    server.reset();
    let normalizer = Normalizer::new(server.port, closed);
    let workers = fill(&expected["workers"], &server.base(), closed);
    let mut services: HashMap<String, Option<MemoryService>> = HashMap::new();
    for worker in workers.as_array().unwrap() {
        // What the composition root does: resolve the endpoint from the environment (no URL here: memory off, as
        // a worker created without hindsightUrl), then the session's backend.
        let url = worker["hindsightUrl"].as_str().unwrap_or("off").to_string();
        let bank = worker["bankId"].as_str().map(String::from);
        let var = move |key: &str| match key {
            "ULTRON_HINDSIGHT_URL" => Some(url.clone()),
            "ULTRON_HINDSIGHT_BANK" => bank.clone(),
            _ => None,
        };
        let service = Endpoint::resolve(&var, None).map(|endpoint| {
            let backend = endpoint
                .session_backend(
                    worker["sessionId"].as_str().unwrap(),
                    worker["cwd"].as_str().unwrap(),
                    "/home/fixture",
                )
                .unwrap();
            MemoryService::new(
                Arc::new(MemoryDocumentStore::new(None)),
                Arc::new(backend),
                worker_gate(),
            )
            .unwrap()
        });
        services.insert(worker["name"].as_str().unwrap().into(), service);
    }
    let mut ids: Vec<Value> = Vec::new();
    let mut steps = Vec::new();
    for step in expected["steps"].as_array().unwrap() {
        let (worker, kind, payload) = (
            step["worker"].as_str().unwrap(),
            step["type"].as_str().unwrap(),
            &step["payload"],
        );
        let mut resolved = payload.clone();
        if let Some(id) = resolved.get_mut("id")
            && let Some(index) = id.as_str().and_then(|s| s.strip_prefix('$'))
        {
            *id = ids[index.parse::<usize>().unwrap()].clone();
        }
        let result = kernel_apis::memory::handle(
            services[worker].as_ref(),
            kind,
            resolved.as_object().unwrap(),
            &Signal::new(),
        );
        if kind == "memory.propose"
            && let Ok(value) = &result
            && let Some(id) = value.get("memoryId")
        {
            ids.push(id.clone());
        }
        let head = json!({"worker": worker, "type": kind, "payload": payload});
        steps.push(with(head.as_object().unwrap().clone(), error_or_result(result)));
    }
    let record = normalizer.apply(&json!({
        "workers": expected["workers"],
        "steps": steps,
        "requests": server.requests(&transport),
    }));
    for (index, (rust, ts)) in record["steps"]
        .as_array()
        .unwrap()
        .iter()
        .zip(expected["steps"].as_array().unwrap())
        .enumerate()
    {
        assert_eq!(rust, ts, "step {index}");
    }
    assert_eq!(record["steps"], expected["steps"]);
    assert_eq!(record["requests"], expected["requests"]);
}

#[test]
fn hindsight_autoreview_lessons_match_typescript() {
    let fixture = fixture();
    let expected = &fixture["lessons"];
    let transport = strings(&fixture["transportHeaders"]);
    let server = FakeHindsight::start();
    let closed = closed_port();
    server.reset();
    let normalizer = Normalizer::new(server.port, closed);
    let fates: Vec<Fate> = serde_json::from_value(expected["fates"].clone()).unwrap();
    let lessons: Vec<Lesson> = expected["lessons"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| Lesson {
            category: l["category"].as_str().unwrap().into(),
            kind: l["kind"].as_str().unwrap().into(),
            accepted: l["accepted"].as_u64().unwrap() as u32,
            rejected: l["rejected"].as_u64().unwrap() as u32,
            lowered: l["lowered"].as_bool().unwrap(),
        })
        .collect();
    let mut cases = Vec::new();
    for case in expected["cases"].as_array().unwrap() {
        let base = fill(&case["baseUrl"], &server.base(), closed);
        let memory = HindsightLessons::new(base.as_str().unwrap(), Box::new(|| 1_760_000_000_000), None).unwrap();
        let mut record = json!({"name": case["name"], "baseUrl": case["baseUrl"], "repo": case["repo"]});
        if let Err(error) = memory.retain(case["repo"].as_str().unwrap(), &fates, &lessons) {
            record["error"] = json!(error.code);
            // What the reviewer logs: the error's message.
            assert_eq!(
                error.to_string(),
                format!("Memory operation did not complete ({})", error.code)
            );
        }
        cases.push(record);
    }
    let record = normalizer.apply(&json!({"cases": cases, "requests": server.requests(&transport)}));
    assert_eq!(record["cases"], expected["cases"]);
    assert_eq!(record["requests"], expected["requests"]);
}

#[test]
fn hindsight_url_resolution_matches_typescript() {
    for case in fixture()["urls"].as_array().unwrap() {
        let (env, setting) = (case["env"].as_str(), case["setting"].as_str());
        let url = hindsight_url(env, setting);
        assert_eq!(json!(url), case["worker"], "{case}");
        assert_eq!(json!(url), case["autoreview"], "{case}");
        let (effective, source) = hindsight::resolve::effective_hindsight_url(env, setting);
        let mut setup = Map::new();
        if let Some(effective) = effective {
            setup.insert("url".into(), json!(effective));
        }
        setup.insert("source".into(), json!(source));
        assert_eq!(Value::Object(setup), case["setup"], "{case}");
    }
}

/// `process.env.ULTRON_HINDSIGHT_BANK || "ultron"` (session-worker.ts): an empty bank is the default, any other
/// value is used as it is.
#[test]
fn hindsight_bank_from_env_like_typescript() {
    let resolve = |bank: Option<&str>| {
        let bank = bank.map(String::from);
        let var = move |key: &str| {
            if key == "ULTRON_HINDSIGHT_BANK" {
                bank.clone()
            } else {
                None
            }
        };
        Endpoint::resolve(&var, None).unwrap()
    };
    assert_eq!(resolve(None).bank, "ultron");
    assert_eq!(resolve(Some("")).bank, "ultron");
    assert_eq!(resolve(Some(" team ")).bank, " team ");
    assert_eq!(resolve(None).url, "http://localhost:8888");
}
