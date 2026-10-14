//! Shared by the live, metered acceptance tests (A39, A46): the opt-in switch, an isolated install that holds only a
//! copy of the profile's `models.json`, the `ultron` binary in RPC mode against a real provider, the session records
//! read back through the product's own stores, and result scrubbing. Nothing here runs unless `ULTRON_LIVE_EVAL=1`.
#![allow(dead_code)]

use cli::rpc::client::{RpcClient, RpcClientOptions};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// `ULTRON_LIVE_EVAL=1`: the only switch that lets a live test spend provider calls.
pub fn live() -> bool {
    std::env::var("ULTRON_LIVE_EVAL").as_deref() == Ok("1")
}

/// `ULTRON_LIVE_EVAL_MODELS` (comma-separated `provider/model`), or `default`.
pub fn models(default: &str) -> Vec<String> {
    std::env::var("ULTRON_LIVE_EVAL_MODELS")
        .unwrap_or_else(|_| default.to_string())
        .split(',')
        .map(|model| model.trim().to_string())
        .collect()
}

/// `ULTRON_LIVE_EVAL_PROFILE`, default `~/.ultron/agent`.
pub fn profile() -> PathBuf {
    std::env::var("ULTRON_LIVE_EVAL_PROFILE").map_or_else(
        |_| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".ultron/agent"),
        PathBuf::from,
    )
}

/// The repository's `acceptance/<kind>` directory, where results are recorded.
pub fn results_dir(kind: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../acceptance")
        .join(kind)
}

/// `model.replace(/[^a-z0-9.-]+/gi, "_")`.
pub fn file_stem(model: &str) -> String {
    let mut out = String::new();
    let mut in_run = false;
    for c in model.chars() {
        if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
            out.push(c);
            in_run = false;
        } else if !in_run {
            out.push('_');
            in_run = true;
        }
    }
    out
}

/// `new Date().toISOString()`.
pub fn now_iso() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64);
    kernel_apis::util::iso(ms)
}

/// `(provider, model)` of `provider/model`.
pub fn split_model(model: &str) -> (String, String) {
    let (provider, id) = model.split_once('/').expect("a provider/model reference");
    (provider.to_string(), id.to_string())
}

/// One isolated run: private agent (holding only the profile's `models.json`), project, sessions and server
/// directories under one temp root that is removed on drop.
pub struct LiveInstall {
    pub root: tempfile::TempDir,
    /// Strings that must never reach a recorded result: the copied provider keys.
    secrets: Vec<String>,
}

impl LiveInstall {
    pub fn new(prefix: &str) -> LiveInstall {
        let root = tempfile::Builder::new().prefix(prefix).tempdir().unwrap();
        let mut install = LiveInstall {
            root,
            secrets: Vec::new(),
        };
        for dir in [install.agent(), install.project(), install.sessions()] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let source = profile().join("models.json");
        if source.exists() {
            std::fs::copy(&source, install.agent().join("models.json")).unwrap();
            let models: Value = serde_json::from_str(&std::fs::read_to_string(&source).unwrap()).unwrap_or(Value::Null);
            install.secrets = keys(&models);
        }
        install
    }

    pub fn agent(&self) -> PathBuf {
        self.root.path().join("agent")
    }

    pub fn project(&self) -> PathBuf {
        self.root.path().join("project")
    }

    pub fn sessions(&self) -> PathBuf {
        self.root.path().join("sessions")
    }

    pub fn server(&self) -> PathBuf {
        self.root.path().join("server")
    }

    /// `ultron --mode rpc --provider <p> --model <m> --session-dir <private> <args>` in the project directory, from
    /// a minimal environment: private agent and server directories, Hindsight off, the temp root as HOME, and a
    /// PATH without any directory that holds a `claude` executable.
    pub fn rpc(&self, model: &str, args: &[&str]) -> RpcClient {
        let (provider, id) = split_model(model);
        let mut env: Vec<(String, String)> = vec![
            ("PATH".into(), path_without_claude()),
            ("HOME".into(), self.root.path().display().to_string()),
            ("ULTRON_CODING_AGENT_DIR".into(), self.agent().display().to_string()),
            ("ULTRON_SERVER_DIR".into(), self.server().display().to_string()),
            ("ULTRON_HINDSIGHT_URL".into(), "off".into()),
            ("NO_COLOR".into(), "1".into()),
        ];
        if let Ok(python) = std::env::var("ULTRON_PYTHON") {
            env.push(("ULTRON_PYTHON".into(), python));
        }
        let mut all = vec!["--session-dir".to_string(), self.sessions().display().to_string()];
        all.extend(args.iter().map(|a| a.to_string()));
        let options = RpcClientOptions {
            program: env!("CARGO_BIN_EXE_ultron-acceptance").into(),
            cwd: Some(self.project()),
            env_clear: true,
            env,
            provider: Some(provider),
            model: Some(id),
            args: all,
            ..Default::default()
        };
        RpcClient::start(&options).expect("start ultron --mode rpc")
    }

    /// The session's stored value at `namespace`/`key`, read from the session file of this project through the
    /// journal's own reader (the process has exited). One root session must exist.
    pub fn session_value(&self, namespace: &str, key: &str) -> Result<Option<Value>, String> {
        let repo = session_journal::SessionRepo::new(&self.sessions(), session_journal::SystemClock::shared());
        let roots: Vec<_> = repo
            .list(Some(&self.project().display().to_string()))
            .into_iter()
            .filter(|m| m.parent_session_id.is_none())
            .collect();
        let [metadata] = roots.as_slice() else {
            return Err(format!("expected one session file, found {}", roots.len()));
        };
        let session = repo.open(metadata)?;
        Ok(session.value(namespace, key).cloned())
    }

    /// The typed agents' task records (`agents.status` tasks), through the product's task journal.
    pub fn agent_tasks(&self) -> Result<Vec<Value>, String> {
        let stored = self.session_value("ultron.tasks", "agents")?;
        delegation::task_store::TaskJournal::new(Box::new(delegation::task_store::MemoryStore { value: stored })).list()
    }

    /// The session's module files (`<agent>/rlm-modules/<session id>/`).
    pub fn modules(&self, session_id: &str) -> PathBuf {
        self.agent().join("rlm-modules").join(session_id)
    }

    /// `text` with every copied key and the real home directory removed.
    pub fn scrub(&self, text: &str) -> String {
        scrub_with(text, &self.secrets)
    }
}

/// `text` with every key of the profile's models.json and the real home directory removed.
pub fn scrub(text: &str) -> String {
    let models: Value = std::fs::read_to_string(profile().join("models.json"))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(Value::Null);
    scrub_with(text, &keys(&models))
}

fn scrub_with(text: &str, secrets: &[String]) -> String {
    let mut out = text.to_string();
    for secret in secrets {
        out = out.replace(secret.as_str(), "[redacted]");
    }
    if let Ok(home) = std::env::var("HOME")
        && home.len() > 1
    {
        out = out.replace(home.as_str(), "~");
    }
    out
}

/// Every string under a key that names a credential.
fn keys(value: &Value) -> Vec<String> {
    let mut out = Vec::new();
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                let lower = key.to_ascii_lowercase();
                match value {
                    Value::String(text)
                        if text.len() >= 8
                            && (lower.contains("key") || lower.contains("token") || lower.contains("auth")) =>
                    {
                        out.push(text.clone())
                    }
                    _ => out.extend(keys(value)),
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|item| out.extend(keys(item))),
        _ => {}
    }
    out
}

fn path_without_claude() -> String {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let kept: Vec<PathBuf> = std::env::split_paths(&path)
        .filter(|dir| !dir.join("claude").exists())
        .collect();
    std::env::join_paths(kept).unwrap().to_string_lossy().into_owned()
}

/// Whether `c` is a regex word character.
fn word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// `/\b<needle>\b/.test(text)` for a word `needle`.
pub fn has_word(text: &str, needle: &str) -> bool {
    text.match_indices(needle).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + needle.len()..].chars().next();
        !before.is_some_and(word) && !after.is_some_and(word)
    })
}

/// `answer.replace(/,/g, "").match(/-?\d+(?:\.\d+)?/g)?.at(-1)` as a number.
pub fn number_in(answer: &str) -> Option<f64> {
    let text: Vec<char> = answer.chars().filter(|c| *c != ',').collect();
    let mut last = None;
    let mut i = 0;
    while i < text.len() {
        let start = i;
        let negative = text[i] == '-' && text.get(i + 1).is_some_and(char::is_ascii_digit);
        if !(negative || text[i].is_ascii_digit()) {
            i += 1;
            continue;
        }
        if negative {
            i += 1;
        }
        while i < text.len() && text[i].is_ascii_digit() {
            i += 1;
        }
        if text.get(i) == Some(&'.') && text.get(i + 1).is_some_and(char::is_ascii_digit) {
            i += 1;
            while i < text.len() && text[i].is_ascii_digit() {
                i += 1;
            }
        }
        last = text[start..i].iter().collect::<String>().parse().ok();
    }
    last
}

/// `text.match(/\{[\s\S]*\}/)?.[0]`: from the first `{` to the last `}`.
pub fn braced(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    (end > start).then(|| &text[start..=end])
}
