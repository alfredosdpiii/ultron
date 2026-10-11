//! Replays the scenarios recorded from the TypeScript SessionManager (fixtures/generate.mjs) and
//! requires the same results, errors, manager snapshots, and file bytes.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs;
use std::rc::Rc;
use std::time::{Duration, UNIX_EPOCH};

use serde_json::{Map, Value, json};
use session_file::{
    Env, Error, Leaf, NewSessionOptions, SessionManager, SessionTree, Uuidv7, build_context_entries,
    build_session_context, default_session_dir, find_most_recent_session, find_session_by_id, list_all_sessions,
    list_sessions, load_entries_from_file, migrate_session_entries, migrate_sessions_from_agent_root,
    parse_session_entries,
};

const FIXTURE: &str = include_str!("../fixtures/session-manager.json");

/// The generator's deterministic clock and randomness.
#[derive(Default)]
struct State {
    clock: i64,
    queue: VecDeque<String>,
    counter: u64,
    random_calls: u64,
    uuid: Uuidv7,
    agent_dir: String,
}

#[derive(Clone)]
struct FixtureEnv(Rc<RefCell<State>>);

impl Env for FixtureEnv {
    fn now_ms(&mut self) -> i64 {
        self.0.borrow().clock
    }
    fn random_uuid(&mut self) -> String {
        let mut s = self.0.borrow_mut();
        if let Some(id) = s.queue.pop_front() {
            return id;
        }
        s.counter += 1;
        format!("{:08x}-0000-4000-8000-{:012x}", s.counter, s.counter)
    }
    fn session_id(&mut self) -> String {
        let mut s = self.0.borrow_mut();
        s.random_calls += 1;
        let k = s.random_calls;
        let bytes: [u8; 16] = std::array::from_fn(|i| ((k * 31 + i as u64 * 7) & 0xff) as u8);
        let now = s.clock;
        s.uuid.next(now, bytes)
    }
    fn agent_dir(&self) -> String {
        self.0.borrow().agent_dir.clone()
    }
}

struct Scenario {
    tmp: String,
    encoded: String,
    env: FixtureEnv,
    managers: HashMap<String, SessionManager>,
    named: HashMap<String, Value>,
}

fn map_strings(value: &Value, f: &dyn Fn(&str) -> String) -> Value {
    match value {
        Value::String(s) => Value::String(f(s)),
        Value::Array(items) => Value::Array(items.iter().map(|v| map_strings(v, f)).collect()),
        Value::Object(map) => Value::Object(map.iter().map(|(k, v)| (f(k), map_strings(v, f))).collect()),
        other => other.clone(),
    }
}

fn shorten(s: String) -> String {
    let units: Vec<u16> = s.encode_utf16().collect();
    if units.len() <= 4096 {
        return s;
    }
    let head = String::from_utf16_lossy(&units[..32]);
    let tail = String::from_utf16_lossy(&units[units.len() - 32..]);
    format!("$LONG:{}:{head}:{tail}", units.len())
}

impl Scenario {
    fn sub_str(&self, s: &str) -> String {
        s.replace("$ETMP", &self.encoded).replace("$TMP", &self.tmp)
    }
    fn sub(&self, v: &Value) -> Value {
        map_strings(v, &|s| self.sub_str(s))
    }
    /// Recorded form: JavaScript number semantics (via `JSON.stringify`), placeholders, long strings shortened.
    fn unsub(&self, v: &Value) -> Value {
        let js_view: Value = serde_json::from_str(&session_file::js::stringify(v)).unwrap();
        let cwd = std::env::current_dir().unwrap().to_string_lossy().into_owned();
        map_strings(&js_view, &|s| {
            shorten(
                s.replace(&self.tmp, "$TMP")
                    .replace(&self.encoded, "$ETMP")
                    .replace(&cwd, "$CWD"),
            )
        })
    }
    fn path(&self, p: &Value) -> String {
        let p = p.as_str().expect("path");
        match p.strip_prefix('@') {
            Some(name) => self.managers[name].session_file().expect("session file").to_string(),
            None => self.sub_str(p),
        }
    }
    fn env(&self) -> Box<dyn Env> {
        Box::new(self.env.clone())
    }
}

fn options(v: Option<&Value>) -> Option<NewSessionOptions> {
    let v = v.filter(|v| !v.is_null())?;
    Some(NewSessionOptions {
        id: v.get("id").and_then(Value::as_str).map(str::to_string),
        parent_session: v.get("parentSession").and_then(Value::as_str).map(str::to_string),
    })
}

/// An optional argument: missing or `null` (JSON's spelling of `undefined` in an argument list).
fn opt(args: &[Value], i: usize) -> Option<Value> {
    args.get(i).filter(|v| !v.is_null()).cloned()
}

fn opt_str(args: &[Value], i: usize) -> Option<String> {
    opt(args, i).and_then(|v| v.as_str().map(str::to_string))
}

fn s(args: &[Value], i: usize) -> String {
    args[i].as_str().expect("string argument").to_string()
}

fn error_json(e: &Error) -> Value {
    match e.code() {
        Some(code) => json!({ "code": code }),
        None => json!({ "message": e.to_string() }),
    }
}

fn tree_json(tree: &SessionTree, nodes: &[usize]) -> Value {
    nodes
        .iter()
        .map(|i| {
            let node = &tree.nodes[*i];
            let mut out = Map::new();
            out.insert("entry".into(), node.entry.clone());
            out.insert("children".into(), tree_json(tree, &node.children));
            if let Some(label) = node.label {
                out.insert("label".into(), label.clone());
            }
            if let Some(ts) = node.label_timestamp {
                out.insert("labelTimestamp".into(), ts.clone());
            }
            Value::Object(out)
        })
        .collect()
}

fn context_json(messages: &[Value], thinking: &Option<Value>, model: &Option<Value>) -> Map<String, Value> {
    let mut out = Map::new();
    out.insert("messages".into(), Value::Array(messages.to_vec()));
    if let Some(t) = thinking {
        out.insert("thinkingLevel".into(), t.clone());
    }
    out.insert("model".into(), model.clone().unwrap_or(Value::Null));
    out
}

fn snapshot(sm: &SessionManager) -> Value {
    let projection = sm.build_session_projection();
    let mut projection_json = context_json(&projection.messages, &projection.thinking_level, &projection.model);
    projection_json.insert(
        "entries".into(),
        projection
            .entries
            .iter()
            .map(|e| json!({ "sourceEntry": e.source, "messages": e.messages }))
            .collect(),
    );
    let tree = sm.tree();
    let labels: Map<String, Value> = sm
        .entries()
        .iter()
        .filter_map(|e| {
            let id = e.get("id")?.as_str()?;
            sm.label(id).map(|l| (id.to_string(), l.clone()))
        })
        .collect();
    json!({
        "sessionId": sm.session_id(),
        "sessionFile": sm.session_file(),
        "cwd": sm.cwd(),
        "sessionDir": sm.session_dir(),
        "persisted": sm.is_persisted(),
        "leafId": sm.leaf_id(),
        "header": sm.header(),
        "entries": sm.entries(),
        "tree": tree_json(&tree, &tree.roots),
        "contextEntries": sm.build_context_entries().iter().map(|e| e.get("id").cloned().unwrap_or(Value::Null)).collect::<Vec<_>>(),
        "projection": projection_json,
        "sessionName": sm.session_name(),
        "labels": labels,
        "fileContent": sm.session_file().and_then(|f| fs::read_to_string(f).ok()),
    })
}

fn session_info_json(infos: Vec<session_file::SessionInfo>) -> Value {
    infos
        .into_iter()
        .map(|i| {
            json!({
                "path": i.path, "id": i.id, "cwd": i.cwd, "name": i.name, "parentSessionPath": i.parent_session_path,
                "created": i.created, "modified": i.modified, "messageCount": i.message_count,
                "firstMessage": i.first_message, "allMessagesText": i.all_messages_text,
            })
        })
        .collect()
}

fn leaf(step: &Value) -> Leaf<'_> {
    match step.get("leafId") {
        None => Leaf::Last,
        Some(Value::Null) => Leaf::Root,
        Some(id) => Leaf::Id(id),
    }
}

fn call(sc: &mut Scenario, step: &Value) -> Result<Value, Error> {
    let name = step["sm"].as_str().unwrap();
    let raw: Vec<Value> = step["args"].as_array().unwrap().clone();
    let args: Vec<Value> = raw
        .iter()
        .map(|a| match a.as_str() {
            Some(p) if p.starts_with('@') => Value::String(sc.path(a)),
            Some(n) if n.starts_with('=') => sc.named[&n[1..]].clone(),
            _ => sc.sub(a),
        })
        .collect();
    let sm = sc.managers.get_mut(name).unwrap();
    let id = |r: Result<String, Error>| r.map(Value::from);
    let unit = |r: Result<(), Error>| r.map(|()| Value::Null);
    match step["method"].as_str().unwrap() {
        "appendMessage" => id(sm.append_message(args[0].clone())),
        "appendThinkingLevelChange" => id(sm.append_thinking_level_change(&s(&args, 0))),
        "appendModelChange" => id(sm.append_model_change(&s(&args, 0), &s(&args, 1))),
        "appendCustomEntry" => id(sm.append_custom_entry(&s(&args, 0), opt(&args, 1))),
        "appendUsage" => sm.append_usage(
            &s(&args, 0),
            &s(&args, 1),
            &s(&args, 2),
            args[3].clone(),
            opt_str(&args, 4).as_deref(),
        ),
        "appendSessionInfo" => id(sm.append_session_info(&s(&args, 0))),
        "appendCustomMessageEntry" => id(sm.append_custom_message_entry(
            &s(&args, 0),
            args[1].clone(),
            args[2].as_bool().unwrap(),
            opt(&args, 3),
        )),
        "appendContextEdit" => id(sm.append_context_edit(&s(&args, 0), args[1].clone())),
        "appendLabelChange" => id(sm.append_label_change(&s(&args, 0), opt_str(&args, 1).as_deref())),
        "appendCompaction" => id(sm.append_compaction(
            &s(&args, 0),
            opt_str(&args, 1).as_deref(),
            args[2].as_u64().unwrap(),
            opt(&args, 3),
            opt(&args, 4).and_then(|v| v.as_bool()),
            opt(&args, 5),
        )),
        "branchWithSummary" => id(sm.branch_with_summary(
            opt_str(&args, 0).as_deref(),
            &s(&args, 1),
            opt(&args, 2),
            opt(&args, 3).and_then(|v| v.as_bool()),
            opt(&args, 4),
        )),
        "getChildren" => Ok(sm.children(&s(&args, 0)).into_iter().cloned().collect()),
        "getBranch" => Ok(sm.branch_entries(Some(&s(&args, 0))).into_iter().cloned().collect()),
        "branch" => unit(sm.branch(&s(&args, 0))),
        "resetLeaf" => {
            sm.reset_leaf();
            Ok(Value::Null)
        }
        "newSession" => sm
            .new_session(options(args.first()))
            .map(|f| f.map_or(Value::Null, Value::from)),
        "setSessionFile" => unit(sm.set_session_file(&s(&args, 0))),
        "createBranchedSession" => sm
            .create_branched_session(&s(&args, 0))
            .map(|f| f.map_or(Value::Null, Value::from)),
        other => panic!("unknown method {other}"),
    }
}

fn run_step(sc: &mut Scenario, step: &Value) -> Result<Value, Error> {
    let sm_name = step.get("sm").and_then(Value::as_str).map(str::to_string);
    let opt_dir = |sc: &Scenario, key: &str| step.get(key).map(|d| sc.sub_str(d.as_str().unwrap()));
    let new_manager = match step["op"].as_str().unwrap() {
        "create" => Some(SessionManager::create(
            sc.env(),
            &sc.sub_str(step["cwd"].as_str().unwrap()),
            opt_dir(sc, "dir").as_deref(),
            options(step.get("options")),
        )),
        "open" => Some(SessionManager::open(
            sc.env(),
            &sc.path(&step["path"]),
            opt_dir(sc, "dir").as_deref(),
            opt_dir(sc, "cwdOverride").as_deref(),
        )),
        "continueRecent" => Some(SessionManager::continue_recent(
            sc.env(),
            &sc.sub_str(step["cwd"].as_str().unwrap()),
            opt_dir(sc, "dir").as_deref(),
        )),
        "inMemory" => {
            let entries = step.get("entries").map(|e| sc.sub(e).as_array().unwrap().clone());
            Some(SessionManager::in_memory(
                sc.env(),
                &sc.sub_str(step["cwd"].as_str().unwrap()),
                options(step.get("options")),
                entries,
            ))
        }
        "forkFrom" => Some(SessionManager::fork_from(
            sc.env(),
            &sc.path(&step["source"]),
            &sc.sub_str(step["targetCwd"].as_str().unwrap()),
            opt_dir(sc, "dir").as_deref(),
            options(step.get("options")),
        )),
        _ => None,
    };
    if let Some(result) = new_manager {
        sc.managers.insert(sm_name.unwrap(), result?);
        return Ok(Value::Null);
    }
    let result = match step["op"].as_str().unwrap() {
        "call" => call(sc, step)?,
        "write" => {
            let path = sc.sub_str(step["path"].as_str().unwrap());
            let content = match sc.sub(&step["content"]) {
                Value::String(s) => s,
                Value::Array(parts) => parts
                    .iter()
                    .map(|p| match p {
                        Value::String(s) => s.clone(),
                        Value::Array(rep) => rep[0].as_str().unwrap().repeat(rep[1].as_u64().unwrap() as usize),
                        other => panic!("bad content part {other}"),
                    })
                    .collect(),
                other => panic!("bad content {other}"),
            };
            fs::create_dir_all(std::path::Path::new(&path).parent().unwrap()).unwrap();
            fs::write(&path, content).unwrap();
            Value::Null
        }
        "utimes" => {
            let path = sc.path(&step["path"]);
            let mtime = UNIX_EPOCH + Duration::from_millis(step["mtime"].as_u64().unwrap());
            fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(mtime)
                .unwrap();
            Value::Null
        }
        "ids" => {
            sc.env.0.borrow_mut().queue = step["queue"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect();
            Value::Null
        }
        "loadEntries" => Value::Array(load_entries_from_file(&sc.sub_str(step["path"].as_str().unwrap()))?),
        "findMostRecent" => json!(find_most_recent_session(
            &sc.sub_str(step["dir"].as_str().unwrap()),
            opt_dir(sc, "cwd").as_deref()
        )),
        "findById" => json!(find_session_by_id(
            &sc.env,
            &sc.sub_str(step["cwd"].as_str().unwrap()),
            step["id"].as_str().unwrap(),
            opt_dir(sc, "dir").as_deref()
        )),
        "list" => session_info_json(list_sessions(
            &sc.env,
            &sc.sub_str(step["cwd"].as_str().unwrap()),
            opt_dir(sc, "dir").as_deref(),
        )?),
        "listAll" => session_info_json(list_all_sessions(&sc.env, opt_dir(sc, "dir").as_deref())),
        "defaultSessionDir" => json!(default_session_dir(
            &sc.sub_str(step["cwd"].as_str().unwrap()),
            &sc.env.agent_dir()
        )?),
        "buildSessionContext" => {
            let entries = step["entries"].as_array().unwrap();
            let ctx = build_session_context(entries, leaf(step));
            Value::Object(context_json(&ctx.messages, &ctx.thinking_level, &ctx.model))
        }
        "buildContextEntries" => build_context_entries(step["entries"].as_array().unwrap(), leaf(step))
            .iter()
            .map(|e| e.get("id").cloned().unwrap_or(Value::Null))
            .collect(),
        "migrate" => {
            let mut entries = step["entries"].as_array().unwrap().clone();
            let mut env = sc.env.clone();
            migrate_session_entries(&mut entries, &mut env);
            Value::Array(entries)
        }
        "parse" => Value::Array(parse_session_entries(step["content"].as_str().unwrap())),
        "migrateAgentRoot" => {
            migrate_sessions_from_agent_root(&sc.env.agent_dir());
            Value::Null
        }
        other => panic!("unknown op {other}"),
    };
    if let Some(name) = step.get("as").and_then(Value::as_str) {
        sc.named.insert(name.to_string(), result.clone());
    }
    Ok(result)
}

fn list_files(dir: &std::path::Path, root: &std::path::Path, out: &mut BTreeMap<String, Value>) {
    let mut names: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).collect();
    names.sort();
    for path in names {
        if path.is_dir() {
            list_files(&path, root, out);
        } else {
            let rel = path.strip_prefix(root).unwrap().to_string_lossy().into_owned();
            out.insert(
                rel,
                Value::String(String::from_utf8_lossy(&fs::read(&path).unwrap()).into_owned()),
            );
        }
    }
}

fn replay(name: &str) {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    let scenario = fixture["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == name)
        .expect("scenario");
    let dir = tempfile::Builder::new()
        .prefix("session-file-fixture-")
        .tempdir()
        .unwrap();
    let tmp = dir.path().to_string_lossy().into_owned();
    let encoded = tmp.trim_start_matches(['/', '\\']).replace(['/', '\\', ':'], "-");
    let env = FixtureEnv(Rc::new(RefCell::new(State {
        agent_dir: format!("{tmp}/agent"),
        ..State::default()
    })));
    let mut sc = Scenario {
        tmp,
        encoded,
        env,
        managers: HashMap::new(),
        named: HashMap::new(),
    };
    for (i, step) in scenario["steps"].as_array().unwrap().iter().enumerate() {
        if let Some(now) = step.get("now").and_then(Value::as_i64) {
            sc.env.0.borrow_mut().clock = now;
        }
        let outcome = run_step(&mut sc, step);
        let context = format!(
            "{name} step {i}: {}",
            serde_json::to_string(step)
                .unwrap()
                .chars()
                .take(300)
                .collect::<String>()
        );
        match (&outcome, step.get("error")) {
            (Ok(result), None) => assert_eq!(sc.unsub(result), step["result"], "{context}"),
            (Err(e), Some(expected)) => assert_eq!(sc.unsub(&error_json(e)), *expected, "{context}"),
            (Ok(result), Some(expected)) => panic!("{context}: expected error {expected}, got {result}"),
            (Err(e), None) => panic!("{context}: unexpected error {e}"),
        }
        if step["op"] == "loadEntries" {
            let after = fs::read_to_string(sc.sub_str(step["path"].as_str().unwrap())).ok();
            assert_eq!(sc.unsub(&json!(after)), step["fileAfter"], "{context}: file after load");
        }
        if let Some(expected) = step.get("snapshot") {
            let sm = &sc.managers[step["sm"].as_str().unwrap()];
            let actual = sc.unsub(&snapshot(sm));
            for key in expected.as_object().unwrap().keys() {
                assert_eq!(actual[key], expected[key], "{context}: snapshot.{key}");
            }
            assert_eq!(&actual, expected, "{context}: snapshot");
        }
    }
    let mut files = BTreeMap::new();
    list_files(dir.path(), dir.path(), &mut files);
    let files = sc.unsub(&json!(files));
    assert_eq!(files, scenario["files"], "{name}: files left on disk");
}

#[test]
fn session_file_persisted_lifecycle_matches_typescript() {
    replay("persisted_lifecycle");
}

#[test]
fn session_file_deferred_flush_and_branched_sessions_match_typescript() {
    replay("deferred_flush_and_branched_sessions");
}

#[test]
fn session_file_open_edge_cases_match_typescript() {
    replay("open_edge_cases");
}

#[test]
fn session_file_migrations_match_typescript() {
    replay("migrations");
}

#[test]
fn session_file_fork_and_discovery_match_typescript() {
    replay("fork_and_discovery");
}

#[test]
fn session_file_agent_root_migration_matches_typescript() {
    replay("agent_root_migration");
}

#[test]
fn session_file_pure_context_matches_typescript() {
    replay("pure_context");
}

#[test]
fn session_file_tree_order_matches_typescript() {
    replay("tree_order");
}

#[test]
fn session_file_fixture_covers_every_scenario() {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    let names: Vec<&str> = fixture["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "persisted_lifecycle",
            "deferred_flush_and_branched_sessions",
            "open_edge_cases",
            "migrations",
            "fork_and_discovery",
            "agent_root_migration",
            "pure_context",
            "tree_order"
        ]
    );
}
