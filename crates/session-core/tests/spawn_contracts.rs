//! Host-checked delegation contracts (decision row 210): `rlm.spawn(brief, name=..., role=..., contract={...},
//! hop=...)` from the root REPL of a real session (the session actor, its delegation host, durable child runs and
//! kernels) against a scripted provider on loopback. The host runs the contract's checks itself when the child ends:
//! a child that broke a check fails with `contract_failed`, a retry hop re-runs it once with the failures in its
//! brief, a review hop spawns the reviewer, all-pass is `verified` with evidence; `changed_files_within` and
//! `no_new_dependencies` catch out-of-scope and dependency edits. New in Rust: literal expectations.

mod p_session_support;

use p_session_support::{Setup, actor, text, tool, tool_results, user_texts};
use serde_json::{Value, json};
use std::path::Path;

/// Where a root cell writes what it saw, relative to the session's working directory (the kernel may be sandboxed).
const RESULT: &str = "contract-result.json";

/// The first user text of a request (its brief, for a child).
fn first_user(request: &Value) -> String {
    user_texts(request)
        .iter()
        .filter_map(Value::as_str)
        .find(|text| !text.starts_with("[clock]"))
        .unwrap_or_default()
        .to_string()
}

/// The root cell: spawn with `options` (Python keyword arguments), wait for the child's end (a poll bounded at
/// about 120 s, a hang guard: the host never lets a cell wait), write its result to `out`; with `reviewer`, wait
/// for the reviewer the review hop spawned too and write its result beside it.
fn root_cell(brief: &str, options: &str, out: &Path, reviewer: bool) -> String {
    let wait = "async def wait(h):\n    for _ in range(12000):\n        try:\n            return (await rlm.collect([h]))[0]['result']\n        except RuntimeError:\n            await asyncio.sleep(0.01)\n";
    let mut code = format!(
        "import asyncio, json\n{wait}h = await rlm.spawn({}, name='c', {options})\nr = await wait(h)\nout = {{'child': r}}\n",
        json!(brief)
    );
    if reviewer {
        code.push_str(
            "rid = r['check']['contract']['hop']['review']['rlm_child_id']\nout['reviewer'] = await wait(rid)\n",
        );
    }
    code.push_str(&format!(
        "import os\nout['root_files'] = sorted(os.listdir('.'))\nopen({}, 'w').write(json.dumps(out))\nprint('written')",
        json!(out.display().to_string())
    ));
    code
}

/// A child cell: run `body`, then record a passed verdict naming `changed`.
fn child_cell(body: &str, changed: &[&str]) -> String {
    format!(
        "from pathlib import Path\n{body}\nprint(await rlm.finish('passed', 'did the work', evidence=['work.txt:1 written'], changed_files={}))",
        json!(changed)
    )
}

struct Run {
    out: Value,
    requests: Vec<Value>,
}

/// Run a session whose root spawns with `options`; `child(brief, has_results)` scripts the child's replies (the
/// reviewer's too: its brief has the review marker).
fn run(brief: &str, options: &str, reviewer: bool, child: impl Fn(&str, bool) -> String + Send + 'static) -> Run {
    let out = std::path::PathBuf::from(RESULT);
    run_cell(root_cell(brief, options, &out, reviewer), &out, false, child)
}

/// [`run`] in a session with `settings`.
fn run_with(settings: Value, brief: &str, options: &str, child: impl Fn(&str, bool) -> String + Send + 'static) -> Run {
    let out = std::path::PathBuf::from(RESULT);
    run_in(
        Setup {
            settings: Some(settings),
            ..Setup::default()
        },
        root_cell(brief, options, &out, false),
        &out,
        false,
        child,
    )
}

fn git(cwd: &Path, home: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("HOME", home)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Run a session whose root runs `cell` (which writes `out`), its cwd a Git repository with one commit if `repo`.
fn run_cell(cell: String, out: &Path, repo: bool, child: impl Fn(&str, bool) -> String + Send + 'static) -> Run {
    run_in(Setup::default(), cell, out, repo, child)
}

fn run_in(
    setup: Setup,
    cell: String,
    out: &Path,
    repo: bool,
    child: impl Fn(&str, bool) -> String + Send + 'static,
) -> Run {
    let script = Box::new(move |request: &Value, _| {
        let first = first_user(request);
        let has_results = !tool_results(request).is_empty();
        if first.starts_with("ROOT") {
            return match has_results {
                false => tool("toolu_root", &cell),
                true => text("root done"),
            };
        }
        child(&first, has_results)
    });
    let a = actor(script, setup);
    if repo {
        git(&a.cwd, &a.root, &["init", "-q"]);
        std::fs::write(a.cwd.join("README.md"), "repo\n").unwrap();
        git(&a.cwd, &a.root, &["add", "README.md"]);
        git(
            &a.cwd,
            &a.root,
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "init",
            ],
        );
    }
    let ended = a.prompt("ROOT: delegate with a contract");
    assert_eq!(ended["error"], Value::Null, "{ended}");
    let written = std::fs::read_to_string(a.cwd.join(out)).unwrap_or_else(|_| {
        let results: Vec<Value> = a.requests().iter().flat_map(tool_results).collect();
        panic!("the root cell wrote no result: {results:?}")
    });
    let out: Value = serde_json::from_str(&written).unwrap();
    if out["child"].is_null() {
        let seen: Vec<(String, Vec<Value>)> = a.requests().iter().map(|r| (first_user(r), tool_results(r))).collect();
        panic!("the child never ended: {seen:#?}");
    }
    Run {
        out,
        requests: a.requests(),
    }
}

fn checks_of(result: &Value) -> Vec<(Value, Value)> {
    result["check"]["contract"]["checks"]
        .as_array()
        .unwrap_or_else(|| panic!("no contract checks in {result}"))
        .iter()
        .map(|c| (c["ok"].clone(), c["evidence"].clone()))
        .collect()
}

#[test]
fn session_core_spawn_contract_all_checks_pass_is_verified_with_evidence() {
    let options = r#"role='fixer', contract={'checks': [{'run': 'cat src/a.txt; echo; echo FOUND', 'expect': {'stdout_contains': 'FOUND'}}, {'run': 'test -f src/a.txt', 'expect': 'exit 0'}, {'changed_files_within': ['src/**']}, {'no_new_dependencies': True}, {'files_exist': ['src/a.txt']}], 'returns': {'type': 'object'}}"#;
    let r = run(
        "CHILD: write src/a.txt",
        options,
        false,
        |_, has_results| match has_results {
            false => tool(
                "toolu_c",
                &child_cell(
                    "Path('src').mkdir(exist_ok=True); Path('src/a.txt').write_text('alpha')",
                    &["src/a.txt"],
                ),
            ),
            true => text("child done"),
        },
    );
    let child = &r.out["child"];
    assert_eq!(child["status"], "succeeded", "{child}");
    assert_eq!(child["check"]["contract"]["verdict"], "verified", "{child}");
    assert_eq!(child["check"]["contract"]["failed"], json!([]), "{child}");
    assert_eq!(child["check"]["contract"]["attempts"], 1, "{child}");
    assert_eq!(child["check"]["contract"]["hop"], "report", "{child}");
    assert_eq!(
        checks_of(child),
        vec![
            (json!(true), json!("rlm.finish outputs match contract.returns")),
            (json!(true), json!("exit 0; stdout contains \"FOUND\"")),
            (json!(true), json!("exit 0")),
            (json!(true), json!("1 changed file(s), all within [\"src/**\"]")),
            (json!(true), json!("no dependency manifest or lockfile changed")),
            (json!(true), json!("1 file(s) exist")),
        ],
        "{child}"
    );
    // The command's output is the evidence: it ran in the child's working directory, on the child's file.
    assert_eq!(
        child["check"]["contract"]["checks"][1]["stdout"], "alpha\nFOUND\n",
        "{child}"
    );
    assert_eq!(child["check"]["contract"]["checks"][1]["exit_code"], 0, "{child}");
    // The child's brief led with its role and the checks the host runs.
    let brief = r
        .requests
        .iter()
        .map(first_user)
        .find(|f| f.contains("CHILD: write"))
        .unwrap();
    assert!(brief.starts_with("Role: fixer\nContract: when you finish, the host itself runs these checks in your working directory: {\"run\":\"cat src/a.txt; echo; echo FOUND\""), "{brief}");
    assert!(brief.ends_with("\n\nCHILD: write src/a.txt"), "{brief}");
}

#[test]
fn session_core_spawn_contract_a_broken_check_is_contract_failed() {
    let options =
        r#"contract={'checks': [{'run': 'grep -q fixed status.txt'}, {'files_exist': ['status.txt', 'missing.md']}]}"#;
    let r = run(
        "CHILD: fix status",
        options,
        false,
        |_, has_results| match has_results {
            false => tool(
                "toolu_c",
                &child_cell("Path('status.txt').write_text('wrong\\n')", &["status.txt"]),
            ),
            true => text("child done"),
        },
    );
    let child = &r.out["child"];
    assert_eq!(child["status"], "failed", "{child}");
    assert_eq!(child["check"]["contract"]["verdict"], "contract_failed", "{child}");
    assert_eq!(
        child["check"]["contract"]["failed"],
        json!([
            "run \"grep -q fixed status.txt\": exit 1",
            "files_exist [\"status.txt\",\"missing.md\"]: missing: missing.md"
        ]),
        "{child}"
    );
    assert_eq!(
        child["error"],
        "Contract failed: run \"grep -q fixed status.txt\": exit 1; files_exist [\"status.txt\",\"missing.md\"]: missing: missing.md",
        "{child}"
    );
    // The child's own claim stays visible beside the host's verdict.
    assert_eq!(child["verdict"]["status"], "passed", "{child}");
}

#[test]
fn session_core_spawn_contract_retry_hop_reruns_once_with_the_failure_in_the_brief() {
    let options = r#"contract={'checks': [{'run': 'grep fixed status.txt', 'expect': {'stdout_contains': 'fixed'}}]}, hop={'retry': 1}"#;
    let r = run("CHILD: fix status", options, false, |brief, has_results| {
        let body = match brief.contains("[Contract] Attempt 1") {
            true => "Path('status.txt').write_text('fixed\\n')",
            false => "Path('status.txt').write_text('wrong\\n')",
        };
        match has_results {
            false => tool("toolu_c", &child_cell(body, &["status.txt"])),
            true => text("child done"),
        }
    });
    let child = &r.out["child"];
    assert_eq!(child["status"], "succeeded", "{child}");
    assert_eq!(child["check"]["contract"]["verdict"], "verified", "{child}");
    assert_eq!(child["check"]["contract"]["attempts"], 2, "{child}");
    assert_eq!(
        child["check"]["contract"]["previous"],
        json!([["run \"grep fixed status.txt\": exit 1; stdout does not contain \"fixed\""]]),
        "{child}"
    );
    assert_eq!(child["check"]["contract"]["hop"], json!({"retry": 1}), "{child}");
    // Exactly two child runs: the second one's brief is the first plus the failed checks.
    let briefs: Vec<String> = r
        .requests
        .iter()
        .filter(|req| tool_results(req).is_empty())
        .map(first_user)
        .filter(|f| f.contains("CHILD: fix status"))
        .collect();
    assert_eq!(briefs.len(), 2, "{briefs:?}");
    assert!(!briefs[0].contains("[Contract]"), "{}", briefs[0]);
    assert!(
        briefs[1].starts_with(&briefs[0]),
        "the retry keeps the brief: {}",
        briefs[1]
    );
    assert!(
        briefs[1].ends_with("\n\n[Contract] Attempt 1 did not pass the host's contract checks:\n- run \"grep fixed status.txt\": exit 1; stdout does not contain \"fixed\"\nFix these in your working directory, then call rlm.finish again; the host re-runs every check when you end."),
        "{}",
        briefs[1]
    );
}

#[test]
fn session_core_spawn_contract_retry_hop_runs_only_as_often_as_asked() {
    let options = r#"contract={'checks': [{'run': 'exit 3', 'expect': {'exit': 0}}]}, hop={'retry': 1}"#;
    let r = run(
        "CHILD: never passes",
        options,
        false,
        |_, has_results| match has_results {
            false => tool(
                "toolu_c",
                &child_cell("Path('work.txt').write_text('x')", &["work.txt"]),
            ),
            true => text("child done"),
        },
    );
    let child = &r.out["child"];
    assert_eq!(child["check"]["contract"]["verdict"], "contract_failed", "{child}");
    assert_eq!(child["check"]["contract"]["attempts"], 2, "{child}");
    assert_eq!(
        child["check"]["contract"]["failed"],
        json!(["run \"exit 3\": exit 3"]),
        "{child}"
    );
    let runs = r
        .requests
        .iter()
        .filter(|req| tool_results(req).is_empty() && first_user(req).contains("CHILD: never passes"))
        .count();
    assert_eq!(runs, 2, "one run and one retry");
}

#[test]
fn session_core_spawn_contract_review_hop_spawns_the_reviewer() {
    let options = r#"contract={'checks': [{'run': 'grep -q fixed status.txt'}]}, hop={'review': 'REVIEW-BRIEF: check the status fix'}"#;
    let r = run("CHILD: fix status", options, true, |brief, has_results| {
        if brief.starts_with("REVIEW-BRIEF") {
            return text("review: status.txt still says wrong");
        }
        match has_results {
            false => tool(
                "toolu_c",
                &child_cell("Path('status.txt').write_text('wrong\\n')", &["status.txt"]),
            ),
            true => text("child done"),
        }
    });
    let child = &r.out["child"];
    assert_eq!(child["check"]["contract"]["verdict"], "contract_failed", "{child}");
    let reviewer_id = child["check"]["contract"]["hop"]["review"]["rlm_child_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{child}"));
    assert_eq!(
        child["check"]["contract"]["hop"]["review"]["name"], "c-review",
        "{child}"
    );
    assert_eq!(
        child["error"],
        format!("Contract failed: run \"grep -q fixed status.txt\": exit 1 (reviewer {reviewer_id} spawned)")
    );
    let reviewer = &r.out["reviewer"];
    assert_eq!(reviewer["status"], "succeeded", "{reviewer}");
    assert_eq!(reviewer["value"], "review: status.txt still says wrong", "{reviewer}");
    let brief = r
        .requests
        .iter()
        .map(first_user)
        .find(|f| f.starts_with("REVIEW-BRIEF"))
        .expect("the reviewer ran");
    let child_id = brief
        .split(" (")
        .nth(1)
        .and_then(|rest| rest.split(')').next())
        .unwrap_or_default()
        .to_string();
    assert_eq!(
        brief,
        format!(
            "REVIEW-BRIEF: check the status fix\n\n[Contract review] Subagent \"c\" ({child_id}) ended; its contract failed the host's checks:\n- run \"grep -q fixed status.txt\": exit 1\nIts summary: did the work\nFiles it declared changed: status.txt"
        )
    );
    assert_ne!(child_id, reviewer_id);
}

#[test]
fn session_core_spawn_contract_changed_files_within_catches_an_out_of_scope_edit() {
    let options = r#"contract={'checks': [{'changed_files_within': ['src/**']}]}"#;
    let r = run("CHILD: edit src", options, false, |_, has_results| match has_results {
        false => tool(
            "toolu_c",
            &child_cell(
                "Path('src').mkdir(exist_ok=True); Path('src/ok.rs').write_text('fn ok() {}')\nPath('other').mkdir(exist_ok=True); Path('other/out.txt').write_text('oops')",
                &["src/ok.rs", "other/out.txt"],
            ),
        ),
        true => text("child done"),
    });
    let child = &r.out["child"];
    assert_eq!(child["check"]["contract"]["verdict"], "contract_failed", "{child}");
    assert_eq!(
        child["check"]["contract"]["failed"],
        json!(["changed_files_within [\"src/**\"]: changed outside [\"src/**\"]: other/out.txt"]),
        "{child}"
    );
    assert_eq!(
        child["check"]["contract"]["checks"][0]["outside"],
        json!(["other/out.txt"]),
        "{child}"
    );
}

#[test]
fn session_core_spawn_contract_no_new_dependencies_catches_manifest_changes() {
    let options = r#"contract={'checks': [{'no_new_dependencies': True}]}"#;
    let r = run("CHILD: add a dep", options, false, |_, has_results| match has_results {
        false => tool(
            "toolu_c",
            &child_cell(
                "Path('Cargo.toml').write_text('[dependencies]\\nserde = \"1\"\\n')\nPath('web').mkdir(exist_ok=True); Path('web/package.json').write_text('{\"dependencies\": {\"left-pad\": \"1\"}}')\nPath('notes.md').write_text('fine')",
                &["Cargo.toml", "web/package.json", "notes.md"],
            ),
        ),
        true => text("child done"),
    });
    let child = &r.out["child"];
    assert_eq!(child["check"]["contract"]["verdict"], "contract_failed", "{child}");
    assert_eq!(
        child["check"]["contract"]["failed"],
        json!(["no_new_dependencies: dependency files changed: Cargo.toml, web/package.json"]),
        "{child}"
    );
}

#[test]
fn session_core_spawn_contract_returns_shape_rejects_a_finish_that_misses_it() {
    // The child's first finish misses `outputs.count`: rejected with the schema problem, it fixes it, and the
    // contract (only `returns`) is verified.
    let options = r#"contract={'returns': {'type': 'object', 'required': ['count'], 'properties': {'count': {'type': 'integer'}}}}"#;
    let cell = "try:\n    await rlm.finish('passed', 'counted', evidence=['data.txt:1 counted'], outputs={})\nexcept Exception as e:\n    print('REJECTED', str(e).splitlines()[1])\nprint(await rlm.finish('passed', 'counted', evidence=['data.txt:1 counted'], outputs={'count': 3}))";
    let r = run(
        "CHILD: count",
        options,
        false,
        move |_, has_results| match has_results {
            false => tool("toolu_c", cell),
            true => text("child done"),
        },
    );
    let child = &r.out["child"];
    assert_eq!(child["check"]["contract"]["verdict"], "verified", "{child}");
    assert_eq!(child["verdict"]["outputs"], json!({"count": 3}), "{child}");
    let results: String = r
        .requests
        .iter()
        .flat_map(tool_results)
        .map(|v| v.to_string())
        .collect();
    assert!(
        results.contains("REJECTED - outputs: must have required properties count (contract.returns)"),
        "{results}"
    );
}

#[test]
fn session_core_spawn_contract_is_refused_when_malformed() {
    // A bad contract is refused at the spawn, before any child runs.
    let out_dir = tempfile::tempdir().unwrap();
    let out = out_dir.path().join("error.txt");
    let cell = format!(
        "try:\n    await rlm.spawn('CHILD: x', name='c', contract={{'checks': [{{'run': 'true', 'expect': 'zero'}}]}})\nexcept Exception as e:\n    open({}, 'w').write(str(e))",
        json!(out.display().to_string())
    );
    let script = Box::new(move |request: &Value, _| match tool_results(request).is_empty() {
        true => tool("toolu_root", &cell),
        false => text("root done"),
    });
    let a = actor(script, Setup::default());
    a.prompt("ROOT: bad contract");
    assert_eq!(
        std::fs::read_to_string(&out).unwrap(),
        "contract.checks[0].expect must be \"exit 0\" (or \"exit N\"), {\"exit\": N} or {\"stdout_contains\": \"text\"}"
    );
    assert!(!a.requests().iter().any(|r| first_user(r).contains("CHILD: x")));
}

#[test]
fn session_core_spawn_contract_checks_of_a_worktree_child_run_in_its_worktree() {
    let options = r#"worktree=True, contract={'checks': [{'run': 'echo "$ULTRON_WORKTREE"; pwd -P'}, {'files_exist': ['made.txt']}, {'changed_files_within': ['made.txt']}]}"#;
    let r = run_git("CHILD: make a file", options, |_, has_results| match has_results {
        false => tool(
            "toolu_c",
            &child_cell("Path('made.txt').write_text('made')", &["made.txt"]),
        ),
        true => text("child done"),
    });
    let child = &r.out["child"];
    assert_eq!(child["check"]["contract"]["verdict"], "verified", "{child}");
    let stdout = child["check"]["contract"]["checks"][0]["stdout"].as_str().unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    let worktree = child["worktree"]["path"].as_str().unwrap_or_else(|| panic!("{child}"));
    // The command ran in the child's worktree with its environment, not in the session's checkout.
    assert_eq!(lines, vec![worktree, worktree], "{child}");
    assert_eq!(child["worktree"]["changed_files"], json!(["made.txt"]), "{child}");
    assert!(
        !r.out["root_files"].as_array().unwrap().contains(&json!("made.txt")),
        "{}",
        r.out
    );
}

fn run_git(brief: &str, options: &str, child: impl Fn(&str, bool) -> String + Send + 'static) -> Run {
    let out = std::path::PathBuf::from(RESULT);
    run_cell(root_cell(brief, options, &out, false), &out, true, child)
}

#[test]
fn session_core_spawn_contract_workflow_nodes_take_role_contract_and_hop() {
    let out = std::path::PathBuf::from(RESULT);
    let node = json!({"id": "fix", "definition": "rlm-child@1", "input": {"prompt": "CHILD: workflow fix"},
        "role": "fixer", "contract": {"checks": [{"run": "grep -q fixed status.txt"}]}, "hop": {"retry": 1}});
    let cell = format!(
        "import json\nr = await workflows.run([{node}])\nout = {{'child': r['fix']}}\nopen({}, 'w').write(json.dumps(out))\nprint('written')",
        json!(out.display().to_string())
    )
    .replace(": true", ": True");
    let r = run_cell(cell, &out, false, |brief, has_results| {
        let body = match brief.contains("[Contract] Attempt 1") {
            true => "Path('status.txt').write_text('fixed\\n')",
            false => "Path('status.txt').write_text('wrong\\n')",
        };
        match has_results {
            false => tool("toolu_c", &child_cell(body, &["status.txt"])),
            true => text("child done"),
        }
    });
    let child = &r.out["child"];
    assert_eq!(child["status"], "succeeded", "{child}");
    assert_eq!(child["check"]["contract"]["verdict"], "verified", "{child}");
    assert_eq!(child["check"]["contract"]["attempts"], 2, "{child}");
    let briefs: Vec<String> = r
        .requests
        .iter()
        .filter(|req| tool_results(req).is_empty())
        .map(first_user)
        .filter(|f| f.contains("CHILD: workflow fix"))
        .collect();
    assert_eq!(briefs.len(), 2, "{briefs:?}");
    assert!(
        briefs[0].starts_with("Role: fixer\nContract: when you finish"),
        "{}",
        briefs[0]
    );
}

#[test]
fn session_core_spawn_contract_help_documents_the_options_and_keep_refuses_them() {
    let out = std::path::PathBuf::from(RESULT);
    let cell = format!(
        "import json\ndoc = rlm.spawn.__doc__\ntry:\n    await rlm.spawn('CHILD: kept', name='k', keep=True, contract={{'checks': [{{'run': 'true'}}]}})\n    kept = 'spawned'\nexcept ValueError as e:\n    kept = str(e)\nopen({}, 'w').write(json.dumps({{'child': {{'doc': doc, 'keep': kept}}}}))\nprint('written')",
        json!(out.display().to_string())
    );
    let r = run_cell(cell, &out, false, |_, _| text("unexpected child"));
    let doc = r.out["child"]["doc"].as_str().unwrap();
    for needle in [
        "Host-checked contracts (Rust host)",
        "{\"run\": \"cargo test -q\", \"expect\": \"exit 0\"",
        "{\"changed_files_within\": [\"src/**\", \"tests/*.rs\"]}",
        "{\"no_new_dependencies\": True}",
        "{\"files_exist\": [\"docs/x.md\"]}",
        "{\"retry\": 1}",
        "{\"review\": \"brief\"}",
        "check[\"contract\"]",
        "rlm-child@1 workflow nodes take the same role, contract and hop fields.",
    ] {
        assert!(doc.contains(needle), "{needle}: {doc}");
    }
    assert_eq!(
        r.out["child"]["keep"],
        "rlm.spawn role, contract and hop apply to children without keep=True"
    );
}

#[test]
fn session_core_spawn_contract_run_checks_run_under_the_session_sandbox() {
    // With `sandbox.mode: required` a check cannot write outside the writable paths (the home is read-only), or,
    // where no sandbox works here, it is refused: either way the check fails, where an unsandboxed one passes.
    let options = r#"contract={'checks': [{'run': 'touch ../outside-the-project.txt'}]}"#;
    let child = |_: &str, has_results: bool| match has_results {
        false => tool(
            "toolu_c",
            &child_cell("Path('work.txt').write_text('x')", &["work.txt"]),
        ),
        true => text("child done"),
    };
    let off = run_with(json!({"sandbox": {"mode": "off"}}), "CHILD: sandboxed", options, child);
    assert_eq!(
        off.out["child"]["check"]["contract"]["verdict"], "verified",
        "{}",
        off.out
    );
    let required = run_with(
        json!({"sandbox": {"mode": "required"}}),
        "CHILD: sandboxed",
        options,
        child,
    );
    let child = &required.out["child"];
    assert_eq!(child["check"]["contract"]["verdict"], "contract_failed", "{child}");
    let evidence = child["check"]["contract"]["checks"][0]["evidence"].as_str().unwrap();
    assert!(
        evidence.contains("Read-only file system") || evidence.contains("sandbox.mode is required"),
        "{evidence}"
    );
}

#[test]
fn session_core_spawn_contract_a_retried_child_needs_a_verdict_of_its_own() {
    // The retry fixes the file but ends without rlm.finish: the first attempt's verdict does not carry over.
    let options = r#"contract={'checks': [{'run': 'grep -q fixed status.txt'}]}, hop={'retry': 1}"#;
    let r = run("CHILD: fix status", options, false, |brief, has_results| {
        match (brief.contains("[Contract] Attempt 1"), has_results) {
            (true, false) => tool(
                "toolu_c2",
                "from pathlib import Path\nPath('status.txt').write_text('fixed\\n')\nprint('no verdict')",
            ),
            (false, false) => tool(
                "toolu_c",
                &child_cell("Path('status.txt').write_text('wrong\\n')", &["status.txt"]),
            ),
            (_, true) => text("child done"),
        }
    });
    let child = &r.out["child"];
    assert_eq!(child["check"]["contract"]["verdict"], "verified", "{child}");
    assert_eq!(child["check"]["contract"]["attempts"], 2, "{child}");
    assert_eq!(
        (&child["verdict"], &child["unverified"]),
        (&Value::Null, &json!(true)),
        "{child}"
    );
}
