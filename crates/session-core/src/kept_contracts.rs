//! Contracts for persistent children (wave 41): `rlm.spawn(brief, name=..., keep=True, role=, contract=, hop=)`.
//! A kept child answers more than once (its brief, then each `rlm.ask`), so each answer is held to its contract as a
//! one-shot child's end is (`delegation::spawn_contract::enforce`): `returns` against its latest `rlm.finish`
//! verdict, the host's checks in the session's working directory under the session's sandbox, and the hop (a
//! `{"retry": N}` asks the child again with its failures, a `{"review": brief}` spawns a reviewer subagent).
//!
//! - The first answer is checked when `rlm.collect` / `rlm.results` first report it; that checked result is kept and
//!   reported from then on. Each `rlm.ask` answer is checked when it arrives.
//! - Workspace checks compare against the child's start: its metadata snapshot in this process, else the Git
//!   baseline kept with the contract (`delegation::git_baseline`), so they work after a restart too; with neither
//!   they fail closed. Changes are cumulative since the spawn.
//! - A kept child records its verdict with `rlm.finish` as a one-shot child does; each question starts it afresh.
//! - The contract, baseline, latest verdict and checked first answer are the session value
//!   `ultron.kept-contracts` / `<child id>` (Rust-only), so they survive a restart with the child.

use crate::compose::Stack;
use crate::durable_store::lock as lock_session;
use crate::subagents::DurableChildren;
use delegation::git_baseline::GitBaseline;
use delegation::spawn_contract::{CheckPlace, Enforcer, SPAWN_KEYS, SpawnContract, enforce};
use delegation::workspace::{DEFAULT_MAX_SNAPSHOT_ENTRIES, WorkspaceSnapshot, diff_snapshots, snapshot_workspace};
use kernel::Signal;
use serde_json::{Map, Value, json};
use session_journal::Write;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError, Weak};

/// Where the session keeps each kept child's contract.
pub const KEPT_CONTRACTS_NAMESPACE: &str = "ultron.kept-contracts";

/// Serves a request for the calling lane: `(kind, payload)` -> its answer (the durable runtime's host operations).
pub type Serve<'a> = &'a dyn Fn(&str, &Map<String, Value>) -> Result<Value, String>;

/// The kept children's contracts of one session.
pub struct KeptContracts {
    children: Arc<DurableChildren>,
    stack: Weak<Stack>,
    /// The metadata snapshot of each kept child's start taken in this process.
    before: Mutex<HashMap<String, WorkspaceSnapshot>>,
    /// One answer is checked at a time (checks run commands in the shared working directory).
    checking: Mutex<()>,
}

fn guard<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl KeptContracts {
    pub fn new(children: Arc<DurableChildren>, stack: Weak<Stack>) -> KeptContracts {
        KeptContracts {
            children,
            stack,
            before: Mutex::new(HashMap::new()),
            checking: Mutex::new(()),
        }
    }

    fn kept(&self, id: &str) -> Option<Value> {
        let session = self.children.session()?;
        lock_session(&session).value(KEPT_CONTRACTS_NAMESPACE, id).cloned()
    }

    fn keep(&self, id: &str, value: Value) -> Result<(), String> {
        let session = self.children.session().ok_or("The session is not open")?;
        lock_session(&session)
            .commit(&[Write::set(KEPT_CONTRACTS_NAMESPACE, id, value)])
            .map(drop)
    }

    /// A persistent spawn: one naming a contract is checked, its brief led by the role and checks, and its contract
    /// kept (with the workspace's start) once the child exists. `None`: no contract, served as before.
    pub fn spawn(&self, payload: &Map<String, Value>, serve: Serve<'_>) -> Option<Result<Value, String>> {
        let kwargs = payload.get("kwargs").and_then(Value::as_object)?;
        let contract = match SpawnContract::parse(kwargs) {
            Ok(Some(contract)) => contract,
            Ok(None) => return None,
            Err(error) => return Some(Err(error)),
        };
        Some(self.spawn_with(payload, kwargs, &contract, serve))
    }

    fn spawn_with(
        &self,
        payload: &Map<String, Value>,
        kwargs: &Map<String, Value>,
        contract: &SpawnContract,
        serve: Serve<'_>,
    ) -> Result<Value, String> {
        let stack = self.stack.upgrade().ok_or("The session is closed")?;
        let cwd = stack.config().cwd.clone();
        let (baseline, before) = match contract.needs_baseline() {
            true => (
                GitBaseline::capture(&cwd, &stack.config().child_env()),
                Some(snapshot_workspace(&cwd, DEFAULT_MAX_SNAPSHOT_ENTRIES, true)),
            ),
            false => (None, None),
        };
        let mut payload = payload.clone();
        let brief = payload.get("prompt").and_then(Value::as_str).unwrap_or_default();
        payload.insert("prompt".into(), json!(contract.prompt(brief)));
        let mut options = kwargs.clone();
        for key in SPAWN_KEYS {
            options.remove(key);
        }
        payload.insert("kwargs".into(), Value::Object(options));
        let handle = serve("rlm.spawn", &payload)?;
        let id = handle["rlm_child_id"].as_str().unwrap_or_default().to_string();
        let mut kept = json!({ "contract": contract.describe(), "verdict": null, "first": null });
        if let Some(baseline) = &baseline {
            kept["baseline"] = baseline.describe();
        }
        self.keep(&id, kept)?;
        if let Some(before) = before {
            guard(&self.before).insert(id, before);
        }
        Ok(handle)
    }

    /// `rlm.finish` from a kept child's lane: its verdict, held to `returns`, recorded for its answer's check.
    /// `None` when `lane` is not a kept child with a contract.
    pub fn finish(&self, lane: &str, payload: &Map<String, Value>) -> Option<Result<Value, String>> {
        let id = self.children.kept_child_of_lane(lane)?;
        let mut kept = self.kept(&id)?;
        let contract = match SpawnContract::from_described(&kept["contract"]) {
            Ok(contract) => contract,
            Err(error) => return Some(Err(format!("rlm.finish: the kept contract is unreadable: {error}"))),
        };
        let cwd = self
            .stack
            .upgrade()
            .map(|stack| stack.config().cwd.display().to_string());
        let checked = delegation::verdict::validate_verdict(payload, cwd.as_deref()).and_then(|verdict| {
            let problems = contract.returns_problems(&verdict["outputs"]);
            if problems.is_empty() {
                Ok(verdict)
            } else {
                Err(problems)
            }
        });
        Some(match checked {
            Ok(verdict) => {
                let status = verdict["status"].clone();
                kept["verdict"] = verdict;
                self.keep(&id, kept).map(|()| {
                    json!({ "recorded": true, "status": status,
                        "next": "End your turn now with a short reply; the host checks your answer against your contract." })
                })
            }
            Err(problems) => {
                let list = problems.iter().map(|p| format!("- {p}")).collect::<Vec<_>>().join("\n");
                Err(format!(
                    "rlm.finish rejected:\n{list}\nFix these and call rlm.finish again."
                ))
            }
        })
    }

    /// The child's verdict starts afresh: a question (or a retry) is answered on a verdict of its own.
    fn clear_verdict(&self, id: &str) {
        if let Some(mut kept) = self.kept(id)
            && !kept["verdict"].is_null()
        {
            kept["verdict"] = Value::Null;
            let _ = self.keep(id, kept);
        }
    }

    /// Before `rlm.ask` reaches kept child `id`.
    pub fn before_ask(&self, id: &str) {
        if self.kept(id).is_some() {
            self.clear_verdict(id);
        }
    }

    /// `rlm.ask`'s answer from kept child `id` (`{"id", "status", "value" | "error"}`), held to its contract.
    pub fn ask_answer(&self, id: &str, answer: Value, lane: &str, signal: &Signal, serve: Serve<'_>) -> Value {
        let _checking = guard(&self.checking);
        let Some(kept) = self.kept(id) else {
            return answer;
        };
        let mut result = answer.clone();
        if let Some(fields) = result.as_object_mut() {
            fields.remove("id");
        }
        let checked = self.check(id, &kept, result, lane, signal, serve);
        let mut out = json!({ "id": answer["id"] });
        for (key, value) in checked.as_object().into_iter().flatten() {
            out[key] = value.clone();
        }
        out
    }

    /// `rlm.collect` / `rlm.results` answers (`{"results": [{"id", "result", ...}], ...}`): a kept child's first
    /// answer with a contract is checked once, then reported as checked.
    pub fn reported(&self, mut answer: Value, lane: &str, signal: &Signal, serve: Serve<'_>) -> Value {
        let Some(results) = answer["results"].as_array_mut() else {
            return answer;
        };
        for report in results.iter_mut() {
            let Some(id) = report["id"].as_str().map(String::from) else {
                continue;
            };
            let _checking = guard(&self.checking);
            let Some(mut kept) = self.kept(&id) else {
                continue;
            };
            if kept["first"].is_null() {
                let checked = self.check(&id, &kept, report["result"].clone(), lane, signal, serve);
                kept = self.kept(&id).unwrap_or(kept);
                kept["first"] = checked;
                let _ = self.keep(&id, kept.clone());
            }
            report["result"] = kept["first"].clone();
        }
        answer
    }

    /// Hold one answer (`result`) of kept child `id` to its contract.
    fn check(&self, id: &str, kept: &Value, result: Value, lane: &str, signal: &Signal, serve: Serve<'_>) -> Value {
        let contract = match SpawnContract::from_described(&kept["contract"]) {
            Ok(contract) => contract,
            Err(error) => {
                let mut result = result;
                result["status"] = json!("failed");
                result["error"] = json!(format!(
                    "Contract failed: the session's kept contract is unreadable: {error}"
                ));
                return result;
            }
        };
        let Some(stack) = self.stack.upgrade() else {
            return result;
        };
        let baseline = GitBaseline::from_value(&kept["baseline"]);
        let name = self
            .children
            .chat()
            .and_then(|chat| chat.get_task(durable::TaskId(id.parse().ok()?)).ok().flatten())
            .and_then(|task| task.input["name"].as_str().map(String::from))
            .unwrap_or_else(|| id.to_string());
        let runner = crate::contract_checks::check_runner(self.stack.clone());
        let mut check = || {
            let cwd = stack.config().cwd.clone();
            let verdict = self
                .kept(id)
                .map(|kept| kept["verdict"].clone())
                .filter(|v| !v.is_null());
            let before = guard(&self.before).get(id).cloned();
            let diff = match (&before, &baseline) {
                (Some(before), _) => {
                    diff_snapshots(before, &snapshot_workspace(&cwd, DEFAULT_MAX_SNAPSHOT_ENTRIES, true)).ok()
                }
                (None, Some(baseline)) => baseline.changes(&cwd, &stack.config().child_env()).ok(),
                (None, None) => None,
            };
            let place = CheckPlace {
                cwd: Some(&cwd),
                env: &[],
                diff: diff.as_ref(),
                signal,
            };
            let report = contract.evaluate(verdict.as_ref(), &place, &runner);
            (!signal.is_aborted()).then_some(report)
        };
        let mut retry = |note: &str| {
            self.clear_verdict(id);
            let mut ask = Map::new();
            ask.insert("child".into(), json!(id));
            ask.insert("question".into(), json!(note));
            let answer = serve("rlm.ask", &ask);
            if signal.is_aborted() {
                return None;
            }
            Some(Some(match answer {
                Ok(mut answer) => {
                    if let Some(fields) = answer.as_object_mut() {
                        fields.remove("id");
                    }
                    answer
                }
                Err(error) => json!({ "status": "failed", "error": error }),
            }))
        };
        let mut review = |name: &str, prompt: &str| {
            let payload = json!({ "prompt": prompt, "kwargs": { "name": name } });
            stack.delegation.handle("rlm.spawn", payload, lane)
        };
        let mut enforcer = Enforcer {
            check: &mut check,
            retry: &mut retry,
            review: &mut review,
        };
        let fallback = result.clone();
        let mut checked = enforce(&contract, result, id, &name, &mut enforcer).unwrap_or(fallback);
        // The child's own claim for this answer, beside the host's verdict (as a one-shot child's result has it).
        checked["verdict"] = self.kept(id).map_or(Value::Null, |kept| kept["verdict"].clone());
        checked
    }
}
