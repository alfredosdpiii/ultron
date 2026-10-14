//! The Rust acceptance gate against the Node instrument (A26): `acceptance-gate lock` and
//! `node scripts/acceptance-lock.mjs` run on identical temp roots (and the real checkout, read-only) and must agree
//! byte for byte (lock file, stdout, stderr, exit code); `apply_edits` against the Node `applyEdits`; the mutation
//! runner against a scripted cargo (killed, survived, build error, invalid edits, baseline not green, timeout, an
//! interrupted run's in-flight record written in the Node format), restoring every file.

use acceptance_gate::{MutationOptions, Profile, apply_edits, compute_lock, run_mutations, stringify_tabs};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn write(root: &Path, path: &str, text: &str) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A root with a manifest, a mutation list, the instrument scripts and evidence files (mixed case, nested, one
/// named only by a mutation).
fn lock_root() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let manifest = json!({"version": 1, "vitest_cwd": "packages/x", "rows": [
        {"id": "A01", "evidence": [{"file": "packages/x/test/b.test.ts", "tests": ["t"]}, {"file": "packages/x/test/A.test.ts", "tests": []}]},
        {"id": "A02", "evidence": [{"file": "packages/x/test/nested/c.test.ts"}, {"file": "packages/x/test/b.test.ts"}]},
        {"id": "A03"},
    ]});
    write(root, "acceptance/manifest.json", &stringify_tabs(&manifest));
    let mutations = json!({"version": 1, "mutations": [
        {"id": "m1", "row": "A01", "file": "packages/x/src/s.ts", "edits": [], "tests": ["packages/x/test/_m.test.ts"]},
    ]});
    write(root, "acceptance/mutations.json", &stringify_tabs(&mutations));
    for script in ["lock", "report", "mutation"] {
        write(
            root,
            &format!("scripts/acceptance-{script}.mjs"),
            &format!("// {script}\n"),
        );
    }
    for (file, text) in [
        ("packages/x/test/b.test.ts", "b\n"),
        ("packages/x/test/A.test.ts", "A\n"),
        ("packages/x/test/nested/c.test.ts", "c\n"),
        ("packages/x/test/_m.test.ts", "m\n"),
    ] {
        write(root, file, text);
    }
    dir
}

fn node_lock(root: &Path, mode: &str) -> (Option<i32>, String, String) {
    let output = Command::new("node")
        .arg(repo().join("scripts/acceptance-lock.mjs"))
        .args([mode, "--root"])
        .arg(root)
        .output()
        .expect("node runs (the Node instrument is the reference)");
    (
        output.status.code(),
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

fn rust_lock(root: &Path, mode: &str) -> (Option<i32>, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_acceptance-gate"))
        .args(["lock", mode, "--root"])
        .arg(root)
        .output()
        .unwrap();
    (
        output.status.code(),
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

#[test]
fn acceptance_gate_lock_matches_node_script() {
    let node = lock_root();
    let rust = lock_root();
    let both = |change: &dyn Fn(&Path)| {
        change(node.path());
        change(rust.path());
    };
    let lock = "acceptance/instrument.lock";
    let compare = |what: &str, mode: &str| {
        let expected = node_lock(node.path(), mode);
        assert_eq!(rust_lock(rust.path(), mode), expected, "{what}");
        let read = |root: &Path| std::fs::read(root.join(lock)).ok();
        assert_eq!(read(rust.path()), read(node.path()), "{what}: lock file");
        expected
    };
    assert_eq!(compare("missing lock", "--check").0, Some(1));
    assert_eq!(compare("write", "--write").0, Some(0));
    assert_eq!(compare("matches", "--check").1, "instrument lock matches\n");
    both(&|r| write(r, "packages/x/test/b.test.ts", "b changed\n"));
    both(&|r| write(r, "packages/x/test/_m.test.ts", "m changed\n"));
    both(&|r| write(r, "scripts/acceptance-report.mjs", "// changed\n"));
    let changed = compare("evidence and instrument changed", "--check");
    assert!(changed.1.contains("evidence changed: "), "{changed:?}");
    // A manifest change (a new row naming a new file) and a removed instrument script.
    both(&|r| {
        let mut manifest: Value =
            serde_json::from_str(&std::fs::read_to_string(r.join("acceptance/manifest.json")).unwrap()).unwrap();
        manifest["rows"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id": "A04", "evidence": [{"file": "packages/x/test/d.test.ts"}]}));
        write(r, "acceptance/manifest.json", &manifest.to_string());
        std::fs::remove_file(r.join("scripts/acceptance-mutation.mjs")).unwrap();
    });
    compare("manifest changed, file missing", "--check");
    assert_eq!(compare("write refuses missing files", "--write").0, Some(1));
    both(&|r| {
        write(r, "packages/x/test/d.test.ts", "d\n");
        write(r, "scripts/acceptance-mutation.mjs", "// back\n");
    });
    compare("rewrite", "--write");
    compare("rewritten lock matches", "--check");
    // A file only the recorded lock names (its row dropped from the manifest) is a change too.
    both(&|r| {
        let mut manifest: Value =
            serde_json::from_str(&std::fs::read_to_string(r.join("acceptance/manifest.json")).unwrap()).unwrap();
        manifest["rows"].as_array_mut().unwrap().pop();
        write(r, "acceptance/manifest.json", &manifest.to_string());
    });
    let dropped = compare("evidence dropped", "--check");
    assert!(
        dropped.1.contains("evidence changed: packages/x/test/d.test.ts"),
        "{dropped:?}"
    );
    both(&|r| write(r, lock, "{not json"));
    assert_eq!(
        compare("unreadable lock", "--check").1,
        "instrument lock is missing or unreadable\n"
    );
    // Usage: same exit code; the text names each program.
    assert_eq!(
        rust_lock(rust.path(), "--neither").0,
        node_lock(node.path(), "--neither").0
    );
    assert_eq!(node_lock(node.path(), "--neither").0, Some(2));
}

/// The real checkout's lock, computed by both without writing anything.
#[test]
fn acceptance_gate_lock_of_the_checkout_matches_node() {
    let script = format!(
        "const {{ computeLock }} = await import({:?}); process.stdout.write(JSON.stringify(computeLock({:?}), null, '\\t'));",
        repo().join("scripts/acceptance-lock.mjs").display().to_string(),
        repo().display().to_string()
    );
    let output = Command::new("node")
        .args(["--input-type=module", "-e", &script])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let ours = stringify_tabs(&compute_lock(&repo(), &Profile::node()).unwrap());
    assert_eq!(ours, String::from_utf8(output.stdout).unwrap());
}

/// The Rust profile locks every file under crates/acceptance/tests, the Rust evidence map and the gate itself.
#[test]
fn acceptance_gate_rust_profile_locks_rust_evidence() {
    let lock = compute_lock(&repo(), &Profile::rust()).unwrap();
    let evidence = lock["evidence"].as_object().unwrap();
    assert!(evidence.contains_key("crates/acceptance/tests/a56_shell_jobs.rs"));
    assert!(evidence.contains_key("crates/acceptance/tests/common/mod.rs"));
    let count = std::fs::read_dir(repo().join("crates/acceptance/tests"))
        .unwrap()
        .flatten()
        .filter(|e| e.file_type().unwrap().is_file())
        .count();
    assert!(evidence.len() > count, "{} files, {count} top-level", evidence.len());
    let instrument: Vec<&str> = lock["instrument"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        instrument,
        [
            "crates/acceptance/mutations.json",
            "acceptance/manifest.json",
            "crates/acceptance-gate/src/lib.rs",
            "crates/acceptance-gate/src/main.rs"
        ]
    );
    assert_eq!(
        lock["manifest"],
        json!(acceptance_gate::sha256_file(
            &repo().join("crates/acceptance/COVERAGE.md")
        ))
    );
}

#[test]
fn acceptance_gate_apply_edits_matches_node() {
    let cases = json!([
        ["a b a", [["b", "B"]]],
        ["one two", [["one", "1"], ["1 two", "done"]]],
        ["x", [["y", "z"]]],
        ["aa", [["a", "b"]]],
        ["abcabc", [["abc", "X"]]],
        ["héllo wörld", [["wörld", "world"], ["é", "e"]]],
        ["keep", []],
    ]);
    let script = format!(
        "const {{ applyEdits }} = await import({:?}); const cases = {cases}; process.stdout.write(JSON.stringify(cases.map(([s, e]) => applyEdits(s, e.map(([find, replace]) => ({{ find, replace }}))))));",
        repo().join("scripts/acceptance-mutation.mjs").display().to_string()
    );
    let output = Command::new("node")
        .args(["--input-type=module", "-e", &script])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let expected: Value = serde_json::from_slice(&output.stdout).unwrap();
    let ours: Vec<Value> = cases
        .as_array()
        .unwrap()
        .iter()
        .map(|case| {
            let edits: Vec<(String, String)> = case[1]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| (e[0].as_str().unwrap().into(), e[1].as_str().unwrap().into()))
                .collect();
            match apply_edits(case[0].as_str().unwrap(), &edits) {
                Ok(text) => json!({"text": text}),
                Err(error) => json!({"error": error}),
            }
        })
        .collect();
    assert_eq!(Value::Array(ours), expected);
}

/// A cargo stand-in: reads the mutated source and answers like libtest (or hangs, or fails to build).
const FAKE_CARGO: &str = r#"#!/bin/sh
src=$(cat crates/demo/src/lib.rs)
for arg in "$@"; do last=$arg; done
case "$last" in red) echo "test red_case ... FAILED"; exit 101;; esac
case "$src" in
  *KILL*) echo "running 2 tests"; echo "test demo_case ... FAILED"; echo "test other_case ... ok"; exit 101;;
  *BUILD*) echo "error[E0425]: cannot find value \`x\` in this scope" >&2; exit 101;;
  *HANG*) exec tail -f /dev/null;;
esac
echo "test demo_case ... ok"
exit 0
"#;

fn mutation_root(mutations: Value) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "crates/demo/Cargo.toml", "[package]\nname = \"demo-pkg\"\n");
    write(root, "crates/demo/src/lib.rs", "fn guard() -> bool { true }\n");
    write(root, "crates/demo/tests/green.rs", "");
    write(root, "crates/demo/tests/red.rs", "");
    write(root, "fake-cargo", FAKE_CARGO);
    write(root, "crates/acceptance/mutations.json", &stringify_tabs(&mutations));
    dir
}

fn mutation(id: &str, find: &str, replace: &str, tests: &[&str]) -> Value {
    json!({"id": id, "row": "A99", "file": "crates/demo/src/lib.rs", "guarantee": "g",
        "edits": [{"find": find, "replace": replace}], "tests": tests})
}

fn options(root: &Path, timeout_ms: u64) -> MutationOptions {
    MutationOptions {
        only: None,
        timeout: Duration::from_millis(timeout_ms),
        runner: vec!["sh".into(), root.join("fake-cargo").display().to_string()],
    }
}

#[test]
fn acceptance_gate_mutation_runner_classifies_and_restores() {
    let green = "crates/demo/tests/green.rs";
    let dir = mutation_root(json!({"version": 1, "mutations": [
        mutation("kill", "true", "KILL", &[green]),
        mutation("survive", "true", "false", &[green]),
        mutation("build", "true", "BUILD", &[green]),
        mutation("absent", "nowhere", "x", &[green]),
        {"id": "twice", "row": "A99", "file": "crates/demo/src/lib.rs", "edits": [{"find": "u", "replace": "v"}], "tests": [green]},
        mutation("red-baseline", "true", "KILL", &["crates/demo/tests/red.rs"]),
    ]}));
    let root = dir.path();
    let original = std::fs::read(root.join("crates/demo/src/lib.rs")).unwrap();
    let (code, log) = run_mutations(root, &Profile::rust(), &options(root, 120_000)).unwrap();
    assert_eq!(code, 1);
    assert_eq!(std::fs::read(root.join("crates/demo/src/lib.rs")).unwrap(), original);
    assert!(!root.join("crates/acceptance/.mutation-inflight.json").exists());
    assert_eq!(
        log,
        [
            "baseline crates/demo/tests/green.rs: green",
            "baseline crates/demo/tests/red.rs: not green (killed)",
            "kill (A99): killed",
            "survive (A99): survived",
            "build (A99): error",
            "Mutation slice: 1/6 killed, 1 survived, 2 error, 2 invalid. Wrote crates/acceptance/mutation.json",
        ]
    );
    let report: Value =
        serde_json::from_str(&std::fs::read_to_string(root.join("crates/acceptance/mutation.json")).unwrap()).unwrap();
    assert_eq!(
        report["summary"],
        json!({"total": 6, "killed": 1, "survived": 1, "error": 2, "invalid": 2})
    );
    assert_eq!(
        report["findings"],
        json!(["survive", "build", "absent", "twice", "red-baseline"])
    );
    let results = report["results"].as_array().unwrap();
    assert_eq!(results[0]["failing_tests"], json!(["demo_case"]));
    assert_eq!(results[0]["tests"], json!([green]));
    assert_eq!(
        results[2]["detail"],
        "suite failed outside its tests (exit 101): error[E0425]: cannot find value `x` in this scope"
    );
    assert_eq!(results[3]["detail"], "find string not present: nowhere");
    assert_eq!(results[4]["detail"], "find string is not unique: u");
    assert_eq!(results[5]["detail"], "baseline not green: crates/demo/tests/red.rs");
    assert_eq!(
        report["mutations_sha256"],
        json!(acceptance_gate::sha256_file(
            &root.join("crates/acceptance/mutations.json")
        ))
    );
    let generated = report["generated_at"].as_str().unwrap();
    assert_eq!((generated.len(), &generated[10..11], &generated[23..]), (24, "T", "Z"));
}

/// A run that never ends is killed at the deadline and is an error, not a kill; the file is restored.
#[test]
fn acceptance_gate_mutation_runner_times_out_and_restores() {
    let dir = mutation_root(json!({"version": 1, "mutations": [
        mutation("hang", "true", "HANG", &["crates/demo/tests/green.rs"]),
    ]}));
    let root = dir.path();
    let original = std::fs::read(root.join("crates/demo/src/lib.rs")).unwrap();
    // The deadline is the behavior under test: the scripted runner never ends, so it always fires.
    let (code, _) = run_mutations(root, &Profile::rust(), &options(root, 1_500)).unwrap();
    assert_eq!(code, 1);
    assert_eq!(std::fs::read(root.join("crates/demo/src/lib.rs")).unwrap(), original);
    let report: Value =
        serde_json::from_str(&std::fs::read_to_string(root.join("crates/acceptance/mutation.json")).unwrap()).unwrap();
    assert_eq!(report["results"][0]["outcome"], "error");
    assert_eq!(report["results"][0]["detail"], "test runner timed out after 1500 ms");
}

/// An interrupted run (Node's record format: the file and its original bytes in base64) is undone first; a runner
/// that cannot start is an infrastructure error, never a kill.
#[test]
fn acceptance_gate_mutation_runner_restores_interrupted_run() {
    let dir = mutation_root(json!({"version": 1, "mutations": [
        mutation("kill", "true", "KILL", &["crates/demo/tests/green.rs"]),
    ]}));
    let root = dir.path();
    let original = std::fs::read(root.join("crates/demo/src/lib.rs")).unwrap();
    let script = "const [orig] = process.argv.slice(1); process.stdout.write(JSON.stringify({ file: 'crates/demo/src/lib.rs', original: Buffer.from(orig).toString('base64') }));";
    let record = Command::new("node")
        .args(["-e", script, std::str::from_utf8(&original).unwrap()])
        .output()
        .unwrap();
    write(
        root,
        "crates/acceptance/.mutation-inflight.json",
        std::str::from_utf8(&record.stdout).unwrap(),
    );
    write(root, "crates/demo/src/lib.rs", "fn guard() -> bool { KILL }\n");
    let mut opts = options(root, 120_000);
    opts.runner = vec![root.join("no-such-cargo").display().to_string()];
    let (code, log) = run_mutations(root, &Profile::rust(), &opts).unwrap();
    assert_eq!(
        log[0],
        "Restored crates/demo/src/lib.rs from an interrupted mutation run"
    );
    assert_eq!(log[1], "baseline crates/demo/tests/green.rs: not green (error)");
    assert_eq!(code, 1);
    assert_eq!(std::fs::read(root.join("crates/demo/src/lib.rs")).unwrap(), original);
    assert!(!root.join("crates/acceptance/.mutation-inflight.json").exists());
}

/// `--only` limits the run, and the binary's exit code is the verdict.
#[test]
fn acceptance_gate_mutate_binary_runs_only_named() {
    let green = "crates/demo/tests/green.rs";
    let dir = mutation_root(json!({"version": 1, "mutations": [
        mutation("kill", "true", "KILL", &[green]),
        mutation("survive", "true", "false", &[green]),
    ]}));
    let root = dir.path();
    let runner = format!("sh {}", root.join("fake-cargo").display());
    let run = |only: &str| {
        Command::new(env!("CARGO_BIN_EXE_acceptance-gate"))
            .args(["mutate", "--only", only, "--runner", &runner, "--root"])
            .arg(root)
            .output()
            .unwrap()
    };
    let killed = run("kill");
    assert_eq!(killed.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&killed.stdout).ends_with(
        "Mutation slice: 1/1 killed, 0 survived, 0 error, 0 invalid. Wrote crates/acceptance/mutation.json\n"
    ));
    assert_eq!(run("survive").status.code(), Some(1));
}
