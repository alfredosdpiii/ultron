//! Replays `fixtures/shell-writes-corpus.json` (loki.py `shell-writes`, recorded by `fixtures/shell_writes.mjs`)
//! through the library call the host uses and through the `loki` binary.

use serde_json::{Value, json};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

fn corpus() -> Value {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/shell-writes-corpus.json"
    ))
    .unwrap();
    serde_json::from_str(&text).unwrap()
}

/// A fresh repository for `case` (under `tmp/repo`) and the envelope sent on stdin.
fn setup(tmp: &Path, base: &Value, case: &Value) -> (std::path::PathBuf, Value) {
    let root = tmp.join("repo");
    for files in [base, &case["files"]] {
        for (path, content) in files.as_object().into_iter().flatten() {
            let target = root.join(path);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::write(&target, content.as_str().unwrap()).unwrap();
        }
    }
    if case["git"] != false {
        let status = Command::new("git")
            .args(["init", "-q"])
            .current_dir(&root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .unwrap();
        assert!(status.success());
    }
    let payload = if case["payload"].is_null() {
        let mut payload = json!({"command": case["command"]});
        if let Some(cwd) = case["cwd"].as_str() {
            payload["cwd"] = json!(root.join(cwd).to_str().unwrap());
        }
        payload
    } else {
        case["payload"].clone()
    };
    (root, payload)
}

fn compare(run: impl Fn(&Path, &Value) -> (i64, String)) {
    let corpus = corpus();
    let cases = corpus["cases"].as_array().unwrap();
    assert!(cases.len() >= 90, "corpus has {} cases", cases.len());
    let mut failures = Vec::new();
    for case in cases {
        let tmp = tempfile::tempdir().unwrap();
        let tmp_path = tmp.path().canonicalize().unwrap();
        let (root, payload) = setup(&tmp_path, &corpus["base"], case);
        let (status, stderr) = run(&root, &payload);
        let stderr = stderr.replace(root.to_str().unwrap(), "<ROOT>");
        let expected = &case["expected"];
        if status != expected["status"].as_i64().unwrap() || stderr != expected["stderr"].as_str().unwrap() {
            failures.push(format!(
                "{} {:?}: expected [{}] {:?}, got [{status}] {stderr:?}",
                case["name"], case["command"], expected["status"], expected["stderr"]
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} cases differ:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn loki_shell_writes_library_matches_loki_py_corpus() {
    compare(|root, payload| {
        let (status, stderr) = loki::shell_writes::run(root, payload);
        (i64::from(status), stderr)
    });
}

#[test]
fn loki_shell_writes_binary_matches_loki_py_corpus() {
    compare(|root, payload| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_loki"))
            .args(["--root", root.to_str().unwrap(), "shell-writes", "--harness", "ultron"])
            .current_dir(root)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", root.parent().unwrap())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.stdout.is_empty());
        (
            i64::from(output.status.code().unwrap()),
            String::from_utf8(output.stderr).unwrap(),
        )
    });
}

/// `shlex.shlex(posix=True, punctuation_chars=True)` with whitespace_split, against values printed by
/// `python3 -c 'import shlex; l = shlex.shlex(S, posix=True, punctuation_chars=True); l.whitespace_split = True;
/// l.commenters = ""; print(list(l))'`.
#[test]
fn loki_shell_tokens_match_python_shlex() {
    let cases: [(&str, &[&str]); 7] = [
        ("echo a;>x.c", &["echo", "a", ";>", "x.c"]),
        ("a&&b||c", &["a", "&&", "b", "||", "c"]),
        ("x>'y z'\"q\\\"r\"", &["x", ">", "y zq\"r"]),
        ("echo '' # not a comment", &["echo", "", "#", "not", "a", "comment"]),
        ("a\\ b \"c\\d\"", &["a b", "c\\d"]),
        ("(echo){x}", &["(", "echo", ")", "{x}"]),
        ("2>&1 >|f", &["2", ">&", "1", ">|", "f"]),
    ];
    for (input, expected) in cases {
        assert_eq!(loki::shell_writes::shlex_tokens(input).unwrap(), expected, "{input:?}");
    }
    assert_eq!(
        loki::shell_writes::shlex_tokens("'a").unwrap_err(),
        "No closing quotation"
    );
    assert_eq!(
        loki::shell_writes::shlex_tokens("a\\").unwrap_err(),
        "No escaped character"
    );
}

/// `shell_write_targets` against values printed by loki.py
/// (`python3 -I -c 'import loki; print(loki.shell_write_targets(C))'` in loki-engine/).
#[test]
fn loki_shell_write_targets_match_loki_py() {
    let cases: [(&str, &[(&str, &str)]); 4] = [
        (
            "echo > /dev/fd/3 > /proc/self/fd/2 >/dev/null > a.c",
            &[("a.c", "redirect")],
        ),
        ("cp -t d x y/z", &[("d/x", "cp"), ("d/z", "cp")]),
        ("tee a b | cat >> c", &[("a", "tee"), ("b", "tee"), ("c", "redirect")]),
        ("sed -i -e s/a/b/ f g", &[("f", "sed -i"), ("g", "sed -i")]),
    ];
    let dir = tempfile::tempdir().unwrap();
    for (command, expected) in cases {
        let expected: Vec<(String, String)> = expected.iter().map(|(t, h)| (t.to_string(), h.to_string())).collect();
        assert_eq!(
            loki::shell_writes::shell_write_targets(command, dir.path()),
            Some(expected),
            "{command:?}"
        );
    }
}
