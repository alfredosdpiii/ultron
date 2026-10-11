//! Replays fixtures/kernel-pool.json (recorded by fixtures/generate-kernel-pool.mjs from kernel-pool.ts over fake
//! kernels with a fixed clock): every operation returns the same value or error, and each lane sees the same kernel
//! lifecycle (create, snapshot, shutdown). TS close() evicts lanes concurrently, so the log is compared per lane.

use delegation::kernel_pool::{KernelPool, KernelPoolOptions, Lease, PoolableKernel, SnapshotPath, snapshot_outcome};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::{Arc, Mutex};

struct Fake {
    lane: String,
    reply: Value,
    log: Arc<Mutex<Vec<String>>>,
}

impl PoolableKernel for Fake {
    fn snapshot(&mut self, path: &str) -> Result<Vec<String>, String> {
        self.log.lock().unwrap().push(format!("snapshot {} {path}", self.lane));
        snapshot_outcome(&self.reply)
    }
    fn shutdown(&mut self) {
        self.log.lock().unwrap().push(format!("shutdown {}", self.lane));
    }
}

/// The fixture's scripted snapshot outcome as the kernel's reply (`throws` is the transport error text).
fn reply(outcome: &Value) -> Value {
    if let Some(text) = outcome["throws"].as_str() {
        return json!({ "status": "error", "error": { "evalue": text } });
    }
    if outcome["none"] == true {
        return json!({ "status": "ok" });
    }
    if let Some(error) = outcome.get("error") {
        return if error.is_null() {
            json!({ "status": "error" })
        } else {
            json!({ "status": "error", "error": { "evalue": error } })
        };
    }
    json!({ "status": "ok", "snapshot": { "skipped": outcome["skipped"] } })
}

fn per_lane(log: &[String], lane: &str) -> Vec<String> {
    log.iter()
        .filter(|l| l.split(' ').nth(1) == Some(lane))
        .cloned()
        .collect()
}

#[test]
fn delegation_kernel_pool_replays_typescript_scenarios() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/kernel-pool.json");
    let fixture: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    for scenario in fixture.as_array().unwrap() {
        let name = scenario["name"].as_str().unwrap();
        let options = &scenario["options"];
        let clock = Arc::new(Mutex::new(1000.0_f64));
        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        let snapshots = scenario["snapshots"].clone();
        let (create_log, now) = (log.clone(), clock.clone());
        let pool = KernelPool::new(KernelPoolOptions {
            create: Box::new(move |lane: &str| {
                create_log.lock().unwrap().push(format!("create {lane}"));
                let outcome = snapshots.get(lane).cloned().unwrap_or(json!({ "skipped": [] }));
                Fake {
                    lane: lane.into(),
                    reply: reply(&outcome),
                    log: create_log.clone(),
                }
            }),
            max_live: options["maxLive"].as_u64().unwrap() as usize,
            idle_ttl_ms: options["idleTtlMs"].as_f64().unwrap(),
            max_pinned: options["maxPinned"].as_u64().map(|n| n as usize),
            snapshot_path: (options["snapshot"] == true)
                .then(|| Box::new(|lane: &str| Some(format!("/snap/{lane}.pkl"))) as SnapshotPath),
            now: Box::new(move || *now.lock().unwrap()),
        });
        let mut pool = match pool {
            Ok(pool) => pool,
            Err(error) => {
                assert_eq!(Some(error.as_str()), scenario["constructError"].as_str(), "{name}");
                continue;
            }
        };
        assert!(scenario["constructError"].is_null(), "{name}");
        let mut leases: Vec<Lease> = Vec::new();
        for (index, step) in scenario["steps"].as_array().unwrap().iter().enumerate() {
            let op = step["op"].as_array().unwrap();
            let s = |n: usize| op[n].as_str().unwrap_or_default().to_string();
            let outcome: Result<Value, String> = match op[0].as_str().unwrap() {
                "acquire" => pool.acquire(&s(1)).map(|lease| {
                    leases.push(lease);
                    json!(pool.kernel(lease).unwrap().lane)
                }),
                "release" => {
                    pool.release(leases[op[1].as_u64().unwrap() as usize]);
                    Ok(Value::Null)
                }
                "tick" => {
                    *clock.lock().unwrap() += op[1].as_f64().unwrap();
                    Ok(Value::Null)
                }
                "pin" => pool.pin(&s(1), &s(2)).map(|()| Value::Null),
                "unpin" => {
                    pool.unpin(&s(1), &s(2));
                    Ok(Value::Null)
                }
                "evict" => Ok(pool.evict(&s(1), "explicit")),
                "sweep" => Ok(Value::from(pool.sweep())),
                "stats" => Ok(pool.stats()),
                "evictions" => Ok(Value::from(pool.evictions().to_vec())),
                "live" => Ok(pool.live(&s(1)).map_or(Value::Null, |k| json!(k.lane))),
                "close" => {
                    pool.close();
                    Ok(Value::Null)
                }
                other => panic!("unknown op {other}"),
            };
            let at = format!("{name} step {index} {op:?}");
            match outcome {
                Ok(result) => assert_eq!(result, step["result"], "{at}"),
                Err(error) => assert_eq!(Some(error.as_str()), step["error"].as_str(), "{at}"),
            }
        }
        let expected: Vec<String> = scenario["log"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l.as_str().unwrap().to_string())
            .collect();
        let actual = log.lock().unwrap().clone();
        assert_eq!(actual.len(), expected.len(), "{name} log");
        for line in &expected {
            let lane = line.split(' ').nth(1).unwrap();
            assert_eq!(per_lane(&actual, lane), per_lane(&expected, lane), "{name} lane {lane}");
        }
    }
}
