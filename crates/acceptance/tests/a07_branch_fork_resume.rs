//! A07: branch, fork and resume restore matching state, and no lesson from an abandoned branch leaks. Every case of
//! packages/coding-agent/test/ultron-a07-branch-fork-resume.test.ts and ultron-a07-lesson-branches.test.ts with
//! their literal expectations, over real format-4 session journals (`session_journal`: create, open, tree fork,
//! delete) whose values hold the whole Ultron state, as the session worker keeps it (`services::SessionValues`): the
//! task journal, definition registry and usage ledger of the typed-agent host, the progress module, the memory
//! journal and the local services (lessons are instruction refinements, injected into later child prompts by the
//! host's refinement hook). Hindsight is the TS test's external stand-in shared by every session.
//!
//! The lesson-branch case drives the conversation on the session's own `main` branch: a prompt appends the user and
//! assistant entries, navigation moves the branch tip (the storage effect of `navigateTree` to an entry), and the
//! refinement branch is the tip plus the entries on its path, the session worker's computation.

mod services_support;

use delegation::usage::UsageLedger;
use kernel::Signal;
use kernel_apis::memory::MemoryBackend;
use kernel_apis::{MemoryApi, MemoryService, ProgressModule, worker_gate};
use serde_json::{Map, Value, json};
use services::local::{Branch, LocalServices, LocalServicesModule};
use services::{SessionValues, instruction_hook};
use services_support::{Host, Setup, build, rejects};
use session_journal::session::BRANCH_TIP;
use session_journal::{ForkOptions, Metadata, SessionRepo, SystemClock, Write};
use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct Document {
    id: String,
    original_text: String,
    tags: Vec<Value>,
}

/// External Hindsight stand-in shared by every session: documents survive session forks and deletes.
#[derive(Clone, Default)]
struct Hindsight {
    documents: Arc<Mutex<Vec<Document>>>,
    operations: Arc<Mutex<Vec<(String, String)>>>,
}

struct Backend {
    hindsight: Hindsight,
    session: String,
}

impl MemoryBackend for Backend {
    fn namespace(&self) -> String {
        "fake://hindsight/a07".into()
    }
    fn scope_tags(&self) -> Map<String, Value> {
        json!({
            "session": [format!("ultron:session:{}", self.session)],
            "project": ["ultron:project:a07"],
            "global": ["ultron:global:a07"],
        })
        .as_object()
        .unwrap()
        .clone()
    }
    fn recall(&self, request: &Value, _: &Signal) -> Result<Value, String> {
        let tags = request["tags"].as_array().cloned().unwrap_or_default();
        let results: Vec<Value> = self
            .hindsight
            .documents
            .lock()
            .unwrap()
            .iter()
            .filter(|d| d.tags.len() == tags.len() && d.tags.iter().all(|tag| tags.contains(tag)))
            .map(|d| json!({ "id": d.id, "text": d.original_text, "tags": d.tags }))
            .collect();
        Ok(json!({ "results": results }))
    }
    fn retain(&self, request: &Value, _: &Signal) -> Result<Value, String> {
        let item = &request["items"][0];
        let id = item["document_id"].as_str().unwrap().to_string();
        let mut documents = self.hindsight.documents.lock().unwrap();
        documents.retain(|d| d.id != id);
        documents.push(Document {
            id,
            original_text: item["content"].as_str().unwrap().into(),
            tags: item["tags"].as_array().cloned().unwrap_or_default(),
        });
        let operation = request["operation_id"].as_str().unwrap().to_string();
        self.hindsight
            .operations
            .lock()
            .unwrap()
            .push((operation.clone(), "completed".into()));
        Ok(json!({ "success": true, "async": true, "operation_id": operation }))
    }
    fn operation(&self, id: &str, _: &Signal) -> Result<Value, String> {
        let status = self
            .hindsight
            .operations
            .lock()
            .unwrap()
            .iter()
            .find(|(op, _)| op == id)
            .map_or("not_found".to_string(), |(_, status)| status.clone());
        Ok(json!({ "operation_id": id, "status": status }))
    }
    fn get(&self, id: &str, _: &Signal) -> Result<Value, String> {
        Ok(self
            .hindsight
            .documents
            .lock()
            .unwrap()
            .iter()
            .find(|d| d.id == id)
            .map_or(
                Value::Null,
                |d| json!({ "id": d.id, "original_text": d.original_text, "tags": d.tags }),
            ))
    }
    fn delete(&self, id: &str, _: &Signal) -> Result<Value, String> {
        self.hindsight.documents.lock().unwrap().retain(|d| d.id != id);
        Ok(json!({ "success": true, "document_id": id }))
    }
}

fn step() -> Value {
    json!({
        "id": "count-step", "version": "1", "strategy": "deterministic", "instructions": "Record one external step.",
        "inputSchema": {}, "outputSchema": {}, "maxRepairs": 0, "inputDescription": "Any JSON value",
        "outputDescription": "Any JSON value",
    })
}

struct Env {
    root: tempfile::TempDir,
    repo: SessionRepo,
    hindsight: Hindsight,
    effects: Arc<Mutex<Vec<Value>>>,
    runs: Arc<AtomicUsize>,
}

/// One resumed session with the stack the session worker builds over its values.
struct Resumed {
    host: Host,
}

impl Env {
    fn new() -> Env {
        let root = tempfile::Builder::new().prefix("ultron-a07-").tempdir().unwrap();
        let repo = SessionRepo::new(&root.path().join("sessions"), SystemClock::shared());
        Env {
            root,
            repo,
            hindsight: Hindsight::default(),
            effects: Arc::default(),
            runs: Arc::default(),
        }
    }

    fn cwd(&self) -> String {
        self.root.path().display().to_string()
    }

    fn metadata(&self, id: &str) -> Metadata {
        let found: Vec<Metadata> = self.repo.list(None).into_iter().filter(|m| m.id == id).collect();
        assert_eq!(found.len(), 1, "expected one session {id}");
        found.into_iter().next().unwrap()
    }

    fn resume(&self, id: &str) -> Resumed {
        let values = SessionValues::new(self.repo.open(&self.metadata(id)).unwrap());
        let local = Arc::new(LocalServices::worker(Box::new(values.keyed("ultron.local"))));
        let memory = MemoryService::new(
            Arc::new(values.document("ultron.memory.state", "root")),
            Arc::new(Backend {
                hindsight: self.hindsight.clone(),
                session: id.into(),
            }),
            worker_gate(),
        )
        .unwrap();
        let effects = self.effects.clone();
        let runs = self.runs.clone();
        let host = build(Setup {
            tasks: Box::new(values.document("ultron.tasks", "root")),
            definitions: Box::new(values.document("ultron.definitions", "root")),
            usage: Some(
                UsageLedger::new(Box::new(values.document("ultron.usage", "root")), Default::default()).unwrap(),
            ),
            // Model lanes echo their prompt's run number; the prompt itself is recorded.
            script: Some(Arc::new(move |_, _, _| {
                Ok(format!("ran {}", runs.fetch_add(1, Ordering::SeqCst) + 1))
            })),
            deterministic: Some(Arc::new(move |request: delegation::agents::AdapterRequest<'_>| {
                effects.lock().unwrap().push(request.input.clone());
                Ok(request.input.clone())
            })),
            refinements: Some(instruction_hook(local.clone())),
            modules: vec![
                Arc::new(ProgressModule::new(Arc::new(
                    values.document("ultron.module", "progress"),
                ))),
                Arc::new(LocalServicesModule { services: local }),
                Arc::new(MemoryApi { service: Some(memory) }),
            ],
            ..Setup::default()
        });
        Resumed { host }
    }

    fn create(&self, id: &str) -> Resumed {
        drop(self.repo.create(&self.cwd(), Some(id), None).unwrap());
        self.resume(id)
    }

    fn fork(&self, source: &str, id: &str) {
        drop(
            self.repo
                .fork(&self.metadata(source), &ForkOptions::Tree, Some(id))
                .unwrap(),
        );
    }
}

impl Resumed {
    fn call(&self, kind: &str, payload: Value) -> Result<Value, String> {
        self.host.call(kind, payload)
    }

    fn run_child(&self, prompt: &str) -> (String, String) {
        let task = self
            .call(
                "agents.spawn",
                json!({ "definition": "rlm-child@1", "input": { "prompt": prompt } }),
            )
            .unwrap();
        let result = self.call("agents.result", json!({ "id": task["id"] })).unwrap();
        assert_eq!(result["status"], "succeeded", "{result}");
        (task["id"].as_str().unwrap().into(), self.host.last_prompt())
    }

    fn learn(&self, lesson: &str) -> String {
        let current = self
            .call(
                "refinements.current",
                json!({ "kind": "instruction", "target": "instruction:rlm-child" }),
            )
            .unwrap();
        let proposal = self
            .call(
                "refinements.propose",
                json!({
                    "kind": "instruction", "target": "instruction:rlm-child",
                    "baseVersion": current["version"].as_u64().unwrap_or(0), "content": lesson,
                    "evidence": [{ "ref": format!("lesson:{lesson}") }],
                }),
            )
            .unwrap();
        self.call("refinements.activate", json!({ "id": proposal["id"] }))
            .unwrap();
        proposal["id"].as_str().unwrap().into()
    }

    fn snapshot(&self) -> Value {
        let status = self.call("agents.status", json!({})).unwrap();
        json!({
            "tasks": status["tasks"],
            "usage": status["usage"]["usage"],
            "refinements": self.call("refinements.list", json!({})).unwrap(),
            "memory": self.call("memory.list", json!({})).unwrap(),
        })
    }

    fn close(self) {
        self.host.close();
    }
}

fn texts(recall: &Value) -> Vec<String> {
    recall["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["text"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn acceptance_a07_resume_after_restart_restores_tasks_definitions_lessons_progress_memory_and_usage_from_real_session_values()
 {
    let env = Env::new();
    let first = env.create("s-main");
    first.call("agents.register", json!({ "definition": step() })).unwrap();
    let done = first
        .call(
            "agents.invoke",
            json!({ "definition": "count-step@1", "input": { "n": 1 }, "key": "step-1" }),
        )
        .unwrap();
    let lesson_id = first.learn("Always check empty inputs.");
    let (child_id, child_prompt) = first.run_child("analyze");
    assert!(child_prompt.contains("Always check empty inputs."));
    first
        .call(
            "progress.report",
            json!({ "task_id": child_id, "summary": "analysis written",
                    "evidence": [{ "kind": "file", "ref": "analysis.md" }] }),
        )
        .unwrap();
    let remembered = first
        .call(
            "memory.propose",
            json!({ "text": "The launcher lives in bin/pi", "evidence": [{ "ref": format!("task:{child_id}") }] }),
        )
        .unwrap();
    let before = first.snapshot();
    first.close();

    let resumed = env.resume("s-main");
    assert_eq!(resumed.snapshot(), before);
    // The durable task answers the same key with no re-execution; the registered definition was restored.
    assert_eq!(
        resumed
            .call(
                "agents.invoke",
                json!({ "definition": "count-step@1", "input": { "n": 1 }, "key": "step-1" })
            )
            .unwrap(),
        done
    );
    assert_eq!(*env.effects.lock().unwrap(), [json!({ "n": 1 })]);
    let history = resumed
        .call("progress.history", json!({ "task_id": child_id }))
        .unwrap();
    assert_eq!(history["receipts"].as_array().unwrap().len(), 1);
    assert_eq!(history["receipts"][0]["summary"], "analysis written");
    let current = resumed
        .call(
            "refinements.current",
            json!({ "kind": "instruction", "target": "instruction:rlm-child" }),
        )
        .unwrap();
    assert_eq!(current["id"], lesson_id.as_str());
    assert_eq!(current["content"], "Always check empty inputs.");
    assert!(resumed.run_child("again").1.contains("Always check empty inputs."));
    let recalled = resumed
        .call(
            "memory.prepare",
            json!({ "query": "launcher", "taskId": "after-resume" }),
        )
        .unwrap();
    let results = recalled["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["id"], remembered["memoryId"]);
    assert_eq!(results[0]["text"], "The launcher lives in bin/pi");
    resumed.close();
}

#[test]
fn acceptance_a07_a_fork_starts_from_the_source_state_then_lessons_written_on_either_side_never_leak_to_the_other_across_restarts()
 {
    let env = Env::new();
    let source = env.create("s-source");
    source.call("agents.register", json!({ "definition": step() })).unwrap();
    let invoke_shared = json!({ "definition": "count-step@1", "input": { "n": 1 }, "key": "shared" });
    let shared = source.call("agents.invoke", invoke_shared.clone()).unwrap();
    let shared_lesson = source.learn("Shared lesson before the fork.");
    let at_fork = source.snapshot();
    source.close();

    env.fork("s-source", "s-fork");
    let fork = env.resume("s-fork");
    // The fork carries the source state at the fork point, including its effect history.
    assert_eq!(fork.snapshot(), at_fork);
    assert_eq!(fork.call("agents.invoke", invoke_shared).unwrap(), shared);
    assert_eq!(*env.effects.lock().unwrap(), [json!({ "n": 1 })]);

    // Work and lessons on the fork branch, which will later be abandoned.
    let fork_lesson = fork.learn("Fork-only lesson: skip validation.");
    let (fork_task, fork_prompt) = fork.run_child("fork work");
    assert!(fork_prompt.contains("Fork-only lesson"));
    fork.call(
        "progress.report",
        json!({ "task_id": fork_task, "summary": "fork progress", "evidence": [{ "kind": "file", "ref": "fork.md" }] }),
    )
    .unwrap();
    fork.call(
        "memory.propose",
        json!({ "text": "Fork-only memory", "evidence": [{ "ref": "fork" }] }),
    )
    .unwrap();
    fork.close();

    // Back on the source branch after a restart: nothing from the fork is visible or applied.
    let resumed_source = env.resume("s-source");
    let source_state = resumed_source.snapshot();
    assert_eq!(source_state, at_fork);
    assert!(!source_state.to_string().contains(&fork_lesson));
    assert!(!source_state.to_string().contains(&fork_task));
    let (source_run, source_prompt) = resumed_source.run_child("source work");
    assert!(source_prompt.contains("Shared lesson before the fork."));
    assert!(!source_prompt.contains("Fork-only lesson"));
    rejects(
        resumed_source.call("progress.history", json!({ "task_id": fork_task })),
        "Unknown Ultron task",
    );
    let source_recall = resumed_source
        .call(
            "memory.prepare",
            json!({ "query": "memory", "taskId": "source-recall" }),
        )
        .unwrap();
    assert!(!texts(&source_recall).contains(&"Fork-only memory".to_string()));
    // A lesson learned on the source after the fork does not flow into the fork either.
    let source_lesson = resumed_source.learn("Source-only lesson after the fork.");
    resumed_source.close();

    let resumed_fork = env.resume("s-fork");
    let fork_state = resumed_fork.snapshot();
    assert!(!fork_state.to_string().contains(&source_lesson));
    assert!(!fork_state.to_string().contains(&source_run));
    let (_, fork_again) = resumed_fork.run_child("fork again");
    assert!(fork_again.contains("Fork-only lesson"));
    assert!(!fork_again.contains("Source-only lesson"));
    let fork_recall = resumed_fork
        .call("memory.prepare", json!({ "query": "memory", "taskId": "fork-recall" }))
        .unwrap();
    assert!(texts(&fork_recall).contains(&"Fork-only memory".to_string()));
    resumed_fork.close();

    // Abandon the fork entirely: the source keeps exactly its own state.
    env.repo.delete(&env.metadata("s-fork")).unwrap();
    let final_source = env.resume("s-source");
    let ids: Vec<String> = final_source
        .call("refinements.list", json!({}))
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids, [shared_lesson, source_lesson.clone()]);
    assert_eq!(
        final_source
            .call(
                "refinements.current",
                json!({ "kind": "instruction", "target": "instruction:rlm-child" })
            )
            .unwrap()["id"],
        source_lesson.as_str()
    );
    final_source.close();
}

#[test]
fn acceptance_a07_rolling_back_a_lesson_on_one_branch_before_a_restart_does_not_resurrect_it_and_the_other_branch_keeps_its_own()
 {
    let env = Env::new();
    let source = env.create("s-rollback");
    let base = source.learn("Base lesson.");
    source.close();
    env.fork("s-rollback", "s-rollback-fork");

    let fork = env.resume("s-rollback-fork");
    let bad = fork.learn("Harmful lesson.");
    fork.call("refinements.rollback", json!({ "id": bad })).unwrap();
    fork.close();

    let resumed_fork = env.resume("s-rollback-fork");
    let (_, fork_prompt) = resumed_fork.run_child("after rollback");
    assert!(!fork_prompt.contains("Harmful lesson."));
    assert!(fork_prompt.contains("Base lesson."));
    resumed_fork.close();

    let resumed_source = env.resume("s-rollback");
    assert_eq!(
        resumed_source
            .call(
                "refinements.current",
                json!({ "kind": "instruction", "target": "instruction:rlm-child" })
            )
            .unwrap()["id"],
        base.as_str()
    );
    assert!(
        !resumed_source
            .call("refinements.list", json!({}))
            .unwrap()
            .to_string()
            .contains(&bad)
    );
    resumed_source.close();
}

/// A conversation on the session's `main` branch.
struct Lane {
    values: SessionValues,
}

impl Lane {
    fn tip(&self) -> Option<String> {
        self.values.session().lock().unwrap().branch_tip("main").flatten()
    }

    /// A completed prompt: the user's message and the assistant's reply, appended at the tip.
    fn prompt(&self, text: &str) {
        let mut session = self.values.session().lock().unwrap();
        session
            .append_message(
                "main",
                json!({ "role": "user", "content": [{ "type": "text", "text": text }], "timestamp": 0 }),
            )
            .unwrap();
        session
            .append_message(
                "main",
                json!({ "role": "assistant", "content": [{ "type": "text", "text": "ok" }], "stopReason": "stop",
                        "timestamp": 0 }),
            )
            .unwrap();
    }

    /// `navigateTree(entry)`: the branch continues from `entry`.
    fn navigate(&self, entry: &str) {
        self.values.set(BRANCH_TIP, "main", &json!(entry)).unwrap();
    }

    /// The session worker's branch: the tip, and the entries on its path.
    fn branch(&self) -> Branch {
        let on_path: HashSet<String> = self
            .values
            .session()
            .lock()
            .unwrap()
            .branch_entries("main")
            .unwrap()
            .iter()
            .map(|entry| entry["id"].as_str().unwrap().to_string())
            .collect();
        Branch {
            anchor: self.tip(),
            on_branch: Some(Arc::new(move |anchor: &str| on_path.contains(anchor))),
        }
    }
}

#[test]
fn acceptance_a07_a_lesson_from_an_abandoned_branch_never_applies_after_navigating_away_and_each_branch_keeps_its_own()
{
    let env = Env::new();
    let session = env.repo.create(&env.cwd(), Some("a07"), None).unwrap();
    let values = SessionValues::new(session);
    values
        .session()
        .lock()
        .unwrap()
        .commit(&[Write::set(BRANCH_TIP, "main", Value::Null)])
        .unwrap();
    let lane = Lane { values: values.clone() };
    let services = LocalServices::worker(Box::new(values.keyed("ultron.local")));
    let call = |kind: &str, payload: Value, on: &LocalServices| on.handle(kind, &payload, Some(&lane.branch()));
    let current = |on: &LocalServices| -> Option<String> {
        call(
            "refinements.current",
            json!({ "kind": "instruction", "target": "instruction:correctness-reviewer" }),
            on,
        )
        .unwrap()["content"]
            .as_str()
            .map(str::to_string)
    };
    let lesson = |content: &str, base_version: u64| {
        let proposed = call(
            "refinements.propose",
            json!({ "kind": "instruction", "target": "instruction:correctness-reviewer", "baseVersion": base_version,
                    "content": content, "evidence": [{ "run": content }] }),
            &services,
        )
        .unwrap();
        call("refinements.activate", json!({ "id": proposed["id"] }), &services).unwrap();
    };

    lane.prompt("first question");
    let fork_point = lane.tip().unwrap();
    lane.prompt("second question");
    lesson("Lesson learned on branch A.", 0);
    assert_eq!(current(&services).as_deref(), Some("Lesson learned on branch A."));
    let branch_a_tip = lane.tip().unwrap();

    // Navigate back to before the lesson: branch B must not see it.
    lane.navigate(&fork_point);
    assert_eq!(current(&services), None);
    lane.prompt("a different second question");
    assert_eq!(current(&services), None);
    lesson("Lesson learned on branch B.", 0);
    assert_eq!(current(&services).as_deref(), Some("Lesson learned on branch B."));

    // Returning to branch A restores its own lesson, untouched by B, including after a restart.
    lane.navigate(&branch_a_tip);
    assert_eq!(current(&services).as_deref(), Some("Lesson learned on branch A."));
    let restarted = LocalServices::worker(Box::new(values.keyed("ultron.local")));
    assert_eq!(current(&restarted).as_deref(), Some("Lesson learned on branch A."));

    // A rollback on branch A holds on A and does not touch B.
    let listed = call("refinements.list", json!({}), &services).unwrap();
    let lesson_a = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["content"] == "Lesson learned on branch A.")
        .unwrap()["id"]
        .clone();
    call("refinements.rollback", json!({ "id": lesson_a }), &services).unwrap();
    assert_eq!(current(&services), None);
    lane.navigate(&fork_point);
    // The fork point is before both lessons, so nothing applies there either.
    assert_eq!(current(&services), None);
}
