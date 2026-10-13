//! Autoreview's frame scheduler (`autoreview_api.py` `Frames`), as a host operation: it runs a review's frames, one
//! `rlm.map` request each, with bounded concurrency (overall and per model; a model the provider throttles has its
//! limit halved for the rest of the review), a per-frame timeout and the review's deadline, a token grant out of the
//! review's cap, retries for transient failures (the classification in [`crate::levels`]), and a timing record per
//! frame. The frame itself goes through a [`FrameRunner`]: the host serves it with the inference crate's `rlm.map`
//! (one item, `concurrency=1`); tests script it. Time goes through a [`Clock`], randomness through `rng`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use indexmap::IndexMap;
use serde_json::{Value, json};

use crate::findings::text;
use crate::levels::{AUTH_RETRY_S, DEADLINE_ERROR, is_auth_failure, is_throttled, is_transient, retry_after};

pub const DEFAULT_CONCURRENCY: usize = 8;
pub const MAX_CONCURRENCY: usize = 16;
pub const DEFAULT_MODEL_CONCURRENCY: usize = 8;
pub const MAX_MODEL_CONCURRENCY: usize = 16;
pub const HOST_MAX_TIMEOUT_MS: u64 = 60 * 60 * 1000;
/// A frame is not started with less than this left before the deadline.
pub const MIN_START_S: f64 = 3.0;
/// Tokens granted to one frame beyond its input estimate.
pub const FRAME_GRANT: i64 = 32_000;
/// A frame needs at least this much room for its reply.
pub const MIN_FRAME_OUTPUT: i64 = 2_000;
pub const MAX_RETRIES: u32 = 2;
pub const DEFAULT_RETRY_BASE_S: f64 = 2.0;
pub const MAX_RETRY_WAIT_S: f64 = 30.0;

/// What one frame produced: a value, an `Incomplete` (budget or repairs ran out) or a `FrameError`.
#[derive(Debug, Clone, PartialEq)]
pub enum FrameResult {
    Value(Value),
    Incomplete { status: String, detail: String },
    Error(String),
}

/// One `rlm.map` request: a single item.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameRequest {
    pub task: String,
    pub item: Value,
    /// The frames' shared views, `None` when there are none.
    pub context: Option<Value>,
    pub contract: Value,
    /// `Budget(tokens=...)`; `None` without a cap.
    pub budget_tokens: Option<u64>,
    pub model: Option<String>,
    pub thinking: Option<String>,
    /// Send the shared views as the system prompt.
    pub shared_prefix: bool,
    pub concurrency: u64,
    pub timeout_ms: u64,
}

/// What `rlm.map` returned: the results and what they spent.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MapReply {
    pub results: Vec<FrameResult>,
    pub calls: u64,
    pub tokens: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost: f64,
}

/// Runs one `rlm.map` request. An `Err` is a host or bridge failure (`"Type: message"`): a failed frame, not a
/// failed review.
pub trait FrameRunner: Sync {
    fn map(&self, request: &FrameRequest) -> Result<MapReply, String>;
}

/// Monotonic seconds and sleeping.
pub trait Clock: Sync {
    fn now(&self) -> f64;
    fn sleep(&self, seconds: f64);
}

/// The process's monotonic clock.
pub struct SystemClock {
    start: Instant,
}

impl Default for SystemClock {
    fn default() -> Self {
        Self { start: Instant::now() }
    }
}

impl Clock for SystemClock {
    fn now(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }

    fn sleep(&self, seconds: f64) {
        std::thread::sleep(Duration::from_secs_f64(seconds.max(0.0)));
    }
}

/// Frames and tokens, overall and by phase; input, output and cost as the provider reported them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Usage {
    pub frames: u64,
    pub tokens: u64,
    pub input: u64,
    pub output: u64,
    pub cost: f64,
    pub by_phase: IndexMap<String, (u64, u64)>,
}

impl Usage {
    fn add(&mut self, reply: &MapReply, phase: &str) {
        self.frames += reply.calls;
        self.tokens += reply.tokens;
        if !phase.is_empty() {
            let entry = self.by_phase.entry(phase.to_string()).or_insert((0, 0));
            entry.0 += reply.calls;
            entry.1 += reply.tokens;
        }
        self.input += reply.input_tokens;
        self.output += reply.output_tokens;
        self.cost += reply.cost;
    }
}

/// How one frame went, for the report.
#[derive(Debug, Clone, PartialEq)]
pub struct Timing {
    pub phase: String,
    pub reviewer: String,
    pub ms: i64,
    /// `ok`, `budget`, `deadline`, `incomplete`, `timeout` or `failed`.
    pub status: &'static str,
    pub retries: u32,
    pub tokens: u64,
    pub model: Option<String>,
    pub error: Option<String>,
}

impl Timing {
    pub fn to_json(&self) -> Value {
        let mut out = json!({ "phase": self.phase, "reviewer": self.reviewer, "ms": self.ms, "status": self.status,
                              "retries": self.retries, "tokens": self.tokens });
        if let Some(model) = &self.model {
            out["model"] = json!(model);
        }
        if let Some(error) = &self.error {
            out["error"] = json!(error);
        }
        out
    }
}

/// One frame to run: a label (the reviewer), its task and item, and a contract overriding the run's.
#[derive(Debug, Clone, PartialEq)]
pub struct Job {
    pub label: String,
    pub task: String,
    pub item: Value,
    pub contract: Option<Value>,
}

/// What every frame of one run shares.
#[derive(Debug, Clone, Copy)]
pub struct Run<'a> {
    pub phase: &'a str,
    pub contract: &'a Value,
    pub model: Option<&'a str>,
    pub thinking: Option<&'a str>,
    /// The shared views (a list) or text; empty means none.
    pub context: Option<&'a Value>,
    /// The deadline on the scheduler's clock.
    pub cutoff: Option<f64>,
}

/// Settings of a scheduler.
#[derive(Debug, Clone)]
pub struct Settings {
    /// The review's token cap; `None`: nothing is refused or granted.
    pub cap: Option<u64>,
    pub concurrency: usize,
    /// Frames in flight per model (`autoreview.modelConcurrency`).
    pub model_concurrency: IndexMap<String, f64>,
    /// 0: no timeout of its own.
    pub frame_timeout_s: f64,
    pub retry_base_s: f64,
    pub system_prefix: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            cap: None,
            concurrency: DEFAULT_CONCURRENCY,
            model_concurrency: IndexMap::new(),
            frame_timeout_s: 0.0,
            retry_base_s: DEFAULT_RETRY_BASE_S,
            system_prefix: false,
        }
    }
}

#[derive(Default)]
struct State {
    usage: Usage,
    held: i64,
    model_limits: IndexMap<String, usize>,
    throttled: IndexMap<String, usize>,
    in_flight: HashMap<String, usize>,
    busy: usize,
    timings: Vec<Timing>,
}

/// The review's frame scheduler. Shared by every phase of one review (usage, limits and timings accumulate).
pub struct Scheduler {
    cap: Option<u64>,
    concurrency: usize,
    frame_timeout_s: f64,
    retry_base_s: f64,
    system_prefix: bool,
    state: Mutex<State>,
    changed: Condvar,
    rng: Mutex<Box<dyn FnMut() -> f64 + Send>>,
}

/// Python `len()` summed the way `_item_chars` does: a string's characters, or its parts' lengths.
fn item_chars(item: &Value) -> i64 {
    let len = |value: &Value| match value {
        Value::String(text) => text.chars().count(),
        Value::Array(items) => items.len(),
        Value::Object(map) => map.len(),
        _ => 0,
    };
    match item {
        Value::String(text) => text.chars().count() as i64,
        Value::Array(items) => items.iter().map(len).sum::<usize>() as i64,
        Value::Object(map) => map.keys().map(|key| key.chars().count()).sum::<usize>() as i64,
        _ => 0,
    }
}

impl Scheduler {
    pub fn new(settings: Settings, rng: impl FnMut() -> f64 + Send + 'static) -> Self {
        let model_limits = settings
            .model_concurrency
            .iter()
            .map(|(name, limit)| {
                (
                    name.clone(),
                    (limit.trunc().max(1.0) as usize).min(MAX_MODEL_CONCURRENCY),
                )
            })
            .collect();
        Self {
            cap: settings.cap,
            concurrency: settings.concurrency.clamp(1, MAX_CONCURRENCY),
            frame_timeout_s: settings.frame_timeout_s,
            retry_base_s: settings.retry_base_s,
            system_prefix: settings.system_prefix,
            state: Mutex::new(State {
                model_limits,
                ..State::default()
            }),
            changed: Condvar::new(),
            rng: Mutex::new(Box::new(rng)),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn concurrency(&self) -> usize {
        self.concurrency
    }

    pub fn usage(&self) -> Usage {
        self.lock().usage.clone()
    }

    /// Tokens granted to frames in flight.
    pub fn held(&self) -> i64 {
        self.lock().held
    }

    pub fn timings(&self) -> Vec<Timing> {
        self.lock().timings.clone()
    }

    /// Per-model in-flight limits: configured, or lowered after throttling.
    pub fn model_limits(&self) -> IndexMap<String, usize> {
        self.lock().model_limits.clone()
    }

    /// Models the provider throttled, with their lowered limits.
    pub fn throttled(&self) -> IndexMap<String, usize> {
        self.lock().throttled.clone()
    }

    fn limit_of(state: &State, key: &str) -> usize {
        state
            .model_limits
            .get(key)
            .copied()
            .unwrap_or(DEFAULT_MODEL_CONCURRENCY)
    }

    /// Wait for a free slot overall and under the model's in-flight limit.
    fn admit(&self, key: &str) {
        let mut state = self.lock();
        while state.busy >= self.concurrency
            || state.in_flight.get(key).copied().unwrap_or(0) >= Self::limit_of(&state, key)
        {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        state.busy += 1;
        *state.in_flight.entry(key.to_string()).or_insert(0) += 1;
    }

    fn release(&self, key: &str) {
        let mut state = self.lock();
        state.busy = state.busy.saturating_sub(1);
        let count = state.in_flight.entry(key.to_string()).or_insert(0);
        *count = count.saturating_sub(1);
        self.changed.notify_all();
    }

    /// Run `jobs` and return their results in order: a value, an `Incomplete` (the cap is spent) or a `FrameError`
    /// (failed after retries, or unfinished at the cutoff).
    pub fn run(&self, run: Run<'_>, jobs: &[Job], runner: &dyn FrameRunner, clock: &dyn Clock) -> Vec<FrameResult> {
        let results: Vec<Mutex<Option<FrameResult>>> = jobs.iter().map(|_| Mutex::new(None)).collect();
        let next = AtomicUsize::new(0);
        let workers = self.concurrency.min(jobs.len()).max(1);
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| {
                    loop {
                        let index = next.fetch_add(1, Ordering::SeqCst);
                        let Some(job) = jobs.get(index) else { break };
                        let result = self.one(run, job, runner, clock);
                        *results[index].lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(result);
                    }
                });
            }
        });
        results
            .into_iter()
            .map(|slot| {
                slot.into_inner()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .unwrap_or_else(|| FrameResult::Error("the frame did not run".to_string()))
            })
            .collect()
    }

    fn one(&self, run: Run<'_>, job: &Job, runner: &dyn FrameRunner, clock: &dyn Clock) -> FrameResult {
        let context = run.context.filter(|context| crate::levels::truthy(Some(context)));
        let estimate = (job.task.chars().count() as i64 + item_chars(&job.item) + context.map_or(0, item_chars)) / 3;
        let key = run.model.unwrap_or("");
        self.admit(key);
        let began = clock.now();
        let mut retries = 0;
        let mut tokens = 0;
        let (result, status) = loop {
            let left = run.cutoff.map(|cutoff| cutoff - clock.now());
            if left.is_some_and(|left| left < MIN_START_S) {
                break (FrameResult::Error(DEADLINE_ERROR.to_string()), "deadline");
            }
            // Refused only when what was really spent plus what the frames in flight may still spend leaves no room
            // for this one's input and a reply.
            let mut grant = 0;
            if let Some(cap) = self.cap {
                let mut state = self.lock();
                grant = (estimate + FRAME_GRANT).min(cap as i64 - state.usage.tokens as i64 - state.held);
                if grant < estimate + MIN_FRAME_OUTPUT {
                    break (
                        FrameResult::Incomplete {
                            status: "budget_exhausted".to_string(),
                            detail: "the review's token cap is spent".to_string(),
                        },
                        "budget",
                    );
                }
                state.held += grant;
            }
            let own = (self.frame_timeout_s > 0.0).then_some(self.frame_timeout_s);
            let timeout_s = [own, left, Some(HOST_MAX_TIMEOUT_MS as f64 / 1000.0)]
                .into_iter()
                .flatten()
                .fold(f64::INFINITY, f64::min);
            let at_cutoff = left.is_some_and(|left| own.is_none_or(|own| left <= own));
            let request = FrameRequest {
                task: job.task.clone(),
                item: job.item.clone(),
                context: context.cloned(),
                contract: job.contract.clone().unwrap_or_else(|| run.contract.clone()),
                budget_tokens: self.cap.map(|_| grant.max(0) as u64),
                model: run.model.map(String::from),
                thinking: run.thinking.map(String::from),
                shared_prefix: self.system_prefix && context.is_some(),
                concurrency: 1,
                timeout_ms: ((timeout_s * 1000.0) as u64).max(1_000),
            };
            let reply = runner.map(&request);
            let result = {
                let mut state = self.lock();
                state.held -= grant;
                match reply {
                    Ok(reply) => {
                        state.usage.add(&reply, run.phase);
                        tokens += reply.tokens;
                        reply
                            .results
                            .into_iter()
                            .next()
                            .unwrap_or_else(|| FrameResult::Error("the frame returned nothing".to_string()))
                    }
                    Err(error) => FrameResult::Error(error),
                }
            };
            let error = match result {
                FrameResult::Incomplete { .. } => break (result, "incomplete"),
                FrameResult::Value(_) => break (result, "ok"),
                FrameResult::Error(error) => error,
            };
            let (error, status) = if error == "cancelled" {
                // The host cancels a frame that outlives its timeout.
                if at_cutoff {
                    break (FrameResult::Error(DEADLINE_ERROR.to_string()), "deadline");
                }
                (format!("timed out after {timeout_s:.0} s"), "timeout")
            } else {
                (error, "failed")
            };
            if is_throttled(&error) {
                let mut state = self.lock();
                let limit = (Self::limit_of(&state, key) / 2).max(1);
                state.model_limits.insert(key.to_string(), limit);
                state.throttled.insert(key.to_string(), limit);
            }
            let transient = is_transient(&error);
            if !transient || retries >= MAX_RETRIES {
                break (FrameResult::Error(error), status);
            }
            let base = if is_auth_failure(&error) && self.retry_base_s > 0.0 {
                AUTH_RETRY_S
            } else {
                self.retry_base_s
            };
            let delay = match retry_after(&error) {
                Some(hint) => hint,
                None => {
                    let draw = (self.rng.lock().unwrap_or_else(|poisoned| poisoned.into_inner()))();
                    base * 2f64.powi(retries as i32) * (0.5 + draw)
                }
            }
            .min(MAX_RETRY_WAIT_S);
            if run
                .cutoff
                .is_some_and(|cutoff| clock.now() + delay + MIN_START_S >= cutoff)
            {
                break (FrameResult::Error(error), status);
            }
            retries += 1;
            clock.sleep(delay);
        };
        let error = match &result {
            FrameResult::Error(error) => Some(text(Some(&Value::from(error.as_str())), 200)),
            _ => None,
        };
        let timing = Timing {
            phase: run.phase.to_string(),
            reviewer: job.label.clone(),
            ms: ((clock.now() - began) * 1000.0) as i64,
            status,
            retries,
            tokens,
            model: run.model.filter(|model| !model.is_empty()).map(String::from),
            error,
        };
        self.lock().timings.push(timing);
        self.release(key);
        result
    }
}
