//! The per-session persona toggle and keep-going mode (Rust-only, no TypeScript reference).
//!
//! - The session's own toggle (`on`, `off`, `auto`: follow the settings' `persona`) overrides the global setting.
//! - A user prompt that asks the agent to keep working unattended ([`trigger`]: "keep going", "don't stop until you
//!   finish", "I'm going to sleep", ...) turns the persona on for the session and starts keep-going mode.
//! - Keep-going mode ([`after_run`]): after a run that did work (an `rlm` call), the host continues it with the
//!   keep-going nudge, whatever the run's last paragraph says and with no 5-nudge cap; a run that ends with a question
//!   is told that the user is away. It ends when a run makes no `rlm` call (that run is the final report), on user
//!   input or an abort, when the user turns it off, or at the ceiling ([`Limits`]: 100 continues or 8 hours), where a
//!   last nudge asks for the final report.
//!
//! The state is one session value (`ultron.module/persona`, [`SessionPersona::to_json`]), so a resumed session (a
//! restarted durable instance, a crashed worker) keeps the toggle, the mode and its counters.

use regex::Regex;
use serde_json::{Value, json};
use std::sync::OnceLock;

/// The session value's namespace and key.
pub const STATE_NAMESPACE: &str = "ultron.module";
pub const STATE_KEY: &str = "persona";
/// Continues allowed in one keep-going stretch.
pub const MAX_CONTINUES: u32 = 100;
/// How long one keep-going stretch may run.
pub const MAX_DURATION_MS: i64 = 8 * 60 * 60 * 1000;
/// Overrides of the ceiling (tests, and a user who wants another one): a count and milliseconds.
pub const ENV_MAX_CONTINUES: &str = "ULTRON_KEEPGOING_MAX_CONTINUES";
pub const ENV_MAX_MS: &str = "ULTRON_KEEPGOING_MAX_MS";

/// What the keep-going nudge says after a run that did work.
pub const KEEP_GOING_TEXT: &str = "Keep going: the user is away and asked you to work until the task is done. Take the next step now. Skip irreversible or outward actions (pushing, publishing, deploying, deleting data, sending messages, spending money) and never start schedules: list them for the user in your final report instead of asking. When the work is done and verified, write the final report and make no further rlm call.";
/// The keep-going nudge after a run that ended with a question to the user.
pub const KEEP_GOING_ASKED_TEXT: &str = "The user is away and cannot answer your question. Decide yourself: take the safest reversible option, record the decision and the alternative in your final report, and continue. Skip irreversible or outward actions (pushing, publishing, deploying, deleting data, sending messages, spending money) and never start schedules: list them for the user in your final report instead of asking. When the work is done and verified, write the final report and make no further rlm call.";
/// The last nudge at the ceiling.
pub const FINAL_TEXT: &str = "Keep-going limit reached: stop working now and write the final report: what was done, what was verified, the decisions you made with their alternatives, and the irreversible or outward actions left for the user. Make no further rlm call.";
/// What the model is told when a prompt starts keep-going mode (`ultron claude`'s UserPromptSubmit context).
pub const START_TEXT: &str = "Keep-going mode is on: the user is away. Work until the task is done and verified without waiting for answers; decide open questions yourself and record each decision and its alternative for the final report. Skip irreversible or outward actions (pushing, publishing, deploying, deleting data, sending messages, spending money) and never start schedules: list them for the user in the final report instead of asking.";

/// The session's own persona choice.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Toggle {
    /// Follow the settings' `persona`.
    #[default]
    Auto,
    On,
    Off,
}

impl Toggle {
    pub fn name(self) -> &'static str {
        match self {
            Toggle::Auto => "auto",
            Toggle::On => "on",
            Toggle::Off => "off",
        }
    }
}

/// One keep-going stretch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeepGoing {
    /// When it started (ms since the epoch).
    pub started_at: i64,
    /// Keep-going nudges sent.
    pub continues: u32,
    /// The phrase that started it (or `/keepgoing on`).
    pub reason: String,
    /// The run in progress made an `rlm` call (kept so a run resumed after a restart still counts its work).
    pub worked: bool,
    /// `ultron claude`: the tool calls Claude Code's transcript held at the last decision (a run did work when the
    /// count grew); `None` before the first.
    pub tool_calls_seen: Option<u64>,
}

/// The session's persona state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionPersona {
    pub toggle: Toggle,
    pub keep_going: Option<KeepGoing>,
}

impl SessionPersona {
    /// The session value; anything unreadable is the default (auto, no keep-going).
    pub fn from_json(value: Option<&Value>) -> SessionPersona {
        let Some(value) = value.filter(|v| v.is_object()) else {
            return SessionPersona::default();
        };
        let toggle = match value["persona"].as_str() {
            Some("on") => Toggle::On,
            Some("off") => Toggle::Off,
            _ => Toggle::Auto,
        };
        let keep_going = value["keepGoing"].as_object().map(|k| KeepGoing {
            started_at: k.get("startedAt").and_then(Value::as_i64).unwrap_or(0),
            continues: k
                .get("continues")
                .and_then(Value::as_u64)
                .map_or(0, |n| u32::try_from(n).unwrap_or(u32::MAX)),
            reason: k.get("reason").and_then(Value::as_str).unwrap_or_default().to_string(),
            worked: k.get("worked") == Some(&Value::Bool(true)),
            tool_calls_seen: k.get("toolCallsSeen").and_then(Value::as_u64),
        });
        SessionPersona { toggle, keep_going }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "version": 1,
            "persona": self.toggle.name(),
            "keepGoing": self.keep_going.as_ref().map(|k| json!({
                "startedAt": k.started_at,
                "continues": k.continues,
                "reason": k.reason,
                "worked": k.worked,
                "toolCallsSeen": k.tool_calls_seen,
            })),
        })
    }

    /// Whether the persona is on for this session, given the settings' `persona` (`global`).
    pub fn effective(&self, global: bool) -> bool {
        match self.toggle {
            Toggle::On => true,
            Toggle::Off => false,
            Toggle::Auto => global,
        }
    }

    /// The session's prompt-mode override: `None` follows the settings.
    pub fn prompt_override(&self) -> Option<bool> {
        match self.toggle {
            Toggle::On => Some(true),
            Toggle::Off => Some(false),
            Toggle::Auto => None,
        }
    }

    /// A user's input (a prompt, a queued message) at `now`: its keep-going phrase turns the persona on and starts
    /// keep-going mode; any other input ends a stretch in progress. The one-line notice for the user, if any.
    pub fn user_input(&mut self, text: &str, now: i64) -> Option<String> {
        if let Some(phrase) = trigger(text) {
            self.toggle = Toggle::On;
            let notice = format!("keep-going on: \"{phrase}\" (/keepgoing off ends it)");
            self.keep_going = Some(KeepGoing {
                started_at: now,
                continues: 0,
                reason: phrase,
                worked: false,
                tool_calls_seen: None,
            });
            return Some(notice);
        }
        self.keep_going
            .take()
            .map(|_| "keep-going off: you sent input".to_string())
    }

    /// The run made an `rlm` call; `true` when the stored state changed.
    pub fn worked(&mut self) -> bool {
        match &mut self.keep_going {
            Some(k) if !k.worked => {
                k.worked = true;
                true
            }
            _ => false,
        }
    }

    /// The status line `/persona` and `/keepgoing` answer with.
    pub fn status(&self, global: bool, limits: Limits) -> String {
        let persona = match self.toggle {
            Toggle::Auto => format!(
                "Persona for this session follows the settings ({})",
                if global { "on" } else { "off" }
            ),
            Toggle::On => "Persona is on for this session".to_string(),
            Toggle::Off => "Persona is off for this session".to_string(),
        };
        let keep_going = match &self.keep_going {
            Some(k) => format!(
                "keep-going on ({}/{} continues, \"{}\")",
                k.continues, limits.max_continues, k.reason
            ),
            None => "keep-going off".to_string(),
        };
        format!("{persona}; {keep_going}")
    }

    /// `/persona on|off|auto` and `/keepgoing on|off` (`command`: `persona` or `keepgoing`; empty `args` reports).
    /// Turning the persona off ends keep-going too. Errors are the usage text.
    pub fn command(
        &mut self,
        command: &str,
        args: &str,
        now: i64,
        global: bool,
        limits: Limits,
    ) -> Result<String, String> {
        let arg = args.trim().to_lowercase();
        match (command, arg.as_str()) {
            ("persona", "") | ("keepgoing", "") => {}
            ("persona", "on") => self.toggle = Toggle::On,
            ("persona", "off") => {
                self.toggle = Toggle::Off;
                self.keep_going = None;
            }
            ("persona", "auto") => self.toggle = Toggle::Auto,
            ("keepgoing", "on") => {
                self.toggle = Toggle::On;
                if self.keep_going.is_none() {
                    self.keep_going = Some(KeepGoing {
                        started_at: now,
                        continues: 0,
                        reason: "/keepgoing on".into(),
                        worked: false,
                        tool_calls_seen: None,
                    });
                }
            }
            ("keepgoing", "off") => self.keep_going = None,
            ("persona", _) => return Err("Usage: /persona on|off|auto (this session)".into()),
            _ => return Err("Usage: /keepgoing on|off".into()),
        }
        Ok(self.status(global, limits))
    }
}

/// `/persona <args>` or `/keepgoing <args>` as a whole prompt (with or without the slash): `(command, args)`.
pub fn toggle_command(prompt: &str) -> Option<(&'static str, String)> {
    let text = prompt.trim();
    let text = text.strip_prefix('/').unwrap_or(text);
    let (head, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    let command = match head.to_lowercase().as_str() {
        "persona" => "persona",
        "keepgoing" | "keep-going" => "keepgoing",
        _ => return None,
    };
    let args = rest.trim().to_lowercase();
    let known = match command {
        "persona" => matches!(args.as_str(), "on" | "off" | "auto" | ""),
        _ => matches!(args.as_str(), "on" | "off" | ""),
    };
    // Without the slash only an explicit argument makes it a command ("persona" alone is a word).
    (known && (prompt.trim().starts_with('/') || !args.is_empty())).then_some((command, args))
}

/// The keep-going ceiling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub max_continues: u32,
    pub max_ms: i64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_continues: MAX_CONTINUES,
            max_ms: MAX_DURATION_MS,
        }
    }
}

impl Limits {
    /// The ceiling with [`ENV_MAX_CONTINUES`] / [`ENV_MAX_MS`] over the defaults (unparsable values are ignored).
    pub fn from_env(var: &dyn Fn(&str) -> Option<String>) -> Limits {
        let defaults = Limits::default();
        Limits {
            max_continues: var(ENV_MAX_CONTINUES)
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(defaults.max_continues),
            max_ms: var(ENV_MAX_MS)
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(defaults.max_ms),
        }
    }
}

/// How a root run ended, for [`after_run`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunEnd<'a> {
    pub aborted: bool,
    pub failed: bool,
    /// A user message is queued, or other input waits for the lane.
    pub pending_input: bool,
    /// `rlm` calls the run made.
    pub tool_calls: usize,
    /// The run's final assistant text.
    pub text: &'a str,
}

/// What follows a run in keep-going mode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Next {
    /// Not in keep-going mode: the persona's own nudge rule decides.
    Persona,
    /// Nothing (the mode ended, or input waits).
    Stop,
    /// Continue with keep-going nudge `n`; `asked`: the run ended with a question.
    Continue { n: u32, asked: bool },
    /// The ceiling: the last nudge, asking for the final report (the mode has ended).
    Final { n: u32 },
}

/// The keep-going decision after a root run at `now`. Changes `state` (counters, the end of the mode).
pub fn after_run(state: &mut SessionPersona, end: &RunEnd, now: i64, limits: Limits) -> Next {
    let Some(stretch) = state.keep_going.as_mut() else {
        return Next::Persona;
    };
    if end.aborted || end.failed {
        state.keep_going = None;
        return Next::Stop;
    }
    if end.pending_input {
        // The waiting input runs next; a user's message ends the mode when it arrives.
        return Next::Stop;
    }
    if end.tool_calls == 0 && !stretch.worked {
        // A run that only reported: the final report.
        state.keep_going = None;
        return Next::Stop;
    }
    if stretch.continues >= limits.max_continues || now.saturating_sub(stretch.started_at) >= limits.max_ms {
        let n = stretch.continues + 1;
        state.keep_going = None;
        return Next::Final { n };
    }
    stretch.continues += 1;
    stretch.worked = false;
    Next::Continue {
        n: stretch.continues,
        asked: asks(end.text),
    }
}

/// Whether the text's last paragraph ends with a question.
pub fn asks(text: &str) -> bool {
    text.trim()
        .rsplit("\n\n")
        .next()
        .unwrap_or("")
        .trim_end()
        .ends_with('?')
}

/// The text of a keep-going nudge.
pub fn nudge_text(next: &Next) -> Option<&'static str> {
    match next {
        Next::Continue { asked: false, .. } => Some(KEEP_GOING_TEXT),
        Next::Continue { asked: true, .. } => Some(KEEP_GOING_ASKED_TEXT),
        Next::Final { .. } => Some(FINAL_TEXT),
        Next::Persona | Next::Stop => None,
    }
}

/// A keep-going nudge as a transcript message (the persona nudge's custom type, `details.keepGoing`).
pub fn nudge_message(next: &Next, limits: Limits, timestamp: i64) -> Option<Value> {
    let text = nudge_text(next)?;
    let (n, last) = match next {
        Next::Continue { n, .. } => (*n, false),
        Next::Final { n } => (*n, true),
        _ => return None,
    };
    Some(json!({
        "role": "custom",
        "customType": crate::persona::NUDGE_CUSTOM_TYPE,
        "content": text,
        "display": true,
        "details": {"nudge": n, "max": limits.max_continues, "keepGoing": true, "final": last},
        "timestamp": timestamp,
    }))
}

fn patterns() -> &'static [Regex; 5] {
    static PATTERNS: OnceLock<[Regex; 5]> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            // "don't stop", "do not stop", "don't ever stop": what follows is checked by `stop_tail`.
            Regex::new(r"\b(?:don'?t|do not)(?: ever)? stop\b").expect("pattern"),
            // "keep going", "keep on going".
            Regex::new(r"\bkeep (?:on )?going\b").expect("pattern"),
            // "continue until you're done", "keep working till it's finished", "work until done".
            Regex::new(
                r"\b(?:continue|carry on|go on|keep working|work|keep at it) (?:until|till|til|'til|untill) (?:(?:you(?:'re| are)?|it(?:'s| is)|everything(?:'s| is)|the (?:work|task|job)s? (?:is|are)) )?(?:all )?(?:done|finished|complete|completed|finish|finishing|through)\b",
            )
            .expect("pattern"),
            // "work through the night", "work overnight".
            Regex::new(r"\bwork (?:through the night|all night|overnight)\b").expect("pattern"),
            // The user says they will be away: "I'm going to sleep", "im gonna sleep", "I'll be away".
            Regex::new(
                r"\bi(?:'m| am|m) (?:going to|gonna|off to|about to|heading to) (?:go to )?(?:sleep|bed)\b|\bi(?:'m| am|m) (?:going|heading) to bed\b|\bi(?:'ll| will|ll) be (?:away|afk|offline|asleep|gone|out)\b|\bi(?:'m| am|m) (?:away|afk|stepping away|heading out|logging off)\b",
            )
            .expect("pattern"),
        ]
    })
}

/// Words that may come before an instruction in its clause and still leave it addressed to the agent ("please
/// keep going", "ok, just keep going", "can you work through the night").
const ADDRESS_WORDS: [&str; 30] = [
    "please",
    "pls",
    "plz",
    "just",
    "and",
    "so",
    "then",
    "ok",
    "okay",
    "now",
    "also",
    "but",
    "simply",
    "you",
    "can",
    "could",
    "should",
    "must",
    "will",
    "to",
    "alright",
    "yes",
    "yeah",
    "great",
    "good",
    "thanks",
    "hey",
    "ultron",
    "really",
    "seriously",
];

/// What may follow "don't stop" for it to mean "don't stop working" (not "don't stop the server").
const STOP_TAIL_WORDS: [&str; 16] = [
    "until", "till", "til", "'til", "untill", "unless", "working", "now", "at", "even", "no", "for", "and", "or",
    "yet", "there",
];

/// Whether the words before the match in its clause leave it addressed to the agent: after the clause's last
/// `and` / `then` (an instruction joined to another, "fix it and keep going"), only [`ADDRESS_WORDS`] may come
/// first, so "the loop should keep going" or "why do you keep going back" are not instructions.
fn addressed(prefix: &str) -> bool {
    let prefix = prefix.trim();
    let prefix = [
        "i want you to",
        "i need you to",
        "i'd like you to",
        "i would like you to",
    ]
    .iter()
    .find_map(|lead| prefix.strip_suffix(lead).map(str::trim))
    .unwrap_or(prefix);
    let words: Vec<&str> = prefix
        .split_whitespace()
        .map(|word| word.trim_matches(|c: char| !c.is_alphanumeric() && c != '\''))
        .collect();
    let start = words
        .iter()
        .rposition(|word| matches!(*word, "and" | "then"))
        .map_or(0, |index| index + 1);
    words[start..].iter().all(|word| ADDRESS_WORDS.contains(word))
}

fn stop_tail(rest: &str) -> bool {
    match rest.split_whitespace().next() {
        None => true,
        Some(word) => STOP_TAIL_WORDS.contains(&word),
    }
}

/// The keep-going phrase a user prompt contains, addressed to the agent (lowercased, as written), if any.
pub fn trigger(prompt: &str) -> Option<String> {
    let text = prompt.to_lowercase().replace(['\u{2019}', '\u{2018}', '`'], "'");
    for clause in text.split(['.', '!', '?', ';', ':', ',', '\n', '(', ')', '"']) {
        let clause = clause.split_whitespace().collect::<Vec<_>>().join(" ");
        for (index, pattern) in patterns().iter().enumerate() {
            let Some(found) = pattern.find(&clause) else { continue };
            let (before, rest) = (&clause[..found.start()], &clause[found.end()..]);
            let ok = match index {
                0 => addressed(before) && stop_tail(rest),
                // The user's own away message is about them, not an instruction.
                4 => true,
                _ => addressed(before),
            };
            if ok {
                let phrase = match index {
                    0 => format!("{}{}", found.as_str(), rest).trim().to_string(),
                    _ => found.as_str().to_string(),
                };
                return Some(phrase);
            }
        }
    }
    None
}
