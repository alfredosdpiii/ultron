//! Per-root turn, token and cost limits on every lane loop (session-worker.ts's `before_tool`, `before_request` and
//! `transform_context` budget hooks). Every model response on any lane (the root's own, subagents, typed agents,
//! other root conversations) is charged to the root that admitted it ([`super::durable_turns`]); once that root's tree
//! is spent:
//!
//! - a cell is refused with the limit text; at a turn or token limit the first refusal also says tools are off for
//!   the run's last reply (state `pending`), after which a refused cell ends the run;
//! - at a turn or token limit the run's next request is granted once as its final request: tool choice `none`, the
//!   `[Ultron]` note appended to its context (state `final`); every later request is refused;
//! - a cost cap is money: no final request, every cell and request is refused at once.
//!
//! Each refusal is counted (`sessionStats.usageLimitBlock()`), except the grant of the final request.

use delegation::usage::UsageLedger;
use durable::ConversationId;
use harness::chat::CellCall;
use harness::chat::gate::{GateRun, RequestGate, ToolBlock};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use crate::session_stats::SessionStats;

/// `FINAL_ANSWER_NOTE` of session-worker.ts.
pub const FINAL_ANSWER_NOTE: &str =
    "Tools are off for your last reply: answer now with what you have, and say what remains unknown or unfinished.";

/// Where a lane run stands after its tree's limit: a cell was refused (the next request is the final one), or the
/// final request was granted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FinalAnswer {
    Pending,
    Final,
}

/// One lane run (`finalKey(lane, runId)`): the lane (a conversation, a frame) and its run.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LaneRun {
    pub lane: String,
    pub run: String,
}

impl LaneRun {
    /// A run of a conversation of the durable runtime.
    pub fn of(run: &GateRun) -> LaneRun {
        LaneRun {
            lane: format!("conversation:{}", run.conversation_id.0),
            run: run.run.map_or_else(String::new, |run| run.to_string()),
        }
    }
}

/// What a lane run's next request is, once its tree is spent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RequestDecision {
    Allow,
    Final(String),
    Block(String),
}

/// The usage root a conversation's responses are charged to (`usageRootForLane`).
pub type RootOf = Arc<dyn Fn(ConversationId) -> Option<String> + Send + Sync>;

pub struct BudgetGate {
    usage: UsageLedger,
    stats: Arc<SessionStats>,
    root_of: RootOf,
    /// Per lane run (`finalAnswers`); a lane keeps only its newest run (`run_end` deletes an ended run's state).
    finals: Arc<Mutex<HashMap<LaneRun, FinalAnswer>>>,
}

/// Where lane runs stand after their tree's limit.
pub type Finals = Arc<Mutex<HashMap<LaneRun, FinalAnswer>>>;

/// The final-answer states of frame lanes (`frame:<lane>`; a frame's lane is named after its task, unique across
/// sessions).
pub fn frame_finals() -> Finals {
    static FINALS: std::sync::LazyLock<Finals> = std::sync::LazyLock::new(Finals::default);
    FINALS.clone()
}

impl BudgetGate {
    pub fn new(usage: UsageLedger, stats: Arc<SessionStats>, root_of: RootOf) -> Arc<BudgetGate> {
        BudgetGate::with_finals(usage, stats, root_of, Finals::default())
    }

    /// A gate over the run states `finals` (shared with other gates).
    pub fn with_finals(
        usage: UsageLedger,
        stats: Arc<SessionStats>,
        root_of: RootOf,
        finals: Finals,
    ) -> Arc<BudgetGate> {
        Arc::new(BudgetGate {
            usage,
            stats,
            root_of,
            finals,
        })
    }

    fn exhaustion(&self, root: Option<&str>) -> Option<delegation::usage::Exhaustion> {
        self.usage.budget_exhaustion(root).ok().flatten()
    }

    fn state(&self, run: &LaneRun) -> Option<FinalAnswer> {
        self.finals
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(run)
            .copied()
    }

    fn set(&self, run: &LaneRun, state: FinalAnswer) {
        let mut finals = self.finals.lock().unwrap_or_else(PoisonError::into_inner);
        finals.retain(|key, _| key.lane != run.lane || key == run);
        finals.insert(run.clone(), state);
    }

    /// `before_tool` of a cell of `run`, charged to `root`.
    pub fn tool(&self, run: &LaneRun, root: Option<&str>) -> Option<ToolBlock> {
        let exhausted = self.exhaustion(root)?;
        self.stats.usage_limit_block();
        if exhausted.kind != "cost" && self.state(run) != Some(FinalAnswer::Final) {
            self.set(run, FinalAnswer::Pending);
            return Some(ToolBlock {
                reason: format!("{} {FINAL_ANSWER_NOTE}", exhausted.reason),
                terminate: false,
            });
        }
        Some(ToolBlock {
            reason: exhausted.reason,
            terminate: true,
        })
    }

    /// `before_request` (step `assistant`) of `run`, with the final request's `transform_context` note.
    pub fn request(&self, run: &LaneRun, root: Option<&str>) -> RequestDecision {
        let Some(exhausted) = self.exhaustion(root) else {
            return RequestDecision::Allow;
        };
        if exhausted.kind != "cost" && self.state(run) != Some(FinalAnswer::Final) {
            self.set(run, FinalAnswer::Final);
            return RequestDecision::Final(format!("[Ultron] {} {FINAL_ANSWER_NOTE}", exhausted.reason));
        }
        self.stats.usage_limit_block();
        RequestDecision::Block(exhausted.reason)
    }

    /// The tool gate of a durable conversation's run.
    pub fn before_tool(&self, run: &GateRun, _call: &CellCall) -> Option<ToolBlock> {
        let root = (self.root_of)(run.conversation_id);
        self.tool(&LaneRun::of(run), root.as_deref())
    }

    /// The request gate of a durable conversation's run.
    pub fn before_request(&self, run: &GateRun) -> RequestGate {
        let root = (self.root_of)(run.conversation_id);
        match self.request(&LaneRun::of(run), root.as_deref()) {
            RequestDecision::Allow => RequestGate::Allow,
            RequestDecision::Final(note) => RequestGate::Final { note },
            RequestDecision::Block(reason) => RequestGate::Block { reason },
        }
    }
}
