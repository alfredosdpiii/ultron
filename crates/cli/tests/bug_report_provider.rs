//! `/bug`'s report.json `provider` section, replaying `fixtures/bug_report_provider.json` (recorded from
//! `collectBugReportMetadata` -> `describeProvider` over a TypeScript `ModelRuntime` by
//! `fixtures/bug_report_provider.mjs`): every built-in provider unconfigured and with its API-key variable set,
//! keys from the environment, stored keys and OAuth logins, Bedrock's, Vertex's, Cloudflare's and the Claude Code
//! CLI's ambient credentials, a models.json override of a built-in, and models.json providers with `$VAR`, literal
//! and `!command` keys. Each case runs in a private temp HOME / agent dir with only the variables it names.

mod tui_harness;

use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;

fn fixture() -> Vec<Value> {
    serde_json::from_str(include_str!("../fixtures/bug_report_provider.json")).unwrap()
}

/// The case's environment in `root`: its files, models.json and auth.json written, its variables (with `${TMP}`
/// as `root`) and nothing else but PATH and the agent dir.
fn case_env(case: &Value, root: &Path) -> cli::Env {
    let agent = root.join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    if !case["models"].is_null() {
        std::fs::write(agent.join("models.json"), case["models"].to_string()).unwrap();
    }
    if !case["auth"].is_null() {
        std::fs::write(agent.join("auth.json"), case["auth"].to_string()).unwrap();
    }
    for (path, content) in case["files"].as_object().unwrap() {
        let file = root.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, content.as_str().unwrap()).unwrap();
    }
    let mut vars: std::collections::HashMap<String, String> = case["env"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| {
            (
                k.clone(),
                v.as_str().unwrap().replace("${TMP}", &root.display().to_string()),
            )
        })
        .collect();
    vars.insert("PATH".into(), "/usr/bin:/bin".into());
    vars.insert("PI_OFFLINE".into(), "1".into());
    vars.insert("ULTRON_CODING_AGENT_DIR".into(), agent.display().to_string());
    cli::Env {
        cwd: root.to_path_buf(),
        home: root.to_path_buf(),
        var: Arc::new(move |k| vars.get(k).cloned()),
        docs: root.join("docs"),
    }
}

fn described(env: &cli::Env, provider: &str) -> Value {
    telemetry::bug_report::describe_provider(&cli::bug_report::provider_info(env, provider, false, false))
}

#[test]
fn cli_bug_report_provider_section_matches_typescript() {
    let mut compared = 0;
    for case in fixture() {
        let name = case["name"].as_str().unwrap();
        let root = tempfile::tempdir().unwrap();
        let env = case_env(&case, root.path());
        if case["model"]["provider"] == "--every-builtin" {
            for (provider, recorded) in case["provider"].as_object().unwrap() {
                assert_eq!(described(&env, provider), recorded["provider"], "{name}: {provider}");
                compared += 1;
            }
        } else {
            let provider = case["model"]["provider"].as_str().unwrap();
            assert_eq!(described(&env, provider), case["provider"], "{name}");
            compared += 1;
        }
    }
    assert!(compared > 100, "{compared}");
}

/// `--api-key` is the runtime key (`getProviderAuthStatus` checks it first), and a provider only an extension
/// declares is `registeredByExtension`.
#[test]
fn cli_bug_report_provider_runtime_key_and_extension_provider() {
    let root = tempfile::tempdir().unwrap();
    let case = json!({"env": {"ANTHROPIC_API_KEY": "k"}, "files": {}, "models": null,
        "auth": {"anthropic": {"type": "oauth", "access": "a", "refresh": "r", "expires": 4102444800000_u64}}});
    let env = case_env(&case, root.path());
    let info = cli::bug_report::provider_info(&env, "anthropic", true, false);
    let section = telemetry::bug_report::describe_provider(&info);
    assert_eq!(section["authStatus"], json!({"configured": true, "source": "runtime"}));
    assert_eq!(section["usingOAuth"], true);
    let info = cli::bug_report::provider_info(&env, "from-extension", false, true);
    let section = telemetry::bug_report::describe_provider(&info);
    assert_eq!(section["registeredByExtension"], true);
    assert_eq!(section["name"], "from-extension");
    assert_eq!(section["authStatus"], json!({"configured": false}));
}

/// The archive's files, read by the tests' Python.
fn unzip(path: &Path) -> serde_json::Map<String, Value> {
    let python = std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into());
    let out = std::process::Command::new(python)
        .args([
            "-I",
            "-c",
            "import json, sys, zipfile\nz = zipfile.ZipFile(sys.argv[1])\nprint(json.dumps({n: z.read(n).decode() for n in z.namelist()}))",
        ])
        .arg(path)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice::<Value>(&out.stdout)
        .unwrap()
        .as_object()
        .unwrap()
        .clone()
}

/// Through the product: `/bug` in the TUI of `ultron --api-key ...` exports a report whose provider section is the
/// session model's provider as the session sees it (models.json's base URL for the built-in, the command line's key).
#[test]
fn cli_bug_report_provider_section_of_the_tui_report() {
    let mock = tui_harness::Mock::start(vec![tui_harness::text_reply("Hello back.")], None);
    let tmp = tui_harness::TempDir::new();
    let env = [
        ("ULTRON_AUTO_NAME", "off"),
        ("ULTRON_SPLASH", "off"),
        ("PI_RADIUS_GATEWAY", "http://127.0.0.1:9"),
    ];
    let args = ["--model", "anthropic/claude-sonnet-4-5", "--api-key", "cli-key"];
    let tui = tui_harness::Tui::start_at(&tmp.0, mock.port, 120, 50, &args, &env);
    tui.wait("Session:");
    tui.send_text("/bug");
    tui.send_keys(&["Enter"]);
    tui.wait("What went wrong? (optional)");
    tui.send_keys(&["Enter"]);
    tui.wait("Include the session transcript?");
    tui.send_keys(&["Enter"]);
    tui.wait("Upload sends the report to");
    tui.send_keys(&["Down"]);
    tui.wait("→ Export as Zip");
    tui.send_keys(&["Enter"]);
    tui.wait("Bug report exported to:");
    let cwd = tmp.0.join("cwd");
    let zip = std::fs::read_dir(&cwd)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.file_name().unwrap().to_string_lossy().starts_with("pi-bug-report-"))
        .unwrap();
    let report: Value = serde_json::from_str(unzip(&zip)["report.json"].as_str().unwrap()).unwrap();
    assert_eq!(
        report["provider"],
        json!({
            "id": "anthropic",
            "name": "Anthropic",
            "baseUrl": format!("http://127.0.0.1:{}", mock.port),
            "headerNames": [],
            "authTypes": ["api_key", "oauth"],
            "authStatus": {"configured": true, "source": "runtime"},
            "usingOAuth": false,
            "registeredByExtension": false,
        })
    );
    drop(tui);
}
