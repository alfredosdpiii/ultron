//! Session flags of the native command whose TypeScript behavior has no Rust-observable twin to record: `-e`
//! extension manifests (TypeScript loads JavaScript extensions, Rust extension processes), `--models` scoping a new
//! session's model (decisions: scoped models choose it), the tool flags (the model's tool list stays `[rlm]`), the
//! `httpProxy` setting (HTTP_PROXY for the session's provider requests) and `!command` keys in auth.json.

mod common;

use common::{Naming, Sandbox, anthropic_text, completions_text, mock_server};
use serde_json::Value;
use std::path::Path;
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

fn hello_extension(at: &Path) {
    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("../extensions/examples/hello");
    std::fs::create_dir_all(at).unwrap();
    for file in ["extension.json", "hello.py"] {
        std::fs::copy(example.join(file), at.join(file)).unwrap();
    }
}

fn system_of(request: &Value) -> String {
    request["body"]["system"].to_string()
}

#[test]
fn cli_native_extension_flag_loads_a_manifest_also_with_no_extensions() {
    let greet = "ext.hello.greet(name: string)";
    let (port, seen) = mock_server(
        vec![anthropic_text("a"), anthropic_text("b"), anthropic_text("c")],
        Naming::Fail,
    );
    let sandbox = Sandbox::new(port);
    hello_extension(&sandbox.root.join("exts/hello"));
    // A path relative to the cwd, resolved by the client as native-command.ts does.
    let output = run(&sandbox, &["-p", "-e", "../exts/hello", "--model", MODEL, "x"], &[]);
    assert_eq!(text(&output.stdout), "a\n", "{}", text(&output.stderr));
    let output = run(
        &sandbox,
        &["-p", "--no-extensions", "-e", "../exts/hello", "--model", MODEL, "y"],
        &[],
    );
    assert_eq!(text(&output.stdout), "b\n", "{}", text(&output.stderr));
    // Installed in the profile, it is left out by --no-extensions.
    hello_extension(&sandbox.agent_dir.join("extensions/hello"));
    let output = run(&sandbox, &["-p", "--no-extensions", "--model", MODEL, "z"], &[]);
    assert_eq!(text(&output.stdout), "c\n", "{}", text(&output.stderr));
    let requests = seen.lock().unwrap().requests.clone();
    assert!(system_of(&requests[0]).contains(greet), "{}", system_of(&requests[0]));
    assert!(system_of(&requests[1]).contains(greet));
    assert!(!system_of(&requests[2]).contains(greet));
    for request in &requests {
        assert!(
            cli::compose::only_rlm(request["body"]["tools"].as_array().unwrap()),
            "{request}"
        );
    }
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
