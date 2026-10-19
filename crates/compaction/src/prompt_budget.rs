//! The prompt budget check before a request (wave 38, from Mirror's react/agent.rs `validate_budget` and
//! compaction/compactor.rs `Compactor::new`): the system prompt and the tool schemas are sent with every request and
//! no compaction can shrink them. When they leave the conversation less than `reserveTokens` below the compaction
//! threshold (`contextWindow - reserveTokens`), every compaction would end above the threshold again, so the run
//! fails at once with the sizes instead of compacting in a loop or overflowing at the provider.
//!
//! Sizes use auto-compaction's own estimator ([`crate::estimate_tokens`] of the system message, its `toolsAdded`
//! included). Small windows (under three times the reserve, where compaction cannot keep a reserve-sized
//! conversation anyway and the TypeScript product still runs) are not refused for that: the fixed part may always
//! take up to `reserveTokens` (or the window minus one, when smaller), so the limit is
//! `max(contextWindow - 2 * reserveTokens, min(contextWindow - 1, reserveTokens))` and grows continuously with the
//! window. Without compaction only the window limits: the fixed part must leave some room in it. An unknown window
//! (0) is not checked.

use crate::{CompactionSettings, estimate_tokens};
use serde_json::{Value, json};

/// The fixed part of every request, in estimated tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PromptSize {
    /// The system prompt text (and its sections).
    pub system: u64,
    /// The tool schemas the system message declares.
    pub tools: u64,
}

impl PromptSize {
    /// The sizes of `system`, a system message as the transcript holds it (`{role: "system", content, sections?,
    /// toolsAdded?}`); `tools` is what `toolsAdded` adds to the estimate, so the two sum to the message's.
    pub fn of(system: &Value) -> PromptSize {
        let total = estimate_tokens(system);
        let mut text_only = json!({"role": "system", "content": system.get("content").cloned().unwrap_or(Value::Null)});
        if let Some(sections) = system.get("sections") {
            text_only["sections"] = sections.clone();
        }
        let text = estimate_tokens(&text_only).min(total);
        PromptSize {
            system: text,
            tools: total - text,
        }
    }

    pub fn fixed(&self) -> u64 {
        self.system + self.tools
    }
}

/// Whether a request whose fixed part is `size` fits `context_window` under `settings`; the error names the sizes
/// and what to change. `model` labels the message (`provider/id`).
pub fn check_prompt_budget(
    size: PromptSize,
    context_window: u64,
    settings: &CompactionSettings,
    model: &str,
) -> Result<(), String> {
    if context_window == 0 {
        return Ok(());
    }
    let (fixed, reserve) = (size.fixed(), settings.reserve_tokens);
    let fix = "Shorten the system prompt (AGENTS.md and other context files, --system-prompt or --append-system-prompt, extension guides), lower compaction.reserveTokens, or switch to a model with a larger context window.";
    let (system, tools) = (size.system, size.tools);
    if settings.enabled {
        let reserved = context_window.saturating_sub(reserve.saturating_mul(2));
        let small = (context_window - 1).min(reserve);
        if fixed <= reserved.max(small) {
            return Ok(());
        }
        if reserved >= small {
            let threshold = context_window - reserve;
            return Err(format!(
                "Prompt budget exceeded for {model}: the system prompt ({system} tokens) and tool schemas ({tools} tokens) take {fixed} tokens, leaving {} below the compaction threshold of {threshold} tokens (context window {context_window} minus compaction.reserveTokens {reserve}); the conversation needs at least {reserve}. {fix}",
                threshold.saturating_sub(fixed),
            ));
        }
        return Err(format!(
            "Prompt budget exceeded for {model}: the system prompt ({system} tokens) and tool schemas ({tools} tokens) take {fixed} tokens; a {context_window}-token context window with compaction.reserveTokens {reserve} leaves room for at most {small}. {fix}"
        ));
    }
    if fixed < context_window {
        return Ok(());
    }
    Err(format!(
        "Prompt budget exceeded for {model}: the system prompt ({system} tokens) and tool schemas ({tools} tokens) take {fixed} tokens of the {context_window}-token context window, leaving no room for the conversation. {fix}"
    ))
}
