//! Accounting reconciliation of `ultron/reconcile.ts` (A24): a pure reconstruction from the durable documents the
//! host writes (task journal, usage ledger, progress and schedule module documents) of every task's admission and
//! calls, the unattributed and imported usage, a timeline, and the discrepancies between them; and the comparison of
//! an `agents.status` inspection with that record.

use crate::usage::number;
use serde_json::{Map, Value, json};

const MEASURED: [&str; 4] = ["inputTokens", "outputTokens", "totalTokens", "cost"];

/// The documents to reconcile (`ReconcileInput`); `None` where a document does not exist.
#[derive(Default)]
pub struct ReconcileInput<'a> {
    pub tasks: Option<&'a Value>,
    pub usage: Option<&'a Value>,
    pub progress: Option<&'a Value>,
    pub schedules: Option<&'a Value>,
    pub root_id: Option<&'a str>,
}

fn object<'a>(value: Option<&'a Value>, name: &str) -> Result<&'a Map<String, Value>, String> {
    value
        .and_then(Value::as_object)
        .ok_or_else(|| format!("{name} is not an object"))
}

fn list<'a>(value: Option<&'a Value>, name: &str) -> Result<Vec<&'a Map<String, Value>>, String> {
    let items = value
        .and_then(Value::as_array)
        .ok_or_else(|| format!("{name} is not an array"))?;
    items
        .iter()
        .enumerate()
        .map(|(index, item)| object(Some(item), &format!("{name}[{index}]")))
        .collect()
}

/// JavaScript `String(value)`.
fn js(value: Option<&Value>) -> String {
    match value {
        None => "undefined".into(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Number(n)) => match number(n.as_f64().unwrap_or(0.0)) {
            Value::Number(n) => n.to_string(),
            _ => "NaN".into(),
        },
        Some(Value::Array(items)) => items.iter().map(|v| js(Some(v))).collect::<Vec<_>>().join(","),
        Some(Value::Object(_)) => "[object Object]".into(),
        Some(other) => other.to_string(),
    }
}

fn num(value: Option<&Value>) -> Option<f64> {
    value.and_then(Value::as_f64).filter(|n| n.is_finite())
}

/// `ReconciledUsage`: counts and sums; a null sum means some call did not report it.
#[derive(Clone, Debug, PartialEq)]
pub struct Usage {
    pub calls: f64,
    pub task_calls: f64,
    pub model_calls: f64,
    pub wall_ms: f64,
    pub measured: [Option<f64>; 4],
    pub unknown_calls: f64,
}

impl Usage {
    fn empty() -> Self {
        Usage {
            calls: 0.0,
            task_calls: 0.0,
            model_calls: 0.0,
            wall_ms: 0.0,
            measured: [Some(0.0); 4],
            unknown_calls: 0.0,
        }
    }

    fn add(&mut self, call: &Map<String, Value>) -> Result<(), String> {
        let usage = object(call.get("usage"), "usage call measurement")?;
        self.calls += 1.0;
        if call.get("kind") == Some(&json!("task")) {
            self.task_calls += 1.0;
        }
        if call.get("kind") == Some(&json!("model")) {
            self.model_calls += 1.0;
        }
        self.wall_ms += num(usage.get("wallMs")).unwrap_or(0.0);
        let mut unknown = false;
        for (slot, field) in self.measured.iter_mut().zip(MEASURED) {
            let measured = num(usage.get(field));
            unknown |= measured.is_none();
            *slot = match (*slot, measured) {
                (Some(total), Some(measured)) => Some(total + measured),
                _ => None,
            };
        }
        if unknown {
            self.unknown_calls += 1.0;
        }
        Ok(())
    }

    fn merge(&mut self, source: &Usage) {
        self.calls += source.calls;
        self.task_calls += source.task_calls;
        self.model_calls += source.model_calls;
        self.wall_ms += source.wall_ms;
        self.unknown_calls += source.unknown_calls;
        for (slot, other) in self.measured.iter_mut().zip(source.measured) {
            *slot = match (*slot, other) {
                (Some(a), Some(b)) => Some(a + b),
                _ => None,
            };
        }
    }

    pub fn to_json(&self) -> Value {
        let known = |n: Option<f64>| n.map_or(Value::Null, number);
        json!({
            "calls": number(self.calls), "taskCalls": number(self.task_calls), "modelCalls": number(self.model_calls),
            "wallMs": number(self.wall_ms), "inputTokens": known(self.measured[0]),
            "outputTokens": known(self.measured[1]), "totalTokens": known(self.measured[2]),
            "cost": known(self.measured[3]), "unknownCalls": number(self.unknown_calls),
        })
    }

    fn history(stored: &Map<String, Value>) -> Usage {
        let count = |key: &str| num(stored.get(key)).unwrap_or(0.0);
        Usage {
            calls: count("calls"),
            task_calls: count("taskCalls"),
            model_calls: count("modelCalls"),
            wall_ms: count("wallMs"),
            measured: MEASURED.map(|field| num(stored.get(field))),
            unknown_calls: count("unknownCalls"),
        }
    }
}

struct Task {
    id: String,
    key: String,
    definition: String,
    state: String,
    result_status: Option<String>,
    parent_id: Option<String>,
    admission: String,
    admitted_at: Option<f64>,
    settled_at: Option<f64>,
    usage: Usage,
    model_usage: Usage,
}

struct Event {
    at: f64,
    kind: &'static str,
    task_id: Option<String>,
    detail: String,
}

fn expected_admission(state: &str) -> Option<&'static str> {
    match state {
        "admitted" | "running" => Some("active"),
        "completed" => Some("succeeded"),
        "failed" => Some("failed"),
        "cancelled" => Some("cancelled"),
        "interrupted" => Some("unknown"),
        _ => None,
    }
}

fn text(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(String::from)
}

/// `reconcileRecords(input)`, as the TypeScript `Reconciliation` object.
pub fn reconcile_records(input: &ReconcileInput<'_>) -> Result<Value, String> {
    let mut discrepancies: Vec<String> = Vec::new();
    let mut timeline: Vec<Event> = Vec::new();
    let journal = match input.tasks {
        None => Vec::new(),
        Some(tasks) => list(object(Some(tasks), "task journal")?.get("tasks"), "tasks")?,
    };
    let empty = Map::new();
    let roots = match input.usage {
        None => &empty,
        Some(usage) => object(object(Some(usage), "usage ledger")?.get("roots"), "usage roots")?,
    };
    let root_ids: Vec<String> = match input.root_id {
        None => roots.keys().cloned().collect(),
        Some(root) => vec![root.into()],
    };
    let mut calls: Vec<&Map<String, Value>> = Vec::new();
    let mut reservations: Vec<&Map<String, Value>> = Vec::new();
    let mut per_root = Map::new();
    let mut imported = Usage::empty();
    for root_id in &root_ids {
        let Some(root) = roots.get(root_id) else { continue };
        let root = object(Some(root), &format!("usage root {root_id}"))?;
        let root_calls = list(root.get("calls"), "usage calls")?;
        let root_reservations = list(root.get("reservations"), "usage reservations")?;
        let mut usage = Usage::empty();
        for call in &root_calls {
            usage.add(call)?;
        }
        let active = root_reservations.len();
        reservations.extend(root_reservations);
        if root.contains_key("imported") {
            imported.merge(&usage);
        } else {
            calls.extend(root_calls);
        }
        per_root.insert(
            root_id.clone(),
            json!({ "usage": usage.to_json(), "activeReservations": active }),
        );
    }
    let stored_history = match (input.usage, input.root_id) {
        (Some(usage), None) => object(Some(usage), "usage ledger")?
            .get("history")
            .filter(|h| !h.is_null()),
        _ => None,
    };
    let history = match stored_history {
        None => None,
        Some(history) => Some(Usage::history(object(
            object(Some(history), "usage history")?.get("usage"),
            "usage history totals",
        )?)),
    };
    let mut folded: Vec<(String, f64)> = match stored_history {
        None => Vec::new(),
        Some(history) => object(
            object(Some(history), "usage history")?.get("taskAdmissions"),
            "folded admissions",
        )?
        .iter()
        .map(|(status, count)| (status.clone(), num(Some(count)).unwrap_or(0.0)))
        .collect(),
    };

    let mut tasks: Vec<Task> = Vec::new();
    for stored in &journal {
        let result = match stored.get("result") {
            None => None,
            Some(result) => Some(object(Some(result), "task result")?),
        };
        let task = Task {
            id: js(stored.get("id")),
            key: js(stored.get("key")),
            definition: js(stored.get("definition")),
            state: js(stored.get("state")),
            result_status: result.and_then(|r| text(r.get("status"))),
            parent_id: text(stored.get("parentId")),
            admission: "missing".into(),
            admitted_at: None,
            settled_at: None,
            usage: Usage::empty(),
            model_usage: Usage::empty(),
        };
        // A later record with the same id replaces the earlier one in its place (Map.set).
        match tasks.iter().position(|t| t.id == task.id) {
            Some(at) => tasks[at] = task,
            None => tasks.push(task),
        }
    }
    let by_key = |tasks: &[Task], key: &str| tasks.iter().rposition(|t| t.key == key);
    let by_id = |tasks: &[Task], id: &str| tasks.iter().position(|t| t.id == id);

    let mut unattributed = Usage::empty();
    let mut task_calls: Vec<(String, Vec<&Map<String, Value>>)> = Vec::new();
    for call in &calls {
        let admitted_at = num(call.get("admittedAt")).unwrap_or(0.0);
        let settled_at = num(call.get("settledAt")).unwrap_or(admitted_at);
        let status = js(call.get("status"));
        match call.get("kind").and_then(Value::as_str) {
            Some("task") => {
                let key = js(call.get("requestKey"));
                let Some(index) = by_key(&tasks, &key) else {
                    unattributed.add(call)?;
                    timeline.push(Event {
                        at: settled_at,
                        kind: "usage.unattributed",
                        task_id: None,
                        detail: format!("admission {key} created no task ({status})"),
                    });
                    continue;
                };
                let id = tasks[index].id.clone();
                match task_calls.iter_mut().find(|(task, _)| *task == id) {
                    Some((_, list)) => list.push(call),
                    None => task_calls.push((id, vec![call])),
                }
                tasks[index].usage.add(call)?;
            }
            Some("model") => {
                let task_id = js(call.get("taskId"));
                let Some(index) = by_id(&tasks, &task_id) else {
                    discrepancies.push(format!(
                        "model call {} names unknown task {task_id}",
                        js(call.get("id"))
                    ));
                    unattributed.add(call)?;
                    continue;
                };
                tasks[index].usage.add(call)?;
                tasks[index].model_usage.add(call)?;
                timeline.push(Event {
                    at: settled_at,
                    kind: "model.call",
                    task_id: Some(tasks[index].id.clone()),
                    detail: status,
                });
            }
            _ => {
                unattributed.add(call)?;
                timeline.push(Event {
                    at: settled_at,
                    kind: "usage.unattributed",
                    task_id: None,
                    detail: format!("{} call ({status})", js(call.get("kind"))),
                });
            }
        }
    }

    let mut active_by_key: Vec<(String, Vec<&Map<String, Value>>)> = Vec::new();
    for reservation in &reservations {
        if reservation.get("kind") != Some(&json!("task")) {
            continue;
        }
        let key = js(reservation.get("requestKey"));
        match active_by_key.iter_mut().find(|(k, _)| *k == key) {
            Some((_, list)) => list.push(reservation),
            None => active_by_key.push((key, vec![reservation])),
        }
    }

    let ids: Vec<String> = tasks.iter().map(|t| t.id.clone()).collect();
    for task in &mut tasks {
        let settled = task_calls
            .iter()
            .find(|(id, _)| *id == task.id)
            .map(|(_, list)| list.clone())
            .unwrap_or_default();
        let active = match active_by_key.iter().position(|(key, _)| *key == task.key) {
            Some(at) => active_by_key.remove(at).1,
            None => Vec::new(),
        };
        if settled.len() + active.len() > 1 {
            discrepancies.push(format!(
                "task {} has {} admission records",
                task.id,
                settled.len() + active.len()
            ));
        }
        if settled.len() == 1 {
            task.admission = js(settled[0].get("status"));
            task.admitted_at = num(settled[0].get("admittedAt"));
            task.settled_at = num(settled[0].get("settledAt"));
        } else if active.len() == 1 {
            task.admission = "active".into();
            task.admitted_at = num(active[0].get("admittedAt"));
        }
        let expected = expected_admission(&task.state);
        let spare = expected.and_then(|e| folded.iter_mut().find(|(status, n)| status == e && *n > 0.0));
        if task.admission == "missing"
            && let Some((_, count)) = spare
        {
            *count -= 1.0;
            task.admission = "folded".into();
        } else if Some(task.admission.as_str()) != expected {
            discrepancies.push(format!(
                "task {} is {} but its admission is {}",
                task.id, task.state, task.admission
            ));
        }
        if let Some(at) = task.admitted_at {
            timeline.push(Event {
                at,
                kind: "task.admitted",
                task_id: Some(task.id.clone()),
                detail: task.definition.clone(),
            });
        }
        if let Some(at) = task.settled_at {
            let detail = match &task.result_status {
                None => task.state.clone(),
                Some(status) => format!("{}/{status}", task.state),
            };
            timeline.push(Event {
                at,
                kind: "task.settled",
                task_id: Some(task.id.clone()),
                detail,
            });
        }
        if let Some(parent) = &task.parent_id
            && !ids.contains(parent)
        {
            discrepancies.push(format!("task {} names unknown parent {parent}", task.id));
        }
    }
    for (key, _) in &active_by_key {
        discrepancies.push(format!("active admission {key} has no journal task"));
    }
    let known = |id: &str| ids.iter().any(|known| known == id);
    let status_of = |id: &str| tasks.iter().find(|t| t.id == id).map(|t| t.result_status.clone());

    if let Some(progress) = input.progress {
        let progress = object(Some(progress), "progress document")?;
        for receipt in list(progress.get("receipts"), "progress receipts")? {
            let task_id = js(receipt.get("task_id"));
            if !known(&task_id) {
                discrepancies.push(format!("progress receipt {} names unknown task", js(receipt.get("id"))));
            }
            timeline.push(Event {
                at: num(receipt.get("at")).unwrap_or(0.0),
                kind: "progress.receipt",
                task_id: Some(task_id),
                detail: js(receipt.get("summary")),
            });
        }
        for decision in list(progress.get("decisions"), "progress decisions")? {
            let task_id = js(decision.get("task_id"));
            let id = js(decision.get("id"));
            if !known(&task_id) {
                discrepancies.push(format!("progress decision {id} names unknown task"));
            }
            let claim = match decision.get("claim") {
                None => None,
                Some(claim) => Some(object(Some(claim), "claim")?),
            };
            if let Some(claim) = claim
                && let Some(verifier) = claim.get("verifier_task_id")
            {
                let verifier_status = status_of(&js(Some(verifier)));
                match &verifier_status {
                    None => discrepancies.push(format!("decision {id} names an unknown verifier task")),
                    Some(status) => {
                        if let Some(recorded) = claim.get("verifier_status")
                            && recorded.as_str() != status.as_deref()
                        {
                            discrepancies.push(format!(
                                "decision {id} records verifier {}, journal has {}",
                                js(Some(recorded)),
                                status.as_deref().unwrap_or("null")
                            ));
                        }
                    }
                }
                if claim.get("decision") == Some(&json!("verified"))
                    && verifier_status.flatten().as_deref() != Some("succeeded")
                {
                    discrepancies.push(format!("decision {id} is verified without a succeeded verifier"));
                }
            }
            let classification = js(object(decision.get("assessment"), "assessment")?.get("classification"));
            timeline.push(Event {
                at: num(decision.get("at")).unwrap_or(0.0),
                kind: "progress.decision",
                task_id: Some(task_id),
                detail: match claim {
                    Some(claim) => format!("{classification}/{}", js(claim.get("decision"))),
                    None => classification,
                },
            });
        }
    }

    if let Some(schedules) = input.schedules {
        let schedules = object(Some(schedules), "schedule document")?;
        for goal in list(schedules.get("goals"), "goals")? {
            let goal_id = js(goal.get("id"));
            for task_id in goal.get("task_ids").and_then(Value::as_array).into_iter().flatten() {
                let task_id = js(Some(task_id));
                if !known(&task_id) {
                    discrepancies.push(format!("goal {goal_id} names unknown task {task_id}"));
                }
            }
            let Some(verification) = goal.get("verification").filter(|v| !v.is_null()) else {
                continue;
            };
            let verification = object(Some(verification), "goal verification")?;
            for check in list(verification.get("checks"), "goal checks")? {
                if check.get("task_id") == Some(&Value::Null) {
                    continue;
                }
                match status_of(&js(check.get("task_id"))) {
                    None => discrepancies.push(format!("goal {goal_id} check names unknown task")),
                    Some(status) => {
                        let equal = match (check.get("status"), &status) {
                            (Some(Value::String(recorded)), Some(journal)) => recorded == journal,
                            (Some(Value::Null), None) => true,
                            _ => false,
                        };
                        if !equal {
                            discrepancies.push(format!(
                                "goal {goal_id} check {} records {}, journal has {}",
                                js(check.get("definition")),
                                js(check.get("status")),
                                status.as_deref().unwrap_or("null")
                            ));
                        }
                    }
                }
            }
            if verification.get("status") == Some(&json!("achieved")) && goal.get("state") != Some(&json!("achieved")) {
                discrepancies.push(format!(
                    "goal {goal_id} verified achieved but is {}",
                    js(goal.get("state"))
                ));
            }
            timeline.push(Event {
                at: num(verification.get("at")).unwrap_or(0.0),
                kind: "goal.verification",
                task_id: None,
                detail: format!("{goal_id}:{}", js(verification.get("status"))),
            });
        }
        for schedule in list(schedules.get("schedules"), "schedules")? {
            let Some(last) = schedule.get("last_task_id").filter(|v| !v.is_null()) else {
                continue;
            };
            let last = js(Some(last));
            if !known(&last) {
                discrepancies.push(format!("schedule {} names unknown task", js(schedule.get("id"))));
            }
            timeline.push(Event {
                at: num(schedule.get("last_fired_at")).unwrap_or(0.0),
                kind: "schedule.fired",
                task_id: Some(last),
                detail: js(schedule.get("id")),
            });
        }
    }

    let mut totals = Usage::empty();
    for task in &tasks {
        totals.merge(&task.usage);
    }
    totals.merge(&unattributed);
    totals.merge(&imported);
    let mut session = totals.clone();
    if let Some(history) = &history {
        session.merge(history);
    }
    // Stable: equal timestamps keep journal/call order.
    timeline.sort_by(|a, b| a.at.partial_cmp(&b.at).unwrap_or(std::cmp::Ordering::Equal));
    let tasks: Vec<Value> = tasks
        .iter()
        .map(|t| {
            json!({
                "id": t.id, "key": t.key, "definition": t.definition, "state": t.state,
                "resultStatus": t.result_status, "parentId": t.parent_id, "admission": t.admission,
                "admittedAt": t.admitted_at.map(number), "settledAt": t.settled_at.map(number),
                "usage": t.usage.to_json(), "modelUsage": t.model_usage.to_json(),
            })
        })
        .collect();
    let timeline: Vec<Value> = timeline
        .into_iter()
        .map(|e| json!({ "at": number(e.at), "kind": e.kind, "taskId": e.task_id, "detail": e.detail }))
        .collect();
    Ok(json!({
        "tasks": tasks,
        "unattributed": unattributed.to_json(),
        "imported": imported.to_json(),
        "totals": totals.to_json(),
        "activeReservations": reservations.len(),
        "scope": input.root_id,
        "roots": per_root,
        "history": history.map_or(Value::Null, |h| h.to_json()),
        "sessionTotals": session.to_json(),
        "timeline": timeline,
        "discrepancies": discrepancies,
    }))
}

fn usage_discrepancies(label: &str, expected: &Value, actual: Option<&Value>) -> Result<Vec<String>, String> {
    let actual = object(actual, &format!("{label} totals"))?;
    let mut problems = Vec::new();
    for (field, want) in expected.as_object().into_iter().flatten() {
        let got = actual.get(field);
        let equal = match (want.as_f64(), got.and_then(Value::as_f64)) {
            (Some(want), Some(got)) => (want - got).abs() <= 1e-9 * want.abs().max(1.0),
            _ => Some(want) == got,
        };
        if !equal {
            problems.push(format!("{label} {field} {} != records {}", js(got), js(Some(want))));
        }
    }
    Ok(problems)
}

/// `inspectionDiscrepancies(reconciliation, status)`: where `agents.status` disagrees with the records.
pub fn inspection_discrepancies(reconciliation: &Value, status: &Value) -> Result<Vec<String>, String> {
    let view = object(Some(status), "agents.status")?;
    let mut discrepancies = Vec::new();
    let viewed = list(view.get("tasks"), "agents.status tasks")?;
    let tasks = reconciliation["tasks"].as_array().cloned().unwrap_or_default();
    if viewed.len() != tasks.len() {
        discrepancies.push(format!(
            "inspection lists {} tasks, journal has {}",
            viewed.len(),
            tasks.len()
        ));
    }
    for item in viewed {
        let id = js(item.get("id"));
        let Some(task) = tasks.iter().find(|t| t["id"] == id.as_str()) else {
            discrepancies.push(format!("inspection shows unknown task {id}"));
            continue;
        };
        let result = match item.get("result") {
            None => None,
            Some(result) => Some(object(Some(result), "inspected result")?),
        };
        if item.get("state") != Some(&task["state"]) {
            discrepancies.push(format!("task {id} inspected {}", js(item.get("state"))));
        }
        let status = result.and_then(|r| r.get("status")).and_then(Value::as_str);
        if status != task["resultStatus"].as_str() {
            discrepancies.push(format!(
                "task {id} inspected result {}",
                js(result.and_then(|r| r.get("status")))
            ));
        }
        if item.get("parentId").and_then(Value::as_str) != task["parentId"].as_str() {
            discrepancies.push(format!("task {id} inspected parent {}", js(item.get("parentId"))));
        }
    }
    if let Some(inspected) = view.get("usage").filter(|u| !u.is_null()) {
        let inspected = object(Some(inspected), "inspected usage")?;
        let scoped = match inspected.get("rootId").and_then(Value::as_str) {
            None => {
                json!({ "usage": reconciliation["totals"], "activeReservations": reconciliation["activeReservations"] })
            }
            Some(root) => reconciliation["roots"]
                .get(root)
                .cloned()
                .unwrap_or_else(|| json!({ "usage": Usage::empty().to_json(), "activeReservations": 0 })),
        };
        discrepancies.extend(usage_discrepancies(
            "inspected usage",
            &scoped["usage"],
            inspected.get("usage"),
        )?);
        if inspected.get("activeReservations").and_then(Value::as_f64) != scoped["activeReservations"].as_f64() {
            discrepancies.push(format!(
                "inspected {} active reservations, records have {}",
                js(inspected.get("activeReservations")),
                js(Some(&scoped["activeReservations"]))
            ));
        }
        if let Some(session) = inspected.get("session")
            && reconciliation["scope"].is_null()
        {
            discrepancies.extend(usage_discrepancies(
                "inspected session usage",
                &reconciliation["sessionTotals"],
                object(Some(session), "inspected session")?.get("usage"),
            )?);
        }
    }
    Ok(discrepancies)
}
