//! A root agent that lives outside Ultron (external-root.ts): Claude Code calling the `rlm` tool over MCP. Ultron's
//! runtime is the same as for its own root lane (the session's host stack, kernel, file hooks, secret masking,
//! subagents, frames), but nothing here runs a model loop: Claude Code owns the conversation.
//!
//! - a cell is run directly ([`ExternalRoot::run_cell`]), not as a tool call of a harness run;
//! - a root turn is a Claude Code user turn, opened by the UserPromptSubmit hook ([`ExternalRoot::begin_turn`]) or
//!   by the first cell of a turn, and closed by the Stop hook ([`ExternalRoot::end_turn`]);
//! - completion events for root-owned work cannot start a run: they wait in an inbox and lead the next cell result,
//!   or go out with the next user prompt's hook context;
//! - the research-loop brake and the wait nudge, which steer a native run, are appended to the next cell result;
//! - a turn whose budget is spent refuses its cells.

use agent::AgentTool;
use ai::{AbortSignal, UserContent};
use serde_json::{Value, json};
use session_core::lane::{Lane, MAIN};
use session_core::nudge::{SkillExtractionNudger, ToolRoundNudger};
use session_core::session_stats::SessionStats;
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

/// What the runtime does for the root's turns (`ExternalRootDeps`: `host`, `usage`, `hints`, `fileHooks`), all
/// of it as the session worker does for its own root lane. The defaults do nothing.
pub trait RootHost: Send + Sync {
    /// `host.beginRootTurn`: work admitted while the turn is open is charged to the turn's own root.
    fn begin_root_turn(&self, _turn: &str) {}
    /// `host.endRootTurn`.
    fn end_root_turn(&self, _turn: &str) {}
    /// `host.rootIdOfRun`: the usage root of a turn.
    fn root_id_of_run(&self, turn: &str) -> String {
        format!("turn:{turn}")
    }
    /// `host.pendingRootNotifications`: subagents and tasks the root started that are still running.
    fn pending_root_notifications(&self) -> u64 {
        0
    }
    /// `usage.turnBudgetExhausted`: why the root's budget refuses more work, if it does.
    fn turn_budget_exhausted(&self, _root: &str) -> Option<String> {
        None
    }
    /// `hints.runEnded`: the lane's stuck-loop detection starts over.
    fn run_ended(&self, _lane: &str) {}
    /// `fileHooks.beginTurn`: the guards' per-turn times start from zero.
    fn begin_file_turn(&self) {}
    /// Claude Code cancelled a cell's call (Esc): stop the shell jobs and REPL calls started under the usage root
    /// `root`, as Esc on a native root turn does (`onRootAborted`).
    fn root_aborted(&self, _root: &str) {}
}

/// A host that does nothing (cells and turns only).
struct NoHost;
impl RootHost for NoHost {}

#[derive(Default)]
struct State {
    inbox: Vec<String>,
    dropped: usize,
    turns: u64,
    turn: Option<String>,
    cells: u64,
    /// Loki's one-time setup note, until it is shown.
    loki_notice: session_core::loki_notice::LokiNotice,
}

pub struct ExternalRoot {
    state: Mutex<State>,
    prefix: String,
    events: EventSource,
    runner: CellRunner,
    /// Loki's short policy note, the context a session starts with.
    loki_context: Option<String>,
    /// The session's counters: Claude Code's turns leave no run in the session, so their number and open time are
    /// counted here (`externalTurnStarted`/`externalTurnEnded`), with each steer appended and each refused cell.
    stats: Option<Arc<SessionStats>>,
    host: Arc<dyn RootHost>,
    /// The research-loop brake and wait nudge, and the procedural-memory hint (`ULTRON_TOOL_ROUNDS_NUDGE`,
    /// `ULTRON_SKILL_NUDGE`): a native run gets them as steers, an external root at the end of its next result.
    nudgers: Mutex<(ToolRoundNudger, SkillExtractionNudger)>,
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
            stats: None,
            host: Arc::new(NoHost),
            nudgers: Mutex::new((ToolRoundNudger::new(0, true), SkillExtractionNudger::new(0))),
        }
    }

    /// This root with its turns counted in `stats`.
    pub fn with_stats(mut self, stats: Arc<SessionStats>) -> ExternalRoot {
        self.stats = Some(stats);
        self
    }

    /// This root's turns driven through `host`.
    pub fn with_host(mut self, host: Arc<dyn RootHost>) -> ExternalRoot {
        self.host = host;
        self
    }

    /// The nudges' thresholds (0 disables one) and whether completion events are on (`asyncEvents`).
    pub fn with_nudges(self, tool_rounds: u64, skill: u64, async_events: bool) -> ExternalRoot {
        *self.nudgers.lock().unwrap_or_else(PoisonError::into_inner) = (
            ToolRoundNudger::new(tool_rounds, async_events).with_next_call(),
            SkillExtractionNudger::new(skill),
        );
        self
    }

    /// The host wakes the model when root work ends while it is idle (`ultron claude`'s wait-events hook): the wait
    /// steer says so instead of "nothing wakes you".
    pub fn waking(self) -> ExternalRoot {
        self.nudgers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .0
            .set_next_call(false);
        self
    }

    /// Loki's one-time setup note (`lokiNotice`): shown with the session context or the next user turn, once.
    pub fn with_loki_notice(self, notice: Option<String>) -> ExternalRoot {
        self.with_loki_setup(session_core::loki_notice::LokiNotice::known(
            notice.filter(|n| !n.is_empty()),
        ))
    }

    /// [`ExternalRoot::with_loki_notice`] for a note still being made (`.loki/` committing in the background): a
    /// commit still running is waited for briefly; otherwise the note comes with a later turn.
    pub fn with_loki_setup(self, notice: session_core::loki_notice::LokiNotice) -> ExternalRoot {
        self.lock().loki_notice = notice;
        self
    }

    /// Over a session's root lane: events through `sink` (the dispatcher's coalescing: shell-job and tool-call ends
    /// published into it by crate::external_lane::wire, and the lane's runtime events, forwarded as they arrive),
    /// cells through its `rlm` tool, turns through `host`, nudges as the environment sets them.
    pub fn over_lane(
        lane: Arc<Lane>,
        loki_context: Option<String>,
        sink: Arc<crate::root_sink::RootSink>,
        host: Arc<dyn RootHost>,
        var: &dyn Fn(&str) -> Option<String>,
    ) -> ExternalRoot {
        let stats = lane.stack.stats.clone();
        // Subagents', typed tasks' and detached cells' ends, forwarded until the root or its lane goes away (the
        // native session forwards them to its dispatcher the same way).
        let (forward_lane, forward_sink) = (Arc::downgrade(&lane), Arc::downgrade(&sink));
        std::thread::spawn(move || {
            loop {
                let (Some(lane), Some(sink)) = (forward_lane.upgrade(), forward_sink.upgrade()) else {
                    return;
                };
                for line in lane_event_lines(&lane) {
                    sink.publish(line);
                }
                drop((lane, sink));
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        });
        let events: EventSource = Arc::new(move || sink.take());
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
        let async_events = session_core::async_events::async_events_enabled(var("ULTRON_ASYNC_EVENTS").as_deref());
        ExternalRoot::new(events, runner, loki_context)
            .with_stats(stats)
            .with_host(host)
            .with_nudges(
                session_core::nudge::tool_rounds_nudge_from_env(var("ULTRON_TOOL_ROUNDS_NUDGE").as_deref()),
                session_core::nudge::skill_nudge_from_env(var("ULTRON_SKILL_NUDGE").as_deref()),
                async_events,
            )
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

    /// Loki's one-time setup note, the first time it is asked for.
    fn loki_notice(&self, state: &mut State) -> Option<String> {
        state.loki_notice.take(session_core::loki_notice::NOTICE_WAIT)
    }

    /// Loki's policy note and, once, its setup note: the context a session starts with.
    pub fn session_context(&self) -> Option<String> {
        let mut parts: Vec<String> = self.loki_context.iter().filter(|c| !c.is_empty()).cloned().collect();
        parts.extend(self.loki_notice(&mut self.lock()));
        (!parts.is_empty()).then(|| parts.join("\n\n"))
    }

    fn open_turn(&self, state: &mut State) -> String {
        state.turns += 1;
        let turn = format!("{}-{}", self.prefix, state.turns);
        state.turn = Some(turn.clone());
        if let Some(stats) = &self.stats {
            stats.external_turn_started(&turn);
        }
        self.host.begin_root_turn(&turn);
        self.host.begin_file_turn();
        turn
    }

    fn close_turn(&self, state: &mut State) {
        let Some(turn) = state.turn.take() else {
            return;
        };
        if let Some(stats) = &self.stats {
            stats.external_turn_ended(&turn);
        }
        self.host.end_root_turn(&turn);
        self.host.run_ended(MAIN);
        let mut nudgers = self.nudgers.lock().unwrap_or_else(PoisonError::into_inner);
        nudgers.0.run_ended(&turn);
        nudgers.1.run_ended(&turn);
    }

    /// A user turn starts (UserPromptSubmit): end the turn still open, open a root turn and return the context to
    /// add to the prompt, namely Loki's notice and the events that arrived since the last cell.
    pub fn begin_turn(&self) -> Option<String> {
        let mut parts = Vec::new();
        {
            let mut state = self.lock();
            self.close_turn(&mut state);
            self.open_turn(&mut state);
            parts.extend(self.loki_notice(&mut state));
        }
        if let Some(events) = self.take_events() {
            parts.push(format!("Runtime events since your last rlm call:\n{events}"));
        }
        (!parts.is_empty()).then(|| parts.join("\n\n"))
    }

    /// The turn ended (Stop): budget window, stuck-loop count.
    pub fn end_turn(&self) {
        let mut state = self.lock();
        self.close_turn(&mut state);
    }

    /// Run one cell in the root kernel: the rlm tool's own result, with waiting events first and notes last. A
    /// turn whose budget is spent refuses the cell.
    pub fn run_cell(&self, code: &str, signal: &AbortSignal) -> CellResult {
        let turn = {
            let mut state = self.lock();
            let turn = match state.turn.clone() {
                Some(turn) => turn,
                None => self.open_turn(&mut state),
            };
            state.cells += 1;
            turn
        };
        let lead = self.take_events().map(|e| format!("{e}\n")).unwrap_or_default();
        if let Some(exhausted) = self.host.turn_budget_exhausted(&self.host.root_id_of_run(&turn)) {
            if let Some(stats) = &self.stats {
                stats.usage_limit_block();
            }
            return CellResult {
                content: vec![
                    json!({"type": "text", "text": format!("{lead}[Ultron] {exhausted}; answer with what you have.")}),
                ],
                is_error: true,
            };
        }
        let (content, is_error) = match (self.runner)(code, signal) {
            Ok(content) => (content, false),
            Err(error) => (vec![json!({"type": "text", "text": error})], true),
        };
        // The call was cancelled: the cell was cancelled with it (the rlm tool's abort); what it started under this
        // turn stops too.
        if signal.aborted() {
            self.host.root_aborted(&self.host.root_id_of_run(&turn));
        }
        let running = self.host.pending_root_notifications();
        let notes: Vec<String> = {
            let mut nudgers = self.nudgers.lock().unwrap_or_else(PoisonError::into_inner);
            let (rounds, skill) = &mut *nudgers;
            [
                rounds.turn_ended(&turn, 1, running),
                skill.turn_ended(&turn, 1, usize::from(is_error), running),
            ]
            .into_iter()
            .flatten()
            .collect()
        };
        if let Some(stats) = &self.stats {
            for note in &notes {
                stats.nudge(note);
            }
        }
        let first = content
            .iter()
            .find(|part| part["type"] == "text")
            .and_then(|part| part["text"].as_str())
            .unwrap_or("(no result)");
        let tail = if notes.is_empty() {
            String::new()
        } else {
            format!("\n{}", notes.join("\n"))
        };
        let mut parts = vec![json!({"type": "text", "text": format!("{lead}{first}{tail}")})];
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

/// The `<runtime_event>` lines of the root lane's runtime events that have arrived (never waiting for running work).
fn lane_event_lines(lane: &Lane) -> Vec<String> {
    let now = AbortSignal::new();
    now.abort();
    lane.events
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
}
