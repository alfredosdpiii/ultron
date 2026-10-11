//! Replays fixtures/context.json (fixtures/generate-context.mjs): `ctx.*` cells through runtime.py and crates/kernel
//! against `ContextControl`, over the same scripted lane branches as the TS recording (seeded from
//! context-helpers.json; `test.*` requests switch the calling lane, append transcript entries, and hold appended
//! entries back as a running turn does). Also replays the pure helpers table (taskLine, collapsedCellText,
//! trimmedResultText, applyContextEdits).

mod common;

use common::{Made, assert_area};
use kernel::{HostHandler, Signal};
use kernel_apis::context_control::{
    ContextBranches, ContextControl, TaskLine, apply_context_edits, collapsed_cell_text, task_line, trimmed_result_text,
};
use kernel_apis::util::lock;
use kernel_apis::{ApiModule, KernelApis, Request};
use serde_json::{Map, Value, json};
use std::path::Path;
use std::sync::{Arc, Mutex};

fn helpers() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/context-helpers.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

struct Branches {
    lanes: Map<String, Value>,
    seq: i64,
    appended: i64,
    deferred: bool,
    held: Vec<(String, Value)>,
    lane: String,
}

/// generate-context.mjs's scripted lanes (findEntries / appendCustomEntry / getTipId).
#[derive(Clone)]
struct Fake(Arc<Mutex<Branches>>);

impl Fake {
    fn seeded() -> Self {
        let seed = helpers()["seed"].clone();
        Self(Arc::new(Mutex::new(Branches {
            lanes: seed["branches"].as_object().unwrap().clone(),
            seq: seed["seq"].as_i64().unwrap(),
            appended: 0,
            deferred: false,
            held: Vec::new(),
            lane: "main".into(),
        })))
    }

    fn lane_mut<'a>(branches: &'a mut Branches, lane: &str) -> &'a mut Vec<Value> {
        branches
            .lanes
            .entry(lane)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .unwrap()
    }
}

impl ContextBranches for Fake {
    fn entries(&self, lane: &str) -> Result<Vec<Value>, String> {
        let mut branches = lock(&self.0);
        Ok(Self::lane_mut(&mut branches, lane).clone())
    }

    fn append_custom(&self, lane: &str, custom_type: &str, data: &Value) -> Result<String, String> {
        let mut branches = lock(&self.0);
        branches.seq += 1;
        branches.appended += 1;
        let id = format!("{lane}-x{}", branches.appended);
        let entry = json!({"id": id, "parentId": null, "seq": branches.seq, "timestamp": branches.seq,
            "type": "custom", "customType": custom_type, "data": data});
        if branches.deferred {
            branches.held.push((lane.to_string(), entry));
        } else {
            Self::lane_mut(&mut branches, lane).push(entry);
        }
        Ok(id)
    }

    fn tip_id(&self, lane: &str) -> Result<Option<String>, String> {
        let mut branches = lock(&self.0);
        Ok(Self::lane_mut(&mut branches, lane)
            .last()
            .and_then(|entry| entry["id"].as_str().map(str::to_string)))
    }
}

/// The fixture's wrapper: the module sees the lane `test.lane` selected.
struct OnLane {
    control: ContextControl,
    fake: Fake,
}

impl ApiModule for OnLane {
    fn prefixes(&self) -> &[&str] {
        &["ctx."]
    }

    fn handle(&self, request: &Request<'_>) -> Result<Value, String> {
        let lane = lock(&self.fake.0).lane.clone();
        self.control.handle_request(request.kind, request.payload, &lane)
    }
}

fn test_requests(fake: Fake) -> Arc<dyn HostHandler> {
    Arc::new(move |kind: &str, payload: Map<String, Value>, _: &Signal| {
        let mut branches = lock(&fake.0);
        match kind {
            "test.lane" => branches.lane = payload["lane"].as_str().unwrap().into(),
            "test.defer" => branches.deferred = payload["on"].as_bool().unwrap(),
            "test.flush" => {
                for (lane, entry) in std::mem::take(&mut branches.held) {
                    Fake::lane_mut(&mut branches, &lane).push(entry);
                }
            }
            "test.append" => {
                let spec: Value = serde_json::from_str(payload["json"].as_str().unwrap()).unwrap();
                branches.seq += 1;
                let mut entry =
                    json!({"id": spec["id"], "parentId": null, "seq": branches.seq, "timestamp": branches.seq});
                for (key, value) in spec["entry"].as_object().unwrap() {
                    entry[key] = value.clone();
                }
                Fake::lane_mut(&mut branches, spec["lane"].as_str().unwrap()).push(entry);
            }
            _ => return Err(format!("Ultron RLM host request is not wired: {kind}")),
        }
        Ok(json!({"lane": branches.lane}))
    })
}

fn context_host(_: &Path) -> Made {
    let fake = Fake::seeded();
    let module = OnLane {
        control: ContextControl::new(Arc::new(fake.clone())),
        fake: fake.clone(),
    };
    let state = fake.clone();
    Made {
        handler: KernelApis::new(test_requests(fake)).with(Arc::new(module)),
        env: Vec::new(),
        state: Box::new(move || Value::Object(lock(&state.0).lanes.clone())),
    }
}

#[test]
fn kernel_apis_context_cells_match_typescript_host() {
    assert_area("context", &context_host);
}

fn task(value: &Value) -> TaskLine {
    TaskLine {
        id: value["id"].as_str().unwrap().into(),
        definition: value["definition"].as_str().unwrap().into(),
        key: value["key"].as_str().unwrap().into(),
        status: value["status"].as_str().unwrap().into(),
        cost: value["cost"].as_f64(),
    }
}

#[test]
fn kernel_apis_context_helpers_match_typescript() {
    let helpers = helpers();
    for case in helpers["taskLines"].as_array().unwrap() {
        assert_eq!(task_line(&task(&case[0])), case[1].as_str().unwrap());
    }
    for case in helpers["collapsed"].as_array().unwrap() {
        let tasks: Vec<TaskLine> = case[2].as_array().unwrap().iter().map(task).collect();
        assert_eq!(
            collapsed_cell_text(case[0].as_str().unwrap(), case[1].as_str().unwrap(), &tasks),
            case[3].as_str().unwrap()
        );
    }
    for case in helpers["trimmed"].as_array().unwrap() {
        assert_eq!(
            trimmed_result_text(case[0].as_str().unwrap(), case[1].as_str().unwrap()),
            case[2].as_str().unwrap()
        );
    }
    for (index, case) in helpers["apply"].as_array().unwrap().iter().enumerate() {
        let applied: Vec<Value> = apply_context_edits(case[0].as_array().unwrap())
            .into_iter()
            .map(|applied| applied.entry)
            .collect();
        assert_eq!(Value::Array(applied), case[1], "apply case {index}");
    }
}

/// Ported from test/ultron-context-control.test.ts: an omitted assistant message omits the results of its calls,
/// and a later edit of the same target wins.
#[test]
fn kernel_apis_context_apply_edits_pairs_calls_and_results() {
    let message = |id: &str, seq: i64, message: Value| json!({"id": id, "parentId": null, "seq": seq, "timestamp": seq, "type": "message", "message": message});
    let entries = vec![
        message("u1", 1, json!({"role": "user", "content": "first", "timestamp": 0})),
        message(
            "a1",
            2,
            json!({"role": "assistant", "content": [{"type": "text", "text": "calling"},
                {"type": "toolCall", "id": "c1", "name": "rlm", "arguments": {}}]}),
        ),
        message(
            "r1",
            3,
            json!({"role": "toolResult", "toolCallId": "c1", "toolName": "rlm", "content": [{"type": "text", "text": "BIG"}]}),
        ),
        json!({"id": "e1", "parentId": null, "seq": 4, "timestamp": 4, "type": "custom", "customType": "context_edit",
            "data": {"edits": [{"targetId": "u1", "replacement": null}, {"targetId": "a1", "replacement": null}]}}),
        json!({"id": "e2", "parentId": null, "seq": 5, "timestamp": 5, "type": "custom", "customType": "context_edit",
            "data": {"targetId": "u1", "replacement": {"content": "kept after all"}}}),
    ];
    let applied = apply_context_edits(&entries);
    let visible: Vec<&str> = applied
        .iter()
        .filter(|applied| applied.entry["customType"] != "context_edit:omitted")
        .map(|applied| applied.entry["id"].as_str().unwrap())
        .collect();
    assert_eq!(visible, ["u1", "e1", "e2"]);
    assert_eq!(applied[0].entry["message"]["content"], "kept after all");
    assert!(applied[0].changed && applied[2].changed && !applied[3].changed);
}

/// generate-context.mjs `recordHooks`' harness: appended custom entries get `<lane>-y<n>` ids, and every entry the
/// harness adds is announced as `entry_added`.
/// (lanes, seq, appended count, entries added since the last drain).
type HookState = (Map<String, Value>, i64, i64, Vec<(String, Value)>);

#[derive(Clone, Default)]
struct HookBranches(Arc<Mutex<HookState>>);

impl HookBranches {
    fn push(&self, lane: &str, entry: Value) {
        let mut state = lock(&self.0);
        state
            .0
            .entry(lane)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .unwrap()
            .push(entry.clone());
        state.3.push((lane.to_string(), entry));
    }
}

impl ContextBranches for HookBranches {
    fn entries(&self, lane: &str) -> Result<Vec<Value>, String> {
        Ok(lock(&self.0)
            .0
            .get(lane)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    fn append_custom(&self, lane: &str, custom_type: &str, data: &Value) -> Result<String, String> {
        let (id, seq) = {
            let mut state = lock(&self.0);
            state.1 += 1;
            state.2 += 1;
            (format!("{lane}-y{}", state.2), state.1)
        };
        self.push(
            lane,
            json!({"id": id, "parentId": null, "seq": seq, "timestamp": seq, "type": "custom", "customType": custom_type, "data": data}),
        );
        Ok(id)
    }

    fn tip_id(&self, lane: &str) -> Result<Option<String>, String> {
        Ok(self
            .entries(lane)?
            .last()
            .and_then(|entry| entry["id"].as_str().map(str::to_string)))
    }
}

#[test]
fn kernel_apis_context_hooks_collapse_trim_and_keep_like_typescript() {
    let recorded = helpers()["hooks"].clone();
    let branches = HookBranches::default();
    lock(&branches.0).0.insert("main".into(), json!([]));
    let control = ContextControl::new(Arc::new(branches.clone())).with_trim(true);
    let mut observed = Vec::new();
    let mut outputs = Vec::new();
    let drain = |control: &ContextControl, observed: &mut Vec<Value>| {
        let added = std::mem::take(&mut lock(&branches.0).3);
        for (lane, entry) in added {
            observed.extend(control.entry_added(&lane, &entry));
        }
    };
    for step in recorded["steps"].as_array().unwrap() {
        let mut output = Value::Null;
        let lane = step["lane"].as_str().unwrap_or("main");
        match step["op"].as_str().unwrap() {
            "append" => {
                let seq = {
                    let mut state = lock(&branches.0);
                    state.1 += 1;
                    state.1
                };
                let mut entry = json!({"id": step["entry"]["id"], "parentId": null, "seq": seq, "timestamp": seq});
                for (key, value) in step["entry"].as_object().unwrap() {
                    entry[key] = value.clone();
                }
                branches.push(lane, entry);
            }
            "event" => {
                let event = &step["event"];
                let lane = event["lane"].as_str().unwrap();
                let call = event["toolCallId"].as_str().unwrap_or_default();
                match step["name"].as_str().unwrap() {
                    "tool_start" => control.tool_start(lane, event["toolName"].as_str().unwrap(), call),
                    "tool_end" => control.tool_end(lane, call),
                    "message_end" => control
                        .message_end(lane, event["message"]["role"].as_str().unwrap())
                        .unwrap(),
                    "run_end" => control.run_end(lane).unwrap(),
                    other => panic!("event {other}"),
                }
            }
            "taskEnded" => control.task_ended(&step["task"], step["cost"].as_f64()),
            "settle" => {}
            "snapshot" => output = Value::Array(branches.entries(lane).unwrap()),
            "request" => {
                let payload = step["payload"].as_object().unwrap();
                output = match control.handle_request(step["type"].as_str().unwrap(), payload, lane) {
                    Ok(value) => json!({"ok": value}),
                    Err(error) => json!({"error": error}),
                };
            }
            "hook" => {
                let event = &step["event"];
                let lane = event["lane"].as_str().unwrap();
                output = match step["name"].as_str().unwrap() {
                    "before_compaction" => {
                        let preparation = &event["preparation"];
                        let decline = control
                            .before_compaction(
                                lane,
                                event["reason"].as_str().unwrap(),
                                preparation["tokensBefore"].as_i64().unwrap(),
                                Some(40000),
                                preparation["settings"]["reserveTokens"].as_i64().unwrap(),
                            )
                            .unwrap();
                        if decline { json!({"decline": true}) } else { Value::Null }
                    }
                    "transform_context" => {
                        match control
                            .transform_context(lane, event["messages"].as_array().unwrap())
                            .unwrap()
                        {
                            None => Value::Null,
                            Some(mut messages) => {
                                for message in &mut messages {
                                    if message["customType"] == "ultron-context-kept" {
                                        message["timestamp"] = 0.into();
                                    }
                                }
                                json!({"messages": messages})
                            }
                        }
                    }
                    other => panic!("hook {other}"),
                };
            }
            other => panic!("op {other}"),
        }
        drain(&control, &mut observed);
        let index = outputs.len();
        assert_eq!(output, recorded["outputs"][index], "step {index}: {step}");
        outputs.push(output);
    }
    assert_eq!(Value::Array(observed), recorded["observed"]);
}
