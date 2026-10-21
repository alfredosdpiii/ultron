//! The kernel APIs `ultron mcp` cannot serve, refused with a clear error instead of failing obscurely or succeeding
//! without effect (the kernel's `swarm_api.py` installs them in every kernel):
//!
//! - persistent children (`rlm.spawn(keep=True)`) and `rlm.ask`, the swarm (`swarm.*`), the task board (`board.*`)
//!   and shared notes (`notes.*`): the session actor's durable runtime serves them (session_core::swarm_ops), and
//!   an `ultron mcp` root has none; a keep spawn reached the subagent host as an unknown payload field and the rest
//!   as unknown host requests;
//! - creating a prompt schedule (`schedule.every`, `schedule.at`): a schedule fires through the Ultron server that
//!   runs the session, and Claude Code's session has none, so it was accepted and never fired (`schedule.list` and
//!   `schedule.cancel` still work on what a session already holds).
//!
//! The guide leaves them out too (crate::guide::external_root_apis).

use serde_json::{Map, Value};
use session_core::request_layer::{Next, RequestLayer};
use std::sync::Arc;

/// Requests of the session actor's durable runtime that always come to it (session_core::swarm_ops).
const DURABLE_ONLY: [&str; 13] = [
    "rlm.ask",
    "swarm.register",
    "swarm.agents",
    "swarm.route",
    "swarm.send",
    "swarm.inbox",
    "board.add",
    "board.claim",
    "board.update",
    "board.list",
    "notes.read",
    "notes.append",
    "notes.write",
];

/// Why `kind` with `payload` cannot be served under `ultron claude`, if it cannot.
pub fn refusal(kind: &str, payload: &Map<String, Value>) -> Option<String> {
    let keep = |value: Option<&Value>| value == Some(&Value::Bool(true));
    if kind == "rlm.spawn" && (keep(payload.get("keep")) || keep(payload.get("kwargs").and_then(|k| k.get("keep")))) {
        return Some(
            "rlm.spawn(keep=True) is not available in ultron claude: a subagent here ends with its answer and cannot be asked follow-ups (no rlm.ask); spawn without keep=True and give each subagent a self-contained brief"
                .into(),
        );
    }
    if DURABLE_ONLY.contains(&kind) {
        let api = kind.split('.').next().unwrap_or(kind);
        let what = if kind == "rlm.ask" { "rlm.ask" } else { api };
        return Some(format!(
            "{what} is not available in ultron claude: it needs the durable session runtime of `ultron` (persistent agents, the swarm, the task board and notes)"
        ));
    }
    if kind == "schedule.every" || kind == "schedule.at" {
        return Some(format!(
            "{kind} is not available in ultron claude: scheduled prompts are fired by the Ultron server running the session, and a Claude Code session has none"
        ));
    }
    None
}

/// The request layer of an `ultron mcp` session: [`refusal`]s answered, everything else passed on.
pub fn layer() -> RequestLayer {
    Arc::new(
        |_lane: &str, kind: &str, payload: Map<String, Value>, next: Next<'_>| match refusal(kind, &payload) {
            Some(error) => Err(error),
            None => next(payload),
        },
    )
}
