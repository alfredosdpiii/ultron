//! The request layer of an `ultron mcp` session (session_core::request_layer): what this server serves differently
//! from a native session, and the kernel APIs it cannot serve to a caller, refused with a clear error instead of
//! failing obscurely or succeeding without effect.
//!
//! A root server serves persistent children, `rlm.ask`, the swarm, the board and the notes from its durable runtime
//! (crate::durable_apis), and the root's schedules from the session (crate::schedules). Its subagents with no
//! conversation there (in-memory `--children ultron` subagents) reach the swarm, board and notes by their spawn name
//! (crate::shared_world). Refused there:
//!
//! - `rlm.spawn(keep=True)` and `rlm.ask` from such a subagent (crate::durable_apis::subagent_refusal);
//! - `schedule.*` from any lane but the root's (as in a native session).
//!
//! A Claude Code subagent's server (`--child`) forwards the swarm, board and notes to its parent server
//! (crate::shared_world); `rlm.spawn(keep=True)`, `rlm.ask` and creating a schedule are refused there.

use crate::durable_apis::{DurableApis, SHARED_RECORDS, is_keep_spawn, subagent_refusal};
use crate::shared_world::SharedWorld;
use kernel_apis::ApiModule;
use kernel_apis::prompt_schedule::ScheduleModule;
use serde_json::{Map, Value};
use session_core::compose::Stack;
use session_core::lane::MAIN;
use session_core::request_layer::{Next, RequestLayer};
use std::sync::{Arc, Weak};

/// Why a Claude Code subagent's server cannot serve `kind` with `payload`, if it cannot.
pub fn child_refusal(kind: &str, payload: &Map<String, Value>) -> Option<String> {
    if is_keep_spawn(kind, payload) || kind == "rlm.ask" {
        return Some(subagent_refusal(kind));
    }
    (kind == "schedule.every" || kind == "schedule.at").then(|| format!("{kind} is available to the root agent only"))
}

/// An `rlm.spawn` passed on; the subagent's name is remembered from its answer.
fn spawn(world: &SharedWorld, kind: &str, payload: Map<String, Value>, next: Next<'_>) -> Result<Value, String> {
    let answer = next(payload)?;
    if kind == "rlm.spawn" {
        world.spawned(&answer);
    }
    Ok(answer)
}

/// The layer of a root server over its durable runtime `apis`, its subagents' `world` and the root's `schedules`
/// (of `stack`, the session's).
pub fn root_layer(
    apis: Arc<DurableApis>,
    world: Arc<SharedWorld>,
    schedules: Arc<ScheduleModule>,
    stack: Weak<Stack>,
) -> RequestLayer {
    Arc::new(
        move |lane: &str, kind: &str, payload: Map<String, Value>, next: Next<'_>| {
            if kind.starts_with("schedule.") {
                if lane != MAIN {
                    return Err(format!("{kind} is available to the root agent only"));
                }
                let stack = stack.upgrade().ok_or("The session is closed")?;
                return schedules.handle(&kernel_apis::Request {
                    kind,
                    payload: &payload,
                    lane,
                    caller_task: None,
                    host: &stack.agents,
                    signal: &kernel::Signal::new(),
                });
            }
            if let Some(error) = apis.refusal(lane, kind, &payload) {
                return Err(error);
            }
            if SHARED_RECORDS.contains(&kind) && apis.conversation(lane).is_none() {
                return world.serve(Some(&world.name_of_lane(lane)), kind, &payload);
            }
            match apis.wake_root(lane, kind, &payload, next) {
                Some(answer) => answer,
                None => spawn(&world, kind, payload, next),
            }
        },
    )
}

/// The layer of a Claude Code subagent's server: the shared world goes to the parent (`world`), [`child_refusal`]s
/// are answered, everything else passed on.
pub fn child_layer(world: Arc<SharedWorld>) -> RequestLayer {
    Arc::new(
        move |lane: &str, kind: &str, payload: Map<String, Value>, next: Next<'_>| {
            if let Some(error) = child_refusal(kind, &payload) {
                return Err(error);
            }
            if SHARED_RECORDS.contains(&kind) {
                // This server's root is the subagent its parent knows by its token; its own subagents go by name.
                let name = (lane != MAIN).then(|| world.name_of_lane(lane));
                return world.serve(name.as_deref(), kind, &payload);
            }
            spawn(&world, kind, payload, next)
        },
    )
}
