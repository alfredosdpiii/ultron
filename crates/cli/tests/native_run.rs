//! Replays `fixtures/native_run.json` (recorded from the TypeScript CLI by `fixtures/native_run.mjs`): the native
//! command's prompt assembly (piped stdin, `@file` text and images, messages without `-p`), runs without a prompt on
//! a pipe, and the session flags `--system-prompt`, `--no-skills`, `--no-context-files`. The Rust binary runs the
//! same arguments against the same scripted provider and must print the same output and send the same request
//! bodies (system prompt, messages, tools).
//!
//! Two recorded differences are deliberate (decisions.tsv): a prompt without `-p` streams its answer in Rust (the
//! TypeScript client looks for `frame` on transcript `message_update` events, which its transcript service strips,
//! so it prints only the closing newline); and a start without a usable model reports why in Rust where TypeScript
//! says "Internal server error" (the setup hint after it is the same).

mod common;

use common::{Naming, Sandbox, mock_server};
use serde_json::Value;
use std::io::Write;
use std::process::Stdio;

fn root_dir() -> String {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
        .display()
        .to_string()
}

/// The fixture generator's `normalize`: temp dir, checkout, session directory name, clock lines, printed ids, the
/// setup sessions' ids.
fn normalize(text: &str, tmp: &str, sessions: &[String]) -> String {
    let tmp_name = tmp.trim_start_matches('/').replace(['/', '\\', ':'], "-");
    let mut text = text
        .replace(tmp, "<TMP>")
        .replace(&root_dir(), "<root>")
        .replace(&tmp_name, "<TMPNAME>");
    while let Some(i) = text.find("[clock] 2") {
        let end = i + "[clock] ".len() + "2026-10-10 18:53 UTC+00:00".len();
        text.replace_range(i..end, "[clock] <CLOCK>");
    }
    for key in ["serverId", "sessionId"] {
        let marker = format!("\"{key}\":\"");
        if let Some(i) = text.find(&marker) {
            let start = i + marker.len();
            let end = start + text[start..].find('"').unwrap();
            text.replace_range(start..end, "<ID>");
        }
    }
    for (n, id) in sessions.iter().enumerate() {
        text = text.replace(id, &format!("<SESSION:{n}>"));
    }
    text
}

/// The server id leading each line of a run without a prompt.
fn server_prefix(stdout: &str) -> String {
    stdout
        .lines()
        .map(|line| match line.split_once('\t') {
            Some((_, rest)) => format!("<SERVER>\t{rest}\n"),
            None => format!("{line}\n"),
        })
        .collect()
}

fn session_ids(dir: &std::path::Path) -> Vec<String> {
    common::session_files(dir)
        .iter()
        .filter_map(|f| {
            let text = std::fs::read_to_string(f).ok()?;
            let header: Value = serde_json::from_str(text.lines().next()?).ok()?;
            header["id"].as_str().map(str::to_string)
        })
        .collect()
}

fn command(sandbox: &Sandbox, case: &Value, args: &[String]) -> std::process::Command {
    let mut command = sandbox.ultron();
    // Nothing here launches a program by name from PATH but the kernel's interpreter.
    command.env("PATH", "/usr/bin:/bin").env("PI_OFFLINE", "1").args(args);
    if case["noKey"] == true {
        command.env_remove("ANTHROPIC_API_KEY");
    }
    command
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap().to_string())
        .collect()
}

/// Image data of a resized image differs (Photon vs the `image` crate); its type and the text do not.
fn without_resized_data(mut body: Value) -> Value {
    for message in body["messages"].as_array_mut().into_iter().flatten() {
        for block in message["content"].as_array_mut().into_iter().flatten() {
            if block["type"] == "image" {
                block["source"]["data"] = Value::String("<DATA>".into());
            }
        }
    }
    body
}

#[test]
fn cli_native_run_matches_typescript() {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/native_run.json")).unwrap();
    for case in fixture.as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let replies: Vec<String> = strings(&case["replies"]);
        let setup_replies: Vec<String> = case["setup"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|s| strings(&s["replies"]))
            .collect();
        let (port, seen) = mock_server(
            setup_replies.iter().chain(&replies).cloned().collect(),
            Naming::Title("Scripted title".into()),
        );
        let sandbox = Sandbox::new(port);
        let tmp = sandbox.root.display().to_string();
        for (path, content) in case["files"].as_object().unwrap() {
            let path = sandbox.root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let content = content.as_str().unwrap();
            let bytes = match content.strip_prefix("base64:") {
                Some(data) => {
                    use base64::Engine;
                    base64::engine::general_purpose::STANDARD.decode(data).unwrap()
                }
                None => content.as_bytes().to_vec(),
            };
            std::fs::write(path, bytes).unwrap();
        }
        for step in case["setup"].as_array().unwrap() {
            let output = command(&sandbox, case, &strings(&step["args"])).output().unwrap();
            assert!(output.status.success(), "{name} setup: {output:?}");
        }
        let setup_requests = seen.lock().unwrap().requests.len();
        let sessions = session_ids(&sandbox.sessions_dir());
        let mut child = command(&sandbox, case, &strings(&case["args"]))
            .stdin(if case["stdin"].is_null() {
                Stdio::null()
            } else {
                Stdio::piped()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        if let Some(stdin) = case["stdin"].as_str() {
            child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
        }
        let output = child.wait_with_output().unwrap();
        let stdout = server_prefix(&normalize(&String::from_utf8(output.stdout).unwrap(), &tmp, &sessions));
        let stderr = normalize(&String::from_utf8(output.stderr).unwrap(), &tmp, &sessions);
        let want_stdout = case["stdout"].as_str().unwrap();
        if name.ends_with("-streams") || name.ends_with("-stream") {
            // Rust streams the answer; TypeScript prints only the newline after it.
            let reply_text = ["Hi there.", "Both."][usize::from(name.starts_with("two"))];
            assert_eq!(stdout, format!("{reply_text}{want_stdout}"), "{name} stdout");
        } else {
            assert_eq!(stdout, want_stdout, "{name} stdout");
        }
        let want_stderr = case["stderr"].as_str().unwrap();
        if name.starts_with("hint-") {
            // The error line says why in Rust; the hint after it is TypeScript's.
            let (first, hint) = stderr.split_once('\n').unwrap();
            assert!(first.starts_with("Error: "), "{name} stderr: {stderr}");
            assert_eq!(hint, want_stderr.split_once('\n').unwrap().1, "{name} hint");
        } else {
            assert_eq!(stderr, want_stderr, "{name} stderr");
        }
        assert_eq!(
            output.status.code(),
            case["code"].as_i64().map(|c| c as i32),
            "{name} exit code"
        );
        let got: Vec<Value> = seen.lock().unwrap().requests[setup_requests..]
            .iter()
            .map(|r| serde_json::from_str(&normalize(&r.to_string(), &tmp, &sessions)).unwrap())
            .collect();
        let want = case["requests"].as_array().unwrap();
        assert_eq!(got.len(), want.len(), "{name} request count");
        for (i, (g, w)) in got.iter().zip(want).enumerate() {
            let (g, w) = if name == "file-image-resized" {
                (
                    without_resized_data(g["body"].clone()),
                    without_resized_data(w["body"].clone()),
                )
            } else {
                (g["body"].clone(), w["body"].clone())
            };
            for field in ["system", "messages", "tools"] {
                assert_eq!(g[field], w[field], "{name} request {i} body.{field}");
            }
        }
    }
}
