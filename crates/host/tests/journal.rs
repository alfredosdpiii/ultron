//! Replays crates/host/fixtures/journal.json, recorded by fixtures/journal.mjs from the TypeScript shell job journal
//! (shell-jobs.ts restored after a restart) and utils/shell.ts getShellEnv; plus the session worker's jobEvent
//! rules with literal values.

use host::journal::{JobRecord, handle, job_event, job_summary, persisted, restore};
use host::shell_env::shell_env;
use serde_json::{Map, Value, json};
use std::path::Path;

fn fixture() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/journal.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn swap(value: &Value, from: &str, to: &str) -> Value {
    serde_json::from_str(&value.to_string().replace(from, to)).unwrap()
}

/// Each scenario's journal and spill files in a temp dir, as the generator laid them out.
fn restored(scenario: &Value, dir: &Path, now: u64) -> Result<host::journal::Restored, String> {
    for (id, text) in scenario["files"].as_object().unwrap() {
        std::fs::write(dir.join(format!("{id}.log")), text.as_str().unwrap()).unwrap();
    }
    let saved = swap(&scenario["saved"], "<dir>", dir.to_str().unwrap());
    restore(if saved.is_null() { None } else { Some(&saved) }, now)
}

#[test]
fn host_journal_restart_replies_match_typescript() {
    let data = fixture();
    let now = data["now"].as_u64().unwrap();
    let requests = [
        ("list", "shell.list", json!({}), "main"),
        ("child_list", "shell.list", json!({}), "child"),
        ("get", "shell.get", json!({"id": "job-00000001"}), "main"),
        ("get_big", "shell.get", json!({"id": "job-00000005"}), "main"),
        (
            "result",
            "shell.result",
            json!({"id": "job-00000002", "wait": 1}),
            "main",
        ),
        ("read", "shell.read", json!({"id": "job-00000002", "cursor": 6}), "main"),
        ("child_get_main", "shell.get", json!({"id": "job-00000002"}), "child"),
    ];
    for scenario in data["scenarios"].as_array().unwrap() {
        let dir = tempfile::tempdir().unwrap();
        let dir_text = dir.path().to_str().unwrap();
        let name = scenario["name"].as_str().unwrap();
        let restored = restored(scenario, dir.path(), now);
        for (key, kind, payload, lane) in &requests {
            let payload: Map<String, Value> = payload.as_object().unwrap().clone();
            let got = match &restored {
                Ok(restored) => handle(&restored.records, kind, &payload, lane, now),
                Err(error) => Err(error.clone()),
            };
            let got = match got {
                Ok(value) => json!({ "ok": swap(&value, dir_text, "<dir>") }),
                Err(error) => json!({ "error": error }),
            };
            assert_eq!(got, scenario["replies"][key], "{name} {key}");
        }
        let persist = restored.ok().and_then(|restored| restored.persist);
        let persist = persist.map_or(Value::Null, |value| swap(&value, dir_text, "<dir>"));
        assert_eq!(persist, scenario["persisted"], "{name}");
    }
}

#[test]
fn host_journal_summaries_match_typescript() {
    let data = fixture();
    let scenario = &data["scenarios"][0];
    let dir = tempfile::tempdir().unwrap();
    let records = restored(scenario, dir.path(), data["now"].as_u64().unwrap())
        .unwrap()
        .records;
    let got: Vec<Value> = records
        .iter()
        .rev()
        .map(|job| json!([job.id, job_summary(job, 240), job_summary(job, 20)]))
        .collect();
    assert_eq!(json!(got), scenario["summaries"]);
}

#[test]
fn host_journal_round_trips_records() {
    let data = fixture();
    let jobs = data["scenarios"][0]["saved"]["jobs"].as_array().unwrap();
    let records: Vec<JobRecord> = jobs.iter().map(JobRecord::from_value).collect();
    let document = persisted(&records);
    let again: Vec<JobRecord> = document["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .map(JobRecord::from_value)
        .collect();
    assert_eq!(again.len(), 6);
    assert_eq!(again[5].tail, "");
    assert_eq!(
        again[5].error.as_deref(),
        Some("timed out after 2s; process tree killed")
    );
    assert_eq!(again[5].timeout, Some(2.0));
    assert_eq!(document["jobs"][5]["timeout"], json!(2));
}

#[test]
fn host_journal_job_done_events_follow_session_worker() {
    let record = JobRecord::from_value(&json!({
        "id": "job-0000abcd", "lane": "w1", "rootId": "root-7", "command": "make", "status": "completed",
        "exitCode": 2, "tail": "a\n\n b \nc\nd\n",
    }));
    assert_eq!(job_event(&record, true, false), None);
    assert_eq!(job_event(&record, false, true), None);
    assert_eq!(
        job_event(&record, false, false),
        Some(json!({
            "kind": "job_done", "id": "job-0000abcd", "status": "completed", "summary": "exit 2; b | c | d",
            "fetch": "await rlm.job(\"job-0000abcd\")", "lane": "w1", "rootId": "root-7",
        }))
    );
    let mut no_root = record.clone();
    no_root.root_id = None;
    assert!(job_event(&no_root, false, false).unwrap().get("rootId").is_none());
}

#[test]
fn host_shell_env_path_matches_typescript() {
    for case in fixture()["shell_env"].as_array().unwrap() {
        let env: Vec<(String, String)> = case["env"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
            .collect();
        let got: Vec<Value> = shell_env(&env, "/a/agent/bin")
            .into_iter()
            .map(|(k, v)| json!([k, v]))
            .collect();
        assert_eq!(json!(got), case["result"], "{env:?}");
    }
}
