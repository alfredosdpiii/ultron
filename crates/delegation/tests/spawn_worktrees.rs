//! Replays fixtures/spawn-worktrees.json (recorded by fixtures/generate-spawn-worktrees.mjs from NativeRlmHost with a
//! Git workspace and scripted `externalChild` runs): worktree spawns, children's briefs/cwd/env, verdict checks
//! against workspace snapshots, worktree commits on finish, `rlm.merge` (clean, conflict stop/skip/markers, skipped
//! failures, already merged, errors), `rlm.worktrees.list/cleanup`, the files and branches left behind, completion
//! events and the journal. Temp roots, worktree ids, task ids, keys and Git ids are normalized as by the generator.

use delegation::host::{ChildRunner, CollectMode, Delegation, DelegationOptions, DetachedEnd};
use delegation::task_store::TaskStore;
use regex::Regex;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct Store(Arc<Mutex<Option<Value>>>);

impl TaskStore for Store {
    fn read(&mut self) -> Result<Option<Value>, String> {
        Ok(self.0.lock().unwrap().clone())
    }
    fn write(&mut self, document: &Value) -> Result<(), String> {
        *self.0.lock().unwrap() = Some(document.clone());
        Ok(())
    }
}

fn git(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git").args(args).current_dir(cwd).output().unwrap();
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8(out.stdout).unwrap()
}

fn settle(host: &Delegation) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while host
        .tasks()
        .unwrap()
        .iter()
        .any(|t| t["state"] == "running" || t["state"] == "queued")
    {
        assert!(Instant::now() < deadline, "children did not settle");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Removes idempotency keys after checking each task has one key everywhere (TS numbers them by event order).
fn strip_keys(value: &mut Value, seen: &mut Vec<(String, String)>) {
    match value {
        Value::Object(map) => {
            if let (Some(Value::String(id)), Some(Value::String(key))) = (map.get("id"), map.get("key")) {
                match seen.iter().find(|(i, _)| i == id) {
                    Some((_, previous)) => assert_eq!(previous, key, "task {id} has one key"),
                    None => seen.push((id.clone(), key.clone())),
                }
                map.remove("key");
            }
            map.values_mut().for_each(|v| strip_keys(v, seen));
        }
        Value::Array(items) => items.iter_mut().for_each(|v| strip_keys(v, seen)),
        _ => {}
    }
}

fn number(text: &str, pattern: &Regex, label: impl Fn(usize) -> String) -> String {
    let mut seen: Vec<String> = Vec::new();
    pattern
        .replace_all(text, |c: &regex::Captures<'_>| {
            let found = c[0].to_string();
            let n = seen.iter().position(|s| *s == found).unwrap_or_else(|| {
                seen.push(found);
                seen.len() - 1
            });
            label(n)
        })
        .into_owned()
}

#[test]
fn delegation_spawn_worktrees_replay_typescript_scenarios() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/spawn-worktrees.json");
    let fixture: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let mut documents = Vec::new();
    let mut replacements: Vec<(String, Vec<String>)> = Vec::new();
    let mut dirs = Vec::new();
    for scenario in fixture.as_array().unwrap() {
        let name = scenario["name"].as_str().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let kind = scenario["workspace"].as_str().unwrap();
        let workspace: Option<PathBuf> = (kind != "none").then(|| root.join("repo"));
        if let Some(workspace) = &workspace {
            std::fs::create_dir_all(workspace).unwrap();
        }
        if kind == "git" {
            let w = workspace.as_ref().unwrap();
            git(w, &["init", "-q", "-b", "main"]);
            git(w, &["config", "user.name", "Fixture"]);
            git(w, &["config", "user.email", "fixture@example.com"]);
            for (path, text) in scenario["files"].as_object().unwrap() {
                std::fs::write(w.join(path), text.as_str().unwrap()).unwrap();
            }
            let quiet = ["-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null"];
            git(w, &[&quiet[..], &["add", "-A"]].concat());
            let message = format!("initial: {name}");
            git(w, &[&quiet[..], &["commit", "-q", "-m", &message]].concat());
        }
        let scripts = scenario["children"].as_array().unwrap().clone();
        let runs: Arc<Mutex<Vec<Value>>> = Arc::default();
        let spawned = Arc::new(Mutex::new(0usize));
        let runner: ChildRunner = {
            let (runs, spawned, workspace) = (runs.clone(), spawned.clone(), workspace.clone());
            Arc::new(move |run| {
                let index = {
                    let mut n = spawned.lock().unwrap();
                    *n += 1;
                    *n - 1
                };
                let script = &scripts[index];
                let env: Value = if run.env.is_empty() {
                    Value::Null
                } else {
                    Value::Object(run.env.iter().map(|(k, v)| (k.clone(), json!(v))).collect())
                };
                let cwd = run.cwd.clone().map(PathBuf::from).or(workspace.clone()).unwrap();
                let mut calls = Vec::new();
                for action in script["actions"].as_array().unwrap() {
                    if action[0] == "write" {
                        let file = cwd.join(action[1].as_str().unwrap());
                        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
                        std::fs::write(file, action[2].as_str().unwrap()).unwrap();
                        continue;
                    }
                    let repo = workspace.as_ref().map(|w| w.display().to_string()).unwrap_or_default();
                    let payload: Value = serde_json::from_str(&action[2].to_string().replace("<repo>", &repo)).unwrap();
                    let kind = action[1].as_str().unwrap();
                    calls.push(match run.host.handle(kind, payload, &run.lane_name) {
                        Ok(result) => json!({ "type": kind, "result": result }),
                        Err(error) => json!({ "type": kind, "error": error }),
                    });
                }
                runs.lock()
                    .unwrap()
                    .push(json!({ "index": index, "taskId": run.task_id, "prompt": run.prompt,
                    "cwd": run.cwd, "env": env, "calls": calls }));
                Ok(script["text"].as_str().unwrap().to_string())
            })
        };
        let journal = Arc::new(Mutex::new(None));
        let mut options = DelegationOptions::new(runner);
        options.store = Box::new(Store(journal.clone()));
        options.collect = CollectMode::Wait;
        options.now = Box::new(|| 1000.0);
        options.workspace = workspace.clone();
        options.session_id = "fixture-session".into();
        let (host, events) = Delegation::new(options);
        let mut ids: Vec<String> = Vec::new();
        let mut worktree_ids: Vec<String> = Vec::new();
        let mut steps = Vec::new();
        for step in scenario["steps"].as_array().unwrap() {
            let op = step["op"].as_array().unwrap();
            match op[0].as_str().unwrap() {
                "call" => {
                    let mut text = op[2].to_string();
                    for (n, id) in ids.iter().enumerate().rev() {
                        text = text.replace(&format!("\"${n}\""), &format!("\"{id}\""));
                    }
                    let kind = op[1].as_str().unwrap();
                    steps.push(match host.handle(kind, serde_json::from_str(&text).unwrap(), "main") {
                        Ok(result) => {
                            if kind == "rlm.spawn" {
                                ids.push(result["rlm_child_id"].as_str().unwrap().to_string());
                                if let Some(path) = result["worktree"]["path"].as_str() {
                                    worktree_ids.push(path.rsplit('/').next().unwrap().to_string());
                                }
                            }
                            json!({ "op": step["op"], "result": result })
                        }
                        Err(error) => json!({ "op": step["op"], "error": error }),
                    });
                }
                "files" => {
                    let w = workspace.as_ref().unwrap();
                    let contents: Vec<Value> = op[1]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|p| std::fs::read_to_string(w.join(p.as_str().unwrap())).map_or(Value::Null, Value::from))
                        .collect();
                    steps.push(json!({ "op": step["op"], "result": contents }));
                }
                "branches" => {
                    let out = git(
                        workspace.as_ref().unwrap(),
                        &["for-each-ref", "--format=%(refname:short)", "refs/heads"],
                    );
                    let branches: Vec<&str> = out.lines().collect();
                    steps.push(json!({ "op": step["op"], "result": branches }));
                }
                "status" => {
                    let out = git(
                        workspace.as_ref().unwrap(),
                        &["status", "--porcelain=v1", "--untracked-files=all"],
                    );
                    steps.push(json!({ "op": step["op"], "result": out }));
                }
                _ => {
                    settle(&host);
                    steps.push(json!({ "op": step["op"] }));
                }
            }
        }
        settle(&host);
        host.close();
        let mut runs = runs.lock().unwrap().clone();
        runs.sort_by_key(|r| r["index"].as_u64());
        let events: Vec<Value> = events
            .try_iter()
            .map(|e: DetachedEnd| {
                json!({ "task": e.task, "kind": e.kind, "ownerLane": e.owner_lane, "awaited": e.awaited, "fetch": e.fetch })
            })
            .collect();
        documents.push(json!({ "name": name, "workspace": kind, "files": scenario["files"], "children": scenario["children"], "steps": steps,
            "runs": runs, "events": events, "journal": journal.lock().unwrap().clone() }));
        replacements.push((root.display().to_string(), worktree_ids));
        dirs.push(dir);
    }
    let mut text = serde_json::to_string_pretty(&Value::from(documents)).unwrap();
    for (root, ids) in &replacements {
        text = text.replace(root.as_str(), "<root>");
        for (n, id) in ids.iter().enumerate() {
            text = text.replace(id.as_str(), &format!("wt#{n}"));
        }
    }
    let task = Regex::new(r"ultron-task-[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}").unwrap();
    let text = number(&text, &task, |n| format!("ultron-task-#{n}"));
    let hex = Regex::new(r"\b[0-9a-f]{40}\b").unwrap();
    let text = number(&text, &hex, |n| format!("#{n}"));
    let mut got: Value = serde_json::from_str(&text).unwrap();
    let mut want = fixture.clone();
    strip_keys(&mut got, &mut Vec::new());
    strip_keys(&mut want, &mut Vec::new());
    for (g, w) in got.as_array().unwrap().iter().zip(want.as_array().unwrap()) {
        let name = &w["name"];
        for (gs, ws) in g["steps"]
            .as_array()
            .unwrap()
            .iter()
            .zip(w["steps"].as_array().unwrap())
        {
            assert_eq!(gs, ws, "{name} step {}", ws["op"]);
        }
        for key in ["steps", "runs", "events", "journal"] {
            assert_eq!(g[key], w[key], "{name} {key}");
        }
    }
    assert_eq!(got, want);
}
