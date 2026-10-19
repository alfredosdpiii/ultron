//! The session's model list (`Op::AvailableModels`) is read when asked: a credential saved after the session opened
//! makes its provider's models available, and selectable even before the list is asked for again. Private temp
//! profile; no request reaches a provider.

mod common;

use serde_json::{Value, json};
use session_core::core::{Input, Op};
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::Duration;

const HANG: Duration = Duration::from_secs(120);

fn env(sandbox: &common::Sandbox) -> cli::Env {
    let vars: Vec<(String, String)> = vec![
        ("HOME".into(), sandbox.root.display().to_string()),
        (
            "ULTRON_CODING_AGENT_DIR".into(),
            sandbox.agent_dir.display().to_string(),
        ),
        (
            "ULTRON_SERVER_DIR".into(),
            sandbox.root.join("server").display().to_string(),
        ),
        ("ANTHROPIC_API_KEY".into(), "test-key".into()),
        ("ULTRON_HINDSIGHT_URL".into(), "off".into()),
        ("PI_OFFLINE".into(), "1".into()),
    ];
    cli::Env {
        cwd: sandbox.cwd.clone(),
        home: sandbox.root.clone(),
        var: Arc::new(move |key| vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())),
        docs: config::paths::normalize(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/docs")),
    }
}

struct Actor {
    tx: Sender<Input>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Actor {
    fn open(sandbox: &common::Sandbox, model: &str) -> Actor {
        let repo = session_journal::SessionRepo::new(&sandbox.sessions_dir(), session_journal::SystemClock::shared());
        let metadata = repo
            .create(&sandbox.cwd.display().to_string(), None, None)
            .unwrap()
            .metadata()
            .clone();
        let open = session_core::core::OpenOptions {
            env: env(sandbox),
            flags: session_core::ModelFlags {
                model: Some(model.into()),
                ..Default::default()
            },
            session_root: sandbox.sessions_dir(),
            metadata,
            mode: "rpc".into(),
            services: cli::compose::Services::default(),
            auto_name: false,
        };
        let (tx, handle, _) = session_core::core::spawn(open, Box::new(|_| {})).unwrap();
        Actor {
            tx,
            handle: Some(handle),
        }
    }

    fn call(&self, op: Op) -> Result<Option<Value>, String> {
        session_core::core::call(&self.tx, op)
    }

    fn close(mut self) {
        let (done, closed) = std::sync::mpsc::channel();
        self.tx.send(Input::Close(done)).unwrap();
        closed.recv_timeout(HANG).unwrap();
        self.handle.take().unwrap().join().unwrap();
    }
}

fn refs(models: &Value) -> Vec<String> {
    models
        .as_array()
        .unwrap()
        .iter()
        .map(|m| format!("{}/{}", m["provider"].as_str().unwrap(), m["id"].as_str().unwrap()))
        .collect()
}

#[test]
fn cli_session_available_models_follow_credentials_saved_later() {
    let sandbox = common::Sandbox::new(9);
    let actor = Actor::open(&sandbox, "anthropic/claude-sonnet-4-5");
    let has = |models: &Value, provider: &str| refs(models).iter().any(|r| r.starts_with(&format!("{provider}/")));
    assert!(!has(&actor.call(Op::AvailableModels).unwrap().unwrap(), "groq"));
    let auth = sandbox.agent_dir.join("auth.json");
    std::fs::write(
        &auth,
        json!({"groq": {"type": "api_key", "key": "groq-key"}}).to_string(),
    )
    .unwrap();
    assert!(has(&actor.call(Op::AvailableModels).unwrap().unwrap(), "groq"));
    std::fs::write(
        &auth,
        json!({"groq": {"type": "api_key", "key": "groq-key"}, "openai": {"type": "api_key", "key": "openai-key"}})
            .to_string(),
    )
    .unwrap();
    let selected = actor
        .call(Op::SetModel {
            provider: "openai".into(),
            model_id: "gpt-5".into(),
        })
        .unwrap()
        .unwrap();
    assert_eq!(
        (&selected["provider"], &selected["id"]),
        (&json!("openai"), &json!("gpt-5"))
    );
    actor.close();
}
