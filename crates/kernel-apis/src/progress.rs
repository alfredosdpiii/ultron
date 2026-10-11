//! `progress.*` (port of `ultron/progress.ts`): evidence-based progress receipts and reassessment. Progress is
//! inferred only from recorded evidence and task state; reassessment never raises a budget, and a completion claim
//! is verified only by a verifier task the host ran that returned `passed: true`. The document is
//! `{version: 1, receipts, decisions}`; a verifier still pending when the owner ended is settled as unverified on load.

use crate::store::{DocumentStore, read_versioned};
use crate::util::{fields, js_trim, lock, utf16_len, uuid};
use crate::{ApiModule, Request, TaskHost};
use serde_json::{Map, Value, json};
use std::sync::{Arc, Mutex};

const DEFAULT_STALL_AFTER_MS: i64 = 10 * 60 * 1000;
const DEFAULT_MAX_RECEIPTS: usize = 256;
const MAX_EVIDENCE: usize = 32;
const MAX_METRICS: usize = 32;
const MAX_SUMMARY: usize = 4096;
const MAX_REF: usize = 1024;
const DURABILITY_ERROR: &str = "Progress store durability is uncertain; reopen the owner";
const TERMINAL: &[&str] = &["completed", "failed", "cancelled", "interrupted"];
const CLASSIFICATIONS: &[&str] = &["progressing", "busy", "stalled", "finished"];
const CLAIM_DECISIONS: &[&str] = &["pending", "verified", "unverified", "rejected"];

fn task_id_field(value: Option<&Value>) -> Result<String, String> {
    match value.and_then(Value::as_str) {
        Some(text) if !js_trim(text).is_empty() => Ok(text.to_string()),
        _ => Err("task_id must be a nonempty string".into()),
    }
}

fn plain_object<'a>(value: &'a Value, name: &str) -> Result<&'a Map<String, Value>, String> {
    value.as_object().ok_or_else(|| format!("{name} must be a JSON object"))
}

/// `/^[a-z][a-z0-9_.-]{0,63}$/`
fn valid_kind(kind: &str) -> bool {
    let bytes = kind.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_.-".contains(b))
}

/// `/^[a-z][a-z0-9-]*@[0-9]+$/`
fn valid_definition(text: &str) -> bool {
    let Some((id, version)) = text.split_once('@') else {
        return false;
    };
    id.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !version.is_empty()
        && version.bytes().all(|b| b.is_ascii_digit())
}

fn valid_sha256(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn evidence_list(value: Option<&Value>) -> Result<Vec<Value>, String> {
    let items = value
        .and_then(Value::as_array)
        .filter(|items| !items.is_empty() && items.len() <= MAX_EVIDENCE)
        .ok_or_else(|| format!("evidence must be a list of 1 to {MAX_EVIDENCE} entries"))?;
    items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let entry = plain_object(item, &format!("evidence[{index}]"))?;
            fields(entry, &["kind", "ref", "sha256"])?;
            let kind = entry
                .get("kind")
                .and_then(Value::as_str)
                .filter(|kind| valid_kind(kind))
                .ok_or_else(|| format!("evidence[{index}].kind must be a short lowercase identifier"))?;
            let reference = entry
                .get("ref")
                .and_then(Value::as_str)
                .filter(|r| !js_trim(r).is_empty() && utf16_len(r) <= MAX_REF)
                .ok_or_else(|| {
                    format!("evidence[{index}].ref must be a nonempty string of at most {MAX_REF} characters")
                })?;
            match entry.get("sha256").filter(|sha| !sha.is_null()) {
                Some(sha) => {
                    let sha = sha
                        .as_str()
                        .filter(|sha| valid_sha256(sha))
                        .ok_or_else(|| format!("evidence[{index}].sha256 must be a lowercase SHA-256 hex string"))?;
                    Ok(json!({"kind": kind, "ref": reference, "sha256": sha}))
                }
                None => Ok(json!({"kind": kind, "ref": reference})),
            }
        })
        .collect()
}

fn metrics_field(value: Option<&Value>) -> Result<Option<Value>, String> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let metrics = plain_object(value, "metrics")?;
    if metrics.len() > MAX_METRICS {
        return Err(format!("metrics may hold at most {MAX_METRICS} entries"));
    }
    for (key, metric) in metrics {
        let length = utf16_len(key);
        if length == 0 || length > 64 {
            return Err("metric names must be 1 to 64 characters".into());
        }
        if !metric.is_number() {
            return Err(format!("metric {key} must be a finite number"));
        }
    }
    Ok(Some(value.clone()))
}

fn evidence_key(entry: &Value) -> String {
    format!(
        "{}:{}",
        entry["kind"].as_str().unwrap_or(""),
        entry["ref"].as_str().unwrap_or("")
    )
}

fn nonempty_text(value: Option<&Value>) -> bool {
    value.and_then(Value::as_str).is_some_and(|text| !text.is_empty())
}

fn text_or_null(value: Option<&Value>) -> bool {
    value.is_some_and(|value| value.is_null() || value.as_str().is_some_and(|text| !text.is_empty()))
}

fn strings(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_array)
        .is_some_and(|items| items.iter().all(Value::is_string))
}

fn exact(map: &Map<String, Value>, keys: &[&str]) -> bool {
    map.keys().all(|key| keys.contains(&key.as_str()))
}

fn valid_receipt(receipt: &Value) -> bool {
    let Some(r) = receipt.as_object() else {
        return false;
    };
    exact(
        r,
        &[
            "id",
            "task_id",
            "reporter_task_id",
            "at",
            "summary",
            "evidence",
            "metrics",
            "new_evidence",
        ],
    ) && nonempty_text(r.get("id"))
        && nonempty_text(r.get("task_id"))
        && text_or_null(r.get("reporter_task_id"))
        && r.get("at").is_some_and(Value::is_number)
        && r.get("summary").is_some_and(Value::is_string)
        && r.get("evidence").and_then(Value::as_array).is_some_and(|items| {
            items.iter().all(|e| {
                e.as_object().is_some_and(|e| {
                    exact(e, &["kind", "ref", "sha256"])
                        && e.get("kind").is_some_and(Value::is_string)
                        && e.get("ref").is_some_and(Value::is_string)
                        && e.get("sha256").is_none_or(Value::is_string)
                })
            })
        })
        && r.get("metrics")
            .is_none_or(|m| m.as_object().is_some_and(|m| m.values().all(Value::is_number)))
        && strings(r.get("new_evidence"))
}

fn valid_decision(decision: &Value) -> bool {
    let Some(d) = decision.as_object() else {
        return false;
    };
    let assessment = d.get("assessment").and_then(Value::as_object).is_some_and(|a| {
        exact(a, &["classification", "reasons", "receipts"])
            && a.get("classification")
                .and_then(Value::as_str)
                .is_some_and(|c| CLASSIFICATIONS.contains(&c))
            && strings(a.get("reasons"))
            && strings(a.get("receipts"))
    });
    let budget = d.get("budget").is_none_or(|b| {
        b.as_object().is_some_and(|b| {
            exact(b, &["requested", "decision", "reason"])
                && b.contains_key("requested")
                && b.get("decision") == Some(&json!("denied"))
                && b.get("reason").is_some_and(Value::is_string)
        })
    });
    let claim = d.get("claim").is_none_or(|c| {
        c.as_object().is_some_and(|c| {
            exact(
                c,
                &[
                    "claim",
                    "decision",
                    "reason",
                    "verifier",
                    "verifier_task_id",
                    "verifier_status",
                ],
            ) && c.get("claim") == Some(&json!("complete"))
                && c.get("decision")
                    .and_then(Value::as_str)
                    .is_some_and(|x| CLAIM_DECISIONS.contains(&x))
                && c.get("reason").is_some_and(Value::is_string)
                && ["verifier", "verifier_task_id", "verifier_status"]
                    .iter()
                    .all(|key| c.get(*key).is_none_or(Value::is_string))
        })
    });
    exact(
        d,
        &[
            "id",
            "task_id",
            "requester_task_id",
            "at",
            "assessment",
            "budget",
            "claim",
        ],
    ) && nonempty_text(d.get("id"))
        && nonempty_text(d.get("task_id"))
        && text_or_null(d.get("requester_task_id"))
        && d.get("at").is_some_and(Value::is_number)
        && assessment
        && budget
        && claim
}

fn valid_document(document: &Value) -> bool {
    document.as_object().is_some_and(|d| {
        exact(d, &["version", "receipts", "decisions"])
            && d.get("version") == Some(&json!(1))
            && d.get("receipts")
                .and_then(Value::as_array)
                .is_some_and(|r| r.iter().all(valid_receipt))
            && d.get("decisions")
                .and_then(Value::as_array)
                .is_some_and(|r| r.iter().all(valid_decision))
    })
}

struct State {
    document: Value,
    /// The outcome of the first load, kept like TS keeps its loading promise.
    loaded: Option<Result<(), String>>,
    broken: bool,
}

pub struct ProgressModule {
    store: Arc<dyn DocumentStore>,
    stall_after_ms: i64,
    max_receipts: usize,
    state: Mutex<State>,
}

impl ProgressModule {
    pub fn new(store: Arc<dyn DocumentStore>) -> Self {
        Self::with_limits(store, DEFAULT_STALL_AFTER_MS, DEFAULT_MAX_RECEIPTS)
    }

    /// `stallAfterMs` and `maxReceiptsPerTask`.
    pub fn with_limits(store: Arc<dyn DocumentStore>, stall_after_ms: i64, max_receipts: usize) -> Self {
        Self {
            store,
            stall_after_ms,
            max_receipts,
            state: Mutex::new(State {
                document: json!({"version": 1, "receipts": [], "decisions": []}),
                loaded: None,
                broken: false,
            }),
        }
    }

    fn write(&self, state: &mut State, next: Value) -> Result<(), String> {
        if self.store.write(&next).is_err() {
            state.broken = true;
            return Err(DURABILITY_ERROR.into());
        }
        state.document = next;
        Ok(())
    }

    fn load(&self, state: &mut State) -> Result<(), String> {
        let Some(saved) = read_versioned("ultron.module/progress", self.store.read()?, "version")? else {
            return Ok(());
        };
        if !valid_document(&saved) {
            return Err("Invalid progress document".into());
        }
        let pending = |d: &Value| d["claim"]["decision"] == "pending";
        let decisions = saved["decisions"].as_array().cloned().unwrap_or_default();
        if !decisions.iter().any(pending) {
            state.document = saved;
            return Ok(());
        }
        // A verifier that was still running when the owner ended produced no evidence of success.
        let mut next = saved.clone();
        next["decisions"] = decisions
            .into_iter()
            .map(|mut decision| {
                if pending(&decision) {
                    decision["claim"]["decision"] = "unverified".into();
                    decision["claim"]["reason"] = "Owner ended before the verifier returned".into();
                }
                decision
            })
            .collect();
        self.write(state, next)
    }

    /// Runs `change` on the loaded document, one at a time (TS `enqueue`).
    fn enqueue<T>(&self, change: impl FnOnce(&mut State) -> Result<T, String>) -> Result<T, String> {
        let mut state = lock(&self.state);
        if state.broken {
            return Err(DURABILITY_ERROR.into());
        }
        if state.loaded.is_none() {
            let outcome = self.load(&mut state);
            state.loaded = Some(outcome);
        }
        state.loaded.clone().expect("loaded")?;
        if state.broken {
            return Err(DURABILITY_ERROR.into());
        }
        change(&mut state)
    }

    fn find_task(host: &dyn TaskHost, task_id: &str) -> Result<(Value, Vec<Value>), String> {
        let all = host.tasks()?;
        let task = all
            .iter()
            .find(|task| task["id"] == task_id)
            .cloned()
            .ok_or("Unknown Ultron task")?;
        Ok((task, all))
    }

    /// The caller itself, the root agent, or any ancestor of the task may inspect and reassess it.
    fn owner_or_ancestor(caller: Option<&str>, task: &Value, all: &[Value]) -> Result<(), String> {
        let Some(caller) = caller else {
            return Ok(());
        };
        if task["id"] == caller {
            return Ok(());
        }
        let mut seen = std::collections::HashSet::new();
        let mut parent = task["parentId"].as_str().map(str::to_string);
        while let Some(id) = parent {
            if id == caller {
                return Ok(());
            }
            if !seen.insert(id.clone()) {
                break;
            }
            parent = all
                .iter()
                .find(|candidate| candidate["id"] == id.as_str())
                .and_then(|candidate| candidate["parentId"].as_str().map(str::to_string));
        }
        Err("Caller may only access its own progress or that of its descendants".into())
    }

    fn receipts_of(document: &Value, task_id: &str) -> Vec<Value> {
        document["receipts"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|receipt| receipt["task_id"] == task_id)
            .cloned()
            .collect()
    }

    fn assess(&self, document: &Value, task: &Value, now: i64) -> Value {
        let task_id = task["id"].as_str().unwrap_or_default();
        let task_state = task["state"].as_str().unwrap_or_default();
        let receipts = Self::receipts_of(document, task_id);
        let ids = |items: &[&Value]| -> Vec<Value> { items.iter().map(|r| r["id"].clone()).collect() };
        let mut out = json!({
            "task_id": task_id,
            "task_state": task["state"],
            "stall_after_ms": self.stall_after_ms,
            "assessed_at": now,
            "verification": "unverified",
        });
        let all: Vec<&Value> = receipts.iter().collect();
        if TERMINAL.contains(&task_state) {
            out["classification"] = "finished".into();
            let result = task.get("result").filter(|result| !result.is_null());
            if let Some(result) = result {
                out["result_status"] = result["status"].clone();
            }
            let with = result
                .map(|result| format!(" with result {}", result["status"].as_str().unwrap_or("")))
                .unwrap_or_default();
            out["reasons"] = json!([
                format!("Task is {task_state}{with}"),
                "A terminal state is not verification; only a passing verifier can verify completion",
            ]);
            out["receipts"] = Value::Array(ids(&all));
            return out;
        }
        let at = |receipt: &Value| receipt["at"].as_i64().unwrap_or(0);
        let recent: Vec<&Value> = receipts
            .iter()
            .filter(|receipt| now - at(receipt) <= self.stall_after_ms)
            .collect();
        if recent.is_empty() {
            let last = receipts.last();
            out["classification"] = "stalled".into();
            let reason = match last {
                None => format!("Task is {task_state} and has never reported progress"),
                Some(last) => format!(
                    "Task is {task_state} and its last receipt is {}ms old, beyond the {}ms window",
                    now - at(last),
                    self.stall_after_ms
                ),
            };
            out["reasons"] = json!([reason]);
            out["receipts"] = json!(last.map(|last| vec![last["id"].clone()]).unwrap_or_default());
            return out;
        }
        let novel: Vec<&Value> = recent
            .iter()
            .copied()
            .filter(|receipt| receipt["new_evidence"].as_array().is_some_and(|n| !n.is_empty()))
            .collect();
        if !novel.is_empty() {
            out["classification"] = "progressing".into();
            out["reasons"] = novel
                .iter()
                .map(|receipt| {
                    let keys: Vec<&str> = receipt["new_evidence"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .collect();
                    Value::from(format!(
                        "Receipt {} added new or changed evidence: {}",
                        receipt["id"].as_str().unwrap_or(""),
                        keys.join(", ")
                    ))
                })
                .collect();
            out["receipts"] = Value::Array(ids(&novel));
            return out;
        }
        out["classification"] = "busy".into();
        out["reasons"] = json!([format!(
            "{} receipt(s) within {}ms repeated previously seen evidence; activity is not progress",
            recent.len(),
            self.stall_after_ms
        )]);
        out["receipts"] = Value::Array(ids(&recent));
        out
    }

    fn report(&self, request: &Request<'_>) -> Result<Value, String> {
        let payload = request.payload;
        fields(payload, &["task_id", "summary", "evidence", "metrics"])?;
        let caller = request.caller_task;
        let task_id = match payload.get("task_id").filter(|value| !value.is_null()) {
            None => caller.map(str::to_string),
            value => Some(task_id_field(value)?),
        };
        let task_id = task_id.ok_or("The root agent must name the task_id it reports for")?;
        let summary = payload
            .get("summary")
            .and_then(Value::as_str)
            .filter(|s| !js_trim(s).is_empty() && utf16_len(s) <= MAX_SUMMARY)
            .ok_or_else(|| format!("summary must be a nonempty string of at most {MAX_SUMMARY} characters"))?;
        let evidence = evidence_list(payload.get("evidence"))?;
        let metrics = metrics_field(payload.get("metrics"))?;
        let (task, _) = Self::find_task(request.host, &task_id)?;
        if task["id"] != caller.map(Value::from).unwrap_or(Value::Null) && task["parentId"].as_str() != caller {
            return Err("Caller may only report progress for itself or its direct children".into());
        }
        self.enqueue(|state| {
            let previous = Self::receipts_of(&state.document, &task_id);
            if previous.len() >= self.max_receipts {
                return Err(format!(
                    "Task {task_id} already has the maximum of {} progress receipts",
                    self.max_receipts
                ));
            }
            let mut known: Vec<(String, Option<String>)> = Vec::new();
            for receipt in &previous {
                for entry in receipt["evidence"].as_array().into_iter().flatten() {
                    let key = evidence_key(entry);
                    let sha = entry["sha256"].as_str().map(str::to_string);
                    match known.iter_mut().find(|(k, _)| *k == key) {
                        Some(slot) => slot.1 = sha.or(slot.1.take()),
                        None => known.push((key, sha)),
                    }
                }
            }
            let mut fresh: Vec<String> = Vec::new();
            for entry in &evidence {
                let key = evidence_key(entry);
                let seen = known.iter().find(|(k, _)| *k == key);
                let changed = entry["sha256"]
                    .as_str()
                    .is_some_and(|sha| seen.and_then(|(_, known)| known.as_deref()) != Some(sha));
                if (seen.is_none() || changed) && !fresh.contains(&key) {
                    fresh.push(key);
                }
            }
            let mut receipt = json!({
                "id": format!("progress-receipt-{}", uuid()),
                "task_id": task_id,
                "reporter_task_id": caller,
                "at": request.host.now(),
                "summary": summary,
                "evidence": evidence,
            });
            if let Some(metrics) = metrics {
                receipt["metrics"] = metrics;
            }
            receipt["new_evidence"] = json!(fresh);
            let mut next = state.document.clone();
            if let Some(Value::Array(receipts)) = next.get_mut("receipts") {
                receipts.push(receipt.clone());
            }
            self.write(state, next)?;
            Ok(receipt)
        })
    }

    fn record_decision(&self, decision: &Value) -> Result<(), String> {
        self.enqueue(|state| {
            let mut next = state.document.clone();
            if let Some(Value::Array(decisions)) = next.get_mut("decisions") {
                decisions.push(decision.clone());
            }
            self.write(state, next)
        })
    }

    fn verify(request: &Request<'_>, task: &Value, verifier: &str, input: Value, caller: Option<&str>) -> Value {
        let host = request.host;
        let mut claim = json!({"claim": "complete", "verifier": verifier});
        let spawned = host.spawn(&json!({"definition": verifier, "input": input}), caller, request.signal);
        let verifier_task_id = match spawned {
            Ok(task) => task["id"].as_str().unwrap_or_default().to_string(),
            Err(error) => {
                claim["decision"] = "unverified".into();
                claim["reason"] = format!("Verifier could not be started: {error}").into();
                return claim;
            }
        };
        claim["verifier_task_id"] = verifier_task_id.clone().into();
        let result = match host.result(&verifier_task_id) {
            Ok(result) => result,
            Err(error) => {
                claim["decision"] = "unverified".into();
                claim["reason"] = format!("Verifier result is unavailable: {error}").into();
                return claim;
            }
        };
        let status = result["status"].as_str().unwrap_or_default().to_string();
        claim["verifier_status"] = status.clone().into();
        if status != "succeeded" {
            let error = result["error"]
                .as_str()
                .filter(|e| !e.is_empty())
                .map(|e| format!(": {e}"))
                .unwrap_or_default();
            claim["decision"] = "unverified".into();
            claim["reason"] = format!("Verifier {status}{error}").into();
            return claim;
        }
        let passed = result
            .get("value")
            .and_then(Value::as_object)
            .and_then(|v| v.get("passed"));
        let (decision, reason) = match passed {
            Some(Value::Bool(true)) => (
                "verified",
                format!(
                    "Verifier {verifier} passed for task {}",
                    task["id"].as_str().unwrap_or("")
                ),
            ),
            Some(Value::Bool(false)) => ("unverified", format!("Verifier {verifier} did not pass")),
            _ => ("unverified", "Verifier output has no boolean passed field".to_string()),
        };
        claim["decision"] = decision.into();
        claim["reason"] = reason.into();
        claim
    }

    fn reassess(&self, request: &Request<'_>) -> Result<Value, String> {
        let payload = request.payload;
        fields(
            payload,
            &["task_id", "extend_budget", "claim", "verifier", "verifier_input"],
        )?;
        let task_id = task_id_field(payload.get("task_id"))?;
        let present = |key: &str| payload.get(key).filter(|value| !value.is_null());
        let extend = present("extend_budget");
        if extend.is_some_and(|e| !e.is_boolean() && !e.is_object()) {
            return Err("extend_budget must be a boolean or a JSON object".into());
        }
        let claim = present("claim");
        if claim.is_some_and(|c| c != "complete") {
            return Err("claim must be \"complete\"".into());
        }
        let verifier = present("verifier");
        if verifier.is_some_and(|v| !v.as_str().is_some_and(valid_definition)) {
            return Err("verifier must be id@version".into());
        }
        let verifier = verifier.and_then(Value::as_str);
        let verifier_input = present("verifier_input");
        if (verifier.is_some() || verifier_input.is_some()) && claim.is_none() {
            return Err("verifier and verifier_input require claim".into());
        }
        let caller = request.caller_task;
        let (task, all) = Self::find_task(request.host, &task_id)?;
        Self::owner_or_ancestor(caller, &task, &all)?;
        let assessment = self.enqueue(|state| Ok(self.assess(&state.document, &task, request.host.now())))?;
        let mut decision = json!({
            "id": format!("progress-decision-{}", uuid()),
            "task_id": task_id,
            "requester_task_id": caller,
            "at": request.host.now(),
            "assessment": {
                "classification": assessment["classification"],
                "reasons": assessment["reasons"],
                "receipts": assessment["receipts"],
            },
        });
        if let Some(extend) = extend.filter(|e| **e != Value::Bool(false)) {
            decision["budget"] = json!({
                "requested": extend,
                "decision": "denied",
                "reason": "Budgets are fixed at admission; progress reassessment cannot extend them",
            });
        }
        if claim.is_none() {
            self.record_decision(&decision)?;
            return Ok(decision);
        }
        let state = task["state"].as_str().unwrap_or_default();
        if state != "completed" || task["result"]["status"] != "succeeded" {
            decision["claim"] = json!({
                "claim": "complete",
                "decision": "rejected",
                "reason": format!("Task is {state}; only a succeeded task can be claimed complete"),
            });
            self.record_decision(&decision)?;
            return Ok(decision);
        }
        let Some(verifier) = verifier else {
            decision["claim"] = json!({
                "claim": "complete",
                "decision": "unverified",
                "reason": "No verifier was named; a task result alone is not verification",
            });
            self.record_decision(&decision)?;
            return Ok(decision);
        };
        // Record the intent first so an owner crash mid-verification leaves an unverified trace.
        decision["claim"] =
            json!({"claim": "complete", "decision": "pending", "reason": "Verifier running", "verifier": verifier});
        self.record_decision(&decision)?;
        let input = match verifier_input {
            Some(input) => input.clone(),
            None => {
                let evidence: Vec<Value> = {
                    let state = lock(&self.state);
                    Self::receipts_of(&state.document, &task_id)
                        .iter()
                        .flat_map(|receipt| receipt["evidence"].as_array().cloned().unwrap_or_default())
                        .collect()
                };
                let result = task["result"].get("value").cloned().unwrap_or(Value::Null);
                json!({"task_id": task_id, "result": result, "evidence": evidence})
            }
        };
        let outcome = Self::verify(request, &task, verifier, input, caller);
        let id = decision["id"].clone();
        self.enqueue(|state| {
            let mut next = state.document.clone();
            let mut updated = Value::Null;
            if let Some(Value::Array(decisions)) = next.get_mut("decisions") {
                for decision in decisions.iter_mut().filter(|d| d["id"] == id) {
                    decision["claim"] = outcome.clone();
                    updated = decision.clone();
                }
            }
            self.write(state, next)?;
            Ok(updated)
        })
    }

    fn history(&self, request: &Request<'_>) -> Result<Value, String> {
        fields(request.payload, &["task_id"])?;
        let task_id = task_id_field(request.payload.get("task_id"))?;
        let (task, all) = Self::find_task(request.host, &task_id)?;
        Self::owner_or_ancestor(request.caller_task, &task, &all)?;
        self.enqueue(|state| {
            let decisions: Vec<Value> = state.document["decisions"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|decision| decision["task_id"] == task_id.as_str())
                .cloned()
                .collect();
            Ok(json!({
                "task_id": task_id,
                "receipts": Self::receipts_of(&state.document, &task_id),
                "decisions": decisions,
            }))
        })
    }

    fn assess_request(&self, request: &Request<'_>) -> Result<Value, String> {
        fields(request.payload, &["task_id"])?;
        let task_id = task_id_field(request.payload.get("task_id"))?;
        let (task, all) = Self::find_task(request.host, &task_id)?;
        Self::owner_or_ancestor(request.caller_task, &task, &all)?;
        self.enqueue(|state| Ok(self.assess(&state.document, &task, request.host.now())))
    }

    /// The stored document (loading it first).
    pub fn document(&self) -> Result<Value, String> {
        self.enqueue(|state| Ok(state.document.clone()))
    }
}

impl ApiModule for ProgressModule {
    fn prefixes(&self) -> &[&str] {
        &["progress."]
    }

    fn start(&self, _host: &dyn TaskHost) -> Result<(), String> {
        self.enqueue(|_| Ok(()))
    }

    fn handle(&self, request: &Request<'_>) -> Result<Value, String> {
        match request.kind {
            "progress.report" => self.report(request),
            "progress.assess" => self.assess_request(request),
            "progress.reassess" => self.reassess(request),
            "progress.history" => self.history(request),
            other => Err(format!("Unknown progress request: {other}")),
        }
    }
}
