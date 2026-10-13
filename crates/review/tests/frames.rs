//! Replays crates/review/fixtures/frames.json, recorded from autoreview_api.py's `Frames` (fixtures/record_frames.py)
//! on a virtual clock with scripted `rlm.map` replies: requests, results, sleeps, timings, usage, held grants and
//! model limits. Plus the scheduler's concurrency limits under real threads.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use indexmap::IndexMap;
use review::frames::{
    Clock, FrameRequest, FrameResult, FrameRunner, Job, MapReply, Run, Scheduler, Settings, SystemClock,
};
use serde_json::{Value, json};

fn fixture() -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/frames.json");
    serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("fixture json")
}

struct VirtualClock {
    now: Mutex<f64>,
    sleeps: Mutex<Vec<f64>>,
}

impl Clock for VirtualClock {
    fn now(&self) -> f64 {
        *self.now.lock().expect("clock")
    }

    fn sleep(&self, seconds: f64) {
        self.sleeps.lock().expect("sleeps").push(seconds);
        *self.now.lock().expect("clock") += seconds;
    }
}

struct Scripted<'a> {
    script: Mutex<VecDeque<Value>>,
    clock: &'a VirtualClock,
    requests: Mutex<Vec<Value>>,
}

fn int(value: &Value) -> u64 {
    match value {
        Value::String(text) => text.trim().parse().expect("int"),
        other => other.as_u64().unwrap_or(0),
    }
}

impl FrameRunner for Scripted<'_> {
    fn map(&self, request: &FrameRequest) -> Result<MapReply, String> {
        self.requests.lock().expect("requests").push(json!({
            "task": request.task, "item": request.item, "context": request.context, "contract": request.contract,
            "budget": request.budget_tokens, "model": request.model, "thinking": request.thinking,
            "shared_prefix": request.shared_prefix, "concurrency": request.concurrency, "timeout_ms": request.timeout_ms,
        }));
        let step = self
            .script
            .lock()
            .expect("script")
            .pop_front()
            .unwrap_or_else(|| json!({"value": "default"}));
        *self.clock.now.lock().expect("clock") += step.get("seconds").and_then(Value::as_f64).unwrap_or(1.0);
        if let Some(message) = step.get("raise") {
            return Err(format!("RuntimeError: {}", message.as_str().expect("message")));
        }
        let mut results = Vec::new();
        if let Some(value) = step.get("value") {
            results.push(FrameResult::Value(value.clone()));
        } else if let Some(status) = step.get("incomplete") {
            results.push(FrameResult::Incomplete {
                status: status.as_str().expect("status").to_string(),
                detail: "d".to_string(),
            });
        } else if let Some(error) = step.get("error") {
            results.push(FrameResult::Error(error.as_str().expect("error").to_string()));
        }
        let usage = step.get("usage").cloned().unwrap_or(json!({}));
        Ok(MapReply {
            results,
            calls: step.get("calls").map_or(1, int),
            tokens: step.get("tokens").map_or(100, int),
            input_tokens: usage.get("input_tokens").map_or(0, int),
            output_tokens: usage.get("output_tokens").map_or(0, int),
            cost: usage.get("cost").and_then(Value::as_f64).unwrap_or(0.0),
        })
    }
}

fn result_json(result: &FrameResult) -> Value {
    match result {
        FrameResult::Value(value) => json!({ "value": value }),
        FrameResult::Incomplete { status, detail } => json!({ "incomplete": status, "detail": detail }),
        FrameResult::Error(error) => json!({ "error": error }),
    }
}

fn settings(options: &Value) -> Settings {
    let mut settings = Settings {
        concurrency: 1,
        ..Settings::default()
    };
    if let Some(cap) = options.get("cap") {
        settings.cap = cap.as_u64();
    }
    if let Some(base) = options.get("retry_base_s") {
        settings.retry_base_s = base.as_f64().expect("base");
    }
    if let Some(timeout) = options.get("frame_timeout_s") {
        settings.frame_timeout_s = timeout.as_f64().expect("timeout");
    }
    if let Some(prefix) = options.get("system_prefix") {
        settings.system_prefix = prefix.as_bool().expect("prefix");
    }
    if let Some(limits) = options.get("model_concurrency") {
        settings.model_concurrency = limits
            .as_object()
            .expect("limits")
            .iter()
            .map(|(name, limit)| (name.clone(), limit.as_f64().expect("limit")))
            .collect();
    }
    settings
}

#[test]
fn review_frames_scheduler_matches_python() {
    let fx = fixture();
    let rng_values: Vec<f64> = serde_json::from_value(fx["rng"].clone()).expect("rng");
    for case in fx["scenarios"].as_array().expect("scenarios") {
        let name = case["name"].as_str().expect("name");
        let clock = VirtualClock {
            now: Mutex::new(fx["start"].as_f64().expect("start")),
            sleeps: Mutex::new(Vec::new()),
        };
        let runner = Scripted {
            script: Mutex::new(case["script"].as_array().expect("script").iter().cloned().collect()),
            clock: &clock,
            requests: Mutex::new(Vec::new()),
        };
        let values = rng_values.clone();
        let mut draws = 0;
        // Recorded with the Python tasks running one by one: one worker here.
        let scheduler = Scheduler::new(settings(&case["options"]), move || {
            draws += 1;
            values[(draws - 1) % values.len()]
        });
        let mut configured = settings(&case["options"]);
        configured.concurrency = case["options"]["concurrency"].as_u64().map_or(1, |n| n as usize);
        assert_eq!(
            json!(Scheduler::new(configured, || 0.5).concurrency()),
            case["concurrency"],
            "{name}"
        );
        let contract = json!({"type": "string"});
        let mut runs = Vec::new();
        for spec in case["runs_spec"].as_array().expect("runs") {
            let contracts = spec.get("contracts").and_then(Value::as_array);
            let jobs: Vec<Job> = spec["jobs"]
                .as_array()
                .expect("jobs")
                .iter()
                .enumerate()
                .map(|(index, job)| Job {
                    label: job[0].as_str().expect("label").to_string(),
                    task: job[1].as_str().expect("task").to_string(),
                    item: job[2].clone(),
                    contract: contracts.map(|contracts| contracts[index].clone()),
                })
                .collect();
            let cutoff = spec
                .get("cutoff_in")
                .and_then(Value::as_f64)
                .map(|left| clock.now() + left);
            let run = Run {
                phase: spec["phase"].as_str().expect("phase"),
                contract: &contract,
                model: spec["model"].as_str(),
                thinking: spec.get("thinking").and_then(Value::as_str),
                context: spec.get("context"),
                cutoff,
            };
            let results = scheduler.run(run, &jobs, &runner, &clock);
            runs.push(json!(results.iter().map(result_json).collect::<Vec<_>>()));
        }
        assert_eq!(json!(runs), case["results"], "{name}");
        assert_eq!(
            json!(runner.requests.lock().expect("requests").clone()),
            case["requests"],
            "{name}"
        );
        assert_eq!(
            json!(clock.sleeps.lock().expect("sleeps").clone()),
            case["sleeps"],
            "{name}"
        );
        assert_eq!(json!(clock.now()), case["end"], "{name}");
        let timings: Vec<Value> = scheduler.timings().iter().map(|t| t.to_json()).collect();
        assert_eq!(json!(timings), case["timings"], "{name}");
        let usage = scheduler.usage();
        let by_phase: IndexMap<&str, Value> = usage
            .by_phase
            .iter()
            .map(|(phase, (frames, tokens))| (phase.as_str(), json!({"frames": frames, "tokens": tokens})))
            .collect();
        let got = json!({ "frames": usage.frames, "tokens": usage.tokens, "input": usage.input, "output": usage.output,
                          "cost": usage.cost, "by_phase": by_phase });
        assert_eq!(got, case["usage"], "{name}");
        assert_eq!(json!(scheduler.held()), case["held"], "{name}");
        assert_eq!(json!(scheduler.model_limits()), case["model_limits"], "{name}");
        assert_eq!(json!(scheduler.throttled()), case["throttled"], "{name}");
    }
}

/// Counts frames in flight, overall and for one model, and remembers the peaks.
struct Gauge {
    now: AtomicUsize,
    peak: AtomicUsize,
}

impl Gauge {
    fn enter(&self) {
        let now = self.now.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
    }
    fn leave(&self) {
        self.now.fetch_sub(1, Ordering::SeqCst);
    }
}

struct Slow {
    all: Gauge,
    calls: AtomicUsize,
}

impl FrameRunner for Slow {
    fn map(&self, request: &FrameRequest) -> Result<MapReply, String> {
        self.all.enter();
        self.calls.fetch_add(1, Ordering::SeqCst);
        // Hold until a second frame runs alongside (hang guard 120 s), so parallelism shows however slowly threads
        // start under load; then stay a little longer so a scheduler over its bound would overlap more.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        while self.all.peak.load(Ordering::SeqCst) < 2 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
        self.all.leave();
        Ok(MapReply {
            results: vec![FrameResult::Value(request.item.clone())],
            calls: 1,
            tokens: 10,
            ..MapReply::default()
        })
    }
}

#[test]
fn review_frames_scheduler_bounds_concurrency_per_model_under_threads() {
    let gauge = || Gauge {
        now: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
    };
    let jobs: Vec<Job> = (0..12)
        .map(|i| Job {
            label: format!("j{i}"),
            task: "t".into(),
            item: json!(i),
            contract: None,
        })
        .collect();
    let contract = json!({});
    let run = |model| Run {
        phase: "find",
        contract: &contract,
        model,
        thinking: None,
        context: None,
        cutoff: None,
    };
    let clock = SystemClock::default();

    // Six at once overall; results stay in job order.
    let scheduler = Scheduler::new(
        Settings {
            concurrency: 6,
            ..Settings::default()
        },
        || 0.5,
    );
    let runner = Slow {
        all: gauge(),
        calls: AtomicUsize::new(0),
    };
    let results = scheduler.run(run(Some("m/a")), &jobs, &runner, &clock);
    assert_eq!(
        results,
        (0..12).map(|i| FrameResult::Value(json!(i))).collect::<Vec<_>>()
    );
    assert!(runner.all.peak.load(Ordering::SeqCst) <= 6);
    assert!(runner.all.peak.load(Ordering::SeqCst) >= 2, "frames ran in parallel");

    // A model limit of 2 holds below the overall 16.
    let mut limits = IndexMap::new();
    limits.insert("m/b".to_string(), 2.0);
    let scheduler = Scheduler::new(
        Settings {
            concurrency: 16,
            model_concurrency: limits,
            ..Settings::default()
        },
        || 0.5,
    );
    let runner = Slow {
        all: gauge(),
        calls: AtomicUsize::new(0),
    };
    scheduler.run(run(Some("m/b")), &jobs, &runner, &clock);
    assert_eq!(runner.all.peak.load(Ordering::SeqCst), 2);
    assert_eq!(runner.calls.load(Ordering::SeqCst), 12);
    assert_eq!(scheduler.usage().tokens, 120);
    assert_eq!(scheduler.timings().len(), 12);
}
