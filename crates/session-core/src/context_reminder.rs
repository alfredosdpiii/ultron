//! The context-pressure reminder (Rust-only; TypeScript has none): when a lane's context nears the point where the
//! harness compacts it, the lane's next `rlm` result ends with one `[context: ...]` line telling the model to free
//! space with `ctx` itself. Models given one reminder shortly before the budget edit their context; without one they
//! rarely do ("Context Language Models", arXiv 2609.37725).
//!
//! - The limit is the compaction threshold, `contextWindow - reserveTokens` (with auto-compaction off the same
//!   number, the usable window). The reminder triggers at `limit - margin`, where the margin is
//!   `max(2048, contextWindow / 10)`; `ULTRON_CONTEXT_REMINDER_TOKENS=<n>` sets it, `ULTRON_CONTEXT_REMINDER=off`
//!   (`0`, `false`) disables the reminder.
//! - The context use is the estimate the harness compacts by: the newest answer's usage plus estimates of the
//!   messages after it (the session actor wires that meter, [`ContextReminders::set_meter`]), plus this result
//!   (its text, and 4,800 characters per image as the compaction estimator counts one). An inference frame's lane
//!   (`rlm.infer` / `rlm.map` with depth) measures its own conversation the same way against the frame model's
//!   window ([`ContextReminders::set_frame_lane`]).
//! - Successful results, error results (a raised exception, a kernel failure) and the completion message of a cell
//!   that kept running in the background (`cell_done`) all carry it; an aborted cell does not.
//! - Once per approach: a lane that was reminded is not reminded again until its use fell below the trigger (after
//!   `ctx` edits) or its context was compacted.
//! - Every lane whose model has a known window, except lanes on Claude Code (`claude-code/*`, whose context is Claude
//!   Code's) and the external Claude Code root (no session actor, so no meter).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

/// The smallest margin before the limit (the paper's 2,048 tokens).
pub const MIN_MARGIN: u64 = 2048;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether reminders are on, and the margin the environment sets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReminderSettings {
    pub enabled: bool,
    pub margin: Option<u64>,
}

impl ReminderSettings {
    /// From `ULTRON_CONTEXT_REMINDER` (`off`, `0`, `false`: disabled) and `ULTRON_CONTEXT_REMINDER_TOKENS` (a
    /// non-negative integer; anything else keeps the default margin).
    pub fn from_vars(reminder: Option<&str>, tokens: Option<&str>) -> ReminderSettings {
        let enabled =
            !reminder.is_some_and(|value| matches!(value.trim().to_ascii_lowercase().as_str(), "off" | "0" | "false"));
        ReminderSettings {
            enabled,
            margin: tokens.and_then(|value| value.trim().parse::<u64>().ok()),
        }
    }
}

/// The margin before the limit: the environment's, else `max(2048, 10% of the context window)`.
pub fn margin(context_window: u64, configured: Option<u64>) -> u64 {
    configured.unwrap_or_else(|| MIN_MARGIN.max(context_window / 10))
}

/// Where a lane's context runs out: the compaction threshold, or with auto-compaction off the usable window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limit {
    pub context_window: u64,
    pub reserve_tokens: u64,
    /// Whether auto-compaction is on (it compacts at the limit).
    pub compaction: bool,
}

impl Limit {
    /// `contextWindow - reserveTokens`.
    pub fn tokens(&self) -> u64 {
        self.context_window.saturating_sub(self.reserve_tokens)
    }

    /// The use at which the reminder shows.
    pub fn trigger(&self, configured_margin: Option<u64>) -> u64 {
        self.tokens()
            .saturating_sub(margin(self.context_window, configured_margin))
    }
}

/// The limit of a lane on `model` whose compaction is `compaction` (on or off) with `reserve_tokens`; none on Claude
/// Code (`claude-code/*`), which manages its own context.
pub fn lane_limit(model: &ai::Model, compaction: bool, reserve_tokens: u64) -> Option<Limit> {
    (model.provider != ai::claude_code::PROVIDER).then_some(Limit {
        context_window: model.context_window,
        reserve_tokens,
        compaction,
    })
}

/// A token count as the reminder shows it: below 1,000 as is, else in thousands to one decimal (rounded down).
pub fn format_tokens(tokens: u64) -> String {
    if tokens < 1000 {
        return tokens.to_string();
    }
    let tenths = tokens / 100;
    match tenths % 10 {
        0 => format!("{}k", tenths / 10),
        decimal => format!("{}.{decimal}k", tenths / 10),
    }
}

/// The reminder line for `used` tokens of `limit`.
pub fn reminder_line(used: u64, limit: &Limit) -> String {
    let at = if limit.compaction {
        "before compaction"
    } else {
        "before the context limit"
    };
    format!(
        "[context: {} of {} tokens used {at}; free space with ctx now: await ctx.history() lists items with their bytes, await ctx.forget(ids, reason) or await ctx.summarize(ids, text) drops or shortens them]",
        format_tokens(used),
        format_tokens(limit.tokens())
    )
}

/// A lane's context use: `tokens`, and `epoch`, which changes when the context is compacted (the summary's entry).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Measure {
    pub tokens: u64,
    pub epoch: Option<u64>,
}

/// A lane's context use before its cell result, if known.
pub type ContextMeter = Arc<dyn Fn(&str) -> Option<Measure> + Send + Sync>;

/// One lane's own context use (an inference frame's conversation, which has no durable conversation).
pub type LaneMeter = Arc<dyn Fn() -> Option<u64> + Send + Sync>;

/// The characters an image counts as in the estimate, as the compaction estimator counts it (`estimateTokens`).
pub const ESTIMATED_IMAGE_CHARS: u64 = 4800;

#[derive(Default)]
struct LaneState {
    /// Reminded since the use last fell below the trigger or the context was compacted.
    reminded: bool,
    epoch: Option<u64>,
}

/// Every lane's reminder state, its limit and the meter of its context use.
pub struct ContextReminders {
    settings: ReminderSettings,
    limits: Mutex<HashMap<String, Limit>>,
    lanes: Mutex<HashMap<String, LaneState>>,
    meter: OnceLock<ContextMeter>,
    lane_meters: Mutex<HashMap<String, LaneMeter>>,
}

impl ContextReminders {
    pub fn new(settings: ReminderSettings) -> ContextReminders {
        ContextReminders {
            settings,
            limits: Mutex::default(),
            lanes: Mutex::default(),
            meter: OnceLock::new(),
            lane_meters: Mutex::default(),
        }
    }

    pub fn settings(&self) -> ReminderSettings {
        self.settings
    }

    /// Where the lane's context use comes from (the session actor's durable conversations).
    pub fn set_meter(&self, meter: ContextMeter) {
        let _ = self.meter.set(meter);
    }

    /// The limit of `lane` from now on: its model and compaction settings; `None` (a lane on Claude Code, a model
    /// without a known window): no reminders.
    pub fn set_limit(&self, lane: &str, limit: Option<Limit>) {
        let mut limits = lock(&self.limits);
        match limit.filter(|limit| limit.context_window > 0) {
            Some(limit) => limits.insert(lane.to_string(), limit),
            None => limits.remove(lane),
        };
    }

    /// `lane`'s own context meter (an in-memory subagent's loop, crate::memory_children), its limit unchanged;
    /// `None` returns it to the session's meter.
    pub fn set_lane_meter(&self, lane: &str, meter: Option<LaneMeter>) {
        match meter {
            Some(meter) => lock(&self.lane_meters).insert(lane.to_string(), meter),
            None => lock(&self.lane_meters).remove(lane),
        };
    }

    /// An inference frame's lane (crate::frames): its own meter (the frame conversation's estimate) and its limit,
    /// the frame model's window (frames never compact). `None` ends both (the frame ended).
    pub fn set_frame_lane(&self, lane: &str, frame: Option<(Limit, LaneMeter)>) {
        match frame {
            Some((limit, meter)) => {
                self.set_limit(lane, Some(limit));
                lock(&self.lane_meters).insert(lane.to_string(), meter);
            }
            None => {
                self.set_limit(lane, None);
                lock(&self.lane_meters).remove(lane);
                lock(&self.lanes).remove(lane);
            }
        }
    }

    pub fn limit(&self, lane: &str) -> Option<Limit> {
        lock(&self.limits).get(lane).copied()
    }

    /// The lane's context was compacted: its next result past the trigger reminds again.
    pub fn rearm(&self, lane: &str) {
        if let Some(state) = lock(&self.lanes).get_mut(lane) {
            state.reminded = false;
        }
    }

    /// The lane's use is `measure`: the reminder line when it reached the trigger and the lane was not reminded
    /// in this approach; below the trigger (or in a new epoch) the lane is re-armed.
    pub fn observe(&self, lane: &str, measure: Measure) -> Option<String> {
        if !self.settings.enabled {
            return None;
        }
        let limit = self.limit(lane)?;
        let mut lanes = lock(&self.lanes);
        let state = lanes.entry(lane.to_string()).or_default();
        if state.epoch != measure.epoch {
            state.epoch = measure.epoch;
            state.reminded = false;
        }
        if measure.tokens < limit.trigger(self.settings.margin) {
            state.reminded = false;
            return None;
        }
        if state.reminded {
            return None;
        }
        state.reminded = true;
        Some(reminder_line(measure.tokens, &limit))
    }

    /// The reminder for `lane`'s result `text` with `images` images: the meter's use plus the result's own estimate,
    /// as the harness estimates a tool result (`ceil(chars / 4)`, an image counting
    /// [`ESTIMATED_IMAGE_CHARS`]). A lane with its own meter (a frame) reads that one.
    pub fn reminder(&self, lane: &str, text: &str, images: usize) -> Option<String> {
        if !self.settings.enabled {
            return None;
        }
        let own = lock(&self.lane_meters).get(lane).cloned();
        let measure = match own {
            Some(meter) => Measure {
                tokens: meter()?,
                epoch: None,
            },
            None => (self.meter.get()?)(lane)?,
        };
        let chars = text.encode_utf16().count() as u64 + images as u64 * ESTIMATED_IMAGE_CHARS;
        self.observe(
            lane,
            Measure {
                tokens: measure.tokens + chars.div_ceil(4),
                epoch: measure.epoch,
            },
        )
    }
}
