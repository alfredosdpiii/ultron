//! Under `ultron claude` every Ultron child that names no model runs on the Claude root's own subscription model
//! (`claude-code/<id>`, whose `rlm` lanes run on the Claude subscription only): in-memory `--children ultron`
//! subagents, persistent children and typed agents' `rlm` lanes. The chain is `--ultron-child-model` /
//! `claudeCode.ultronChildModel`, then `rlm.childModel` only when it names a Claude Code model, then the root's
//! model (`--root-model` from the launcher, else the default frame model). The settings' default model is never
//! used (TypeScript falls back to it; under a Claude root that put children on API credits). Rust-only behavior,
//! so the expectations are literal.

use claude_root::guide::PromptResources;
use claude_root::launcher::{LaunchInput, LauncherOptions, build_claude_launch};
use claude_root::mcp_server::{
    Resolved, claude_code_setting, default_frame_model, parse_mcp_args, resolve, subscription_model, ultron_child_model,
};
use claude_root::self_cmd::SelfCommand;
use harness::chat::ModelRef;
use std::collections::BTreeSet;
use std::path::Path;

fn resolved(ultron: Option<&str>) -> Resolved {
    Resolved {
        frame_model: "claude-code/claude-opus-5-5".into(),
        frame_model_explicit: false,
        children: "ultron".into(),
        child_model: "claude-opus-5-5".into(),
        child_model_explicit: false,
        ultron_child_model: ultron.map(str::to_string),
        root_model: None,
    }
}

/// The registry's `claude-code` models.
fn registry(id: &str) -> bool {
    ai::claude_code::MODELS.contains_key(id)
}

#[test]
fn claude_root_subscription_model_is_the_roots_registry_model_else_the_default_frame_model() {
    assert_eq!(subscription_model(None, &registry), "claude-code/claude-opus-5-5");
    assert_eq!(subscription_model(Some("sonnet"), &registry), "claude-code/sonnet");
    assert_eq!(
        subscription_model(Some(" claude-code/haiku "), &registry),
        "claude-code/haiku"
    );
    assert_eq!(
        subscription_model(Some("claude-haiku-5-5"), &registry),
        "claude-code/claude-haiku-5-5"
    );
    // Not a registry claude-code model (another alias, a 1M suffix, blank): the default frame model.
    for unknown in ["fable", "opus[1m]", "", "claude-code/"] {
        assert_eq!(
            subscription_model(Some(unknown), &registry),
            "claude-code/claude-opus-5-5",
            "{unknown:?}"
        );
    }
    // The fallback is in the registry, and the subscription serves it (a request with tools is rerouted there).
    let fallback = default_frame_model();
    let id = fallback.strip_prefix("claude-code/").unwrap();
    assert!(registry(id), "{fallback}");
    assert!(ai::claude_subscription::catalog::model(id).is_some(), "{fallback}");
    for id in ai::claude_code::MODELS.keys() {
        assert!(ai::claude_subscription::catalog::model(id).is_some(), "{id}");
    }
}

#[test]
fn claude_root_ultron_children_fall_back_to_the_roots_subscription_model() {
    let subscription = "claude-code/sonnet";
    // Nothing names a model: the root's subscription model.
    assert_eq!(ultron_child_model(&resolved(None), None, subscription), subscription);
    // `rlm.childModel` naming another provider's model is a native session's choice, not used here.
    assert_eq!(
        ultron_child_model(&resolved(None), Some("cliproxyapi/gpt-6-astra"), subscription),
        subscription
    );
    // `rlm.childModel` naming a Claude Code model is used.
    assert_eq!(
        ultron_child_model(&resolved(None), Some(" claude-code/haiku "), subscription),
        "claude-code/haiku"
    );
    // `--ultron-child-model` / `claudeCode.ultronChildModel` wins, on any provider; blank counts as unset.
    assert_eq!(
        ultron_child_model(&resolved(Some("groq/llama")), Some("claude-code/haiku"), subscription),
        "groq/llama"
    );
    assert_eq!(
        ultron_child_model(&resolved(Some("claude-code/opus")), None, subscription),
        "claude-code/opus"
    );
    assert_eq!(
        ultron_child_model(&resolved(Some("  ")), None, subscription),
        subscription
    );
    assert_eq!(claude_code_setting(Some("claude-code/")), None);
    assert_eq!(claude_code_setting(None), None);
}

#[test]
fn claude_root_keep_children_default_to_the_subscription_model() {
    use claude_root::durable_apis::keep_model;
    let subscription = "claude-code/claude-opus-5-5";
    assert_eq!(
        keep_model(&resolved(None), None, subscription),
        ModelRef::new("claude-code", "claude-opus-5-5")
    );
    assert_eq!(
        keep_model(&resolved(None), Some("openai/gpt-5.5"), subscription),
        ModelRef::new("claude-code", "claude-opus-5-5")
    );
    assert_eq!(
        keep_model(&resolved(Some("groq/llama")), None, subscription),
        ModelRef::new("groq", "llama")
    );
    // A Claude Code `--ultron-child-model` is no longer refused.
    assert_eq!(
        keep_model(&resolved(Some("claude-code/haiku")), None, subscription),
        ModelRef::new("claude-code", "haiku")
    );
    // A named model that is not `provider/model` falls through to the subscription model.
    assert_eq!(
        keep_model(&resolved(Some("llama")), None, subscription),
        ModelRef::new("claude-code", "claude-opus-5-5")
    );
}

#[test]
fn claude_root_server_learns_the_roots_model_from_root_model_or_a_subagents_child_model() {
    let strings = |args: &[&str]| args.iter().map(|a| a.to_string()).collect::<Vec<_>>();
    let dir = tempfile::tempdir().unwrap();
    let resolve_args = |args: &[&str]| {
        let parsed = parse_mcp_args(&strings(args)).unwrap();
        resolve(&parsed, &|_| None, dir.path(), dir.path())
    };
    assert_eq!(
        resolve_args(&["--root-model", "sonnet"]).root_model.as_deref(),
        Some("sonnet")
    );
    assert_eq!(resolve_args(&[]).root_model, None);
    // A root server's `--child-model` is its Claude Code children's, not its own.
    assert_eq!(resolve_args(&["--child-model", "haiku"]).root_model, None);
    // A Claude Code subagent's server: its root is that subagent, on its `--child-model`.
    let child = [
        "--child",
        "--parent-socket",
        "/s",
        "--parent-lane",
        "l",
        "--child-model",
        "haiku",
    ];
    assert_eq!(resolve_args(&child).root_model.as_deref(), Some("haiku"));
}

fn mcp_args(claude_args: &[&str], var: &dyn Fn(&str) -> Option<String>) -> Vec<String> {
    // No hooks: no `--wake` after the model flags.
    let options = LauncherOptions {
        hooks: false,
        claude_args: claude_args.iter().map(|a| a.to_string()).collect(),
        ..LauncherOptions::default()
    };
    let self_cmd = SelfCommand {
        command: "ultron".into(),
        args: Vec::new(),
    };
    let flags = BTreeSet::from(["mcp-config".to_string()]);
    let resources = PromptResources::default();
    let launch = build_claude_launch(&LaunchInput {
        claude: "claude",
        flags: &flags,
        self_cmd: &self_cmd,
        options: &options,
        cwd: "/w",
        dir: Some(Path::new("/tmp/x")),
        socket: "/run/s.sock",
        var,
        date: "2026-10-22".into(),
        platform: "linux",
        resources: &resources,
        bundled_skills: None,
        hints: false,
        skip_permissions: Vec::new(),
        apis: None,
    });
    let mcp: serde_json::Value = serde_json::from_str(&launch.files[0].1).unwrap();
    serde_json::from_value(mcp["mcpServers"]["ultron"]["args"].clone()).unwrap()
}

#[test]
fn claude_root_launcher_tells_the_server_the_roots_model_unless_it_is_the_default() {
    let none = |_: &str| None;
    let base = ["mcp", "--no-instructions", "--socket", "/run/s.sock"];
    let with = |model: &str| {
        let mut args: Vec<String> = base.iter().map(|a| a.to_string()).collect();
        args.extend(["--root-model".to_string(), model.to_string()]);
        args
    };
    assert_eq!(mcp_args(&[], &none), base);
    assert_eq!(mcp_args(&["--model", "claude-opus-5-5"], &none), base);
    assert_eq!(mcp_args(&["-p", "hi", "--model", "sonnet"], &none), with("sonnet"));
    assert_eq!(mcp_args(&["--model=haiku"], &none), with("haiku"));
    let env = |key: &str| (key == "ULTRON_CLAUDE_MODEL").then(|| " claude-haiku-5-5 ".to_string());
    assert_eq!(mcp_args(&[], &env), with("claude-haiku-5-5"));
    // A `--model` among the claude args beats ULTRON_CLAUDE_MODEL, as for `claude` itself.
    assert_eq!(mcp_args(&["--model", "sonnet"], &env), with("sonnet"));
}
