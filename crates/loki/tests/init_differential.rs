//! `loki init` without `--minimal`: the Rust binary against loki.py runs recorded in `fixtures/init-corpus.json` by
//! `fixtures/generate-init.mjs` (stdout, stderr, exit status of each run, then every file under the repository and
//! the managed-engine directory). Runs use a private HOME and a PATH holding only fake `python3` (and `loki`).

use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn corpus() -> Value {
    let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/init-corpus.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

/// The engine source root loki.py installed from: `loki.py`, `templates/`, `shim/`, `vendor/`.
fn source_file(path: &str) -> Vec<u8> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let engine = manifest.join("../../packages/coding-agent/src/ultron/loki-engine");
    let candidates = [engine.join(path), manifest.join("init-assets").join(path)];
    candidates
        .iter()
        .find_map(|candidate| std::fs::read(candidate).ok())
        .unwrap_or_else(|| panic!("no source file {path}"))
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

fn files(dir: &Path) -> BTreeMap<String, (Vec<u8>, u32)> {
    fn walk(dir: &Path, base: &Path, out: &mut BTreeMap<String, (Vec<u8>, u32)>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries {
            let path = entry.unwrap().path();
            let meta = std::fs::metadata(&path).unwrap();
            if meta.is_dir() {
                walk(&path, base, out);
            } else {
                let key = path.strip_prefix(base).unwrap().to_string_lossy().into_owned();
                out.insert(key, (std::fs::read(&path).unwrap(), meta.permissions().mode() & 0o777));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

fn executable(path: &Path) {
    std::fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

struct World {
    _dir: tempfile::TempDir,
    root: PathBuf,
    managed: PathBuf,
    home: PathBuf,
    bin: PathBuf,
}

impl World {
    fn new(case: &Value) -> World {
        let dir = tempfile::Builder::new().prefix("loki-init-").tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let world = World {
            root: base.join("repo"),
            managed: base.join("managed"),
            home: base.join("home"),
            bin: base.join("bin"),
            _dir: dir,
        };
        for path in [&world.root, &world.managed, &world.home, &world.bin] {
            std::fs::create_dir_all(path).unwrap();
        }
        executable(&world.bin.join("python3"));
        if case["lokiOnPath"] == true {
            executable(&world.bin.join("loki"));
        }
        let place = |base: &Path, map: &Value| {
            for (path, text) in map.as_object().unwrap() {
                let target = base.join(path);
                std::fs::create_dir_all(target.parent().unwrap()).unwrap();
                std::fs::write(&target, text.as_str().unwrap()).unwrap();
            }
        };
        place(&world.root, &case["files"]);
        place(&world.managed, &case["managedFiles"]);
        for path in case["dirs"].as_array().unwrap() {
            std::fs::create_dir_all(world.root.join(path.as_str().unwrap())).unwrap();
        }
        world
    }

    fn fill(&self, arg: &str) -> String {
        arg.replace("<ROOT>", self.root.to_str().unwrap())
            .replace("<MANAGED>", self.managed.to_str().unwrap())
    }

    fn normalize(&self, text: &str) -> String {
        text.replace(self.root.to_str().unwrap(), "<ROOT>")
            .replace(self.managed.to_str().unwrap(), "<MANAGED>")
            .replace(self.bin.join("python3").to_str().unwrap(), "<PYTHON>")
    }

    fn run(&self, args: &[String]) -> (i64, String, String) {
        let output = Command::new(env!("CARGO_BIN_EXE_loki"))
            .args(args)
            .current_dir(&self.root)
            .env_clear()
            .env("PATH", &self.bin)
            .env("HOME", &self.home)
            .env("LANG", "C.UTF-8")
            .output()
            .unwrap();
        (
            i64::from(output.status.code().unwrap()),
            self.normalize(&String::from_utf8(output.stdout).unwrap()),
            self.normalize(&String::from_utf8(output.stderr).unwrap()),
        )
    }

    /// Differences between `expected` (the recorded tree) and the files under `dir`.
    fn tree_diff(&self, dir: &Path, expected: &Value) -> Vec<String> {
        let actual = files(dir);
        let expected = expected.as_object().unwrap();
        let mut out = Vec::new();
        for key in actual.keys().filter(|k| !expected.contains_key(*k)) {
            out.push(format!("unexpected file {key}"));
        }
        for (key, want) in expected {
            let Some((bytes, mode)) = actual.get(key) else {
                out.push(format!("missing file {key}"));
                continue;
            };
            let readonly = want["readonly"] == true;
            if readonly != (*mode == 0o444) {
                out.push(format!("{key}: mode {mode:o}, readonly expected {readonly}"));
            }
            let same = if let Some(text) = want.as_str().or(want["text"].as_str()) {
                std::str::from_utf8(bytes).is_ok_and(|actual| self.normalize(actual) == text)
            } else if let Some(source) = want["same"].as_str() {
                *bytes == source_file(source)
            } else {
                want["sha256"].as_str() == Some(sha256(bytes).as_str())
            };
            if !same {
                out.push(format!(
                    "{key}: content differs\n--- expected\n{}\n--- actual\n{}",
                    want,
                    self.normalize(&String::from_utf8_lossy(bytes))
                ));
            }
        }
        out
    }
}

#[test]
fn loki_init_matches_loki_py_corpus() {
    let corpus = corpus();
    let cases = corpus["cases"].as_array().unwrap();
    assert!(cases.len() >= 40, "corpus has {} cases", cases.len());
    let mut failures = Vec::new();
    for case in cases {
        let world = World::new(case);
        for run in case["runs"].as_array().unwrap() {
            let args: Vec<String> = run["args"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| world.fill(a.as_str().unwrap()))
                .collect();
            let actual = world.run(&args);
            let expected = (
                run["status"].as_i64().unwrap(),
                run["stdout"].as_str().unwrap().to_string(),
                run["stderr"].as_str().unwrap().to_string(),
            );
            if actual != expected {
                failures.push(format!(
                    "{} {:?}:\n  expected {expected:?}\n  actual   {actual:?}",
                    case["name"], run["args"]
                ));
            }
        }
        for (dir, tree) in [(&world.root, &case["tree"]), (&world.managed, &case["managed"])] {
            for difference in world.tree_diff(dir, tree) {
                failures.push(format!("{}: {difference}", case["name"]));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} differences:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Hooks merged into an existing Claude Code settings file keep the user's handlers and drop stale Loki ones
/// (loki.py `merge_factory_hooks`): literal expectations, independent of the corpus.
#[test]
fn loki_init_merge_keeps_foreign_handlers_and_drops_stale_loki_ones() {
    use serde_json::json;
    let mut existing = json!({
        "PreToolUse": [
            {"matcher": "Edit", "hooks": [
                {"type": "command", "command": "python3 .loki/loki.py protect"},
                {"type": "command", "command": "echo keep"}
            ]},
            {"matcher": "Write", "hooks": [{"type": "command", "command": "python3 x/.loki/loki.py hook"}]},
            "opaque"
        ]
    })
    .as_object()
    .unwrap()
    .clone();
    let fragment = json!({"PreToolUse": [{"matcher": "New", "hooks": []}], "Stop": [1]});
    loki::init::merge_factory_hooks(&mut existing, fragment.as_object().unwrap()).unwrap();
    assert_eq!(
        serde_json::Value::Object(existing),
        json!({
            "PreToolUse": [
                {"matcher": "Edit", "hooks": [{"type": "command", "command": "echo keep"}]},
                "opaque",
                {"matcher": "New", "hooks": []}
            ],
            "Stop": [1]
        })
    );
}
