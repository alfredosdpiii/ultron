//! A16 (partial): memory skip causes no retrieval; scopes survive consolidation.
//! Ported from packages/coding-agent/test/ultron-a16-memory-skip-scope.test.ts over the Rust `MemoryService` and
//! the worker's `memory.*` dispatch (`KernelApis` + `MemoryApi` with the worker gate), through the real Hindsight
//! adapter (`hindsight::HindsightBackend`) over the fake Hindsight of `memory_support`.
//! ultron-a16-live-hindsight.test.ts is ported as an opt-in test with the same switch (`ULTRON_LIVE_HINDSIGHT=1`,
//! `ULTRON_LIVE_HINDSIGHT_URL`, default http://localhost:8888) over the reqwest transport; it is ignored by default
//! and no Rust live run is recorded yet, which keeps the row partial (TS stands in a recorded demonstration).

mod memory_support;

use hindsight::{HindsightBackend, HindsightOptions, HttpRequest, ReqwestTransport, Transport};
use kernel::{HostHandler, Signal};
use kernel_apis::{KernelApis, MemoryApi, MemoryService, worker_gate};
use memory_support::*;
use serde_json::{Value, json};
use std::sync::Arc;

#[test]
fn acceptance_a16_skip_sends_nothing_and_secrets_are_refused_before_any_request_ported_part() {
    let hindsight = FakeHindsight::new();
    let log = GateLog::default();
    let memory = open(&hindsight, &durable_store(), gate(false, log.clone()));
    for scope in ["session", "project", "global"] {
        let prepared = memory
            .prepare(
                &p(json!({"query": "what is the launcher path", "scope": scope, "taskId": format!("task-{scope}")})),
                &sig(),
            )
            .unwrap();
        assert_match(&prepared, &json!({"results": [], "context": ""}));
        assert_match(
            &prepared["operation"],
            &json!({"state": "skipped", "phase": "gate", "references": []}),
        );
        assert!(prepared["operation"].get("tags").is_none());
    }
    let actions: Vec<Value> = log.lock().unwrap().iter().map(|r| r["action"].clone()).collect();
    assert_eq!(actions, vec![json!("recall"); 3]);
    // Zero recall calls, and no other Hindsight endpoint was touched either.
    assert_eq!(hindsight.captured(), vec![]);

    // The production binding: the worker keeps every write except one holding a secret.
    let service = MemoryService::new(
        Arc::new(kernel_apis::MemoryDocumentStore::default()),
        hindsight.backend(),
        worker_gate(),
    )
    .unwrap();
    let refuse: Arc<dyn HostHandler> =
        Arc::new(|kind: &str, _: serde_json::Map<String, Value>, _: &Signal| Err(format!("unknown {kind}")));
    let worker = KernelApis::new(refuse).with(Arc::new(MemoryApi { service: Some(service) }));
    let handle = |kind: &str, payload: Value| worker.handle(kind, p(payload), &Signal::new());
    let secret = "the deploy token is ghp_abcdefghijklmnopqrstuvwx";
    let refused = handle(
        "memory.propose",
        json!({"text": secret, "evidence": [{"ref": "task:1"}]}),
    )
    .unwrap();
    assert_eq!(refused["state"], "sensitive");
    assert_eq!(hindsight.captured(), vec![]);
    let stored = handle(
        "memory.propose",
        json!({"text": "tests live in test/", "evidence": [{"ref": "task:2"}]}),
    )
    .unwrap();
    // Accepted is not stored: a correction waits until the receipt confirms retention.
    let got = handle("memory.get", json!({"id": stored["memoryId"]})).unwrap();
    assert_eq!(got["state"], "stored");
    let written = hindsight.captured().len();
    assert!(written > 0);
    let corrected = handle(
        "memory.correct",
        json!({"id": stored["memoryId"], "text": secret, "evidence": [{"ref": "user:3"}]}),
    )
    .unwrap();
    assert_eq!(corrected["state"], "sensitive");
    assert_eq!(hindsight.captured().len(), written);
}

#[test]
fn acceptance_a16_scope_tags_pinned_through_consolidation_and_cross_scope_merge_refused_ported_part() {
    let hindsight = FakeHindsight::new();
    let memory = open(&hindsight, &durable_store(), keep_gate());
    let session_fact = memory
        .propose(
            &p(json!({"text": "session: use pnpm", "evidence": [{"ref": "task:1"}]})),
            &sig(),
        )
        .unwrap();
    let project_fact = memory
        .propose(
            &p(json!({"text": "project: tests live in test/", "evidence": [{"ref": "task:2"}], "scope": "project"})),
            &sig(),
        )
        .unwrap();
    let got = memory.get(&p(json!({"id": session_fact["memoryId"]})), &sig()).unwrap();
    assert_eq!(got["state"], "stored");
    let corrected = memory
        .correct(
            &p(json!({"id": session_fact["memoryId"], "text": "session: use npm, not pnpm", "evidence": [{"ref": "user:3"}]})),
            &sig(),
        )
        .unwrap();
    for operation in [&session_fact, &project_fact, &corrected] {
        assert_eq!(operation["state"], "accepted");
    }

    // Every write carries exactly its scope's tags and pins consolidation to those tags alone.
    let retains: Vec<Value> = hindsight
        .retains()
        .iter()
        .map(|body| {
            let item = &body["items"][0];
            json!([
                item["document_id"],
                item["tags"],
                item["observation_scopes"],
                item["update_mode"]
            ])
        })
        .collect();
    let (session, project) = (tags("session"), tags("project"));
    assert_eq!(
        retains,
        vec![
            json!([session_fact["memoryId"], session, [session], "replace"]),
            json!([project_fact["memoryId"], project, [project], "replace"]),
            json!([session_fact["memoryId"], session, [session], "replace"]),
        ]
    );

    hindsight.consolidate();
    let session_recall = memory
        .prepare(&p(json!({"query": "package manager", "taskId": "task-a"})), &sig())
        .unwrap();
    let project_recall = memory
        .prepare(
            &p(json!({"query": "package manager", "scope": "project", "taskId": "task-b"})),
            &sig(),
        )
        .unwrap();
    let observation = |prepared: &Value| -> Value {
        prepared["results"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["type"] == "observation")
            .cloned()
            .unwrap()
    };
    let (session_observation, project_observation) = (observation(&session_recall), observation(&project_recall));
    assert_eq!(session_observation["tags"], session);
    let text = session_observation["text"].as_str().unwrap();
    assert!(
        text.contains("use npm, not pnpm") && !text.contains("tests live in test/"),
        "{text}"
    );
    assert_eq!(project_observation["tags"], project);
    assert!(!project_observation["text"].as_str().unwrap().contains("npm"));
    let recalls = hindsight.recalls();
    assert_eq!(
        recalls.iter().map(|r| r["tags"].clone()).collect::<Vec<_>>(),
        vec![session.clone(), project.clone()]
    );
    assert!(recalls.iter().all(|r| r["tags_match"] == "exact"));

    // Fault: a consolidation that ignores observation scopes produces a cross-scope observation.
    hindsight.lock().merge_across_scopes = true;
    hindsight.consolidate();
    let merged: Vec<Value> = hindsight
        .lock()
        .observations
        .iter()
        .map(|o| o["tags"].clone())
        .collect();
    assert_eq!(merged, vec![json!(["ultron:session:s1", "ultron:project:p1"])]);
    let prepared = memory
        .prepare(&p(json!({"query": "package manager", "taskId": "task-c"})), &sig())
        .unwrap();
    assert_match(&prepared, &json!({"results": [{"type": "world"}]}));
    hindsight.lock().leak_observations = true;
    let error = memory
        .prepare(
            &p(json!({"query": "package manager", "taskId": "task-c", "refresh": true})),
            &sig(),
        )
        .unwrap_err();
    assert_eq!(error.code, "INVALID_RESPONSE");
    let decisions = memory.why(&json!("task-c")).unwrap();
    let states: Vec<Value> = decisions
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["state"].clone())
        .collect();
    assert_eq!(states, vec![json!("recalled"), json!("failed")]);
    assert_match(
        &decisions[1],
        &json!({"error": {"code": "INVALID_RESPONSE"}, "tags": session}),
    );
    assert!(decisions[1].get("references").is_none());
}

/// `until(read, timeoutMs)`: polls every 3 s.
fn until(timeout: std::time::Duration, mut read: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if read() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_secs(3));
    }
    false
}

/// Opt-in: a live Hindsight 0.9.2 with a disposable bank that is always deleted. Consolidation runs Hindsight's
/// configured LLM.
#[test]
#[ignore = "live Hindsight: ULTRON_LIVE_HINDSIGHT=1 cargo test -- --ignored"]
fn acceptance_a16_live_recall_after_a_real_consolidation_keeps_exact_scope_tags_opt_in() {
    if std::env::var("ULTRON_LIVE_HINDSIGHT").as_deref() != Ok("1") {
        eprintln!("skipped: set ULTRON_LIVE_HINDSIGHT=1 to run against a live Hindsight");
        return;
    }
    let base = std::env::var("ULTRON_LIVE_HINDSIGHT_URL").unwrap_or_else(|_| "http://localhost:8888".into());
    let bank = format!("ultron-a16-live-{}", &kernel_apis::util::uuid()[..8]);
    let bank_url = format!("{base}/v1/default/banks/{bank}");
    let http = ReqwestTransport::new().unwrap();
    let raw = |method: &str, url: String, body: Option<&str>| {
        http.send(
            HttpRequest {
                method: method.into(),
                url,
                headers: vec![("content-type".into(), "application/json".into())],
                body: body.map(|b| b.as_bytes().to_vec()),
            },
            1 << 20,
            std::time::Duration::from_secs(120),
            &Signal::new(),
        )
    };
    let _ = raw("PUT", bank_url.clone(), Some("{}"));
    let scopes = json!({"session": ["ultron:session:a16-live"], "project": ["ultron:project:a16-live"],
        "global": ["ultron:global:a16-live"]});
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut options = HindsightOptions::new(base.clone(), bank.clone(), scopes.as_object().unwrap().clone());
        options.timeout_ms = Some(120_000.0);
        let backend = Arc::new(HindsightBackend::new(options).unwrap());
        let open_gate: kernel_apis::memory::MemoryGate = Arc::new(|request: &Value, _: &Signal| {
            Ok(if request["action"] == "recall" {
                json!({"retrieve": true})
            } else {
                json!({"action": "keep"})
            })
        });
        let memory = MemoryService::new(durable_store(), backend, open_gate).unwrap();
        let session_fact = memory
            .propose(
                &p(json!({"text": "In this repository the package manager is pnpm.", "evidence": [{"ref": "task:1"}]})),
                &sig(),
            )
            .unwrap();
        let project_fact = memory
            .propose(
                &p(
                    json!({"text": "The project's tests live in the test/ directory and run with vitest.",
                    "evidence": [{"ref": "task:2"}], "scope": "project"}),
                ),
                &sig(),
            )
            .unwrap();
        let stored = |id: &Value| {
            memory
                .get(&p(json!({"id": id})), &sig())
                .is_ok_and(|d| d["state"] == "stored")
        };
        for fact in [&session_fact, &project_fact] {
            assert!(
                until(std::time::Duration::from_secs(180), || stored(&fact["memoryId"])),
                "retention of {} completed",
                fact["memoryId"]
            );
        }
        memory
            .correct(
                &p(json!({"id": session_fact["memoryId"],
                    "text": "Correction: in this repository the package manager is npm, not pnpm.",
                    "evidence": [{"ref": "user:3"}]})),
                &sig(),
            )
            .unwrap();
        until(std::time::Duration::from_secs(180), || {
            stored(&session_fact["memoryId"])
        });
        let _ = raw("POST", format!("{bank_url}/consolidate"), None);
        let prepare = |task: &str, scope: &str| {
            memory
                .prepare(
                    &p(json!({"query": "package manager and tests", "taskId": task, "scope": scope, "refresh": true})),
                    &sig(),
                )
                .unwrap()
        };
        let has_observation = |v: &Value| {
            v["results"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["type"] == "observation")
        };
        let (mut session, mut project) = (prepare("live-a", "session"), prepare("live-b", "project"));
        until(std::time::Duration::from_secs(240), || {
            session = prepare("live-a", "session");
            project = prepare("live-b", "project");
            has_observation(&session) && has_observation(&project)
        });
        // The service rejects any recalled item whose tags differ from the scope's (INVALID_RESPONSE), so reaching
        // here already means every result carried exactly its scope's tags.
        for item in session["results"].as_array().unwrap() {
            assert_eq!(item["tags"], scopes["session"]);
        }
        for item in project["results"].as_array().unwrap() {
            assert_eq!(item["tags"], scopes["project"]);
        }
        assert!(has_observation(&session) && has_observation(&project));
        assert!(!session["context"].as_str().unwrap().contains("vitest"));
        let project_context = project["context"].as_str().unwrap();
        assert!(!project_context.contains("pnpm") && !project_context.contains("npm"));
    }));
    let _ = raw("DELETE", bank_url, None);
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}
