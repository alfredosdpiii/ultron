//! The session inspector (`createSessionControl().inspect`, session-control-provider.ts): the read-only host
//! requests behind RPC `inspect`, `/agents`, `/memory why|list`, `/skills`, `/experiments`, `/goals`, `/progress`
//! and the Claude Code watch views. A request outside [`INSPECTION_REQUESTS`] is refused before it reaches the
//! runtime, so inspecting never runs a search, starts work or changes state. Allowed requests go to the session's
//! host stack as the root lane's requests (the worker's `host.handle(request, payload)`), outside any cell.

use kernel::{HostHandler, Signal};
use serde_json::{Map, Value};

/// Read-only host requests the inspector may issue (`INSPECTION_REQUESTS` of session-control.ts). None of them
/// runs a search or starts work.
pub const INSPECTION_REQUESTS: [&str; 24] = [
    "agents.status",
    "agents.list",
    "agents.inspect",
    "memory.why",
    "memory.list",
    "skills.list",
    "skills.why",
    "experiments.list",
    "refinements.list",
    "progress.history",
    "progress.assess",
    "schedules.list",
    "goals.list",
    "goals.get",
    "instances.list",
    "instances.get",
    "gates.list",
    "gates.history",
    "grants.list",
    "rlm.pool",
    "rlm.frames",
    "ctx.state",
    "async.pending",
    "usage.report",
];

/// `inspect(request, payload)`: refuse anything but an inspection request, then ask `runtime` (none: the session
/// has no Ultron runtime). A null payload is an empty one; any other non-object is refused.
pub fn inspect(
    runtime: Option<&dyn HostHandler>,
    request: &str,
    payload: Value,
    signal: &Signal,
) -> Result<Value, String> {
    if !INSPECTION_REQUESTS.contains(&request) {
        return Err(format!("Not an inspection request: {request}"));
    }
    let runtime = runtime.ok_or("This Session has no Ultron runtime to inspect")?;
    let body = match payload {
        Value::Null => Map::new(),
        Value::Object(body) => body,
        _ => return Err("Inspection payload must be an object".into()),
    };
    runtime.handle(request, body, signal)
}
