//! The parts of `ultron/session-stats.ts` the report reads: which REPL APIs a cell names, which steer a message is,
//! and the stored runtime counters (`ultron.module/stats`).

use indexmap::IndexMap;
use serde_json::{Map, Value};

/// The recorder's helpers, shared with the session that writes the counters (crates/session-core).
pub use session_core::session_stats::{CELL_APIS, cell_apis, classify_nudge};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Cells {
    pub count: f64,
    pub failed: f64,
    pub apis: IndexMap<String, f64>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Guard {
    pub checks: f64,
    pub blocked: f64,
    pub unchecked: f64,
    pub after_checks: f64,
    pub after_findings: f64,
    pub ms: f64,
}

/// A stored stats document with every field present (`SessionStatsDocument`).
#[derive(Clone, Debug, PartialEq)]
pub struct Stats {
    pub external: bool,
    pub since: f64,
    pub cells: [Cells; 3],
    pub host_calls: IndexMap<String, f64>,
    pub secrets_masked: f64,
    pub guards: IndexMap<String, Guard>,
    /// toolRounds, wait, skill.
    pub nudges: [f64; 3],
    pub usage_limit_blocks: f64,
    pub merges: IndexMap<String, String>,
    pub child_models: IndexMap<String, String>,
    pub external_turns: (f64, f64),
}

impl Stats {
    /// `emptySessionStats("lane", now)`.
    pub fn empty(now: f64) -> Stats {
        Stats {
            external: false,
            since: now,
            cells: Default::default(),
            host_calls: IndexMap::new(),
            secrets_masked: 0.0,
            guards: IndexMap::new(),
            nudges: [0.0; 3],
            usage_limit_blocks: 0.0,
            merges: IndexMap::new(),
            child_models: IndexMap::new(),
            external_turns: (0.0, 0.0),
        }
    }
}

fn count(value: Option<&Value>) -> f64 {
    match value.and_then(Value::as_f64) {
        Some(n) if n.is_finite() && n >= 0.0 => n,
        _ => 0.0,
    }
}

fn object(value: Option<&Value>) -> Option<&Map<String, Value>> {
    value.and_then(Value::as_object)
}

/// Entries in JavaScript's enumeration order (array-index keys first, ascending).
fn entries(map: &Map<String, Value>) -> Vec<(&String, &Value)> {
    config::json::js_key_order(map)
        .into_iter()
        .map(|k| (k, &map[k]))
        .collect()
}

fn counts(value: Option<&Value>) -> IndexMap<String, f64> {
    object(value)
        .map(|m| {
            entries(m)
                .into_iter()
                .filter(|(_, v)| count(Some(v)) > 0.0)
                .map(|(k, v)| (k.clone(), count(Some(v))))
                .collect()
        })
        .unwrap_or_default()
}

fn strings(value: Option<&Value>) -> IndexMap<String, String> {
    object(value)
        .map(|m| {
            entries(m)
                .into_iter()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

/// `parseSessionStats`: None when `saved` is not a stats document of version 1. Unknown fields are dropped and
/// malformed counters read as zero. A value that is not an object (`null` included) reads as an empty part.
pub fn parse_session_stats(saved: Option<&Value>) -> Option<Stats> {
    let item = object(saved)?;
    if let Some(version) = item.get("version")
        && version.as_f64() != Some(1.0)
    {
        return None;
    }
    let part = |key: &str| item.get(key).filter(|v| !v.is_null());
    let cells = part("cells");
    let cells_of = |name: &str| {
        let c = cells.and_then(|c| c.get(name)).filter(|v| !v.is_null());
        Cells {
            count: count(c.and_then(|c| c.get("count"))),
            failed: count(c.and_then(|c| c.get("failed"))),
            apis: counts(c.and_then(|c| c.get("apis"))),
        }
    };
    let mut guards = IndexMap::new();
    if let Some(map) = object(item.get("guards")) {
        for (name, value) in entries(map) {
            let g = |k: &str| count(Some(value).filter(|v| !v.is_null()).and_then(|v| v.get(k)));
            guards.insert(
                name.clone(),
                Guard {
                    checks: g("checks"),
                    blocked: g("blocked"),
                    unchecked: g("unchecked"),
                    after_checks: g("afterChecks"),
                    after_findings: g("afterFindings"),
                    ms: g("ms"),
                },
            );
        }
    }
    let nudges = part("nudges");
    let nudge = |k: &str| count(nudges.and_then(|n| n.get(k)));
    let turns = part("externalTurns");
    Some(Stats {
        external: item.get("root").and_then(Value::as_str) == Some("external"),
        since: count(item.get("since")),
        cells: [cells_of("root"), cells_of("subagents"), cells_of("other")],
        host_calls: counts(item.get("hostCalls")),
        secrets_masked: count(item.get("secretsMasked")),
        guards,
        nudges: [nudge("toolRounds"), nudge("wait"), nudge("skill")],
        usage_limit_blocks: count(item.get("usageLimitBlocks")),
        merges: strings(item.get("merges")),
        child_models: strings(item.get("childModels")),
        external_turns: (
            count(turns.and_then(|t| t.get("count"))),
            count(turns.and_then(|t| t.get("wallMs"))),
        ),
    })
}
