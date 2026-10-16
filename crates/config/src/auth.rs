//! `auth.json` (`auth-storage.ts`): a JSON object of provider id to credential, `api_key` or `oauth`, locked with
//! `proper-lockfile`'s `auth.json.lock` directory and written with mode 0600. No network refresh happens here.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Map, Value};

use crate::json::{parse, stringify_pretty};
use crate::lock::{acquire, lock_path_for};

pub type AuthData = Map<String, Value>;

/// The async TypeScript lock (`withLockAsync`): stale after 30 s, retried for up to 30 s.
const AUTH_STALE: Duration = Duration::from_secs(30);
const AUTH_ATTEMPTS: u32 = 1500;

fn write_private(path: &Path, text: &str) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(text.as_bytes())
}

/// `FileAuthStorageBackend.withLockAsync`: create the parent (0700) and an empty `{}` file (0600) if missing, then
/// run `f` on the file's text under the lock; `Some(next)` is written before the lock is released.
pub(crate) fn with_locked_file<T>(
    path: &Path,
    f: impl FnOnce(&str) -> Result<(T, Option<String>), String>,
) -> Result<T, String> {
    if let Some(dir) = path.parent().filter(|d| !d.exists()) {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| e.to_string())?;
    }
    if !path.exists() {
        write_private(path, "{}").map_err(|e| e.to_string())?;
    }
    let _lock = acquire(&lock_path_for(path), AUTH_STALE, AUTH_ATTEMPTS).map_err(|e| e.to_string())?;
    let current = fs::read_to_string(path).map_err(|e| e.to_string())?;
    let (result, next) = f(&current)?;
    if let Some(next) = next {
        write_private(path, &next).map_err(|e| e.to_string())?;
    }
    Ok(result)
}

pub(crate) fn parse_data(content: &str) -> Result<AuthData, String> {
    if content.is_empty() {
        return Ok(AuthData::new());
    }
    match parse(content).map_err(|e| e.to_string())? {
        Value::Object(map) => Ok(map),
        other => Err(format!("Invalid auth.json: expected an object, found {other}")),
    }
}

/// Read-write credential store over one `auth.json`.
pub struct AuthStorage {
    path: PathBuf,
    data: AuthData,
}

impl AuthStorage {
    pub fn create(path: impl Into<PathBuf>) -> Self {
        let mut storage = AuthStorage {
            path: path.into(),
            data: AuthData::new(),
        };
        let _ = storage.reload();
        storage
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Re-read the file; a malformed file leaves the previous data in place.
    pub fn reload(&mut self) -> Result<(), String> {
        let data = with_locked_file(&self.path, |current| Ok((parse_data(current)?, None)))?;
        self.data = data;
        Ok(())
    }

    /// The stored credential, with an `api_key` template (`$VAR`, `${VAR}`) resolved against its `env` and the
    /// process environment.
    pub fn read(&mut self, provider: &str) -> Result<Option<Value>, String> {
        self.reload()?;
        let Some(credential) = self.data.get(provider).cloned() else {
            return Ok(None);
        };
        resolve_stored_api_key(credential, &|name| std::env::var(name).ok())
    }

    /// Provider ids and credential types, in file order.
    pub fn list(&mut self) -> Result<Vec<(String, Value)>, String> {
        self.reload()?;
        Ok(self
            .data
            .iter()
            .map(|(id, c)| (id.clone(), c.get("type").cloned().unwrap_or(Value::Null)))
            .collect())
    }

    /// Replace one provider's credential with `f(current)` under the lock, keeping every other entry as the file
    /// has it now; `None` leaves the file untouched and returns the current credential.
    pub fn modify(
        &mut self,
        provider: &str,
        f: impl FnOnce(Option<&Value>) -> Option<Value>,
    ) -> Result<Option<Value>, String> {
        let (result, data) = with_locked_file(&self.path, |content| {
            let mut data = parse_data(content)?;
            match f(data.get(provider)) {
                None => {
                    let current = data.get(provider).cloned();
                    Ok(((current, data), None))
                }
                Some(next) => {
                    data.insert(provider.to_string(), next.clone());
                    let text = stringify_pretty(&Value::Object(data.clone()));
                    Ok(((Some(next), data), Some(text)))
                }
            }
        })?;
        self.data = data;
        Ok(result)
    }

    pub fn delete(&mut self, provider: &str) -> Result<(), String> {
        let data = with_locked_file(&self.path, |content| {
            let mut data = parse_data(content)?;
            data.shift_remove(provider);
            let text = stringify_pretty(&Value::Object(data.clone()));
            Ok((data, Some(text)))
        })?;
        self.data = data;
        Ok(())
    }
}

/// `AuthStorage.read`'s key resolution: an `api_key` credential's `key` template (`$VAR`, `${VAR}`) resolved
/// against its own `env`, then `process_env`, or its `!command` run (auth-storage.ts:446); a key naming an unset
/// variable, or whose command fails, is removed. Other credentials as given.
pub fn resolve_stored_api_key(
    mut credential: Value,
    process_env: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<Value>, String> {
    if credential.get("type").and_then(Value::as_str) != Some("api_key") {
        return Ok(Some(credential));
    }
    let Some(key) = credential.get("key").and_then(Value::as_str).map(str::to_string) else {
        return Ok(Some(credential));
    };
    let env = credential
        .get("env")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let lookup = |name: &str| {
        env.get(name)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
            .or_else(|| process_env(name))
    };
    let resolved = resolve_config_value(&key, &lookup)?;
    let object = credential.as_object_mut().expect("api_key credential is an object");
    match resolved {
        Some(value) => object.insert("key".into(), value.into()),
        None => object.shift_remove("key"),
    };
    Ok(Some(credential))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplatePart {
    Literal(String),
    Env(String),
}

fn env_name_len(s: &str) -> usize {
    let mut chars = s.char_indices();
    match chars.next() {
        Some((_, c)) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return 0,
    }
    chars
        .find(|(_, c)| !(c.is_ascii_alphanumeric() || *c == '_'))
        .map_or(s.len(), |(i, _)| i)
}

fn push_literal(parts: &mut Vec<TemplatePart>, value: &str) {
    if value.is_empty() {
        return;
    }
    if let Some(TemplatePart::Literal(previous)) = parts.last_mut() {
        previous.push_str(value);
    } else {
        parts.push(TemplatePart::Literal(value.to_string()));
    }
}

/// `parseConfigValueTemplate`: `$NAME`, `${NAME}`, `$$` and `$!` escapes; anything else stays literal.
pub fn parse_template(config: &str) -> Vec<TemplatePart> {
    let mut parts = Vec::new();
    let mut rest = config;
    while let Some(dollar) = rest.find('$') {
        push_literal(&mut parts, &rest[..dollar]);
        let after = &rest[dollar + 1..];
        if after.starts_with('$') || after.starts_with('!') {
            push_literal(&mut parts, &after[..1]);
            rest = &after[1..];
        } else if let Some(braced) = after.strip_prefix('{') {
            match braced.find('}') {
                None => {
                    push_literal(&mut parts, "$");
                    rest = after;
                }
                Some(end) => {
                    let name = &braced[..end];
                    if !name.is_empty() && env_name_len(name) == name.len() {
                        parts.push(TemplatePart::Env(name.to_string()));
                    } else {
                        push_literal(&mut parts, &rest[dollar..dollar + end + 3]);
                    }
                    rest = &braced[end + 1..];
                }
            }
        } else {
            let len = env_name_len(after);
            if len > 0 {
                parts.push(TemplatePart::Env(after[..len].to_string()));
            } else {
                push_literal(&mut parts, "$");
            }
            rest = &after[len..];
        }
    }
    push_literal(&mut parts, rest);
    parts
}

/// How long a `!command` value may run (`execSync`'s `timeout: 10000`).
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// `commandResultCache`: every `!command` runs once per process; failures are cached too.
static COMMAND_CACHE: std::sync::Mutex<Option<std::collections::HashMap<String, Option<String>>>> =
    std::sync::Mutex::new(None);

/// `executeWithDefaultShell`: `/bin/sh -c <command>` with stdin and stderr ignored; its trimmed stdout, or `None`
/// when it fails, times out, or prints nothing.
fn run_command(command: &str) -> Option<String> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = stdout.read_to_end(&mut out);
        let _ = tx.send(out);
    });
    let Ok(out) = rx.recv_timeout(COMMAND_TIMEOUT) else {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    };
    if !child.wait().ok()?.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out).trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// `executeCommand`: a `!command` value (the command without its `!`), cached for the process's lifetime.
pub fn command_value(command: &str) -> Option<String> {
    let key = format!("!{command}");
    if let Some(cached) = COMMAND_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .and_then(|cache| cache.get(&key))
    {
        return cached.clone();
    }
    let value = run_command(command);
    COMMAND_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get_or_insert_with(Default::default)
        .insert(key, value.clone());
    value
}

/// `clearConfigValueCache`: forget every cached `!command` result.
pub fn clear_command_cache() {
    *COMMAND_CACHE.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = None;
}

/// `resolveConfigValue`: a `!command`'s trimmed output (run once per process, failures included), or the template
/// with its variables filled; `None` when the command fails or prints nothing, or a referenced variable is unset or
/// empty.
pub fn resolve_config_value(config: &str, env: &dyn Fn(&str) -> Option<String>) -> Result<Option<String>, String> {
    if let Some(command) = config.strip_prefix('!') {
        return Ok(command_value(command));
    }
    let mut out = String::new();
    for part in parse_template(config) {
        match part {
            TemplatePart::Literal(s) => out.push_str(&s),
            TemplatePart::Env(name) => match env(&name).filter(|v| !v.is_empty()) {
                Some(v) => out.push_str(&v),
                None => return Ok(None),
            },
        }
    }
    Ok(Some(out))
}

/// `ReadOnlyAuthStorage`'s validation: every entry an `api_key` (string key, string env map) or a complete `oauth`
/// credential (string access/refresh, finite numeric expires).
pub fn validate_auth_file(content: &str) -> Result<AuthData, String> {
    let parsed = parse(content).map_err(|e| format!("Failed to read auth.json: {e}"))?;
    let Value::Object(data) = parsed else {
        return Err("Invalid auth.json: expected an object".into());
    };
    for (provider, credential) in &data {
        let valid = match credential {
            Value::Object(c) => match c.get("type").and_then(Value::as_str) {
                Some("api_key") => {
                    c.get("key").is_none_or(Value::is_string)
                        && c.get("env")
                            .is_none_or(|e| e.as_object().is_some_and(|m| m.values().all(Value::is_string)))
                }
                Some("oauth") => {
                    c.get("access").is_some_and(Value::is_string)
                        && c.get("refresh").is_some_and(Value::is_string)
                        && c.get("expires").and_then(Value::as_f64).is_some_and(f64::is_finite)
                }
                _ => false,
            },
            _ => false,
        };
        if !valid {
            return Err(format!("Invalid auth.json credential for provider \"{provider}\""));
        }
    }
    Ok(data)
}

/// `readStoredCredential`: the raw entry, or `None` when the file is missing or malformed.
pub fn read_stored_credential(path: &Path, provider: &str) -> Option<Value> {
    let content = fs::read_to_string(path).ok()?;
    parse(&content).ok()?.get(provider).cloned()
}
