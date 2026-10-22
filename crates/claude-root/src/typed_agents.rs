//! Typed agents' `rlm` lanes under `ultron claude` (`agents.invoke`/`agents.spawn` of a definition whose strategy is
//! `rlm`, `instances.invoke` of a retained one), run as child conversations of the root server's durable runtime
//! (session_core::subagents::DurableChildren::run_lane, as in a native session; crate::durable_apis).
//!
//! The model: the request's, else the definition's (the typed-agent host passes either as the run's model), else the
//! Claude root's child model ([`ChildModel`]: crate::mcp_server::ultron_child_model, ending at the root's own
//! subscription model, or a `--children claude` root's Claude Code child model). A retained instance's next
//! invocation continues its lane's conversation, on any model: a `claude-code/<id>` lane declares `rlm`, so it runs
//! on the Claude subscription inside that conversation's transcript (ai::claude_subscription::claude_code_lane).

use delegation::agents::{LaneReply, LaneRun, LaneRunner};
use session_core::compose::{AgentLanes, Stack};
use session_core::subagents::DurableChildren;
use std::sync::{Arc, OnceLock, Weak};

/// The model typed agents run on when neither the request nor the definition names one (`provider/model`), set once
/// the server knows its settings.
pub type ChildModel = Arc<OnceLock<String>>;

/// Run `run` on `children`'s runtime with the model chain above.
pub fn run(
    children: &DurableChildren,
    stack: &Weak<Stack>,
    mut run: LaneRun,
    child_model: &ChildModel,
) -> Result<LaneReply, String> {
    let model = run
        .model
        .clone()
        .filter(|model| !model.trim().is_empty())
        .or_else(|| child_model.get().cloned())
        .ok_or("typed agents have no model to run on yet")?;
    run.model = Some(model);
    children.run_lane(stack, run)
}

/// The lane runner of a root server's typed agents.
pub fn lanes(children: Arc<DurableChildren>, child_model: ChildModel) -> AgentLanes {
    Arc::new(move |stack: Weak<Stack>| -> LaneRunner {
        let (children, child_model) = (children.clone(), child_model.clone());
        Arc::new(move |lane: LaneRun| run(&children, &stack, lane, &child_model))
    })
}
