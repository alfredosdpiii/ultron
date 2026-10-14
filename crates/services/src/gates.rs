//! `gates.*` (port of `ultron/release-gate.ts`): frozen release-gate definitions plus a recorded baseline-versus-
//! candidate comparison. A gate definition is immutable once defined and is the only source of which checks exist
//! and which are required; a run cannot introduce its own checks. Every comparison attempt, blocked or passed, is
//! recorded before it is returned, with the digest of the definition it was judged against. A required check that
//! regressed, or is not passed in the candidate, blocks; an optional regression is reported and does not block; a
//! run on another fixture blocks. This module only records and decides; it never prompts.
//!
//! Requests (`release_gate_api.py`): `gates.define` `{id, checks, fixture_hash}`, `gates.compare`
//! `{gate_id, baseline, candidate}`, `gates.history` `{gate_id}`, `gates.list` `{}`.

use kernel_apis::DocumentStore;
use kernel_apis::store::read_versioned;
use kernel_apis::util::{Clock, fields, lock, sha256_hex, stringify, utf16_len, uuid};
use serde_json::{Map, Value, json};
use std::sync::{Arc, Mutex};

const MAX_CHECKS: usize = 256;
const MAX_ATTEMPTS: usize = 4096;
const DURABILITY_ERROR: &str = "Release gate store durability is uncertain; reopen the owner";
const OUTCOMES: [&str; 3] = ["passed", "failed", "missing"];

fn js_space(ch: char) -> bool {
    (ch.is_whitespace() && ch != '\u{85}') || ch == '\u{feff}'
}

/// `/^[a-z][a-z0-9._-]{0,127}$/`.
fn valid_id(value: &str) -> bool {
    value.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
}

/// `/^[A-Za-z0-9][A-Za-z0-9 ._:/-]{0,127}$/`.
fn valid_check(value: &str) -> bool {
    value.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b" ._:/-".contains(&b))
}

/// `/^[a-f0-9]{64}$/`.
fn valid_hash(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `/^\S.{0,255}$/` in UTF-16 units: no leading space, no line terminator.
fn valid_variant(value: &str) -> bool {
    value.chars().next().is_some_and(|ch| !js_space(ch))
        && !value
            .chars()
            .any(|ch| matches!(ch, '\n' | '\r' | '\u{2028}' | '\u{2029}'))
        && utf16_len(value) <= 256
}

fn plain_object<'a>(value: Option<&'a Value>, name: &str) -> Result<&'a Map<String, Value>, String> {
    value
        .and_then(Value::as_object)
        .ok_or_else(|| format!("{name} must be a JSON object"))
}

fn gate_id_field(value: Option<&Value>, name: &str) -> Result<String, String> {
    match value.and_then(Value::as_str) {
        Some(id) if valid_id(id) => Ok(id.into()),
        _ => Err(format!("{name} must be a short lowercase identifier")),
    }
}

fn hash_field(value: Option<&Value>, name: &str) -> Result<String, String> {
    match value.and_then(Value::as_str) {
        Some(hash) if valid_hash(hash) => Ok(hash.into()),
        _ => Err(format!("{name} must be a lowercase SHA-256 hex string")),
    }
}

/// `checksField`: 1 to 256 uniquely named checks, at least one required, sorted by name.
fn checks_field(value: Option<&Value>) -> Result<Vec<(String, bool)>, String> {
    let list = value
        .and_then(Value::as_array)
        .filter(|list| !list.is_empty() && list.len() <= MAX_CHECKS)
        .ok_or_else(|| format!("checks must be a list of 1 to {MAX_CHECKS} entries"))?;
    let mut checks: Vec<(String, bool)> = Vec::new();
    for (index, item) in list.iter().enumerate() {
        let entry = plain_object(Some(item), &format!("checks[{index}]"))?;
        fields(entry, &["name", "required"])?;
        let name = entry
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| valid_check(name))
            .ok_or_else(|| format!("checks[{index}].name must be a check name of at most 128 characters"))?;
        let required = entry
            .get("required")
            .and_then(Value::as_bool)
            .ok_or_else(|| format!("checks[{index}].required must be a boolean"))?;
        if checks.iter().any(|(seen, _)| seen == name) {
            return Err(format!("Duplicate check name: {name}"));
        }
        checks.push((name.into(), required));
    }
    if !checks.iter().any(|(_, required)| *required) {
        return Err("A gate needs at least one required check; a gate with none could never block".into());
    }
    checks.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(checks)
}

/// SHA-256 of the canonical definition `{id, checks: [[name, required]], fixture_hash}`.
fn gate_digest(id: &str, checks: &[(String, bool)], fixture_hash: &str) -> String {
    let checks: Vec<Value> = checks.iter().map(|(name, required)| json!([name, required])).collect();
    sha256_hex(stringify(&json!({"id": id, "checks": checks, "fixture_hash": fixture_hash})).as_bytes())
}

fn gate_checks(gate: &Value) -> Vec<(String, bool)> {
    gate["checks"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|check| {
            (
                check["name"].as_str().unwrap_or_default().to_string(),
                check["required"].as_bool().unwrap_or(false),
            )
        })
        .collect()
}

/// `runField`: `{variant, fixture_hash, results}` with every gate check present (absent ones `missing`).
fn run_field(value: Option<&Value>, name: &str, gate: &Value) -> Result<Value, String> {
    let run = plain_object(value, name)?;
    if let Some(key) = run
        .keys()
        .find(|key| !["variant", "fixture_hash", "results"].contains(&key.as_str()))
    {
        return Err(format!(
            "Unknown field {name}.{key}: a run is {{variant, fixture_hash, results: {{check: \"passed\" | \"failed\" | \"missing\"}}}}"
        ));
    }
    let variant = run
        .get("variant")
        .and_then(Value::as_str)
        .filter(|variant| valid_variant(variant))
        .ok_or_else(|| format!("{name}.variant must be a nonempty string of at most 256 characters"))?;
    let fixture_hash = hash_field(run.get("fixture_hash"), &format!("{name}.fixture_hash"))?;
    let supplied = plain_object(run.get("results"), &format!("{name}.results"))?;
    let checks = gate_checks(gate);
    for (check, outcome) in supplied {
        // The gate definition is the protected checker input: a run cannot introduce its own checks.
        if !checks.iter().any(|(known, _)| known == check) {
            return Err(format!(
                "{name}.results names a check the gate does not define: {check}"
            ));
        }
        if !outcome.as_str().is_some_and(|o| OUTCOMES.contains(&o)) {
            return Err(format!("{name}.results.{check} must be passed, failed, or missing"));
        }
    }
    let results: Map<String, Value> = checks
        .iter()
        .map(|(check, _)| {
            (
                check.clone(),
                supplied.get(check).cloned().unwrap_or_else(|| json!("missing")),
            )
        })
        .collect();
    Ok(json!({"variant": variant, "fixture_hash": fixture_hash, "results": results}))
}

/// `judge`: the decision, its reasons, regressions and improvements.
fn judge(gate: &Value, baseline: &Value, candidate: &Value) -> (bool, Vec<String>, Vec<Value>, Vec<Value>) {
    let mut reasons = Vec::new();
    let mut blocked = false;
    let gate_hash = gate["fixture_hash"].as_str().unwrap_or_default();
    for (label, run) in [("baseline", baseline), ("candidate", candidate)] {
        let hash = run["fixture_hash"].as_str().unwrap_or_default();
        if hash != gate_hash {
            blocked = true;
            reasons.push(format!(
                "Fixture mismatch: {label} {} ran against {hash}, gate {} requires {gate_hash}",
                run["variant"].as_str().unwrap_or_default(),
                gate["id"].as_str().unwrap_or_default(),
            ));
        }
    }
    let (mut regressions, mut improvements) = (Vec::new(), Vec::new());
    for (check, required) in gate_checks(gate) {
        let before = baseline["results"][&check].as_str().unwrap_or("missing");
        let after = candidate["results"][&check].as_str().unwrap_or("missing");
        let change = json!({"check": check, "required": required, "baseline": before, "candidate": after});
        if before == "passed" && after != "passed" {
            regressions.push(change);
            if required {
                blocked = true;
                reasons.push(format!(
                    "Required check {check} regressed: passed in baseline, {after} in candidate"
                ));
            } else {
                reasons.push(format!(
                    "Optional check {check} regressed ({after}); reported, not blocking"
                ));
            }
        } else if before != "passed" && after == "passed" {
            improvements.push(change);
        } else if required && after != "passed" {
            blocked = true;
            reasons.push(format!("Required check {check} is {after} in candidate"));
        }
    }
    if !blocked {
        reasons.insert(
            0,
            format!(
                "Every required check passed in candidate {}",
                candidate["variant"].as_str().unwrap_or_default()
            ),
        );
    }
    (blocked, reasons, regressions, improvements)
}

fn nonempty_string(value: Option<&Value>) -> bool {
    value.and_then(Value::as_str).is_some_and(|text| !text.is_empty())
}

fn string_or_null(value: Option<&Value>) -> bool {
    value.is_some_and(|v| v.is_null() || nonempty_string(Some(v)))
}

fn only(map: &Map<String, Value>, keys: &[&str]) -> bool {
    map.keys().all(|key| keys.contains(&key.as_str()))
}

/// `gateSchema`.
fn valid_gate(gate: &Value) -> bool {
    let Some(map) = gate.as_object() else {
        return false;
    };
    only(
        map,
        &["id", "checks", "fixture_hash", "digest", "defined_at", "defined_by"],
    ) && ["id", "fixture_hash", "digest"]
        .iter()
        .all(|key| nonempty_string(map.get(*key)))
        && map.get("defined_at").is_some_and(Value::is_number)
        && string_or_null(map.get("defined_by"))
        && map.get("checks").and_then(Value::as_array).is_some_and(|checks| {
            checks.iter().all(|check| {
                check.as_object().is_some_and(|check| {
                    only(check, &["name", "required"])
                        && nonempty_string(check.get("name"))
                        && check.get("required").is_some_and(Value::is_boolean)
                })
            })
        })
}

fn valid_outcome(value: Option<&Value>) -> bool {
    value.and_then(Value::as_str).is_some_and(|o| OUTCOMES.contains(&o))
}

/// `runSchema`.
fn valid_run(run: Option<&Value>) -> bool {
    run.and_then(Value::as_object).is_some_and(|map| {
        only(map, &["variant", "fixture_hash", "results"])
            && nonempty_string(map.get("variant"))
            && nonempty_string(map.get("fixture_hash"))
            && map
                .get("results")
                .and_then(Value::as_object)
                .is_some_and(|results| results.values().all(|o| valid_outcome(Some(o))))
    })
}

/// `changeSchema`.
fn valid_change(change: &Value) -> bool {
    change.as_object().is_some_and(|map| {
        only(map, &["check", "required", "baseline", "candidate"])
            && map.get("check").is_some_and(Value::is_string)
            && map.get("required").is_some_and(Value::is_boolean)
            && valid_outcome(map.get("baseline"))
            && valid_outcome(map.get("candidate"))
    })
}

/// `attemptSchema`.
fn valid_attempt(attempt: &Value) -> bool {
    let Some(map) = attempt.as_object() else {
        return false;
    };
    let changes = |key: &str| {
        map.get(key)
            .and_then(Value::as_array)
            .is_some_and(|list| list.iter().all(valid_change))
    };
    only(
        map,
        &[
            "id",
            "gate_id",
            "gate_digest",
            "at",
            "requester_task_id",
            "baseline",
            "candidate",
            "decision",
            "reasons",
            "regressions",
            "improvements",
        ],
    ) && ["id", "gate_id", "gate_digest"]
        .iter()
        .all(|key| nonempty_string(map.get(*key)))
        && map.get("at").is_some_and(Value::is_number)
        && string_or_null(map.get("requester_task_id"))
        && valid_run(map.get("baseline"))
        && valid_run(map.get("candidate"))
        && map
            .get("decision")
            .and_then(Value::as_str)
            .is_some_and(|d| d == "passed" || d == "blocked")
        && map
            .get("reasons")
            .and_then(Value::as_array)
            .is_some_and(|list| list.iter().all(Value::is_string))
        && changes("regressions")
        && changes("improvements")
}

/// `documentSchema`.
fn valid_document(document: &Value) -> bool {
    document.as_object().is_some_and(|map| {
        only(map, &["version", "gates", "attempts"])
            && map.get("version").and_then(Value::as_f64) == Some(1.0)
            && map
                .get("gates")
                .and_then(Value::as_array)
                .is_some_and(|gates| gates.iter().all(valid_gate))
            && map
                .get("attempts")
                .and_then(Value::as_array)
                .is_some_and(|attempts| attempts.iter().all(valid_attempt))
    })
}

struct State {
    document: Value,
    /// The outcome of loading the stored document, kept as TS keeps its load promise.
    loaded: Option<Result<(), String>>,
    broken: bool,
}

/// The release-gate module over one durable document (`createReleaseGateModule`).
pub struct ReleaseGateModule {
    store: Arc<dyn DocumentStore>,
    /// `None`: the host's clock.
    now: Option<Clock>,
    ids: Arc<dyn Fn() -> String + Send + Sync>,
    state: Mutex<State>,
}

impl ReleaseGateModule {
    pub fn new(store: Arc<dyn DocumentStore>, now: Option<Clock>) -> Self {
        ReleaseGateModule {
            store,
            now,
            ids: Arc::new(uuid),
            state: Mutex::new(State {
                document: json!({"version": 1, "gates": [], "attempts": []}),
                loaded: None,
                broken: false,
            }),
        }
    }

    /// Attempt ids from `ids` instead of random UUIDs (fixtures).
    pub fn with_ids(mut self, ids: Arc<dyn Fn() -> String + Send + Sync>) -> Self {
        self.ids = ids;
        self
    }

    fn load(&self) -> Result<Value, String> {
        let Some(saved) = read_versioned("ultron.module/release-gates", self.store.read()?, "version")? else {
            return Ok(json!({"version": 1, "gates": [], "attempts": []}));
        };
        if !valid_document(&saved) {
            return Err("Invalid release gate document".into());
        }
        for gate in saved["gates"].as_array().into_iter().flatten() {
            let id = gate["id"].as_str().unwrap_or_default();
            if gate["digest"]
                != gate_digest(
                    id,
                    &gate_checks(gate),
                    gate["fixture_hash"].as_str().unwrap_or_default(),
                )
            {
                return Err(format!("Release gate {id} definition does not match its digest"));
            }
        }
        Ok(saved)
    }

    /// The loaded state (`enqueue`): refused once a write failed; the stored document is read once.
    fn ready<'a>(&self, state: &'a mut State) -> Result<&'a mut State, String> {
        if state.broken {
            return Err(DURABILITY_ERROR.into());
        }
        if state.loaded.is_none() {
            state.loaded = Some(self.load().map(|document| state.document = document));
        }
        state.loaded.clone().unwrap_or(Ok(()))?;
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

    fn find_gate(document: &Value, id: &str) -> Result<Value, String> {
        document["gates"]
            .as_array()
            .and_then(|gates| gates.iter().find(|gate| gate["id"] == id))
            .cloned()
            .ok_or_else(|| format!("Unknown release gate: {id}"))
    }

    /// Serves one `gates.*` request from `caller` (a task id, `None` for the root) at time `now`.
    pub fn handle_at(
        &self,
        kind: &str,
        payload: &Map<String, Value>,
        caller: Option<&str>,
        now: i64,
    ) -> Result<Value, String> {
        let now = self.now.as_ref().map_or(now, |clock| clock());
        match kind {
            "gates.define" => {
                fields(payload, &["id", "checks", "fixture_hash"])?;
                let id = gate_id_field(payload.get("id"), "id")?;
                let checks = checks_field(payload.get("checks"))?;
                let fixture_hash = hash_field(payload.get("fixture_hash"), "fixture_hash")?;
                let digest = gate_digest(&id, &checks, &fixture_hash);
                let mut guard = lock(&self.state);
                let state = self.ready(&mut guard)?;
                if let Ok(existing) = Self::find_gate(&state.document, &id) {
                    if existing["digest"] != digest.as_str() {
                        return Err(format!(
                            "Release gate {id} is already defined with different content and cannot be changed"
                        ));
                    }
                    return Ok(existing);
                }
                let checks: Vec<Value> = checks
                    .iter()
                    .map(|(name, required)| json!({"name": name, "required": required}))
                    .collect();
                let gate = json!({
                    "id": id, "checks": checks, "fixture_hash": fixture_hash, "digest": digest,
                    "defined_at": now, "defined_by": caller,
                });
                let mut next = state.document.clone();
                if let Some(gates) = next["gates"].as_array_mut() {
                    gates.push(gate.clone());
                }
                self.write(state, next)?;
                Ok(gate)
            }
            "gates.compare" => {
                fields(payload, &["gate_id", "baseline", "candidate"])?;
                let gate_id = gate_id_field(payload.get("gate_id"), "gate_id")?;
                let mut guard = lock(&self.state);
                let state = self.ready(&mut guard)?;
                if state.document["attempts"].as_array().map_or(0, Vec::len) >= MAX_ATTEMPTS {
                    return Err(format!("Release gate history is full ({MAX_ATTEMPTS} attempts)"));
                }
                let gate = Self::find_gate(&state.document, &gate_id)?;
                let baseline = run_field(payload.get("baseline"), "baseline", &gate)?;
                let candidate = run_field(payload.get("candidate"), "candidate", &gate)?;
                let (blocked, reasons, regressions, improvements) = judge(&gate, &baseline, &candidate);
                let attempt = json!({
                    "id": format!("gate-attempt-{}", (self.ids)()),
                    "gate_id": gate["id"], "gate_digest": gate["digest"], "at": now, "requester_task_id": caller,
                    "baseline": baseline, "candidate": candidate,
                    "decision": if blocked { "blocked" } else { "passed" },
                    "reasons": reasons, "regressions": regressions, "improvements": improvements,
                });
                let mut next = state.document.clone();
                if let Some(attempts) = next["attempts"].as_array_mut() {
                    attempts.push(attempt.clone());
                }
                self.write(state, next)?;
                Ok(attempt)
            }
            "gates.history" => {
                fields(payload, &["gate_id"])?;
                let gate_id = gate_id_field(payload.get("gate_id"), "gate_id")?;
                let mut guard = lock(&self.state);
                let state = self.ready(&mut guard)?;
                let gate = Self::find_gate(&state.document, &gate_id)?;
                let attempts: Vec<Value> = state.document["attempts"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|attempt| attempt["gate_id"] == gate_id.as_str())
                    .cloned()
                    .collect();
                Ok(json!({"gate": gate, "attempts": attempts}))
            }
            "gates.list" => {
                fields(payload, &[])?;
                let mut guard = lock(&self.state);
                let state = self.ready(&mut guard)?;
                Ok(json!({"gates": state.document["gates"]}))
            }
            _ => Err(format!("Unknown release gate request: {kind}")),
        }
    }
}

impl kernel_apis::ApiModule for ReleaseGateModule {
    fn prefixes(&self) -> &[&str] {
        &["gates."]
    }

    fn start(&self, _host: &dyn kernel_apis::TaskHost) -> Result<(), String> {
        let mut guard = lock(&self.state);
        self.ready(&mut guard).map(drop)
    }

    fn handle(&self, request: &kernel_apis::Request<'_>) -> Result<Value, String> {
        self.handle_at(request.kind, request.payload, request.caller_task, request.host.now())
    }
}
