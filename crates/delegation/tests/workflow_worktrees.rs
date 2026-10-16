//! `rlm-child@1` workflow nodes with `worktree: true` (native-host.ts `workflowTask` + `newWorktree`), ported from
//! packages/coding-agent/test/ultron-worktree-host.test.ts ("workflow nodes run in worktrees with `worktree: true`
//! and merge by their results") with its literal expectations, plus the validation messages of `workflow()`.
//! The workflow host is [`Agents`]; its worktree nodes are subagents of the workflow's caller in [`Delegation`].

use delegation::agents::{Agents, AgentsOptions, LaneRunner};
use delegation::host::{ChildRunner, Delegation, DelegationOptions};
use delegation::task_store::MemoryStore;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

const APP: &str = "def one():\n    return 1\n\n\n\ndef two():\n    return 2\n\n\n\ndef three():\n    return 3\n";

fn git(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git").args(args).current_dir(cwd).output().unwrap();
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8(out.stdout).unwrap()
}

fn repository(dir: &Path) -> PathBuf {
    let root = std::fs::canonicalize(dir).unwrap().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.email", "test@example.com"]);
    git(&root, &["config", "user.name", "Test"]);
    std::fs::write(root.join(".gitignore"), "node_modules/\n").unwrap();
    std::fs::write(root.join("app.py"), APP).unwrap();
    let quiet = ["-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null"];
    git(&root, &[&quiet[..], &["add", "-A"]].concat());
    git(&root, &[&quiet[..], &["commit", "-q", "-m", "initial"]].concat());
    root
}

fn passed(files: &[&str]) -> Value {
    json!({ "status": "passed", "summary": format!("Changed {}", files.join(", ")),
        "evidence": files.iter().map(|f| format!("{f}:1 changed")).collect::<Vec<_>>(), "changed_files": files })
}

/// Each child run's brief and working directory.
type Prompts = Arc<Mutex<Vec<(String, Option<String>)>>>;

struct Setup {
    agents: Agents,
    delegation: Delegation,
    prompts: Prompts,
}

/// Children named by their brief's first line edit `app.py` in their working directory and finish passed.
fn setup(root: &Path, with_children: bool) -> Setup {
    let prompts: Prompts = Arc::default();
    let (seen, workspace) = (prompts.clone(), root.to_path_buf());
    let host: Arc<std::sync::OnceLock<Agents>> = Arc::default();
    let workflow_host = host.clone();
    let runner: ChildRunner = Arc::new(move |run| {
        seen.lock().unwrap().push((run.prompt.clone(), run.cwd.clone()));
        let key = run.prompt.lines().next().unwrap_or_default().to_string();
        if key == "nested" {
            // A subagent that runs a workflow from its own lane: its worktree node is its own child.
            let agents = workflow_host.get().unwrap();
            let outcome = agents.handle(
                "workflows.run",
                json!({ "nodes": [node("left", json!(true))] }),
                &run.lane_name,
                None,
            )?;
            return Ok(outcome.to_string());
        }
        if key == "failing" {
            let cwd = run.cwd.clone().map_or(workspace.clone(), PathBuf::from);
            std::fs::write(cwd.join("app.py"), "broken\n").unwrap();
            let verdict = json!({ "status": "failed", "summary": "could not finish" });
            run.host.handle("rlm.finish", verdict, &run.lane_name)?;
            return Ok(key);
        }
        let (from, to) = match key.as_str() {
            "left" => ("return 1", "return 'left'"),
            "right" => ("return 3", "return 'right'"),
            other => return Err(format!("no script for {other}")),
        };
        let cwd = run.cwd.clone().map_or(workspace.clone(), PathBuf::from);
        let text = std::fs::read_to_string(cwd.join("app.py")).unwrap();
        std::fs::write(cwd.join("app.py"), text.replace(from, to)).unwrap();
        run.host.handle("rlm.finish", passed(&["app.py"]), &run.lane_name)?;
        Ok(key)
    });
    let mut options = DelegationOptions::new(runner);
    options.workspace = Some(root.to_path_buf());
    let (delegation, _ends) = Delegation::new(options);
    let mut agents = AgentsOptions::new(Box::new(MemoryStore::default()));
    // Worktree nodes never run as lanes of the workflow host.
    let lanes: LaneRunner = Arc::new(|run| Err(format!("lane run {} is not expected", run.lane)));
    agents.lanes = Some(lanes);
    let (agents, _ends) = Agents::new(agents);
    if with_children {
        agents.set_worktree_children(Arc::new(delegation.clone()));
    }
    let _ = host.set(agents.clone());
    Setup {
        agents,
        delegation,
        prompts,
    }
}

fn node(id: &str, worktree: Value) -> Value {
    json!({ "id": id, "definition": "rlm-child@1", "input": { "prompt": id }, "worktree": worktree })
}

#[test]
fn delegation_workflow_worktree_nodes_run_in_worktrees_and_merge_by_their_results() {
    let dir = tempfile::tempdir().unwrap();
    let root = repository(dir.path());
    let Setup {
        agents,
        delegation,
        prompts,
    } = setup(&root, true);
    let outcome = agents
        .handle(
            "workflows.run",
            json!({ "nodes": [node("left", json!(true)), node("right", json!(true))] }),
            "main",
            None,
        )
        .unwrap();
    assert_eq!(outcome["left"]["status"], "succeeded", "{outcome}");
    assert_eq!(outcome["left"]["worktree"]["branch"], "ultron/session/left");
    assert_eq!(outcome["right"]["worktree"]["branch"], "ultron/session/right");
    assert_eq!(outcome["left"]["worktree"]["changed_files"], json!(["app.py"]));
    // Each child worked in its own worktree and was told so; the caller's tree is untouched until it merges.
    for (prompt, cwd) in prompts.lock().unwrap().iter() {
        assert!(
            prompt.contains("[Worktree] You work in your own Git worktree"),
            "{prompt}"
        );
        assert_ne!(cwd.as_deref(), Some(root.to_str().unwrap()));
        assert!(cwd.is_some());
    }
    assert_eq!(std::fs::read_to_string(root.join("app.py")).unwrap(), APP);
    let ids = [
        &outcome["left"]["worktree"]["task"],
        &outcome["right"]["worktree"]["task"],
    ];
    let report = delegation
        .handle("rlm.merge", json!({ "selectors": ids }), "main")
        .unwrap();
    assert_eq!(report["ok"], true, "{report}");
    let text = std::fs::read_to_string(root.join("app.py")).unwrap();
    assert!(
        text.contains("return 'left'") && text.contains("return 'right'"),
        "{text}"
    );
    let error = agents
        .handle(
            "workflows.run",
            json!({ "nodes": [node("x", json!("sometimes"))] }),
            "main",
            None,
        )
        .unwrap_err();
    assert!(error.contains("must be True, False or \"auto\""), "{error}");
}

#[test]
fn delegation_workflow_worktree_auto_outside_git_runs_in_the_shared_tree() {
    let dir = tempfile::tempdir().unwrap();
    let plain = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::write(plain.join("app.py"), APP).unwrap();
    let Setup { agents, prompts, .. } = setup(&plain, true);
    let outcome = agents
        .handle(
            "workflows.run",
            json!({ "nodes": [node("left", json!("auto"))] }),
            "main",
            None,
        )
        .unwrap();
    assert_eq!(outcome["left"]["status"], "succeeded", "{outcome}");
    assert!(outcome["left"]["worktree"].is_null(), "{outcome}");
    assert_eq!(prompts.lock().unwrap()[0].1, None);
    assert!(
        std::fs::read_to_string(plain.join("app.py"))
            .unwrap()
            .contains("return 'left'")
    );
    // worktree=True outside Git is an explicit node failure, not a silent shared-tree run.
    let outcome = agents
        .handle(
            "workflows.run",
            json!({ "nodes": [node("right", json!(true))] }),
            "main",
            None,
        )
        .unwrap();
    assert_eq!(outcome["right"]["status"], "failed");
    let error = outcome["right"]["error"].as_str().unwrap();
    assert!(error.starts_with("Admission refused: worktree=True: "), "{error}");
}

#[test]
fn delegation_workflow_worktree_validation_matches_typescript() {
    let dir = tempfile::tempdir().unwrap();
    let root = repository(dir.path());
    let Setup { agents, .. } = setup(&root, true);
    let run = |nodes: Value| agents.handle("workflows.run", json!({ "nodes": nodes }), "main", None);
    assert_eq!(
        run(json!([{ "id": "a", "definition": "background-job@1", "input": {"command": "x"}, "worktree": true }])),
        Err("Workflow node a: worktree applies only to rlm-child@1 nodes (subagents)".into())
    );
    assert_eq!(
        run(json!([node("a", json!("sometimes"))])),
        Err("Workflow node a worktree must be True, False or \"auto\" (each worktree child gets its own private worktree)".into())
    );
    assert_eq!(
        run(json!([
            { "id": "a", "definition": "rlm-child@1", "input": {"prompt": "a"}, "worktree": true,
              "revise": { "from": "b", "until": { "equals": true }, "max_rounds": 2 } },
            { "id": "b", "definition": "rlm-child@1", "inputFrom": "a", "dependsOn": ["a"] },
        ])),
        Err(
            "Workflow node a: a revised node cannot use a worktree (each round would start over from your tree)".into()
        )
    );
    // A host without a subagent host refuses worktree nodes before anything runs.
    let Setup { agents, .. } = setup(&root, false);
    assert_eq!(
        agents.handle("workflows.run", json!({ "nodes": [node("a", json!(true))] }), "main", None),
        Err("Workflow node a: worktree nodes are not available in this host; spawn the subagent with rlm.spawn(worktree=True)".into())
    );
}

#[test]
fn delegation_workflow_worktree_node_with_a_failed_verdict_fails() {
    let dir = tempfile::tempdir().unwrap();
    let root = repository(dir.path());
    let Setup { agents, .. } = setup(&root, true);
    let outcome = agents
        .handle(
            "workflows.run",
            json!({ "nodes": [node("failing", json!(true))] }),
            "main",
            None,
        )
        .unwrap();
    // verdictGate: the child succeeded, its verdict did not pass.
    assert_eq!(outcome["failing"]["status"], "failed", "{outcome}");
    assert_eq!(outcome["failing"]["error"], "Subagent verdict failed: could not finish");
    assert_eq!(outcome["failing"]["worktree"]["branch"], "ultron/session/failing");
    assert_eq!(std::fs::read_to_string(root.join("app.py")).unwrap(), APP);
}

#[test]
fn delegation_workflow_worktree_node_is_a_child_of_the_lane_that_ran_the_workflow() {
    let dir = tempfile::tempdir().unwrap();
    let root = repository(dir.path());
    let Setup { delegation, .. } = setup(&root, true);
    let parent = delegation
        .handle(
            "rlm.spawn",
            json!({ "prompt": "nested", "kwargs": { "name": "nested", "depth": 1 } }),
            "main",
        )
        .unwrap();
    let parent_id = parent["rlm_child_id"].as_str().unwrap().to_string();
    let ended = delegation.wait(std::slice::from_ref(&parent_id), None);
    assert_eq!(ended[0].1.as_ref().unwrap()["status"], "succeeded", "{ended:?}");
    let tasks = delegation.tasks().unwrap();
    let node = tasks
        .iter()
        .find(|t| t["id"] != parent_id.as_str())
        .expect("the node's child");
    assert_eq!(node["parentId"], parent_id.as_str(), "{tasks:?}");
}
