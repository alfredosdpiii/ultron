//! `claude-code/<id>` requests that declare tools run on the Claude subscription.
//!
//! Problem: `claude -p` (the `claude-code-cli` API) returns completions, never raw tool calls, and TypeScript's lane
//! tool runner (`worker-root.ts` `setClaudeCodeToolRunner`) is not ported. So every request with a tool list failed
//! ("does not support tool calling here"): a session on `/model claude-code/<id>`, and every inference frame with
//! `budget.depth > 1` (its conversation declares `rlm`), which calls `ai::dispatch` directly.
//!
//! Trace: a depth-2 frame on `claude-code/claude-haiku-5-5` sends `[system(tools: rlm), user]` through
//! `dispatch::stream_simple`; [`route`] sees the `claude-code-cli` API and a tool, and the call runs as
//! `claude-subscription/claude-haiku-5-5` on the same `claude` login (the `rlm` tool goes to the model through the
//! inert MCP inventory, Ultron runs the cell). The session lane routes the same way through its own router.
//!
//! Two guarantees (decisions.tsv 2026-10-12/13, wave 45):
//! - No API spend: `claude-code` never used an API key in TypeScript, so a rerouted call runs on the subscription
//!   only. Out of usage, it fails with the plan's usage-limit error; the API credits fallback stays with explicit
//!   `claude-subscription/<id>` models (`fallback::Router::stream`).
//! - A backend override (`CLAUDE_CODE_USE_BEDROCK`/`VERTEX`/`FOUNDRY`, `ANTHROPIC_BASE_URL`) means Claude Code bills
//!   that backend, which the subscription provider refuses; such a request is not rerouted and fails with
//!   [`backend_error`], naming `ultron claude` (Claude Code as the root agent) or a model that is not `claude-code`.
//!
//! Tool-free requests (frames of depth 1, `/review` frames, judges) stay on `claude -p`; an id the subscription
//! does not serve keeps `claude_code_cli`'s tools error.

use super::{Env, catalog, process};
use crate::options::now_ms;
use crate::transcript::current_tools;
use crate::types::{AssistantMessage, AssistantMessageEvent, EventSink, Model, StopReason, TranscriptContext};

/// Where a `claude-code` request goes.
#[derive(Debug, Clone, PartialEq)]
pub enum Route {
    /// Run it as this `claude-subscription` model.
    Subscription(Box<Model>),
    /// Refuse it with this error (a backend override is set).
    Refused(String),
}

/// The error of a `claude-code` request with tools while backend overrides are set; names variables, never values.
pub fn backend_error(model: &Model, overrides: &[&str]) -> String {
    format!(
        "{}/{} cannot run a lane with tools while {} {} set: tool lanes on Claude Code run on the Claude subscription (claude-subscription), which uses only the Claude Code login, not that backend. With this backend, run Claude Code as the root agent with `ultron claude`, or use a model that is not claude-code for this lane.",
        model.provider,
        model.id,
        overrides.join(", "),
        if overrides.len() == 1 { "is" } else { "are" }
    )
}

/// The route of a request: `None` leaves it to `claude -p` (another API, no tools, or an id the subscription does
/// not serve). `env` is the environment the subscription would run in, read only for a request with tools.
pub fn route(model: &Model, context: &TranscriptContext, env: impl FnOnce() -> Env) -> Option<Route> {
    if model.api != "claude-code-cli" || current_tools(&context.messages).is_empty() {
        return None;
    }
    let overrides = process::conflicts(&env());
    if !overrides.is_empty() {
        return Some(Route::Refused(backend_error(model, &overrides)));
    }
    catalog::model(&model.id).map(|m| Route::Subscription(Box::new(m)))
}

/// End a refused request's stream: `start`, then `error` with the message.
pub fn refuse(model: &Model, error: String, sink: &mut EventSink<'_>) -> AssistantMessage {
    let mut message = AssistantMessage::pending(model, now_ms());
    sink(&AssistantMessageEvent::Start {
        partial: message.clone(),
    });
    message.stop_reason = StopReason::Error;
    message.error_message = Some(error);
    sink(&AssistantMessageEvent::Error {
        reason: StopReason::Error,
        error: message.clone(),
    });
    message
}
