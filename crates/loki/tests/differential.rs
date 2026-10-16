//! Differential tests: the Rust `loki` binary against loki.py output recorded in
//! `fixtures/protect-corpus.json` by `fixtures/generate.mjs`.

use serde_json::Value;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

fn corpus() -> Value {
    let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/protect-corpus.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

fn expand(text: &str, tokens: &Value) -> String {
    let mut out = text.to_string();
    for (name, parts) in tokens.as_object().unwrap() {
        let joined: String = parts.as_array().unwrap().iter().map(|p| p.as_str().unwrap()).collect();
        out = out.replace(&format!("@@{name}@@"), &joined);
    }
    out
}

struct Outcome {
    status: i32,
    stdout: String,
    stderr: String,
}

fn run(root: &Path, args: &[String], env: &[(String, String)], stdin: &str) -> Outcome {
    let mut child = Command::new(env!("CARGO_BIN_EXE_loki"))
        .args(args)
        .current_dir(root)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", root)
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
    let output = child.wait_with_output().unwrap();
    let root = root.to_str().unwrap();
    Outcome {
        status: output.status.code().unwrap(),
        stdout: String::from_utf8(output.stdout).unwrap().replace(root, "<ROOT>"),
        stderr: String::from_utf8(output.stderr).unwrap().replace(root, "<ROOT>"),
    }
}

fn replay(case: &Value, tokens: &Value) -> Outcome {
    let dir = tempfile::Builder::new().prefix("loki-fixture-").tempdir().unwrap();
    let root = dir.path();
    for (path, content) in case["files"].as_object().into_iter().flatten() {
        let target = root.join(path);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        match content.as_str() {
            Some(text) => std::fs::write(&target, expand(text, tokens)).unwrap(),
            None => std::os::unix::fs::symlink(content["symlink"].as_str().unwrap(), &target).unwrap(),
        }
    }
    let args: Vec<String> = case["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap().to_string())
        .collect();
    let root_arg = vec!["--root".to_string(), root.to_str().unwrap().to_string()];
    let argv = if case["rootAfter"] == true {
        [args, root_arg].concat()
    } else {
        [root_arg, args].concat()
    };
    let env: Vec<(String, String)> = case["env"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
        .collect();
    run(root, &argv, &env, &expand(case["stdin"].as_str().unwrap_or(""), tokens))
}

#[test]
fn loki_protect_matches_loki_py_corpus() {
    let corpus = corpus();
    let cases = corpus["cases"].as_array().unwrap();
    assert!(cases.len() >= 90, "corpus has {} cases", cases.len());
    let mut failures = Vec::new();
    for case in cases {
        let expected = &case["expected"];
        let actual = replay(case, &corpus["tokens"]);
        let same = actual.status as i64 == expected["status"].as_i64().unwrap()
            && actual.stdout == expected["stdout"].as_str().unwrap()
            && actual.stderr == expected["stderr"].as_str().unwrap();
        if !same {
            failures.push(format!(
                "{}:\n  expected [{}] stdout={:?} stderr={:?}\n  actual   [{}] stdout={:?} stderr={:?}",
                case["name"],
                expected["status"],
                expected["stdout"],
                expected["stderr"],
                actual.status,
                actual.stdout,
                actual.stderr
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} cases differ:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
}

#[test]
fn loki_init_without_minimal_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_str().unwrap().to_string();
    let args = |rest: &[&str]| -> Vec<String> {
        ["--root", root.as_str()]
            .iter()
            .chain(rest)
            .map(|s| s.to_string())
            .collect()
    };
    let init = run(dir.path(), &args(&["init", "--dir", root.as_str()]), &[], "");
    assert_eq!(init.status, 1);
    assert!(
        init.stderr
            .starts_with("loki: guard error: NotPorted: init without --minimal"),
        "{}",
        init.stderr
    );
    assert!(!dir.path().join(".loki").exists());
}

/// `protect` stops at MAX_VIOLATIONS (40) lines, against `loki.py --root R protect --file .loki/f0 ... --file
/// .loki/f44` (exit 2, stderr the first 40 refusals, stdout empty).
#[test]
fn loki_protect_reports_at_most_forty_violations_like_loki_py() {
    let dir = tempfile::tempdir().unwrap();
    let files: Vec<String> = (0..45).map(|i| format!(".loki/f{i}")).collect();
    let outcome = loki::run::protect(dir.path(), &files, None, false, || unreachable!("no preview"));
    let expected: String = (0..40)
        .map(|i| format!("loki: protected file: .loki/f{i} (set LOKI_ALLOW_PROTECTED=1 to override)\n"))
        .collect();
    assert_eq!(
        (outcome.status, outcome.stdout.as_str(), outcome.stderr.as_str()),
        (2, "", expected.as_str())
    );
}
