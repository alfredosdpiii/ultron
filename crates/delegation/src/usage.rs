//! The usage ledger of `ultron/usage.ts` (`NativeUsageLedger`): reservations and settled calls per usage root (a
//! root turn and every task it admitted), admission against the admitted-task cap and the root wall deadline (a
//! child's deadline is capped by what remains of its root's), the per-tree turn, token and cost limits, unknown usage
//! kept unknown, crash reconciliation of open reservations, and folding of idle old roots into a bounded history.
//!
//! The document is the TS one (`ultron.usage/root`), with its key order, so either implementation reads the other's.
//! Every change is one serialized step that writes the whole document; a failed write poisons the ledger.

use crate::task_store::{MemoryStore, TaskStore};
use serde_json::{Map, Value, json};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// Kinds a stored ledger may hold (`jev` calls of older sessions still count as calls).
const STORED_KINDS: [&str; 3] = ["task", "model", "jev"];
const STATUSES: [&str; 4] = ["succeeded", "failed", "cancelled", "unknown"];
pub const DEFAULT_ROOT_ID: &str = "ultron-root";
pub const DEFAULT_DETAILED_ROOTS: usize = 50;
/// Tokens a root turn's tree may use when no token cap is set.
pub const DEFAULT_MAX_TOTAL_TOKENS: u64 = 10_000_000;
/// Usage roots of session goals: no wall deadline, no admitted-task cap, exempt from the default token cap.
pub const GOAL_USAGE_ROOT_PREFIX: &str = "goal:";
/// Worker defaults: 24 unfinished tasks and a 30-minute wall budget per root.
pub const DEFAULT_MAX_ADMITTED_TASKS: u64 = 24;
pub const DEFAULT_MAX_WALL_MS: u64 = 30 * 60 * 1000;
const DURABILITY: &str = "Usage ledger durability is uncertain; reopen the owner";

/// `NativeUsageLimits` (`None`: no limit).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UsageLimits {
    pub max_admitted_tasks: Option<u64>,
    pub max_wall_ms: Option<u64>,
    pub max_cost_usd: Option<f64>,
    pub max_total_tokens: Option<u64>,
    pub max_total_turns: Option<u64>,
    pub default_max_total_tokens: Option<u64>,
}

/// `nativeUsageLimitsFromEnv(env, settings)`: `settings` is `rootBudget` ({maxTotalTokens?, maxTotalTurns?}).
pub fn limits_from_env(var: &dyn Fn(&str) -> Option<String>, settings: &Value) -> UsageLimits {
    let unlimited = |raw: &str| ["none", "off", "unlimited"].contains(&raw.to_lowercase().as_str());
    // Some(Some(n)): a value; Some(None): turned off; the fallback otherwise.
    let integer = |name: &str, fallback: u64, minimum: u64| -> Option<u64> {
        let text = var(name).map(|t| t.trim().to_string()).unwrap_or_default();
        if text.is_empty() {
            return Some(fallback);
        }
        if unlimited(&text) {
            return None;
        }
        match text.parse::<f64>() {
            Ok(n) if n.fract() == 0.0 && n >= minimum as f64 && n <= 9_007_199_254_740_991.0 => Some(n as u64),
            _ => Some(fallback),
        }
    };
    let mut limits = UsageLimits {
        max_admitted_tasks: integer("ULTRON_MAX_ADMITTED_TASKS", DEFAULT_MAX_ADMITTED_TASKS, 0),
        max_wall_ms: integer("ULTRON_MAX_WALL_MS", DEFAULT_MAX_WALL_MS, 1),
        ..UsageLimits::default()
    };
    let cost = var("ULTRON_MAX_COST_USD")
        .map(|t| t.trim().trim_start_matches('$').to_string())
        .filter(|t| !t.is_empty())
        .and_then(|t| t.parse::<f64>().ok());
    if let Some(cost) = cost.filter(|c| c.is_finite() && *c >= 0.0) {
        limits.max_cost_usd = Some(cost);
    }
    let setting = |key: &str| {
        settings
            .get(key)
            .and_then(Value::as_f64)
            .filter(|n| n.fract() == 0.0 && *n >= 1.0)
            .map(|n| n as u64)
    };
    let tokens = integer("ULTRON_MAX_TOTAL_TOKENS", setting("maxTotalTokens").unwrap_or(0), 1);
    let turns = integer("ULTRON_MAX_TOTAL_TURNS", setting("maxTotalTurns").unwrap_or(0), 1);
    match tokens {
        Some(0) => limits.default_max_total_tokens = Some(DEFAULT_MAX_TOTAL_TOKENS),
        Some(n) => limits.max_total_tokens = Some(n),
        None => {}
    }
    if let Some(n) = turns.filter(|n| *n > 0) {
        limits.max_total_turns = Some(n);
    }
    limits
}

/// `budgetLine(remaining, explicitTokenCap)`: the budget note on a cell result, if any.
pub fn budget_line(remaining: &Remaining, explicit_token_cap: bool) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(turns) = remaining.turns_left {
        parts.push(format!("{turns} model turns"));
    }
    if let (Some(tokens), Some(cap)) = (remaining.tokens_left, remaining.token_cap)
        && (explicit_token_cap || tokens as f64 <= cap as f64 / 2.0)
    {
        parts.push(format!("{} tokens", grouped(tokens)));
    }
    (!parts.is_empty()).then(|| format!("[budget left for this request: {}]", parts.join(", ")))
}

/// `toLocaleString("en-US")` of a nonnegative integer.
fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// What a root has left under its turn and token caps (`None`: no cap).
#[derive(Clone, Debug, PartialEq)]
pub struct Remaining {
    pub turns_left: Option<u64>,
    pub tokens_left: Option<u64>,
    pub token_cap: Option<u64>,
}

/// Which limit a spent root reached, with its error text.
#[derive(Clone, Debug, PartialEq)]
pub struct Exhaustion {
    pub reason: String,
    /// "turns" | "tokens" | "cost".
    pub kind: &'static str,
}

/// `NativeUsageReservationRequest`.
#[derive(Clone, Debug, Default)]
pub struct ReserveRequest {
    /// "task" | "model".
    pub kind: String,
    pub root_id: Option<String>,
    pub task_id: Option<String>,
    pub parent_task_id: Option<String>,
    pub request_key: Option<String>,
    pub budget_id: Option<String>,
    /// A positive safe integer when given.
    pub timeout_ms: Option<f64>,
    pub deadline_at: Option<f64>,
    /// Whether admitted work calls a model (the cost cap applies); tasks default to true.
    pub model_backed: Option<bool>,
}

impl ReserveRequest {
    pub fn task(request_key: impl Into<String>) -> Self {
        ReserveRequest {
            kind: "task".into(),
            request_key: Some(request_key.into()),
            ..ReserveRequest::default()
        }
    }
    pub fn model(request_key: impl Into<String>) -> Self {
        ReserveRequest {
            kind: "model".into(),
            request_key: Some(request_key.into()),
            ..ReserveRequest::default()
        }
    }
}

/// A reservation as stored (`NativeUsageReservation`).
#[derive(Clone, Debug, PartialEq)]
pub struct Reservation(pub Value);

impl Reservation {
    pub fn id(&self) -> &str {
        self.0["id"].as_str().unwrap_or_default()
    }
    pub fn root_id(&self) -> &str {
        self.0["rootId"].as_str().unwrap_or_default()
    }
    pub fn admitted_at(&self) -> f64 {
        self.0["admittedAt"].as_f64().unwrap_or(0.0)
    }
    pub fn deadline_at(&self) -> Option<f64> {
        self.0["deadlineAt"].as_f64()
    }
}

/// Provider-reported usage of a call (`NativeUsageMeasurement`); `None` fields stay unknown.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Measurement {
    pub input_tokens: Option<f64>,
    pub output_tokens: Option<f64>,
    pub total_tokens: Option<f64>,
    pub cost: Option<f64>,
    pub wall_ms: Option<f64>,
}

/// A JSON number as JavaScript writes it: integral values without a fraction.
pub fn number(n: f64) -> Value {
    if n.fract() == 0.0 && n.abs() < 9_007_199_254_740_992.0 {
        Value::from(n as i64)
    } else {
        serde_json::Number::from_f64(n).map_or(Value::Null, Value::Number)
    }
}

fn known(n: Option<f64>) -> Value {
    n.map_or(Value::Null, number)
}

/// Told when a reservation is refused because a limit was reached.
pub type Refused = Arc<dyn Fn(&str) + Send + Sync>;

/// The clock (`Date.now()`), injected so fixtures can fix it.
pub type Clock = Arc<dyn Fn() -> f64 + Send + Sync>;
/// Mints ids for reservations and calls (`crypto.randomUUID()`).
pub type IdSource = Box<dyn FnMut() -> String + Send>;

pub struct UsageOptions {
    pub limits: UsageLimits,
    pub root_id: Option<String>,
    pub now: Clock,
    pub detailed_roots: usize,
    /// Told when a reservation is refused because a limit was reached.
    pub on_refused: Option<Refused>,
    pub ids: IdSource,
}

impl Default for UsageOptions {
    fn default() -> Self {
        UsageOptions {
            limits: UsageLimits::default(),
            root_id: None,
            now: Arc::new(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as f64)
                    .unwrap_or(0.0)
            }),
            detailed_roots: DEFAULT_DETAILED_ROOTS,
            on_refused: None,
            ids: Box::new(crate::task_store::uuid_v4),
        }
    }
}

struct State {
    store: Box<dyn TaskStore>,
    document: Value,
    loaded: Option<Result<(), String>>,
    broken: bool,
    ids: IdSource,
}

/// `NativeUsageLedger`; clones share it.
#[derive(Clone)]
pub struct UsageLedger {
    state: Arc<Mutex<State>>,
    limits: UsageLimits,
    root_id: String,
    now: Clock,
    detailed_roots: usize,
    on_refused: Option<Refused>,
}

fn empty_document() -> Value {
    json!({ "version": 1, "roots": {} })
}

fn empty_root(root_id: &str) -> Value {
    json!({ "rootId": root_id, "startedAt": null, "deadlineAt": null, "reservations": [], "calls": [] })
}

fn invalid(what: &str) -> String {
    format!("Invalid usage ledger {what}")
}

fn object(value: &Value) -> Result<&Map<String, Value>, String> {
    value
        .as_object()
        .ok_or_else(|| "Invalid usage ledger document".to_string())
}

fn finite(value: &Value, name: &str) -> Result<f64, String> {
    value.as_f64().filter(|n| n.is_finite()).ok_or_else(|| invalid(name))
}

fn finite_integer(value: &Value, name: &str, minimum: f64) -> Result<f64, String> {
    value
        .as_f64()
        .filter(|n| n.fract() == 0.0 && n.abs() <= 9_007_199_254_740_991.0 && *n >= minimum)
        .ok_or_else(|| invalid(name))
}

fn optional_finite(value: &Value, name: &str) -> Result<(), String> {
    if value.is_null() {
        Ok(())
    } else {
        finite(value, name).map(|_| ())
    }
}

fn optional_nonempty(item: &Map<String, Value>, key: &str, error: &str) -> Result<(), String> {
    match item.get(key) {
        None => Ok(()),
        Some(Value::String(text)) if !text.is_empty() => Ok(()),
        Some(_) => Err(error.into()),
    }
}

fn validate_measurement(value: &Value) -> Result<(), String> {
    let item = object(value)?;
    for key in ["inputTokens", "outputTokens", "totalTokens", "cost", "wallMs"] {
        if !item.contains_key(key) {
            return Err(invalid("measurement"));
        }
    }
    for key in ["inputTokens", "outputTokens", "totalTokens", "cost"] {
        optional_finite(&item[key], key)?;
    }
    if finite(&item["wallMs"], "wallMs")? < 0.0 {
        return Err(invalid("wallMs"));
    }
    Ok(())
}

fn validate_reservation(value: &Value) -> Result<(), String> {
    let item = object(value)?;
    let nonempty = |key: &str| item.get(key).and_then(Value::as_str).is_some_and(|s| !s.is_empty());
    if !nonempty("id") || !nonempty("rootId") {
        return Err(invalid("reservation"));
    }
    if !item
        .get("kind")
        .and_then(Value::as_str)
        .is_some_and(|k| STORED_KINDS.contains(&k))
    {
        return Err(invalid("reservation kind"));
    }
    optional_nonempty(item, "taskId", "Invalid usage ledger task ID")?;
    optional_nonempty(item, "parentTaskId", "Invalid usage ledger parent task ID")?;
    optional_nonempty(item, "requestKey", "Invalid usage ledger request key")?;
    optional_nonempty(item, "budgetId", "Invalid usage ledger budget ID")?;
    if finite(item.get("admittedAt").unwrap_or(&Value::Null), "admittedAt")? < 0.0 {
        return Err(invalid("admittedAt"));
    }
    optional_finite(item.get("deadlineAt").unwrap_or(&Value::Bool(false)), "deadlineAt")
}

fn validate_call(value: &Value) -> Result<(), String> {
    let item = object(value)?;
    let nonempty = |key: &str| item.get(key).and_then(Value::as_str).is_some_and(|s| !s.is_empty());
    if !nonempty("id") || !nonempty("reservationId") {
        return Err(invalid("call"));
    }
    if !nonempty("rootId") {
        return Err(invalid("call root ID"));
    }
    if !item
        .get("kind")
        .and_then(Value::as_str)
        .is_some_and(|k| STORED_KINDS.contains(&k))
    {
        return Err(invalid("call kind"));
    }
    optional_nonempty(item, "requestKey", "Invalid usage ledger call request key")?;
    optional_nonempty(item, "budgetId", "Invalid usage ledger call budget ID")?;
    optional_nonempty(item, "taskId", "Invalid usage ledger call task ID")?;
    optional_nonempty(item, "parentTaskId", "Invalid usage ledger call task ID")?;
    finite(item.get("admittedAt").unwrap_or(&Value::Null), "admittedAt")?;
    finite(item.get("settledAt").unwrap_or(&Value::Null), "settledAt")?;
    if !item
        .get("status")
        .and_then(Value::as_str)
        .is_some_and(|s| STATUSES.contains(&s))
    {
        return Err(invalid("call status"));
    }
    validate_measurement(item.get("usage").unwrap_or(&Value::Null))
}

fn validate_totals(value: &Value) -> Result<(), String> {
    let item = object(value)?;
    for name in ["calls", "taskCalls", "modelCalls", "unknownCalls"] {
        finite_integer(item.get(name).unwrap_or(&Value::Null), &format!("history {name}"), 0.0)?;
    }
    if finite(item.get("wallMs").unwrap_or(&Value::Null), "history wallMs")? < 0.0 {
        return Err(invalid("history wallMs"));
    }
    for name in ["inputTokens", "outputTokens", "totalTokens", "cost"] {
        let Some(value) = item.get(name) else {
            return Err(invalid("history totals"));
        };
        optional_finite(value, &format!("history {name}"))?;
    }
    Ok(())
}

fn validate_history(value: &Value) -> Result<(), String> {
    let item = object(value)?;
    let field = |name: &str| item.get(name).unwrap_or(&Value::Null);
    finite_integer(field("roots"), "history roots", 0.0)?;
    finite_integer(field("importedRoots"), "history importedRoots", 0.0)?;
    optional_finite(
        item.get("firstStartedAt").unwrap_or(&Value::Bool(false)),
        "history firstStartedAt",
    )?;
    optional_finite(
        item.get("lastSettledAt").unwrap_or(&Value::Bool(false)),
        "history lastSettledAt",
    )?;
    validate_totals(field("usage"))?;
    let by_kind = object(field("byKind"))?;
    for kind in ["task", "model"] {
        validate_totals(by_kind.get(kind).unwrap_or(&Value::Null))?;
    }
    finite(field("spentUsd"), "history spentUsd")?;
    finite_integer(field("unknownPricedCalls"), "history unknownPricedCalls", 0.0)?;
    let admissions = object(field("taskAdmissions"))?;
    for status in STATUSES {
        finite_integer(
            admissions.get(status).unwrap_or(&Value::Null),
            "history taskAdmissions",
            0.0,
        )?;
    }
    Ok(())
}

/// `validateDocument`.
fn validate_document(value: &Value) -> Result<(), String> {
    let document = object(value)?;
    if document.get("version").and_then(Value::as_f64) != Some(1.0) {
        return Err("Unsupported usage ledger version".into());
    }
    if let Some(history) = document.get("history") {
        validate_history(history)?;
    }
    let roots = object(document.get("roots").unwrap_or(&Value::Null))?;
    for (root_id, root) in roots {
        let root = object(root)?;
        if root.get("rootId").and_then(Value::as_str) != Some(root_id.as_str()) || root_id.is_empty() {
            return Err(invalid("root"));
        }
        optional_finite(root.get("startedAt").unwrap_or(&Value::Bool(false)), "startedAt")?;
        optional_finite(root.get("deadlineAt").unwrap_or(&Value::Bool(false)), "deadlineAt")?;
        let (Some(reservations), Some(calls)) = (
            root.get("reservations").and_then(Value::as_array),
            root.get("calls").and_then(Value::as_array),
        ) else {
            return Err(invalid("root records"));
        };
        if let Some(turns) = root.get("turns") {
            let turns = object(turns)?;
            let field = |name: &str| turns.get(name).unwrap_or(&Value::Null);
            finite_integer(field("count"), "turn count", 0.0)?;
            if finite(field("tokens"), "turn tokens")? < 0.0 {
                return Err(invalid("turn tokens"));
            }
            finite(field("lastAt"), "turn lastAt")?;
            if let Some(cost) = turns.get("cost")
                && finite(cost, "turn cost")? < 0.0
            {
                return Err(invalid("turn cost"));
            }
            if let Some(unpriced) = turns.get("unpriced") {
                finite_integer(unpriced, "turn unpriced", 0.0)?;
            }
        }
        if let Some(imported) = root.get("imported") {
            let imported = object(imported)?;
            if !imported
                .get("source")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty())
            {
                return Err(invalid("import"));
            }
            finite_integer(imported.get("entries").unwrap_or(&Value::Null), "import entries", 0.0)?;
        }
        let mut ids: Vec<&str> = Vec::new();
        for reservation in reservations {
            validate_reservation(reservation)?;
            let id = reservation["id"].as_str().unwrap_or_default();
            if ids.contains(&id) {
                return Err("Duplicate usage ledger reservation".into());
            }
            ids.push(id);
        }
        let mut ids: Vec<&str> = Vec::new();
        for call in calls {
            validate_call(call)?;
            let id = call["id"].as_str().unwrap_or_default();
            if ids.contains(&id) {
                return Err("Duplicate usage ledger call".into());
            }
            ids.push(id);
        }
    }
    Ok(())
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

fn finite_limit(limit: Option<u64>) -> f64 {
    limit.map_or(f64::INFINITY, |n| n as f64)
}

/// Totals of settled calls (`NativeUsageTotals`); a null token or cost sum means a call did not report it.
pub fn totals(calls: &[Value]) -> Value {
    let count = |kind: &str| calls.iter().filter(|call| call["kind"] == kind).count();
    let sum = |key: &str| -> Value {
        let mut total = 0.0;
        for call in calls {
            match call["usage"][key].as_f64() {
                Some(n) => total += n,
                None => return Value::Null,
            }
        }
        number(total)
    };
    let wall: f64 = calls.iter().filter_map(|call| call["usage"]["wallMs"].as_f64()).sum();
    let unknown = calls
        .iter()
        .filter(|call| {
            ["inputTokens", "outputTokens", "totalTokens", "cost"]
                .iter()
                .any(|key| call["usage"][key].is_null())
        })
        .count();
    json!({
        "calls": calls.len(), "taskCalls": count("task"), "modelCalls": count("model"), "wallMs": number(wall),
        "inputTokens": sum("inputTokens"), "outputTokens": sum("outputTokens"), "totalTokens": sum("totalTokens"),
        "cost": sum("cost"), "unknownCalls": unknown,
    })
}

/// `mergeTotals`: any unknown makes the sum unknown.
fn merge_totals(target: &Value, source: &Value) -> Value {
    let add = |key: &str| number(target[key].as_f64().unwrap_or(0.0) + source[key].as_f64().unwrap_or(0.0));
    let known = |key: &str| match (target[key].as_f64(), source[key].as_f64()) {
        (Some(a), Some(b)) => number(a + b),
        _ => Value::Null,
    };
    json!({
        "calls": add("calls"), "taskCalls": add("taskCalls"), "modelCalls": add("modelCalls"),
        "wallMs": add("wallMs"), "inputTokens": known("inputTokens"), "outputTokens": known("outputTokens"),
        "totalTokens": known("totalTokens"), "cost": known("cost"), "unknownCalls": add("unknownCalls"),
    })
}

fn empty_history() -> Value {
    json!({
        "roots": 0, "firstStartedAt": null, "lastSettledAt": null, "usage": totals(&[]),
        "byKind": { "task": totals(&[]), "model": totals(&[]) }, "spentUsd": 0, "unknownPricedCalls": 0,
        "taskAdmissions": { "succeeded": 0, "failed": 0, "cancelled": 0, "unknown": 0 }, "importedRoots": 0,
    })
}

fn calls_of(root: &Value) -> &[Value] {
    root["calls"].as_array().map_or(&[], Vec::as_slice)
}

fn reservations_of(root: &Value) -> &[Value] {
    root["reservations"].as_array().map_or(&[], Vec::as_slice)
}

impl UsageLedger {
    /// A ledger over the session value `ultron.usage/root` (any [`TaskStore`]).
    pub fn new(store: Box<dyn TaskStore>, options: UsageOptions) -> Result<Self, String> {
        if options.detailed_roots < 1 {
            return Err(invalid("detailedRoots"));
        }
        if let Some(root) = &options.root_id
            && (root.trim().is_empty() || root.contains('\0'))
        {
            return Err("Usage rootId must be a nonempty string without NUL".into());
        }
        let limits = &options.limits;
        if limits.max_cost_usd.is_some_and(|c| !c.is_finite() || c < 0.0) {
            return Err(invalid("maxCostUsd"));
        }
        for (value, name) in [
            (limits.default_max_total_tokens, "defaultMaxTotalTokens"),
            (limits.max_total_tokens, "maxTotalTokens"),
            (limits.max_total_turns, "maxTotalTurns"),
            (limits.max_wall_ms, "maxWallMs"),
        ] {
            if value == Some(0) {
                return Err(invalid(name));
            }
        }
        Ok(UsageLedger {
            state: Arc::new(Mutex::new(State {
                store,
                document: empty_document(),
                loaded: None,
                broken: false,
                ids: options.ids,
            })),
            limits: options.limits,
            root_id: options.root_id.unwrap_or_else(|| DEFAULT_ROOT_ID.into()),
            now: options.now,
            detailed_roots: options.detailed_roots,
            on_refused: options.on_refused,
        })
    }

    /// An in-memory ledger with `limits`.
    pub fn in_memory(limits: UsageLimits) -> Self {
        UsageLedger::new(
            Box::new(MemoryStore::default()),
            UsageOptions {
                limits,
                ..UsageOptions::default()
            },
        )
        .expect("valid limits")
    }

    pub fn default_root(&self) -> &str {
        &self.root_id
    }

    pub fn limits(&self) -> &UsageLimits {
        &self.limits
    }

    /// One serialized step over the loaded document (`enqueue`).
    fn step<T>(&self, change: impl FnOnce(&mut State) -> Result<T, String>) -> Result<T, String> {
        let mut state = lock(&self.state);
        if state.broken {
            return Err(DURABILITY.into());
        }
        if state.loaded.is_none() {
            let loaded = Self::load(&mut state);
            state.loaded = Some(loaded);
        }
        state.loaded.clone().unwrap_or(Ok(()))?;
        if state.broken {
            return Err(DURABILITY.into());
        }
        change(&mut state)
    }

    fn load(state: &mut State) -> Result<(), String> {
        let saved = state
            .store
            .read()
            .map_err(|_| "Usage ledger could not be read".to_string())?;
        state.document = match saved {
            None => empty_document(),
            Some(saved) => {
                let saved = kernel_apis::store::read_versioned("ultron.usage/root", Some(saved), "version")?
                    .unwrap_or(Value::Null);
                validate_document(&saved)?;
                saved
            }
        };
        Ok(())
    }

    fn write(&self, state: &mut State) -> Result<(), String> {
        let mut document = state.document.clone();
        self.fold(&mut document);
        if state.store.write(&document).is_err() {
            state.broken = true;
            return Err(DURABILITY.into());
        }
        state.document = document;
        Ok(())
    }

    /// `ready()`: loads the document once.
    pub fn ready(&self) -> Result<(), String> {
        self.step(|_| Ok(()))
    }

    fn refused(&self, message: String) -> String {
        if let Some(observer) = &self.on_refused {
            observer(&message);
        }
        message
    }

    fn token_cap(&self, root: &Value) -> f64 {
        if self.limits.max_total_tokens.is_some() {
            return finite_limit(self.limits.max_total_tokens);
        }
        if root["rootId"]
            .as_str()
            .is_some_and(|id| id.starts_with(GOAL_USAGE_ROOT_PREFIX))
        {
            f64::INFINITY
        } else {
            finite_limit(self.limits.default_max_total_tokens)
        }
    }

    /// `spend(root, limits)`.
    fn spend(&self, root: &Value, with_limits: bool) -> Value {
        let (spent, unknown) = match root["turns"].get("cost").and_then(Value::as_f64) {
            Some(cost) => (cost, root["turns"]["unpriced"].as_f64().unwrap_or(0.0)),
            None => {
                let models: Vec<&Value> = calls_of(root).iter().filter(|c| c["kind"] == "model").collect();
                (
                    models.iter().filter_map(|c| c["usage"]["cost"].as_f64()).sum(),
                    models.iter().filter(|c| c["usage"]["cost"].is_null()).count() as f64,
                )
            }
        };
        let cap = self.limits.max_cost_usd.filter(|_| with_limits);
        json!({
            "maxCostUsd": cap.map_or(Value::Null, number),
            "spentUsd": number(spent),
            "unknownPricedCalls": number(unknown),
            "remainingUsd": cap.map_or(Value::Null, |cap| number((cap - spent).max(0.0))),
        })
    }

    /// `rootExhaustion`.
    fn exhaustion(&self, root: &Value) -> Option<Exhaustion> {
        let root_id = root["rootId"].as_str().unwrap_or_default();
        let count = root["turns"]["count"].as_f64().unwrap_or(0.0);
        let tokens = root["turns"]["tokens"].as_f64().unwrap_or(0.0);
        let cap = self.token_cap(root);
        let advice = "No further model work is admitted for this root; answer with what you have.";
        let max_turns = finite_limit(self.limits.max_total_turns);
        if count >= max_turns {
            return Some(Exhaustion {
                kind: "turns",
                reason: format!(
                    "Usage turn limit reached for root {root_id}: {} of {} model turns used (max_total_turns, ULTRON_MAX_TOTAL_TURNS). {advice}",
                    js(count),
                    js(max_turns)
                ),
            });
        }
        if tokens >= cap {
            return Some(Exhaustion {
                kind: "tokens",
                reason: format!(
                    "Usage token limit reached for root {root_id}: {} of {} tokens used (max_total_tokens, ULTRON_MAX_TOTAL_TOKENS). {advice}",
                    js(tokens),
                    js(cap)
                ),
            });
        }
        if let Some(max_cost) = self.limits.max_cost_usd {
            let cost = self.spend(root, true);
            let unknown = cost["unknownPricedCalls"].as_f64().unwrap_or(0.0);
            let spent = cost["spentUsd"].as_f64().unwrap_or(0.0);
            if unknown > 0.0 {
                return Some(Exhaustion {
                    kind: "cost",
                    reason: format!(
                        "Usage pricing unknown; cannot enforce cost cap of ${} for root {root_id} ({} model call(s) reported no cost). {advice}",
                        js(max_cost),
                        js(unknown)
                    ),
                });
            }
            if spent >= max_cost {
                return Some(Exhaustion {
                    kind: "cost",
                    reason: format!(
                        "Usage cost cap reached for root {root_id}: spent ${} of ${} (ULTRON_MAX_COST_USD). {advice}",
                        js((spent * 1e6).round() / 1e6),
                        js(max_cost)
                    ),
                });
            }
        }
        None
    }

    fn mint(state: &mut State, prefix: &str) -> String {
        format!("{prefix}{}", (state.ids)())
    }

    /// `reserve(request)`: admission. A known request key returns its open reservation.
    pub fn reserve(&self, request: &ReserveRequest) -> Result<Reservation, String> {
        self.step(|state| {
            let root_id = request.root_id.clone().unwrap_or_else(|| self.root_id.clone());
            if root_id.trim().is_empty() || root_id.contains('\0') {
                return Err("Usage rootId must be a nonempty string without NUL".into());
            }
            if !state.document["roots"][&root_id]["imported"].is_null() {
                return Err(format!("Usage root {root_id} is an imported history root"));
            }
            if request.kind != "task" && request.kind != "model" {
                return Err("Invalid usage kind".into());
            }
            if request.request_key.as_ref().is_some_and(|k| k.trim().is_empty()) {
                return Err("Usage requestKey is empty".into());
            }
            if request.budget_id.as_ref().is_some_and(|k| k.trim().is_empty()) {
                return Err("Usage budgetId is empty".into());
            }
            if let Some(timeout) = request.timeout_ms {
                finite_integer(&number(timeout), "timeoutMs", 1.0)?;
            }
            if request.deadline_at.is_some_and(|d| !d.is_finite()) {
                return Err(invalid("deadlineAt"));
            }
            let mut root = state.document["roots"]
                .get(&root_id)
                .cloned()
                .unwrap_or_else(|| empty_root(&root_id));
            if let Some(key) = &request.request_key
                && let Some(existing) = reservations_of(&root).iter().find(|r| r["requestKey"] == key.as_str())
            {
                return Ok(Reservation(existing.clone()));
            }
            let now = (self.now)();
            let goal = root_id.starts_with(GOAL_USAGE_ROOT_PREFIX);
            if root["startedAt"].is_null() {
                root["startedAt"] = number(now);
                root["deadlineAt"] = match self.limits.max_wall_ms.filter(|_| !goal) {
                    Some(wall) => number(now + wall as f64),
                    None => Value::Null,
                };
                // TS mutates a root already in the document in place: its start stays even if admission is refused.
                if state.document["roots"].get(&root_id).is_some() {
                    state.document["roots"][&root_id] = root.clone();
                }
            }
            let root_deadline = root["deadlineAt"].as_f64();
            if root_deadline.is_some_and(|d| now >= d) {
                return Err(self.refused(format!("Usage wall deadline exceeded for root {root_id}")));
            }
            let admitted = reservations_of(&root).iter().filter(|r| r["kind"] == "task").count() as f64;
            if !goal && request.kind == "task" && admitted >= finite_limit(self.limits.max_admitted_tasks) {
                return Err(self.refused(format!("Usage admitted-task limit exceeded for root {root_id}")));
            }
            let model_work = request.kind == "model" || request.model_backed != Some(false);
            if model_work && let Some(exhausted) = self.exhaustion(&root) {
                return Err(self.refused(exhausted.reason));
            }
            let asked = request.deadline_at.or_else(|| request.timeout_ms.map(|t| now + t));
            let deadline = match (asked, root_deadline) {
                (None, root) => root,
                (Some(asked), None) => Some(asked),
                (Some(asked), Some(root)) => Some(asked.min(root)),
            };
            if deadline.is_some_and(|d| d <= now) {
                return Err(self.refused(format!("Usage deadline has expired for root {root_id}")));
            }
            let mut reservation = Map::new();
            reservation.insert("id".into(), Value::String(Self::mint(state, "ultron-usage-")));
            reservation.insert("rootId".into(), Value::String(root_id.clone()));
            reservation.insert("kind".into(), Value::String(request.kind.clone()));
            for (key, value) in [
                ("taskId", &request.task_id),
                ("parentTaskId", &request.parent_task_id),
                ("requestKey", &request.request_key),
                ("budgetId", &request.budget_id),
            ] {
                if let Some(value) = value {
                    reservation.insert(key.into(), Value::String(value.clone()));
                }
            }
            reservation.insert("admittedAt".into(), number(now));
            reservation.insert("deadlineAt".into(), known(deadline));
            let reservation = Value::Object(reservation);
            if let Some(list) = root["reservations"].as_array_mut() {
                list.push(reservation.clone());
            }
            state.document["roots"][&root_id] = root;
            self.write(state)?;
            Ok(Reservation(reservation))
        })
    }

    /// `settle(reservation, settlement)`: the reservation becomes a call; settling twice is a no-op.
    pub fn settle(
        &self,
        reservation: &Reservation,
        status: Option<&str>,
        usage: Option<&Measurement>,
    ) -> Result<(), String> {
        self.step(|state| {
            let root_id = reservation.root_id().to_string();
            let Some(root) = state.document["roots"].get_mut(&root_id) else {
                return Err("Unknown usage root".into());
            };
            let index = reservations_of(root).iter().position(|r| r["id"] == reservation.id());
            let Some(index) = index else {
                if calls_of(root).iter().any(|c| c["reservationId"] == reservation.id()) {
                    return Ok(());
                }
                return Err("Unknown usage reservation".into());
            };
            let status = status.unwrap_or("unknown");
            if !STATUSES.contains(&status) {
                return Err("Invalid usage settlement status".into());
            }
            let now = (self.now)();
            let active = reservations_of(root)[index].clone();
            let admitted = active["admittedAt"].as_f64().unwrap_or(0.0);
            let measured = normalize_measurement(usage, (now - admitted).max(0.0))?;
            let id = Self::mint(state, "ultron-usage-call-");
            let root = &mut state.document["roots"][&root_id];
            let call = call_from(&active, id, now, status, measured, true);
            if let Some(list) = root["reservations"].as_array_mut() {
                list.remove(index);
            }
            if let Some(list) = root["calls"].as_array_mut() {
                list.push(call);
            }
            self.write(state)
        })
    }

    /// `status(rootId)`.
    pub fn status(&self, root_id: Option<&str>) -> Result<Value, String> {
        let root_id = root_id.unwrap_or(&self.root_id).to_string();
        self.step(|state| {
            let root = state.document["roots"]
                .get(&root_id)
                .cloned()
                .unwrap_or_else(|| empty_root(&root_id));
            let now = (self.now)();
            let deadline = root["deadlineAt"].as_f64();
            let cap = self.token_cap(&root);
            let limit = |n: Option<u64>| n.map_or(Value::Null, |n| number(n as f64));
            Ok(json!({
                "rootId": root_id,
                "limits": {
                    "maxAdmittedTasks": limit(self.limits.max_admitted_tasks),
                    "maxWallMs": limit(self.limits.max_wall_ms),
                    "maxCostUsd": self.limits.max_cost_usd.map_or(Value::Null, number),
                    "maxTotalTokens": limit(self.limits.max_total_tokens),
                    "maxTotalTurns": limit(self.limits.max_total_turns),
                },
                "cost": self.spend(&root, true),
                "turns": {
                    "turns": number(root["turns"]["count"].as_f64().unwrap_or(0.0)),
                    "tokens": number(root["turns"]["tokens"].as_f64().unwrap_or(0.0)),
                    "maxTotalTurns": limit(self.limits.max_total_turns),
                    "maxTotalTokens": if cap.is_finite() { number(cap) } else { Value::Null },
                },
                "admittedTasks": reservations_of(&root).iter().filter(|r| r["kind"] == "task").count(),
                "activeReservations": reservations_of(&root).len(),
                "startedAt": root["startedAt"],
                "deadlineAt": root["deadlineAt"],
                "remainingWallMs": deadline.map_or(Value::Null, |d| number((d - now).max(0.0))),
                "usage": totals(calls_of(&root)),
                "reservations": root["reservations"],
                "session": self.session_totals(&state.document),
            }))
        })
    }

    fn enforces_tree(&self) -> bool {
        self.limits.max_total_turns.is_some()
            || self.limits.max_total_tokens.is_some()
            || self.limits.default_max_total_tokens.is_some()
            || self.limits.max_cost_usd.is_some()
    }

    /// `recordTurn(rootId, measurement)`: one model response of the root's tree.
    pub fn record_turn(
        &self,
        root_id: Option<&str>,
        total_tokens: Option<f64>,
        cost: Option<f64>,
    ) -> Result<(), String> {
        let id = root_id.unwrap_or(&self.root_id).to_string();
        self.step(|state| {
            let mut root = state.document["roots"]
                .get(&id)
                .cloned()
                .unwrap_or_else(|| empty_root(&id));
            if !root["imported"].is_null() {
                return Ok(());
            }
            let ok = |n: Option<f64>| n.filter(|n| n.is_finite() && *n >= 0.0);
            let turns = &root["turns"];
            let next = json!({
                "count": number(turns["count"].as_f64().unwrap_or(0.0) + 1.0),
                "tokens": number(turns["tokens"].as_f64().unwrap_or(0.0) + ok(total_tokens).unwrap_or(0.0)),
                "lastAt": number((self.now)()),
                "cost": number(turns["cost"].as_f64().unwrap_or(0.0) + ok(cost).unwrap_or(0.0)),
                "unpriced": number(turns["unpriced"].as_f64().unwrap_or(0.0) + if ok(cost).is_some() { 0.0 } else { 1.0 }),
            });
            root["turns"] = next;
            state.document["roots"][&id] = root;
            if self.enforces_tree() {
                self.write(state)?;
            }
            Ok(())
        })
    }

    /// `turnBudgetExhausted(rootId)`.
    pub fn turn_budget_exhausted(&self, root_id: Option<&str>) -> Result<Option<String>, String> {
        Ok(self.budget_exhaustion(root_id)?.map(|e| e.reason))
    }

    /// `budgetExhaustion(rootId)`.
    pub fn budget_exhaustion(&self, root_id: Option<&str>) -> Result<Option<Exhaustion>, String> {
        if !self.enforces_tree() {
            return Ok(None);
        }
        let id = root_id.unwrap_or(&self.root_id).to_string();
        self.step(|state| Ok(state.document["roots"].get(&id).and_then(|root| self.exhaustion(root))))
    }

    /// `remaining(rootId)`.
    pub fn remaining(&self, root_id: Option<&str>) -> Result<Remaining, String> {
        let id = root_id.unwrap_or(&self.root_id).to_string();
        self.step(|state| {
            let root = state.document["roots"]
                .get(&id)
                .cloned()
                .unwrap_or_else(|| empty_root(&id));
            let cap = self.token_cap(&root);
            let turns = root["turns"]["count"].as_f64().unwrap_or(0.0);
            let tokens = root["turns"]["tokens"].as_f64().unwrap_or(0.0);
            Ok(Remaining {
                turns_left: self
                    .limits
                    .max_total_turns
                    .map(|max| (max as f64 - turns).max(0.0) as u64),
                tokens_left: cap.is_finite().then(|| (cap - tokens).max(0.0) as u64),
                token_cap: cap.is_finite().then_some(cap as u64),
            })
        })
    }

    fn session_totals(&self, document: &Value) -> Value {
        let history = document.get("history");
        let mut usage = history.map_or_else(|| totals(&[]), |h| h["usage"].clone());
        let mut spent = history.and_then(|h| h["spentUsd"].as_f64()).unwrap_or(0.0);
        let mut unknown = history.and_then(|h| h["unknownPricedCalls"].as_f64()).unwrap_or(0.0);
        let roots = document["roots"].as_object().cloned().unwrap_or_default();
        for root in roots.values() {
            usage = merge_totals(&usage, &totals(calls_of(root)));
            let cost = self.spend(root, true);
            spent += cost["spentUsd"].as_f64().unwrap_or(0.0);
            unknown += cost["unknownPricedCalls"].as_f64().unwrap_or(0.0);
        }
        json!({
            "roots": roots.len(),
            "foldedRoots": history.and_then(|h| h["roots"].as_f64()).map_or(json!(0), number),
            "usage": usage,
            "spentUsd": number(spent),
            "unknownPricedCalls": number(unknown),
        })
    }

    /// `importHistory(rootId, source, calls)`: a historical root that never admits work; idempotent per root.
    pub fn import_history(
        &self,
        root_id: &str,
        source: &str,
        calls: &[(f64, Measurement, Option<String>)],
    ) -> Result<(), String> {
        self.step(|state| {
            if root_id.trim().is_empty() || root_id.contains('\0') {
                return Err("Usage rootId must be a nonempty string without NUL".into());
            }
            if state.document["roots"].get(root_id).is_some() {
                return Ok(());
            }
            let mut stored = Vec::new();
            for (index, (at, usage, status)) in calls.iter().enumerate() {
                let measured = normalize_measurement(Some(usage), 0.0)?;
                stored.push(json!({
                    "id": format!("ultron-usage-call-{root_id}-{index}"),
                    "reservationId": format!("ultron-usage-{root_id}-{index}"),
                    "rootId": root_id, "kind": "model", "admittedAt": number(*at), "settledAt": number(*at),
                    "status": status.clone().unwrap_or_else(|| "succeeded".into()), "usage": measured,
                }));
            }
            let started = calls.iter().map(|(at, _, _)| *at).reduce(f64::min);
            state.document["roots"][root_id] = json!({
                "rootId": root_id, "startedAt": known(started), "deadlineAt": known(started),
                "reservations": [], "calls": stored, "imported": { "source": source, "entries": calls.len() },
            });
            self.write(state)
        })
    }

    /// `reconcile(activeTaskIds)`: open reservations of tasks that are not live settle as unknown.
    pub fn reconcile(&self, active: &[String]) -> Result<(), String> {
        self.step(|state| {
            let now = (self.now)();
            let mut changed = false;
            let ids: Vec<String> = state.document["roots"]
                .as_object()
                .map(|roots| roots.keys().cloned().collect())
                .unwrap_or_default();
            for root_id in ids {
                let abandoned: Vec<Value> = reservations_of(&state.document["roots"][&root_id])
                    .iter()
                    .filter(|r| {
                        let owner = if r["kind"] == "task" {
                            &r["taskId"]
                        } else {
                            &r["parentTaskId"]
                        };
                        owner.as_str().is_none_or(|id| !active.iter().any(|a| a == id))
                    })
                    .cloned()
                    .collect();
                for reservation in abandoned {
                    let admitted = reservation["admittedAt"].as_f64().unwrap_or(0.0);
                    let id = Self::mint(state, "ultron-usage-call-");
                    let call = call_from(
                        &reservation,
                        id,
                        now,
                        "unknown",
                        normalize_measurement(None, (now - admitted).max(0.0))?,
                        false,
                    );
                    let root = &mut state.document["roots"][&root_id];
                    if let Some(list) = root["reservations"].as_array_mut() {
                        list.retain(|r| r["id"] != reservation["id"]);
                    }
                    if let Some(list) = root["calls"].as_array_mut() {
                        list.push(call);
                    }
                    changed = true;
                }
            }
            let mut document = state.document.clone();
            if self.fold(&mut document) || changed {
                self.write(state)?;
            }
            Ok(())
        })
    }

    /// `fold`: idle roots outside the most recently active `detailed_roots` move into `history`.
    fn fold(&self, document: &mut Value) -> bool {
        let roots: Vec<Value> = document["roots"]
            .as_object()
            .map(|roots| roots.values().cloned().collect())
            .unwrap_or_default();
        if roots.len() <= self.detailed_roots {
            return false;
        }
        let now = (self.now)();
        let mut ranked: Vec<(f64, usize)> = roots
            .iter()
            .enumerate()
            .map(|(i, root)| (last_activity(root), i))
            .collect();
        // A stable sort by activity, newest first (Array.prototype.sort is stable).
        ranked.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        let recent: Vec<&str> = ranked
            .iter()
            .take(self.detailed_roots)
            .filter_map(|(_, i)| roots[*i]["rootId"].as_str())
            .collect();
        let mut folded = false;
        for root in &roots {
            let id = root["rootId"].as_str().unwrap_or_default();
            if recent.contains(&id) || id == self.root_id || !reservations_of(root).is_empty() {
                continue;
            }
            if root["imported"].is_null() && root["deadlineAt"].as_f64().is_some_and(|d| now < d) {
                continue;
            }
            if document.get("history").is_none() {
                document["history"] = empty_history();
            }
            let spend = self.spend(root, false);
            fold_root(&mut document["history"], root, &spend);
            if let Some(map) = document["roots"].as_object_mut() {
                map.shift_remove(id);
            }
            folded = true;
        }
        folded
    }

    /// The stored document as it is now (for reconciliation and inspection).
    pub fn document(&self) -> Result<Value, String> {
        self.step(|state| Ok(state.document.clone()))
    }
}

fn js(n: f64) -> String {
    match number(n) {
        Value::Number(number) => number.to_string(),
        _ if n.is_infinite() => if n > 0.0 { "Infinity" } else { "-Infinity" }.into(),
        _ => "NaN".into(),
    }
}

fn normalize_measurement(measurement: Option<&Measurement>, elapsed: f64) -> Result<Value, String> {
    let wall = measurement.and_then(|m| m.wall_ms).unwrap_or(elapsed);
    if !wall.is_finite() || wall < 0.0 {
        return Err("Usage wallMs must be a non-negative finite number".into());
    }
    let fields = measurement.cloned().unwrap_or_default();
    for (name, value) in [
        ("inputTokens", fields.input_tokens),
        ("outputTokens", fields.output_tokens),
        ("totalTokens", fields.total_tokens),
        ("cost", fields.cost),
    ] {
        if value.is_some_and(|n| !n.is_finite()) {
            return Err(invalid(name));
        }
    }
    Ok(json!({
        "inputTokens": known(fields.input_tokens), "outputTokens": known(fields.output_tokens),
        "totalTokens": known(fields.total_tokens), "cost": known(fields.cost), "wallMs": number(wall),
    }))
}

/// A settled call from a reservation; `budget`: whether `budgetId` is copied (reconciliation does not copy it).
fn call_from(reservation: &Value, id: String, now: f64, status: &str, usage: Value, budget: bool) -> Value {
    let mut call = Map::new();
    call.insert("id".into(), Value::String(id));
    call.insert("reservationId".into(), reservation["id"].clone());
    call.insert("rootId".into(), reservation["rootId"].clone());
    call.insert("kind".into(), reservation["kind"].clone());
    let mut keys = vec!["taskId", "parentTaskId", "requestKey"];
    if budget {
        keys.push("budgetId");
    }
    for key in keys {
        if let Some(value) = reservation.get(key) {
            call.insert(key.into(), value.clone());
        }
    }
    call.insert("admittedAt".into(), reservation["admittedAt"].clone());
    call.insert("settledAt".into(), number(now));
    call.insert("status".into(), Value::String(status.into()));
    call.insert("usage".into(), usage);
    Value::Object(call)
}

fn last_activity(root: &Value) -> f64 {
    let mut latest = root["startedAt"].as_f64().unwrap_or(f64::NEG_INFINITY);
    for call in calls_of(root) {
        latest = latest
            .max(call["settledAt"].as_f64().unwrap_or(f64::NEG_INFINITY))
            .max(call["admittedAt"].as_f64().unwrap_or(f64::NEG_INFINITY));
    }
    for reservation in reservations_of(root) {
        latest = latest.max(reservation["admittedAt"].as_f64().unwrap_or(f64::NEG_INFINITY));
    }
    if let Some(at) = root["turns"]["lastAt"].as_f64() {
        latest = latest.max(at);
    }
    latest
}

fn fold_root(history: &mut Value, root: &Value, spend: &Value) {
    let add = |value: &Value, n: f64| number(value.as_f64().unwrap_or(0.0) + n);
    history["roots"] = add(&history["roots"], 1.0);
    if !root["imported"].is_null() {
        history["importedRoots"] = add(&history["importedRoots"], 1.0);
    }
    let calls = calls_of(root);
    let started = root["startedAt"]
        .as_f64()
        .or_else(|| calls.iter().filter_map(|c| c["admittedAt"].as_f64()).reduce(f64::min));
    if let Some(started) = started {
        let first = history["firstStartedAt"].as_f64().map_or(started, |f| f.min(started));
        history["firstStartedAt"] = number(first);
    }
    for call in calls {
        let settled = call["settledAt"].as_f64().unwrap_or(0.0);
        let last = history["lastSettledAt"].as_f64().map_or(settled, |l| l.max(settled));
        history["lastSettledAt"] = number(last);
    }
    history["usage"] = merge_totals(&history["usage"], &totals(calls));
    for kind in ["task", "model"] {
        let of_kind: Vec<Value> = calls.iter().filter(|c| c["kind"] == kind).cloned().collect();
        history["byKind"][kind] = merge_totals(&history["byKind"][kind], &totals(&of_kind));
    }
    history["spentUsd"] = add(&history["spentUsd"], spend["spentUsd"].as_f64().unwrap_or(0.0));
    history["unknownPricedCalls"] = add(
        &history["unknownPricedCalls"],
        spend["unknownPricedCalls"].as_f64().unwrap_or(0.0),
    );
    for call in calls.iter().filter(|c| c["kind"] == "task") {
        if let Some(status) = call["status"].as_str() {
            history["taskAdmissions"][status] = add(&history["taskAdmissions"][status], 1.0);
        }
    }
}
