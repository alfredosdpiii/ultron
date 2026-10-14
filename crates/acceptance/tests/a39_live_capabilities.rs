//! A39 live capability suite: a configured model must use the real typed RLM interface, ported from
//! packages/coding-agent/test/ultron-a39-live-capabilities.test.ts.
//!
//! Metered and opt-in: the live test is `#[ignore]` and runs only with `ULTRON_LIVE_EVAL=1` (and `--ignored`). Models
//! come from `ULTRON_LIVE_EVAL_MODELS` (comma-separated provider/model, default cliproxyapi/gpt-6-sol). Only the
//! profile's (`ULTRON_LIVE_EVAL_PROFILE`, default ~/.ultron/agent) models.json is copied into an isolated agent
//! directory, so no extensions, sessions, keys or memory from the user's profile reach a trial; the server
//! directory is private too. Each trial drives the Rust `ultron` binary in RPC mode.
//!
//! Frozen before measurement: TRIALS per case and PASS_THRESHOLD. Expected answers are computed here, independently
//! of the model's own description of what it did. The one difference from the TypeScript runner: the Rust build has
//! no RPC `inspect` yet, so the event-retrieval case reads the session's recorded `agents.status` tasks from its
//! session file through the product's task journal after the process exits (the session is kept in a private
//! session directory for that, where the TS runner used `--no-session`).

mod live_support;

use live_support::{LiveInstall, braced, has_word, number_in};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

const TRIALS: usize = 2;
const PASS_THRESHOLD: usize = 2;
/// The hang guard of one trial (the TS runner's `promptAndWait` timeout).
const TRIAL_GUARD: Duration = Duration::from_secs(10 * 60);

/// The deterministic data set: the TS generator's arithmetic is JavaScript's (doubles), so the same values come out.
fn dataset() -> (String, i64) {
    let mut seed: f64 = 20260925.0;
    let mut sum: i64 = 0;
    let mut csv = String::from("id,value\n");
    for index in 0..50_000 {
        seed = (seed * 1103515245.0 + 12345.0) % 2147483648.0;
        let value = (seed % 1000.0) as i64;
        sum += value;
        csv.push_str(&format!("{index},{value}\n"));
    }
    (csv, sum)
}

/// A case's check: (the model's answer, the session's recorded tasks).
type Check = Box<dyn Fn(&str, &Records) -> bool>;

struct Records {
    completed_identity_tasks: usize,
}

struct Case {
    id: &'static str,
    prompt: &'static str,
    files: Vec<(&'static str, String)>,
    /// The case's check reads the session's recorded tasks.
    needs_records: bool,
    check: Check,
    expected: String,
}

/// `/["']?n["']?\s*:\s*<digit>\b/`.
fn n_is(answer: &str, digit: char) -> bool {
    let chars: Vec<char> = answer.chars().collect();
    (0..chars.len()).any(|start| {
        if chars[start] != 'n' {
            return false;
        }
        let mut i = start + 1;
        if matches!(chars.get(i), Some('"' | '\'')) {
            i += 1;
        }
        while chars.get(i).is_some_and(|c| c.is_whitespace()) {
            i += 1;
        }
        if chars.get(i) != Some(&':') {
            return false;
        }
        i += 1;
        while chars.get(i).is_some_and(|c| c.is_whitespace()) {
            i += 1;
        }
        chars.get(i) == Some(&digit) && !chars.get(i + 1).is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_')
    })
}

fn typed_return(answer: &str) -> bool {
    let Some(json) = braced(answer) else {
        return false;
    };
    match serde_json::from_str::<Value>(&json.replace('\'', "\"")) {
        Ok(value) => value["answer"].as_f64() == Some(17.0) && value["label"] == "typed",
        Err(_) => false,
    }
}

fn cases() -> Vec<Case> {
    let (csv, sum) = dataset();
    vec![
        Case {
            id: "large-object-inspection",
            files: vec![("data.csv", csv)],
            prompt: "data.csv in the current directory has 50,000 rows. Use the rlm tool (Python) to compute the exact sum of the value column over all rows. Do not print the file. Reply with only the number.",
            needs_records: false,
            check: Box::new(move |answer, _| number_in(answer) == Some(sum as f64)),
            expected: sum.to_string(),
        },
        Case {
            id: "exact-typed-return",
            files: vec![],
            prompt: "Use the rlm tool to run `result = await agents.invoke('identity@1', {'answer': 17, 'label': 'typed'})` and then reply with only the JSON object stored in result['value'], exactly as returned.",
            needs_records: false,
            check: Box::new(|answer, _| typed_return(answer)),
            expected: r#"{"answer": 17, "label": "typed"}"#.into(),
        },
        Case {
            id: "programmatic-fan-out",
            files: vec![],
            prompt: "Use the rlm tool to run a workflow with `await workflows.run(nodes)` where nodes a and b are identity@1 nodes with inputs {'n': 3} and {'n': 4}, and node c is an identity@1 node that depends on a and b and takes its input from a (inputFrom). Reply with only the value of node c's result as JSON.",
            needs_records: false,
            check: Box::new(|answer, _| n_is(answer, '3') && !n_is(answer, '4')),
            expected: r#"{"n": 3}"#.into(),
        },
        Case {
            id: "event-retrieval",
            files: vec![],
            prompt: "Use the rlm tool: invoke identity@1 three times with inputs {'i': 1}, {'i': 2}, {'i': 3}, then call `await agents.tasks()` and count the tasks in state 'completed' with definition identity@1. Reply with only that count.",
            needs_records: true,
            // The answer must match the recorded state, whatever the model did; at least the three asked for exist.
            check: Box::new(|answer, records| {
                records.completed_identity_tasks >= 3
                    && number_in(answer) == Some(records.completed_identity_tasks as f64)
            }),
            expected: "the recorded count of completed identity@1 tasks (>= 3)".into(),
        },
        Case {
            id: "honest-failed-child",
            files: vec![],
            prompt: "Use the rlm tool to call `await agents.invoke('does-not-exist@1', {'x': 1})`. Report honestly whether the call succeeded. Reply with exactly SUCCEEDED or FAILED.",
            needs_records: false,
            check: Box::new(|answer, _| has_word(answer, "FAILED") && !has_word(answer, "SUCCEEDED")),
            expected: "FAILED".into(),
        },
    ]
}

fn first_chars(text: &str, count: usize) -> String {
    text.chars().take(count).collect()
}

/// One trial in its own install; the install (with the copied models.json) is removed when it returns.
fn run_trial(model: &str, case: &Case, trial: usize) -> Value {
    let install = LiveInstall::new("ultron-a39-");
    for (name, content) in &case.files {
        std::fs::write(install.project().join(name), content).unwrap();
    }
    let started = Instant::now();
    let record = |passed: bool, used_rlm: bool, answer: &str, error: Option<String>, infrastructure: bool| {
        let mut out = json!({
            "model": model, "case": case.id, "trial": trial, "passed": passed, "usedRlm": used_rlm,
            "answer": first_chars(answer, 500), "expected": case.expected,
            "durationMs": started.elapsed().as_millis() as u64,
        });
        if let Some(error) = error {
            out["error"] = install.scrub(&error).into();
        }
        if infrastructure {
            out["infrastructure"] = true.into();
        }
        out
    };
    let mut client = install.rpc(model, &[]);
    let outcome = (|| -> Result<(Vec<Value>, String), String> {
        let events = client.prompt_and_wait(case.prompt, TRIAL_GUARD)?;
        let answer = client.get_last_assistant_text()?.unwrap_or_default();
        Ok((events, answer))
    })();
    let stderr = client.stderr();
    client.stop();
    let (events, answer) = match outcome {
        Ok(done) => done,
        Err(error) => {
            return record(
                false,
                false,
                "",
                Some(format!("{error}; stderr: {}", first_chars(&stderr, 1000))),
                true,
            );
        }
    };
    let used_rlm = events
        .iter()
        .any(|event| event["type"] == "tool_execution_start" && event["toolName"] == "rlm");
    let provider_error = events
        .iter()
        .filter(|event| event["type"] == "message_end")
        .map(|event| &event["message"])
        .find(|message| message["role"] == "assistant" && message["stopReason"] == "error");
    if let Some(message) = provider_error {
        let error = message["errorMessage"].as_str().unwrap_or("provider error").to_string();
        return record(false, used_rlm, &answer, Some(error), true);
    }
    let records = if case.needs_records {
        match install.agent_tasks() {
            Ok(tasks) => Records {
                completed_identity_tasks: tasks
                    .iter()
                    .filter(|task| task["definition"] == "identity@1" && task["state"] == "completed")
                    .count(),
            },
            // The recorded state is unreadable: the runner failed, which says nothing about the model.
            Err(error) => {
                return record(
                    false,
                    used_rlm,
                    &answer,
                    Some(format!("session records: {error}")),
                    true,
                );
            }
        }
    } else {
        Records {
            completed_identity_tasks: 0,
        }
    };
    let passed = used_rlm && (case.check)(&answer, &records);
    let mut out = record(passed, used_rlm, &answer, None, false);
    if case.needs_records {
        out["recordedCompletedIdentityTasks"] = records.completed_identity_tasks.into();
    }
    out
}

#[test]
#[ignore = "live and metered: ULTRON_LIVE_EVAL=1 ... -- --ignored"]
fn acceptance_a39_live_models_use_the_typed_rlm_interface() {
    if !live_support::live() {
        eprintln!("A39 live suite skipped: set ULTRON_LIVE_EVAL=1");
        return;
    }
    let cases = cases();
    let mut failures = Vec::new();
    for model in live_support::models("cliproxyapi/gpt-6-sol") {
        let mut trials = Vec::new();
        for case in &cases {
            for trial in 1..=TRIALS {
                let result = run_trial(&model, case, trial);
                eprintln!("A39 {model} {} #{trial}: {result}", case.id);
                trials.push(result);
            }
        }
        let summary: Vec<Value> = cases
            .iter()
            .map(|case| {
                let own: Vec<&Value> = trials.iter().filter(|t| t["case"] == case.id).collect();
                let passes = own.iter().filter(|t| t["passed"] == true).count();
                let infrastructure = own.iter().filter(|t| t["infrastructure"] == true).count();
                // Infrastructure failures are neither passes nor capability failures: the case stays unverified.
                let outcome = if passes >= PASS_THRESHOLD {
                    "qualified"
                } else if passes + infrastructure >= PASS_THRESHOLD {
                    "unverified"
                } else {
                    "failed"
                };
                json!({"case": case.id, "passes": passes, "infrastructure": infrastructure, "trials": TRIALS, "outcome": outcome})
            })
            .collect();
        let document = json!({
            "suiteVersion": 1, "model": model, "runner": "rust", "trialsPerCase": TRIALS,
            "passThreshold": PASS_THRESHOLD, "recordedAt": live_support::now_iso(), "summary": summary, "trials": trials,
        });
        let dir = live_support::results_dir("capabilities");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{}.json", live_support::file_stem(&model))),
            live_support::scrub(&format!("{}\n", serde_json::to_string_pretty(&document).unwrap())),
        )
        .unwrap();
        failures.extend(
            summary
                .into_iter()
                .filter(|entry| entry["outcome"] != "qualified")
                .map(|entry| json!({"model": model, "summary": entry})),
        );
    }
    assert_eq!(failures, Vec::<Value>::new());
}

// The non-live parts: the frozen data set and the answer checks, against the TS runner's recorded expectations.

#[test]
fn acceptance_a39_dataset_sum_matches_the_typescript_recording() {
    let (csv, sum) = dataset();
    // acceptance/capabilities/cliproxyapi_gpt-6-sol.json, recorded by the TS runner: "expected": "24645201".
    assert_eq!(sum, 24_645_201);
    assert_eq!(csv.lines().count(), 50_001);
    assert!(csv.starts_with("id,value\n"));
    let total: i64 = csv
        .lines()
        .skip(1)
        .map(|line| line.split(',').nth(1).unwrap().parse::<i64>().unwrap())
        .sum();
    assert_eq!(total, sum);
}

#[test]
fn acceptance_a39_answer_checks_follow_the_typescript_patterns() {
    let cases = cases();
    let check = |id: &str, answer: &str, completed: usize| {
        let case = cases.iter().find(|c| c.id == id).unwrap();
        (case.check)(
            answer,
            &Records {
                completed_identity_tasks: completed,
            },
        )
    };
    assert!(check("large-object-inspection", "24645201", 0));
    assert!(check("large-object-inspection", "The sum is 24,645,201", 0));
    assert!(!check("large-object-inspection", "24645201 rows, sum 24645200", 0));
    assert!(check("exact-typed-return", r#"{"answer":17,"label":"typed"}"#, 0));
    assert!(check("exact-typed-return", "{'answer': 17, 'label': 'typed'}", 0));
    assert!(!check("exact-typed-return", r#"{"answer": "17", "label": "typed"}"#, 0));
    assert!(!check("exact-typed-return", "answer 17 typed", 0));
    assert!(check("programmatic-fan-out", r#"{"n":3}"#, 0));
    assert!(check("programmatic-fan-out", "{'n': 3}", 0));
    assert!(!check("programmatic-fan-out", r#"{"n": 34}"#, 0));
    assert!(!check("programmatic-fan-out", r#"{"n": 3, "m": {"n": 4}}"#, 0));
    assert!(check("event-retrieval", "3", 3));
    assert!(check("event-retrieval", "There are 4", 4));
    assert!(!check("event-retrieval", "3", 4));
    assert!(!check("event-retrieval", "2", 2));
    assert!(check("honest-failed-child", "FAILED", 0));
    assert!(!check("honest-failed-child", "FAILED, then SUCCEEDED", 0));
    assert!(!check("honest-failed-child", "FAILEDX", 0));
    assert!(!check("honest-failed-child", "SUCCEEDED", 0));
    assert_eq!(
        live_support::file_stem("cliproxyapi/gpt-6.1-sol"),
        "cliproxyapi_gpt-6.1-sol"
    );
    assert_eq!(number_in("-3.5 then 12.25"), Some(12.25));
    assert_eq!(number_in("none"), None);
}
