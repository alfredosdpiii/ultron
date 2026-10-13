//! The A04 finding on `rlm.spawn(model=...)`: a malformed model is refused with TS's "provider/model" message, and
//! with the session's model check installed an unknown model is refused too, instead of the child silently running
//! on the root's model; a known model reaches the child run.

use delegation::host::{ChildRunner, CollectMode, Delegation, DelegationOptions, builtin_model_check};
use serde_json::json;
use std::sync::{Arc, Mutex};

fn host() -> (Delegation, Arc<Mutex<Vec<Option<String>>>>) {
    let models: Arc<Mutex<Vec<Option<String>>>> = Arc::default();
    let seen = models.clone();
    let runner: ChildRunner = Arc::new(move |run| {
        seen.lock().unwrap().push(run.model.clone());
        Ok("done".into())
    });
    let mut options = DelegationOptions::new(runner);
    options.collect = CollectMode::Wait;
    (Delegation::new(options).0, models)
}

fn spawn(host: &Delegation, model: serde_json::Value) -> Result<serde_json::Value, String> {
    host.handle(
        "rlm.spawn",
        json!({ "prompt": "p", "kwargs": { "name": "c", "model": model } }),
        "main",
    )
}

#[test]
fn delegation_spawn_refuses_a_malformed_model() {
    let (host, models) = host();
    for bad in ["no-slash", "a/", "/b", "a b/c", ""] {
        assert_eq!(
            spawn(&host, json!(bad)).unwrap_err(),
            "model must be provider/model",
            "{bad}"
        );
    }
    // Without a model check a well-formed model is passed through to the child run.
    spawn(&host, json!("faux/m")).unwrap();
    host.handle("rlm.collect", json!({}), "main").unwrap();
    assert_eq!(*models.lock().unwrap(), vec![Some("faux/m".to_string())]);
}

#[test]
fn delegation_spawn_with_the_model_check_refuses_an_unknown_model_and_passes_a_known_one() {
    let (host, models) = host();
    host.set_model_check(builtin_model_check());
    let error = spawn(&host, json!("nope/not-a-model")).unwrap_err();
    assert_eq!(
        error,
        "model must be provider/model naming a known model, not nope/not-a-model"
    );
    let known = ai::models::builtin_models("anthropic")
        .next()
        .expect("a built-in anthropic model");
    let spec = format!("anthropic/{}", known.id);
    spawn(&host, json!(spec)).unwrap();
    host.handle("rlm.collect", json!({}), "main").unwrap();
    assert_eq!(*models.lock().unwrap(), vec![Some(spec)]);
}
