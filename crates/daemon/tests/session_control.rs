//! `ultron.session-control`'s settings members, `AgentController.resume` and `Models.refresh` of the session worker
//! (session-control-provider.ts, agent-controller-provider.ts, models-provider.ts) over a recording session core, with
//! literal expectations from the TypeScript.

use daemon::chord::{Member, Service, State};
use daemon::session_control::{self, ControlContext};
use serde_json::{Value, json};
use session_core::core::{Op, OpResult, QueueKind};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Instant;

struct Fixture {
    service: Service,
    ops: Arc<Mutex<Vec<Op>>>,
    state: Arc<Mutex<Value>>,
    _tmp: tempfile::TempDir,
    cwd: std::path::PathBuf,
    agent: std::path::PathBuf,
}

fn fixture_with(pool: OpResult) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let (cwd, agent) = (root.join("project"), root.join("agent"));
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&agent).unwrap();
    let ops: Arc<Mutex<Vec<Op>>> = Arc::default();
    let state = Arc::new(Mutex::new(json!({
        "model": {"provider": "anthropic", "id": "claude-sonnet-4-5", "name": "Claude Sonnet 4.5"},
        "thinkingLevel": "high",
        "steeringMode": "all",
        "followUpMode": "one-at-a-time",
        "autoCompactionEnabled": false,
        "sessionName": "named",
    })));
    let (recorded, current) = (ops.clone(), state.clone());
    let core = Arc::new(move |op: Op| -> OpResult {
        recorded.lock().unwrap().push(op.clone());
        match op {
            Op::State => Ok(Some(current.lock().unwrap().clone())),
            Op::Inspect { .. } => pool.clone(),
            _ => Ok(None),
        }
    });
    let vars = vec![
        ("ULTRON_CODING_AGENT_DIR".to_string(), "/a".to_string()),
        ("ANTHROPIC_API_KEY".to_string(), "sk".to_string()),
        ("ULTRON_API_KEY".to_string(), "sk".to_string()),
        ("PI_OFFLINE".to_string(), "1".to_string()),
    ];
    let context = Arc::new(ControlContext {
        cwd: cwd.clone(),
        agent_dir: agent.clone(),
        session_file: Some("/s/session.jsonl".into()),
        project_trusted: false,
        var: Arc::new(|_| None),
        vars: Arc::new(move || vars.clone()),
        available_models: Arc::new(|| 7),
        started: Instant::now(),
        auto_retry: AtomicBool::new(true),
    });
    let service = session_control::members(Service::new("ultron.session-control"), core, context);
    Fixture {
        service,
        ops,
        state,
        _tmp: tmp,
        cwd,
        agent,
    }
}

fn fixture() -> Fixture {
    fixture_with(Ok(Some(json!({"live": 1, "pinned": 0}))))
}

impl Fixture {
    fn call(&self, member: &str, args: Value) -> Result<Option<Value>, String> {
        let Some(Member::Method(method)) = self.service.members.get(member) else {
            panic!("no method {member}");
        };
        method(args.as_array().unwrap().clone()).map_err(|e| e.message)
    }

    fn ops(&self) -> Vec<Op> {
        std::mem::take(&mut *self.ops.lock().unwrap())
    }

    fn settings_file(&self) -> Value {
        serde_json::from_str(&std::fs::read_to_string(self.agent.join("settings.json")).unwrap()).unwrap()
    }
}

#[test]
fn daemon_session_control_get_settings_reads_the_session() {
    let f = fixture();
    assert_eq!(
        f.call("getSettings", json!([])).unwrap(),
        Some(
            json!({"name": "named", "steeringMode": "all", "followUpMode": "one-at-a-time",
            "autoCompaction": false, "autoRetry": true})
        )
    );
    // An unnamed session: `name ?? null`.
    f.state.lock().unwrap().as_object_mut().unwrap().remove("sessionName");
    f.call("setAutoRetry", json!([false])).unwrap();
    assert_eq!(f.ops().last(), Some(&Op::SetAutoRetry(false)));
    let settings = f.call("getSettings", json!([])).unwrap().unwrap();
    assert_eq!(settings["name"], Value::Null);
    assert_eq!(settings["autoRetry"], false);
}

#[test]
fn daemon_session_control_setters_become_session_operations() {
    let f = fixture();
    f.call("setSteeringMode", json!(["one-at-a-time"])).unwrap();
    f.call("setFollowUpMode", json!(["all"])).unwrap();
    f.call("setAutoCompaction", json!([true])).unwrap();
    f.call("setLabel", json!(["e1", "checkpoint"])).unwrap();
    // `label === null || label === ""` clears it.
    f.call("setLabel", json!(["e1", ""])).unwrap();
    f.call("setLabel", json!(["e2", null])).unwrap();
    assert_eq!(
        f.ops(),
        vec![
            Op::SetQueueMode {
                kind: QueueKind::Steer,
                mode: "one-at-a-time".into()
            },
            Op::SetQueueMode {
                kind: QueueKind::FollowUp,
                mode: "all".into()
            },
            Op::SetAutoCompaction(true),
            Op::SetLabel {
                entry_id: "e1".into(),
                label: Some("checkpoint".into())
            },
            Op::SetLabel {
                entry_id: "e1".into(),
                label: None
            },
            Op::SetLabel {
                entry_id: "e2".into(),
                label: None
            },
        ]
    );
    // No extension cancels a fork.
    assert_eq!(
        f.call("beforeFork", json!(["e1", "before"])).unwrap(),
        Some(json!({"cancelled": false}))
    );
}

#[test]
fn daemon_session_control_set_setting_persists_and_applies_live() {
    let f = fixture();
    assert_eq!(
        f.call("setSetting", json!(["compaction.enabled", false])).unwrap(),
        Some(json!({"applied": "live"}))
    );
    assert_eq!(f.ops(), vec![Op::State, Op::SetAutoCompaction(false)]);
    assert_eq!(f.settings_file(), json!({"compaction": {"enabled": false}}));
    // An override for the session's model applies to it now; another model's is only saved.
    let current = json!({"provider": "anthropic", "modelId": "claude-sonnet-4-5", "level": "low"});
    assert_eq!(
        f.call("setSetting", json!(["modelThinkingLevel", current])).unwrap(),
        Some(json!({"applied": "live"}))
    );
    assert_eq!(f.ops(), vec![Op::State, Op::SetThinking { level: "low".into() }]);
    let other = json!({"provider": "openai", "modelId": "gpt-5", "level": "low"});
    assert_eq!(
        f.call("setSetting", json!(["modelThinkingLevel", other])).unwrap(),
        Some(json!({"applied": "saved"}))
    );
    assert_eq!(f.ops(), vec![Op::State]);
    f.call("setSetting", json!(["retry.enabled", false])).unwrap();
    assert_eq!(f.ops(), vec![Op::State, Op::SetAutoRetry(false)]);
    assert_eq!(f.call("getSettings", json!([])).unwrap().unwrap()["autoRetry"], false);
    f.call("setSetting", json!(["steeringMode", "one-at-a-time"])).unwrap();
    assert_eq!(
        f.ops().last(),
        Some(&Op::SetQueueMode {
            kind: QueueKind::Steer,
            mode: "one-at-a-time".into()
        })
    );
    // The Rust session reads its transport and idle timeout when it opens: they apply on restart.
    assert_eq!(
        f.call("setSetting", json!(["transport", "sse"])).unwrap(),
        Some(json!({"applied": "restart"}))
    );
    assert_eq!(
        f.call("setSetting", json!(["httpIdleTimeoutMs", 0])).unwrap(),
        Some(json!({"applied": "restart"}))
    );
    assert_eq!(f.ops(), vec![Op::State, Op::State]);
    assert_eq!(
        f.call("setSetting", json!(["nope", 1])).unwrap_err(),
        "Unknown setting: nope"
    );
    let file = f.settings_file();
    assert_eq!(file["transport"], "sse");
    assert_eq!(file["httpIdleTimeoutMs"], 0);
    assert_eq!(
        file["modelThinkingLevels"],
        json!({"anthropic/claude-sonnet-4-5": "low", "openai/gpt-5": "low"})
    );
}

#[test]
fn daemon_session_control_trust_round_trips_through_read_settings() {
    let f = fixture();
    let read = f.call("readSettings", json!([])).unwrap().unwrap();
    assert_eq!(read["savedTrust"], Value::Null);
    assert_eq!(read["projectTrusted"], false);
    assert_eq!(read["cwd"], json!(f.cwd.display().to_string()));
    assert_eq!(read["values"]["steeringMode"], "all");
    assert_eq!(read["values"]["compaction.enabled"], false);
    // Trust the parent folder and drop the project's own decision (`Trust parent folder (...)`).
    let parent = f.cwd.parent().unwrap().display().to_string();
    f.call(
        "setProjectTrust",
        json!([[{"path": parent, "decision": true}, {"path": f.cwd.display().to_string(), "decision": null}]]),
    )
    .unwrap();
    let read = f.call("readSettings", json!([])).unwrap().unwrap();
    assert_eq!(read["savedTrust"], json!({"path": parent, "decision": true}));
    let trust: Value = serde_json::from_str(&std::fs::read_to_string(f.agent.join("trust.json")).unwrap()).unwrap();
    assert_eq!(trust, json!({ parent.clone(): true }));
    // A later "Do not trust" for the project wins below the parent.
    f.call(
        "setProjectTrust",
        json!([[{"path": f.cwd.display().to_string(), "decision": false}]]),
    )
    .unwrap();
    let read = f.call("readSettings", json!([])).unwrap().unwrap();
    assert_eq!(
        read["savedTrust"],
        json!({"path": f.cwd.display().to_string(), "decision": false})
    );
    assert!(Path::new(&f.agent.join("trust.json")).exists());
}

#[test]
fn daemon_session_control_reload_auth_counts_available_models() {
    let f = fixture();
    assert_eq!(
        f.call("reloadAuth", json!(["anthropic"])).unwrap(),
        Some(json!({"availableModels": 7}))
    );
    assert_eq!(
        f.call("reloadAuth", json!([null])).unwrap(),
        Some(json!({"availableModels": 7}))
    );
}

#[test]
fn daemon_session_control_debug_info_has_the_typescript_fields() {
    let f = fixture();
    let info = f.call("debugInfo", json!([])).unwrap().unwrap();
    let keys: Vec<&String> = info.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        [
            "pid",
            "parentPid",
            "nodeVersion",
            "version",
            "platform",
            "cwd",
            "agentDir",
            "sessionFile",
            "uptimeMs",
            "rssBytes",
            "model",
            "thinkingLevel",
            "kernelPool",
            "environment"
        ]
    );
    assert_eq!(info["pid"], std::process::id());
    assert_eq!(info["model"], "anthropic/claude-sonnet-4-5");
    assert_eq!(info["thinkingLevel"], "high");
    assert_eq!(info["kernelPool"], json!({"live": 1, "pinned": 0}));
    assert_eq!(info["sessionFile"], "/s/session.jsonl");
    assert!(info["rssBytes"].as_u64().unwrap() > 0);
    assert_eq!(info["platform"], session_control::platform());
    // Only `ULTRON_*`/`PI_*`, secret-looking names redacted.
    assert_eq!(
        info["environment"],
        json!({"PI_OFFLINE": "1", "ULTRON_API_KEY": "<redacted>", "ULTRON_CODING_AGENT_DIR": "/a"})
    );
    assert!(f.ops().contains(&Op::Inspect {
        request: "rlm.pool".into(),
        payload: json!({})
    }));
    // The pool's error is reported in its place.
    let failing = fixture_with(Err("no kernel".into()));
    let info = failing.call("debugInfo", json!([])).unwrap().unwrap();
    assert_eq!(info["kernelPool"], json!({"error": "no kernel"}));
}

#[test]
fn daemon_session_control_resume_maps_recovery_to_agent_responses() {
    let resumed = json!({"resumed": true, "runId": "run-1", "reason": "interrupted run"});
    assert_eq!(
        session_control::resume_response(&resumed, None),
        json!({"accepted": true, "operationId": "run-1", "error": null})
    );
    // A run in progress is the active operation `resume` drives.
    let busy = json!({"resumed": false, "runId": null, "reason": "a run is in progress"});
    assert_eq!(
        session_control::resume_response(&busy, Some("run-2".into())),
        json!({"accepted": true, "operationId": "run-2", "error": null})
    );
    let nothing = json!({"resumed": false, "reason": "nothing to recover"});
    assert_eq!(
        session_control::resume_response(&nothing, None),
        json!({"accepted": false, "operationId": null, "error": {"code": "nothing_to_resume",
            "message": "Lane \"main\" has no active operation to resume"}})
    );
    assert_eq!(
        session_control::resume_response(&busy, None)["accepted"],
        false,
        "no run to point at"
    );
}

#[test]
fn daemon_session_control_models_refresh_publishes_like_models_provider() {
    let models = State::with_sequence(
        json!({"catalog": {"revision": 1, "availableModels": []}, "configuration": {"model": null, "thinkingLevel": "off"},
            "refresh": {"status": "idle"}}),
        1,
    );
    let session =
        json!({"model": {"provider": "local", "id": "m", "name": "M", "reasoning": false}, "thinkingLevel": "low"});
    let available = || vec![json!({"provider": "anthropic", "id": "a", "name": "A", "reasoning": true})];
    let seen = Mutex::new(None);
    session_control::refresh_models(
        &models,
        || {
            *seen.lock().unwrap() = Some(models.value()["refresh"].clone());
            Ok(Vec::new())
        },
        available,
        &session,
    )
    .unwrap();
    assert_eq!(seen.lock().unwrap().take(), Some(json!({"status": "refreshing"})));
    let state = models.value();
    assert_eq!(state["refresh"], json!({"status": "done"}));
    assert_eq!(state["catalog"]["revision"], 2);
    // The selected model joins the catalogue when no provider lists it.
    assert_eq!(
        state["catalog"]["availableModels"],
        json!([
            {"provider": "anthropic", "modelId": "a", "name": "A", "reasoning": true,
                "model": {"provider": "anthropic", "id": "a", "name": "A", "reasoning": true}},
            {"provider": "local", "modelId": "m", "name": "M", "reasoning": false,
                "model": {"provider": "local", "id": "m", "name": "M", "reasoning": false}},
        ])
    );
    assert_eq!(
        state["configuration"],
        json!({"model": {"provider": "local", "modelId": "m"}, "thinkingLevel": "low"})
    );
    session_control::refresh_models(
        &models,
        || Ok(vec![("openai".into(), "HTTP 500".into())]),
        available,
        &session,
    )
    .unwrap();
    assert_eq!(
        models.value()["refresh"],
        json!({"status": "warning", "errors": {"openai": "HTTP 500"}})
    );
    assert_eq!(models.value()["catalog"]["revision"], 3);
    // A failed refresh fails the call and leaves the status refreshing (the TypeScript `await` throws).
    let failed = session_control::refresh_models(&models, || Err("offline".into()), available, &session);
    assert_eq!(failed.unwrap_err().message, "offline");
    assert_eq!(models.value()["refresh"], json!({"status": "refreshing"}));
}
