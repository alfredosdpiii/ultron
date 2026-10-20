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

/// Every tool call in Claude Code's transcript (JSONL), whatever turn it belongs to.
pub fn tool_calls_total(transcript: &str) -> u64 {
    transcript
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|entry| entry["type"] == "assistant")
        .map(|entry| {
            entry["message"]["content"].as_array().map_or(0, |blocks| {
                blocks.iter().filter(|b| b["type"] == "tool_use").count() as u64
            })
        })
        .sum()
}

/// The Stop hook's answer with the session's persona state (resources::keep_going): in keep-going mode a run that
/// did work is continued (`{"decision": "block", "reason": <keep-going nudge>}`) up to the ceiling, where the last
/// nudge asks for the final report; a run that did none ends the mode. Otherwise the persona's own rule applies,
/// with the session's toggle over the settings' `persona` (`global`). `state` and `nudges` are updated.
///
/// A run did work when the transcript's tool calls grew since the last decision (the first decision counts those
/// since the user's prompt), however Claude Code records a blocked stop's reason.
pub fn stop_answer_in(
    state: &mut resources::keep_going::SessionPersona,
    nudges: &mut u32,
    global: bool,
    transcript: &str,
    now: i64,
    limits: resources::keep_going::Limits,
) -> Value {
    use resources::keep_going::{Next, RunEnd, after_run, nudge_text};
    let (text, since_prompt) = resources::persona::last_turn(transcript);
    let total = tool_calls_total(transcript);
    let worked = match state.keep_going.as_ref().and_then(|k| k.tool_calls_seen) {
        Some(seen) => total > seen,
        None => since_prompt > 0,
    };
    let end = RunEnd {
        tool_calls: usize::from(worked),
        text: &text,
        ..RunEnd::default()
    };
    let next = after_run(state, &end, now, limits);
    if let Some(stretch) = state.keep_going.as_mut() {
        stretch.tool_calls_seen = Some(total);
    }
    match next {
        Next::Persona => stop_answer(state.effective(global), transcript, nudges),
        Next::Stop => json!({}),
        Next::Continue { .. } | Next::Final { .. } => {
            if matches!(next, Next::Final { .. }) {
                *nudges = resources::persona::MAX_NUDGES;
            }
            json!({"decision": "block", "reason": nudge_text(&next).unwrap_or_default()})
        }
    }
}

/// The UserPromptSubmit hook's persona part for `prompt`: a whole-prompt toggle (`/persona on|off|auto`,
/// `/keepgoing on|off`) is applied and the prompt is blocked with the new status as its reason (no turn starts);
/// otherwise the prompt is user input ([`SessionPersona::user_input`]): a keep-going phrase starts the mode (a
/// `systemMessage` notice and the start text for the model), other input ends a stretch in progress. `None` for a
/// toggle means the prompt goes on; `Some(answer)` is the whole hook answer.
///
/// [`SessionPersona::user_input`]: resources::keep_going::SessionPersona::user_input
pub fn prompt_toggle(
    state: &mut resources::keep_going::SessionPersona,
    prompt: &str,
    now: i64,
    global: bool,
    limits: resources::keep_going::Limits,
) -> Option<Value> {
    let (command, args) = resources::keep_going::toggle_command(prompt)?;
    let reason = match state.command(command, &args, now, global, limits) {
        Ok(text) | Err(text) => text,
    };
    Some(json!({"decision": "block", "reason": reason}))
}

/// What a prompt that is not a toggle adds to the UserPromptSubmit answer: `(systemMessage, additionalContext)`.
pub fn prompt_input(
    state: &mut resources::keep_going::SessionPersona,
    prompt: &str,
    now: i64,
) -> (Option<String>, Option<String>) {
    let notice = state.user_input(prompt, now);
    let context =
        (notice.is_some() && state.keep_going.is_some()).then(|| resources::keep_going::START_TEXT.to_string());
    (notice, context)
}
