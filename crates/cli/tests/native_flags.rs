//! Session flags of the native command whose TypeScript behavior has no Rust-observable twin to record: `-e` and
//! `--no-extensions` (Pi extensions are not supported by ultron-rs: each flag is warned about and ignored, and so are
//! the settings `packages`, with one note), `--models` scoping a new
//! session's model (decisions: scoped models choose it), the tool flags (the model's tool list stays `[rlm]`), the
//! `httpProxy` setting (HTTP_PROXY for the session's provider requests), `!command` keys in auth.json, and
//! `--append-system-prompt` (the TS server never passed it to a worker).

mod common;

use common::{Naming, Sandbox, anthropic_text, completions_text, mock_server};
use serde_json::Value;
use std::process::Output;

const MODEL: &str = "anthropic/claude-sonnet-4-5";

fn run(sandbox: &Sandbox, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut command = sandbox.ultron();
    command.env("PATH", "/usr/bin:/bin").env("PI_OFFLINE", "1").args(args);
    for (k, v) in env {
        command.env(k, v);
    }
    command.output().unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn system_of(request: &Value) -> String {
    request["body"]["system"].to_string()
}

#[test]
fn cli_native_extension_flags_and_settings_packages_are_warned_about_and_ignored() {
    let (port, seen) = mock_server(vec![anthropic_text("a"), anthropic_text("b")], Naming::Fail);
    let sandbox = Sandbox::new(port);
    let manifest = sandbox.root.join("exts/hello/extension.json");
    std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
    std::fs::write(
        &manifest,
        r#"{"name": "hello", "command": "/bin/false", "capabilities": {"functions": [{"name": "greet"}]}}"#,
    )
    .unwrap();
    let output = run(
        &sandbox,
        &[
            "-p",
            "-e",
            "../exts/hello",
            "--no-extensions",
            "--extension",
            "npm:pi-tools",
            "--model",
            MODEL,
            "x",
        ],
        &[],
    );
    assert_eq!(text(&output.stdout), "a\n", "{}", text(&output.stderr));
    assert_eq!(
        text(&output.stderr),
        "Warning: Pi extensions are not supported by ultron-rs; ignoring --extension ../exts/hello\n\
         Warning: Pi extensions are not supported by ultron-rs; ignoring --extension npm:pi-tools\n\
         Warning: Pi extensions are not supported by ultron-rs; ignoring --no-extensions\n"
    );
    // The settings `packages` are not installed or resolved: one note says how many are ignored.
    let settings = sandbox.agent_dir.join("settings.json");
    let mut json: Value = std::fs::read_to_string(&settings)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    json["packages"] = serde_json::json!(["npm:pi-tools", "../exts/hello"]);
    std::fs::write(&settings, json.to_string()).unwrap();
    let output = run(&sandbox, &["-p", "--model", MODEL, "y"], &[]);
    assert_eq!(text(&output.stdout), "b\n", "{}", text(&output.stderr));
    assert_eq!(
        text(&output.stderr),
        "Note: Pi packages are not supported by ultron-rs; ignoring 2 packages in settings\n"
    );
    let requests = seen.lock().unwrap().requests.clone();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert!(!system_of(request).contains("ext.hello"), "{}", system_of(request));
        assert!(
            cli::compose::only_rlm(request["body"]["tools"].as_array().unwrap()),
            "{request}"
        );
    }
}

#[test]
fn cli_native_append_system_prompt_reaches_the_worker() {
    let (port, seen) = mock_server(vec![anthropic_text("a"), anthropic_text("b")], Naming::Fail);
    let sandbox = Sandbox::new(port);
    std::fs::write(sandbox.agent_dir.join("APPEND_SYSTEM.md"), "DISCOVERED-APPEND").unwrap();
    // A file input is read in the CLI's directory (relative path), a literal input is the text itself.
    std::fs::write(sandbox.cwd.join("extra.md"), "FILE-APPEND").unwrap();
    let output = run(
        &sandbox,
        &[
            "-p",
            "--model",
            MODEL,
            "--append-system-prompt",
            "extra.md",
            "--append-system-prompt",
            "LITERAL-APPEND",
            "x",
        ],
        &[],
    );
    assert_eq!(text(&output.stdout), "a\n", "{}", text(&output.stderr));
    let output = run(&sandbox, &["-p", "--model", MODEL, "y"], &[]);
    assert_eq!(text(&output.stdout), "b\n", "{}", text(&output.stderr));
    let requests = seen.lock().unwrap().requests.clone();
    let flagged = system_of(&requests[0]);
    assert!(
        flagged.contains("FILE-APPEND") && flagged.contains("LITERAL-APPEND"),
        "{flagged}"
    );
    assert!(
        !flagged.contains("DISCOVERED-APPEND"),
        "the flag replaces APPEND_SYSTEM.md: {flagged}"
    );
    let plain = system_of(&requests[1]);
    assert!(
        plain.contains("DISCOVERED-APPEND") && !plain.contains("FILE-APPEND"),
        "{plain}"
    );
}

#[test]
fn cli_native_models_flag_scopes_a_new_sessions_model() {
    let (port, seen) = mock_server(vec![completions_text("from groq")], Naming::Fail);
    let sandbox = Sandbox::new(port);
    let output = run(
        &sandbox,
        &["-p", "--models", "groq/llama-3.3-70b-versatile", "hi"],
        &[("GROQ_API_KEY", "test-groq")],
    );
    assert_eq!(text(&output.stdout), "from groq\n", "{}", text(&output.stderr));
    let requests = seen.lock().unwrap().requests.clone();
    assert_eq!(requests[0]["url"], "/v1/chat/completions");
    assert_eq!(requests[0]["body"]["model"], "llama-3.3-70b-versatile");
}

#[test]
fn cli_native_tool_flags_warn_and_keep_the_rlm_tool() {
    let (port, seen) = mock_server(vec![anthropic_text("a"), anthropic_text("b")], Naming::Fail);
    let sandbox = Sandbox::new(port);
    let output = run(&sandbox, &["-p", "--no-tools", "--model", MODEL, "x"], &[]);
    assert_eq!(text(&output.stdout), "a\n");
    assert_eq!(
        text(&output.stderr),
        "Warning: The model keeps the rlm tool: it is the model's only tool, so it cannot be left out\n"
    );
    let output = run(&sandbox, &["-p", "--tools", "rlm,read", "--model", MODEL, "y"], &[]);
    assert_eq!(text(&output.stdout), "b\n");
    assert_eq!(
        text(&output.stderr),
        "Warning: read is not a model tool here: the model's only tool is rlm, whose REPL has read as Python functions\n"
    );
    for request in seen.lock().unwrap().requests.iter() {
        assert!(
            cli::compose::only_rlm(request["body"]["tools"].as_array().unwrap()),
            "{request}"
        );
    }
}

#[test]
fn cli_native_http_proxy_setting_routes_provider_requests() {
    let (port, seen) = mock_server(vec![anthropic_text("proxied")], Naming::Fail);
    let sandbox = Sandbox::new(port);
    // The provider's host does not resolve: only the proxy can answer.
    std::fs::write(
        sandbox.agent_dir.join("models.json"),
        r#"{"providers":{"anthropic":{"baseUrl":"http://provider.invalid"}}}"#,
    )
    .unwrap();
    std::fs::write(
        sandbox.agent_dir.join("settings.json"),
        format!(r#"{{"httpProxy": " http://127.0.0.1:{port} "}}"#),
    )
    .unwrap();
    let output = run(&sandbox, &["-p", "--model", MODEL, "x"], &[]);
    assert_eq!(text(&output.stdout), "proxied\n", "{}", text(&output.stderr));
    let requests = seen.lock().unwrap().requests.clone();
    assert!(
        requests[0]["url"]
            .as_str()
            .unwrap()
            .starts_with("http://provider.invalid/v1/messages"),
        "{}",
        requests[0]["url"]
    );
}

#[test]
fn cli_native_command_key_in_auth_json_authenticates() {
    let (port, seen) = mock_server(vec![anthropic_text("keyed")], Naming::Fail);
    let sandbox = Sandbox::new(port);
    std::fs::write(
        sandbox.agent_dir.join("auth.json"),
        r#"{"anthropic":{"type":"api_key","key":"!printf command-key"}}"#,
    )
    .unwrap();
    let mut command = sandbox.ultron();
    command
        .env("PATH", "/usr/bin:/bin")
        .env_remove("ANTHROPIC_API_KEY")
        .args(["-p", "--model", MODEL, "x"]);
    let output = command.output().unwrap();
    assert_eq!(text(&output.stdout), "keyed\n", "{}", text(&output.stderr));
    assert_eq!(seen.lock().unwrap().requests.len(), 1);
}
