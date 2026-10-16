//! `ultron install|remove|update|list` against the TypeScript CLI's runs recorded by crates/cli/fixtures/packages.mjs.
//! Each run is replayed in the same private layout with the same fake npm (a python script on the run's PATH that
//! installs from a local registry file) and the real git against local repositories only (GIT_CONFIG_GLOBAL maps
//! https://github.com/ to file://<TMP>/remotes/). Compared: stdout, stderr, exit code, the npm log, the files left
//! under agent/ and work/.pi, and each checkout's HEAD. The self-update cases are served by a local registry stand-in
//! with the answers TypeScript got from its fetch stubs.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/packages.json")).expect("fixture parses")
}

fn tree(root: &Path, rel: &str, tmp: &str) -> Vec<Value> {
    let base = root.join(rel);
    let mut out = Vec::new();
    fn walk(base: &Path, dir: &str, tmp: &str, out: &mut Vec<Value>) {
        let Ok(read) = std::fs::read_dir(base.join(dir)) else {
            return;
        };
        let mut names: Vec<String> = read
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        for name in names {
            if name == ".git" {
                continue;
            }
            let p = if dir.is_empty() {
                name.clone()
            } else {
                format!("{dir}/{name}")
            };
            let meta = std::fs::symlink_metadata(base.join(&p)).unwrap();
            if meta.is_dir() {
                out.push(json!({"path": p, "kind": "dir"}));
                walk(base, &p, tmp, out);
            } else {
                let text = std::fs::read_to_string(base.join(&p))
                    .unwrap_or_default()
                    .replace(tmp, "<TMP>");
                out.push(json!({"path": p, "kind": "file", "text": text}));
            }
        }
    }
    walk(&base, "", tmp, &mut out);
    out
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

fn replay(f: &Value, case: &Value) -> Vec<String> {
    let tmp = tempfile::Builder::new()
        .prefix("ultron-pkg-fixture-")
        .tempdir_in("/tmp")
        .unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let tmp_text = root.display().to_string();
    let fill = |s: &str| s.replace("<TMP>", &tmp_text);
    for d in ["work", "agent", "server", "bin"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    let npm = root.join("bin/npm");
    std::fs::write(&npm, f["fakeNpm"].as_str().unwrap()).unwrap();
    std::fs::set_permissions(&npm, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    for name in case["bin"].as_array().into_iter().flatten() {
        std::os::unix::fs::symlink(&npm, root.join("bin").join(name.as_str().unwrap())).unwrap();
    }
    std::fs::write(root.join("registry.json"), f["registry"].as_str().unwrap()).unwrap();
    std::fs::write(
        root.join("gitconfig"),
        format!(
            "[url \"file://{tmp_text}/remotes/\"]\n\tinsteadOf = https://github.com/\n[advice]\n\tdetachedHead = false\n[init]\n\tdefaultBranch = main\n"
        ),
    )
    .unwrap();
    for (rel, text) in case["files"].as_object().into_iter().flatten() {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, fill(text.as_str().unwrap())).unwrap();
    }
    let mut env: HashMap<String, String> = HashMap::new();
    env.insert("PATH".into(), format!("{}:/usr/bin:/bin", root.join("bin").display()));
    env.insert("HOME".into(), tmp_text.clone());
    env.insert("TZ".into(), "UTC".into());
    env.insert(
        "ULTRON_CODING_AGENT_DIR".into(),
        root.join("agent").display().to_string(),
    );
    env.insert("ULTRON_SERVER_DIR".into(), root.join("server").display().to_string());
    env.insert("GIT_CONFIG_GLOBAL".into(), root.join("gitconfig").display().to_string());
    env.insert("FAKE_NPM_LOG".into(), root.join("npm.log").display().to_string());
    env.insert(
        "FAKE_NPM_REGISTRY".into(),
        root.join("registry.json").display().to_string(),
    );
    for (k, v) in f["gitEnv"].as_object().unwrap() {
        env.insert(k.clone(), v.as_str().unwrap().into());
    }
    for (k, v) in case["env"].as_object().into_iter().flatten() {
        env.insert(k.clone(), fill(v.as_str().unwrap()));
    }
    for command in case["setup"].as_array().into_iter().flatten() {
        let status = Command::new("/bin/sh")
            .args(["-c", command.as_str().unwrap()])
            .current_dir(&root)
            .env_clear()
            .envs(&env)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "setup {command}");
    }
    std::fs::write(root.join("npm.log"), "").unwrap();
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
        let endpoints = cli::package_cli::Endpoints {
            latest_version_url: format!("http://127.0.0.1:{port}/ultron-agent/latest"),
        };
        let code = cli::package_cli::run_package_with(&args, &run_env, &mut out, &mut err, &endpoints).unwrap();
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
            diffs.push(format!(
                "{} {what}:\nrust {}\nts   {}",
                case["name"],
                serde_json::to_string_pretty(&got).unwrap(),
                serde_json::to_string_pretty(want).unwrap()
            ));
        }
    };
    check("stdout", json!(sub(&stdout)), &case["stdout"]);
    check("stderr", json!(sub(&stderr)), &case["stderr"]);
    check("status", json!(status), &case["status"]);
    check("requests", json!(requests), &case["requests"]);
    let log = std::fs::read_to_string(root.join("npm.log")).unwrap_or_default();
    check("npm log", json!(sub(&log)), &case["npmLog"]);
    check("agent files", json!(tree(&root, "agent", &tmp_text)), &case["agent"]);
    check(
        "project files",
        json!(tree(&root, "work/.pi", &tmp_text)),
        &case["project"],
    );
    let heads: Vec<Value> = case["checkouts"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|rel| {
            let dir = root.join(rel.as_str().unwrap());
            if !dir.exists() {
                return Value::Null;
            }
            let out = Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&dir)
                .env_clear()
                .envs(&env)
                .output()
                .unwrap();
            json!(String::from_utf8_lossy(&out.stdout).trim())
        })
        .collect();
    check("heads", json!(heads), &case["heads"]);
    diffs
}

/// Cases by name (each test replays one group, so a failure names its area).
fn run(names: &[&str]) {
    let f = fixture();
    let mut diffs = Vec::new();
    let mut seen = 0;
    for case in f["runs"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        if names.iter().any(|n| name.starts_with(n)) {
            seen += 1;
            diffs.extend(replay(&f, case));
        }
    }
    assert!(seen > 0, "no cases for {names:?}");
    assert!(diffs.is_empty(), "{}", diffs.join("\n\n"));
}

#[test]
fn cli_packages_install_and_remove_npm_match_typescript() {
    run(&[
        "install npm",
        "install scoped",
        "install twice",
        "remove npm",
        "remove unknown",
    ]);
}

#[test]
fn cli_packages_update_npm_matches_typescript() {
    run(&["update npm", "update suggests", "update offline"]);
}

#[test]
fn cli_packages_git_install_update_remove_match_typescript() {
    run(&["install git", "update git", "remove git"]);
}

#[test]
fn cli_packages_list_reads_the_legacy_global_npm_root_like_typescript() {
    run(&["list legacy", "install local missing"]);
}

#[test]
fn cli_packages_self_update_check_matches_typescript() {
    run(&["update self", "update default", "update all"]);
}

/// The self-contained binary does not replace itself: with a newer release (or `--force`) it says where to download
/// it, as TypeScript's Bun binary does (`getSelfUpdateUnavailableInstruction` for `bun-binary`).
#[test]
fn cli_packages_self_update_points_the_binary_at_the_release_download() {
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
        let endpoints = cli::package_cli::Endpoints {
            latest_version_url: format!("http://127.0.0.1:{port}/ultron-agent/latest"),
        };
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let code = cli::package_cli::run_package_with(&args, &run_env, &mut out, &mut err, &endpoints);
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
