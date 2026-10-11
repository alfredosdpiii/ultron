//! `skills.*` requests (port of `ultron/skills.ts`): the version-pinned, explainable skill catalog and the code-skill
//! requests, journaled in the module document. Selection never grants capabilities: frontmatter keys that would
//! grant tools, models, budgets or permissions are reported and ignored.

use crate::code_skills::{CodeSkillKind, CodeSkills};
use crate::store::{DocumentStore, read_versioned};
use crate::util::{
    Clock, fields, js_trim, locale_compare, lock, object, safe_integer, sha256_hex, utf16_len, utf16_prefix, uuid,
};
use kernel::Signal;
use serde_json::{Map, Value, json};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use yaml_rust2::Yaml;

/// A skill as Pi's loader discovered it; this module never discovers skills itself.
#[derive(Debug, Clone)]
pub struct SkillSource {
    pub name: String,
    pub description: String,
    pub file_path: String,
    pub content: String,
    pub disable_model_invocation: bool,
}

pub type SkillLoader = Arc<dyn Fn() -> Result<Vec<SkillSource>, String> + Send + Sync>;

const CAPABILITY_KEYS: &[&str] = &[
    "allowed-tools",
    "allowed_tools",
    "allowedTools",
    "tools",
    "grants",
    "permissions",
    "capabilities",
    "model",
    "budget",
    "timeout",
];
const STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "for", "how", "i", "in", "is", "it", "me", "my", "of", "on", "or", "the", "this", "to",
    "with",
];
const EXACT_NAME_BONUS: i64 = 5;
const MAX_RECORDS: usize = 2000;
const MAX_QUERY_CHARS: usize = 4000;

struct Skill {
    name: String,
    description: String,
    file_path: String,
    version: String,
    invocable: bool,
    content: String,
    body: String,
    ignored: Map<String, Value>,
}

fn yaml_json(value: &Yaml) -> Value {
    match value {
        Yaml::Real(text) => text.parse::<f64>().map(Value::from).unwrap_or(Value::Null),
        Yaml::Integer(number) => Value::from(*number),
        Yaml::String(text) => Value::String(text.clone()),
        Yaml::Boolean(flag) => Value::Bool(*flag),
        Yaml::Array(items) => Value::Array(items.iter().map(yaml_json).collect()),
        Yaml::Hash(map) => Value::Object(
            map.iter()
                .map(|(key, value)| {
                    let key = match key {
                        Yaml::String(text) | Yaml::Real(text) => text.clone(),
                        Yaml::Integer(number) => number.to_string(),
                        Yaml::Boolean(flag) => flag.to_string(),
                        _ => "null".into(),
                    };
                    (key, yaml_json(value))
                })
                .collect(),
        ),
        _ => Value::Null,
    }
}

fn parse_skill(source: &SkillSource) -> Skill {
    let (frontmatter, body) = match resources::frontmatter::parse(&source.content) {
        Ok(parsed) => (parsed.frontmatter, parsed.body),
        // Pi's loader already rejected unparseable skills; tolerate them here as plain bodies.
        Err(_) => (Yaml::Null, source.content.clone()),
    };
    let mut ignored = Map::new();
    if let Yaml::Hash(map) = &frontmatter {
        for key in CAPABILITY_KEYS {
            if let Some(value) = map.get(&Yaml::String((*key).to_string())) {
                ignored.insert((*key).to_string(), yaml_json(value));
            }
        }
    }
    Skill {
        name: source.name.clone(),
        description: source.description.clone(),
        file_path: source.file_path.clone(),
        version: sha256_hex(source.content.as_bytes()),
        invocable: !source.disable_model_invocation,
        content: source.content.clone(),
        body,
        ignored,
    }
}

fn tokens(text: &str) -> Vec<String> {
    let lower = text.to_lowercase();
    lower
        .split(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit()))
        .filter(|token| !token.is_empty())
        .map(str::to_string)
        .collect()
}

fn query_terms(query: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    tokens(query)
        .into_iter()
        .filter(|term| term.len() > 1 && !STOPWORDS.contains(&term.as_str()) && seen.insert(term.clone()))
        .collect()
}

fn score(skill: &Skill, terms: &[String], query: &str) -> Option<Value> {
    let set = |text: &str| -> HashSet<String> { tokens(text).into_iter().collect() };
    let (name, description, body) = (set(&skill.name), set(&skill.description), set(&skill.body));
    let mut matches = Vec::new();
    let mut total = 0;
    for term in terms {
        let mut matched = Vec::new();
        for (field, tokens, weight) in [("name", &name, 3), ("description", &description, 2), ("body", &body, 1)] {
            if tokens.contains(term) {
                matched.push(field);
                total += weight;
            }
        }
        if !matched.is_empty() {
            matches.push(json!({"term": term, "fields": matched}));
        }
    }
    if skill.name.to_lowercase() == query.trim().to_lowercase() {
        total += EXACT_NAME_BONUS;
    }
    (total > 0).then(|| json!({"name": skill.name, "version": skill.version, "score": total, "matches": matches}))
}

fn nonempty_string(value: Option<&Value>, name: &str) -> Result<String, String> {
    match value.and_then(Value::as_str) {
        Some(text) if !js_trim(text).is_empty() => Ok(text.to_string()),
        _ => Err(format!("{name} must be a nonempty string")),
    }
}

fn version_string(value: Option<&Value>) -> Result<String, String> {
    match value.and_then(Value::as_str) {
        Some(text) if text.len() == 64 && text.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) => {
            Ok(text.to_string())
        }
        _ => Err("version must be a lowercase sha256 hex digest".into()),
    }
}

fn empty_document() -> Value {
    json!({"format": 1, "generation": 0, "catalog": [], "changes": [], "decisions": [], "loads": [], "invocations": []})
}

fn read_document(saved: Option<Value>) -> Result<Value, String> {
    let Some(document) = read_versioned("ultron.module/skills", saved, "format")? else {
        return Ok(empty_document());
    };
    let valid = document["format"] == 1
        && document.get("generation").and_then(safe_integer).is_some()
        && ["catalog", "changes", "decisions", "loads", "invocations"]
            .iter()
            .all(|key| document[key].is_array());
    if !valid {
        return Err("Skill module document is invalid".into());
    }
    Ok(document)
}

fn append(document: &mut Value, key: &str, record: Value) {
    if document.get(key).is_none() {
        document[key] = json!([]);
    }
    if let Some(Value::Array(records)) = document.get_mut(key) {
        records.push(record);
        if records.len() > MAX_RECORDS {
            let excess = records.len() - MAX_RECORDS;
            records.drain(..excess);
        }
    }
}

struct Snapshot {
    generation: i64,
    skills: Vec<Skill>,
}

impl Snapshot {
    fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|skill| skill.name == name)
    }
}

/// Serves `skills.*`. Durable mutations and snapshot installation run one at a time.
pub struct SkillsModule {
    store: Arc<dyn DocumentStore>,
    load_skills: SkillLoader,
    now: Clock,
    code: Option<CodeSkills>,
    snapshot: Mutex<Option<Arc<Snapshot>>>,
    exclusive: Mutex<()>,
}

impl SkillsModule {
    pub fn new(store: Arc<dyn DocumentStore>, load_skills: SkillLoader, now: Clock, code: Option<CodeSkills>) -> Self {
        Self {
            store,
            load_skills,
            now,
            code,
            snapshot: Mutex::new(None),
            exclusive: Mutex::new(()),
        }
    }

    pub fn code(&self) -> Option<&CodeSkills> {
        self.code.as_ref()
    }

    fn mutate<T>(&self, change: impl FnOnce(&mut Value) -> T) -> Result<T, String> {
        let mut document = read_document(self.store.read()?)?;
        let result = change(&mut document);
        self.store.write(&document)?;
        Ok(result)
    }

    /// Caller holds `exclusive`.
    fn refresh_locked(&self) -> Result<Value, String> {
        let sources = (self.load_skills)()?;
        let mut seen = HashSet::new();
        for source in &sources {
            if source.name.is_empty() {
                return Err("loadSkills returned an invalid skill".into());
            }
            if !seen.insert(source.name.clone()) {
                return Err(format!("Duplicate skill name: {}", source.name));
            }
        }
        let skills: Vec<Skill> = sources.iter().map(parse_skill).collect();
        let at = (self.now)();
        let outcome = self.mutate(|document| {
            let previous: Vec<(String, String)> = document["catalog"]
                .as_array()
                .map(|entries| {
                    entries
                        .iter()
                        .map(|entry| {
                            let text = |key: &str| entry[key].as_str().unwrap_or_default().to_string();
                            (text("name"), text("version"))
                        })
                        .collect()
                })
                .unwrap_or_default();
            let generation = document["generation"].as_i64().unwrap_or(0) + 1;
            let lookup = |list: &[(String, String)], name: &str| {
                list.iter()
                    .find(|(key, _)| key == name)
                    .map(|(_, version)| version.clone())
            };
            let next: Vec<(String, String)> = skills.iter().map(|s| (s.name.clone(), s.version.clone())).collect();
            let mut names: Vec<String> = previous
                .iter()
                .chain(next.iter())
                .map(|(name, _)| name.clone())
                .collect();
            names.sort();
            names.dedup();
            let mut changes = Vec::new();
            for name in names {
                let (from, to) = (lookup(&previous, &name), lookup(&next, &name));
                if from != to {
                    changes.push(json!({"at": at, "generation": generation, "name": name, "from": from, "to": to}));
                }
            }
            document["generation"] = generation.into();
            document["catalog"] = skills
                .iter()
                .map(|skill| {
                    json!({"name": skill.name, "description": skill.description, "file_path": skill.file_path,
                        "version": skill.version, "invocable": skill.invocable})
                })
                .collect();
            for change in &changes {
                append(document, "changes", change.clone());
            }
            json!({"generation": generation, "changes": changes})
        })?;
        // Install only after the catalog record is durable.
        *lock(&self.snapshot) = Some(Arc::new(Snapshot {
            generation: outcome["generation"].as_i64().unwrap_or(0),
            skills,
        }));
        Ok(outcome)
    }

    /// One consistent catalog for the whole request, loaded on first use.
    fn current(&self) -> Result<Arc<Snapshot>, String> {
        if let Some(snapshot) = lock(&self.snapshot).clone() {
            return Ok(snapshot);
        }
        let _exclusive = lock(&self.exclusive);
        if lock(&self.snapshot).is_none() {
            self.refresh_locked()?;
        }
        Ok(lock(&self.snapshot).clone().expect("installed by refresh"))
    }

    fn pinned<'a>(snapshot: &'a Snapshot, name: &str, version: Option<&str>) -> Result<&'a Skill, String> {
        let Some(skill) = snapshot.get(name) else {
            return Err(match version {
                Some(version) => {
                    format!("Stale skill version: {name}@{version} is no longer in the catalog (skill removed)")
                }
                None => format!("Unknown skill: {name}"),
            });
        };
        if let Some(version) = version
            && skill.version != version
        {
            return Err(format!(
                "Stale skill version: {name}@{version} was requested but the catalog has {name}@{}; select or load again",
                skill.version
            ));
        }
        Ok(skill)
    }

    /// Module start: loads the catalog.
    pub fn start(&self) -> Result<(), String> {
        self.current().map(drop)
    }

    pub fn handle(
        &self,
        kind: &str,
        payload: &Map<String, Value>,
        caller: Option<&str>,
        signal: &Signal,
    ) -> Result<Value, String> {
        match kind {
            "skills.refresh" => {
                fields(payload, &[])?;
                let _exclusive = lock(&self.exclusive);
                self.refresh_locked()
            }
            "skills.list" => {
                fields(payload, &[])?;
                let snapshot = self.current()?;
                let skills: Vec<Value> = snapshot
                    .skills
                    .iter()
                    .map(|s| json!({"name": s.name, "description": s.description, "version": s.version, "invocable": s.invocable}))
                    .collect();
                Ok(json!({"generation": snapshot.generation, "skills": skills}))
            }
            "skills.select" => self.select(payload, caller),
            "skills.load" => self.load(payload, caller),
            "skills.why" => {
                fields(payload, &["decision_id"])?;
                let id = nonempty_string(payload.get("decision_id"), "decision_id")?;
                let document = read_document(self.store.read()?)?;
                document["decisions"]
                    .as_array()
                    .and_then(|decisions| decisions.iter().find(|decision| decision["id"] == id.as_str()))
                    .cloned()
                    .ok_or_else(|| format!("Unknown skill decision: {id}"))
            }
            "skills.invoke" => {
                fields(payload, &["name", "version", "input"])?;
                let name = nonempty_string(payload.get("name"), "name")?;
                let version = version_string(payload.get("version"))?;
                if !payload.contains_key("input") {
                    return Err("input must be JSON".into());
                }
                let snapshot = self.current()?;
                let skill = Self::pinned(&snapshot, &name, Some(&version))?;
                if !skill.invocable {
                    return Err(format!("Skill {name} disables model invocation"));
                }
                Err("skills.invoke needs task spawning, which this host does not provide yet".into())
            }
            "skills.propose_code" | "skills.propose_policy" | "skills.rollback" => {
                self.change_code(kind, payload, caller, signal)
            }
            "skills.code_list" => {
                fields(payload, &["kind", "family"])?;
                let filter_kind = match payload.get("kind") {
                    None => None,
                    Some(Value::String(kind)) if kind == "code" || kind == "policy" => Some(kind.as_str()),
                    Some(_) => return Err("kind must be \"code\" or \"policy\"".into()),
                };
                let family = match payload.get("family") {
                    None => None,
                    value => Some(nonempty_string(value, "family")?),
                };
                Ok(Value::Array(self.code_skills()?.list(filter_kind, family.as_deref())))
            }
            "skills.code_history" => {
                fields(payload, &["name"])?;
                self.code_skills()?.history(payload.get("name"))
            }
            _ => Err(format!("Unsupported skills request: {kind}")),
        }
    }

    fn code_skills(&self) -> Result<&CodeSkills, String> {
        self.code
            .as_ref()
            .ok_or_else(|| "Code skills are not configured in this worker".to_string())
    }

    /// Proposals and rollbacks, recorded in this module's journal whatever their outcome.
    fn change_code(
        &self,
        kind: &str,
        payload: &Map<String, Value>,
        caller: Option<&str>,
        signal: &Signal,
    ) -> Result<Value, String> {
        let code = self.code_skills()?;
        let action = &kind["skills.".len()..];
        let name = payload
            .get("name")
            .and_then(Value::as_str)
            .map(|name| utf16_prefix(name, 64).to_string())
            .unwrap_or_default();
        let base = vec![
            ("id", Value::from(uuid())),
            ("at", Value::from((self.now)())),
            ("action", Value::from(action)),
            ("name", Value::from(name)),
            ("caller_task_id", caller.into()),
        ];
        let outcome = if kind == "skills.rollback" {
            code.rollback(payload).map(|result| {
                let mut record = base.clone();
                record.extend([
                    ("version", result["rolledBack"].clone()),
                    ("status", "rolled_back".into()),
                    ("active", result["active"].clone()),
                ]);
                (object(record), result)
            })
        } else {
            let kind = if kind == "skills.propose_policy" {
                CodeSkillKind::Policy
            } else {
                CodeSkillKind::Code
            };
            code.propose(kind, payload, signal).map(|result| {
                let mut record = base.clone();
                record.extend([
                    ("version", result["version"].clone()),
                    ("status", result["status"].clone()),
                    ("secretCheck", result["secretCheck"].clone()),
                ]);
                if let Some(test) = result["test"].as_object() {
                    let tests = test.get("tests").and_then(Value::as_array).cloned().unwrap_or_default();
                    let failed: Vec<Value> = tests
                        .iter()
                        .filter(|item| item["ok"] != true)
                        .map(|item| item["name"].clone())
                        .collect();
                    let mut summary = vec![
                        ("passed", test.get("passed").cloned().unwrap_or(Value::Null)),
                        ("tests", tests.len().into()),
                        ("failed", failed.into()),
                    ];
                    if let Some(error) = test.get("error").and_then(Value::as_str).filter(|e| !e.is_empty()) {
                        summary.push(("error", utf16_prefix(error, 500).into()));
                    }
                    record.push(("test", object(summary)));
                }
                (object(record), result)
            })
        };
        let (record, result) = match outcome {
            Ok(done) => (done.0, Ok(done.1)),
            Err(error) => {
                let mut record = base;
                record.extend([
                    ("version", Value::Null),
                    ("status", "failed".into()),
                    ("error", utf16_prefix(&error, 500).into()),
                ]);
                (object(record), Err(error))
            }
        };
        {
            let _exclusive = lock(&self.exclusive);
            self.mutate(|document| append(document, "code", record))?;
        }
        result
    }

    fn select(&self, payload: &Map<String, Value>, caller: Option<&str>) -> Result<Value, String> {
        fields(payload, &["query", "limit"])?;
        let query = nonempty_string(payload.get("query"), "query")?;
        if utf16_len(&query) > MAX_QUERY_CHARS {
            return Err(format!("query must be at most {MAX_QUERY_CHARS} characters"));
        }
        let limit = match payload.get("limit") {
            None => 5,
            Some(value) => safe_integer(value)
                .filter(|limit| (1..=50).contains(limit) && value.is_number())
                .ok_or("limit must be an integer between 1 and 50")?,
        };
        let snapshot = self.current()?;
        let terms = query_terms(&query);
        let mut considered = Map::new();
        let mut excluded = Vec::new();
        let mut scored = Vec::new();
        let mut ordered: Vec<&Skill> = snapshot.skills.iter().collect();
        ordered.sort_by(|a, b| locale_compare(&a.name, &b.name));
        for skill in ordered {
            considered.insert(skill.name.clone(), skill.version.clone().into());
            if !skill.invocable {
                excluded.push(json!({"name": skill.name, "version": skill.version, "reason": "not_invocable"}));
                continue;
            }
            if let Some(candidate) = score(skill, &terms, &query) {
                scored.push(candidate);
            }
        }
        scored.sort_by(|a, b| {
            b["score"]
                .as_i64()
                .cmp(&a["score"].as_i64())
                .then_with(|| a["name"].as_str().cmp(&b["name"].as_str()))
        });
        scored.truncate(limit as usize);
        let chosen: Vec<Value> = scored
            .iter()
            .map(|candidate| json!({"name": candidate["name"], "version": candidate["version"]}))
            .collect();
        let decision = json!({
            "id": uuid(),
            "at": (self.now)(),
            "query": query,
            "terms": terms,
            "caller_task_id": caller,
            "catalog_generation": snapshot.generation,
            "considered": considered,
            "excluded": excluded,
            "candidates": scored,
            "chosen": chosen,
        });
        let _exclusive = lock(&self.exclusive);
        self.mutate(|document| append(document, "decisions", decision.clone()))?;
        Ok(decision)
    }

    fn load(&self, payload: &Map<String, Value>, caller: Option<&str>) -> Result<Value, String> {
        fields(payload, &["name", "version"])?;
        let name = nonempty_string(payload.get("name"), "name")?;
        let version = match payload.get("version") {
            None => None,
            value => Some(version_string(value)?),
        };
        let snapshot = self.current()?;
        let skill = Self::pinned(&snapshot, &name, version.as_deref())?;
        let id = uuid();
        let context_chars = utf16_len(&skill.content);
        let record = json!({"id": id, "at": (self.now)(), "name": name, "version": skill.version,
            "caller_task_id": caller, "context_chars": context_chars});
        {
            let _exclusive = lock(&self.exclusive);
            self.mutate(|document| append(document, "loads", record))?;
        }
        Ok(json!({
            "load_id": id,
            "name": name,
            "version": skill.version,
            "description": skill.description,
            "invocable": skill.invocable,
            "content": skill.content,
            "context_chars": context_chars,
            "ignored_capability_requests": skill.ignored,
        }))
    }
}

impl crate::ApiModule for SkillsModule {
    fn prefixes(&self) -> &[&str] {
        &["skills."]
    }

    fn start(&self, _host: &dyn crate::TaskHost) -> Result<(), String> {
        SkillsModule::start(self)
    }

    fn handle(&self, request: &crate::Request<'_>) -> Result<Value, String> {
        SkillsModule::handle(self, request.kind, request.payload, request.caller_task, request.signal)
    }
}
