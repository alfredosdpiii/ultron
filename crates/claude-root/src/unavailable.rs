//! The request layer of an `ultron mcp` session (session_core::request_layer): the kernel APIs this server cannot
//! serve to a caller are refused with a clear error instead of failing obscurely or succeeding without effect.
//!
//! A root server serves persistent children, `rlm.ask`, the swarm, the board and the notes from its durable runtime
//! (crate::durable_apis) and fires schedules itself (crate::schedules). Refused there:
//!
//! - the durable APIs from a lane with no conversation in that runtime (an in-memory `--children ultron` subagent):
//!   the session's shared world is the root's and its persistent children's;
//! - `rlm.spawn(keep=True)` on a Claude Code model, or with no Ultron model to run on.
//!
//! A Claude Code subagent's server (`--child`) is its own small session: the durable APIs and creating a schedule
//! (`schedule.every`, `schedule.at`: the root's only, as in a native session) are refused there.

use crate::durable_apis::{DurableApis, SHARED_RECORDS, is_keep_spawn};
use serde_json::{Map, Value};
use session_core::request_layer::{Next, RequestLayer};
use std::sync::Arc;

/// Why a Claude Code subagent's server cannot serve `kind` with `payload`, if it cannot.
pub fn child_refusal(kind: &str, payload: &Map<String, Value>) -> Option<String> {
    let api = if is_keep_spawn(kind, payload) {
        "rlm.spawn(keep=True)"
    } else if kind == "rlm.ask" {
        "rlm.ask"
    } else if SHARED_RECORDS.contains(&kind) {
        kind.split('.').next().unwrap_or(kind)
    } else if kind == "schedule.every" || kind == "schedule.at" {
        return Some(format!("{kind} is available to the root agent only"));
    } else {
        return None;
    };
    Some(format!(
        "{api} is not available to a Claude Code subagent in ultron claude: the session's persistent agents, swarm, board and notes are the root's; put what the root needs in your answer"
    ))
}

/// The layer of a root server over its durable runtime `apis`.
pub fn root_layer(apis: Arc<DurableApis>) -> RequestLayer {
    Arc::new(
        move |lane: &str, kind: &str, payload: Map<String, Value>, next: Next<'_>| {
            if let Some(error) = apis.refusal(lane, kind, &payload) {
                return Err(error);
            }
            match apis.wake_root(lane, kind, &payload, next) {
                Some(answer) => answer,
                None => next(payload),
            }
        },
    )
}

/// The layer of a Claude Code subagent's server: [`child_refusal`]s answered, everything else passed on.
pub fn child_layer() -> RequestLayer {
    Arc::new(|_lane: &str, kind: &str, payload: Map<String, Value>, next: Next<'_>| {
        match child_refusal(kind, &payload) {
            Some(error) => Err(error),
            None => next(payload),
        }
    })
}
