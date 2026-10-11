//! Code skills: procedural memory as tested Python (port of `ultron/code-skills.ts`).
//!
//! A code skill is a Python module under `<dir>/<name>/`. Every proposal becomes a numbered version in
//! `.versions/<n>/` (skill.py, test_skill.py, SKILL.md); its test runs in a fresh kernel that refuses every host
//! request, and only a passing version becomes active. The active version is copied to the skill directory's top
//! level (Pi's skill loader lists its SKILL.md) and `skills_api.py` makes it importable as
//! `from code_skills import <name>` in every kernel, verified against the recorded sha256. States: proposed →
//! active | rejected; an activation supersedes the previous active version and remembers it; only the active
//! version can be rolled back, which restores the version it superseded (or leaves none active).
//! `.history.json` is written as `JSON.stringify(history, null, "\t")`, so either implementation reads the other's.

use crate::util::{
    Clock, fields, iso, js_trim, locale_compare, object, safe_integer, sha256_hex, stringify, stringify_tabs,
    system_clock, utf16_len, utf16_prefix, uuid,
};
use kernel::{HostHandler, Kernel, KernelOptions, Signal, Status};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

const MAX_SOURCE_BYTES: usize = 64 * 1024;
const MAX_EVIDENCE_BYTES: usize = 16 * 1024;
const MAX_FAMILY_CHARS: usize = 200;
pub const DEFAULT_TEST_TIMEOUT_MS: u64 = 120_000;
const MAX_DESCRIPTION_ENTRY_CHARS: usize = 240;
const MAX_DESCRIPTION_CHARS: usize = 3000;
const MAX_DESCRIPTION_ENTRIES: usize = 24;
const REPORT_MARKER: &str = "ULTRON_CODE_SKILL_REPORT";
const RUNNER: &str = include_str!("code_skill_runner.txt");
const PYTHON_KEYWORDS: &[&str] = &[
    "false", "none", "true", "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del",
    "elif", "else", "except", "finally", "for", "from", "global", "if", "import", "in", "is", "lambda", "nonlocal",
    "not", "or", "pass", "raise", "return", "try", "while", "with", "yield", "match", "case", "type",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeSkillKind {
    Code,
    Policy,
}

impl CodeSkillKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Code => "code",
            Self::Policy => "policy",
        }
    }
}

/// `validCodeSkillName`: a lowercase Python identifier of at most 64 characters that is not a keyword.
pub fn valid_code_skill_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_')
        && !PYTHON_KEYWORDS.contains(&name)
}

fn code_skill_name(value: Option<&Value>) -> Result<String, String> {
    match value.and_then(Value::as_str) {
        Some(name) if valid_code_skill_name(name) => Ok(name.to_string()),
        _ => Err("name must be a lowercase Python identifier (a-z, 0-9, _; at most 64 characters)".into()),
    }
}

fn source_text(value: Option<&Value>, label: &str) -> Result<String, String> {
    let text = match value.and_then(Value::as_str) {
        Some(text) if !js_trim(text).is_empty() => text,
        _ => return Err(format!("{label} must be nonempty Python source")),
    };
    if text.len() > MAX_SOURCE_BYTES {
        return Err(format!("{label} exceeds 64 KiB"));
    }
    Ok(text.to_string())
}

fn nonempty_json(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(text) => !js_trim(text).is_empty(),
        Value::Array(items) => items.iter().any(nonempty_json),
        Value::Object(map) => map.values().any(nonempty_json),
        _ => true,
    }
}

fn evidence_value(value: Option<&Value>) -> Result<Value, String> {
    // As for refinement activation, a skill needs nonempty evidence of the decomposition that worked.
    let value = value
        .filter(|value| nonempty_json(value))
        .ok_or("evidence must be nonempty JSON (what worked, and where)")?;
    if stringify(value).len() > MAX_EVIDENCE_BYTES {
        return Err("evidence exceeds 16 KiB".into());
    }
    Ok(value.clone())
}

fn atomic_write(path: &Path, content: &str) -> Result<(), String> {
    let temporary = PathBuf::from(format!("{}.{}.tmp", path.display(), uuid()));
    std::fs::write(&temporary, content).map_err(|error| error.to_string())?;
    std::fs::rename(&temporary, path).map_err(|error| error.to_string())
}

fn read_history(base: &Path, name: &str) -> Result<Value, String> {
    let path = base.join(".history.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(json!({"format": 1, "name": name, "active": null, "versions": []}));
        }
        Err(error) => return Err(error.to_string()),
    };
    let history: Value = serde_json::from_str(&text).map_err(|error| error.to_string())?;
    if history["format"] != 1 || history["name"] != name || !history["versions"].is_array() {
        return Err(format!("Code skill history for {name} is invalid"));
    }
    Ok(history)
}

fn versions(history: &mut Value) -> &mut Vec<Value> {
    match history.get_mut("versions") {
        Some(Value::Array(items)) => items,
        _ => unreachable!("read_history checks versions"),
    }
}

/// `oneLine`: the first nonblank line, trimmed, cut to `limit` UTF-16 units with an ellipsis.
fn one_line(text: Option<&str>, limit: usize) -> String {
    let line = text
        .unwrap_or("")
        .split('\n')
        .map(|item| item.strip_suffix('\r').unwrap_or(item))
        .find(|item| !js_trim(item).is_empty())
        .unwrap_or("");
    let trimmed = js_trim(line);
    if utf16_len(trimmed) > limit {
        format!("{}…", utf16_prefix(trimmed, limit - 1))
    } else {
        trimmed.to_string()
    }
}

fn nonempty_str(value: &Value) -> Option<&str> {
    value.as_str().filter(|text| !text.is_empty())
}

fn skill_markdown(name: &str, record: &Value) -> String {
    let doc = nonempty_str(&record["test"]["doc"]);
    let kind = record["kind"].as_str().unwrap_or("code");
    let mut description = one_line(doc, 300);
    if description.is_empty() {
        let label = if kind == "policy" { "policy" } else { "code" };
        description = format!("Python {label} skill {name} (tested).");
    }
    let mut lines = vec![
        "---".to_string(),
        format!("name: {}", name.replace('_', "-")),
        format!("description: {}", stringify(&Value::String(description))),
        "---".to_string(),
        format!("# {name} ({kind} skill, version {})", record["version"]),
        String::new(),
        format!("Tested Python. In the rlm tool: `from code_skills import {name}`."),
    ];
    if let Some(family) = nonempty_str(&record["family"]) {
        lines.extend([String::new(), format!("Task family: {family}")]);
    }
    if let Some(doc) = doc {
        lines.extend([String::new(), js_trim(doc).to_string()]);
    }
    let functions = record["test"]["functions"].as_array().cloned().unwrap_or_default();
    if !functions.is_empty() {
        lines.extend([String::new(), "## Functions".to_string(), String::new()]);
        for function in &functions {
            let signature = function["signature"].as_str().unwrap_or("");
            let doc = match nonempty_str(&function["doc"]) {
                Some(doc) => format!(": {}", one_line(Some(doc), 200)),
                None => String::new(),
            };
            lines.push(format!("- `{signature}`{doc}"));
        }
    }
    lines.push(String::new());
    lines.join("\n")
}

/// Python run in the fresh test kernel; it writes its report to a nonce file, never trusting stdout framing.
fn test_runner(name: &str, kind: CodeSkillKind, dir: &Path, report_path: &Path) -> String {
    let options = json!({
        "name": name,
        "kind": kind.as_str(),
        "dir": dir.to_string_lossy(),
        "reportPath": report_path.to_string_lossy(),
    });
    let literal = stringify(&Value::String(stringify(&options)));
    format!("\n{}", RUNNER.replace("{CONFIG}", &literal))
}

fn failed_report(error: &str, duration_ms: i64) -> Value {
    json!({
        "passed": false,
        "tests": [],
        "error": utf16_prefix(error, 2000),
        "durationMs": duration_ms,
        "doc": null,
        "functions": [],
    })
}

fn parse_report(text: &str) -> Result<Value, String> {
    let body = text.strip_prefix(REPORT_MARKER).ok_or("malformed test report")?;
    let value: Value = serde_json::from_str(body).map_err(|error| error.to_string())?;
    if !value["passed"].is_boolean() || !value["tests"].is_array() {
        return Err("malformed test report".into());
    }
    Ok(value)
}

/// A kernel that refuses every host request: a skill test gets no host capabilities.
struct Refuse;

impl HostHandler for Refuse {
    fn handle(&self, _kind: &str, _payload: Map<String, Value>, _signal: &Signal) -> Result<Value, String> {
        Err("Host requests are unavailable while a code skill test runs".into())
    }
}

/// Cross-process exclusion per skill, compatible with proper-lockfile: the lock is the `.lock` directory;
/// one older than 30 s is stale and taken over.
struct SkillLock {
    path: PathBuf,
}

impl SkillLock {
    fn acquire(base: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(base).map_err(|error| error.to_string())?;
        let path = base.join(".lock");
        let mut delay = 20u64;
        for attempt in 0..=100 {
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = std::fs::metadata(&path)
                        .and_then(|meta| meta.modified())
                        .ok()
                        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                        .is_some_and(|age| age > Duration::from_secs(30));
                    if stale {
                        let _ = std::fs::remove_dir(&path);
                        continue;
                    }
                    if attempt == 100 {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(delay));
                    delay = (delay * 2).min(500);
                }
                Err(error) => return Err(error.to_string()),
            }
        }
        Err("Lock file is already being held".into())
    }
}

impl Drop for SkillLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.path);
    }
}

pub struct CodeSkillsOptions {
    /// Where skills live (`ULTRON_CODE_SKILLS_DIR`, else `<agentDir>/skills`).
    pub dir: PathBuf,
    /// runtime.py for the test kernels.
    pub runtime_path: PathBuf,
    /// Interpreter for the test kernels; default as for every kernel.
    pub python: Option<PathBuf>,
    /// Wall budget of one test run (`ULTRON_CODE_SKILL_TEST_TIMEOUT_MS`, default 120 s).
    pub test_timeout_ms: u64,
    pub now: Clock,
}

impl CodeSkillsOptions {
    pub fn new(dir: impl Into<PathBuf>, runtime_path: impl Into<PathBuf>) -> Self {
        let configured = std::env::var("ULTRON_CODE_SKILL_TEST_TIMEOUT_MS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .filter(|value| *value > 0);
        Self {
            dir: dir.into(),
            runtime_path: runtime_path.into(),
            python: None,
            test_timeout_ms: configured.unwrap_or(DEFAULT_TEST_TIMEOUT_MS),
            now: system_clock(),
        }
    }
}

pub struct CodeSkills {
    options: CodeSkillsOptions,
}

impl CodeSkills {
    pub fn new(options: CodeSkillsOptions) -> Self {
        Self { options }
    }

    pub fn dir(&self) -> &Path {
        &self.options.dir
    }

    fn base(&self, name: &str) -> PathBuf {
        self.options.dir.join(name)
    }

    fn now(&self) -> i64 {
        (self.options.now)()
    }

    fn write_history(&self, history: &Value) -> Result<(), String> {
        let name = history["name"].as_str().unwrap_or_default();
        atomic_write(
            &self.base(name).join(".history.json"),
            &format!("{}\n", stringify_tabs(history)),
        )
    }

    /// Top-level copy of the active version for Pi's loader and people; removed when none is active.
    fn publish(&self, history: &Value) -> Result<(), String> {
        let base = self.base(history["name"].as_str().unwrap_or_default());
        let active = history["versions"]
            .as_array()
            .and_then(|items| items.iter().find(|item| item["version"] == history["active"]));
        for file in ["SKILL.md", "skill.py", "test_skill.py"] {
            let target = base.join(file);
            match active {
                Some(record) => {
                    let source = base.join(".versions").join(record["version"].to_string()).join(file);
                    let content = std::fs::read_to_string(source).map_err(|error| error.to_string())?;
                    atomic_write(&target, &content)?;
                }
                None => {
                    let _ = std::fs::remove_file(target);
                }
            }
        }
        Ok(())
    }

    fn secret_check(name: &str, source: &str, test_source: &str, evidence: &Value) -> Value {
        let evidence_text = match evidence {
            Value::String(text) => text.clone(),
            other => stringify(other),
        };
        if host::secrets::contains_code_secret(&format!("{name}\n{source}\n{test_source}\n{evidence_text}")) {
            json!({"status": "sensitive", "reason": "the proposal contains a secret or credential"})
        } else {
            json!({"status": "clean"})
        }
    }

    fn run_test(
        &self,
        name: &str,
        kind: CodeSkillKind,
        source: &str,
        test_source: &str,
        signal: &Signal,
    ) -> Result<Value, String> {
        let started = self.now();
        let work = tempfile::Builder::new()
            .prefix("ultron-skill-test-")
            .tempdir()
            .map_err(|error| error.to_string())?;
        let report_path = work.path().join(format!(".report-{}.json", uuid()));
        // A copy, so the test can never alter the version it is judging.
        let dir = work.path().join("candidate");
        std::fs::create_dir(&dir).map_err(|error| error.to_string())?;
        std::fs::write(dir.join("skill.py"), source).map_err(|error| error.to_string())?;
        std::fs::write(dir.join("test_skill.py"), test_source).map_err(|error| error.to_string())?;
        let mut options = KernelOptions::new(work.path(), &self.options.runtime_path);
        options.python = self.options.python.clone();
        options.env = vec![(
            "ULTRON_CODE_SKILLS_DIR".into(),
            self.options.dir.to_string_lossy().into_owned(),
        )];
        let timeout = Duration::from_millis(self.options.test_timeout_ms);
        let run = Signal::new();
        let done = Signal::new();
        let watcher = {
            let (run, done, parent) = (run.clone(), done.clone(), signal.clone());
            std::thread::spawn(move || {
                let deadline = Instant::now() + timeout;
                loop {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if done.wait(left.min(Duration::from_millis(50))) {
                        return;
                    }
                    if let Some(reason) = parent.reason() {
                        run.abort(reason);
                        return;
                    }
                    if Instant::now() >= deadline {
                        run.abort(format!("test timed out after {} ms", timeout.as_millis()));
                        return;
                    }
                }
            })
        };
        let outcome = Kernel::new(options, Arc::new(Refuse)).and_then(|kernel| {
            let result = kernel.execute(&test_runner(name, kind, &dir, &report_path), Some(&run), None);
            kernel.shutdown();
            result
        });
        done.abort("finished");
        let _ = watcher.join();
        let outcome = match outcome {
            Ok(result) if result.status == Status::Error => {
                let error = result.error.unwrap_or_else(|| kernel::CellError {
                    ename: "PythonError".into(),
                    evalue: "Execution failed".into(),
                    traceback: Vec::new(),
                });
                Err(format!("{}: {}", error.ename, error.evalue))
            }
            Ok(_) => std::fs::read_to_string(&report_path)
                .map_err(|error| error.to_string())
                .and_then(|text| parse_report(&text)),
            Err(error) => Err(error.message().to_string()),
        };
        match outcome {
            Ok(report) => Ok(report),
            Err(message) => {
                if let Some(reason) = signal.reason() {
                    return Err(reason);
                }
                let elapsed = self.now() - started;
                let timed_out = elapsed >= self.options.test_timeout_ms as i64;
                let message = if timed_out {
                    format!("test timed out after {} ms", self.options.test_timeout_ms)
                } else {
                    message
                };
                Ok(failed_report(&message, elapsed))
            }
        }
    }

    /// `skills.propose_code` / `skills.propose_policy`.
    pub fn propose(&self, kind: CodeSkillKind, payload: &Map<String, Value>, signal: &Signal) -> Result<Value, String> {
        let allowed: &[&str] = match kind {
            CodeSkillKind::Code => &["name", "source", "test_source", "evidence"],
            CodeSkillKind::Policy => &["name", "source", "test_source", "evidence", "family"],
        };
        fields(payload, allowed)?;
        let name = code_skill_name(payload.get("name"))?;
        let source = source_text(payload.get("source"), "source")?;
        let test_source = source_text(payload.get("test_source"), "test_source")?;
        let evidence = evidence_value(payload.get("evidence"))?;
        let family = match payload.get("family") {
            None => Value::Null,
            Some(Value::String(family)) if !js_trim(family).is_empty() && utf16_len(family) <= MAX_FAMILY_CHARS => {
                Value::String(js_trim(family).to_string())
            }
            Some(_) => {
                return Err(format!(
                    "family must be a nonempty string of at most {MAX_FAMILY_CHARS} characters"
                ));
            }
        };
        let base = self.base(&name);
        if base.join("SKILL.md").exists() && !base.join(".history.json").exists() {
            return Err(format!(
                "{name} is an instruction skill (SKILL.md without code history); choose another name"
            ));
        }
        let secret_check = Self::secret_check(&name, &source, &test_source, &evidence);
        if secret_check["status"] == "sensitive" {
            return Ok(object(vec![
                ("status", "refused".into()),
                ("name", name.into()),
                ("kind", kind.as_str().into()),
                ("version", Value::Null),
                ("previous", Value::Null),
                (
                    "reason",
                    format!("{}; nothing was written", secret_check["reason"].as_str().unwrap_or("")).into(),
                ),
                ("test", Value::Null),
                ("secretCheck", secret_check),
            ]));
        }
        let at = || iso(self.now());
        let cause = uuid();
        let version = {
            let _lock = SkillLock::acquire(&base)?;
            let mut history = read_history(&base, &name)?;
            let next = versions(&mut history)
                .iter()
                .filter_map(|item| item["version"].as_i64())
                .fold(0, i64::max)
                + 1;
            let directory = base.join(".versions").join(next.to_string());
            std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
            let record = object(vec![
                ("version", next.into()),
                ("kind", kind.as_str().into()),
                ("family", family.clone()),
                ("state", "proposed".into()),
                ("createdAt", at().into()),
                (
                    "sha256",
                    json!({"skill": sha256_hex(source.as_bytes()), "test": sha256_hex(test_source.as_bytes())}),
                ),
                ("evidence", evidence.clone()),
                ("previous", Value::Null),
                ("secretCheck", secret_check.clone()),
                ("test", Value::Null),
                ("history", json!([{"state": "proposed", "at": at(), "cause": cause}])),
            ]);
            let write = |file: &str, content: &str| {
                std::fs::write(directory.join(file), content).map_err(|error| error.to_string())
            };
            write("skill.py", &source)?;
            write("test_skill.py", &test_source)?;
            write("SKILL.md", &skill_markdown(&name, &record))?;
            versions(&mut history).push(record);
            self.write_history(&history)?;
            next
        };
        let report = self.run_test(&name, kind, &source, &test_source, signal)?;
        let _lock = SkillLock::acquire(&base)?;
        let mut history = read_history(&base, &name)?;
        let passed = report["passed"] == true;
        let active = history["active"].clone();
        let entries = versions(&mut history);
        let index = entries
            .iter()
            .position(|item| item["version"] == version)
            .ok_or_else(|| format!("Code skill {name} lost version {version}"))?;
        entries[index]["test"] = report.clone();
        // Regenerate SKILL.md now that the docstring is known.
        let directory = base.join(".versions").join(version.to_string());
        std::fs::write(directory.join("SKILL.md"), skill_markdown(&name, &entries[index]))
            .map_err(|error| error.to_string())?;
        let mut previous = Value::Null;
        let push = |record: &mut Value, state: &str| {
            record["state"] = state.into();
            if let Some(Value::Array(changes)) = record.get_mut("history") {
                changes.push(json!({"state": state, "at": at(), "cause": cause}));
            }
        };
        if passed {
            if let Some(current) = entries
                .iter_mut()
                .find(|item| item["version"] == active && item["state"] == "active")
            {
                push(current, "superseded");
                previous = current["version"].clone();
            }
            entries[index]["previous"] = previous.clone();
            push(&mut entries[index], "active");
            history["active"] = version.into();
        } else {
            push(&mut entries[index], "rejected");
        }
        self.write_history(&history)?;
        if passed {
            self.publish(&history)?;
        }
        let mut result = vec![
            ("status", Value::from(if passed { "active" } else { "rejected" })),
            ("name", name.clone().into()),
            ("kind", kind.as_str().into()),
            ("version", version.into()),
            ("previous", previous),
        ];
        if passed {
            result.push(("import", format!("from code_skills import {name}").into()));
        }
        result.push(("test", report));
        result.push(("secretCheck", secret_check));
        Ok(object(result))
    }

    /// `skills.rollback`: only the active version can be rolled back; the version it superseded returns.
    pub fn rollback(&self, payload: &Map<String, Value>) -> Result<Value, String> {
        fields(payload, &["name", "version"])?;
        let name = code_skill_name(payload.get("name"))?;
        let version = payload
            .get("version")
            .and_then(safe_integer)
            .filter(|version| *version >= 1)
            .ok_or("version must be a positive integer")?;
        let base = self.base(&name);
        if !base.join(".history.json").exists() {
            return Err(format!("Unknown code skill: {name}"));
        }
        let _lock = SkillLock::acquire(&base)?;
        let mut history = read_history(&base, &name)?;
        let active = history["active"].clone();
        let at = iso(self.now());
        let cause = uuid();
        let entries = versions(&mut history);
        let index = entries
            .iter()
            .position(|item| item["version"] == version)
            .ok_or_else(|| format!("Unknown version {version} of code skill {name}"))?;
        if active != version || entries[index]["state"] != "active" {
            let has = if active.is_null() {
                "no active version".to_string()
            } else {
                format!("version {active} active")
            };
            return Err(format!("Only the active version can be rolled back: {name} has {has}"));
        }
        let change = |record: &mut Value, state: &str| {
            record["state"] = state.into();
            if let Some(Value::Array(changes)) = record.get_mut("history") {
                changes.push(json!({"state": state, "at": at, "cause": cause}));
            }
        };
        change(&mut entries[index], "rolled_back");
        let superseded = entries[index]["previous"].clone();
        let restored = entries
            .iter_mut()
            .find(|item| item["version"] == superseded && item["state"] == "superseded")
            .map(|previous| {
                change(previous, "active");
                previous["version"].clone()
            })
            .unwrap_or(Value::Null);
        history["active"] = restored.clone();
        self.write_history(&history)?;
        self.publish(&history)?;
        Ok(json!({"name": name, "rolledBack": version, "active": restored}))
    }

    /// `skills.code_history`.
    pub fn history(&self, name: Option<&Value>) -> Result<Value, String> {
        let name = code_skill_name(name)?;
        let base = self.base(&name);
        if !base.join(".history.json").exists() {
            return Err(format!("Unknown code skill: {name}"));
        }
        read_history(&base, &name)
    }

    /// Active code skills, most recently activated first. Unreadable entries are skipped.
    pub fn list(&self, kind: Option<&str>, family: Option<&str>) -> Vec<Value> {
        let Ok(entries) = std::fs::read_dir(&self.options.dir) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
            .collect();
        names.sort();
        let mut skills = Vec::new();
        for name in names {
            let base = self.base(&name);
            if !valid_code_skill_name(&name) || !base.join(".history.json").exists() {
                continue;
            }
            let Ok(history) = read_history(&base, &name) else {
                continue;
            };
            let Some(record) = history["versions"].as_array().and_then(|items| {
                items
                    .iter()
                    .find(|item| item["version"] == history["active"] && item["state"] == "active")
            }) else {
                continue;
            };
            if kind.is_some_and(|kind| record["kind"] != kind)
                || family.is_some_and(|family| record["family"] != family)
            {
                continue;
            }
            let activated = record["history"]
                .as_array()
                .and_then(|changes| changes.iter().rev().find(|change| change["state"] == "active"))
                .map(|change| change["at"].clone())
                .unwrap_or_else(|| record["createdAt"].clone());
            skills.push(object(vec![
                ("name", name.into()),
                ("kind", record["kind"].clone()),
                ("family", record["family"].clone()),
                ("version", record["version"].clone()),
                ("doc", record["test"].get("doc").cloned().unwrap_or(Value::Null)),
                (
                    "functions",
                    record["test"].get("functions").cloned().unwrap_or(json!([])),
                ),
                ("activatedAt", activated),
            ]));
        }
        skills.sort_by(|a, b| {
            let text = |value: &Value, key: &str| value[key].as_str().unwrap_or("").to_string();
            text(b, "activatedAt")
                .cmp(&text(a, "activatedAt"))
                .then_with(|| locale_compare(&text(a, "name"), &text(b, "name")))
        });
        skills
    }

    /// The rlm tool description's listing: one line per active skill, bounded in entries and characters.
    pub fn describe(&self) -> String {
        let skills = self.list(None, None);
        if skills.is_empty() {
            return String::new();
        }
        let mut lines = Vec::new();
        let mut size = 0;
        for (index, skill) in skills.iter().enumerate() {
            let signatures: Vec<&str> = skill["functions"]
                .as_array()
                .map(|functions| functions.iter().filter_map(|f| f["signature"].as_str()).collect())
                .unwrap_or_default();
            let signatures = signatures.join("; ");
            let summary = one_line(nonempty_str(&skill["doc"]), 160);
            let policy = if skill["kind"] == "policy" {
                match nonempty_str(&skill["family"]) {
                    Some(family) => format!(" (policy, {family})"),
                    None => " (policy)".to_string(),
                }
            } else {
                String::new()
            };
            let gap = if !summary.is_empty() && !signatures.is_empty() {
                " "
            } else {
                ""
            };
            let listed = if signatures.is_empty() {
                String::new()
            } else {
                format!("[{signatures}]")
            };
            let mut line = format!(
                "- {} v{}{policy}: {summary}{gap}{listed}",
                skill["name"].as_str().unwrap_or(""),
                skill["version"]
            );
            if utf16_len(&line) > MAX_DESCRIPTION_ENTRY_CHARS {
                line = format!("{}…", utf16_prefix(&line, MAX_DESCRIPTION_ENTRY_CHARS - 1));
            }
            if index >= MAX_DESCRIPTION_ENTRIES || size + utf16_len(&line) > MAX_DESCRIPTION_CHARS {
                lines.push(format!("- … {} more: `await skills.code_list()`", skills.len() - index));
                break;
            }
            size += utf16_len(&line) + 1;
            lines.push(line);
        }
        let mut out = vec!["Active code skills (`from code_skills import <name>`):".to_string()];
        out.extend(lines);
        out.join("\n")
    }
}
