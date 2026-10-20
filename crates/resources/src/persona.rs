//! The Ultron persona (`"persona": "ultron"` in settings.json): `<agent dir>/personality.md` as a section of the
//! system prompt, and the auto-nudge that continues a root turn which ended by announcing its next step instead of
//! taking it. Rust-only (no TypeScript reference); the nudge rule and text are those of the user's Claude Code hook
//! (ultron-persona.py), recorded in fixtures/persona-nudge.json.

use regex::Regex;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The setting that turns the persona on.
pub const SETTING: &str = "persona";
/// The only persona there is.
pub const ULTRON: &str = "ultron";
/// The persona's text, in the agent dir.
pub const PERSONALITY_FILE: &str = "personality.md";
/// The custom message type of a nudge (`role: custom`): clients draw it as one line, the model sees its text.
pub const NUDGE_CUSTOM_TYPE: &str = "ultron.nudge";
/// Nudges allowed since the user last sent input.
pub const MAX_NUDGES: u32 = 5;
/// What a nudge says.
pub const NUDGE_TEXT: &str = "Continue: take that next step now. End the turn only when the work is done and verified, or when you need the user; then ask one direct question.";

/// The host's rules that close the persona section, whatever personality.md says: outward or irreversible actions
/// wait for the user (in keep-going mode they are skipped and listed), and schedules start only when asked.
pub const GUARDRAILS: &str = "Ask the user before an irreversible or outward action (pushing, publishing, deploying, deleting data, sending messages, spending money); in keep-going mode, while the user is away, skip it and list it for the user in the final report instead of asking. Never start a schedule (schedule.every, schedule.at) unless the user asked for one.";

/// The persona section's text: the persona's own text, then [`GUARDRAILS`].
pub fn section_text(personality: &str) -> String {
    format!("{personality}\n\n{GUARDRAILS}")
}

/// Whether the settings' `persona` is `ultron` (any other value, or none, is off).
pub fn enabled(settings: &serde_json::Map<String, Value>) -> bool {
    settings.get(SETTING).and_then(Value::as_str) == Some(ULTRON)
}

/// `<agent_dir>/personality.md`.
pub fn personality_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join(PERSONALITY_FILE)
}

/// The persona's text for the system prompt: `Ok(None)` for an empty file, `Err(warning)` when it cannot be read.
pub fn read_personality(agent_dir: &Path) -> Result<Option<String>, String> {
    let path = personality_path(agent_dir);
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let text = text.trim();
            Ok((!text.is_empty()).then(|| text.to_string()))
        }
        Err(error) => Err(format!(
            "persona: cannot read {}: {error}; the persona is not in the system prompt",
            path.display()
        )),
    }
}

fn announce() -> &'static Regex {
    static ANNOUNCE: OnceLock<Regex> = OnceLock::new();
    ANNOUNCE.get_or_init(|| {
        Regex::new(
            r"(?i)\b(next,? I(?:'ll| will)|I(?:'ll| will) (?:now|next|then|start|run|add|fix|write|check)|then I(?:'ll| will)|let me|next step|going to|about to)\b",
        )
        .expect("the announce pattern compiles")
    })
}

/// The hook's `should_nudge`: the turn made a tool call and its final text's last paragraph (split on blank lines)
/// announces a next step instead of asking a question.
pub fn should_nudge(text: &str, tool_calls: usize) -> bool {
    if tool_calls == 0 || text.is_empty() {
        return false;
    }
    let tail = text.trim().rsplit("\n\n").next().unwrap_or("");
    if tail.trim_end().ends_with('?') {
        return false;
    }
    announce().is_match(tail)
}

/// How a root turn ended, for [`decide`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TurnEnd<'a> {
    /// The persona is on.
    pub persona: bool,
    /// The user (or a client) aborted the run.
    pub aborted: bool,
    /// The run ended on an error.
    pub failed: bool,
    /// A user message is queued, or other input waits for the lane.
    pub pending_input: bool,
    /// Tool (`rlm`) calls the turn made.
    pub tool_calls: usize,
    /// The final assistant text.
    pub text: &'a str,
    /// Nudges since the user last sent input.
    pub nudges: u32,
}

/// Whether the host sends a nudge after this turn.
pub fn decide(end: &TurnEnd) -> bool {
    end.persona
        && !end.aborted
        && !end.failed
        && !end.pending_input
        && end.nudges < MAX_NUDGES
        && should_nudge(end.text, end.tool_calls)
}

/// The nudge chain since the user's last input: how many ran, and what the current run did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Chain {
    pub nudges: u32,
    pub tool_calls: usize,
    pub text: String,
}

impl Chain {
    /// The user sent input: the count starts over.
    pub fn user_input(&mut self) {
        self.nudges = 0;
    }

    /// A run starts: its own calls and text.
    pub fn run_started(&mut self) {
        self.tool_calls = 0;
        self.text.clear();
    }

    /// An assistant message ended (a transcript message as stored): its tool calls count, and its text, when it has
    /// any, is the turn's final text so far.
    pub fn assistant_message(&mut self, message: &Value) {
        let blocks = message["content"].as_array().map(Vec::as_slice).unwrap_or_default();
        self.tool_calls += blocks.iter().filter(|b| b["type"] == "toolCall").count();
        let text = blocks
            .iter()
            .filter(|b| b["type"] == "text")
            .map(|b| b["text"].as_str().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n");
        let text = text.trim();
        if !text.is_empty() {
            self.text = text.to_string();
        }
    }

    /// The run ended: the nudge number to send (1-based), if one is due. An abort ends the chain until the user's
    /// next input.
    pub fn run_ended(&mut self, persona: bool, aborted: bool, failed: bool, pending_input: bool) -> Option<u32> {
        if aborted {
            self.nudges = MAX_NUDGES;
        }
        let due = decide(&TurnEnd {
            persona,
            aborted,
            failed,
            pending_input,
            tool_calls: self.tool_calls,
            text: &self.text,
            nudges: self.nudges,
        });
        due.then(|| {
            self.nudges += 1;
            self.nudges
        })
    }
}

/// The nudge as a transcript message: a displayed custom message the model reads as a user message.
pub fn nudge_message(n: u32, timestamp: i64) -> Value {
    serde_json::json!({
        "role": "custom",
        "customType": NUDGE_CUSTOM_TYPE,
        "content": NUDGE_TEXT,
        "display": true,
        "details": {"nudge": n, "max": MAX_NUDGES},
        "timestamp": timestamp,
    })
}

/// The hook's `last_turn` over a Claude Code transcript (JSONL): the final assistant text and the tool calls since
/// the last real user prompt. A line that is not JSON makes the whole transcript unreadable (`("", 0)`).
pub fn last_turn(jsonl: &str) -> (String, usize) {
    let mut entries = Vec::new();
    for line in jsonl.lines().filter(|l| !l.trim().is_empty()) {
        match serde_json::from_str::<Value>(line) {
            Ok(entry) => entries.push(entry),
            Err(_) => return (String::new(), 0),
        }
    }
    let (mut text, mut tools) = (String::new(), 0);
    for entry in entries.iter().rev() {
        let content = &entry["message"]["content"];
        if entry["type"] == "user" {
            let prompt = content.is_string()
                || content
                    .as_array()
                    .is_some_and(|blocks| blocks.iter().any(|b| b.is_object() && b["type"] == "text"));
            if prompt {
                break;
            }
            continue;
        }
        let Some(blocks) = content.as_array().filter(|_| entry["type"] == "assistant") else {
            continue;
        };
        tools += blocks.iter().filter(|b| b["type"] == "tool_use").count();
        if text.is_empty() {
            text = blocks
                .iter()
                .filter(|b| b["type"] == "text")
                .map(|b| b["text"].as_str().unwrap_or(""))
                .collect::<Vec<_>>()
                .join("\n")
                .trim()
                .to_string();
        }
    }
    (text, tools)
}

/// The persona section of the Claude root's prompt: the persona's text and where the pstack principles are.
pub fn claude_persona_text(personality: &str, bundled_skills: Option<&Path>) -> String {
    match bundled_skills {
        Some(dir) => format!(
            "{personality}\n\nThe pstack skills directory is {} (principle-<name>/SKILL.md).",
            dir.join("pstack").display()
        ),
        None => personality.to_string(),
    }
}
