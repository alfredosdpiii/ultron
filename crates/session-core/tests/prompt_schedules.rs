//! Scheduled prompts through the session actor (a Rust-only feature; literal expectations): a cell creates a
//! schedule with `schedule.every` (schedule_api.py over kernel_apis::prompt_schedule), the server's fire
//! (`Op::ScheduleFire`, as crates/daemon sends it) starts a run whose prompt is the `ultron.schedule` custom message,
//! duplicate and early fires do nothing, missed slots collapse into one run, a fire during a run waits for that run
//! to end, and a cancel stops it. Time is a test clock file (`__ULTRON_SCHEDULE_CLOCK_FILE`), never a real wait.

mod p_session_support;

use kernel_apis::prompt_schedule::{CLOCK_FILE_ENV, CUSTOM_TYPE, NAMESPACE};
use p_session_support::{HANG, Setup, actor, in_order, text, tool, tool_results, user_texts};
use serde_json::{Value, json};
use session_core::core::Op;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const T0: i64 = 1_800_000_000_000;
const HOUR: i64 = 3_600_000;

fn clock(dir: &Path) -> PathBuf {
    let path = dir.join("clock");
    std::fs::write(&path, T0.to_string()).unwrap();
    path
}

fn set_clock(path: &Path, now: i64) {
    std::fs::write(path, now.to_string()).unwrap();
}

fn setup(clock: &Path) -> Setup {
    Setup {
        vars: vec![(CLOCK_FILE_ENV, clock.display().to_string())],
        ..Setup::default()
    }
}

fn wait_runs(a: &p_session_support::Actor, n: usize) {
    let end = Instant::now() + HANG;
    while a.events("run_end").len() < n {
        assert!(
            Instant::now() < end,
            "timed out waiting for {n} runs: {:?}",
            a.events("run_end")
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn list(a: &p_session_support::Actor) -> Vec<Value> {
    a.call(Op::Inspect {
        request: "schedule.list".into(),
        payload: json!({}),
    })
    .unwrap()
    .unwrap()
    .as_array()
    .unwrap()
    .clone()
}

fn fire(a: &p_session_support::Actor, id: &str, due: i64) -> Value {
    a.call(Op::ScheduleFire { id: id.into(), due }).unwrap().unwrap()
}

fn schedule_messages(a: &p_session_support::Actor) -> Vec<Value> {
    a.events("message_end")
        .into_iter()
        .map(|e| e["message"].clone())
        .filter(|m| m["customType"] == CUSTOM_TYPE)
        .collect()
}

const CREATE: &str = "sid = await schedule.every('1h', 'check the build', name='build')\nprint(sid)";

#[test]
fn session_core_schedule_from_a_cell_fires_as_a_custom_message_run() {
    let dir = tempfile::tempdir().unwrap();
    let clock = clock(dir.path());
    let script = in_order(vec![
        tool("toolu_1", CREATE),
        text("Scheduled."),
        text("Build is green."),
        text("Still green."),
    ]);
    let a = actor(script, setup(&clock));
    let ended = a.prompt("watch the build");
    assert_eq!(ended["error"], Value::Null, "{ended}");
    let listed = list(&a);
    assert_eq!(listed.len(), 1, "{listed:?}");
    let id = listed[0]["id"].as_str().unwrap().to_string();
    assert_eq!(
        (
            listed[0]["name"].clone(),
            listed[0]["nextDue"].clone(),
            listed[0]["runs"].clone()
        ),
        (json!("build"), json!(T0 + HOUR), json!(0))
    );
    // The cell printed the id it got back.
    let results = tool_results(&a.requests()[1]);
    assert!(results[0].to_string().contains(&id), "{results:?}");
    // Early: nothing runs.
    assert_eq!(
        fire(&a, &id, T0 + HOUR),
        json!({"fired": false, "run": null, "schedules": [{"id": id, "name": "build", "nextDue": T0 + HOUR}]})
    );
    set_clock(&clock, T0 + HOUR + 5_000);
    assert_eq!(
        fire(&a, &id, T0 + HOUR),
        json!({"fired": true, "run": 1, "schedules": [{"id": id, "name": "build", "nextDue": T0 + 2 * HOUR}]})
    );
    wait_runs(&a, 2);
    // The model reads the prompt as the run's user message.
    let requests = a.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(user_texts(&requests[2]).last().unwrap(), &json!("check the build"));
    // Clients see the custom message, never a user bubble with its text.
    let shown = schedule_messages(&a);
    assert_eq!(shown.len(), 1, "{shown:?}");
    assert_eq!(shown[0]["details"], json!({"name": "build", "id": id, "run": 1}));
    assert_eq!(shown[0]["content"], "check the build");
    let echoed: Vec<Value> = a
        .events("message_end")
        .into_iter()
        .filter(|e| e["message"]["role"] == "user" && e.to_string().contains("check the build"))
        .collect();
    assert!(echoed.is_empty(), "{echoed:?}");
    // A duplicate fire of the same slot does nothing.
    assert_eq!(fire(&a, &id, T0 + HOUR)["fired"], false);
    // Four slots pass unserved: one run, the next slot after now.
    set_clock(&clock, T0 + 6 * HOUR + 1);
    let missed = fire(&a, &id, T0 + 2 * HOUR);
    assert_eq!(
        (missed["fired"].clone(), missed["run"].clone()),
        (json!(true), json!(2))
    );
    assert_eq!(missed["schedules"][0]["nextDue"], T0 + 7 * HOUR);
    wait_runs(&a, 3);
    assert_eq!(a.requests().len(), 4);
    assert_eq!(
        schedule_messages(&a)
            .iter()
            .map(|m| m["details"]["run"].clone())
            .collect::<Vec<_>>(),
        vec![json!(1), json!(2)]
    );
    // The transcript a resumed session opens with keeps them as custom messages.
    let messages = a.call(Op::Messages).unwrap().unwrap();
    let kept: Vec<&Value> = messages
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["customType"] == CUSTOM_TYPE)
        .collect();
    assert_eq!(kept.len(), 2, "{messages}");
    assert_eq!(kept[0]["role"], "custom");
    // Every change is a journal write of the schedule's value; a cancel too.
    let cancelled = a.call(Op::ScheduleCancel { id: "build".into() }).unwrap().unwrap();
    assert_eq!(cancelled["schedule"]["state"], "cancelled");
    assert_eq!(cancelled["schedules"], json!([]));
    set_clock(&clock, T0 + 7 * HOUR);
    assert_eq!(fire(&a, &id, T0 + 7 * HOUR)["fired"], false);
    let states: Vec<(Value, Value)> = a
        .writes()
        .into_iter()
        .filter(|w| w["namespace"] == NAMESPACE && w["op"] == "set")
        .map(|w| (w["value"]["runs"].clone(), w["value"]["state"].clone()))
        .collect();
    assert_eq!(
        states,
        vec![
            (json!(0), json!("active")),
            (json!(1), json!("active")),
            (json!(2), json!("active")),
            (json!(2), json!("cancelled")),
        ]
    );
    assert_eq!(a.events("run_start").len(), 3);
}

#[test]
fn session_core_schedule_fired_during_a_run_waits_for_its_end() {
    let dir = tempfile::tempdir().unwrap();
    let clock = clock(dir.path());
    let gate = dir.path().join("release");
    let held = gate.clone();
    let replies = [
        tool("toolu_1", "await schedule.every('1h', 'check nightly', name='nightly')"),
        text("Scheduled."),
        text("Long answer."),
        text("Nightly done."),
    ];
    let script: p_session_support::Script = Box::new(move |_, index| {
        // The third request (the long run) is answered once the test fired the schedule.
        let end = Instant::now() + HANG;
        while index == 2 && !held.exists() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(10));
        }
        replies.get(index).cloned().unwrap_or_else(|| text("(no more replies)"))
    });
    let a = actor(script, setup(&clock));
    a.prompt("schedule the nightly check");
    let id = list(&a)[0]["id"].as_str().unwrap().to_string();
    set_clock(&clock, T0 + HOUR);
    a.call(Op::Prompt {
        message: "long task".into(),
        images: Vec::new(),
        queue: None,
    })
    .unwrap();
    let end = Instant::now() + HANG;
    while a.requests().len() < 3 {
        assert!(Instant::now() < end, "the long run's request did not arrive");
        std::thread::sleep(Duration::from_millis(10));
    }
    // The fire is recorded at once, but its run waits for the one in progress.
    assert_eq!(fire(&a, &id, T0 + HOUR)["run"], 1);
    assert_eq!(a.events("run_start").len(), 2);
    std::fs::write(&gate, "").unwrap();
    wait_runs(&a, 3);
    let requests = a.requests();
    assert_eq!(requests.len(), 4);
    assert!(
        !user_texts(&requests[2]).contains(&json!("check nightly")),
        "{:?}",
        user_texts(&requests[2])
    );
    assert_eq!(user_texts(&requests[3]).last().unwrap(), &json!("check nightly"));
    assert!(a.events("run_end").iter().all(|e| e["status"] == "completed"));
}

#[test]
fn session_core_schedule_python_api_validates_and_lists() {
    let dir = tempfile::tempdir().unwrap();
    let clock = clock(dir.path());
    let code = "\
for bad in [('30s', 'x'), ('1h', '')]:
    try:
        await schedule.every(*bad)
    except Exception as e:
        print(type(e).__name__, e)
sid = await schedule.at(90, 'one shot', name='once')
rows = await schedule.list()
print([(r['name'], r['kind'], r['next']) for r in rows])
print((await schedule.cancel('once'))['state'])
print(repr(schedule))";
    let script = in_order(vec![tool("toolu_1", code), text("Done.")]);
    let a = actor(script, setup(&clock));
    a.prompt("try schedules");
    let results = tool_results(&a.requests()[1])
        .iter()
        .map(Value::to_string)
        .collect::<String>();
    for expected in [
        "RuntimeError schedule interval must be at least 60s (got 30s)",
        "ValueError schedule prompt must be a non-empty string",
        "[('once', 'at', '2027-01-15T08:01:30.000Z')]",
        "cancelled",
        "<schedule: every(interval, prompt, name=, until=, max_runs=), at(when, prompt), list(), cancel(id)>",
    ] {
        assert!(results.contains(expected), "{expected} not in {results}");
    }
}

#[test]
fn session_core_schedule_fire_that_cannot_start_counts_nothing_and_answers_the_error() {
    let dir = tempfile::tempdir().unwrap();
    let clock = clock(dir.path());
    // The key is a stored credential the test removes: then a prompt cannot start (a worker without credentials).
    let mut setup = setup(&clock);
    setup.vars.push(("ANTHROPIC_API_KEY", String::new()));
    let script = in_order(vec![
        tool("toolu_1", "await schedule.at(3600, 'say hi', name='hello')"),
        text("Scheduled."),
        text("Hi."),
    ]);
    let a = actor(script, setup);
    let auth = a.root.join("agent/auth.json");
    let store_key = || std::fs::write(&auth, r#"{"anthropic": {"type": "api_key", "key": "test-key"}}"#).unwrap();
    store_key();
    assert_eq!(a.prompt("greet me later")["error"], Value::Null);
    let id = list(&a)[0]["id"].as_str().unwrap().to_string();
    std::fs::remove_file(&auth).unwrap();
    set_clock(&clock, T0 + HOUR + 5);
    for _ in 0..2 {
        let error = a
            .call(Op::ScheduleFire {
                id: id.clone(),
                due: T0 + HOUR,
            })
            .unwrap_err();
        assert!(error.contains("No API key found for anthropic"), "{error}");
    }
    // Still due on its slot (an `at` schedule is not done), no run counted, nothing started or shown.
    let listed = list(&a);
    assert_eq!(
        (
            listed[0]["runs"].clone(),
            listed[0]["state"].clone(),
            listed[0]["nextDue"].clone()
        ),
        (json!(0), json!("active"), json!(T0 + HOUR))
    );
    assert_eq!(a.events("run_start").len(), 1);
    assert!(schedule_messages(&a).is_empty());
    let sets = || {
        a.writes()
            .into_iter()
            .filter(|w| w["namespace"] == NAMESPACE && w["op"] == "set")
            .map(|w| (w["value"]["runs"].clone(), w["value"]["state"].clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(sets(), vec![(json!(0), json!("active"))]);
    // The retry once the key is back runs it, and only then is it counted (and done).
    store_key();
    assert_eq!(
        fire(&a, &id, T0 + HOUR),
        json!({"fired": true, "run": 1, "schedules": []})
    );
    wait_runs(&a, 2);
    assert_eq!(user_texts(&a.requests()[2]).last().unwrap(), &json!("say hi"));
    assert_eq!(sets(), vec![(json!(0), json!("active")), (json!(1), json!("done"))]);
}
