//! Replays fixtures/worktrees.json (recorded by fixtures/generate-worktrees.mjs from worktrees.ts): the same scripted
//! operations on fresh throwaway repositories give the same records, commits, merges (clean, conflicting, with
//! markers), setup links and copies, removals, prunes and errors. Temp roots, worktree ids and Git object ids are
//! normalized the same way as the generator (ids numbered by first appearance over the whole document).

use delegation::worktrees::{
    self as wt, CommitOutcome, CreateWorktreeOptions, MergeSource, PruneOptions, WorktreeRecord, WorktreeSetup,
};
use regex::Regex;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;

fn fixture() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/worktrees.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn sh(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git").args(args).current_dir(cwd).output().unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect()
}

fn setup(value: &Value) -> Option<WorktreeSetup> {
    let value = value.as_object()?;
    Some(WorktreeSetup {
        link: value.get("link").map(strings),
        copy: value.get("copy").map(strings),
        command: value.get("command").and_then(Value::as_str).map(String::from),
        timeout_ms: value.get("timeoutMs").and_then(Value::as_u64),
    })
}

struct Run {
    name: String,
    root: PathBuf,
    records: Vec<WorktreeRecord>,
    commits: Vec<Option<CommitOutcome>>,
}

impl Run {
    fn path(&self, raw: &str) -> PathBuf {
        if let Some(rest) = raw.strip_prefix('$') {
            let (n, tail) = rest.split_once('/').map_or((rest, ""), |(n, t)| (n, t));
            let base = PathBuf::from(&self.records[n.parse::<usize>().unwrap()].path);
            return if tail.is_empty() { base } else { base.join(tail) };
        }
        self.root.join(raw)
    }

    fn op(&mut self, op: &[Value]) -> Result<Value, (String, bool)> {
        let kind = op[0].as_str().unwrap();
        let s = |i: usize| op[i].as_str().unwrap().to_string();
        let err = |e: wt::WorktreeError| (e.message, e.fallback);
        Ok(match kind {
            "init" => {
                let dir = self.root.join(s(1));
                std::fs::create_dir_all(&dir).unwrap();
                sh(&dir, &["init", "-q", "-b", "main"]);
                sh(&dir, &["config", "user.name", "Fixture"]);
                sh(&dir, &["config", "user.email", "fixture@example.com"]);
                for (path, text) in op[2].as_object().unwrap() {
                    let file = dir.join(path);
                    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
                    std::fs::write(file, text.as_str().unwrap()).unwrap();
                }
                let quiet = ["-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null"];
                sh(&dir, &[&quiet[..], &["add", "-A"]].concat());
                let message = format!("initial: {}", self.name);
                sh(&dir, &[&quiet[..], &["commit", "-q", "-m", &message]].concat());
                Value::Null
            }
            "write" => {
                let path = self.path(&s(1));
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, s(2)).unwrap();
                Value::Null
            }
            "symlink" => {
                let path = self.path(&s(2));
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::os::unix::fs::symlink(s(1), path).unwrap();
                Value::Null
            }
            "mkdir" => {
                std::fs::create_dir_all(self.path(&s(1))).unwrap();
                Value::Null
            }
            "rm" => {
                std::fs::remove_file(self.path(&s(1))).unwrap();
                Value::Null
            }
            "read" => std::fs::read_to_string(self.path(&s(1))).map_or(Value::Null, Value::from),
            "exists" => json!(std::fs::symlink_metadata(self.path(&s(1))).is_ok()),
            "ls" => {
                let dir = self.path(&s(1));
                let mut names: Vec<String> = std::fs::read_dir(&dir)
                    .unwrap()
                    .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                    .collect();
                names.sort();
                Value::from(
                    names
                        .into_iter()
                        .map(|name| {
                            let info = std::fs::symlink_metadata(dir.join(&name)).unwrap();
                            if info.file_type().is_symlink() {
                                let link = std::fs::read_link(dir.join(&name)).unwrap();
                                json!({ "name": name, "link": link.display().to_string() })
                            } else {
                                json!({ "name": name, "dir": info.is_dir() })
                            }
                        })
                        .collect::<Vec<_>>(),
                )
            }
            "create" => {
                let o = &op[1];
                let setup = setup(&o["setup"]);
                let record = wt::create_child_worktree(&CreateWorktreeOptions {
                    cwd: &self.path(o["cwd"].as_str().unwrap()),
                    session_id: o["sessionId"].as_str().unwrap(),
                    name: o["name"].as_str().unwrap(),
                    setup: setup.as_ref(),
                    now: 0.0,
                })
                .map_err(err)?;
                self.records.push(record.clone());
                self.commits.push(None);
                serde_json::to_value(&record).unwrap()
            }
            "dirty" => json!(wt::list_dirty(&self.path(&s(1)))),
            "commit" | "commitPaths" => {
                let n = op[1].as_u64().unwrap() as usize;
                let record = &self.records[n];
                let outcome = if kind == "commit" {
                    let o = &op[2];
                    let message = wt::commit_message(
                        o["summary"].as_str().unwrap(),
                        &record.name,
                        o["taskId"].as_str(),
                        o["status"].as_str().unwrap(),
                    );
                    wt::commit_child_worktree(record, &message, None)
                } else {
                    wt::commit_child_worktree(record, "limited", Some(&strings(&op[2])))
                }
                .map_err(err)?;
                self.commits[n] = Some(outcome.clone());
                outcome.to_json()
            }
            "merge" => {
                let n = op[2].as_u64().unwrap() as usize;
                let record = &self.records[n];
                let source = MergeSource {
                    name: record.name.clone(),
                    branch: record.branch.clone(),
                    base: record.base.clone(),
                    commit: self.commits[n].as_ref().unwrap().commit.clone(),
                };
                wt::merge_child_worktree(&self.path(&s(1)), &source, op[3] == "markers")
                    .map_err(err)?
                    .to_json()
            }
            "remove" => {
                wt::remove_child_worktree(&self.records[op[1].as_u64().unwrap() as usize], op[2] == true);
                Value::Null
            }
            "branches" => Value::from(
                sh(
                    &self.path(&s(1)),
                    &["for-each-ref", "--format=%(refname:short)", "refs/heads"],
                )
                .lines()
                .map(String::from)
                .collect::<Vec<_>>(),
            ),
            "records" => serde_json::to_value(wt::list_records(&self.path(&s(1)))).unwrap(),
            "status" => json!(sh(
                &self.path(&s(1)),
                &["status", "--porcelain=v1", "--untracked-files=all"]
            )),
            "git" => {
                let cwd = if s(1).is_empty() {
                    self.root.clone()
                } else {
                    self.path(&s(1))
                };
                let args = strings(&op[2]);
                json!(sh(&cwd, &args.iter().map(String::as_str).collect::<Vec<_>>()))
            }
            "notes" => {
                let record = &self.records[op[1].as_u64().unwrap() as usize];
                let env: serde_json::Map<String, Value> = wt::worktree_env(record)
                    .into_iter()
                    .map(|(k, v)| (k, json!(v)))
                    .collect();
                json!({ "env": env, "brief": wt::worktree_brief_note(record) })
            }
            "setupCommand" => {
                wt::run_setup_command(&self.records[op[1].as_u64().unwrap() as usize], None).map_err(err)?;
                Value::Null
            }
            "deadOwner" => {
                let record = &self.records[op[1].as_u64().unwrap() as usize];
                let path = Path::new(&record.common_dir)
                    .join(wt::WORKTREES_DIR)
                    .join(format!("{}.json", record.id));
                let mut saved: WorktreeRecord = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
                saved.owner.pid = 2147483646;
                std::fs::write(&path, format!("{}\n", wt::record_json(&saved))).unwrap();
                Value::Null
            }
            "prune" => {
                let o = &op[2];
                wt::prune_worktrees(
                    &self.path(&s(1)),
                    &PruneOptions {
                        select: None,
                        force: o["force"] == true,
                        delete_branches: o["deleteBranches"] == true,
                    },
                )
                .to_json()
            }
            other => panic!("unknown op {other}"),
        })
    }
}

/// The generator's normalization: roots, worktree ids, then 40-hex ids by first appearance in document order.
fn normalize(document: Value, roots: &[(String, Vec<String>)]) -> Value {
    let mut text = serde_json::to_string_pretty(&document).unwrap();
    for (root, ids) in roots {
        text = text.replace(root.as_str(), "<root>");
        for (n, id) in ids.iter().enumerate() {
            text = text.replace(id.as_str(), &format!("wt#{n}"));
        }
    }
    let hex = Regex::new(r"\b[0-9a-f]{40}\b").unwrap();
    let mut seen: Vec<String> = Vec::new();
    let text = hex.replace_all(&text, |c: &regex::Captures<'_>| {
        let h = c[0].to_string();
        let n = seen.iter().position(|s| *s == h).unwrap_or_else(|| {
            seen.push(h);
            seen.len() - 1
        });
        format!("#{n}")
    });
    let mut value: Value = serde_json::from_str(&text).unwrap();
    fn fix(value: &mut Value) {
        match value {
            Value::Array(items) => items.iter_mut().for_each(fix),
            Value::Object(map) => {
                if map.get("owner").is_some_and(Value::is_object) {
                    map.insert("owner".into(), json!({ "pid": 0, "host": "<host>" }));
                }
                if map.get("createdAt").is_some_and(Value::is_number) {
                    map.insert("createdAt".into(), json!(0));
                }
                map.values_mut().for_each(fix);
            }
            _ => {}
        }
    }
    for scenario in value["scenarios"].as_array_mut().unwrap() {
        fix(&mut scenario["steps"]);
    }
    value
}

#[test]
fn delegation_worktrees_replay_typescript_scenarios() {
    let fixture = fixture();
    let mut scenarios = Vec::new();
    let mut roots = Vec::new();
    let mut keep = Vec::new();
    for scenario in fixture["scenarios"].as_array().unwrap() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let mut run = Run {
            name: scenario["name"].as_str().unwrap().to_string(),
            root: root.clone(),
            records: Vec::new(),
            commits: Vec::new(),
        };
        let mut steps = Vec::new();
        for step in scenario["steps"].as_array().unwrap() {
            let op = step["op"].as_array().unwrap();
            steps.push(match run.op(op) {
                Ok(result) => json!({ "op": step["op"], "result": result }),
                Err((error, fallback)) => json!({ "op": step["op"], "error": error, "fallback": fallback }),
            });
        }
        roots.push((
            root.display().to_string(),
            run.records.iter().map(|r| r.id.clone()).collect(),
        ));
        scenarios.push(json!({ "name": scenario["name"], "steps": steps }));
        keep.push(dir);
    }
    let got = normalize(json!({ "scenarios": scenarios }), &roots);
    for (g, w) in got["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .zip(fixture["scenarios"].as_array().unwrap())
    {
        for (gs, ws) in g["steps"]
            .as_array()
            .unwrap()
            .iter()
            .zip(w["steps"].as_array().unwrap())
        {
            assert_eq!(gs, ws, "{}", w["name"]);
        }
    }
    assert_eq!(got["scenarios"], fixture["scenarios"]);
}

#[test]
fn delegation_worktree_pure_helpers_match_typescript() {
    let fixture = fixture();
    for case in fixture["pure"]["commitMessage"].as_array().unwrap() {
        let a = case["args"].as_array().unwrap();
        assert_eq!(
            wt::commit_message(
                a[0].as_str().unwrap(),
                a[1].as_str().unwrap(),
                a[2].as_str(),
                a[3].as_str().unwrap()
            ),
            case["result"].as_str().unwrap(),
            "{a:?}"
        );
    }
    for case in fixture["pure"]["conflictHunks"].as_array().unwrap() {
        let got: Vec<Value> = wt::conflict_hunks(case["text"].as_str().unwrap())
            .into_iter()
            .map(|h| json!({ "line": h.line, "ours": h.ours, "theirs": h.theirs }))
            .collect();
        assert_eq!(Value::from(got), case["result"], "{}", case["text"]);
    }
    for case in fixture["pure"]["keepWorktrees"].as_array().unwrap() {
        assert_eq!(
            json!(wt::keep_worktrees(case["value"].as_str())),
            case["result"],
            "{}",
            case["value"]
        );
    }
}

/// A same-size edit made in the second its file was staged is in the snapshot even when the snapshot is taken a
/// second later, as the real index sees it (`git diff` reports it): the index copy keeps the index's mtime, so Git
/// still treats the entry as racily clean and rehashes it. A copy with a fresh mtime trusted the stale stat and the
/// snapshot left the edit out (the load-only flake of `delegation_worktrees_replay_typescript_scenarios`, whose
/// conflicting merge then came back as "changed during the merge").
#[test]
fn delegation_worktree_snapshot_keeps_a_same_second_same_size_edit() {
    let dir = tempfile::tempdir().unwrap();
    let top = std::fs::canonicalize(dir.path()).unwrap();
    sh(&top, &["init", "-q", "-b", "main"]);
    sh(&top, &["config", "user.name", "Fixture"]);
    sh(&top, &["config", "user.email", "fixture@example.com"]);
    let file = top.join("a.txt");
    let index = top.join(".git/index");
    let second = |path: &Path| {
        std::fs::metadata(path)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    };
    // Commit, then rewrite with the same size within the second the file was staged (setup only: a run that crosses
    // a second boundary commits again). The index is last written by that commit, while the content still matched.
    let commit = [
        "-c",
        "commit.gpgsign=false",
        "commit",
        "-q",
        "--allow-empty",
        "-m",
        "base",
    ];
    loop {
        std::fs::write(&file, "shared\n").unwrap();
        let staged = second(&file);
        sh(&top, &["add", "a.txt"]);
        sh(&top, &commit);
        std::fs::write(&file, "parent\n").unwrap();
        if second(&file) == staged && second(&index) == staged {
            break;
        }
    }
    let head = sh(&top, &["rev-parse", "HEAD"]).trim().to_string();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    // The real index sees the edit (`git diff` writes no index).
    assert_eq!(sh(&top, &["diff", "--name-only"]), "a.txt\n");
    let (commit, dirty) = wt::snapshot_working_tree(&top, &head, "snapshot").unwrap();
    assert!(dirty);
    assert_eq!(sh(&top, &["show", &format!("{commit}:a.txt")]), "parent\n");
}
