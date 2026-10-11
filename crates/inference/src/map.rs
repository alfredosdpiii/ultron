//! `rlm.map`, `rlm.models` and the frame-model settings (inference.ts, model-settings.ts). `Frames` wraps
//! `Inference`: a map runs one frame per item (up to `concurrency` at once, results in order) under one shared budget node (a top-level map without a
//! token limit gets `ULTRON_RLM_MAP_TOKENS`, default 500,000); every frame's first call is held at admission, so the
//! shared pool decides deterministically which frames run. Code-free frames that name no model take
//! ULTRON_RLM_FRAME_MODEL, else `rlm.frameModel`; `rlm.frameThinking` is the default thinking level.

use super::{
    Budget, DEFAULT_REPAIRS, DEFAULT_SCALAR_REPAIRS, DEFAULT_TIMEOUT_MS, Frame, Inference, Kind, Ledger, MAX_DEPTH,
    MAX_REPAIRS, MAX_SAFE_INTEGER, MAX_TASK_CHARS, MAX_TIMEOUT_MS, Options, Outcome, THINKING_LEVELS, fields,
    is_model_ref, is_plain_type, js_len, js_normalize, js_trim, lock, observation, optional_integer, schema, trace,
};
use kernel::{HostHandler, Signal};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

pub const DEFAULT_MAP_TOKENS: u64 = 500_000;
const MAX_MAP_FRAMES: usize = 10_000;
const MAX_CONCURRENCY: u64 = 16;
const FRAME_MODEL_ENV: &str = "ULTRON_RLM_FRAME_MODEL";
const REVIEW_MODEL_ENV: &str = "ULTRON_REVIEW_MODEL";
const MAP_TOKENS_ENV: &str = "ULTRON_RLM_MAP_TOKENS";

/// The bounded-inference API description for the model's runtime guide.
pub const INFERENCE_PROMPT: &str = "Large inputs stay out of your context as handles; examples: `help(rlm.load)`, `help(rlm.map)`.
- `h = await rlm.load(path_or_text)`: `h.search(regex)`, `h.count(regex)`, `h.lines(a, b)`, `h.chunks(n)`; views print their text.
- `await rlm.infer(task, context=[views], contract=T, budget=Budget(calls, tokens, depth))`: one private sub-model call (no transcript, no tools), contract-validated and re-asked; running out returns a falsy `Incomplete`.
- `await rlm.map(task, items, contract=T)`: one frame per item, in order, under one shared budget (default 500,000 tokens; `budget=Budget(tokens=...)` for more).
- Every frame is a model request: filter with code first, give each frame only the passage it must judge, and compute exact numbers in Python.";

/// The saved model settings (`/settings -> Models`), read for every request so a change applies to the next frame.
#[derive(Clone, Debug, Default)]
pub struct ModelSettings {
    pub frame_model: Option<String>,
    pub frame_thinking: Option<String>,
    pub child_model: Option<String>,
    pub review_model: Option<String>,
}

/// Serves `rlm.map`, `rlm.models` and the frame-model defaults of `rlm.infer`; everything else goes to `inference`.
pub struct Frames {
    inference: Inference,
    env: HashMap<String, String>,
    settings: Box<dyn Fn() -> ModelSettings + Send + Sync>,
}

impl Frames {
    pub fn new(
        inference: Inference,
        env: HashMap<String, String>,
        settings: impl Fn() -> ModelSettings + Send + Sync + 'static,
    ) -> Self {
        Self {
            inference,
            env,
            settings: Box::new(settings),
        }
    }

    /// `envModel`: a `provider/model` value of an environment variable, else none.
    fn env_model(&self, name: &str) -> Option<String> {
        let value = js_trim(self.env.get(name)?);
        is_model_ref(value).then(|| value.to_string())
    }

    /// `effectiveFrameModel` as `{model?, source, env?}`.
    fn frame_model(&self, settings: &ModelSettings) -> Value {
        match (self.env_model(FRAME_MODEL_ENV), &settings.frame_model) {
            (Some(model), _) => json!({"model": model, "source": "env", "env": FRAME_MODEL_ENV}),
            (None, Some(model)) => json!({"model": model, "source": "setting"}),
            (None, None) => json!({"source": "session"}),
        }
    }

    /// `rlm.models`: the models frames, /review frames and sub-agents take when a call names none, and why.
    fn models(&self) -> Value {
        let settings = (self.settings)();
        let frame = self.frame_model(&settings);
        let review = match (self.env_model(REVIEW_MODEL_ENV), &settings.review_model) {
            (Some(model), _) => json!({"model": model, "source": "env", "env": REVIEW_MODEL_ENV}),
            (None, Some(model)) => json!({"model": model, "source": "setting"}),
            (None, None) => match frame.get("model") {
                Some(model) => json!({"model": model, "source": "frame"}),
                None => frame.clone(),
            },
        };
        let child = match &settings.child_model {
            Some(model) => json!({"model": model, "source": "setting"}),
            None => json!({"source": "session"}),
        };
        json!({"frame": frame, "review": review, "child": child, "frameThinking": settings.frame_thinking})
    }

    /// `defaultMapTokens`: ULTRON_RLM_MAP_TOKENS when it is a positive safe integer (JS `Number`), else 500,000.
    fn default_map_tokens(&self) -> u64 {
        let value = self.env.get(MAP_TOKENS_ENV).map(|v| js_trim(v).parse::<f64>());
        match value {
            Some(Ok(n)) if n.fract() == 0.0 && n > 0.0 && n <= MAX_SAFE_INTEGER as f64 => n as u64,
            _ => DEFAULT_MAP_TOKENS,
        }
    }

    /// A code-free frame (depth 1) that names no model takes the frame model; no `thinking=` takes `rlm.frameThinking`.
    fn defaults(&self, payload: &mut Map<String, Value>) {
        let settings = (self.settings)();
        let depth = payload
            .get("budget")
            .and_then(|b| b.get("depth"))
            .filter(|d| !d.is_null());
        let unset = |payload: &Map<String, Value>, key: &str| payload.get(key).is_none_or(Value::is_null);
        if unset(payload, "model")
            && depth.is_none_or(|d| d.as_f64() == Some(1.0))
            && let Some(model) = self.frame_model(&settings).get("model")
        {
            payload.insert("model".into(), model.clone());
        }
        if unset(payload, "thinking")
            && let Some(level) = settings.frame_thinking
        {
            payload.insert("thinking".into(), level.into());
        }
    }

    fn map(&self, payload: &mut Map<String, Value>, signal: &Signal) -> Result<Value, String> {
        fields(
            payload,
            &[
                "frames",
                "prefix",
                "contract",
                "budget",
                "model",
                "max_repairs",
                "concurrency",
                "timeout_ms",
                "thinking",
            ],
        )?;
        let items = match payload.get("frames") {
            Some(Value::Array(items)) => items.clone(),
            _ => return Err("frames must be a list".into()),
        };
        if items.len() > MAX_MAP_FRAMES {
            return Err(format!("rlm.map takes at most {MAX_MAP_FRAMES} frames"));
        }
        let concurrency = optional_integer(payload.get("concurrency"), "concurrency", 1, MAX_CONCURRENCY)?.unwrap_or(8);
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
        if !payload.get("model").is_none_or(Value::is_null)
            && !payload.get("model").and_then(Value::as_str).is_some_and(is_model_ref)
        {
            return Err("model must be provider/model".into());
        }
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
        let given = payload.get("thinking").filter(|t| !t.is_null());
        if given.is_some_and(|t| !t.as_str().is_some_and(|t| THINKING_LEVELS.contains(&t))) {
            return Err(format!("thinking must be one of {}", THINKING_LEVELS.join(", ")));
        }
        self.defaults(payload);
        let model = payload.get("model").and_then(Value::as_str).map(str::to_string);
        let thinking = payload.get("thinking").and_then(Value::as_str).map(str::to_string);
        let default_tokens = tokens.is_none();
        let tokens = tokens.or_else(|| Some(self.default_map_tokens()));
        if !self.inference.views(payload.get("prefix"))?.is_empty() {
            return Err(
                "rlm.map's shared_prefix is not supported by this host yet; pass the shared context per frame".into(),
            );
        }
        // Validate every item before any frame runs or any budget is touched.
        let mut requests = Vec::new();
        for item in &items {
            let item = item.as_object().ok_or("frame must be an object")?;
            let task = item
                .get("task")
                .and_then(Value::as_str)
                .filter(|task| !js_trim(task).is_empty())
                .ok_or("task must be a nonempty string")?;
            if js_len(task) > MAX_TASK_CHARS {
                return Err(format!(
                    "task exceeds {MAX_TASK_CHARS} characters; pass data as context"
                ));
            }
            requests.push((task, self.inference.views(item.get("context"))?));
        }
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
            default_tokens,
        };
        // Each frame's first call is claimed in order at admission, so the shared pool decides deterministically
        // which frames run.
        let frames: Vec<(Mutex<Frame>, bool)> = requests
            .into_iter()
            .map(|(task, views)| {
                let mut frame = Frame::new(&mut ledger, &budget, task, views);
                frame.kind = "map";
                frame.batch = items.len();
                let held = ledger.hold(budget.node, Kind::Calls, 1);
                (Mutex::new(frame), held)
            })
            .collect();
        let options = Options {
            contract,
            max_repairs,
            thinking,
            timeout: Duration::from_millis(timeout.unwrap_or(DEFAULT_TIMEOUT_MS)),
        };
        let ledger = Mutex::new(ledger);
        let outcomes: Vec<Mutex<Option<Outcome>>> = frames.iter().map(|_| Mutex::new(None)).collect();
        // Up to `concurrency` workers take frames in order; concurrent frames' holds shrink each other's tranches.
        let next = AtomicUsize::new(0);
        let worker = || {
            loop {
                let at = next.fetch_add(1, Ordering::SeqCst);
                let Some((frame, held)) = frames.get(at) else { break };
                let mut frame = lock(frame);
                let outcome = self
                    .inference
                    .run_frame(&ledger, &mut frame, *held, model.as_deref(), &options, signal);
                let outcome = self
                    .inference
                    .finish(&ledger, &budget, &frame, &options, model.as_deref(), outcome);
                *lock(&outcomes[at]) = Some(outcome);
            }
        };
        let workers = (concurrency as usize).min(frames.len());
        std::thread::scope(|scope| {
            for _ in 1..workers {
                scope.spawn(worker);
            }
            worker();
        });
        // Observations once every frame is done (their `remaining` is the pool's final state).
        let mut ledger = lock(&ledger);
        let mut results = Vec::new();
        let mut usage = (0, 0, 0.0);
        for ((frame, _), outcome) in frames.iter().zip(outcomes) {
            let frame = lock(frame);
            let outcome = outcome.into_inner().unwrap_or_else(|poisoned| poisoned.into_inner());
            let outcome = outcome.unwrap_or_else(|| Outcome::Error("frame did not run".into()));
            results.push(observation(&mut ledger, &frame, depth, outcome));
            let measured = frame.measured;
            usage = (usage.0 + measured.0, usage.1 + measured.1, usage.2 + measured.2);
        }
        let mut reply = budget.snapshot(&ledger);
        reply["remaining"] = json!({
            "calls": ledger.available(budget.node, Kind::Calls),
            "tokens": ledger.available(budget.node, Kind::Tokens),
            "depth": depth,
        });
        Ok(json!({
            "results": results,
            "budget": reply,
            "usage": js_normalize(json!({"input_tokens": usage.0, "output_tokens": usage.1, "cost": usage.2})),
        }))
    }
}

impl HostHandler for Frames {
    fn handle(&self, kind: &str, mut payload: Map<String, Value>, signal: &Signal) -> Result<Value, String> {
        match kind {
            "rlm.map" => self.map(&mut payload, signal),
            "rlm.models" => Ok(self.models()),
            "rlm.infer" => {
                self.defaults(&mut payload);
                self.inference.handle(kind, payload, signal)
            }
            _ => self.inference.handle(kind, payload, signal),
        }
    }
}
