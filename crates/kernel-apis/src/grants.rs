//! `grants.*` (port of `ultron/grants.ts`): scoped approvals bound to scope, revision, policy, action, owner and
//! expiry, re-evaluated on every check. Advisory unless the host enables enforcement (`enforced` in results).

use crate::store::{DocumentStore, read_versioned};
use crate::util::{Clock, fields, js_trim, lock, safe_integer, utf16_len, uuid};
use serde_json::{Map, Value, json};
use std::sync::{Arc, Mutex};

const MAX_TTL_MS: i64 = 30 * 24 * 60 * 60 * 1000;
const MAX_GRANTS: usize = 4096;
const DURABILITY_ERROR: &str = "Grant store durability is uncertain; reopen the owner";

/// A task and its parent, for owner/descendant checks.
#[derive(Debug, Clone)]
pub struct TaskLink {
    pub id: String,
    pub parent_id: Option<String>,
}

fn is_line_terminator(ch: char) -> bool {
    matches!(ch, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

fn valid_scope(value: &str) -> bool {
    // /^[a-z][a-z0-9_-]{0,31}:\S.{0,1023}$/ in UTF-16 units.
    let Some((kind, target)) = value.split_once(':') else {
        return false;
    };
    let head = kind.as_bytes();
    let first = target.chars().next();
    !head.is_empty()
        && head.len() <= 32
        && head[0].is_ascii_lowercase()
        && head
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_' || *b == b'-')
        && first.is_some_and(|ch| !ch.is_whitespace() && ch != '\u{feff}')
        && !target.chars().any(is_line_terminator)
        && utf16_len(target) <= 1024
}

fn charset(text: &str, min: usize, max: usize, allowed: impl Fn(u8) -> bool) -> bool {
    (min..=max).contains(&text.len()) && text.bytes().all(allowed)
}

fn valid_revision(value: &str) -> bool {
    charset(value, 1, 256, |b| b.is_ascii_alphanumeric() || b"._:/@+-".contains(&b))
}

fn valid_policy(value: &str) -> bool {
    let Some((id, version)) = value.split_once('@') else {
        return false;
    };
    id.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && charset(id, 1, 128, |b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b)
        })
        && charset(version, 1, 64, |b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

fn valid_action(value: &str) -> bool {
    value.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && charset(value, 1, 64, |b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || b"_.-".contains(&b)
        })
}

fn matching(value: Option<&Value>, name: &str, valid: fn(&str) -> bool, description: &str) -> Result<String, String> {
    match value.and_then(Value::as_str) {
        Some(text) if valid(text) => Ok(text.to_string()),
        _ => Err(format!("{name} must be {description}")),
    }
}

fn scope_field(value: Option<&Value>) -> Result<String, String> {
    matching(value, "scope", valid_scope, "kind:target, such as repo:/path")
}
fn revision_field(value: Option<&Value>) -> Result<String, String> {
    matching(
        value,
        "revision",
        valid_revision,
        "a commit, content hash, or other base identifier",
    )
}
fn policy_field(value: Option<&Value>) -> Result<String, String> {
    matching(value, "policy", valid_policy, "id@version")
}
fn action_field(value: Option<&Value>) -> Result<String, String> {
    matching(value, "action", valid_action, "a short lowercase identifier")
}

fn id_field(value: Option<&Value>) -> Result<String, String> {
    match value.and_then(Value::as_str) {
        Some(text) if !js_trim(text).is_empty() => Ok(text.to_string()),
        _ => Err("id must be a nonempty string".into()),
    }
}

/// True when `task` is `ancestor` or descends from it; a `None` ancestor is the root, which every task descends from.
pub fn descends_from(task: Option<&str>, ancestor: Option<&str>, tasks: &[TaskLink]) -> bool {
    let Some(ancestor) = ancestor else {
        return true;
    };
    let mut current = task.map(str::to_string);
    let mut seen = std::collections::HashSet::new();
    while let Some(id) = current {
        if id == ancestor {
            return true;
        }
        if !seen.insert(id.clone()) {
            return false;
        }
        current = tasks
            .iter()
            .find(|link| link.id == id)
            .and_then(|link| link.parent_id.clone());
    }
    false
}

fn valid_grant(grant: &Value) -> bool {
    let Some(map) = grant.as_object() else {
        return false;
    };
    let text = |key: &str| map.get(key).and_then(Value::as_str).is_some_and(|s| !s.is_empty());
    let text_or_null = |key: &str| {
        map.get(key)
            .is_some_and(|v| v.is_null() || v.as_str().is_some_and(|s| !s.is_empty()))
    };
    let number = |key: &str| map.get(key).is_some_and(Value::is_number);
    const KEYS: &[&str] = &[
        "id",
        "scope",
        "revision",
        "policy",
        "action",
        "owner_task_id",
        "issued_at",
        "expires_at",
        "revoked_at",
        "revoked_by",
    ];
    map.keys().all(|key| KEYS.contains(&key.as_str()))
        && ["id", "scope", "revision", "policy", "action"]
            .iter()
            .all(|key| text(key))
        && text_or_null("owner_task_id")
        && number("issued_at")
        && number("expires_at")
        && map.get("revoked_at").is_some_and(|v| v.is_null() || v.is_number())
        && text_or_null("revoked_by")
}

struct State {
    document: Value,
    loaded: bool,
    broken: bool,
}

pub struct GrantsModule {
    store: Arc<dyn DocumentStore>,
    now: Clock,
    enforced: bool,
    state: Mutex<State>,
}

impl GrantsModule {
    pub fn new(store: Arc<dyn DocumentStore>, now: Clock, enforce: bool) -> Self {
        Self {
            store,
            now,
            enforced: enforce,
            state: Mutex::new(State {
                document: json!({"version": 1, "grants": []}),
                loaded: false,
                broken: false,
            }),
        }
    }

    fn ready<'a>(&self, state: &'a mut State) -> Result<&'a mut State, String> {
        if state.broken {
            return Err(DURABILITY_ERROR.into());
        }
        if !state.loaded {
            if let Some(saved) = read_versioned("ultron.module/grants", self.store.read()?, "version")? {
                let valid = saved.as_object().is_some_and(|map| {
                    map.keys().all(|key| key == "version" || key == "grants")
                        && map.get("version") == Some(&json!(1))
                        && map
                            .get("grants")
                            .and_then(Value::as_array)
                            .is_some_and(|grants| grants.iter().all(valid_grant))
                });
                if !valid {
                    return Err("Invalid grant document".into());
                }
                state.document = saved;
            }
            state.loaded = true;
        }
        Ok(state)
    }

    fn write(&self, state: &mut State, next: Value) -> Result<(), String> {
        if self.store.write(&next).is_err() {
            state.broken = true;
            return Err(DURABILITY_ERROR.into());
        }
        state.document = next;
        Ok(())
    }

    pub fn start(&self) -> Result<(), String> {
        let mut state = lock(&self.state);
        self.ready(&mut state).map(drop)
    }

    fn grants(document: &Value) -> Vec<Value> {
        document["grants"].as_array().cloned().unwrap_or_default()
    }

    /// Serves one `grants.*` request from `caller` (a task id, `None` for the root) over the task tree `tasks`.
    pub fn handle(
        &self,
        kind: &str,
        payload: &Map<String, Value>,
        caller: Option<&str>,
        tasks: &[TaskLink],
    ) -> Result<Value, String> {
        match kind {
            "grants.issue" => {
                fields(payload, &["scope", "revision", "policy", "action", "ttl_ms"])?;
                let scope = scope_field(payload.get("scope"))?;
                let revision = revision_field(payload.get("revision"))?;
                let policy = policy_field(payload.get("policy"))?;
                let action = action_field(payload.get("action"))?;
                let ttl = payload
                    .get("ttl_ms")
                    .and_then(safe_integer)
                    .filter(|ttl| (1..=MAX_TTL_MS).contains(ttl))
                    .ok_or_else(|| format!("ttl_ms must be an integer from 1 to {MAX_TTL_MS}"))?;
                let mut guard = lock(&self.state);
                let state = self.ready(&mut guard)?;
                let mut grants = Self::grants(&state.document);
                if grants.len() >= MAX_GRANTS {
                    return Err(format!("Grant store is full ({MAX_GRANTS})"));
                }
                let issued = (self.now)();
                let grant = json!({
                    "id": format!("grant-{}", uuid()),
                    "scope": scope, "revision": revision, "policy": policy, "action": action,
                    "owner_task_id": caller, "issued_at": issued, "expires_at": issued + ttl,
                    "revoked_at": null, "revoked_by": null,
                });
                grants.push(grant.clone());
                let mut next = state.document.clone();
                next["grants"] = Value::Array(grants);
                self.write(state, next)?;
                Ok(grant)
            }
            "grants.revoke" => {
                fields(payload, &["id"])?;
                let id = id_field(payload.get("id"))?;
                let mut guard = lock(&self.state);
                let state = self.ready(&mut guard)?;
                let mut grants = Self::grants(&state.document);
                let grant = grants
                    .iter_mut()
                    .find(|grant| grant["id"] == id.as_str())
                    .ok_or("Unknown grant")?;
                // The owner or any ancestor of the owner may withdraw an approval.
                if !descends_from(grant["owner_task_id"].as_str(), caller, tasks) {
                    return Err("Only the grant owner or one of its ancestors may revoke it".into());
                }
                if !grant["revoked_at"].is_null() {
                    return Ok(grant.clone());
                }
                grant["revoked_at"] = (self.now)().into();
                grant["revoked_by"] = caller.unwrap_or("root").into();
                let revoked = grant.clone();
                let mut next = state.document.clone();
                next["grants"] = Value::Array(grants);
                self.write(state, next)?;
                Ok(revoked)
            }
            "grants.list" => {
                fields(payload, &[])?;
                let mut guard = lock(&self.state);
                let state = self.ready(&mut guard)?;
                // Grants the caller may use (owned by it or an ancestor) or may revoke (owned by a descendant).
                let visible: Vec<Value> = Self::grants(&state.document)
                    .into_iter()
                    .filter(|grant| {
                        let owner = grant["owner_task_id"].as_str();
                        descends_from(caller, owner, tasks) || descends_from(owner, caller, tasks)
                    })
                    .collect();
                Ok(json!({"grants": visible, "enforced": self.enforced}))
            }
            "grants.check" => {
                fields(payload, &["id", "scope", "revision", "policy", "action"])?;
                let id = id_field(payload.get("id"))?;
                let scope = scope_field(payload.get("scope"))?;
                let revision = revision_field(payload.get("revision"))?;
                let policy = policy_field(payload.get("policy"))?;
                let action = action_field(payload.get("action"))?;
                let mut guard = lock(&self.state);
                let state = self.ready(&mut guard)?;
                let now = (self.now)();
                let mut result = json!({"grant_id": id, "checked_at": now});
                let grants = Self::grants(&state.document);
                let Some(grant) = grants.iter().find(|grant| grant["id"] == id.as_str()) else {
                    result["allowed"] = false.into();
                    result["reasons"] = json!(["Unknown grant"]);
                    result["enforced"] = self.enforced.into();
                    return Ok(result);
                };
                let mut reasons = Vec::new();
                let text = |key: &str| grant[key].as_str().unwrap_or_default().to_string();
                if !grant["revoked_at"].is_null() {
                    reasons.push(format!("Grant was revoked at {}", grant["revoked_at"]));
                }
                if grant["expires_at"]
                    .as_f64()
                    .is_some_and(|expires| now as f64 >= expires)
                {
                    reasons.push(format!("Grant expired at {}", grant["expires_at"]));
                }
                if text("revision") != revision {
                    reasons.push(format!(
                        "Base changed: grant was approved against {}, request is for {revision}",
                        text("revision")
                    ));
                }
                if text("policy") != policy {
                    reasons.push(format!(
                        "Policy changed: grant was issued under {}, request is under {policy}",
                        text("policy")
                    ));
                }
                if text("scope") != scope {
                    reasons.push(format!(
                        "Scope differs: grant covers {}, request is for {scope}",
                        text("scope")
                    ));
                }
                if text("action") != action {
                    reasons.push(format!(
                        "Action differs: grant covers {}, request is for {action}",
                        text("action")
                    ));
                }
                let owner = grant["owner_task_id"].as_str();
                if !descends_from(caller, owner, tasks) {
                    reasons.push(format!(
                        "Wrong owner: grant belongs to {}; caller {} is neither the owner nor its descendant",
                        owner.unwrap_or("the root agent"),
                        caller.unwrap_or("root")
                    ));
                }
                result["allowed"] = reasons.is_empty().into();
                result["reasons"] = json!(reasons);
                result["enforced"] = self.enforced.into();
                Ok(result)
            }
            _ => Err(format!("Unknown grants request: {kind}")),
        }
    }
}

impl crate::ApiModule for GrantsModule {
    fn prefixes(&self) -> &[&str] {
        &["grants."]
    }

    fn start(&self, _host: &dyn crate::TaskHost) -> Result<(), String> {
        GrantsModule::start(self)
    }

    fn handle(&self, request: &crate::Request<'_>) -> Result<Value, String> {
        let tasks: Vec<TaskLink> = request
            .host
            .tasks()?
            .iter()
            .filter_map(|task| {
                Some(TaskLink {
                    id: task["id"].as_str()?.to_string(),
                    parent_id: task["parentId"].as_str().map(str::to_string),
                })
            })
            .collect();
        GrantsModule::handle(self, request.kind, request.payload, request.caller_task, &tasks)
    }
}
