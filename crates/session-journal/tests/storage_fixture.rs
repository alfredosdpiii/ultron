//! Replays `fixtures/storage.json` (recorded from the TypeScript format-4 session journal by
//! `fixtures/generate.mjs`): every operation's result and every session file's bytes must match.

use serde_json::{Map, Value, json};
use session_journal::fork::ForkOptions;
use session_journal::repo::Metadata;
use session_journal::state::Write;
use session_journal::{Session, SessionRepo, SharedClock, TestClock, context};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};

const START: i64 = 1_790_000_000_000;

fn meta_json(m: &Metadata, root: &Path) -> Value {
    let mut out = Map::new();
    out.insert("id".into(), m.id.clone().into());
    out.insert("createdAt".into(), m.created_at.into());
    out.insert("storageVersion".into(), m.storage_version.into());
    out.insert("cwd".into(), m.cwd.clone().into());
    out.insert(
        "path".into(),
        m.path.strip_prefix(root).unwrap().display().to_string().into(),
    );
    if let Some(p) = &m.parent_session_id {
        out.insert("parentSessionId".into(), p.clone().into());
    }
    if let Some(p) = &m.legacy_parent_session_path {
        out.insert("legacyParentSessionPath".into(), p.clone().into());
    }
    Value::Object(out)
}

/// A TypeScript `Write` object.
fn write_of(value: &Value) -> Write {
    let ns = value["namespace"].as_str().unwrap_or("");
    let key = value["key"].as_str().unwrap_or("");
    match (value["kind"].as_str().unwrap(), value["op"].as_str()) {
        ("entry", _) => Write::Entry(value["entry"].as_object().unwrap().clone()),
        ("usage", _) => Write::Usage(value["row"].as_object().unwrap().clone()),
        ("value", Some("set")) => Write::set(ns, key, value["value"].clone()),
        ("value", _) => Write::delete(ns, key),
        ("list", Some("append")) => Write::append(ns, key, value["value"].clone()),
        _ => Write::delete_list(ns, key),
    }
}

fn snapshot(root: &Path) -> BTreeMap<String, String> {
    fn walk(dir: &Path, root: &Path, out: &mut BTreeMap<String, String>) {
        let mut entries: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, root, out);
            } else {
                out.insert(
                    path.strip_prefix(root).unwrap().display().to_string(),
                    std::fs::read_to_string(&path).unwrap(),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

fn dump(session: &Session) -> Value {
    let state = session.state();
    let strip = |v: &Value| {
        let mut m = v.as_object().unwrap().clone();
        m.shift_remove("kind");
        Value::Object(m)
    };
    json!({
        "stats": session.storage().stats(),
        "nextSeq": state.next_seq(),
        "entries": state.entries().iter().map(strip).collect::<Vec<_>>(),
        "values": state.values().map(|v| json!({"namespace": v.namespace, "key": v.key, "value": v.value, "seq": v.seq})).collect::<Vec<_>>(),
        "lists": state.lists().into_iter().map(|(namespace, key, elements)| json!({
            "namespace": namespace, "key": key,
            "elements": elements.iter().map(|e| json!({"seq": e.seq, "value": e.value})).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "usage": state.usage_rows().into_iter().cloned().collect::<Vec<_>>(),
        "legacyV3": session.storage().is_legacy_v3(),
    })
}

fn fork_options(options: &Value, nth: &[String]) -> (ForkOptions, Option<String>) {
    let id = options["id"].as_str().map(str::to_string);
    if options["scope"] == "tree" {
        return (ForkOptions::Tree, id);
    }
    let entry_id = match options.get("entryNth") {
        Some(n) => Some(nth[n.as_u64().unwrap() as usize].clone()),
        None => options["entryId"].as_str().map(str::to_string),
    };
    (
        ForkOptions::Branch {
            branch: options["branch"].as_str().unwrap().into(),
            entry_id,
            any_tree_entry: options["anyTreeEntry"] == true,
            before: options["position"] == "before",
        },
        id,
    )
}

fn replace_nth(value: &Value, nth: &[String]) -> Value {
    let text = serde_json::to_string(value).unwrap();
    let mut out = String::new();
    let mut rest = text.as_str();
    while let Some(i) = rest.find("\"nth:") {
        out.push_str(&rest[..i]);
        let tail = &rest[i + 5..];
        let end = tail.find('"').unwrap();
        let n: usize = tail[..end].parse().unwrap();
        out.push_str(&serde_json::to_string(&nth[n]).unwrap());
        rest = &tail[end + 1..];
    }
    out.push_str(rest);
    serde_json::from_str(&out).unwrap()
}

struct Runner {
    root: tempfile::TempDir,
    repo: SessionRepo,
    sessions: HashMap<String, Session>,
    metas: HashMap<String, Metadata>,
    nth: HashMap<String, Vec<String>>,
}

impl Runner {
    fn root(&self) -> &Path {
        self.root.path()
    }

    fn track(&mut self, key: &str, session: Session) -> Value {
        let meta = meta_json(session.metadata(), self.root());
        self.metas.insert(key.into(), session.metadata().clone());
        self.sessions.insert(key.into(), session);
        meta
    }

    fn session(&mut self, key: &str) -> Result<&mut Session, String> {
        self.sessions
            .get_mut(key)
            .ok_or_else(|| format!("no open session {key}"))
    }

    fn listed(&self, cwd: &str, id: &str) -> Metadata {
        self.repo.list(Some(cwd)).into_iter().find(|m| m.id == id).unwrap()
    }

    fn fork(&mut self, op: &Value, source: Metadata) -> Result<Value, String> {
        let key = op["source"].as_str().unwrap();
        let nth = self.nth.get(key).cloned().unwrap_or_default();
        let (options, id) = fork_options(&op["options"], &nth);
        let forked = match self.sessions.get(key) {
            Some(open) => open.fork_into(&self.repo, &options, id.as_deref())?,
            None => self.repo.fork(&source, &options, id.as_deref())?,
        };
        Ok(self.track(op["s"].as_str().unwrap(), forked))
    }

    fn run(&mut self, op: &Value) -> Result<Value, String> {
        let s = op["s"].as_str().unwrap_or("").to_string();
        Ok(match op["op"].as_str().unwrap() {
            "create" => {
                let session = self.repo.create(op["cwd"].as_str().unwrap(), op["id"].as_str(), None)?;
                self.track(&s, session)
            }
            "createBranch" => {
                let at = op["at"].clone();
                self.session(&s)?
                    .commit(&[Write::set("pi.branch.tip", op["branch"].as_str().unwrap(), at)])?;
                Value::Null
            }
            "createBranchAtNth" => {
                let at = self.nth[&s][op["nth"].as_u64().unwrap() as usize].clone();
                self.session(&s)?
                    .commit(&[Write::set("pi.branch.tip", op["branch"].as_str().unwrap(), at.into())])?;
                Value::Null
            }
            "commit" | "commitOn" => {
                let writes: Vec<Write> = op["writes"].as_array().unwrap().iter().map(write_of).collect();
                let result = self.session(&s)?.commit(&writes)?;
                json!({"firstSeq": result.first_seq, "seqs": result.seqs, "timestamp": result.timestamp, "stats": result.stats})
            }
            "commitEntry" => {
                let id = format!("entry-{}", op["step"].as_u64().unwrap());
                let session = self.session(&s)?;
                let tip = session.branch_tip("main").flatten();
                let mut entry = Map::new();
                entry.insert("id".into(), id.clone().into());
                entry.insert("parentId".into(), tip.map_or(Value::Null, Value::from));
                entry.extend(op["entry"].as_object().unwrap().clone());
                let result = session.commit(&[
                    Write::Entry(entry),
                    Write::set("pi.branch.tip", "main", id.clone().into()),
                ])?;
                self.nth.entry(s).or_default().push(id);
                json!({"firstSeq": result.first_seq, "seqs": result.seqs, "timestamp": result.timestamp, "stats": result.stats})
            }
            "append" | "appendBranch" => {
                let branch = op["branch"].as_str().unwrap_or("main").to_string();
                let id = self.session(&s)?.append_message(&branch, op["message"].clone())?;
                self.nth.entry(s).or_default().push(id.clone());
                id.into()
            }
            "appendCustom" => {
                let data = replace_nth(&op["data"], self.nth.get(&s).map_or(&[][..], Vec::as_slice));
                let id = self
                    .session(&s)?
                    .append_custom("main", op["customType"].as_str().unwrap(), Some(data))?;
                self.nth.entry(s).or_default().push(id.clone());
                id.into()
            }
            "setName" => {
                self.session(&s)?.set_name(op["name"].as_str())?;
                Value::Null
            }
            "setLabelNth" => {
                let target = self.nth[&s][op["nth"].as_u64().unwrap() as usize].clone();
                self.session(&s)?.set_label(&target, op["label"].as_str())?;
                Value::Null
            }
            "close" => {
                self.sessions.remove(&s).ok_or("no session")?;
                Value::Null
            }
            "open" => {
                let session = self.repo.open(&self.metas[&s].clone())?;
                self.track(&s, session)
            }
            "openListed" => {
                let meta = self.listed(op["cwd"].as_str().unwrap(), op["id"].as_str().unwrap());
                let session = self.repo.open(&meta)?;
                self.track(&s, session)
            }
            "appendRaw" => {
                let path = self.metas[&s].path.clone();
                let mut text = std::fs::read_to_string(&path).unwrap();
                text.push_str(op["text"].as_str().unwrap());
                std::fs::write(&path, text).unwrap();
                Value::Null
            }
            "writeFile" => {
                let path = self.root().join(op["path"].as_str().unwrap());
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                let text = op["text"]
                    .as_str()
                    .unwrap()
                    .replace("<ROOT>", &self.root().display().to_string());
                std::fs::write(&path, text).unwrap();
                Value::Null
            }
            "list" => Value::Array(
                self.repo
                    .list(op["cwd"].as_str())
                    .iter()
                    .map(|m| meta_json(m, self.root()))
                    .collect(),
            ),
            "listAll" => Value::Array(self.repo.list(None).iter().map(|m| meta_json(m, self.root())).collect()),
            "fork" | "forkOpen" => {
                let source = self.metas[op["source"].as_str().unwrap()].clone();
                self.fork(op, source)?
            }
            "forkListed" => {
                let source = self.listed(op["cwd"].as_str().unwrap(), op["source"].as_str().unwrap());
                self.fork(op, source)?
            }
            "dump" => dump(self.session(&s)?),
            "context" | "contextBranch" => {
                let session = self.session(&s)?;
                let mut tip = session.branch_tip("main").flatten();
                if op.get("tipOf").is_some() {
                    tip = session
                        .state()
                        .entries()
                        .iter()
                        .find(|e| e["message"]["content"][0]["text"] == "side")
                        .map(|e| e["id"].as_str().unwrap().to_string());
                }
                let path = session.state().branch_path(tip.as_deref())?;
                Value::Array(context::build_session_context(&path))
            }
            other => panic!("unknown op {other}"),
        })
    }
}

#[test]
fn session_journal_storage_matches_typescript() {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/storage.json")).unwrap();
    // One clock for every scenario: the TypeScript UUIDv7 sequence is process-wide.
    let clock = Arc::new(Mutex::new(TestClock::new(START, 1)));
    for scenario in fixture.as_array().unwrap() {
        clock.lock().unwrap().time = START;
        let shared: SharedClock = clock.clone();
        let root = tempfile::tempdir().unwrap();
        let repo = SessionRepo::new(root.path(), shared);
        let mut runner = Runner {
            root,
            repo,
            sessions: HashMap::new(),
            metas: HashMap::new(),
            nth: HashMap::new(),
        };
        let name = scenario["name"].as_str().unwrap();
        let mut previous = BTreeMap::new();
        for (index, (op, step)) in scenario["ops"]
            .as_array()
            .unwrap()
            .iter()
            .zip(scenario["steps"].as_array().unwrap())
            .enumerate()
        {
            let mut op = op.clone();
            op["step"] = index.into();
            let root_text = runner.root().display().to_string();
            let got = match runner.run(&op) {
                Ok(value) => value,
                Err(error) => json!({"error": error}),
            };
            let got: Value =
                serde_json::from_str(&serde_json::to_string(&got).unwrap().replace(&root_text, "<ROOT>")).unwrap();
            let want = &step["result"];
            let context = format!("{name} step {index} {}", op["op"]);
            match (
                want.get("error").and_then(Value::as_str),
                got.get("error").and_then(Value::as_str),
            ) {
                (Some(want), Some(got)) => assert!(
                    got.starts_with(want),
                    "{context}: error {got:?} does not start with {want:?}"
                ),
                _ => assert_eq!(got, *want, "{context}"),
            }
            let files: BTreeMap<String, String> = snapshot(runner.root())
                .into_iter()
                .map(|(k, v)| (k, v.replace(&root_text, "<ROOT>")))
                .collect();
            if let Some(want) = step.get("files") {
                let want: BTreeMap<String, String> = serde_json::from_value(want.clone()).unwrap();
                for (path, text) in &want {
                    assert_eq!(files.get(path), Some(text), "{context}: file {path}");
                }
                assert_eq!(
                    files.keys().collect::<Vec<_>>(),
                    want.keys().collect::<Vec<_>>(),
                    "{context}: file set"
                );
            } else {
                assert_eq!(
                    files, previous,
                    "{context}: files changed but TypeScript left them unchanged"
                );
            }
            previous = files;
        }
    }
}
