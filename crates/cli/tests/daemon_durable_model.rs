//! The durable instance's sessions on the Claude subscription (user requirement; literal expectations): a new
//! session of `ultron durable` or `ultron-s` with no `--model`/`--provider` starts on `durable.model` (default
//! `claude-code/claude-opus-5-5`, whose tool lane runs as `claude-subscription`) ahead of the global
//! `defaultProvider`/`defaultModel`; an explicit `--model` wins; a resumed session keeps its own model; a model chosen
//! in a durable session (`set_model`, `cycle_model`) is saved as `durable.model`, leaving the global keys to plain
//! `ultron`, which is unchanged. Everything runs under a private HOME, sealed (crates/ai/tests/claude_subscription_support
//! `seal()`), with a FAKE `claude` (crates/ai/fixtures/claude-subscription/claude.py, answering offline) and a scripted
//! Anthropic server on loopback standing in for the global default's provider.

#[path = "daemon_common/mod.rs"]
mod common;
mod executable_support;
#[path = "../../ai/tests/claude_subscription_support/isolate.rs"]
mod isolate;

use cli::rpc::client::{RpcClient, RpcClientOptions};
use common::{HANG_GUARD, Home, Provider, assert_none_mentioning, provider, text_reply};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// What the fake `claude` answers every subscription call with.
const SUBSCRIPTION_ANSWER: &str = "from the subscription";

/// The global default of these tests: a provider other than the subscription.
const GLOBAL: &str =
    r#"{"sessionNaming":{"enabled":false},"defaultProvider":"anthropic","defaultModel":"claude-sonnet-4-5"}"#;

struct Fixture {
    home: Home,
    /// The global default's provider: a durable session without flags never reaches it.
    anthropic: Provider,
    fake: PathBuf,
    log: PathBuf,
}

impl Fixture {
    fn new(replies: usize) -> Fixture {
        isolate::seal();
        let anthropic = provider(
            (0..replies)
                .map(|i| text_reply(&format!("from anthropic {i}")))
                .collect(),
            Duration::ZERO,
        );
        let home = Home::new(anthropic.port);
        std::fs::write(home.agent().join("settings.json"), GLOBAL).unwrap();
        let python = std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into());
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../ai/fixtures/claude-subscription/claude.py");
        let fake = home.root.join("fake-bin/claude");
        std::fs::create_dir_all(fake.parent().unwrap()).unwrap();
        executable_support::install(
            &fake,
            &format!("#!/bin/sh\nexec {python} -I {} \"$@\"\n", script.display()),
        );
        let log = home.root.join("claude-log.jsonl");
        Fixture {
            home,
            anthropic,
            fake,
            log,
        }
    }

    /// Only the fake `claude` and the system tools on PATH; the fake answers without any upstream request.
    fn with_fake(&self, command: &mut Command) {
        command
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.fake.parent().unwrap().display()),
            )
            .env("ULTRON_CLAUDE_COMMAND", &self.fake)
            .env("FAKE_LOG", &self.log)
            .env(
                "FAKE_OFFLINE_BLOCKS",
                json!([{"type": "text", "text": SUBSCRIPTION_ANSWER}]).to_string(),
            );
    }

    /// `ultron <args>` in the instance of the caller's directory (the durable one for `durable ...`).
    fn ultron(&self, args: &[&str]) -> (i32, String, String) {
        let mut command = self.home.instance_command(&self.home.cwd(), args);
        self.with_fake(&mut command);
        output(&mut command)
    }

    /// `ultron-s <args>`: the binary through a link of that name.
    fn ultron_s(&self, args: &[&str]) -> (i32, String, String) {
        let link = self.home.root.join("bin/ultron-s");
        if !link.exists() {
            std::fs::create_dir_all(link.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_ultron"), &link).unwrap();
        }
        let template = self.home.instance_command(&self.home.cwd(), &[]);
        let mut command = Command::new(&link);
        command
            .args(args)
            .current_dir(self.home.cwd())
            .stdin(std::process::Stdio::null())
            .env_clear();
        for (key, value) in template.get_envs() {
            if let Some(value) = value {
                command.env(key, value);
            }
        }
        command.env_remove("ULTRON_SERVER_DIR");
        self.with_fake(&mut command);
        output(&mut command)
    }

    /// `ultron <args>` as a plain run (the test's own server directory, not the durable instance's).
    fn plain(&self, args: &[&str]) -> (i32, String, String) {
        let mut command = self.home.command(args);
        self.with_fake(&mut command);
        output(&mut command)
    }

    /// The model of each subscription call the fake `claude` answered (its `--model`).
    fn subscription_calls(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .filter(|r| r.get("kind").is_none())
            .map(|r| {
                let argv: Vec<String> = serde_json::from_value(r["argv"].clone()).unwrap();
                argv[argv.iter().position(|a| a == "--model").unwrap() + 1].clone()
            })
            .collect()
    }

    /// The model of each request the global default's provider got.
    fn anthropic_models(&self) -> Vec<String> {
        self.anthropic
            .requests
            .lock()
            .unwrap()
            .iter()
            .map(|r| r["model"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    fn settings(&self) -> Value {
        serde_json::from_str(&std::fs::read_to_string(self.home.agent().join("settings.json")).unwrap()).unwrap()
    }

    fn rpc(&self, program_args: &[&str], args: &[&str]) -> RpcClient {
        let mut command = match program_args.first() {
            Some(_) => self.home.instance_command(&self.home.cwd(), &[]),
            None => self.home.command(&[]),
        };
        self.with_fake(&mut command);
        let env = command
            .get_envs()
            .filter_map(|(k, v)| Some((k.to_str()?.to_string(), v?.to_str()?.to_string())))
            .collect();
        RpcClient::start(&RpcClientOptions {
            program: env!("CARGO_BIN_EXE_ultron").into(),
            cwd: Some(self.home.cwd()),
            env_clear: true,
            env,
            program_args: program_args.iter().map(|a| a.to_string()).collect(),
            args: args.iter().map(|a| a.to_string()).collect(),
            ..Default::default()
        })
        .unwrap()
    }

    /// Stop the durable server; every process of the test is gone.
    fn stop(&self) {
        let (code, _, err) = self.ultron(&["durable", "stop"]);
        assert_eq!(code, 0, "{err}");
        assert_none_mentioning(
            &[
                self.home.root.clone(),
                daemon::instance::socket_dir(&self.home.root.join(".ultron/server")),
            ],
            HANG_GUARD,
        );
    }
}

fn output(command: &mut Command) -> (i32, String, String) {
    let output = command.output().unwrap();
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// The `provider/modelId` a session's stored lane names.
fn stored_model(home: &Home, id: &str) -> String {
    let meta = daemon::catalog::list(&home.sessions())
        .into_iter()
        .find(|m| m.id == id)
        .unwrap_or_else(|| panic!("no session {id}"));
    let text = std::fs::read_to_string(&meta.path).unwrap();
    // The last lane configuration written for the main lane.
    text.lines()
        .rev()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find_map(|record| find_lane_model(&record))
        .unwrap_or_else(|| panic!("no lane model in {}", meta.path.display()))
}

fn find_lane_model(value: &Value) -> Option<String> {
    match value {
        Value::Object(map) => {
            if let (Some(provider), Some(id)) = (
                map.get("model").and_then(|m| m["provider"].as_str()),
                map.get("model").and_then(|m| m["modelId"].as_str()),
            ) && map.contains_key("thinkingLevel")
            {
                return Some(format!("{provider}/{id}"));
            }
            map.values().find_map(find_lane_model)
        }
        Value::Array(items) => items.iter().find_map(find_lane_model),
        _ => None,
    }
}

#[test]
fn cli_durable_new_sessions_start_on_the_subscription_and_explicit_flags_win() {
    let fixture = Fixture::new(5);
    // `ultron durable -p` with a global default on another provider: the subscription, never that provider.
    let (code, out, err) = fixture.ultron(&["durable", "-p", "first"]);
    assert_eq!((code, out.as_str()), (0, "from the subscription\n"), "{err}");
    // `ultron durable --mode json`: a new session, on `claude-code/claude-opus-5-5`.
    let (code, out, err) = fixture.ultron(&["durable", "--mode", "json", "second"]);
    assert_eq!(code, 0, "{err}");
    let result: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(result["text"], SUBSCRIPTION_ANSWER);
    let durable_id = result["sessionId"].as_str().unwrap().to_string();
    assert_eq!(stored_model(&fixture.home, &durable_id), "claude-code/claude-opus-5-5");
    // `ultron-s -p` (the home session) and `ultron-s --new -p`.
    let (code, out, err) = fixture.ultron_s(&["-p", "third"]);
    assert_eq!((code, out.as_str()), (0, "from the subscription\n"), "{err}");
    let (code, out, err) = fixture.ultron_s(&["--new", "--mode", "json", "fourth"]);
    assert_eq!(code, 0, "{err}");
    let fresh: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(fresh["text"], SUBSCRIPTION_ANSWER);
    assert_eq!(
        stored_model(&fixture.home, fresh["sessionId"].as_str().unwrap()),
        "claude-code/claude-opus-5-5"
    );
    // Every one of them ran as Claude Opus on the subscription (a tool lane: `claude-subscription`).
    let calls = fixture.subscription_calls();
    assert_eq!(calls.len(), 4, "{calls:?}");
    assert!(calls.iter().all(|m| m.starts_with("claude-opus-5-5")), "{calls:?}");
    assert_eq!(fixture.anthropic_models(), Vec::<String>::new());
    // An explicit `--model` wins over `durable.model`.
    let (code, out, err) = fixture.ultron(&[
        "durable",
        "--mode",
        "json",
        "--model",
        "anthropic/claude-haiku-4-5",
        "explicit",
    ]);
    assert_eq!(code, 0, "{err}");
    let explicit: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(explicit["text"], "from anthropic 0");
    assert_eq!(fixture.anthropic_models(), ["claude-haiku-4-5"]);
    // A resumed session keeps its own model: the explicit one, and the subscription one.
    let explicit_id = explicit["sessionId"].as_str().unwrap();
    let (code, out, err) = fixture.ultron(&["durable", "-p", "--session-id", explicit_id, "again"]);
    assert_eq!((code, out.as_str()), (0, "from anthropic 1\n"), "{err}");
    assert_eq!(fixture.anthropic_models(), ["claude-haiku-4-5", "claude-haiku-4-5"]);
    let (code, out, err) = fixture.ultron(&["durable", "-p", "--session-id", &durable_id, "again"]);
    assert_eq!((code, out.as_str()), (0, "from the subscription\n"), "{err}");
    assert_eq!(fixture.subscription_calls().len(), 5);
    // `--models` chooses a new session's model too.
    let (code, out, err) = fixture.ultron(&["durable", "-p", "--models", "anthropic/claude-haiku-4-5", "scoped"]);
    assert_eq!((code, out.as_str()), (0, "from anthropic 2\n"), "{err}");
    assert_eq!(fixture.anthropic_models().len(), 3);
    // Nothing was saved: the global default is as it was, and no `durable.model` was written.
    assert_eq!(fixture.settings(), serde_json::from_str::<Value>(GLOBAL).unwrap());
    // A client with no model to start on of its own (no provider key, no `claude`): its own checks (a new session
    // needs a model with a key) follow `durable.model` too, while plain `ultron` has nothing to start on. The durable
    // server already runs with the fake.
    let keyless = |args: &[&str], plain: bool| {
        let mut command = match plain {
            true => fixture.home.command(args),
            false => fixture.home.instance_command(&fixture.home.cwd(), args),
        };
        output(command.env_remove("ANTHROPIC_API_KEY").env("PATH", "/usr/bin:/bin"))
    };
    let (code, out, err) = keyless(&["durable", "-p", "keyless"], false);
    assert_eq!((code, out.as_str()), (0, "from the subscription\n"), "{err}");
    let (code, _, err) = keyless(&["-p", "keyless"], true);
    assert_eq!(code, 1, "{err}");
    assert_eq!(fixture.subscription_calls().len(), 6);
    // Plain `ultron` is unchanged: its new session starts on the global default.
    let (code, out, err) = fixture.plain(&["-p", "plain"]);
    assert_eq!((code, out.as_str()), (0, "from anthropic 3\n"), "{err}");
    assert_eq!(
        fixture.anthropic_models(),
        [
            "claude-haiku-4-5",
            "claude-haiku-4-5",
            "claude-haiku-4-5",
            "claude-sonnet-4-5"
        ]
    );
    assert_eq!(fixture.subscription_calls().len(), 6);
    fixture.stop();
}

#[test]
fn cli_durable_model_changes_save_durable_model_and_plain_ultron_saves_the_global_default() {
    let fixture = Fixture::new(4);
    let global = (json!("anthropic"), json!("claude-sonnet-4-5"));
    let saved = |f: &Fixture| {
        let settings = f.settings();
        (
            (settings["defaultProvider"].clone(), settings["defaultModel"].clone()),
            settings["durable"]["model"].clone(),
        )
    };
    // `set_model` in a durable session (/model, /settings): saved as `durable.model`, the global keys untouched.
    let mut client = fixture.rpc(&["durable"], &[]);
    let selected = client
        .send(json!({"type": "set_model", "provider": "anthropic", "modelId": "claude-haiku-4-5"}))
        .unwrap();
    assert_eq!(
        RpcClient::data(&selected).unwrap()["id"],
        "claude-haiku-4-5",
        "{selected}"
    );
    assert_eq!(saved(&fixture), (global.clone(), json!("anthropic/claude-haiku-4-5")));
    // `cycle_model` (Ctrl+P): the model it moves to, saved there too.
    let cycled = client.send(json!({"type": "cycle_model"})).unwrap();
    let model = RpcClient::data(&cycled).unwrap()["model"].clone();
    let cycled_ref = format!(
        "{}/{}",
        model["provider"].as_str().unwrap(),
        model["id"].as_str().unwrap()
    );
    assert_ne!(cycled_ref, "anthropic/claude-haiku-4-5");
    assert_eq!(saved(&fixture), (global.clone(), json!(cycled_ref)));
    // Back to a known model for the next session.
    client
        .send(json!({"type": "set_model", "provider": "anthropic", "modelId": "claude-haiku-4-5"}))
        .unwrap();
    let (_, code) = client.finish(HANG_GUARD);
    assert_eq!(code, Some(0));
    // A new durable session starts on the saved `durable.model`.
    let (code, out, err) = fixture.ultron(&["durable", "-p", "next"]);
    assert_eq!((code, out.as_str()), (0, "from anthropic 0\n"), "{err}");
    assert_eq!(fixture.anthropic_models(), ["claude-haiku-4-5"]);
    assert_eq!(fixture.subscription_calls(), Vec::<String>::new());
    // Plain `ultron`: its new session is on the global default, and a model chosen there is saved globally.
    let (code, out, err) = fixture.plain(&["-p", "plain"]);
    assert_eq!((code, out.as_str()), (0, "from anthropic 1\n"), "{err}");
    assert_eq!(fixture.anthropic_models(), ["claude-haiku-4-5", "claude-sonnet-4-5"]);
    let mut plain = fixture.rpc(&[], &[]);
    plain
        .send(json!({"type": "set_model", "provider": "anthropic", "modelId": "claude-opus-4-8"}))
        .unwrap();
    assert_eq!(
        saved(&fixture),
        (
            (json!("anthropic"), json!("claude-opus-4-8")),
            json!("anthropic/claude-haiku-4-5")
        )
    );
    let (_, code) = plain.finish(HANG_GUARD);
    assert_eq!(code, Some(0));
    fixture.stop();
}
