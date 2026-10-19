//! Spawn contracts in the subagent host (decision row 210): parsing and refusals of `role` / `contract` / `hop`, and
//! the host's checks on a child that ended, with scripted child runs in a temporary workspace (the session-level
//! behaviour is in session-core/tests/spawn_contracts.rs). New in Rust: literal expectations.

use delegation::host::{ChildRunner, Delegation, DelegationOptions};
use delegation::spawn_contract::{
    CheckPlace, CommandCheck, Expect, Hop, HostCheck, SpawnContract, is_dependency_file, plain_check_runner,
};
use delegation::workspace::WorkspaceDiff;
use kernel::Signal;
use serde_json::{Map, Value, json};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn kwargs(value: Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap()
}

fn parse(value: Value) -> Result<Option<SpawnContract>, String> {
    SpawnContract::parse(&kwargs(value))
}

#[test]
fn delegation_spawn_contract_parses_checks_returns_and_hops() {
    assert_eq!(parse(json!({"name": "c"})).unwrap(), None);
    let contract = parse(json!({
        "role": "fixer",
        "contract": {"checks": [
            {"run": "cargo test -q"},
            {"run": "make", "expect": "exit 2", "timeout_ms": 5},
            {"run": "x", "expect": {"exit": 1}},
            {"run": "y", "expect": {"stdout_contains": "ok"}},
            {"changed_files_within": ["src/**", "*.md"]},
            {"no_new_dependencies": true},
            {"files_exist": ["a/b.txt"]},
        ], "returns": {"type": "object"}},
        "hop": {"retry": 2},
    }))
    .unwrap()
    .unwrap();
    assert_eq!(contract.role.as_deref(), Some("fixer"));
    assert_eq!(
        contract.checks,
        vec![
            HostCheck::Run {
                command: "cargo test -q".into(),
                expect: Expect::Exit(0),
                timeout_ms: 120_000
            },
            HostCheck::Run {
                command: "make".into(),
                expect: Expect::Exit(2),
                timeout_ms: 5
            },
            HostCheck::Run {
                command: "x".into(),
                expect: Expect::Exit(1),
                timeout_ms: 120_000
            },
            HostCheck::Run {
                command: "y".into(),
                expect: Expect::StdoutContains("ok".into()),
                timeout_ms: 120_000
            },
            HostCheck::ChangedFilesWithin(vec!["src/**".into(), "*.md".into()]),
            HostCheck::NoNewDependencies,
            HostCheck::FilesExist(vec!["a/b.txt".into()]),
        ]
    );
    assert_eq!(contract.returns, Some(json!({"type": "object"})));
    assert_eq!(contract.hop, Hop::Retry(2));
    assert_eq!(
        parse(json!({"contract": {"checks": [{"run": "t"}]}, "hop": "report"}))
            .unwrap()
            .unwrap()
            .hop,
        Hop::Report
    );
    assert_eq!(
        parse(json!({"contract": {"checks": [{"run": "t"}]}, "hop": {"review": "look"}}))
            .unwrap()
            .unwrap()
            .hop,
        Hop::Review("look".into())
    );
    // A role alone is a contract with nothing to check.
    assert!(!parse(json!({"role": "r"})).unwrap().unwrap().checks_anything());
}

#[test]
fn delegation_spawn_contract_refuses_malformed_options() {
    let cases = [
        (
            json!({"contract": [{"run": "x"}]}),
            "contract must be a dict: {\"checks\": [...], \"returns\": <JSON schema of rlm.finish outputs>}",
        ),
        (
            json!({"contract": {"checks": [], "extra": 1}}),
            "Unknown contract field: extra",
        ),
        (
            json!({"contract": {"checks": [{"run": ""}]}}),
            "contract.checks[0].run must be a nonempty string of at most 4000 characters",
        ),
        (
            json!({"contract": {"checks": [{"run": "x", "expect": "exit"}]}}),
            "contract.checks[0].expect must be \"exit 0\" (or \"exit N\"), {\"exit\": N} or {\"stdout_contains\": \"text\"}",
        ),
        (
            json!({"contract": {"checks": [{"run": "x", "expect": {"exit": 0, "stdout_contains": "a"}}]}}),
            "contract.checks[0].expect must be \"exit 0\" (or \"exit N\"), {\"exit\": N} or {\"stdout_contains\": \"text\"}",
        ),
        (
            json!({"contract": {"checks": [{"run": "x", "timeout_ms": 600_001}]}}),
            "contract.checks[0].timeout_ms must be an integer from 1 to 600000",
        ),
        (
            json!({"contract": {"checks": [{"run": "x", "cwd": "/"}]}}),
            "Unknown contract check field: cwd",
        ),
        (
            json!({"contract": {"checks": [{"changed_files_within": []}]}}),
            "contract.checks[0].changed_files_within must be a nonempty list of at most 100 strings",
        ),
        (
            json!({"contract": {"checks": [{"changed_files_within": ["a/**b"]}]}}),
            "contract.checks[0].changed_files_within has an invalid glob: a/**b",
        ),
        (
            json!({"contract": {"checks": [{"no_new_dependencies": false}]}}),
            "contract.checks[0].no_new_dependencies must be true",
        ),
        (
            json!({"contract": {"checks": [{"files_exist": ["../x"]}]}}),
            "contract.checks[0].files_exist paths are relative to the child's working directory, not ../x",
        ),
        (
            json!({"contract": {"checks": [{"files_exist": ["/etc/passwd"]}]}}),
            "contract.checks[0].files_exist paths are relative to the child's working directory, not /etc/passwd",
        ),
        (
            json!({"contract": {"checks": [{"what": 1}]}}),
            "contract.checks[0] must be one of {\"run\": cmd, \"expect\": ...}, {\"changed_files_within\": [globs]}, {\"no_new_dependencies\": true}, {\"files_exist\": [paths]}",
        ),
        (
            json!({"contract": {"returns": "object"}}),
            "contract.returns must be a JSON schema object",
        ),
        (
            json!({"contract": {"checks": [{"run": "t"}]}, "hop": {"retry": 4}}),
            "hop must be \"report\", {\"retry\": N} (N from 1 to 3) or {\"review\": \"brief\"}",
        ),
        (
            json!({"contract": {"checks": [{"run": "t"}]}, "hop": {"retry": 0}}),
            "hop must be \"report\", {\"retry\": N} (N from 1 to 3) or {\"review\": \"brief\"}",
        ),
        (
            json!({"contract": {"checks": [{"run": "t"}]}, "hop": "retry"}),
            "hop must be \"report\", {\"retry\": N} (N from 1 to 3) or {\"review\": \"brief\"}",
        ),
        (
            json!({"hop": {"retry": 1}}),
            "hop needs a contract to fail: give contract={\"checks\": [...]}",
        ),
        (
            json!({"role": ""}),
            "role must be a nonempty string of at most 4000 characters",
        ),
    ];
    for (value, error) in cases {
        assert_eq!(parse(value.clone()).unwrap_err(), error, "{value}");
    }
    let many: Vec<Value> = (0..17).map(|_| json!({"run": "t"})).collect();
    assert_eq!(
        parse(json!({"contract": {"checks": many}})).unwrap_err(),
        "contract.checks must be a list of at most 16 checks"
    );
}

#[test]
fn delegation_spawn_contract_dependency_files_are_manifests_and_lockfiles_at_any_depth() {
    for path in [
        "Cargo.toml",
        "crates/x/Cargo.toml",
        "Cargo.lock",
        "web/package.json",
        "yarn.lock",
        "requirements-dev.txt",
        "go.sum",
        "pyproject.toml",
    ] {
        assert!(is_dependency_file(path), "{path}");
    }
    for path in [
        "src/Cargo.rs",
        "package.json.bak",
        "docs/requirements.md",
        "notes.txt",
        "my-Cargo.toml",
    ] {
        assert!(!is_dependency_file(path), "{path}");
    }
}

fn evaluate(contract: Value, verdict: Option<Value>, cwd: &Path, diff: Option<&WorkspaceDiff>) -> Value {
    let contract = parse(contract).unwrap().unwrap();
    let signal = Signal::new();
    let place = CheckPlace {
        cwd: Some(cwd),
        env: &[("CHECK_VAR".into(), "from-env".into())],
        diff,
        signal: &signal,
    };
    contract.evaluate(verdict.as_ref(), &place, &plain_check_runner())
}

#[test]
fn delegation_spawn_contract_run_checks_judge_exit_codes_and_stdout_in_the_working_directory() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("here.txt"), "present\n").unwrap();
    let report = evaluate(
        json!({"contract": {"checks": [
            {"run": "cat here.txt; echo $CHECK_VAR"},
            {"run": "echo out; echo err >&2; exit 4", "expect": {"exit": 4}},
            {"run": "echo nope", "expect": {"stdout_contains": "yes"}},
            {"run": "echo boom >&2; exit 2"},
            {"run": "exec sleep 30", "timeout_ms": 200},
        ]}}),
        None,
        dir.path(),
        None,
    );
    assert_eq!(report["verdict"], "contract_failed");
    let checks = report["checks"].as_array().unwrap();
    assert_eq!(checks[0]["ok"], true);
    assert_eq!(checks[0]["stdout"], "present\nfrom-env\n");
    assert_eq!(checks[0]["evidence"], "exit 0");
    assert_eq!(
        (&checks[1]["ok"], &checks[1]["stderr"]),
        (&json!(true), &json!("err\n"))
    );
    assert_eq!(checks[1]["exit_code"], 4);
    assert_eq!(checks[2]["evidence"], "exit 0; stdout does not contain \"yes\": nope");
    assert_eq!(checks[3]["evidence"], "exit 2: boom");
    assert_eq!(checks[4]["evidence"], "timed out after 200 ms");
    assert_eq!(checks[4]["exit_code"], Value::Null);
    assert_eq!(
        report["failed"],
        json!([
            "run \"echo nope\": exit 0; stdout does not contain \"yes\": nope",
            "run \"echo boom >&2; exit 2\": exit 2: boom",
            "run \"exec sleep 30\": timed out after 200 ms",
        ])
    );
}

#[test]
fn delegation_spawn_contract_long_output_keeps_its_end() {
    let dir = tempfile::tempdir().unwrap();
    let report = evaluate(
        json!({"contract": {"checks": [{"run": "seq 1 3000; echo LAST"}]}}),
        None,
        dir.path(),
        None,
    );
    let stdout = report["checks"][0]["stdout"].as_str().unwrap();
    assert!(stdout.starts_with("[... "), "{stdout}");
    assert!(stdout.ends_with("2999\n3000\nLAST\n"), "{stdout}");
    let kept = stdout.split_once(" characters cut]").unwrap().1;
    assert_eq!(kept.chars().count(), 2000);
}

#[test]
fn delegation_spawn_contract_file_checks_read_the_diff_and_the_tree() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/a.rs"), "").unwrap();
    let diff = WorkspaceDiff {
        changed: vec!["Cargo.lock".into(), "docs/x.md".into(), "src/a.rs".into()],
        complete: true,
        ..WorkspaceDiff::default()
    };
    let report = evaluate(
        json!({"contract": {"checks": [
            {"changed_files_within": ["src/**", "Cargo.lock", "docs/*.md"]},
            {"changed_files_within": ["src/**"]},
            {"no_new_dependencies": true},
            {"files_exist": ["src/a.rs", "src/b.rs"]},
        ]}}),
        None,
        dir.path(),
        Some(&diff),
    );
    assert_eq!(
        report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["ok"].clone())
            .collect::<Vec<_>>(),
        vec![json!(true), json!(false), json!(false), json!(false)]
    );
    assert_eq!(
        report["failed"],
        json!([
            "changed_files_within [\"src/**\"]: changed outside [\"src/**\"]: Cargo.lock, docs/x.md",
            "no_new_dependencies: dependency files changed: Cargo.lock",
            "files_exist [\"src/a.rs\",\"src/b.rs\"]: missing: src/b.rs",
        ])
    );
    // Without a snapshot to compare, a diff check fails rather than passing unseen.
    let report = evaluate(
        json!({"contract": {"checks": [{"no_new_dependencies": true}]}}),
        None,
        dir.path(),
        None,
    );
    assert_eq!(
        report["failed"],
        json!([
            "no_new_dependencies: no working directory was snapshotted when the child started, so its changes are unknown"
        ])
    );
    let empty = WorkspaceDiff::default();
    let report = evaluate(
        json!({"contract": {"checks": [{"changed_files_within": ["src/**"]}, {"no_new_dependencies": true}]}}),
        None,
        dir.path(),
        Some(&empty),
    );
    assert_eq!(report["verdict"], "verified");
}

#[test]
fn delegation_spawn_contract_returns_is_checked_on_the_verdict_outputs() {
    let dir = tempfile::tempdir().unwrap();
    let contract = json!({"contract": {"returns": {"type": "object", "required": ["n"]}}});
    let verified = evaluate(contract.clone(), Some(json!({"outputs": {"n": 1}})), dir.path(), None);
    assert_eq!(verified["verdict"], "verified");
    let missing = evaluate(contract.clone(), Some(json!({"outputs": {}})), dir.path(), None);
    assert_eq!(missing["verdict"], "contract_failed");
    assert!(
        missing["failed"][0].as_str().unwrap().starts_with("returns: outputs"),
        "{missing}"
    );
    let none = evaluate(contract, None, dir.path(), None);
    assert_eq!(
        none["failed"],
        json!(["returns: the child ended without rlm.finish, so its outputs were never given"])
    );
}

#[test]
fn delegation_spawn_contract_a_cancelled_check_stops_its_command() {
    let dir = tempfile::tempdir().unwrap();
    let signal = Signal::new();
    let stopper = signal.clone();
    let check = CommandCheck {
        command: "echo started > started.txt; exec sleep 60".into(),
        cwd: dir.path().to_path_buf(),
        env: Vec::new(),
        timeout: Duration::from_secs(120),
        signal: signal.clone(),
    };
    let started = dir.path().join("started.txt");
    let waiter = std::thread::spawn(move || {
        // Cancel once the command is running (a file it wrote), as a task cancel does.
        while !started.exists() {
            std::thread::yield_now();
        }
        stopper.abort("cancelled");
    });
    let outcome = plain_check_runner()(&check);
    waiter.join().unwrap();
    assert_eq!((outcome.exit, outcome.timed_out), (None, false));
}

/// A host whose children run `script(prompt, lane, host)` and reply "done"; the prompts are kept.
fn host(
    workspace: &Path,
    script: impl Fn(&str, &str, &Delegation) + Send + Sync + 'static,
) -> (Delegation, Arc<Mutex<Vec<String>>>) {
    let prompts: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = prompts.clone();
    let runner: ChildRunner = Arc::new(move |run| {
        seen.lock().unwrap().push(run.prompt.clone());
        script(&run.prompt, &run.lane_name, &run.host);
        Ok("done".into())
    });
    let mut options = DelegationOptions::new(runner);
    options.workspace = Some(workspace.to_path_buf());
    let (host, _) = Delegation::new(options);
    (host, prompts)
}

fn finish(host: &Delegation, lane: &str, outputs: Value) -> Result<Value, String> {
    host.handle(
        "rlm.finish",
        json!({"status": "passed", "summary": "done it", "evidence": ["a.txt:1 written"], "outputs": outputs,
               "changed_files": []}),
        lane,
    )
}

fn spawn_and_collect(host: &Delegation, kwargs: Value) -> Value {
    let spawned = host
        .handle("rlm.spawn", json!({"prompt": "TASK", "kwargs": kwargs}), "main")
        .unwrap();
    let id = spawned["rlm_child_id"].as_str().unwrap().to_string();
    let collected = host.handle("rlm.collect", json!({"selectors": [id]}), "main").unwrap();
    collected["results"][0]["result"].clone()
}

#[test]
fn delegation_spawn_contract_host_verifies_a_child_and_its_prompt_names_the_checks() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (host, prompts) = host(dir.path(), move |_, lane, host| {
        std::fs::write(root.join("a.txt"), "ok\n").unwrap();
        finish(host, lane, json!({})).unwrap();
    });
    let result = spawn_and_collect(
        &host,
        json!({"name": "c", "role": "writer", "contract": {"checks": [{"run": "grep -q ok a.txt"}, {"changed_files_within": ["*.txt"]}]}}),
    );
    assert_eq!(result["status"], "succeeded", "{result}");
    assert_eq!(result["check"]["contract"]["verdict"], "verified", "{result}");
    assert_eq!(
        result["check"]["contract"]["checks"][1]["changed"],
        json!(["a.txt"]),
        "{result}"
    );
    assert_eq!(
        prompts.lock().unwrap().clone(),
        vec!["Role: writer\nContract: when you finish, the host itself runs these checks in your working directory: {\"run\":\"grep -q ok a.txt\",\"expect\":{\"exit\":0},\"timeout_ms\":120000}; {\"changed_files_within\":[\"*.txt\"]}\n\nTASK".to_string()]
    );
}

#[test]
fn delegation_spawn_contract_completion_tag_names_the_contract_verdict() {
    use delegation::verdict::verdict_tag;
    let verified = json!({"status": "succeeded", "verdict": {"status": "passed"},
        "check": {"outcome": "verified", "contract": {"verdict": "verified"}}});
    assert_eq!(
        verdict_tag(&verified).as_deref(),
        Some("[passed, verified; contract verified]")
    );
    let failed = json!({"status": "failed", "verdict": {"status": "passed"},
        "check": {"outcome": "verified", "contract": {"verdict": "contract_failed"}}});
    assert_eq!(
        verdict_tag(&failed).as_deref(),
        Some("[passed, verified; contract contract_failed]")
    );
    let unverified = json!({"status": "failed", "unverified": true, "verdict": null,
        "check": {"outcome": "unchecked", "contract": {"verdict": "contract_failed"}}});
    assert_eq!(
        verdict_tag(&unverified).as_deref(),
        Some("[unverified; contract contract_failed]")
    );
    // A typed agent brief's evidence (`check.contract` without a verdict) adds nothing.
    let brief = json!({"status": "succeeded", "verdict": {"status": "passed"},
        "check": {"outcome": "verified", "contract": {"passed": true}}});
    assert_eq!(verdict_tag(&brief).as_deref(), Some("[passed, verified]"));
}
