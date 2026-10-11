//! Host side of `rlm.infer` (inference.ts): one private, bounded model call per frame over explicit context views,
//! with `{calls, tokens, depth}` budget accounting, contract validation and bounded re-asks, and `Incomplete`
//! observations when budget or repairs run out or `timeout_ms` passes. Also serves `rlm.load` (content-addressed
//! handles under the session) and `rlm.frames` (frame traces); every other request goes to the inner handler.

mod map;
pub mod schema;
mod trace;

pub use map::{DEFAULT_MAP_TOKENS, Frames, INFERENCE_PROMPT, ModelSettings};

use ai::{AssistantContent, AssistantMessage, Message, Model, SimpleStreamOptions, StopReason, Transport};
use kernel::{HostHandler, Signal};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use trace::{Budget, Frame, Spec, Traces};

const MAX_TRANCHE: u64 = 16_384;
const MIN_TRANCHE: u64 = 32;
const REQUEST_OVERHEAD_TOKENS: u64 = 64;
const OUTPUT_PREVIEW_CHARS: usize = 2_000;
const LAST_OUTPUTS: usize = 3;
const MAX_TASK_CHARS: usize = 65_536;
const MAX_FRAME_CONTEXT_CHARS: u64 = 4_000_000;
const MAX_REPAIRS: u64 = 8;
const DEFAULT_REPAIRS: u64 = 2;
const DEFAULT_SCALAR_REPAIRS: u64 = 1;
const MAX_DEPTH: u64 = 4;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const DEFAULT_TIMEOUT_MS: u64 = 10 * 60 * 1000;
const MAX_TIMEOUT_MS: u64 = 60 * 60 * 1000;
const THINKING_LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

/// JS `string.length`.
fn js_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// `bounded(text, limit)`: at most `limit` UTF-16 units, the last one an ellipsis.
fn bounded(text: &str, limit: usize) -> String {
    if js_len(text) <= limit {
        return text.to_string();
    }
    let head: Vec<u16> = text.encode_utf16().take(limit - 1).collect();
    format!("{}…", String::from_utf16_lossy(&head))
}

/// Integral floats become integers, as a JS round trip (`JSON.parse` then `JSON.stringify`) leaves them.
fn js_normalize(value: Value) -> Value {
    match value {
        Value::Number(n) if !n.is_i64() && !n.is_u64() => match n.as_f64() {
            Some(f) if f.fract() == 0.0 && f.abs() <= MAX_SAFE_INTEGER as f64 => json!(f as i64),
            _ => Value::Number(n),
        },
        Value::Array(items) => Value::Array(items.into_iter().map(js_normalize).collect()),
        Value::Object(map) => Value::Object(map.into_iter().map(|(k, v)| (k, js_normalize(v))).collect()),
        other => other,
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Calls,
    Tokens,
}

#[derive(Default)]
struct Pool {
    limit: Option<u64>,
    spent: u64,
    held: u64,
}

struct Node {
    parent: Option<usize>,
    calls: Pool,
    tokens: Pool,
}

/// Budget nodes (`BudgetNode`): holds and charges apply to a node and every ancestor, so a node's availability is
/// the tightest remaining pool on its path to the root.
#[derive(Default)]
struct Ledger {
    nodes: Vec<Node>,
}

impl Ledger {
    fn add(&mut self, parent: Option<usize>, calls: Option<u64>, tokens: Option<u64>) -> usize {
        let pool = |limit| Pool {
            limit,
            ..Pool::default()
        };
        self.nodes.push(Node {
            parent,
            calls: pool(calls),
            tokens: pool(tokens),
        });
        self.nodes.len() - 1
    }

    fn chain(&self, node: usize) -> Vec<usize> {
        let mut out = vec![node];
        while let Some(parent) = self.nodes[*out.last().unwrap_or(&node)].parent {
            out.push(parent);
        }
        out
    }

    fn pool(&mut self, node: usize, kind: Kind) -> &mut Pool {
        let node = &mut self.nodes[node];
        match kind {
            Kind::Calls => &mut node.calls,
            Kind::Tokens => &mut node.tokens,
        }
    }

    /// Remaining capacity; `None` when no node on the path limits it.
    fn available(&mut self, node: usize, kind: Kind) -> Option<u64> {
        let mut remaining: Option<i128> = None;
        for id in self.chain(node) {
            let pool = self.pool(id, kind);
            if let Some(limit) = pool.limit {
                let left = limit as i128 - pool.spent as i128 - pool.held as i128;
                remaining = Some(remaining.map_or(left, |r| r.min(left)));
            }
        }
        remaining.map(|r| r.max(0) as u64)
    }

    fn hold(&mut self, node: usize, kind: Kind, amount: u64) -> bool {
        if self.available(node, kind).is_some_and(|left| amount > left) {
            return false;
        }
        for id in self.chain(node) {
            self.pool(id, kind).held += amount;
        }
        true
    }

    fn release(&mut self, node: usize, kind: Kind, amount: u64) {
        for id in self.chain(node) {
            let pool = self.pool(id, kind);
            pool.held = pool.held.saturating_sub(amount);
        }
    }

    fn charge(&mut self, node: usize, kind: Kind, amount: u64) {
        for id in self.chain(node) {
            self.pool(id, kind).spent += amount;
        }
    }

    /// Claim a call (unless already held) and `estimate` input tokens plus an output tranche of at most a quarter of
    /// the remaining pool and 16k. Returns `(held tokens, tranche)`, or `None` holding nothing.
    fn claim(&mut self, node: usize, estimate: u64, model_max: u64, call_held: bool) -> Option<(u64, u64)> {
        if !call_held && !self.hold(node, Kind::Calls, 1) {
            return None;
        }
        let ceiling = MAX_TRANCHE.min(if model_max > 0 { model_max } else { MAX_TRANCHE });
        let tranche = match self.available(node, Kind::Tokens) {
            Some(left) => (ceiling as i128)
                .min((left / 4) as i128)
                .min(left as i128 - estimate as i128),
            None => ceiling as i128,
        };
        if tranche < MIN_TRANCHE as i128 || !self.hold(node, Kind::Tokens, estimate + tranche as u64) {
            self.release(node, Kind::Calls, 1);
            return None;
        }
        Some((estimate + tranche as u64, tranche as u64))
    }
}

/// Tokens a response is charged: input, output and cache writes, cache reads at a tenth.
fn charged_tokens(usage: &ai::Usage) -> u64 {
    usage.input + usage.output + usage.cache_write + usage.cache_read.div_ceil(10)
}

fn fields(payload: &Map<String, Value>, allowed: &[&str]) -> Result<(), String> {
    match payload.keys().find(|key| !allowed.contains(&key.as_str())) {
        Some(key) => Err(format!("Unknown payload field: {key}")),
        None => Ok(()),
    }
}

fn optional_integer(item: Option<&Value>, name: &str, minimum: u64, maximum: u64) -> Result<Option<u64>, String> {
    match item {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let integer = value.as_u64().or_else(|| {
                value
                    .as_f64()
                    .filter(|f| f.fract() == 0.0 && *f >= 0.0 && *f <= MAX_SAFE_INTEGER as f64)
                    .map(|f| f as u64)
            });
            integer
                .filter(|n| (minimum..=maximum).contains(n))
                .map(Some)
                .ok_or_else(|| format!("{name} must be an integer between {minimum} and {maximum}"))
        }
    }
}

fn digest_hex(item: Option<&Value>) -> Option<&str> {
    let hex = item?.as_str()?.strip_prefix("sha256:")?;
    (hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))).then_some(hex)
}

fn label_of(item: &Map<String, Value>, fallback: &str) -> String {
    item.get("label")
        .and_then(Value::as_str)
        .map_or_else(|| fallback.to_string(), |label| bounded(label, 200))
}

struct View {
    label: String,
    text: String,
    digest: Option<String>,
    start: Option<Value>,
    end: Option<Value>,
}

fn frame_system_prompt() -> &'static str {
    "You are an inference frame inside Ultron: a private, bounded sub-call made by a program. You see only the task and the context views in the user message; there is no earlier conversation and no files. Answer from the views alone and never invent content that is not in them. When a contract (JSON schema) is given, reply with exactly one JSON value that satisfies it: no prose, no code fences. Without a contract, reply with the answer only. You have no tools."
}

fn frame_prompt(task: &str, views: &[View], contract: Option<&Value>) -> String {
    let mut parts = vec![format!("Task:\n{task}")];
    if views.is_empty() {
        parts.push("Context: none.".into());
    }
    for (index, view) in views.iter().enumerate() {
        let n = index + 1;
        parts.push(format!(
            "--- view {n}: {} ({} chars) ---\n{}\n--- end of view {n} ---",
            view.label,
            js_len(&view.text),
            view.text
        ));
    }
    if let Some(contract) = contract {
        parts.push(format!(
            "Contract (JSON schema): {contract}\nReply with only a JSON value that satisfies the contract."
        ));
    }
    parts.join("\n\n")
}

fn repair_prompt(error: &str, contract: &Value) -> String {
    format!(
        "Your reply does not satisfy the contract: {error}.\nReply again with only a JSON value that satisfies this schema: {contract}"
    )
}

/// `{type: <scalar>}` with at most `description`/`title` besides; `types` limits which scalars count.
fn is_plain_type(contract: &Value, types: &[&str]) -> bool {
    contract.as_object().is_some_and(|map| {
        map.get("type")
            .and_then(Value::as_str)
            .is_some_and(|t| types.contains(&t))
            && map
                .keys()
                .all(|key| ["type", "description", "title"].contains(&key.as_str()))
    })
}

/// JS `String.prototype.trim` whitespace.
fn js_trim(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
}

/// `text.replace(/^```(?:json)?\s*\n([\s\S]*?)\n?```$/, "$1")`.
fn strip_fence(text: &str) -> &str {
    let Some(inner) = text.strip_prefix("```").and_then(|rest| rest.strip_suffix("```")) else {
        return text;
    };
    if text.len() < 6 {
        return text;
    }
    let body = |rest: &'_ str| -> Option<usize> {
        let run = rest.len()
            - rest
                .trim_start_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
                .len();
        rest[..run].rfind('\n').map(|at| at + 1)
    };
    let start = match inner.strip_prefix("json").and_then(|rest| body(rest).map(|at| 4 + at)) {
        Some(at) => at,
        None => match body(inner) {
            Some(at) => at,
            None => return text,
        },
    };
    // The capture's optional trailing newline is trimmed by the caller.
    &inner[start..]
}

/// `parseReply`: the contract-valid value, or the reason to re-ask.
fn parse_reply(text: &str, contract: &Value) -> Result<Value, String> {
    let cleaned = js_trim(strip_fence(js_trim(text)));
    let parsed = serde_json::from_str::<Value>(cleaned).ok().or_else(|| {
        let start = cleaned.find(['[', '{'])?;
        let end = cleaned.rfind(['}', ']'])?;
        (end > start)
            .then(|| serde_json::from_str::<Value>(&cleaned[start..=end]).ok())
            .flatten()
    });
    let parsed = parsed.map(js_normalize);
    if is_plain_type(contract, &["string"]) && !parsed.as_ref().is_some_and(Value::is_string) {
        return Ok(Value::String(cleaned.to_string()));
    }
    let parsed = parsed.ok_or("the reply is not JSON")?;
    let errors = schema::errors(contract, &parsed);
    if errors.is_empty() {
        return Ok(parsed);
    }
    Err(errors.into_iter().take(3).collect::<Vec<_>>().join("; "))
}

enum Outcome {
    Complete(Value),
    Incomplete(&'static str, String),
    Error(String),
}

struct Options {
    contract: Option<Value>,
    max_repairs: u64,
    thinking: Option<String>,
    /// Each frame's wall-clock limit from admission (the TS frame task's `timeoutMs`).
    timeout: Duration,
}

/// Serves `rlm.infer` and `rlm.load`; everything else goes to `inner`.
pub struct Inference {
    inner: Arc<dyn HostHandler>,
    model: Model,
    api_key: Option<String>,
    transport: Arc<dyn Transport>,
    context_dir: PathBuf,
    labels: Mutex<HashMap<String, String>>,
    traces: Mutex<Traces>,
}

impl Inference {
    /// Frames call `model` through `transport`; loaded handles live in `context_dir` (created on first load).
    pub fn new(
        inner: Arc<dyn HostHandler>,
        model: Model,
        api_key: Option<String>,
        transport: Arc<dyn Transport>,
        context_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            inner,
            model,
            api_key,
            transport,
            context_dir: context_dir.into(),
            labels: Mutex::new(HashMap::new()),
            traces: Mutex::new(Traces::default()),
        }
    }

    fn load(&self, payload: &Map<String, Value>) -> Result<Value, String> {
        fields(payload, &["digest", "label", "size", "chars", "source"])?;
        let digest = payload.get("digest").and_then(Value::as_str).unwrap_or_default();
        let hex = digest_hex(payload.get("digest")).ok_or("digest must be sha256:<64 hex>")?;
        let path = self.context_dir.join(hex);
        let shown = path.to_string_lossy().into_owned();
        let mut labels = self.labels.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if payload.get("size").is_none() {
            let meta = std::fs::metadata(&path).map_err(|_| format!("Unknown context handle {digest}"))?;
            let label = labels.get(digest).cloned();
            return Ok(json!({"path": shown, "stored": true, "label": label, "size": meta.len()}));
        }
        let size = optional_integer(payload.get("size"), "size", 0, MAX_SAFE_INTEGER)?.unwrap_or(0);
        optional_integer(payload.get("chars"), "chars", 0, MAX_SAFE_INTEGER)?;
        let label = payload
            .get("label")
            .and_then(Value::as_str)
            .filter(|label| !js_trim(label).is_empty())
            .map_or_else(|| "context".to_string(), |label| bounded(label, 200));
        create_private_dir(&self.context_dir)?;
        let stored = std::fs::metadata(&path).is_ok_and(|meta| meta.len() == size);
        labels.insert(digest.to_string(), label);
        Ok(json!({"path": shown, "stored": stored}))
    }

    fn views(&self, items: Option<&Value>) -> Result<Vec<View>, String> {
        let empty = Vec::new();
        let items = match items {
            None | Some(Value::Null) => &empty,
            Some(Value::Array(items)) => items,
            Some(_) => return Err("context must be a list".into()),
        };
        let mut total = 0u64;
        let mut views = Vec::new();
        for raw in items {
            let item = raw.as_object().ok_or("context item must be an object")?;
            let label = label_of(item, "context");
            match item.get("kind").and_then(Value::as_str) {
                Some("text") => {
                    let text = item
                        .get("text")
                        .and_then(Value::as_str)
                        .ok_or("context text must be a string")?;
                    total += js_len(text) as u64;
                    let number = |key: &str| item.get(key).filter(|v| v.is_number()).cloned();
                    views.push(View {
                        label,
                        text: text.to_string(),
                        digest: item.get("digest").and_then(Value::as_str).map(str::to_string),
                        start: number("start"),
                        end: number("end"),
                    });
                }
                Some("handle") => {
                    let digest = item.get("digest").and_then(Value::as_str).unwrap_or_default();
                    let hex = digest_hex(item.get("digest")).ok_or("context handle digest must be sha256:<64 hex>")?;
                    let path = self.context_dir.join(hex);
                    let meta = std::fs::metadata(&path).map_err(|_| format!("Unknown context handle {digest}"))?;
                    if meta.len() > MAX_FRAME_CONTEXT_CHARS * 4 {
                        return Err(format!(
                            "Context handle {label} is too large to give a frame whole; pass slices or chunks"
                        ));
                    }
                    let bytes = std::fs::read(&path).map_err(|error| error.to_string())?;
                    let text = String::from_utf8_lossy(&bytes).into_owned();
                    total += js_len(&text) as u64;
                    views.push(View {
                        label,
                        text,
                        digest: Some(digest.to_string()),
                        start: None,
                        end: None,
                    });
                }
                _ => return Err("context item kind must be text or handle".into()),
            }
        }
        if total > MAX_FRAME_CONTEXT_CHARS {
            return Err(format!(
                "A frame's context is {total} characters (limit {MAX_FRAME_CONTEXT_CHARS}); pass slices or chunks"
            ));
        }
        Ok(views)
    }

    fn infer(&self, payload: &Map<String, Value>, signal: &Signal) -> Result<Value, String> {
        fields(
            payload,
            &[
                "task",
                "context",
                "contract",
                "budget",
                "model",
                "max_repairs",
                "timeout_ms",
                "thinking",
            ],
        )?;
        let contract = payload
            .get("contract")
            .filter(|c| !c.is_null())
            .cloned()
            .map(js_normalize);
        if let Some(contract) = &contract {
            schema::validate_schema(contract, "$.contract")?;
            if let Some(at) = schema::unchecked_keyword(contract, "$.contract") {
                return Err(format!("{at}: this contract keyword is not checked by this host yet"));
            }
        }
        let empty = Map::new();
        let budget = match payload.get("budget") {
            None | Some(Value::Null) => &empty,
            Some(Value::Object(budget)) => budget,
            Some(_) => return Err("budget must be an object".into()),
        };
        fields(budget, &["calls", "tokens", "depth"])?;
        let calls = optional_integer(budget.get("calls"), "budget.calls", 0, MAX_SAFE_INTEGER)?;
        let tokens = optional_integer(budget.get("tokens"), "budget.tokens", 0, MAX_SAFE_INTEGER)?;
        let depth = optional_integer(budget.get("depth"), "budget.depth", 1, MAX_DEPTH)?.unwrap_or(1);
        let model = match payload.get("model") {
            None | Some(Value::Null) => None,
            Some(Value::String(model)) if is_model_ref(model) => Some(model.clone()),
            Some(_) => return Err("model must be provider/model".into()),
        };
        let max_repairs = optional_integer(payload.get("max_repairs"), "max_repairs", 0, MAX_REPAIRS)?.unwrap_or(
            if contract
                .as_ref()
                .is_some_and(|c| is_plain_type(c, &["string", "integer", "number", "boolean", "null"]))
            {
                DEFAULT_SCALAR_REPAIRS
            } else {
                DEFAULT_REPAIRS
            },
        );
        let timeout = optional_integer(payload.get("timeout_ms"), "timeout_ms", 1, MAX_TIMEOUT_MS)?;
        let thinking = match payload.get("thinking") {
            None | Some(Value::Null) => None,
            Some(level) => match level.as_str().filter(|level| THINKING_LEVELS.contains(level)) {
                Some(level) => Some(level.to_string()),
                None => return Err(format!("thinking must be one of {}", THINKING_LEVELS.join(", "))),
            },
        };
        let task = payload
            .get("task")
            .and_then(Value::as_str)
            .filter(|task| !js_trim(task).is_empty());
        let task = task.ok_or("task must be a nonempty string")?;
        if js_len(task) > MAX_TASK_CHARS {
            return Err(format!(
                "task exceeds {MAX_TASK_CHARS} characters; pass data as context"
            ));
        }
        let views = self.views(payload.get("context"))?;
        if depth > 1 {
            return Err(format!(
                "budget depth {depth} needs frames that run code (the rlm tool), which this host does not run yet"
            ));
        }

        let mut ledger = Ledger::default();
        let budget = Budget {
            id: trace::uuid("budget"),
            node: ledger.add(None, calls, tokens),
            depth,
            default_tokens: false,
        };
        let mut frame = Frame::new(&mut ledger, &budget, task, views);
        let options = Options {
            contract,
            max_repairs,
            thinking,
            timeout: Duration::from_millis(timeout.unwrap_or(DEFAULT_TIMEOUT_MS)),
        };
        // The first request's call is claimed at admission.
        let held = ledger.hold(budget.node, Kind::Calls, 1);
        let ledger = Mutex::new(ledger);
        let outcome = self.run_frame(&ledger, &mut frame, held, model.as_deref(), &options, signal);
        let outcome = self.finish(&ledger, &budget, &frame, &options, model.as_deref(), outcome);
        Ok(observation(&mut lock(&ledger), &frame, depth, outcome))
    }

    /// Run one admitted frame: refused when its call was not held, the cell is aborted or the model is not this
    /// host's; otherwise its conversation, within the frame's timeout.
    fn run_frame(
        &self,
        ledger: &Mutex<Ledger>,
        frame: &mut Frame,
        held: bool,
        model: Option<&str>,
        options: &Options,
        signal: &Signal,
    ) -> Outcome {
        let deadline = Instant::now() + options.timeout;
        let own = format!("{}/{}", self.model.provider, self.model.id);
        if !held {
            return Outcome::Incomplete("budget_exhausted", "no call left in the budget for this frame".into());
        }
        let refused = if signal.is_aborted() {
            Some("cancelled: the calling cell was aborted".to_string())
        } else {
            model
                .filter(|model| *model != own)
                .map(|model| format!("Admission refused: model {model} is not available to this host"))
        };
        if let Some(error) = refused {
            lock(ledger).release(frame.node, Kind::Calls, 1);
            return Outcome::Error(error);
        }
        frame.model = Some(own);
        self.converse(ledger, frame, options, signal, deadline)
    }

    /// Persist the finished frame's trace and summary; returns the outcome its observation reports.
    fn finish(
        &self,
        ledger: &Mutex<Ledger>,
        budget: &Budget,
        frame: &Frame,
        options: &Options,
        model: Option<&str>,
        outcome: Outcome,
    ) -> Outcome {
        let ended_at = trace::now_ms();
        let spec = Spec {
            contract: options.contract.as_ref(),
            max_repairs: options.max_repairs,
            depth: budget.depth,
            model,
        };
        let mut ledger = lock(ledger);
        let ledger = &mut *ledger;
        let document = trace::trace(ledger, budget, frame, &spec, &outcome, ended_at);
        let summary = trace::summary(ledger, budget, frame, &outcome, ended_at);
        self.traces
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .write(&frame.id, document, summary);
        match outcome {
            Outcome::Incomplete("budget_exhausted", detail) if budget.default_tokens => Outcome::Incomplete(
                "budget_exhausted",
                format!(
                    "{detail} (rlm.map's default budget is {} tokens; pass budget=Budget(tokens=...) to allow more)",
                    ledger.nodes[budget.node].tokens.limit.unwrap_or_default()
                ),
            ),
            other => other,
        }
    }

    /// The frame's conversation: the seeded prompt, then re-asks within budget until the contract holds. A request
    /// the provider does not answer before the deadline is cancelled and refunded.
    fn converse(
        &self,
        ledger: &Mutex<Ledger>,
        frame: &mut Frame,
        options: &Options,
        signal: &Signal,
        deadline: Instant,
    ) -> Outcome {
        let system = frame_system_prompt();
        let mut conversation_chars = js_len(system) as u64;
        let mut message = frame_prompt(&frame.task, &frame.views, options.contract.as_ref());
        let mut call_held = true;
        let mut history: Vec<Message> = Vec::new();
        for attempt in 0u64.. {
            if signal.is_aborted() || Instant::now() >= deadline {
                if call_held {
                    lock(ledger).release(frame.node, Kind::Calls, 1);
                }
                return Outcome::Error("cancelled".into());
            }
            let estimate = (conversation_chars + js_len(&message) as u64).div_ceil(3) + REQUEST_OVERHEAD_TOKENS;
            let window = self.model.context_window;
            if window > 0 && estimate + MIN_TRANCHE > window {
                if call_held {
                    lock(ledger).release(frame.node, Kind::Calls, 1);
                }
                return Outcome::Error(format!(
                    "context too large for {}/{}: about {estimate} tokens against a {window}-token window; pass smaller slices",
                    self.model.provider, self.model.id
                ));
            }
            let claimed = lock(ledger).claim(frame.node, estimate, self.model.max_tokens, call_held);
            call_held = false;
            let Some((held, tranche)) = claimed else {
                let detail = if attempt == 0 {
                    "the budget cannot cover the first request".to_string()
                } else {
                    format!("the budget ran out after {attempt} attempt(s) without a valid answer")
                };
                return Outcome::Incomplete("budget_exhausted", detail);
            };
            conversation_chars += js_len(&message) as u64;
            history.push(user_message(&message));
            let max_tokens = if self.model.max_tokens > 0 {
                self.model.max_tokens.min(tranche)
            } else {
                tranche
            };
            let response = self.request(system, &history, max_tokens, options.thinking.as_deref(), deadline);
            // A failed or timed-out request is refunded; only a response is charged. One lock, so a concurrent frame
            // never sees the hold released before the charge lands.
            let charged = match &response {
                Ok(Some(reply)) => charged_tokens(&reply.usage),
                _ => 0,
            };
            {
                let mut ledger = lock(ledger);
                ledger.release(frame.node, Kind::Calls, 1);
                ledger.release(frame.node, Kind::Tokens, held);
                if let Ok(Some(_)) = &response {
                    ledger.charge(frame.node, Kind::Calls, 1);
                    ledger.charge(frame.node, Kind::Tokens, charged);
                }
            }
            let reply = match response {
                Ok(Some(reply)) => reply,
                Ok(None) => return Outcome::Error("cancelled".into()),
                Err(error) => return Outcome::Error(format!("frame request failed: {}", Value::String(error))),
            };
            frame.measured.0 += reply.usage.input;
            frame.measured.1 += reply.usage.output;
            frame.measured.2 += reply.usage.cost.total;
            if frame.requests.len() < trace::TRACE_REQUEST_LIMIT {
                frame.requests.push(json!({
                    "at": trace::now_ms(), "tranche": tranche, "held": held, "charged": charged,
                    "status": serde_json::to_value(reply.stop_reason).unwrap_or(Value::Null),
                }));
            }
            if signal.is_aborted() {
                return Outcome::Error("cancelled".into());
            }
            let limited = reply
                .diagnostics
                .iter()
                .flatten()
                .any(|d| d.kind == "provider_usage_limit");
            if limited {
                let detail = reply
                    .error_message
                    .clone()
                    .unwrap_or("the provider's usage limit is reached".into());
                return Outcome::Incomplete("usage_limit", detail);
            }
            if matches!(reply.stop_reason, StopReason::Error | StopReason::Aborted) {
                let status = if reply.stop_reason == StopReason::Aborted {
                    "aborted"
                } else {
                    "failed"
                };
                let error = reply.error_message.clone().unwrap_or_default();
                return Outcome::Error(format!(
                    "frame run did not complete: {status} ({})",
                    json!({"message": error})
                ));
            }
            let text: Vec<&str> = reply
                .content
                .iter()
                .filter_map(|part| match part {
                    AssistantContent::Text(text) => Some(text.text.as_str()),
                    _ => None,
                })
                .collect();
            let text = text.join("\n");
            history.push(Message::Assistant(reply));
            conversation_chars += js_len(&text) as u64;
            let output = bounded(&text, OUTPUT_PREVIEW_CHARS);
            frame.outputs.push(output.clone());
            if frame.outputs.len() > LAST_OUTPUTS {
                frame.outputs.remove(0);
            }
            let attempt_record = |error: Option<&str>| {
                let mut record = json!({"attempt": attempt, "at": trace::now_ms(), "output": output});
                if let Some(error) = error {
                    record["error"] = error.into();
                }
                record
            };
            let error = if js_trim(&text).is_empty() {
                "the reply is empty".to_string()
            } else {
                let parsed = match &options.contract {
                    None => Ok(Value::String(text)),
                    Some(contract) => parse_reply(&text, contract),
                };
                match parsed {
                    Ok(value) => {
                        frame.attempts.push(attempt_record(None));
                        return Outcome::Complete(value);
                    }
                    Err(error) => error,
                }
            };
            frame.attempts.push(attempt_record(Some(&error)));
            if attempt >= options.max_repairs {
                return Outcome::Incomplete(
                    "contract_unmet",
                    format!("no valid answer after {} attempt(s): {error}", attempt + 1),
                );
            }
            let fallback = json!({"type": "string", "minLength": 1});
            message = repair_prompt(&error, options.contract.as_ref().unwrap_or(&fallback));
        }
        unreachable!("the attempt counter is unbounded")
    }

    /// One provider request through crates/ai; `None` when the deadline passes first.
    fn request(
        &self,
        system: &str,
        history: &[Message],
        max_tokens: u64,
        thinking: Option<&str>,
        deadline: Instant,
    ) -> Result<Option<AssistantMessage>, String> {
        let mut messages = vec![Message::System(
            serde_json::from_value(json!({"role": "system", "content": system, "timestamp": 0}))
                .map_err(|error| error.to_string())?,
        )];
        messages.extend(history.iter().cloned());
        let context = ai::TranscriptContext { messages };
        let mut options = SimpleStreamOptions::default();
        options.base.api_key = self.api_key.clone();
        options.base.max_tokens = Some(max_tokens);
        options.reasoning = thinking.filter(|_| self.model.reasoning).map(str::to_string);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        let transport = &*self.transport;
        let mut sink = |_: &ai::AssistantMessageEvent| {};
        let stream = async {
            match self.model.api.as_str() {
                "openai-completions" => {
                    ai::openai_completions::stream_simple(&self.model, &context, &options, transport, &mut sink).await
                }
                "openai-responses" => {
                    ai::openai_responses::stream_simple(&self.model, &context, &options, transport, &mut sink).await
                }
                "anthropic-messages" => {
                    ai::anthropic::stream_simple(&self.model, &context, &options, transport, &mut sink).await
                }
                other => Err(format!("frames cannot call {other} models in this host yet")),
            }
        };
        runtime.block_on(async {
            match tokio::time::timeout_at(deadline.into(), stream).await {
                Ok(reply) => reply.map(Some),
                Err(_) => Ok(None),
            }
        })
    }
}

fn user_message(text: &str) -> Message {
    Message::User(ai::UserMessage {
        content: ai::UserMessageContent::Text(text.to_string()),
        timestamp: 0,
    })
}

/// `^[^/\s]+\/[^/\s]+(?:\/[^/\s]+)*$`.
fn is_model_ref(model: &str) -> bool {
    let parts: Vec<&str> = model.split('/').collect();
    parts.len() >= 2
        && parts
            .iter()
            .all(|part| !part.is_empty() && !part.chars().any(char::is_whitespace))
}

fn create_private_dir(dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|error| error.to_string())
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn observation(ledger: &mut Ledger, frame: &Frame, depth: u64, outcome: Outcome) -> Value {
    let mut out = Map::new();
    match outcome {
        Outcome::Complete(value) => {
            out.insert("status".into(), "complete".into());
            out.insert("value".into(), value);
        }
        Outcome::Incomplete(reason, detail) => {
            out.insert("status".into(), "incomplete".into());
            out.insert("reason".into(), reason.into());
            out.insert("detail".into(), detail.into());
        }
        Outcome::Error(error) => {
            out.insert("status".into(), "error".into());
            out.insert("error".into(), error.into());
        }
    }
    let complete = out["status"] == "complete";
    out.insert("trace_id".into(), frame.id.clone().into());
    out.insert("spent".into(), frame.spent(ledger));
    out.insert("remaining".into(), frame.remaining(ledger, depth));
    if !complete {
        out.insert("last_outputs".into(), frame.outputs.clone().into());
    }
    Value::Object(out)
}

impl HostHandler for Inference {
    fn handle(&self, kind: &str, payload: Map<String, Value>, signal: &Signal) -> Result<Value, String> {
        match kind {
            "rlm.infer" => self.infer(&payload, signal),
            "rlm.load" => self.load(&payload),
            "rlm.frames" => self
                .traces
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .list(&payload),
            _ => self.inner.handle(kind, payload, signal),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Kind, Ledger};

    /// BudgetNode: a node's availability is the tightest pool on its path, and holds/charges reach every ancestor.
    #[test]
    fn inference_ledger_takes_the_tightest_pool_on_the_path() {
        let mut ledger = Ledger::default();
        let root = ledger.add(None, Some(10), Some(1000));
        let child = ledger.add(Some(root), Some(3), None);
        assert_eq!(ledger.available(child, Kind::Calls), Some(3));
        assert_eq!(ledger.available(child, Kind::Tokens), Some(1000));
        assert!(ledger.hold(child, Kind::Calls, 2));
        assert!(!ledger.hold(child, Kind::Calls, 2));
        ledger.charge(child, Kind::Calls, 2);
        ledger.release(child, Kind::Calls, 2);
        assert_eq!(ledger.available(child, Kind::Calls), Some(1));
        assert_eq!(ledger.available(root, Kind::Calls), Some(8));
        assert_eq!(ledger.claim(child, 100, 8192, false), Some((350, 250)));
        assert_eq!(ledger.available(root, Kind::Tokens), Some(650));
        assert_eq!(ledger.claim(child, 0, 8192, false), None);
    }
}
