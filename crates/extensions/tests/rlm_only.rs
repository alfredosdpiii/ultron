//! RLM-first: with an extension loaded that registers functions, the model's provider request still declares exactly
//! one tool, `rlm`; the functions reach the model only as `ext.<extension>.<function>` in the runtime guide and are
//! called through the kernel API.

use ai::transcript::create_initial_system_message;
use ai::{Message, TranscriptContext};
use extensions::{ExtensionHost, NoHost, SessionInfo, kernel_api, parse_manifest};
use serde_json::{Map, json};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

fn load_hello(dir: &Path) -> ExtensionHost {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/hello/hello.py");
    let python = std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into());
    // The startup timeout is an input no test here exercises; long so a loaded machine never trips it.
    let text =
        json!({ "name": "hello", "command": python, "args": [script.to_string_lossy()], "startupTimeoutMs": 120_000 })
            .to_string();
    let path = dir.join("extension.json");
    std::fs::write(&path, &text).unwrap();
    let session = SessionInfo {
        cwd: "/work".into(),
        mode: "print".into(),
        has_ui: false,
        flags: Map::new(),
    };
    ExtensionHost::load(vec![parse_manifest(&path, &text).unwrap()], session, Arc::new(NoHost))
}

#[test]
fn extensions_functions_never_become_model_tools() {
    let dir = tempfile::tempdir().unwrap();
    let host = load_hello(dir.path());
    assert_eq!(host.functions().len(), 6);

    // The request the composition root builds: the runtime guide gains the extension section, the tool list is rlm.
    let guide = kernel_api::guide_section(&host.functions()).unwrap();
    let system = format!("You are Ultron.\n\n{guide}");
    let tools = [cli::print_mode::rlm_tool()];
    let messages = vec![Message::System(
        create_initial_system_message(Some(&system), Some(&tools)).unwrap(),
    )];
    let model = ai::models::builtin_models("mistral").next().unwrap();
    let payload = ai::mistral::build_payload(
        model,
        &TranscriptContext {
            messages: messages.clone(),
        },
        &messages,
        &Default::default(),
    )
    .unwrap();
    let names: Vec<&str> = payload["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["rlm"]);
    let request = payload.to_string();
    assert!(
        request.contains("ext.hello.greet(name: string) - Greet someone"),
        "{request}"
    );
    for name in ["greet", "slow", "crash", "shout"] {
        assert!(!names.contains(&name));
    }

    // From a cell, the kernel calls the function through the host operation.
    let result = kernel_api::call(
        &host,
        &json!({ "extension": "hello", "function": "greet", "arguments": { "name": "REPL" } }),
        Some(Duration::from_secs(120)),
        None,
    )
    .unwrap();
    assert_eq!(result["content"][0]["text"], "Hello, REPL!");
    let listed = kernel_api::listing(&host);
    assert_eq!(
        listed[0],
        json!({
            "extension": "hello",
            "name": "greet",
            "description": "Greet someone",
            "parameters": { "type": "object", "properties": { "name": { "type": "string" } } },
        })
    );
    assert_eq!(listed[1]["parameters"], json!({ "type": "object" }));
    assert_eq!(
        kernel_api::call(
            &host,
            &json!({ "extension": "hello", "function": "greet", "arguments": [1] }),
            None,
            None
        )
        .unwrap_err()
        .to_string(),
        "ext.call \"arguments\" must be an object"
    );
    assert_eq!(kernel_api::guide_section(&[]), None);
    host.shutdown();
}
