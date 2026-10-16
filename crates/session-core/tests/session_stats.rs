//! Replays fixtures/session_stats.json (fixtures/generate-session-stats.mjs, the TypeScript `SessionStatsRecorder`):
//! the same recorder calls on the same clock leave the same stored documents, written as often, and the same
//! snapshots; the pure helpers (`cellApis`, `classifyNudge`, `statsLane`, `parseSessionStats`) agree case by case.

use serde_json::Value;
use session_core::session_stats::{
    GuardOutcome, SessionStats, StatsStore, cell_apis, classify_nudge, parse_session_stats, stats_lane,
};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

static FIXTURE: LazyLock<Value> =
    LazyLock::new(|| serde_json::from_str(include_str!("../fixtures/session_stats.json")).expect("fixture parses"));

/// A store holding one document, refusing the first `fail` writes, recording what it wrote.
struct Recorded {
    document: Mutex<Option<Value>>,
    writes: Mutex<Vec<Value>>,
    fail: AtomicUsize,
}

impl StatsStore for Recorded {
    fn read(&self) -> Result<Option<Value>, String> {
        Ok(self.document.lock().unwrap().clone())
    }
    fn write(&self, document: &Value) -> Result<(), String> {
        if self.fail.load(Ordering::SeqCst) > 0 {
            self.fail.fetch_sub(1, Ordering::SeqCst);
            return Err("disk full".into());
        }
        *self.document.lock().unwrap() = Some(document.clone());
        self.writes.lock().unwrap().push(document.clone());
        Ok(())
    }
}

fn s<'a>(op: &'a Value, key: &str) -> &'a str {
    op[key].as_str().unwrap()
}

/// Run one recorded case; returns (writes, snapshots).
fn replay(case: &Value) -> (Vec<Value>, Vec<Value>) {
    let now = Arc::new(AtomicU64::new(0));
    let clock = now.clone();
    // The scheduled save never fires within a test: only flushes and close save, as in the recording.
    let stats = SessionStats::new(
        case["root"].as_str().unwrap(),
        Arc::new(move || clock.load(Ordering::SeqCst)),
        Duration::from_secs(1_000_000),
    );
    let store = Arc::new(Recorded {
        document: Mutex::new(case["hasStored"].as_bool().unwrap().then(|| case["stored"].clone())),
        writes: Mutex::new(Vec::new()),
        fail: AtomicUsize::new(case["failWrites"].as_u64().unwrap() as usize),
    });
    // The TypeScript recorder reads its store with its first change, the Rust one when the session attaches it; the
    // stored document is the same until the recorder writes.
    stats.attach(store.clone());
    let mut snapshots = Vec::new();
    for op in case["ops"].as_array().unwrap() {
        match s(op, "op") {
            "tick" => now.store(op["now"].as_u64().unwrap(), Ordering::SeqCst),
            "touch" => stats.touch(),
            "cell" => stats.cell(
                s(op, "lane"),
                s(op, "code"),
                op["failed"].as_bool().unwrap(),
                op["masked"].as_u64().unwrap() as usize,
            ),
            "hostCall" => stats.host_call(s(op, "type")),
            "guard" => {
                let record = &op["record"];
                stats.guard(&GuardOutcome {
                    guard: s(record, "guard"),
                    phase: s(record, "phase"),
                    outcome: s(record, "outcome"),
                    ms: record["ms"].as_f64().unwrap(),
                });
            }
            "nudge" => stats.nudge(s(op, "text")),
            "usageLimitBlock" => stats.usage_limit_block(),
            "merge" => stats.merge(s(op, "taskId"), s(op, "status")),
            "childModel" => stats.child_model(s(op, "taskId"), s(op, "model")),
            "externalTurnStarted" => stats.external_turn_started(s(op, "turn")),
            "externalTurnEnded" => stats.external_turn_ended(s(op, "turn")),
            "flush" => stats.flush(),
            "snapshot" => snapshots.push(stats.snapshot()),
            "close" => stats.close(),
            other => panic!("unknown op {other}"),
        }
    }
    stats.flush();
    let writes = store.writes.lock().unwrap().clone();
    (writes, snapshots)
}

fn case(name: &str) -> &'static Value {
    FIXTURE["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("case {name}"))
}

fn check(name: &str) {
    let case = case(name);
    let (writes, snapshots) = replay(case);
    // Compared as text: key order (the newest merge last) is part of the document.
    assert_eq!(
        Value::Array(writes).to_string(),
        case["writes"].to_string(),
        "{name}: stored documents differ from TypeScript"
    );
    assert_eq!(
        Value::Array(snapshots).to_string(),
        case["snapshots"].to_string(),
        "{name}: snapshots differ from TypeScript"
    );
}

/// Every counter: cells per lane kind with the APIs their code names and masked secrets, host requests, guard
/// outcomes by phase, the three steers, refused work, merges and child models (re-keyed newest last), external
/// turns (a turn started twice counts once; close ends an open one); nothing is counted after close.
#[test]
fn session_core_session_stats_counts_like_typescript() {
    check("counting");
}

#[test]
fn session_core_session_stats_external_root_turn_ends_at_close_like_typescript() {
    check("external-root");
}

/// 200 host request types at most (later ones are not added); merges and child models keep the newest 200.
#[test]
fn session_core_session_stats_maps_are_bounded_like_typescript() {
    check("bounded");
}

/// A resumed session keeps its stored counters and their start, under the root it has now; an unversioned
/// document reads as version 1, a malformed one as zeros, a value that is not a document as nothing stored.
#[test]
fn session_core_session_stats_resume_from_stored_documents_like_typescript() {
    for name in ["resumed", "unversioned", "malformed", "not-an-object", "null-stored"] {
        check(name);
    }
}

/// A document in another format version is never overwritten; the counters stay in memory.
#[test]
fn session_core_session_stats_leave_a_foreign_document_alone_like_typescript() {
    check("foreign-version");
    check("string-version");
}

/// Nothing ran: nothing is written. A refused write is retried by the next flush.
#[test]
fn session_core_session_stats_write_only_changes_and_retry_failed_writes_like_typescript() {
    check("nothing-ran");
    check("failing-store");
}

#[test]
fn session_core_session_stats_helpers_match_typescript() {
    for item in FIXTURE["cellApis"].as_array().unwrap() {
        let got: Vec<&str> = cell_apis(item["code"].as_str().unwrap());
        assert_eq!(serde_json::json!(got), item["apis"], "{}", item["code"]);
    }
    for item in FIXTURE["nudges"].as_array().unwrap() {
        let got = classify_nudge(item["text"].as_str().unwrap());
        assert_eq!(serde_json::json!(got), item["kind"], "{}", item["text"]);
    }
    for item in FIXTURE["lanes"].as_array().unwrap() {
        assert_eq!(
            stats_lane(item["lane"].as_str().unwrap()),
            item["kind"],
            "{}",
            item["lane"]
        );
    }
    for item in FIXTURE["parsed"].as_array().unwrap() {
        let got = parse_session_stats(&item["saved"]).unwrap_or(Value::Null);
        assert_eq!(got, item["parsed"], "{}", item["saved"]);
    }
}

/// The store of the scheduled-save test: each write is sent on.
struct Sent(Mutex<std::sync::mpsc::Sender<Value>>);

impl StatsStore for Sent {
    fn read(&self) -> Result<Option<Value>, String> {
        Ok(None)
    }
    fn write(&self, document: &Value) -> Result<(), String> {
        let _ = self.0.lock().unwrap().send(document.clone());
        Ok(())
    }
}

/// A change is saved by itself shortly after it is made (the scheduled save), without a flush.
#[test]
fn session_core_session_stats_save_on_their_own_after_a_change() {
    let stats = SessionStats::new("lane", Arc::new(|| 7), Duration::from_millis(1));
    let (sender, written) = std::sync::mpsc::channel();
    stats.attach(Arc::new(Sent(Mutex::new(sender))));
    stats.usage_limit_block();
    let document = written
        .recv_timeout(Duration::from_secs(120))
        .expect("the scheduled save ran");
    assert_eq!(document["usageLimitBlocks"], 1);
    assert_eq!(document["updatedAt"], 7);
}
