//! Replays crates/inference/fixtures/infer.json, recorded by fixtures/generate.mjs from the TypeScript host
//! (NativeRlmHost + InferenceRuntime of inference.ts) serving the real runtime.py: the same cells run through
//! crates/kernel with `Inference` (frames call crates/ai's openai-completions provider over a scripted transport)
//! and must print and return the same, and send the same prompts with the same output caps, request by request.
//! The typebox Check/Errors and validateJsonSchema tables of the fixture are replayed against `schema`.

mod common;

use common::{Scripted, model};
use inference::{Inference, schema};
use kernel::{Kernel, KernelOptions};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn fixture() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/infer.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn runtime() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/src/ultron/rlm/runtime.py")
}

fn text_of(content: &Value) -> Value {
    match content {
        Value::Array(parts) => Value::String(
            parts
                .iter()
                .filter_map(|p| p["text"].as_str())
                .collect::<Vec<_>>()
                .join(""),
        ),
        Value::Null => Value::String(String::new()),
        other => other.clone(),
    }
}

/// A recorded chat-completions body as the fixture's `{system, messages, max_tokens}`.
fn call_of(body: &Value) -> Value {
    let messages = body["messages"].as_array().unwrap();
    let rest: Vec<Value> = messages[1..]
        .iter()
        .map(|m| json!({"role": m["role"], "content": text_of(&m["content"])}))
        .collect();
    let max = body
        .get("max_completion_tokens")
        .or_else(|| body.get("max_tokens"))
        .cloned()
        .unwrap_or(Value::Null);
    json!({"system": text_of(&messages[0]["content"]), "messages": rest, "max_tokens": max})
}

fn normalize(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find("frame-") {
        out.push_str(&rest[..at]);
        let tail = &rest[at + 6..];
        let id = tail
            .bytes()
            .take(36)
            .filter(|b| b.is_ascii_hexdigit() || *b == b'-')
            .count();
        if id == 36 {
            out.push_str("frame-ID");
            rest = &tail[36..];
        } else {
            out.push_str("frame-");
            rest = tail;
        }
    }
    out + rest
}

fn run_cell(cell: &Value, recorded_model: &Value) -> (Value, Vec<Value>) {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    let transport = Arc::new(Scripted::default());
    *transport.replies.lock().unwrap() = cell["replies"].as_array().unwrap().clone();
    let inner = Arc::new(host::Host::new(&cwd, cwd.join(".jobs"), &cwd, Box::new(host::AllowAll)));
    let handler = Inference::new(
        inner,
        model(recorded_model),
        Some("test-key".into()),
        transport.clone(),
        cwd.join("context"),
    );
    let mut options = KernelOptions::new(&cwd, runtime());
    options.env.push(("ULTRON_RLM_SPILL_DIR".into(), String::new()));
    options.env.push(("TMPDIR".into(), cwd.to_string_lossy().into_owned()));
    let kernel = Kernel::new(options, Arc::new(handler)).unwrap();
    let result = kernel.execute(cell["code"].as_str().unwrap(), None, None).unwrap();
    kernel.shutdown();
    let error = result.error.map(|error| format!("{}: {}", error.ename, error.evalue));
    let ok = json!({"status": result.status, "stdout": result.stdout, "result": result.result.unwrap_or_default(), "error": error});
    let ok: Value = serde_json::from_str(&normalize(&ok.to_string())).unwrap();
    assert!(
        transport.replies.lock().unwrap().is_empty(),
        "replies left over: {}",
        cell["code"]
    );
    let calls = transport.bodies.lock().unwrap().iter().map(call_of).collect();
    (ok, calls)
}

/// The fixture's lanes record the transcript; on the wire the openai-completions provider (TS
/// packages/ai/src/api/openai-completions.ts "Skip assistant messages that have no content", and crates/ai) drops an
/// assistant message with empty text.
fn on_the_wire(call: &Value) -> Value {
    let mut call = call.clone();
    call["messages"]
        .as_array_mut()
        .unwrap()
        .retain(|m| !(m["role"] == "assistant" && m["content"] == ""));
    call
}

#[test]
fn inference_infer_cells_match_typescript_host() {
    let fixture = fixture();
    for cell in fixture["cells"].as_array().unwrap() {
        let (ok, calls) = run_cell(cell, &fixture["model"]);
        assert_eq!(ok, cell["ok"], "cell result: {}", cell["code"]);
        let expected: Vec<Value> = cell["calls"].as_array().unwrap().iter().map(on_the_wire).collect();
        assert_eq!(calls, expected, "provider requests: {}", cell["code"]);
    }
}

#[test]
fn inference_contract_errors_match_typebox() {
    for case in fixture()["checks"].as_array().unwrap() {
        let errors = schema::errors(&case["schema"], &case["value"]);
        assert_eq!(errors.is_empty(), case["ok"].as_bool().unwrap(), "{case}");
        assert_eq!(json!(errors), case["errors"], "{case}");
    }
}

#[test]
fn inference_contract_schema_validation_matches_typescript() {
    for case in fixture()["schemas"].as_array().unwrap() {
        let result = schema::validate_schema(&case["schema"], "$.contract");
        match case.get("error") {
            Some(error) => assert_eq!(result, Err(error.as_str().unwrap().to_string()), "{case}"),
            None => assert_eq!(result, Ok(()), "{case}"),
        }
    }
}
