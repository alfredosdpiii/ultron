//! Frames that name a model other than the session's (`rlm.infer(model=...)`, `rlm.map(model=...)`, the frame model
//! of `ULTRON_RLM_FRAME_MODEL` / `rlm.frameModel`) call it as TypeScript's frame lane does after `setModel`: the
//! session resolves the model and its provider's key, and the request goes out through the same provider dispatch.
//! Ported from ultron-claude-code-frames.test.ts ("ULTRON_RLM_FRAME_MODEL is the model of code-free frames that name
//! none": the request goes to that model), with literal expectations; this host refuses a model it cannot resolve at
//! admission.

mod common;

use common::{Scripted, model};
use inference::{Frames, Inference, ModelResolver, ModelSettings};
use kernel::{EchoHandler, HostHandler, Signal};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

fn own() -> ai::Model {
    model(
        &json!({"provider": "fake", "id": "frame-model", "api": "openai-completions", "contextWindow": 32000, "maxTokens": 8192}),
    )
}

/// Resolves `other/m` and `claude-code/haiku` (both over the scripted chat-completions transport, with their own
/// keys and output limits); records every model asked for.
fn resolver(asked: Arc<Mutex<Vec<String>>>) -> ModelResolver {
    Arc::new(move |spec: &str| {
        asked.lock().unwrap().push(spec.to_string());
        let (provider, id, key, max) = match spec {
            "other/m" => ("other", "m", "other-key", 300),
            "claude-code/haiku" => ("claude-code", "haiku", "cc-key", 8192),
            _ => return Err(format!("Model \"{spec}\" not found")),
        };
        let mut model = model(
            &json!({"provider": provider, "id": id, "api": "openai-completions", "contextWindow": 32000, "maxTokens": max}),
        );
        model.base_url = "http://other.invalid/v1".into();
        Ok((model, Some(key.to_string())))
    })
}

fn reply(text: &str) -> Value {
    json!({"text": text, "usage": {"input": 40, "output": 8, "cacheRead": 0}})
}

fn setup(
    replies: Vec<Value>,
    with_resolver: bool,
) -> (Inference, Arc<Scripted>, Arc<Mutex<Vec<String>>>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let transport = Arc::new(Scripted::default());
    *transport.replies.lock().unwrap() = replies;
    let asked: Arc<Mutex<Vec<String>>> = Arc::default();
    let mut handler = Inference::new(
        Arc::new(EchoHandler),
        own(),
        Some("own-key".into()),
        transport.clone(),
        dir.path().join("context"),
    );
    if with_resolver {
        handler = handler.with_models(resolver(asked.clone()));
    }
    (handler, transport, asked, dir)
}

fn call(handler: &dyn HostHandler, kind: &str, payload: Value) -> Value {
    let Value::Object(payload) = payload else {
        panic!("payload")
    };
    handler.handle(kind, payload, &Signal::new()).unwrap()
}

#[test]
fn inference_frame_naming_another_model_calls_it_with_its_own_key() {
    let (handler, transport, asked, _dir) = setup(vec![reply("from m")], true);
    let observed = call(&handler, "rlm.infer", json!({"task": "t", "model": "other/m"}));
    assert_eq!(observed["status"], "complete");
    assert_eq!(observed["value"], "from m");
    assert_eq!(*asked.lock().unwrap(), ["other/m"]);
    let bodies = transport.bodies.lock().unwrap().clone();
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0]["model"], "m");
    // The resolved model's own output limit caps the request (its maxTokens 300 under the 16k tranche).
    let cap = bodies[0].get("max_completion_tokens").or(bodies[0].get("max_tokens"));
    assert_eq!(cap, Some(&json!(300)), "{}", bodies[0]);
    assert_eq!(
        *transport.authorizations.lock().unwrap(),
        [Some("Bearer other-key".to_string())]
    );
    // The frame's trace names the model it called.
    let trace = call(&handler, "rlm.frames", json!({"id": observed["trace_id"]}));
    assert_eq!(trace["model"], "other/m", "{trace}");
}

#[test]
fn inference_frame_naming_the_sessions_model_needs_no_resolution() {
    let (handler, transport, asked, _dir) = setup(vec![reply("own")], true);
    let observed = call(&handler, "rlm.infer", json!({"task": "t", "model": "fake/frame-model"}));
    assert_eq!(observed["value"], "own");
    assert!(asked.lock().unwrap().is_empty());
    assert_eq!(transport.bodies.lock().unwrap()[0]["model"], "frame-model");
    assert_eq!(
        *transport.authorizations.lock().unwrap(),
        [Some("Bearer own-key".to_string())]
    );
}

#[test]
fn inference_frame_naming_an_unknown_model_is_refused_unsent() {
    for with_resolver in [true, false] {
        let (handler, transport, _asked, _dir) = setup(vec![], with_resolver);
        let observed = call(&handler, "rlm.infer", json!({"task": "t", "model": "nope/x"}));
        assert_eq!(observed["status"], "error");
        assert_eq!(
            observed["error"],
            "Admission refused: model nope/x is not available to this host"
        );
        assert_eq!(observed["spent"], json!({"calls": 0, "tokens": 0}));
        assert!(transport.bodies.lock().unwrap().is_empty());
    }
}

/// ultron-claude-code-frames.test.ts: with ULTRON_RLM_FRAME_MODEL=claude-code/haiku a code-free map frame that names
/// no model goes to claude-code/haiku; a frame of the same map naming its own model keeps it.
#[test]
fn inference_frame_model_env_routes_code_free_frames_to_that_model() {
    let (handler, transport, asked, _dir) = setup(vec![reply("chat"), reply("chat again")], true);
    let env = [("ULTRON_RLM_FRAME_MODEL".to_string(), "claude-code/haiku".to_string())].into();
    let frames = Frames::new(handler, env, ModelSettings::default);
    let reply = call(
        &frames,
        "rlm.map",
        json!({"frames": [{"task": "cat in French", "context": []}]}),
    );
    assert_eq!(reply["results"][0]["value"], "chat");
    let reply = call(&frames, "rlm.infer", json!({"task": "dog in French"}));
    assert_eq!(reply["value"], "chat again");
    assert_eq!(*asked.lock().unwrap(), ["claude-code/haiku", "claude-code/haiku"]);
    let bodies = transport.bodies.lock().unwrap().clone();
    assert_eq!(
        (bodies[0]["model"].clone(), bodies[1]["model"].clone()),
        (json!("haiku"), json!("haiku"))
    );
}
