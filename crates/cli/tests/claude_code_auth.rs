//! Replays `fixtures/claude_code_auth.json` (recorded from the TypeScript CLI by `fixtures/claude_code_auth.mjs`):
//! `ultron auth check` for the `claude-code` provider, with a fake `claude` on the run's injected PATH (or named by
//! `ULTRON_CLAUDE_CODE_BIN`), or none. The fake is never run: it would leave a marker file, which must not exist.

use ai::http::{BoxFuture, HttpRequest, HttpResponse, Transport};
use serde_json::Value;
use session_core::stored_auth::OAuthOptions;
use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;

/// No request may leave the check.
struct Refuse;

impl Transport for Refuse {
    fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, String>> {
        Box::pin(async move { Err(format!("unexpected request {}", request.url)) })
    }
}

fn fake_claude(dir: &Path, marker: &Path) -> String {
    std::fs::create_dir_all(dir).unwrap();
    let file = dir.join("claude");
    std::fs::write(&file, format!("#!/bin/sh\necho ran > '{}'\nexit 1\n", marker.display())).unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
    file.display().to_string()
}

#[test]
fn cli_auth_check_claude_code_matches_typescript() {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/claude_code_auth.json")).unwrap();
    let cases = fixture["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 11);
    for case in cases {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let agent = root.join("agent");
        std::fs::create_dir_all(&agent).unwrap();
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let marker = root.join("ran");
        let mut vars: HashMap<String, String> = HashMap::new();
        match case["cli"].as_str().unwrap() {
            "path" => {
                fake_claude(&bin, &marker);
            }
            "configured" => {
                let file = fake_claude(&root.join("tools"), &marker);
                vars.insert("ULTRON_CLAUDE_CODE_BIN".into(), file);
            }
            "configured-missing" => {
                fake_claude(&bin, &marker);
                let missing = root.join("tools").join("claude");
                vars.insert("ULTRON_CLAUDE_CODE_BIN".into(), missing.display().to_string());
            }
            "none" => {}
            other => panic!("unknown case {other}"),
        }
        vars.insert("PATH".into(), format!("{}:/usr/bin:/bin", bin.display()));
        vars.insert("HOME".into(), root.display().to_string());
        vars.insert("PI_OFFLINE".into(), "1".into());
        vars.insert("ULTRON_CODING_AGENT_DIR".into(), agent.display().to_string());
        vars.insert("ULTRON_SERVER_DIR".into(), root.join("server").display().to_string());
        let env = cli::Env {
            cwd: root.to_path_buf(),
            home: root.to_path_buf(),
            var: Arc::new(move |k| vars.get(k).cloned()),
            docs: root.join("docs"),
        };
        let options = OAuthOptions {
            transport: Arc::new(Refuse),
            callback_port: None,
        };
        let args: Vec<String> = case["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap().to_string())
            .collect();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = cli::auth::run_with(&args, &env, &mut out, &mut err, &options).unwrap();
        let name = format!("{} ({})", args.join(" "), case["cli"]);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            case["stdout"].as_str().unwrap(),
            "{name}: stdout"
        );
        assert_eq!(
            String::from_utf8(err).unwrap(),
            case["stderr"].as_str().unwrap(),
            "{name}: stderr"
        );
        assert_eq!(i64::from(code), case["code"].as_i64().unwrap(), "{name}: exit code");
        assert!(!marker.exists(), "{name}: the fake claude ran");
    }
}
