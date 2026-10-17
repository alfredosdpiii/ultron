//! Replays fixtures/runner.json (recorded from the TypeScript ExtensionRunner by fixtures/generate.mjs): scripted
//! extensions answer each event the way the recorded handlers did, and the Rust router must call them in the same
//! order with the same events, return the same outcome and report the same errors.

use extensions::router::{Endpoint, ExtensionError, Router};
use serde_json::{Map, Value, json};
use std::sync::{Arc, Mutex};

struct Scripted {
    path: String,
    on: Map<String, Value>,
    commands: Vec<String>,
    log: Arc<Mutex<Vec<Value>>>,
}

/// What the TypeScript handler saw; `before_agent_start` drops the rendered options like the generator does.
fn snapshot(event: &Value) -> Value {
    if event["type"] == "before_agent_start" {
        let mut out =
            json!({ "type": event["type"], "prompt": event["prompt"], "systemPrompt": event["systemPrompt"] });
        if let Some(images) = event.get("images") {
            out["images"] = images.clone();
        }
        return out;
    }
    event.clone()
}

impl Endpoint for Scripted {
    fn path(&self) -> &str {
        &self.path
    }

    fn subscribes(&self, event_type: &str) -> bool {
        self.on.contains_key(event_type)
    }

    /// The protocol answer an extension SDK gives for the scripted TypeScript handler.
    fn dispatch(&self, event: &Value) -> Result<Option<Value>, String> {
        self.log
            .lock()
            .unwrap()
            .push(json!({ "ext": self.path, "event": snapshot(event) }));
        let kind = event["type"].as_str().unwrap();
        let action = &self.on[kind];
        if let Some(message) = action.get("throw") {
            return Err(message.as_str().unwrap().to_string());
        }
        if let Some(Value::Object(patch)) = action.get("mutateInput") {
            let mut input = event["input"].clone();
            for (k, v) in patch {
                input[k] = v.clone();
            }
            return Ok(Some(json!({ "input": input })));
        }
        if let Some(patch) = action.get("setHeaders") {
            return Ok(Some(json!({ "headers": patch })));
        }
        match action.get("return") {
            None => Ok(None),
            Some(value) if kind == "before_provider_request" => Ok(Some(json!({ "payload": value }))),
            Some(Value::Null) => Ok(None),
            Some(value) => Ok(Some(value.clone())),
        }
    }

    fn commands(&self) -> Vec<(String, Option<String>)> {
        self.commands
            .iter()
            .map(|name| (name.clone(), Some(format!("{} {name}", self.path))))
            .collect()
    }
}

fn error_value(e: &ExtensionError) -> Value {
    e.to_value()
}

fn opt(value: Option<Value>) -> Value {
    value.unwrap_or(Value::Null)
}

fn run(router: &Router, call: &Value, errors: &Arc<Mutex<Vec<Value>>>) -> Value {
    let ok = |v: Value| json!({ "ok": v });
    let throws = |e: ExtensionError| json!({ "throws": e.error });
    match call["kind"].as_str().unwrap() {
        "emit" => ok(opt(router.emit(&call["event"]))),
        "projectTrust" => {
            let (result, errs) = router.emit_project_trust(&call["event"]);
            errors.lock().unwrap().extend(errs.iter().map(error_value));
            ok(opt(result))
        }
        "beforeFileWrite" => router
            .emit_before_file_write(&call["event"])
            .map_or_else(throws, |v| ok(opt(v))),
        "userBash" => router
            .emit_user_bash(&call["event"])
            .map_or_else(throws, |v| ok(opt(v))),
        "afterCellChanges" => ok(opt(router.emit_after_cell_changes(&call["event"]))),
        "agentStart" => {
            let render = |options: &Value| {
                options
                    .get("forceSystemPrompt")
                    .and_then(Value::as_str)
                    .unwrap_or("<default>")
                    .to_string()
            };
            let outcome = router.emit_before_agent_start(
                call["prompt"].as_str().unwrap(),
                call.get("images"),
                &json!({ "cwd": "/w" }),
                &render,
            );
            let mut out = json!({ "messages": outcome.messages });
            if let Some(prompt) = outcome.system_prompt {
                out["systemPrompt"] = json!(prompt);
            }
            ok(out)
        }
        "context" => ok(Value::Array(router.emit_context(call["messages"].as_array().unwrap()))),
        "commands" => ok(Value::Array(
            router
                .registered_commands()
                .into_iter()
                .map(|c| json!({ "name": c.name, "invocationName": c.invocation_name, "description": c.description }))
                .collect(),
        )),
        other => panic!("unknown call kind {other}"),
    }
}

#[test]
fn extensions_router_matches_typescript_runner() {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/runner.json")).unwrap();
    // The recorded runner also covers emitters no product path calls (the native worker never emits them), which
    // the router does not have.
    let unported = [
        "toolCall",
        "toolResult",
        "messageEnd",
        "cacheWarming",
        "input",
        "providerRequest",
        "providerHeaders",
        "resources",
        "boundary",
    ];
    let scenarios: Vec<&Value> = fixture["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|scenario| !unported.contains(&scenario["call"]["kind"].as_str().unwrap()))
        .collect();
    assert_eq!(scenarios.len(), 21);
    let mut failures = Vec::new();
    for scenario in scenarios {
        let log = Arc::new(Mutex::new(Vec::new()));
        let endpoints: Vec<Arc<dyn Endpoint>> = scenario["extensions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|spec| {
                Arc::new(Scripted {
                    path: spec["path"].as_str().unwrap().to_string(),
                    on: spec.get("on").and_then(Value::as_object).cloned().unwrap_or_default(),
                    commands: spec
                        .get("commands")
                        .and_then(Value::as_array)
                        .map(|c| c.iter().map(|n| n.as_str().unwrap().to_string()).collect())
                        .unwrap_or_default(),
                    log: log.clone(),
                }) as Arc<dyn Endpoint>
            })
            .collect();
        let router = Router::new(endpoints);
        let errors = Arc::new(Mutex::new(Vec::new()));
        let sink = errors.clone();
        router.on_error(move |e| sink.lock().unwrap().push(error_value(e)));
        let outcome = run(&router, &scenario["call"], &errors);
        let got = json!({ "log": *log.lock().unwrap(), "outcome": outcome, "errors": *errors.lock().unwrap() });
        let want = json!({ "log": scenario["log"], "outcome": scenario["outcome"], "errors": scenario["errors"] });
        if got != want {
            failures.push(format!(
                "{}\n  want {}\n  got  {}",
                scenario["name"],
                serde_json::to_string(&want).unwrap(),
                serde_json::to_string(&got).unwrap()
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} scenario(s) differ:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
