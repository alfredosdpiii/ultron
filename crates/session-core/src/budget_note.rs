//! The budget line on a cell result (session-worker.ts `cellBudgetNote`, usage.ts `budgetLine`): what the root turn
//! that a lane's work is charged to has left under its turn and token caps, appended to every `rlm` result of every
//! lane in every mode (print, JSON, RPC and interactive sessions all run through the session actor).

use crate::lane::MAIN;
use crate::subagents::DurableChildren;
use delegation::agents::Agents;
use delegation::usage::{UsageLedger, budget_line};
use std::sync::Arc;

/// The budget line for a cell result on `lane`; `None` when there is nothing to show.
pub type BudgetNote = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// `budgetLine(await usage.remaining(host.usageRootForLane(lane, operationId)), maxTotalTokens !== undefined)`: the
/// root lane's cells are charged to the root turn in progress, a subagent's to the turn that started it, a typed
/// agent's lane to its task's root. A ledger that cannot be read shows nothing (TypeScript's `.catch`).
pub fn budget_note(children: Arc<DurableChildren>, agents: Agents, usage: UsageLedger) -> BudgetNote {
    let explicit_token_cap = usage.limits().max_total_tokens.is_some();
    Arc::new(move |lane| {
        let root = if lane == MAIN {
            children.current_root()
        } else {
            children.usage_root_of_lane(lane, &agents)
        };
        let remaining = usage.remaining(root.as_deref()).ok()?;
        budget_line(&remaining, explicit_token_cap)
    })
}
