//! Replays fixtures/agents.json (recorded by fixtures/generate-agents.mjs from NativeRlmHost with the same scripted
//! adapters and lanes): every request's result or error, the detached-end events, the adapter calls, the lane prompts
//! and their models, and the final task journal, definition document and usage ledger must match, as JSON text.
//! Task ids and idempotency keys are renamed by their position in the journal on both sides; workflow run ids by
//! first appearance. Prompts and adapter calls are compared as multisets (parallel workflow nodes run concurrently).

use delegation::agents::{AdapterRequest, Agents, AgentsOptions, LaneReply, LaneRun, TaskEnd};
use delegation::task_store::TaskStore;
use delegation::usage::{UsageLedger, UsageLimits, UsageOptions};
use kernel_apis::TaskHost;
use regex::Regex;
use serde_json::{Map, Value, json};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

fn fixture() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/agents.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[derive(Clone, Default)]
struct Shared(Arc<Mutex<Option<Value>>>);

impl Shared {
    fn get(&self) -> Value {
        self.0.lock().unwrap().clone().unwrap_or(Value::Null)
    }
}

impl TaskStore for Shared {
    fn read(&mut self) -> Result<Option<Value>, String> {
        Ok(self.0.lock().unwrap().clone())
    }
    fn write(&mut self, document: &Value) -> Result<(), String> {
        *self.0.lock().unwrap() = Some(document.clone());
        Ok(())
    }
}

fn doubled(n: &Value) -> Value {
    match n.as_f64() {
        Some(n) if n < 0.0 => json!({ "doubled": "invalid" }),
        Some(n) => json!({ "doubled": (n * 2.0) as i64 }),
        None => json!({ "doubled": null }),
    }
}

fn record(calls: &Mutex<Vec<Value>>, strategy: &str, request: &AdapterRequest<'_>) {
    calls.lock().unwrap().push(json!({
        "strategy": strategy, "definition": request.definition["id"], "input": request.input,
        "repair": request.repair.clone().unwrap_or(Value::Null),
    }));
}

/// The scripted lanes of the generator.
fn lane(prompts: &Mutex<Vec<Value>>, host: &OnceLock<Agents>, run: &LaneRun) -> String {
    prompts
        .lock()
        .unwrap()
        .push(json!({ "lane": run.lane, "prompt": run.prompt, "model": run.model }));
    let prompt = run.prompt.as_str();
    if run.lane.starts_with("ultron.rlm-child.") {
        if let Some(rest) = prompt.strip_prefix("finish:") {
            let status: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
            let payload = json!({ "status": status, "summary": format!("did {status}"),
                                  "evidence": ["cargo test: exit 0, 4 passed"], "changed_files": [] });
            if let Err(error) = host.get().unwrap().handle("rlm.finish", payload, &run.lane, None) {
                prompts
                    .lock()
                    .unwrap()
                    .push(json!({ "lane": run.lane, "finishError": error }));
            }
        }
        return if prompt == "empty" {
            String::new()
        } else {
            format!("reply to {prompt}")
        };
    }
    if run.lane.starts_with("ultron.background-job.") {
        return format!("done: {prompt}");
    }
    let Some(at) = prompt.find("Input data:\n") else {
        return "unscripted".into();
    };
    let line = prompt[at + 12..].lines().next().unwrap_or_default();
    let input: Value = serde_json::from_str(line).unwrap();
    let value = doubled(&input["n"]).to_string();
    if input["n"] == 3 {
        format!("```json\n{value}\n```")
    } else {
        value
    }
}

struct Run {
    agents: Agents,
    events: std::sync::mpsc::Receiver<TaskEnd>,
    calls: Arc<Mutex<Vec<Value>>>,
    prompts: Arc<Mutex<Vec<Value>>>,
    tasks: Shared,
    definitions: Shared,
    usage: Shared,
}

fn host(scenario: &Value) -> Run {
    let (tasks, definitions, usage_doc) = (Shared::default(), Shared::default(), Shared::default());
    let calls = Arc::new(Mutex::new(Vec::new()));
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let cell: Arc<OnceLock<Agents>> = Arc::new(OnceLock::new());
    let mut options = AgentsOptions::new(Box::new(tasks.clone()));
    options.definition_store = Some(Box::new(definitions.clone()));
    options.now = Arc::new(|| 1000.0);
    if scenario["ledger"] == true {
        let counter = Arc::new(Mutex::new(0));
        let limits = &scenario["limits"];
        options.usage = Some(
            UsageLedger::new(
                Box::new(usage_doc.clone()),
                UsageOptions {
                    limits: UsageLimits {
                        max_admitted_tasks: limits["maxAdmittedTasks"].as_u64(),
                        max_wall_ms: limits["maxWallMs"].as_u64(),
                        ..UsageLimits::default()
                    },
                    now: Arc::new(|| 1000.0),
                    ids: Box::new(move || {
                        let mut n = counter.lock().unwrap();
                        *n += 1;
                        format!("id-{n}")
                    }),
                    ..UsageOptions::default()
                },
            )
            .unwrap(),
        );
    }
    let sink = calls.clone();
    options.deterministic = Some(Arc::new(move |request: AdapterRequest<'_>| {
        record(&sink, "deterministic", &request);
        let input = request.input;
        match request.definition["id"].as_str().unwrap() {
            "det" | "doubler" => Ok(doubled(&input["n"])),
            "step" if input["fail"].as_bool() == Some(true) => Err("step crashed".into()),
            "step" => Ok(input.clone()),
            "draft" => Ok(json!({ "rev": input.get("round").cloned().unwrap_or(json!(1)) })),
            "review" => Ok(json!({ "approved": input["rev"].as_f64().unwrap_or(0.0) >= 2.0 })),
            "check-pass" => Ok(json!({ "passed": true })),
            other => Err(format!("no deterministic script for {other}")),
        }
    }));
    let sink = calls.clone();
    options.predict = Some(Arc::new(move |request: AdapterRequest<'_>| {
        record(&sink, "predict", &request);
        let n = &request.input["n"];
        match request.definition["id"].as_str().unwrap() {
            "pred" => Ok(doubled(n)),
            "judge" if n == 5 => Err("provider refused".into()),
            "judge" if n == 2 && request.repair.is_some() => Ok(json!({ "doubled": 4 })),
            "judge" => Ok(json!({ "doubled": -4 })),
            other => Err(format!("no predict script for {other}")),
        }
    }));
    let (sink, handle) = (prompts.clone(), cell.clone());
    options.lanes = Some(Arc::new(move |run: LaneRun| {
        Ok(LaneReply {
            text: lane(&sink, &handle, &run),
            usage: None,
            turns: 1,
            tool_calls: 0,
        })
    }));
    let (agents, events) = Agents::new(options);
    let _ = cell.set(agents.clone());
    Run {
        agents,
        events,
        calls,
        prompts,
        tasks,
        definitions,
        usage: usage_doc,
    }
}

/// Waits until every journal task has its terminal result (detached work ends before the next request).
fn quiesce(agents: &Agents) {
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        let tasks = agents.tasks().unwrap_or_default();
        if tasks.iter().all(|t| t.get("result").is_some()) {
            std::thread::sleep(Duration::from_millis(20));
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("tasks did not end");
}

/// Renames task ids and uuid keys by journal position, then workflow run ids by first appearance.
fn canonical(text: &str, journal: &Value, task_pattern: &Regex, key_pattern: &Regex) -> String {
    let mut names: Vec<(String, String)> = Vec::new();
    for (i, task) in journal["tasks"].as_array().into_iter().flatten().enumerate() {
        names.push((task["id"].as_str().unwrap().into(), format!("T{i}")));
        let key = task["key"].as_str().unwrap();
        if key_pattern.is_match(key) {
            names.push((key.into(), format!("K{i}")));
        }
    }
    let renamed = task_pattern.replace_all(text, |c: &regex::Captures<'_>| {
        names
            .iter()
            .find(|(from, _)| from == &c[0])
            .map_or_else(|| c[0].to_string(), |(_, to)| to.clone())
    });
    let renamed = key_pattern.replace_all(&renamed, |c: &regex::Captures<'_>| {
        names
            .iter()
            .find(|(from, _)| from == &c[0])
            .map_or_else(|| c[0].to_string(), |(_, to)| to.clone())
    });
    let runs = Regex::new(r"wf-(#\d+|[0-9a-f]{8})\b").unwrap();
    let mut seen: Vec<String> = Vec::new();
    runs.replace_all(&renamed, |c: &regex::Captures<'_>| {
        let at = seen.iter().position(|s| s == &c[0]).unwrap_or_else(|| {
            seen.push(c[0].to_string());
            seen.len() - 1
        });
        format!("RUN{at}")
    })
    .into_owned()
}

/// The texts around their first difference.
fn first_difference(a: &str, b: &str) -> String {
    let at = a
        .bytes()
        .zip(b.bytes())
        .position(|(x, y)| x != y)
        .unwrap_or(a.len().min(b.len()));
    let from = at.saturating_sub(200);
    let window = |text: &str| {
        text.get(from..(at + 300).min(text.len()))
            .unwrap_or_default()
            .to_string()
    };
    format!("\n ours: {}\ntheirs: {}", window(a), window(b))
}

fn sorted(list: &Value) -> Vec<String> {
    let mut items: Vec<String> = list.as_array().unwrap().iter().map(Value::to_string).collect();
    items.sort();
    items
}

#[test]
fn delegation_agents_host_matches_native_rlm_host() {
    let our_ids = Regex::new(r"ultron-task-[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}").unwrap();
    let our_keys = Regex::new(r"\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b").unwrap();
    let their_ids = Regex::new(r"ultron-task-#\d+\b").unwrap();
    let their_keys = Regex::new(r"key-#\d+\b").unwrap();
    for scenario in fixture().as_array().unwrap() {
        let name = scenario["name"].as_str().unwrap();
        let run = host(scenario);
        let mut steps = Vec::new();
        let mut events = Vec::new();
        let mut last = "none".to_string();
        for step in scenario["steps"].as_array().unwrap() {
            let (kind, payload) = (step["op"][0].as_str().unwrap(), &step["op"][1]);
            let payload: Value =
                serde_json::from_str(&payload.to_string().replace("\"$last\"", &json!(last).to_string())).unwrap();
            let mut entry = Map::new();
            entry.insert("op".into(), step["op"].clone());
            match run.agents.handle(kind, payload, "main", None) {
                Ok(result) => {
                    if let Some(id) = result.get("id").and_then(Value::as_str) {
                        last = id.to_string();
                    }
                    entry.insert("result".into(), result);
                }
                Err(error) => {
                    entry.insert("error".into(), json!(error));
                }
            }
            steps.push(Value::Object(entry));
            quiesce(&run.agents);
            while let Ok(end) = run.events.try_recv() {
                events.push(json!({ "task": end.task, "kind": end.kind, "ownerLane": end.owner_lane,
                    "rootId": end.root_id, "awaited": end.awaited, "fetch": end.fetch, "cost": end.cost }));
            }
        }
        run.agents.close();
        let ours = json!({
            "steps": steps, "events": events, "journal": run.tasks.get(), "definitions": run.definitions.get(),
            "usage": run.usage.get(),
        });
        let theirs = json!({
            "steps": scenario["steps"], "events": scenario["events"], "journal": scenario["journal"],
            "definitions": scenario["definitions"], "usage": scenario["usage"],
        });
        let ours_text = canonical(&ours.to_string(), &run.tasks.get(), &our_ids, &our_keys);
        let theirs_text = canonical(&theirs.to_string(), &scenario["journal"], &their_ids, &their_keys);
        let ours: Value = serde_json::from_str(&ours_text).unwrap();
        let theirs: Value = serde_json::from_str(&theirs_text).unwrap();
        for section in ["steps", "events", "journal", "definitions", "usage"] {
            if section == "steps" {
                for (i, (a, b)) in ours[section]
                    .as_array()
                    .unwrap()
                    .iter()
                    .zip(theirs[section].as_array().unwrap())
                    .enumerate()
                {
                    let (a, b) = (a.to_string(), b.to_string());
                    assert!(a == b, "{name}: step {i}{}", first_difference(&a, &b));
                }
            }
            let (a, b) = (ours[section].to_string(), theirs[section].to_string());
            assert!(a == b, "{name}: {section}{}", first_difference(&a, &b));
        }
        let names = |text: String| canonical(&text, &run.tasks.get(), &our_ids, &our_keys);
        let ts_names = |text: String| canonical(&text, &scenario["journal"], &their_ids, &their_keys);
        let our_prompts: Value = serde_json::from_str(&names(json!(*run.prompts.lock().unwrap()).to_string())).unwrap();
        let their_prompts: Value = serde_json::from_str(&ts_names(scenario["prompts"].to_string())).unwrap();
        assert_eq!(sorted(&our_prompts), sorted(&their_prompts), "{name}: prompts");
        assert_eq!(
            sorted(&json!(*run.calls.lock().unwrap())),
            sorted(&scenario["adapterCalls"]),
            "{name}: adapter calls"
        );
    }
}
