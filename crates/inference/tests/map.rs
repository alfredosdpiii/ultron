//! Replays crates/inference/fixtures/map.json, recorded by fixtures/map.mjs from the TypeScript host (NativeRlmHost +
//! InferenceRuntime of inference.ts, scripted frame lanes) serving the real runtime.py: `rlm.map`, `rlm._models` and
//! the frame-model defaults of `rlm.infer` run through crates/kernel with `Frames` over a scripted openai-completions
//! transport and must print the same and send the same prompts with the same output caps (tranches).

mod common;

use common::{Scripted, model};
use inference::{Frames, INFERENCE_PROMPT, Inference, ModelSettings};
use kernel::{HostHandler, Kernel, KernelOptions};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn fixture() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/map.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn runtime() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/src/ultron/rlm/runtime.py")
}

fn text_of(content: &Value) -> Value {
    match content {
        Value::Array(parts) => Value::String(parts.iter().filter_map(|p| p["text"].as_str()).collect()),
        other => other.clone(),
    }
}

fn call_of(body: &Value) -> Value {
    let messages = body["messages"].as_array().unwrap();
    let rest: Vec<Value> = messages[1..]
        .iter()
        .map(|m| json!({"role": m["role"], "content": text_of(&m["content"])}))
        .collect();
    json!({"system": text_of(&messages[0]["content"]), "messages": rest, "max_tokens": body["max_completion_tokens"]})
}

/// Trace and budget ids are random on both sides.
fn normalize(text: &str) -> String {
    normalize_ids(&normalize_ids(text, "frame-"), "budget-")
}

fn normalize_ids(text: &str, prefix: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find(prefix) {
        out.push_str(&rest[..at + prefix.len()]);
        rest = &rest[at + prefix.len()..];
        let id = rest
            .bytes()
            .take(36)
            .filter(|b| b.is_ascii_hexdigit() || *b == b'-')
            .count();
        if id == 36 {
            out.push_str("ID");
            rest = &rest[36..];
        }
    }
    out + rest
}

fn settings_of(cell: &Value) -> ModelSettings {
    let text = |v: &Value| v.as_str().map(str::to_string);
    let s = &cell["settings"];
    ModelSettings {
        frame_model: text(&s["rlm"]["frameModel"]),
        frame_thinking: text(&s["rlm"]["frameThinking"]),
        child_model: text(&s["rlm"]["childModel"]),
        review_model: text(&s["reviewModel"]),
    }
}

fn run_cell(cell: &Value, recorded_model: &Value) -> (Value, Vec<Value>) {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    let transport = Arc::new(Scripted::default());
    // `fail` replies are requests that never reach the provider (here: no API key).
    *transport.replies.lock().unwrap() = cell["replies"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r.get("fail").is_none())
        .cloned()
        .collect();
    let inner = Arc::new(host::Host::new(&cwd, cwd.join(".jobs"), &cwd, Box::new(host::AllowAll)));
    let key = (cell.get("noKey").is_none()).then(|| "test-key".to_string());
    let inference = Inference::new(
        inner,
        model(recorded_model),
        key,
        transport.clone(),
        cwd.join("context"),
    );
    let env: HashMap<String, String> = cell["env"]
        .as_object()
        .map(|env| {
            env.iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
                .collect()
        })
        .unwrap_or_default();
    let settings = settings_of(cell);
    let handler = Frames::new(inference, env, move || settings.clone());
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

#[test]
fn inference_map_cells_match_typescript_host() {
    let fixture = fixture();
    for cell in fixture["cells"].as_array().unwrap() {
        let (ok, calls) = run_cell(cell, &fixture["model"]);
        assert_eq!(ok, cell["ok"], "cell result: {}", cell["code"]);
        let mut calls = calls;
        if cell["replies"][0].get("gather").is_some() {
            // Concurrent frames claim in thread order; the recorded tranches shrink in claim order.
            calls.sort_by_key(|call| std::cmp::Reverse(call["max_tokens"].as_u64()));
        }
        assert_eq!(json!(calls), cell["calls"], "provider requests: {}", cell["code"]);
    }
}

#[test]
fn inference_prompt_matches_typescript() {
    assert_eq!(INFERENCE_PROMPT, fixture()["inferencePrompt"].as_str().unwrap());
}

fn frames(
    env: &[(&str, &str)],
    settings: ModelSettings,
    reasoning: bool,
) -> (Frames, Arc<Scripted>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let transport = Arc::new(Scripted::default());
    let mut model = model(&fixture()["model"]);
    model.reasoning = reasoning;
    let inner = Arc::new(kernel::EchoHandler);
    let inference = Inference::new(
        inner,
        model,
        Some("k".into()),
        transport.clone(),
        dir.path().join("context"),
    );
    let env = env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    (Frames::new(inference, env, move || settings.clone()), transport, dir)
}

fn call(handler: &Frames, kind: &str, payload: Value) -> Value {
    handler
        .handle(kind, payload.as_object().unwrap().clone(), &kernel::Signal::new())
        .unwrap()
}

/// Ported from ultron-claude-code-frames.test.ts "ULTRON_RLM_FRAME_MODEL is the model of code-free frames that name
/// none": the trimmed env model is the frame's model ("haiku" is not provider/model and is ignored). This host has one
/// model, so another frame model is refused at admission instead of routed.
#[test]
fn inference_map_frame_model_env_names_the_frame_model() {
    let frame = json!({"frames": [{"task": "cat in French", "context": []}]});
    let (handler, _, _dir) = frames(
        &[("ULTRON_RLM_FRAME_MODEL", " claude-code/haiku ")],
        ModelSettings::default(),
        false,
    );
    let reply = call(&handler, "rlm.map", frame.clone());
    assert_eq!(
        reply["results"][0]["error"],
        "Admission refused: model claude-code/haiku is not available to this host"
    );
    let reply = call(&handler, "rlm.infer", json!({"task": "cat in French"}));
    assert_eq!(
        reply["error"],
        "Admission refused: model claude-code/haiku is not available to this host"
    );
    let (handler, transport, _dir) = frames(&[("ULTRON_RLM_FRAME_MODEL", "haiku")], ModelSettings::default(), false);
    transport
        .replies
        .lock()
        .unwrap()
        .push(json!({"text": "chat", "usage": {"input": 5, "output": 1, "cacheRead": 0}}));
    assert_eq!(call(&handler, "rlm.map", frame)["results"][0]["value"], "chat");
}

/// inference.ts options(): "The call's own `thinking=` wins over `rlm.frameThinking`"; a reasoning model gets the level.
#[test]
fn inference_frame_thinking_setting_is_the_default_level() {
    let settings = ModelSettings {
        frame_thinking: Some("high".into()),
        ..ModelSettings::default()
    };
    let (handler, transport, _dir) = frames(&[], settings, true);
    for _ in 0..3 {
        transport
            .replies
            .lock()
            .unwrap()
            .push(json!({"text": "ok", "usage": {"input": 5, "output": 1, "cacheRead": 0}}));
    }
    call(&handler, "rlm.infer", json!({"task": "t"}));
    call(&handler, "rlm.infer", json!({"task": "t", "thinking": "low"}));
    call(&handler, "rlm.map", json!({"frames": [{"task": "t"}]}));
    let efforts: Vec<Value> = transport
        .bodies
        .lock()
        .unwrap()
        .iter()
        .map(|b| b["reasoning_effort"].clone())
        .collect();
    assert_eq!(efforts, [json!("high"), json!("low"), json!("high")]);
}

/// inference.ts map(): `concurrency` defaults to 8 on the host too, so frames hold at once and each tranche is at most
/// a quarter of what the others left (the TS recording of the same three frames: 750, 502, 316 of 3,000 tokens).
#[test]
fn inference_map_frames_run_concurrently_without_oversubscribing_the_pool() {
    let (handler, transport, _dir) = frames(&[], ModelSettings::default(), false);
    for _ in 0..3 {
        transport
            .replies
            .lock()
            .unwrap()
            .push(json!({"text": "S", "usage": {"input": 100, "output": 5, "cacheRead": 0}, "gather": 3}));
    }
    let item = json!({"task": "Same", "context": [{"kind": "text", "label": "literal", "text": "a"}]});
    let reply = call(
        &handler,
        "rlm.map",
        json!({"frames": [item, item, item], "budget": {"tokens": 3000}}),
    );
    let mut tranches: Vec<u64> = transport
        .bodies
        .lock()
        .unwrap()
        .iter()
        .map(|b| b["max_completion_tokens"].as_u64().unwrap())
        .collect();
    tranches.sort_unstable_by(|a, b| b.cmp(a));
    assert_eq!(tranches, [750, 502, 316]);
    assert_eq!(reply["budget"]["spent"], json!({"calls": 3, "tokens": 315}));
}

/// A frame past its `timeout_ms` sends nothing more (TS cancels the frame's task): the late first reply is charged,
/// no re-ask goes out, and the frame ends "cancelled" with the reply in `last_outputs`.
#[test]
fn inference_frame_sends_no_request_after_its_deadline() {
    let (handler, transport, _dir) = frames(&[], ModelSettings::default(), false);
    transport
        .replies
        .lock()
        .unwrap()
        .push(json!({"text": "many", "usage": {"input": 40, "output": 2, "cacheRead": 0}, "block_ms": 80}));
    let reply = call(
        &handler,
        "rlm.infer",
        json!({"task": "Count", "contract": {"type": "integer"}, "timeout_ms": 50}),
    );
    assert_eq!(transport.bodies.lock().unwrap().len(), 1);
    assert_eq!(reply["error"], "cancelled");
    assert_eq!(reply["spent"], json!({"calls": 1, "tokens": 42}));
    assert_eq!(reply["last_outputs"], json!(["many"]));
}
