//! The persona's auto-nudge for `ultron claude` (resources::persona): the server's answer to Claude Code's Stop hook
//! blocks the stop with the nudge text when the turn (read from Claude Code's transcript) made a tool call and ended
//! by announcing its next step, at most [`resources::persona::MAX_NUDGES`] times since the user's last prompt. The
//! same rule and text as the user's Claude Code hook (ultron-persona.py), with nothing installed in ~/.claude.

use serde_json::{Map, Value, json};

/// The Stop hook's answer: `{"decision": "block", "reason": <nudge>}` when a nudge is due (counted in `nudges`),
/// else `{}`. `transcript` is Claude Code's JSONL transcript; `stop_hook_active` (a stop that a nudge already
/// continued) does not stop the chain, the cap does.
pub fn stop_answer(persona: bool, transcript: &str, nudges: &mut u32) -> Value {
    if !persona || *nudges >= resources::persona::MAX_NUDGES {
        return json!({});
    }
    let (text, tool_calls) = resources::persona::last_turn(transcript);
    if !resources::persona::should_nudge(&text, tool_calls) {
        return json!({});
    }
    *nudges += 1;
    json!({"decision": "block", "reason": resources::persona::NUDGE_TEXT})
}

/// The transcript a hook input names (`transcript_path`); "" when there is none or it cannot be read.
pub fn transcript_of(input: &Map<String, Value>) -> String {
    input
        .get("transcript_path")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())
        .and_then(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_default()
}
