//! The root agent's research-loop brake and procedural-memory hint (`ultron/tool-round-nudge.ts`), as session-worker.ts
//! wires them on `main`'s `turn_end`: after `ULTRON_TOOL_ROUNDS_NUDGE` consecutive tool rounds the root is steered to
//! answer (again, more firmly, at twice that); while subagents or tasks it started run, busy rounds are steered to wait
//! instead; after `ULTRON_SKILL_NUDGE` successful rounds it is told once to save the procedure as a code skill. The
//! steers are queued as steering messages; nothing is ever aborted.

use std::collections::{HashMap, HashSet};

/// `DEFAULT_TOOL_ROUNDS_NUDGE`.
pub const DEFAULT_TOOL_ROUNDS_NUDGE: u64 = 10;
/// `DEFAULT_WAIT_ROUNDS`.
pub const DEFAULT_WAIT_ROUNDS: u64 = 3;
/// `DEFAULT_SKILL_NUDGE`.
pub const DEFAULT_SKILL_NUDGE: u64 = 8;

/// `nudgeThreshold`: unset or off is 0 (disabled), `on` is `enabled`, a whole number is itself.
fn nudge_threshold(value: Option<&str>, enabled: u64) -> u64 {
    let raw = value.map(|v| v.trim().to_lowercase()).unwrap_or_default();
    if raw.is_empty() || ["off", "false", "no"].contains(&raw.as_str()) {
        return 0;
    }
    if ["on", "true", "yes"].contains(&raw.as_str()) {
        return enabled;
    }
    match raw.parse::<f64>() {
        Ok(n) if n.is_finite() && n >= 0.0 && n.fract() == 0.0 => n as u64,
        _ => 0,
    }
}

/// `toolRoundsNudgeFromEnv` (`ULTRON_TOOL_ROUNDS_NUDGE`).
pub fn tool_rounds_nudge_from_env(value: Option<&str>) -> u64 {
    nudge_threshold(value, DEFAULT_TOOL_ROUNDS_NUDGE)
}

/// `skillNudgeFromEnv` (`ULTRON_SKILL_NUDGE`).
pub fn skill_nudge_from_env(value: Option<&str>) -> u64 {
    nudge_threshold(value, DEFAULT_SKILL_NUDGE)
}

/// `nudgeMessage`.
pub fn nudge_message(rounds: u64, last: bool) -> String {
    if last {
        format!(
            "[Ultron] You have now used {rounds} rounds of tool calls in this turn. Stop calling tools and write your answer now from what you already have. List anything still unknown instead of searching further."
        )
    } else {
        format!(
            "[Ultron] You have used {rounds} rounds of tool calls in this turn without answering. Unless one more specific call is essential, stop gathering and answer now with what you have, noting what remains unknown."
        )
    }
}

/// `waitNudgeMessage`.
pub fn wait_nudge_message(running: u64, async_events: bool, next_call: bool) -> String {
    let what = if running == 1 {
        "1 subagent or task you started is".to_string()
    } else {
        format!("{running} subagents or tasks you started are")
    };
    if async_events && next_call {
        return format!(
            "[Ultron] {what} still running; each end is reported as a `child_done` (or `task_done`) event at the top of a later rlm result. Do only separate work of your own and do not check on them through their files, logs or progress. If nothing of your own is left, wait for free: `await rlm.collect(handles)` (nothing wakes you after you reply)."
        );
    }
    if async_events {
        format!(
            "[Ultron] {what} still running; each result comes to you as a `child_done` (or `task_done`) event. Do only separate work of your own and do not check on them through their files, logs or progress. If nothing of your own is left, wait for free: `await rlm.collect(handles)`, or end your turn and the events wake you."
        )
    } else {
        format!(
            "[Ultron] {what} still running. Do only separate work of your own and do not check on them through their files, logs or progress. If nothing of your own is left, wait for free with `await rlm.collect(handles)` (or `await agents.result(id)`)."
        )
    }
}

/// `skillNudgeMessage`.
pub fn skill_nudge_message(rounds: u64) -> String {
    format!(
        "[Ultron] {rounds} tool rounds in a row succeeded. If this procedure will recur, once it works save it as a tested skill with `await skills.propose_code(name, source, test_source, evidence)`; otherwise carry on."
    )
}

#[derive(Default)]
struct RunRounds {
    rounds: u64,
    waiting: u64,
    was_running: bool,
    wait_nudged: bool,
}

/// `ToolRoundNudger`: [`ToolRoundNudger::turn_ended`] answers the steer to send, if any.
pub struct ToolRoundNudger {
    threshold: u64,
    async_events: bool,
    wait_rounds: u64,
    next_call: bool,
    runs: HashMap<String, RunRounds>,
}

impl ToolRoundNudger {
    pub fn new(threshold: u64, async_events: bool) -> ToolRoundNudger {
        ToolRoundNudger {
            threshold,
            async_events,
            wait_rounds: DEFAULT_WAIT_ROUNDS,
            next_call: false,
            runs: HashMap::new(),
        }
    }

    /// `nextCall: true`: the wait steer says events arrive at the top of a later rlm result (an external root, whose
    /// runs nothing can start).
    pub fn with_next_call(mut self) -> ToolRoundNudger {
        self.next_call = true;
        self
    }

    /// One model turn of `run` ended with `tool_calls` calls while `running` root-started subagents or tasks run.
    pub fn turn_ended(&mut self, run: &str, tool_calls: usize, running: u64) -> Option<String> {
        if self.threshold == 0 {
            return None;
        }
        let state = self.runs.entry(run.to_string()).or_default();
        // A turn that answers without tools ends the streak, and so does the end of the last running child.
        state.rounds = if tool_calls == 0 {
            0
        } else if state.was_running && running == 0 {
            1
        } else {
            state.rounds + 1
        };
        if running == 0 {
            state.wait_nudged = false;
        }
        state.waiting = if running > 0 && tool_calls > 0 {
            state.waiting + 1
        } else {
            0
        };
        state.was_running = running > 0;
        let brake = state.rounds == self.threshold || state.rounds == self.threshold * 2;
        let wait = running > 0 && !state.wait_nudged && self.wait_rounds > 0 && state.waiting >= self.wait_rounds;
        if running > 0 && (brake || wait) {
            state.wait_nudged = true;
            return Some(wait_nudge_message(running, self.async_events, self.next_call));
        }
        brake.then(|| nudge_message(state.rounds, state.rounds == self.threshold * 2))
    }

    pub fn run_ended(&mut self, run: &str) {
        self.runs.remove(run);
    }
}

/// `SkillExtractionNudger`.
pub struct SkillExtractionNudger {
    threshold: u64,
    streaks: HashMap<String, u64>,
    nudged: HashSet<String>,
}

impl SkillExtractionNudger {
    pub fn new(threshold: u64) -> SkillExtractionNudger {
        SkillExtractionNudger {
            threshold,
            streaks: HashMap::new(),
            nudged: HashSet::new(),
        }
    }

    /// `tool_calls` made in the turn, `failed` of whose results were errors; `running` root-started work still running.
    pub fn turn_ended(&mut self, run: &str, tool_calls: usize, failed: usize, running: u64) -> Option<String> {
        if self.threshold == 0 || self.nudged.contains(run) {
            return None;
        }
        let streak = if tool_calls > 0 && failed == 0 && running == 0 {
            self.streaks.get(run).copied().unwrap_or(0) + 1
        } else {
            0
        };
        self.streaks.insert(run.to_string(), streak);
        if streak < self.threshold {
            return None;
        }
        self.nudged.insert(run.to_string());
        self.streaks.remove(run);
        Some(skill_nudge_message(streak))
    }

    pub fn run_ended(&mut self, run: &str) {
        self.streaks.remove(run);
        self.nudged.remove(run);
    }
}
