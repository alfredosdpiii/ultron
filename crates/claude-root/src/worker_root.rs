//! Claude Code lanes in the session worker (worker-root.ts): `ultron --claude`, `ULTRON_ROOT=claude` or the
//! `claudeCode.root` setting run the root lane on Claude. [`prepare_claude_root`] is TypeScript's model selection
//! (`claude-code/<id>`). The Rust build does not port TypeScript's lane tool runner (root-runner.ts, where Claude
//! Code's own loop drives the lane through an MCP bridge); by the user's decision Claude runs on the
//! `claude-subscription` provider, which keeps Ultron's harness as the agent loop with exactly `[rlm]`, so
//! [`subscription_root`] maps the selection to `claude-subscription/<id>` (decisions.tsv, 2026-10-13 claude).

use crate::claude_cli::DEFAULT_CLAUDE_MODEL;
use crate::self_cmd::self_command;

/// `ULTRON_ROOT=claude` (what `ultron --claude` sets): the root lane runs on Claude Code.
pub const ULTRON_ROOT_ENV: &str = "ULTRON_ROOT";
pub const CLAUDE_CODE_PROVIDER_ID: &str = "claude-code";

pub fn claude_root_requested(var: &dyn Fn(&str) -> Option<String>) -> bool {
    var(ULTRON_ROOT_ENV).is_some_and(|v| v.trim().eq_ignore_ascii_case("claude"))
}

/// What the lanes are told about their tools under Claude Code (appended to the lane's own system prompt).
pub const CLAUDE_CODE_LANE_NOTE: &str = "# Running on Claude Code
You run inside Claude Code (headless), driven by Ultron. Your tools are Ultron's, served over MCP: `rlm` is listed as `mcp__ultron__rlm` (any other tool likewise as `mcp__ultron__<name>`). Claude Code's own built-in tools are disabled; do everything through these. When background work you started finishes while you are idle, Ultron starts a new turn with a `<runtime_event>` message, as the runtime guide describes. Claude Code manages your context window.";

/// The root model `--claude` selects, and the environment the session worker must get.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaudeRoot {
    pub provider: Option<String>,
    pub model: String,
    /// `ULTRON_ROOT=claude`, and `ULTRON_SELF_COMMAND` when it was unset.
    pub env: Vec<(String, String)>,
}

fn model_error(model: &str) -> String {
    format!(
        "--claude runs the root agent on Claude Code, which does not serve {model}. Use --model claude-code/<model> (for example claude-code/sonnet), or leave out --claude."
    )
}

/// A bare Claude model or alias (`/^(claude-|opus|sonnet|haiku|fable)/i`).
fn claude_alias(model: &str) -> bool {
    let lower = model.to_lowercase();
    ["claude-", "opus", "sonnet", "haiku", "fable"]
        .iter()
        .any(|prefix| lower.starts_with(prefix))
}

/// `prepareClaudeRoot`: the model the root starts on, as `claude-code/<id>`, from `--model` (`claude-code/<id>`,
/// or a bare Claude model or alias), ULTRON_CLAUDE_MODEL, or claude-opus-5-5. Fails for a model Claude Code does
/// not serve.
pub fn prepare_claude_root(
    provider: Option<&str>,
    model: Option<&str>,
    var: &dyn Fn(&str) -> Option<String>,
) -> Result<ClaudeRoot, String> {
    let mut env = vec![(ULTRON_ROOT_ENV.to_string(), "claude".to_string())];
    if var("ULTRON_SELF_COMMAND").is_none_or(|v| v.trim().is_empty())
        && let Ok(command) = self_command(var)
    {
        let parts: Vec<String> = std::iter::once(command.command).chain(command.args).collect();
        env.push((
            "ULTRON_SELF_COMMAND".into(),
            serde_json::to_string(&parts).expect("strings serialize"),
        ));
    }
    let provider = provider.map(str::trim).filter(|p| !p.is_empty());
    let model = model.map(str::trim).filter(|m| !m.is_empty());
    let done = |provider: Option<&str>, model: String| ClaudeRoot {
        provider: provider.map(str::to_string),
        model,
        env: env.clone(),
    };
    let Some(model) = model else {
        if let Some(provider) = provider
            && provider != CLAUDE_CODE_PROVIDER_ID
        {
            return Err(model_error(provider));
        }
        let id = var("ULTRON_CLAUDE_MODEL")
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_CLAUDE_MODEL.into());
        return Ok(done(None, format!("{CLAUDE_CODE_PROVIDER_ID}/{id}")));
    };
    if let Some(provider) = provider {
        if provider != CLAUDE_CODE_PROVIDER_ID {
            return Err(model_error(&format!("{provider}/{model}")));
        }
        return Ok(done(Some(provider), model.to_string()));
    }
    if let Some(slash) = model.find('/').filter(|s| *s > 0) {
        if &model[..slash] != CLAUDE_CODE_PROVIDER_ID {
            return Err(model_error(model));
        }
        return Ok(done(None, model.to_string()));
    }
    if claude_alias(model) {
        return Ok(done(None, format!("{CLAUDE_CODE_PROVIDER_ID}/{model}")));
    }
    Err(model_error(model))
}

pub const CLAUDE_SUBSCRIPTION_PROVIDER_ID: &str = ai::claude_subscription::PROVIDER;

/// The root model of `--claude` in the Rust build: [`prepare_claude_root`]'s selection (which also takes
/// `claude-subscription/<id>` and `--provider claude-subscription`) as `claude-subscription/<id>`. Fails for a model
/// Claude Code does not serve or the subscription route table refuses.
pub fn subscription_root(
    provider: Option<&str>,
    model: Option<&str>,
    var: &dyn Fn(&str) -> Option<String>,
) -> Result<ClaudeRoot, String> {
    let subscription = |p: &str| p.trim() == CLAUDE_SUBSCRIPTION_PROVIDER_ID;
    let provider = provider.map(|p| if subscription(p) { CLAUDE_CODE_PROVIDER_ID } else { p });
    let rewritten = model.map(|m| {
        m.trim()
            .strip_prefix(&format!("{CLAUDE_SUBSCRIPTION_PROVIDER_ID}/"))
            .map_or_else(|| m.to_string(), |id| format!("{CLAUDE_CODE_PROVIDER_ID}/{id}"))
    });
    let selected = prepare_claude_root(provider, rewritten.as_deref(), var)?;
    let id = match selected.provider.as_deref() {
        Some(_) => selected.model.clone(),
        None => selected
            .model
            .strip_prefix(&format!("{CLAUDE_CODE_PROVIDER_ID}/"))
            .unwrap_or(&selected.model)
            .to_string(),
    };
    if ai::claude_subscription::catalog::model(&id).is_none() {
        return Err(model_error(&format!("{CLAUDE_SUBSCRIPTION_PROVIDER_ID}/{id}")));
    }
    Ok(ClaudeRoot {
        provider: None,
        model: format!("{CLAUDE_SUBSCRIPTION_PROVIDER_ID}/{id}"),
        env: selected.env,
    })
}
