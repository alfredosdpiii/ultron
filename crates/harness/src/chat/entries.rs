//! Built-in entry kinds (spec §8.1).

/// User input: `model` is `[UserMessage]`. Written by submissions.
pub const USER: &str = "pi.user";
/// Provider result with any stop reason: `model` is `[AssistantMessage]`. Written by generation.
pub const ASSISTANT: &str = "pi.assistant";
/// Positional prompt and tool change: `model` is `[SystemMessage]` with empty `content`.
pub const SYSTEM: &str = "pi.system";
/// A cell result: `model` is `[ToolResultMessage]`, whose content ends with the rendered diagnostics; `data` holds the
/// structured diagnostics.
pub const TOOL_RESULT: &str = "pi.tool-result";
/// Start of a new context: always `head: "self"`, with `model` absent or `[UserMessage]` carrying the handoff text.
pub const RESET: &str = "pi.reset";
/// Compaction summary: `model` is `[UserMessage]` with the wrapped summary, `head` the first kept entry.
pub const COMPACTION: &str = "pi.compaction";
