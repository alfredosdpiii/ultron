//! `schedules.*` and `goals.*` (port of `ultron/schedules.ts`): recurring task firings and goals with required
//! checks. Firings are admitted through [`TaskHost::spawn`] with a slot-derived key (`schedule:<id>:<slot>`), and
//! each slot is claimed durably (`last_slot`, `pending_slot`) before admission, so duplicate ticks and restarts never
//! run a slot twice; a claimed slot that fails is recorded once and never retried. Missed slots coalesce into one run
//! of the latest slot (`skipped`). A goal is achieved only when every required check succeeds with `passed: true`;
//! `max_tasks` caps firings, verification and attachments. The document is `{version: 1, schedules, goals}`.
//!
//! The TS module also ticks on a background timer; here the owner calls [`SchedulesModule::tick`] (or the model
//! sends `schedules.tick`).

use crate::store::{DocumentStore, read_versioned};
use crate::tasks::TaskHost;
use crate::util::{Clock, fields, js_trim, lock, safe_integer, uuid};
use kernel::Signal;
use serde_json::{Map, Value, json};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

const MAX_EVERY_MS: i64 = 366 * 24 * 60 * 60 * 1000;
const MAX_SAFE: i64 = 9_007_199_254_740_991;

fn nonempty_string(value: Option<&Value>, name: &str) -> Result<String, String> {
    match value.and_then(Value::as_str) {
        Some(text) if !js_trim(text).is_empty() => Ok(text.to_string()),
        _ => Err(format!("{name} must be a nonempty string")),
    }
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

fn definition_key(value: Option<&Value>, name: &str) -> Result<String, String> {
    match value.and_then(Value::as_str) {
        Some(text) if valid_definition(text) => Ok(text.to_string()),
        _ => Err(format!("{name} must be id@version")),
    }
}

/// `/^[^/\s]+\/[^/\s]+(?:\/[^/\s]+)*$/`
fn valid_model(text: &str) -> bool {
    let parts: Vec<&str> = text.split('/').collect();
    parts.len() >= 2
        && parts
            .iter()
            .all(|part| !part.is_empty() && !part.chars().any(|c| c.is_whitespace() || c == '\u{feff}'))
}

fn integer(value: Option<&Value>, name: &str, min: i64, max: i64) -> Result<i64, String> {
    value
        .filter(|value| value.is_number())
        .and_then(safe_integer)
        .filter(|number| (min..=max).contains(number))
        .ok_or_else(|| format!("{name} must be an integer between {min} and {max}"))
}

fn optional_integer(value: Option<&Value>, name: &str, min: i64) -> Result<Option<i64>, String> {
    match value {
        None | Some(Value::Null) => Ok(None),
        value => integer(value, name, min, MAX_SAFE).map(Some),
    }
}

fn validate_document(saved: Value) -> Result<Value, String> {
    let invalid = || "Invalid schedule document".to_string();
    let map = saved.as_object().ok_or_else(invalid)?;
    let (Some(schedules), Some(goals)) = (
        map.get("schedules").and_then(Value::as_array),
        map.get("goals").and_then(Value::as_array),
    ) else {
        return Err(invalid());
    };
    if map.get("version") != Some(&json!(1)) {
        return Err(invalid());
    }
    let mut ids = HashSet::new();
    for item in schedules.iter().chain(goals) {
        let id = item
            .as_object()
            .and_then(|item| item.get("id"))
            .and_then(Value::as_str)
            .ok_or_else(invalid)?;
        if !ids.insert(id.to_string()) {
            return Err(invalid());
        }
    }
    Ok(saved)
}

/// Slot index for `now`; negative before the schedule starts.
pub fn schedule_slot(start_at: i64, every_ms: i64, now: i64) -> i64 {
    (now - start_at).div_euclid(every_ms)
}

fn schedule_key(schedule: &Value, slot: i64) -> String {
    format!("schedule:{}:{slot}", schedule["id"].as_str().unwrap_or(""))
}

fn check_outcome(definition: &str, task_id: Option<&str>, result: &Value) -> Value {
    let status = result["status"].clone();
    let mut outcome = json!({"definition": definition, "task_id": task_id, "status": status, "passed": false});
    if result["status"] != "succeeded" {
        let error = match result.get("error").and_then(Value::as_str).filter(|e| !e.is_empty()) {
            Some(error) => format!(": {error}"),
            None => String::new(),
        };
        outcome["reason"] = format!("{definition} {}{error}", result["status"].as_str().unwrap_or("")).into();
        return outcome;
    }
    match result["value"]
        .get("passed")
        .and_then(Value::as_bool)
        .filter(|_| result["value"].is_object())
    {
        None => outcome["reason"] = format!("{definition} did not report a boolean passed value").into(),
        Some(passed) => {
            outcome["passed"] = passed.into();
            if !passed {
                outcome["reason"] = format!("{definition} reported passed=false").into();
            }
        }
    }
    outcome
}

fn missing(definition: &str, task_id: Option<&str>, error: &str) -> Value {
    json!({"definition": definition, "task_id": task_id, "status": "missing", "passed": false,
        "reason": format!("{definition}: {error}")})
}

fn find<'a>(items: &'a mut Value, key: &str, id: &str) -> Option<&'a mut Value> {
    items[key].as_array_mut()?.iter_mut().find(|item| item["id"] == id)
}

fn lookup(document: &Value, key: &str, id: Option<&Value>, unknown: &str) -> Result<Value, String> {
    let id = nonempty_string(id, "id")?;
    document[key]
        .as_array()
        .and_then(|items| items.iter().find(|item| item["id"] == id.as_str()))
        .cloned()
        .ok_or_else(|| unknown.to_string())
}

/// Remaining task slots of a goal (`None`: unlimited).
fn capacity(goal: &Value) -> Option<i64> {
    let used = goal["task_ids"].as_array().map_or(0, Vec::len) as i64;
    goal["max_tasks"].as_i64().map(|max| max - used)
}

fn has_capacity(goal: &Value, needed: i64) -> bool {
    capacity(goal).is_none_or(|left| left >= needed)
}

fn push_task(goal: &mut Value, task_id: &Value) {
    if let Some(ids) = goal["task_ids"].as_array_mut()
        && !ids.contains(task_id)
    {
        ids.push(task_id.clone());
    }
}

pub struct SchedulesModule {
    store: Arc<dyn DocumentStore>,
    /// `None`: the host's clock.
    clock: Option<Clock>,
    /// The loaded document; every read-modify-write runs under this lock.
    document: Mutex<Option<Value>>,
}

impl SchedulesModule {
    pub fn new(store: Arc<dyn DocumentStore>, clock: Option<Clock>) -> Self {
        Self {
            store,
            clock,
            document: Mutex::new(None),
        }
    }

    fn now(&self, host: &dyn TaskHost) -> i64 {
        self.clock.as_ref().map_or_else(|| host.now(), |clock| clock())
    }

    fn loaded<'a>(&self, slot: &'a mut Option<Value>) -> Result<&'a mut Value, String> {
        if slot.is_none() {
            *slot = Some(match self.store.read()? {
                None => json!({"version": 1, "schedules": [], "goals": []}),
                saved => {
                    validate_document(read_versioned("ultron.module/schedules", saved, "version")?.unwrap_or_default())?
                }
            });
        }
        Ok(slot.as_mut().expect("loaded"))
    }

    /// Mutates a copy, commits it, then publishes it; a failed mutation or write leaves the old state.
    fn update<T>(
        &self,
        document: &mut Value,
        mutate: impl FnOnce(&mut Value) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut draft = document.clone();
        let result = mutate(&mut draft)?;
        self.store.write(&draft)?;
        *document = draft;
        Ok(result)
    }

    /// Recovers goal task links written by keys the host already admitted before a crash.
    fn reconcile(&self, document: &mut Value, host: &dyn TaskHost) -> Result<(), String> {
        if document["goals"].as_array().is_none_or(Vec::is_empty) {
            return Ok(());
        }
        let tasks = host.tasks()?;
        let mut draft = document.clone();
        let mut changed = false;
        for goal in draft["goals"].as_array_mut().into_iter().flatten() {
            let prefix = format!("goal:{}:", goal["id"].as_str().unwrap_or(""));
            for task in &tasks {
                if task["key"].as_str().is_some_and(|key| key.starts_with(&prefix))
                    && !goal["task_ids"].as_array().is_some_and(|ids| ids.contains(&task["id"]))
                {
                    push_task(goal, &task["id"]);
                    changed = true;
                }
            }
        }
        let schedules = draft["schedules"].as_array().cloned().unwrap_or_default();
        for schedule in schedules {
            let Some(goal_id) = schedule["goal_id"].as_str() else {
                continue;
            };
            let slot = schedule["pending_slot"]
                .as_i64()
                .or_else(|| schedule["last_slot"].as_i64());
            let task = slot.and_then(|slot| {
                let key = schedule_key(&schedule, slot);
                tasks.iter().find(|task| task["key"] == key.as_str())
            });
            if let (Some(goal), Some(task)) = (find(&mut draft, "goals", goal_id), task)
                && !goal["task_ids"].as_array().is_some_and(|ids| ids.contains(&task["id"]))
            {
                push_task(goal, &task["id"]);
                changed = true;
            }
        }
        if changed {
            self.store.write(&draft)?;
            *document = draft;
        }
        Ok(())
    }

    pub fn start(&self, host: &dyn TaskHost) -> Result<(), String> {
        let mut guard = lock(&self.document);
        let document = self.loaded(&mut guard)?;
        self.reconcile(document, host)
    }

    pub fn handle(
        &self,
        kind: &str,
        payload: &Map<String, Value>,
        caller: Option<&str>,
        host: &dyn TaskHost,
        signal: &Signal,
    ) -> Result<Value, String> {
        if kind == "goals.verify" {
            return self.verify(payload, caller, host, signal);
        }
        let mut guard = lock(&self.document);
        let document = self.loaded(&mut guard)?;
        match kind {
            "schedules.create" => self.create_schedule(document, payload, caller, host),
            "schedules.list" => {
                fields(payload, &[])?;
                Ok(document["schedules"].clone())
            }
            "schedules.pause" => self.set_schedule_state(document, payload, "paused"),
            "schedules.resume" => self.set_schedule_state(document, payload, "active"),
            "schedules.delete" => {
                fields(payload, &["id"])?;
                self.update(document, |draft| {
                    let schedule = lookup(draft, "schedules", payload.get("id"), "Unknown schedule")?;
                    if let Some(items) = draft["schedules"].as_array_mut() {
                        items.retain(|item| item["id"] != schedule["id"]);
                    }
                    Ok(json!({"deleted": true}))
                })
            }
            "schedules.tick" => {
                fields(payload, &[])?;
                // Firings outlive the request that happened to drive the tick.
                self.tick_locked(document, host, &Signal::new())
            }
            "goals.create" => self.create_goal(document, payload, caller, host),
            "goals.list" => {
                fields(payload, &[])?;
                Ok(document["goals"].clone())
            }
            "goals.get" => {
                fields(payload, &["id"])?;
                lookup(document, "goals", payload.get("id"), "Unknown goal")
            }
            "goals.pause" => self.set_goal_state(document, payload, "paused"),
            "goals.resume" => self.set_goal_state(document, payload, "active"),
            "goals.attach" => self.attach(document, payload, host),
            _ => Err(format!("Unknown schedule request: {kind}")),
        }
    }

    fn create_schedule(
        &self,
        document: &mut Value,
        payload: &Map<String, Value>,
        owner: Option<&str>,
        host: &dyn TaskHost,
    ) -> Result<Value, String> {
        fields(
            payload,
            &[
                "definition",
                "input",
                "every_ms",
                "start_at",
                "max_runs",
                "goal_id",
                "model",
            ],
        )?;
        let definition = definition_key(payload.get("definition"), "definition")?;
        let input = payload.get("input").cloned().ok_or("input must be JSON")?;
        let every_ms = integer(payload.get("every_ms"), "every_ms", 1, MAX_EVERY_MS)?;
        let now = self.now(host);
        let start_at = optional_integer(payload.get("start_at"), "start_at", 0)?.unwrap_or(now);
        let max_runs = optional_integer(payload.get("max_runs"), "max_runs", 1)?;
        let model = match payload.get("model") {
            None | Some(Value::Null) => Value::Null,
            Some(Value::String(model)) if valid_model(model) => model.clone().into(),
            Some(_) => return Err("model must be provider/model".into()),
        };
        let goal_id = payload.get("goal_id").cloned().unwrap_or(Value::Null);
        self.update(document, |draft| {
            if !goal_id.is_null() && lookup(draft, "goals", Some(&goal_id), "Unknown goal")?["state"] == "achieved" {
                return Err("Cannot attach a schedule to an achieved goal".into());
            }
            let schedule = json!({
                "id": format!("schedule-{}", uuid()),
                "definition": definition, "input": input, "every_ms": every_ms, "start_at": start_at,
                "max_runs": max_runs, "goal_id": goal_id, "model": model, "owner": owner, "state": "active",
                "created_at": now, "runs": 0, "last_slot": null, "pending_slot": null, "last_task_id": null,
                "last_fired_at": null, "last_skipped": 0, "skipped_total": 0, "last_failure": null, "failures": 0,
            });
            if let Some(items) = draft["schedules"].as_array_mut() {
                items.push(schedule.clone());
            }
            Ok(schedule)
        })
    }

    fn set_schedule_state(
        &self,
        document: &mut Value,
        payload: &Map<String, Value>,
        state: &str,
    ) -> Result<Value, String> {
        fields(payload, &["id"])?;
        self.update(document, |draft| {
            let id = lookup(draft, "schedules", payload.get("id"), "Unknown schedule")?["id"].clone();
            let schedule = find(draft, "schedules", id.as_str().unwrap_or("")).expect("found above");
            if schedule["state"] == "completed" {
                return Err("Schedule is completed".into());
            }
            schedule["state"] = state.into();
            Ok(schedule.clone())
        })
    }

    fn create_goal(
        &self,
        document: &mut Value,
        payload: &Map<String, Value>,
        owner: Option<&str>,
        host: &dyn TaskHost,
    ) -> Result<Value, String> {
        fields(payload, &["title", "required_checks", "max_tasks"])?;
        let title = nonempty_string(payload.get("title"), "title")?;
        let items = payload
            .get("required_checks")
            .and_then(Value::as_array)
            .ok_or("required_checks must be an array")?;
        let checks = items
            .iter()
            .map(|item| definition_key(Some(item), "required_checks entry"))
            .collect::<Result<Vec<_>, _>>()?;
        if checks.iter().collect::<HashSet<_>>().len() != checks.len() {
            return Err("required_checks must not repeat".into());
        }
        let max_tasks = optional_integer(payload.get("max_tasks"), "max_tasks", 0)?;
        let now = self.now(host);
        self.update(document, |draft| {
            let goal = json!({
                "id": format!("goal-{}", uuid()), "title": title, "required_checks": checks, "max_tasks": max_tasks,
                "owner": owner, "state": "active", "created_at": now, "task_ids": [], "verify_rounds": 0,
                "verification": null,
            });
            if let Some(items) = draft["goals"].as_array_mut() {
                items.push(goal.clone());
            }
            Ok(goal)
        })
    }

    fn set_goal_state(&self, document: &mut Value, payload: &Map<String, Value>, state: &str) -> Result<Value, String> {
        fields(payload, &["id"])?;
        self.update(document, |draft| {
            let id = lookup(draft, "goals", payload.get("id"), "Unknown goal")?["id"].clone();
            let goal = find(draft, "goals", id.as_str().unwrap_or("")).expect("found above");
            if goal["state"] == "achieved" {
                return Err("Goal is already achieved".into());
            }
            goal["state"] = state.into();
            Ok(goal.clone())
        })
    }

    fn attach(&self, document: &mut Value, payload: &Map<String, Value>, host: &dyn TaskHost) -> Result<Value, String> {
        fields(payload, &["id", "task_id"])?;
        let task_id = nonempty_string(payload.get("task_id"), "task_id")?;
        let goal_id = lookup(document, "goals", payload.get("id"), "Unknown goal")?["id"].clone();
        if !host.tasks()?.iter().any(|task| task["id"] == task_id.as_str()) {
            return Err("Unknown Ultron task".into());
        }
        self.update(document, |draft| {
            let goal = find(draft, "goals", goal_id.as_str().unwrap_or("")).ok_or("Unknown goal")?;
            let task = Value::from(task_id.clone());
            if !goal["task_ids"].as_array().is_some_and(|ids| ids.contains(&task)) {
                if !has_capacity(goal, 1) {
                    return Err(format!("Goal task limit reached ({})", goal["max_tasks"]));
                }
                push_task(goal, &task);
            }
            Ok(goal.clone())
        })
    }

    /// Fires due schedules (the TS background timer's work); returns `{fired, failed}`.
    pub fn tick(&self, host: &dyn TaskHost, signal: &Signal) -> Result<Value, String> {
        let mut guard = lock(&self.document);
        let document = self.loaded(&mut guard)?;
        self.tick_locked(document, host, signal)
    }

    fn tick_locked(&self, document: &mut Value, host: &dyn TaskHost, signal: &Signal) -> Result<Value, String> {
        let mut fired = Vec::new();
        let mut failed = Vec::new();
        let now = self.now(host);
        let ids: Vec<String> = document["schedules"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| item["id"].as_str().map(str::to_string))
            .collect();
        for id in ids {
            let Some(current) = document["schedules"]
                .as_array()
                .and_then(|items| items.iter().find(|item| item["id"] == id.as_str()))
                .cloned()
            else {
                continue;
            };
            if current["state"] != "active" {
                continue;
            }
            let pending = current["pending_slot"].as_i64();
            let slot = pending.unwrap_or_else(|| {
                schedule_slot(
                    current["start_at"].as_i64().unwrap_or(0),
                    current["every_ms"].as_i64().unwrap_or(1),
                    now,
                )
            });
            if slot < 0 {
                continue;
            }
            let resuming = pending.is_some();
            let last = current["last_slot"].as_i64();
            if !resuming && last.is_some_and(|last| slot <= last) {
                continue;
            }
            let goal = current["goal_id"].as_str().and_then(|goal_id| {
                document["goals"]
                    .as_array()
                    .and_then(|goals| goals.iter().find(|goal| goal["id"] == goal_id))
                    .cloned()
            });
            // A missing goal cannot be verified or paused, so its schedules stay idle.
            if !current["goal_id"].is_null() && goal.as_ref().is_none_or(|goal| goal["state"] != "active") {
                continue;
            }
            let skipped = if resuming {
                current["last_skipped"].as_i64().unwrap_or(0)
            } else {
                slot - last.unwrap_or(-1) - 1
            };
            let claimed = self.update(document, |draft| {
                let schedule = find(draft, "schedules", &id).ok_or("Unknown schedule")?;
                schedule["last_slot"] = slot.into();
                schedule["pending_slot"] = slot.into();
                schedule["last_skipped"] = skipped.into();
                if !resuming {
                    schedule["skipped_total"] = (schedule["skipped_total"].as_i64().unwrap_or(0) + skipped).into();
                }
                Ok(schedule.clone())
            })?;
            let mut task = None;
            let mut failure = None;
            if let Some(goal) = goal.as_ref().filter(|goal| !has_capacity(goal, 1)) {
                failure = Some(format!("Goal task limit reached ({})", goal["max_tasks"]));
            } else {
                let mut request = json!({
                    "definition": claimed["definition"], "input": claimed["input"], "key": schedule_key(&claimed, slot),
                });
                if !claimed["model"].is_null() {
                    request["model"] = claimed["model"].clone();
                }
                match host.spawn(&request, claimed["owner"].as_str(), signal) {
                    Ok(spawned) => task = Some(spawned),
                    Err(error) => failure = Some(error),
                }
            }
            // The slot stays claimed on failure: the next attempt is the next slot, never a retry loop.
            self.update(document, |draft| {
                let schedule = find(draft, "schedules", &id).ok_or("Unknown schedule")?;
                schedule["pending_slot"] = Value::Null;
                let mut linked = None;
                match &task {
                    Some(task) => {
                        let runs = schedule["runs"].as_i64().unwrap_or(0) + 1;
                        schedule["runs"] = runs.into();
                        schedule["last_task_id"] = task["id"].clone();
                        schedule["last_fired_at"] = now.into();
                        if schedule["max_runs"].as_i64().is_some_and(|max| runs >= max) {
                            schedule["state"] = "completed".into();
                        }
                        linked = schedule["goal_id"].as_str().map(str::to_string);
                    }
                    None => {
                        schedule["failures"] = (schedule["failures"].as_i64().unwrap_or(0) + 1).into();
                        schedule["last_failure"] = json!({"slot": slot,
                            "error": failure.clone().unwrap_or_else(|| "unknown failure".into()), "at": now});
                    }
                }
                if let (Some(goal_id), Some(task)) = (linked, &task)
                    && let Some(goal) = find(draft, "goals", &goal_id)
                {
                    push_task(goal, &task["id"]);
                }
                Ok(())
            })?;
            match task {
                Some(task) => {
                    fired.push(json!({"schedule_id": id, "slot": slot, "task_id": task["id"], "skipped": skipped}))
                }
                None => failed.push(json!({"schedule_id": id, "slot": slot,
                    "error": failure.unwrap_or_else(|| "unknown failure".into())})),
            }
        }
        Ok(json!({"fired": fired, "failed": failed}))
    }

    /// Runs every required check and accepts the goal only on unanimous, explicit passes. Admission is serialized
    /// with other mutations; waiting for results is not.
    fn verify(
        &self,
        payload: &Map<String, Value>,
        caller: Option<&str>,
        host: &dyn TaskHost,
        signal: &Signal,
    ) -> Result<Value, String> {
        fields(payload, &["id", "input"])?;
        let (goal_id, round, spawned) = {
            let mut guard = lock(&self.document);
            let document = self.loaded(&mut guard)?;
            let goal = lookup(document, "goals", payload.get("id"), "Unknown goal")?;
            if goal["state"] == "achieved" {
                return Err("Goal is already achieved".into());
            }
            if goal["state"] == "paused" {
                return Err("Goal is paused".into());
            }
            let checks: Vec<String> = goal["required_checks"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|check| check.as_str().map(str::to_string))
                .collect();
            if !has_capacity(&goal, checks.len() as i64) {
                return Err(format!(
                    "Goal task limit reached: verification needs {} tasks, {} remain",
                    checks.len(),
                    capacity(&goal).unwrap_or(0).max(0)
                ));
            }
            let goal_id = goal["id"].as_str().unwrap_or("").to_string();
            let round = self.update(document, |draft| {
                let target = find(draft, "goals", &goal_id).ok_or("Unknown goal")?;
                let round = target["verify_rounds"].as_i64().unwrap_or(0) + 1;
                target["verify_rounds"] = round.into();
                Ok(round)
            })?;
            let input = payload
                .get("input")
                .cloned()
                .unwrap_or_else(|| json!({"goal_id": goal_id, "title": goal["title"]}));
            let spawned: Vec<(String, Result<Value, String>)> = checks
                .iter()
                .enumerate()
                .map(|(index, definition)| {
                    let request = json!({"definition": definition, "input": input,
                        "key": format!("goal:{goal_id}:verify:{round}:{index}")});
                    (definition.clone(), host.spawn(&request, caller, signal))
                })
                .collect();
            self.update(document, |draft| {
                let target = find(draft, "goals", &goal_id).ok_or("Unknown goal")?;
                for (_, task) in &spawned {
                    if let Ok(task) = task {
                        push_task(target, &task["id"]);
                    }
                }
                Ok(())
            })?;
            (goal_id, round, spawned)
        };
        let checks: Vec<Value> = spawned
            .iter()
            .map(|(definition, task)| match task {
                Err(error) => missing(definition, None, error),
                Ok(task) => {
                    let id = task["id"].as_str().unwrap_or("");
                    match host.result(id) {
                        Ok(result) => check_outcome(definition, Some(id), &result),
                        Err(error) => missing(definition, Some(id), &error),
                    }
                }
            })
            .collect();
        let mut guard = lock(&self.document);
        let document = self.loaded(&mut guard)?;
        let now = self.now(host);
        self.update(document, |draft| {
            let goal = find(draft, "goals", &goal_id).ok_or("Unknown goal")?;
            let mut reasons: Vec<Value> = checks.iter().filter_map(|check| check.get("reason").cloned()).collect();
            let status = if checks.is_empty() {
                reasons.push("Goal has no required checks".into());
                "unverified"
            } else if checks.iter().all(|check| check["passed"] == true) {
                "achieved"
            } else {
                "unachieved"
            };
            // A newer round supersedes this one; its record is the one that counts.
            if goal["verification"]["round"]
                .as_i64()
                .is_none_or(|recorded| recorded < round)
            {
                goal["verification"] =
                    json!({"round": round, "status": status, "at": now, "checks": checks, "reasons": reasons});
                if status == "achieved" {
                    goal["state"] = "achieved".into();
                }
            }
            Ok(goal.clone())
        })
    }
}

impl crate::ApiModule for SchedulesModule {
    fn prefixes(&self) -> &[&str] {
        &["schedules.", "goals."]
    }

    fn start(&self, host: &dyn TaskHost) -> Result<(), String> {
        SchedulesModule::start(self, host)
    }

    fn handle(&self, request: &crate::Request<'_>) -> Result<Value, String> {
        SchedulesModule::handle(
            self,
            request.kind,
            request.payload,
            request.caller_task,
            request.host,
            request.signal,
        )
    }
}
