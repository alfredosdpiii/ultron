//! The environment and settings that give the model native tools in TypeScript (`ULTRON_TOOLS=native`,
//! `ULTRON_EXTENSION_TOOLS=native`, `ULTRON_NATIVE_EXTENSION_TOOLS`, the `extensionTools` setting) are warned about
//! by the native command and ignored: the model's tool list stays `[rlm]` (user decision 2026-10-17).

mod common;

use common::{Naming, Sandbox, anthropic_text, mock_server};
use std::process::Output;

const MODEL: &str = "anthropic/claude-sonnet-4-5";

fn run(sandbox: &Sandbox, env: &[(&str, &str)]) -> Output {
    let mut command = sandbox.ultron();
    command
        .env("PATH", "/usr/bin:/bin")
        .env("PI_OFFLINE", "1")
        .args(["-p", "--model", MODEL, "x"]);
    for (k, v) in env {
        command.env(k, v);
    }
    command.output().unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn cli_tool_env_and_extension_tools_setting_warn_and_keep_only_rlm() {
    let (port, seen) = mock_server(vec![anthropic_text("a"), anthropic_text("b")], Naming::Fail);
    let sandbox = Sandbox::new(port);
    std::fs::write(
        sandbox.agent_dir.join("settings.json"),
        r#"{"extensionTools": {"mode": "native", "native": ["mcp", " ", 3, "web"]}}"#,
    )
    .unwrap();
    let output = run(
        &sandbox,
        &[
            ("ULTRON_TOOLS", " Native "),
            ("ULTRON_EXTENSION_TOOLS", "NATIVE"),
            ("ULTRON_NATIVE_EXTENSION_TOOLS", "a, ,b"),
        ],
    );
    assert_eq!(text(&output.stdout), "a\n", "{}", text(&output.stderr));
    let ext = "the model's only tool is rlm, and Pi extensions are not supported by ultron-rs";
    assert_eq!(
        text(&output.stderr),
        format!(
            "Warning: ULTRON_TOOLS=native is ignored: the model's only tool is rlm, whose REPL has read, edit, write and bash as Python functions\n\
             Warning: ULTRON_EXTENSION_TOOLS=native is ignored: {ext}\n\
             Warning: ULTRON_NATIVE_EXTENSION_TOOLS (a, b) is ignored: {ext}\n\
             Warning: The extensionTools.mode setting \"native\" is ignored: {ext}\n\
             Warning: The extensionTools.native setting (mcp, web) is ignored: {ext}\n"
        )
    );
    // The REPL-only values TypeScript also accepts warn about nothing.
    std::fs::write(
        sandbox.agent_dir.join("settings.json"),
        r#"{"extensionTools": {"mode": "repl"}}"#,
    )
    .unwrap();
    let output = run(&sandbox, &[("ULTRON_TOOLS", "rlm"), ("ULTRON_EXTENSION_TOOLS", "repl")]);
    assert_eq!(text(&output.stdout), "b\n");
    assert_eq!(text(&output.stderr), "");
    for request in seen.lock().unwrap().requests.iter() {
        assert!(
            cli::compose::only_rlm(request["body"]["tools"].as_array().unwrap()),
            "{request}"
        );
    }
}
