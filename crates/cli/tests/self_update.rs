//! `ultron update` (the self-update and its checks) against the TypeScript CLI's runs recorded by
//! crates/cli/fixtures/packages.mjs: the `update self` cases, served by a local registry stand-in with the answers
//! TypeScript got from its fetch stubs (or offline, as a process). Compared: stdout, stderr, exit code and the number
//! of registry requests. The package commands and `update`'s package targets are not supported by ultron-rs (Pi
//! packages): literal expectations.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/packages.json")).expect("fixture parses")
}

/// A registry stand-in: answers each request with the next stub (the last one repeats); counts requests.
fn registry(stubs: Vec<Value>) -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let count = Arc::new(AtomicUsize::new(0));
    let seen = count.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                    break;
                }
            }
            let index = seen.fetch_add(1, Ordering::SeqCst);
            let stub = &stubs[index.min(stubs.len() - 1)];
            let body = match &stub["body"] {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            let status = stub["status"].as_u64().unwrap();
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let mut rest = Vec::new();
            let _ = stream.flush();
            let _ = reader.read_to_end(&mut rest);
        }
    });
    (port, count)
}

fn replay(case: &Value) -> Vec<String> {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let tmp_text = root.display().to_string();
    for d in ["work", "agent", "server"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    let mut env: HashMap<String, String> = HashMap::new();
    env.insert("PATH".into(), "/usr/bin:/bin".into());
    env.insert("HOME".into(), tmp_text.clone());
    env.insert("TZ".into(), "UTC".into());
    env.insert(
        "ULTRON_CODING_AGENT_DIR".into(),
        root.join("agent").display().to_string(),
    );
    env.insert("ULTRON_SERVER_DIR".into(), root.join("server").display().to_string());
    for (k, v) in case["env"].as_object().into_iter().flatten() {
        env.insert(k.clone(), v.as_str().unwrap().replace("<TMP>", &tmp_text));
    }
    let args: Vec<String> = case["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap().to_string())
        .collect();
    let (stdout, stderr, status, requests) = if let Some(stubs) = case["stubs"].as_array() {
        // In process, with the registry URL pointed at the stand-in.
        let (port, count) = registry(stubs.clone());
        let vars = env.clone();
        let run_env = cli::Env {
            cwd: root.join("work"),
            home: root.clone(),
            var: Arc::new(move |k| vars.get(k).cloned()),
            docs: root.join("docs"),
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let endpoints = cli::update_cli::Endpoints {
            latest_version_url: format!("http://127.0.0.1:{port}/ultron-agent/latest"),
        };
        let code = cli::update_cli::run_update_with(&args, &run_env, &mut out, &mut err, &endpoints).unwrap();
        (
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
            Some(i64::from(code)),
            count.load(Ordering::SeqCst),
        )
    } else {
        let output = Command::new(env!("CARGO_BIN_EXE_ultron"))
            .args(&args)
            .current_dir(root.join("work"))
            .env_clear()
            .envs(&env)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        (
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
            output.status.code().map(i64::from),
            0,
        )
    };
    let sub = |s: &str| s.replace(&tmp_text, "<TMP>");
    let mut diffs = Vec::new();
    let mut check = |what: &str, got: Value, want: &Value| {
        if got != *want {
            diffs.push(format!("{} {what}: rust {got}, ts {want}", case["name"]));
        }
    };
    check("stdout", json!(sub(&stdout)), &case["stdout"]);
    check("stderr", json!(sub(&stderr)), &case["stderr"]);
    check("status", json!(status), &case["status"]);
    check("requests", json!(requests), &case["requests"]);
    diffs
}

#[test]
fn cli_self_update_check_matches_typescript() {
    let f = fixture();
    let mut diffs = Vec::new();
    let mut seen = 0;
    for case in f["runs"].as_array().unwrap() {
        if case["name"].as_str().unwrap().starts_with("update self") {
            seen += 1;
            diffs.extend(replay(case));
        }
    }
    assert_eq!(seen, 5, "the recorded `update self` cases");
    assert!(diffs.is_empty(), "{}", diffs.join("\n"));
}

/// Pi packages are not supported by ultron-rs: the package commands and `update`'s package targets say so and fail,
/// before any registry or npm is reached; `update --models` with a positional target is a usage error.
#[test]
fn cli_self_update_package_commands_and_targets_are_not_supported() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let agent = root.join("agent").display().to_string();
    let run_env = cli::Env {
        cwd: root.clone(),
        home: root.clone(),
        var: Arc::new(move |k| (k == "ULTRON_CODING_AGENT_DIR").then(|| agent.clone())),
        docs: root.join("docs"),
    };
    let (port, count) = registry(vec![json!({"status": 200, "body": {"version": "999.0.0"}})]);
    let endpoints = cli::update_cli::Endpoints {
        latest_version_url: format!("http://127.0.0.1:{port}/ultron-agent/latest"),
    };
    let run = |args: &[&str]| {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = cli::update_cli::run_update_with(&args, &run_env, &mut out, &mut err, &endpoints);
        (code, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
    };
    for args in [
        &["install", "npm:@foo/bar"][..],
        &["remove", "npm:@foo/bar"],
        &["uninstall", "npm:@foo/bar"],
        &["list"],
        &["update", "--extensions"],
        &["update", "--all"],
        &["update", "--extension", "npm:@foo/bar"],
        &["update", "npm:@foo/bar"],
        &["update", "self", "--extensions"],
    ] {
        assert_eq!(
            run(args),
            (
                Some(1),
                String::new(),
                "Pi packages are not supported by ultron-rs\n".to_string()
            ),
            "{args:?}"
        );
    }
    assert_eq!(
        run(&["update", "self", "--models"]),
        (
            Some(1),
            String::new(),
            "--models cannot be combined with --self or a positional target\nUsage: ultron update [self] [--self|--models] [--force]\n"
                .to_string()
        )
    );
    assert_eq!(
        run(&["update", "--bogus"]),
        (
            Some(1),
            String::new(),
            "Unknown option --bogus for \"update\".\nUse \"ultron --help\" or \"ultron update [self] [--self|--models] [--force]\".\n"
                .to_string()
        )
    );
    assert_eq!(run(&["config-other"]).0, None, "not an update command");
    assert_eq!(count.load(Ordering::SeqCst), 0, "nothing asked the registry");
}

/// The self-contained binary does not replace itself: with a newer release (or `--force`) it says where to download
/// it, as TypeScript's Bun binary does (`getSelfUpdateUnavailableInstruction` for `bun-binary`).
#[test]
fn cli_self_update_points_the_binary_at_the_release_download() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    for (args, version) in [
        (vec!["update", "--self"], "999.0.0".to_string()),
        (vec!["update", "--self", "--force"], cli::version()),
    ] {
        let (port, count) = registry(vec![json!({"status": 200, "body": {"version": version}})]);
        let agent = root.join("agent").display().to_string();
        let run_env = cli::Env {
            cwd: root.clone(),
            home: root.clone(),
            var: Arc::new(move |k| (k == "ULTRON_CODING_AGENT_DIR").then(|| agent.clone())),
            docs: root.join("docs"),
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let endpoints = cli::update_cli::Endpoints {
            latest_version_url: format!("http://127.0.0.1:{port}/ultron-agent/latest"),
        };
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let code = cli::update_cli::run_update_with(&args, &run_env, &mut out, &mut err, &endpoints);
        assert_eq!(code, Some(1), "{args:?}");
        assert_eq!(String::from_utf8(out).unwrap(), "");
        let exe = std::env::current_exe().unwrap();
        assert_eq!(
            String::from_utf8(err).unwrap(),
            format!(
                "error: ultron cannot self-update this installation.\nDownload from: https://github.com/alfredosdpiii/ultron/releases/latest\n\nLocation of ultron executable: {}\n",
                exe.display()
            )
        );
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}
