//! A root agent that lives outside Ultron (external-root.ts): Claude Code calling the `rlm` tool over MCP. Ultron's
//! runtime is the same as for its own root lane (the session's host stack, kernel, file hooks, secret masking,
//! subagents, frames), but nothing here runs a model loop: Claude Code owns the conversation.
//!
//! - a cell is run directly ([`ExternalRoot::run_cell`]), not as a tool call of a harness run;
//! - a root turn is a Claude Code user turn, opened by the UserPromptSubmit hook ([`ExternalRoot::begin_turn`]) or
//!   by the first cell of a turn, and closed by the Stop hook ([`ExternalRoot::end_turn`]);
//! - completion events for root-owned work cannot start a run: they wait in an inbox and lead the next cell result,
//!   or go out with the next user prompt's hook context.

use agent::AgentTool;
use ai::{AbortSignal, UserContent};
use serde_json::{Value, json};
use session_core::lane::Lane;
use std::sync::{Arc, Mutex, PoisonError};

/// At most this many events are kept waiting; older ones are dropped with a count.
pub const MAX_INBOX: usize = 64;

/// One cell's result as MCP content.
#[derive(Clone, Debug, PartialEq)]
pub struct CellResult {
    pub content: Vec<Value>,
    pub is_error: bool,
}

/// Where waiting completion lines come from (the lane's runtime events, or a test source).
pub type EventSource = Arc<dyn Fn() -> Vec<String> + Send + Sync>;
/// Runs one cell's code in the root lane: the `rlm` tool's content, or the error it threw.
pub type CellRunner = Arc<dyn Fn(&str, &AbortSignal) -> Result<Vec<Value>, String> + Send + Sync>;

#[derive(Default)]
struct State {
    inbox: Vec<String>,
    dropped: usize,
    turns: u64,
    turn: Option<String>,
    cells: u64,
}

pub struct ExternalRoot {
    state: Mutex<State>,
    prefix: String,
    events: EventSource,
    runner: CellRunner,
    /// Loki's short policy note, the context a session starts with.
    loki_context: Option<String>,
}

impl ExternalRoot {
    pub fn new(events: EventSource, runner: CellRunner, loki_context: Option<String>) -> ExternalRoot {
        ExternalRoot {
            state: Mutex::default(),
            prefix: format!(
                "cc-{}-{}",
                crate::launcher::base36(session_core::lane::now_ms() as u64),
                crate::random_hex(4)
            ),
            events,
            runner,
            loki_context,
        }
    }

    /// Over a session's root lane: events from its runtime events, cells through its `rlm` tool.
    pub fn over_lane(lane: Arc<Lane>, loki_context: Option<String>) -> ExternalRoot {
        let events_lane = lane.clone();
        let events: EventSource = Arc::new(move || {
            // An aborted signal: take what waits now, never wait for running work.
            let now = AbortSignal::new();
            now.abort();
            events_lane
                .events
                .next_message(&now)
                .into_iter()
                .filter_map(|message| match message {
                    ai::Message::User(user) => match user.content {
                        ai::UserMessageContent::Blocks(blocks) => Some(
                            blocks
                                .into_iter()
                                .filter_map(|b| match b {
                                    UserContent::Text(t) => Some(t.text),
                                    UserContent::Image(_) => None,
                                })
                                .collect::<Vec<_>>()
                                .join("\n"),
                        ),
                        ai::UserMessageContent::Text(text) => Some(text),
                    },
                    _ => None,
                })
                .flat_map(|text| text.lines().map(str::to_string).collect::<Vec<_>>())
                .collect()
        });
        let runner: CellRunner = Arc::new(move |code: &str, signal: &AbortSignal| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?;
            let update = |_| {};
            let result =
                runtime.block_on(
                    lane.tool
                        .execute("external-root", json!({"code": code}), signal, &update),
                )?;
            Ok(result
                .content
                .iter()
                .filter_map(|part| serde_json::to_value(part).ok())
                .collect())
        });
        ExternalRoot::new(events, runner, loki_context)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Move completions that arrived into the inbox (the async dispatcher's sink).
    fn pull(&self, state: &mut State) {
        state.inbox.extend((self.events)());
        while state.inbox.len() > MAX_INBOX {
            state.inbox.remove(0);
            state.dropped += 1;
        }
    }

    /// Root events waiting for delivery.
    pub fn pending_events(&self) -> usize {
        let mut state = self.lock();
        self.pull(&mut state);
        state.inbox.len()
    }

    /// The current root turn id, if one is open.
    pub fn turn(&self) -> Option<String> {
        self.lock().turn.clone()
    }

    pub fn cells(&self) -> u64 {
        self.lock().cells
    }

    /// Waiting events as `<runtime_event>` lines (and clears them), or `None` when none wait.
    pub fn take_events(&self) -> Option<String> {
        let mut state = self.lock();
        self.pull(&mut state);
        if state.inbox.is_empty() && state.dropped == 0 {
            return None;
        }
        let lines = std::mem::take(&mut state.inbox).join("\n");
        let dropped = std::mem::take(&mut state.dropped);
        Some(if dropped == 0 {
            lines
        } else {
            format!(
                "{lines}{}[{dropped} older runtime event(s) dropped; `await agents.status()` lists all work]",
                if lines.is_empty() { "" } else { "\n" }
            )
        })
    }

    /// Loki's policy note: the context a session starts with.
    pub fn session_context(&self) -> Option<String> {
        self.loki_context.clone().filter(|c| !c.is_empty())
    }

    fn open_turn(&self, state: &mut State) -> String {
        state.turns += 1;
        let turn = format!("{}-{}", self.prefix, state.turns);
        state.turn = Some(turn.clone());
        turn
    }

    /// A user turn starts (UserPromptSubmit): open a root turn and return the events that arrived since the last
    /// cell as context for the prompt.
    pub fn begin_turn(&self) -> Option<String> {
        {
            let mut state = self.lock();
            state.turn = None;
            self.open_turn(&mut state);
        }
        self.take_events()
            .map(|events| format!("Runtime events since your last rlm call:\n{events}"))
    }

    /// The turn ended (Stop).
    pub fn end_turn(&self) {
        self.lock().turn = None;
    }

    /// Run one cell in the root kernel: the rlm tool's own result, with waiting events first.
    pub fn run_cell(&self, code: &str, signal: &AbortSignal) -> CellResult {
        {
            let mut state = self.lock();
            if state.turn.is_none() {
                self.open_turn(&mut state);
            }
            state.cells += 1;
        }
        let lead = self.take_events().map(|e| format!("{e}\n")).unwrap_or_default();
        let (content, is_error) = match (self.runner)(code, signal) {
            Ok(content) => (content, false),
            Err(error) => (vec![json!({"type": "text", "text": error})], true),
        };
        let first = content
            .iter()
            .find(|part| part["type"] == "text")
            .and_then(|part| part["text"].as_str())
            .unwrap_or("(no result)");
        let mut parts = vec![json!({"type": "text", "text": format!("{lead}{first}")})];
        parts.extend(
            content
                .iter()
                .filter(|part| part["type"] == "image" && part["data"].is_string() && part["mimeType"].is_string())
                .cloned(),
        );
        CellResult {
            content: parts,
            is_error,
        }
    }
}
