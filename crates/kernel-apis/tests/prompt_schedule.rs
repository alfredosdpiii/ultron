//! `schedule.*` (kernel_apis::prompt_schedule; a Rust-only feature, no TypeScript reference): literal expectations
//! for the interval syntax, the session values a schedule writes, firing (slots, duplicates, missed slots collapsing
//! into one fire, `max_runs`, `until`, `at`), cancel, and the requests a cell sends.

use kernel::Signal;
use kernel_apis::artifacts::{KeyedDocuments, MemoryKeyedDocuments};
use kernel_apis::prompt_schedule::{
    CUSTOM_TYPE, Schedule, ScheduleModule, State, fire_message, format_interval, parse_interval, text,
};
use kernel_apis::{ApiModule, ManualTickClock, Request, RootOnly};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

const T0: i64 = 1_800_000_000_000;
const HOUR: i64 = 3_600_000;

/// A keyed store the test can look into after the module wrote it.
#[derive(Clone, Default)]
struct Shared(Arc<MemoryKeyedDocuments>);

impl KeyedDocuments for Shared {
    fn get(&self, key: &str) -> Result<Option<Value>, String> {
        self.0.get(key)
    }
    fn set(&self, key: &str, value: &Value) -> Result<(), String> {
        self.0.set(key, value)
    }
    fn list(&self, prefix: &str) -> Result<Vec<(String, Value)>, String> {
        self.0.list(prefix)
    }
}

struct Fixture {
    module: ScheduleModule,
    clock: Arc<ManualTickClock>,
    store: Shared,
    reports: Arc<Mutex<Vec<Vec<Value>>>>,
}

fn fixture() -> Fixture {
    let clock = ManualTickClock::new(T0);
    let store = Shared::default();
    let reports = Arc::new(Mutex::new(Vec::new()));
    let seen = reports.clone();
    let module = ScheduleModule::new(
        Box::new(store.clone()),
        "session-1",
        clock.clone(),
        Some(Arc::new(move |entries: Vec<Value>| seen.lock().unwrap().push(entries))),
    );
    Fixture {
        module,
        clock,
        store,
        reports,
    }
}

fn call(module: &ScheduleModule, kind: &str, payload: Value, caller_task: Option<&str>) -> Result<Value, String> {
    let host = RootOnly {
        now: kernel_apis::util::system_clock(),
    };
    let signal = Signal::new();
    let payload = payload.as_object().unwrap().clone();
    module.handle(&Request {
        kind,
        payload: &payload,
        lane: "main",
        caller_task,
        host: &host,
        signal: &signal,
    })
}

fn every(f: &Fixture, payload: Value) -> Value {
    call(&f.module, "schedule.every", payload, None).unwrap()
}

#[test]
fn kernel_apis_prompt_schedule_interval_syntax() {
    let ok = |v: Value| parse_interval(&v).unwrap();
    assert_eq!(ok(json!("90s")), 90_000);
    assert_eq!(ok(json!("15m")), 900_000);
    assert_eq!(ok(json!("1h")), 3_600_000);
    assert_eq!(ok(json!(" 1d ")), 86_400_000);
    assert_eq!(ok(json!("1h30m")), 5_400_000);
    assert_eq!(ok(json!(120)), 120_000);
    assert_eq!(ok(json!(60.5)), 60_500);
    assert_eq!(ok(json!("300")), 300_000);
    assert_eq!(
        parse_interval(&json!("30s")),
        Err("schedule interval must be at least 60s (got 30s)".to_string())
    );
    assert_eq!(
        parse_interval(&json!(59)),
        Err("schedule interval must be at least 60s (got 59s)".to_string())
    );
    assert_eq!(
        parse_interval(&json!("366d")),
        Err("schedule interval must be at most 365d".to_string())
    );
    for bad in [
        json!("1x"),
        json!("h"),
        json!("15"),
        json!("1h30"),
        json!(""),
        json!(true),
        json!(-60),
    ] {
        assert!(parse_interval(&bad).is_err(), "{bad}");
    }
    assert_eq!(
        parse_interval(&json!("1x")),
        Err(r#"schedule interval must be like "90s", "15m", "1h" or "1d", or seconds (got "1x")"#.to_string())
    );
    assert_eq!(format_interval(5_400_000), "1h30m");
    assert_eq!(format_interval(90_000), "1m30s");
    assert_eq!(format_interval(86_400_000), "1d");
    assert_eq!(format_interval(90_061_000), "1d1h1m1s");
    assert_eq!(format_interval(60_500), "60500ms");
}

#[test]
fn kernel_apis_prompt_schedule_every_records_a_session_value_and_reports_it() {
    let f = fixture();
    let created = every(
        &f,
        json!({"interval": "1h", "prompt": "check the build", "name": "build"}),
    );
    let id = created["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("sch-") && id.len() == 12, "{id}");
    let stored = f.store.get(&id).unwrap().unwrap();
    assert_eq!(
        stored,
        json!({"id": id, "name": "build", "sessionId": "session-1", "prompt": "check the build", "kind": "every",
               "intervalMs": HOUR, "until": null, "maxRuns": null, "createdAt": T0, "nextDue": T0 + HOUR,
               "runs": 0, "lastFired": null, "state": "active"})
    );
    assert_eq!(created["every"], "1h");
    assert_eq!(created["next"], kernel_apis::util::iso(T0 + HOUR));
    assert_eq!(
        *f.reports.lock().unwrap(),
        vec![vec![json!({"id": id, "name": "build", "nextDue": T0 + HOUR})]]
    );
    // A default name is the id; an active name is taken.
    let unnamed = every(&f, json!({"interval": 120, "prompt": "x"}));
    assert_eq!(unnamed["name"], unnamed["id"]);
    assert_eq!(
        call(
            &f.module,
            "schedule.every",
            json!({"interval": "2h", "prompt": "y", "name": "build"}),
            None
        ),
        Err("a schedule named build is already active".to_string())
    );
    // `list` gives every schedule of the session, and `text: true` the /schedules lines.
    let listed = call(&f.module, "schedule.list", json!({}), None).unwrap();
    assert_eq!(listed.as_array().unwrap().len(), 2);
    let lines = call(&f.module, "schedule.list", json!({"text": true}), None).unwrap();
    assert_eq!(
        lines.as_str().unwrap().lines().next().unwrap(),
        format!(
            "{}  next 2027-01-15T08:02:00.000Z  runs 0  every 2m  {}",
            unnamed["id"].as_str().unwrap(),
            unnamed["id"].as_str().unwrap()
        )
    );
    assert_eq!(
        lines.as_str().unwrap().lines().nth(1).unwrap(),
        format!("build  next 2027-01-15T09:00:00.000Z  runs 0  every 1h  {id}")
    );
}

#[test]
fn kernel_apis_prompt_schedule_fire_runs_each_slot_once_and_collapses_missed_slots() {
    let f = fixture();
    let id = every(
        &f,
        json!({"interval": "1h", "prompt": "check the build", "name": "build"}),
    )["id"]
        .as_str()
        .unwrap()
        .to_string();
    // Not due yet.
    assert!(f.module.fire(&id, T0 + HOUR).unwrap().is_none());
    f.clock.set(T0 + HOUR + 5_000);
    // A slot that is not the schedule's next one does nothing.
    assert!(f.module.fire(&id, T0).unwrap().is_none());
    let fired = f.module.fire(&id, T0 + HOUR).unwrap().unwrap();
    assert_eq!(fired.schedule.runs, 1);
    assert_eq!(fired.schedule.next_due, Some(T0 + 2 * HOUR));
    assert_eq!(fired.schedule.last_fired, Some(T0 + HOUR + 5_000));
    assert_eq!(
        fired.message,
        json!({"role": "custom", "customType": CUSTOM_TYPE, "content": "check the build", "display": true,
               "details": {"name": "build", "id": id, "run": 1}, "timestamp": T0 + HOUR + 5_000})
    );
    // The same slot again (another server generation) is a duplicate.
    assert!(f.module.fire(&id, T0 + HOUR).unwrap().is_none());
    // Five slots pass with no server: one fire, and the next slot is after now.
    f.clock.set(T0 + 6 * HOUR + 1);
    let fired = f.module.fire(&id, T0 + 2 * HOUR).unwrap().unwrap();
    assert_eq!((fired.schedule.runs, fired.schedule.next_due), (2, Some(T0 + 7 * HOUR)));
    assert_eq!(fired.message["details"]["run"], 2);
    let stored = Schedule::from_json(&f.store.get(&id).unwrap().unwrap()).unwrap();
    assert_eq!(stored, fired.schedule);
}

#[test]
fn kernel_apis_prompt_schedule_max_runs_until_and_at_end_the_schedule() {
    let f = fixture();
    let limited = every(&f, json!({"interval": "1h", "prompt": "a", "max_runs": 2}))["id"]
        .as_str()
        .unwrap()
        .to_string();
    f.clock.set(T0 + HOUR);
    assert_eq!(
        f.module.fire(&limited, T0 + HOUR).unwrap().unwrap().schedule.next_due,
        Some(T0 + 2 * HOUR)
    );
    f.clock.set(T0 + 2 * HOUR);
    let last = f.module.fire(&limited, T0 + 2 * HOUR).unwrap().unwrap().schedule;
    assert_eq!((last.runs, last.next_due, last.state), (2, None, State::Done));
    assert!(!last.active());

    let f = fixture();
    let bounded = every(
        &f,
        json!({"interval": "1h", "prompt": "b", "until_ms": T0 + 2 * HOUR + 1}),
    )["id"]
        .as_str()
        .unwrap()
        .to_string();
    f.clock.set(T0 + HOUR);
    let first = f.module.fire(&bounded, T0 + HOUR).unwrap().unwrap().schedule;
    assert_eq!(first.next_due, Some(T0 + 2 * HOUR));
    f.clock.set(T0 + 2 * HOUR);
    let second = f.module.fire(&bounded, T0 + 2 * HOUR).unwrap().unwrap().schedule;
    assert_eq!((second.next_due, second.state), (None, State::Done));
    assert_eq!(
        call(
            &f.module,
            "schedule.every",
            json!({"interval": "1h", "prompt": "c", "until_in_ms": 1000}),
            None
        ),
        Err("schedule until is before the first run (2027-01-15T11:00:00.000Z)".to_string())
    );

    let f = fixture();
    let once = call(
        &f.module,
        "schedule.at",
        json!({"prompt": "d", "when_in_ms": 90_000}),
        None,
    )
    .unwrap();
    assert_eq!(
        (once["kind"].clone(), once["nextDue"].clone()),
        (json!("at"), json!(T0 + 90_000))
    );
    f.clock.set(T0 + 90_000);
    let ran = f
        .module
        .fire(once["id"].as_str().unwrap(), T0 + 90_000)
        .unwrap()
        .unwrap()
        .schedule;
    assert_eq!((ran.runs, ran.state, ran.next_due), (1, State::Done, None));
    assert_eq!(
        call(&f.module, "schedule.at", json!({"prompt": "e", "when_ms": T0}), None),
        Err("schedule.at time must be in the future (got 2027-01-15T08:00:00.000Z)".to_string())
    );
}

#[test]
fn kernel_apis_prompt_schedule_cancel_by_id_or_name_stops_firing() {
    let f = fixture();
    let a = every(&f, json!({"interval": "1h", "prompt": "a", "name": "alpha"}));
    let b = every(&f, json!({"interval": "1h", "prompt": "b"}));
    let cancelled = call(&f.module, "schedule.cancel", json!({"id": "alpha"}), None).unwrap();
    assert_eq!(
        (cancelled["state"].clone(), cancelled["nextDue"].clone()),
        (json!("cancelled"), Value::Null)
    );
    f.module.cancel(b["id"].as_str().unwrap()).unwrap();
    f.clock.set(T0 + HOUR);
    assert!(f.module.fire(a["id"].as_str().unwrap(), T0 + HOUR).unwrap().is_none());
    assert_eq!(f.module.entries().unwrap(), Vec::<Value>::new());
    assert_eq!(f.reports.lock().unwrap().last().unwrap(), &Vec::<Value>::new());
    assert_eq!(f.module.cancel("alpha"), Err("no active schedule alpha".to_string()));
    // The name is free again.
    every(&f, json!({"interval": "1h", "prompt": "a2", "name": "alpha"}));
}

#[test]
fn kernel_apis_prompt_schedule_refuses_subagents_bad_input_and_too_many() {
    let f = fixture();
    assert_eq!(
        call(
            &f.module,
            "schedule.every",
            json!({"interval": "1h", "prompt": "a"}),
            Some("task-1")
        ),
        Err("schedule.every is available to the root agent only".to_string())
    );
    assert_eq!(
        call(
            &f.module,
            "schedule.every",
            json!({"interval": "1h", "prompt": "  "}),
            None
        ),
        Err("schedule prompt must be a non-empty string".to_string())
    );
    assert_eq!(
        call(
            &f.module,
            "schedule.every",
            json!({"interval": "1h", "prompt": "a", "colour": 1}),
            None
        ),
        Err("Unknown payload field: colour".to_string())
    );
    assert_eq!(
        call(
            &f.module,
            "schedule.every",
            json!({"interval": "1h", "prompt": "a", "name": "two words"}),
            None
        ),
        Err("schedule name must be one word of 1 to 64 bytes".to_string())
    );
    assert_eq!(
        call(
            &f.module,
            "schedule.every",
            json!({"interval": "1h", "prompt": "a", "max_runs": 0}),
            None
        ),
        Err("schedule max_runs must be an integer from 1 to 100000".to_string())
    );
    for n in 0..32 {
        every(&f, json!({"interval": "1h", "prompt": format!("p{n}")}));
    }
    assert_eq!(
        call(
            &f.module,
            "schedule.every",
            json!({"interval": "1h", "prompt": "one more"}),
            None
        ),
        Err("a session can have at most 32 active schedules".to_string())
    );
}

#[test]
fn kernel_apis_prompt_schedule_values_of_another_session_are_not_its_own() {
    let f = fixture();
    let copied = Schedule {
        id: "sch-0000aaaa".into(),
        name: "copied".into(),
        session_id: "session-0".into(),
        prompt: "from the fork source".into(),
        kind: kernel_apis::prompt_schedule::Kind::Every,
        interval_ms: Some(HOUR),
        until: None,
        max_runs: None,
        created_at: T0,
        next_due: Some(T0 + HOUR),
        runs: 0,
        last_fired: None,
        state: State::Active,
    };
    f.store.set(&copied.id, &copied.to_json()).unwrap();
    assert!(f.module.all().unwrap().is_empty());
    f.clock.set(T0 + HOUR);
    assert!(f.module.fire("sch-0000aaaa", T0 + HOUR).unwrap().is_none());
    assert_eq!(
        text(&[]),
        "No schedules. A cell creates one with `await schedule.every(\"1h\", prompt)`."
    );
    assert_eq!(
        fire_message(&copied, 3, 7)["details"],
        json!({"name": "copied", "id": "sch-0000aaaa", "run": 3})
    );
}
