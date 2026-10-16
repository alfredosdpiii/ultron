//! Differential tests against crates/cli/fixtures/cli.json, recorded from the TypeScript CLI by
//! crates/cli/fixtures/generate.mjs.

use cli::args::parse_args;
use cli::list_models::{format_token_count, locale_compare};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/cli.json")).expect("fixture parses")
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_string())
        .collect()
}

fn repo() -> PathBuf {
    config::paths::normalize(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
}

#[test]
fn cli_parse_args_matches_typescript() {
    let fx = fixture();
    let cases = fx["parse"].as_array().unwrap();
    assert!(cases.len() > 100);
    for case in cases {
        let args = strings(&case["args"]);
        assert_eq!(parse_args(&args).to_json(), case["result"], "parseArgs({args:?})");
    }
}

#[test]
fn cli_list_models_order_matches_typescript_locale_compare() {
    let fx = fixture();
    let expected: Vec<(String, String)> = fx["sorted"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (p[0].as_str().unwrap().to_string(), p[1].as_str().unwrap().to_string()))
        .collect();
    let mut actual = expected.clone();
    actual.reverse();
    actual.sort_by(|a, b| locale_compare(&a.0, &b.0).then_with(|| locale_compare(&a.1, &b.1)));
    assert_eq!(actual, expected);
}

#[test]
fn cli_format_token_count_rounds_like_to_fixed() {
    assert_eq!(format_token_count(999), "999");
    assert_eq!(format_token_count(1_000), "1K");
    assert_eq!(format_token_count(131_072), "131.1K");
    assert_eq!(format_token_count(131_250), "131.3K");
    assert_eq!(format_token_count(4_096), "4.1K");
    assert_eq!(format_token_count(1_000_000), "1M");
    assert_eq!(format_token_count(1_048_576), "1.0M");
    assert_eq!(format_token_count(1_050_000), "1.1M");
}

#[test]
fn cli_version_matches_typescript() {
    assert_eq!(cli::version(), fixture()["version"].as_str().unwrap());
}

/// Runs the `ultron` binary for every recorded argument vector in the same private layout the generator used and
/// compares stdout, stderr and exit code.
#[test]
fn cli_binary_matches_typescript_cli() {
    let fx = fixture();
    let repo = repo().display().to_string();
    for case in fx["process"].as_array().unwrap() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp_path = tmp.path().canonicalize().unwrap();
        std::fs::create_dir(tmp_path.join("work")).unwrap();
        std::fs::create_dir(tmp_path.join("agent")).unwrap();
        if let Some(auth) = case.get("auth") {
            std::fs::write(tmp_path.join("agent/auth.json"), auth.to_string()).unwrap();
        }
        if let Some(files) = case.get("files").and_then(Value::as_object) {
            for (p, text) in files {
                match text.as_str() {
                    Some(text) => std::fs::write(tmp_path.join(p), text).unwrap(),
                    None => std::fs::create_dir(tmp_path.join(p)).unwrap(),
                }
            }
        }
        let args = strings(&case["args"]);
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ultron"));
        cmd.args(&args)
            .current_dir(tmp_path.join("work"))
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &tmp_path)
            .env("ULTRON_CODING_AGENT_DIR", tmp_path.join("agent"))
            .env("ULTRON_SERVER_DIR", tmp_path.join("server"));
        if let Some(env) = case.get("env").and_then(Value::as_object) {
            for (k, v) in env {
                cmd.env(k, v.as_str().unwrap());
            }
        }
        let output = cmd.output().unwrap();
        let tmp_text = tmp_path.display().to_string();
        let sub = |b: &[u8]| {
            String::from_utf8_lossy(b)
                .replace(&tmp_text, "<TMP>")
                .replace(&repo, "<REPO>")
        };
        assert_eq!(
            sub(&output.stdout),
            case["stdout"].as_str().unwrap(),
            "stdout of {args:?}"
        );
        assert_eq!(
            sub(&output.stderr),
            case["stderr"].as_str().unwrap(),
            "stderr of {args:?}"
        );
        assert_eq!(
            output.status.code().map(i64::from),
            case["status"].as_i64(),
            "exit code of {args:?}"
        );
    }
}

#[test]
fn cli_bare_prompt_without_a_model_reports_the_setup_hint() {
    // `ultron "hello"` runs the prompt (native-command.ts); with no model set up it fails with `noModelHint`'s "none"
    // hint after the error.
    let tmp = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ultron"))
        .arg("hello")
        .current_dir(tmp.path())
        .env_clear()
        .env("HOME", tmp.path())
        .env("ULTRON_CODING_AGENT_DIR", tmp.path().join("agent"))
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "Error: No model selected: pass --model <provider>/<id>\nNo model is configured. Run \"ultron setup\", or set a provider key such as ANTHROPIC_API_KEY or OPENAI_API_KEY.\n"
    );
    assert_eq!(output.status.code(), Some(1));
}

#[test]
fn cli_session_selector_resolves_native_sessions() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("work");
    let dir = tmp.path().join("sessions");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(dir.join("a")).unwrap();
    let header = |id: &str, cwd: &Path| {
        format!(
            r#"{{"kind":"header","id":"{id}","createdAt":1,"cwd":"{}"}}"#,
            cwd.display()
        ) + "\n{}\n"
    };
    std::fs::write(dir.join("a/one.jsonl"), header("abc123", &cwd)).unwrap();
    std::fs::write(dir.join("a/two.jsonl"), header("abd456", &cwd)).unwrap();
    std::fs::write(dir.join("a/other.jsonl"), header("abz789", Path::new("/elsewhere"))).unwrap();
    std::fs::write(dir.join("a/notes.txt"), header("abq000", &cwd)).unwrap();
    let sessions = cli::session::list_native_sessions(&dir, &cwd).unwrap();
    let mut ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
    ids.sort();
    assert_eq!(ids, ["abc123", "abd456"]);
    let select = |s: &str| cli::session::session_id_from_selector(s, &sessions, &cwd);
    assert_eq!(select("abc"), Ok("abc123".to_string()));
    assert_eq!(select("abd456"), Ok("abd456".to_string()));
    assert_eq!(select("ab"), Err("No native session found matching 'ab'".to_string()));
    assert_eq!(select("abz"), Err("No native session found matching 'abz'".to_string()));
    assert_eq!(select("../sessions/a/other.jsonl"), Ok("abz789".to_string()));
    assert!(
        cli::session::list_native_sessions(&tmp.path().join("missing"), &cwd)
            .unwrap()
            .is_empty()
    );
}
