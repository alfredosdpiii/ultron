//! A46 live demonstration: a real model runs fix, review and improve through the typed RLM API, ported from
//! packages/coding-agent/test/ultron-a46-live-demo.test.ts.
//!
//! Metered and opt-in: `#[ignore]`, and runs only with `ULTRON_LIVE_EVAL=1`. The model is `ULTRON_LIVE_EVAL_MODEL`
//! (the TS runner's variable), else the first of `ULTRON_LIVE_EVAL_MODELS`, default cliproxyapi/gpt-6-sol. Only the
//! profile's models.json is copied into a private agent directory; the server directory is private too.
//!
//! The model chooses its own decomposition; this test checks the recorded outcome against facts computed here. The
//! task is the TS task word for word, retained instances included (step 4), and, as in TS, a second client process
//! reattaches to the same session and reads every record back through RPC `inspect` (`agents.status`,
//! `gates.history`, `gates.list`, `instances.list`, `experiments.list`, `refinements.list`).

mod live_support;

use cli::rpc::client::RpcClient;
use live_support::{LiveInstall, braced};
use serde_json::{Map, Value, json};
use std::time::{Duration, Instant};

const SESSION_ID: &str = "a46-live-demo";
const BUGGY: &str = "def average(values):\n    return sum(values) / (len(values) - 1)\n";
/// The hang guard of the task (the TS runner's `promptAndWait` timeout).
const TASK_GUARD: Duration = Duration::from_secs(40 * 60);

/// The diagnostics log and its most-failing location, counted here from the rows themselves.
fn diagnostics() -> (String, String) {
    let locations = ["calc.py:average", "calc.py:total", "io.py:read", "calc.py:average"];
    let mut csv = String::from("id,status,location\n");
    let mut failures: Vec<(&str, usize)> = Vec::new();
    for index in 0..200_000usize {
        let failed = index % 7 == 0;
        let location = locations[index % locations.len()];
        csv.push_str(&format!(
            "row-{index},{},{location}\n",
            if failed { "fail" } else { "ok" }
        ));
        if failed {
            match failures.iter_mut().find(|(known, _)| *known == location) {
                Some((_, count)) => *count += 1,
                None => failures.push((location, 1)),
            }
        }
    }
    let top = failures.iter().max_by_key(|(_, count)| *count).unwrap().0.to_string();
    (csv, top)
}

/// The TS live demo's task, word for word.
const TASK: &str = r#"This project is disposable. repo/test_calc.py fails (run it with `python3 test_calc.py` inside repo/). diagnostics.csv has 200,000 log rows: never read it into your own context; analyze it only inside the rlm tool.

Do all of the following through the rlm tool's Python API:
1. Over the full diagnostics.csv, find the location with the most failing rows.
2. Fix the bug in an isolated copy of repo/ (e.g. under tempfile.mkdtemp()); do not modify repo/ itself. Run the tests on the original and on the copy with `await bash(...)`, then `await gates.define('live-unit', [{'name': 'unit', 'required': True}], <sha256 of repo/test_calc.py>)` and `await gates.compare(...)` with the baseline and candidate results.
3. Start an independent review of your diff with `review = await agents.spawn('correctness-reviewer@1', {'request': <diff>})` and wait for `await review.result()`.
4. Retain that reviewer with `await instances.retain(review.id)` and ask it one follow-up with `await instances.invoke(<instance id>, {'request': 'Does the fix handle an empty list?'})`, then wait for that task's result.
5. Store the diff with `await rlm.host_request('artifacts.put', {'text': <diff>})`.
6. Propose and activate an instruction refinement for target 'instruction:correctness-reviewer' with evidence from this run, run one more review of the same diff, then roll the refinement back. Record each of the two reviews with `await rlm.host_request('experiments.record', {'run': {'variant': ..., 'fixtureHash': 'live-v1', 'outcome': 'passed' or 'failed'}})`.

Finish by replying with only a JSON object: {"top_location": ..., "gate_decision": ..., "review_task_id": ..., "instance_id": ...}."#;

fn model() -> String {
    std::env::var("ULTRON_LIVE_EVAL_MODEL")
        .ok()
        .unwrap_or_else(|| live_support::models("cliproxyapi/gpt-6-sol").remove(0))
}

/// `inspect(rpc, request, payload)`: the read-only inspector over RPC; an unsuccessful response is an error.
fn inspect(client: &mut RpcClient, request: &str, payload: Value) -> Result<Value, String> {
    let response = client.send(json!({"type": "inspect", "request": request, "payload": payload}))?;
    RpcClient::data(&response).map_err(|error| format!("inspect {request}: {error}"))
}

/// The TS runner's checks, made on the records the reattached client reads back through RPC `inspect`; fills
/// `record`.
fn verify(
    second: &mut RpcClient,
    install: &LiveInstall,
    record: &mut Map<String, Value>,
    answer: &Value,
    messages: &[Value],
    top: &str,
    csv_len: usize,
) -> Result<(), String> {
    let status = inspect(second, "agents.status", json!({}))?;
    let history = inspect(second, "gates.history", json!({"gate_id": "live-unit"}))?;
    let instances = inspect(second, "instances.list", json!({}))?;
    let experiments = inspect(second, "experiments.list", json!({}))?;
    let refinements = inspect(second, "refinements.list", json!({}))?;
    let tasks = status["tasks"].as_array().cloned().unwrap_or_default();
    record.insert(
        "tasks".into(),
        tasks
            .iter()
            .map(|task| {
                let error = task["result"]["error"]
                    .as_str()
                    .map(|e| format!(" ({})", e.chars().take(300).collect::<String>()));
                Value::from(format!(
                    "{}:{}{}",
                    task["definition"].as_str().unwrap_or_default(),
                    task["state"].as_str().unwrap_or_default(),
                    error.unwrap_or_default()
                ))
            })
            .collect(),
    );
    record.insert("gates".into(), inspect(second, "gates.list", json!({}))?);
    record.insert(
        "gate".into(),
        history["attempts"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|attempt| attempt["decision"].clone())
            .collect(),
    );
    let instances = instances.as_array().cloned().unwrap_or_default();
    record.insert("instances".into(), instances.len().into());
    let experiment_count = experiments.as_array().map_or(0, Vec::len);
    record.insert("experiments".into(), experiment_count.into());
    record.insert(
        "refinements".into(),
        refinements
            .as_array()
            .into_iter()
            .flatten()
            .map(|item| {
                Value::from(format!(
                    "{}:{}",
                    item["target"].as_str().unwrap_or_default(),
                    item["state"].as_str().unwrap_or_default()
                ))
            })
            .collect(),
    );

    let mut failed = Vec::new();
    let mut expect = |ok: bool, what: String| {
        if !ok {
            failed.push(what);
        }
    };
    expect(
        answer["top_location"] == top,
        format!("top_location {} is not {top}", answer["top_location"]),
    );
    expect(
        history["attempts"]
            .as_array()
            .and_then(|a| a.last())
            .is_some_and(|a| a["decision"] == "passed"),
        format!("the last live-unit gate attempt did not pass: {}", history["attempts"]),
    );
    expect(
        tasks.iter().any(|task| {
            answer["review_task_id"].is_string()
                && task["id"] == answer["review_task_id"]
                && task["definition"] == "correctness-reviewer@1"
                && task["state"] == "completed"
        }),
        format!("no completed correctness-reviewer@1 task {}", answer["review_task_id"]),
    );
    // The retained reviewer: an instance of the review task with exactly one follow-up invocation.
    expect(
        instances.iter().any(|instance| {
            answer["review_task_id"].is_string()
                && instance["task_id"] == answer["review_task_id"]
                && instance["invocations"]
                    .as_array()
                    .is_some_and(|calls| calls.len() == 1 && calls[0].is_object())
        }),
        format!(
            "no retained instance of {} with one invocation: {}",
            answer["review_task_id"],
            Value::Array(instances.clone())
        ),
    );
    expect(
        experiment_count >= 2,
        format!("{experiment_count} experiments recorded"),
    );
    expect(
        refinements
            .as_array()
            .into_iter()
            .flatten()
            .any(|item| item["target"] == "instruction:correctness-reviewer" && item["state"] == "rolled_back"),
        "no rolled-back instruction:correctness-reviewer refinement".into(),
    );
    // The fix happened in an isolated copy; the original repository is untouched.
    expect(
        std::fs::read_to_string(install.project().join("repo/calc.py")).unwrap_or_default() == BUGGY,
        "repo/calc.py was modified".into(),
    );
    // The dataset never entered the conversation.
    let transcript = serde_json::to_string(messages).unwrap();
    expect(
        !transcript.contains("row-199999"),
        "the transcript holds row-199999".into(),
    );
    expect(
        transcript.len() < csv_len / 10,
        format!("the transcript is {} chars, the data {csv_len}", transcript.len()),
    );
    if failed.is_empty() {
        Ok(())
    } else {
        Err(failed.join("; "))
    }
}

#[test]
#[ignore = "live and metered: ULTRON_LIVE_EVAL=1 ... -- --ignored"]
fn acceptance_a46_live_model_fixes_reviews_and_improves_through_the_typed_rlm_api() {
    if !live_support::live() {
        eprintln!("A46 live demo skipped: set ULTRON_LIVE_EVAL=1");
        return;
    }
    let model = model();
    let install = LiveInstall::new("ultron-a46-live-");
    let (csv, top) = diagnostics();
    let repo = install.project().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(install.project().join("diagnostics.csv"), &csv).unwrap();
    std::fs::write(repo.join("calc.py"), BUGGY).unwrap();
    std::fs::write(
        repo.join("test_calc.py"),
        "from calc import average\nassert average([2, 4, 6]) == 4, average([2, 4, 6])\nprint('ok')\n",
    )
    .unwrap();
    let mut record = Map::new();
    record.insert("model".into(), model.clone().into());
    record.insert("runner".into(), "rust".into());
    record.insert("recordedAt".into(), live_support::now_iso().into());
    let outcome = (|| -> Result<(), String> {
        let mut first = install.rpc(&model, &["--session-id", SESSION_ID]);
        let started = Instant::now();
        let prompted = first.prompt_and_wait(TASK, TASK_GUARD);
        record.insert("durationMs".into(), (started.elapsed().as_millis() as u64).into());
        if let Err(error) = prompted {
            let stderr = first.stderr();
            record.insert("cliExit".into(), json!({"code": first.stop()}));
            return Err(format!(
                "{error}; stderr: {}",
                stderr.chars().take(2000).collect::<String>()
            ));
        }
        let answer_text = first.get_last_assistant_text()?.unwrap_or_default();
        record.insert(
            "answer".into(),
            answer_text.chars().take(2000).collect::<String>().into(),
        );
        let answer: Value = braced(&answer_text)
            .and_then(|json| serde_json::from_str(json).ok())
            .unwrap_or_else(|| json!({}));
        let messages = first.get_messages()?;
        record.insert(
            "transcriptChars".into(),
            serde_json::to_string(&messages).unwrap().len().into(),
        );
        record.insert(
            "toolOutputs".into(),
            messages
                .iter()
                .filter(|m| m["role"] == "toolResult")
                .map(|m| Value::from(m["content"].to_string().chars().take(1500).collect::<String>()))
                .collect(),
        );
        record.insert("cliExit".into(), json!({"code": first.stop()}));

        // Reattach from a new client process and reconstruct everything from records.
        let mut second = install.rpc(&model, &["--session-id", SESSION_ID]);
        let checked = (|| {
            let reattached = second.get_messages()?;
            record.insert("reattachedMessages".into(), reattached.len().into());
            if reattached.len() != messages.len() {
                return Err(format!(
                    "the reattached session has {} messages, the run had {}",
                    reattached.len(),
                    messages.len()
                ));
            }
            verify(&mut second, &install, &mut record, &answer, &messages, &top, csv.len())
        })();
        second.stop();
        checked
    })();
    record.insert("passed".into(), outcome.is_ok().into());
    if let Err(error) = &outcome {
        record.insert("error".into(), error.clone().into());
    }
    let dir = live_support::results_dir("demonstrations");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("a46-live-{}.json", live_support::file_stem(&model))),
        live_support::scrub(&format!(
            "{}\n",
            serde_json::to_string_pretty(&Value::Object(record)).unwrap()
        )),
    )
    .unwrap();
    drop(install);
    if let Err(error) = outcome {
        panic!("A46 live demo failed: {error}");
    }
}

#[test]
fn acceptance_a46_live_diagnostics_top_location_is_counted_from_the_rows() {
    let (csv, top) = diagnostics();
    // The TS runner's fixed answer for the same rows.
    assert_eq!(top, "calc.py:average");
    assert_eq!(csv.lines().count(), 200_001);
    assert!(csv.contains("\nrow-199999,ok,calc.py:average\n"));
    assert!(csv.starts_with("id,status,location\nrow-0,fail,calc.py:average\n"));
    // The task is the TS one: retained instances included.
    assert!(TASK.contains("4. Retain that reviewer with `await instances.retain(review.id)`"));
    assert!(TASK.ends_with(r#""review_task_id": ..., "instance_id": ...}."#));
}
