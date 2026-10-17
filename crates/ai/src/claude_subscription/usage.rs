//! The subscription usage Claude Code reports (`rate_limit_event`) as the `claude_code_usage` diagnostic the TUI
//! footer shows ("Claude Code 5h 9% · 7d 62%"): `rateLimitsFromEvent` and `usageDiagnostic` of
//! packages/coding-agent/src/ultron/claude/root-runner.ts. TypeScript's runner attaches the last limits it saw on
//! any call; here each call attaches the limits of its own `rate_limit_event` (one native process per call).

use crate::types::AssistantMessageDiagnostic;
use serde_json::{Map, Value};

/// `CLAUDE_USAGE_DIAGNOSTIC`.
pub const CLAUDE_USAGE_DIAGNOSTIC: &str = "claude_code_usage";

/// `ClaudeRateLimits`: the plan's status and, per window (`five_hour`, `seven_day`, ...), utilization 0..1 and the
/// reset time, in the order the event names them. Numbers keep their JSON form.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ClaudeRateLimits {
    pub status: Option<String>,
    pub windows: Vec<(String, Option<Value>, Option<Value>)>,
}

/// `num`: a finite number at least 0.
fn num(value: Option<&Value>) -> Option<Value> {
    value
        .filter(|v| v.as_f64().is_some_and(|n| n.is_finite() && n >= 0.0))
        .cloned()
}

/// `rateLimitsFromEvent`: the windows of `rate_limit_info.unifiedWindows`, with the event's own window
/// (`rateLimitType`) added when it is not among them; `None` without a `rate_limit_info` object.
pub fn rate_limits_from_event(event: &Value) -> Option<ClaudeRateLimits> {
    let info = event.get("rate_limit_info").and_then(Value::as_object)?;
    let mut windows: Vec<(String, Option<Value>, Option<Value>)> = Vec::new();
    if let Some(unified) = info.get("unifiedWindows").and_then(Value::as_object) {
        for (name, value) in unified {
            let Some(window) = value.as_object() else { continue };
            let entry = (
                name.clone(),
                num(window.get("utilization")),
                num(window.get("resetsAt")),
            );
            // An object key appears once; a repeated name keeps its first position (JS property order).
            match windows.iter_mut().find(|(n, _, _)| n == name) {
                Some(slot) => *slot = entry,
                None => windows.push(entry),
            }
        }
    }
    if let Some(kind) = info.get("rateLimitType").and_then(Value::as_str)
        && !windows.iter().any(|(n, _, _)| n == kind)
    {
        windows.push((
            kind.to_string(),
            num(info.get("utilization")),
            num(info.get("resetsAt")),
        ));
    }
    Some(ClaudeRateLimits {
        status: info.get("status").and_then(Value::as_str).map(str::to_string),
        windows,
    })
}

/// `usageDiagnostic`: `details.windows` maps a window name to `{ utilization, resetsAt }`, after an optional
/// `details.status`.
pub fn usage_diagnostic(limits: &ClaudeRateLimits, timestamp: i64) -> AssistantMessageDiagnostic {
    let mut windows = Map::new();
    for (name, utilization, resets_at) in &limits.windows {
        let mut window = Map::new();
        if let Some(u) = utilization {
            window.insert("utilization".into(), u.clone());
        }
        if let Some(r) = resets_at {
            window.insert("resetsAt".into(), r.clone());
        }
        windows.insert(name.clone(), Value::Object(window));
    }
    let mut details = Map::new();
    if let Some(status) = &limits.status {
        details.insert("status".into(), Value::String(status.clone()));
    }
    details.insert("windows".into(), Value::Object(windows));
    AssistantMessageDiagnostic {
        kind: CLAUDE_USAGE_DIAGNOSTIC.into(),
        timestamp,
        error: None,
        details: Some(details),
    }
}
