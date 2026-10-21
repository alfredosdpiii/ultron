//! The session's own commands typed as a prompt (Rust-only): `/goal [args]` (the session goal), `/persona
//! on|off|auto` and `/keepgoing [on|off]` (the session's persona toggle and keep-going mode). The worker answers
//! them where every client's prompt enters it (`pi.agent-controller.prompt`, the `op` service's `prompt`), so `-p`,
//! `--mode json`, RPC and ACP clients get the command, never a model turn that reads it as text. A command's answer
//! is `{"command": <name>, "text": <its reply>, "data": <the operation's answer>}` in place of a run's.

use serde_json::{Value, json};
use session_core::core::Op;

/// JavaScript's `trim` whitespace (also U+FEFF).
fn trim(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
}

/// The command a prompt `message` is, as `(name, operation)`; `None` for any other text (a turn).
pub fn command(message: &str) -> Option<(&'static str, Op)> {
    let text = trim(message);
    if let Some(rest) = text.strip_prefix("/goal")
        && (rest.is_empty() || rest.starts_with(char::is_whitespace))
    {
        return Some((
            "goal",
            Op::Goal {
                args: trim(rest).to_string(),
            },
        ));
    }
    if !text.starts_with('/') {
        return None;
    }
    let (name, args) = resources::keep_going::toggle_command(text)?;
    Some(match name {
        "keepgoing" => ("keepgoing", Op::KeepGoing { args }),
        _ => ("persona", Op::Persona { args }),
    })
}

/// The answer to command `name` whose operation answered `data`.
pub fn answer(name: &str, data: Option<Value>) -> Value {
    let data = data.unwrap_or(Value::Null);
    let text = match &data {
        Value::String(text) => text.clone(),
        other => other["text"].as_str().unwrap_or_default().to_string(),
    };
    json!({"command": name, "text": text, "data": data})
}
