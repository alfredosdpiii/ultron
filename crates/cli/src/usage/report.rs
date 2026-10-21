//! "What did this session do?" (`ultron/session-report.ts`): turns, cells, depth (frames, subagents, workflows,
//! jobs), tokens and cost per lane kind and model, guardrails and memory decisions, from a session file's digest.
//! A number the session did not keep is `null` and named in `unrecorded`; it is never reported as zero.
//!
//! The report is a `serde_json::Value` in the TypeScript field order: `ultron usage --json` prints it and the text
//! renderer reads it.

use super::log::{EntryKind, LogEntry, LogUsage, SessionLog, log_usage, record};
use super::stats::{CELL_APIS, Cells, Stats, parse_session_stats};
use crate::js::{num, string};
use crate::list_models::locale_compare;
use indexmap::{IndexMap, IndexSet};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};

pub const SESSION_REPORT_SCHEMA: &str = "ultron.session-report/1";
/// Billed through a subscription login whatever the caller passes (session-report.ts `SUBSCRIPTION_PROVIDERS`).
const SUBSCRIPTION_PROVIDERS: [&str; 1] = ["claude-code"];
const COUNTER_SLACK_MS: f64 = 1000.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Lane {
    Root,
    Frames,
    Subagents,
    Other,
}

/// The model row of usage the host's ledger counted beyond every response the session file records (it keeps no
/// model per call): totals only. Durable children's own responses are read from the durable records, with their
/// models; this row is what remains (a session whose durable records are incomplete).
pub const DURABLE_CHILDREN_MODEL: &str = "sub-agents (host ledger)";

fn lane_kind(lane: Option<&str>) -> Lane {
    match lane {
        Some("main") => Lane::Root,
        Some(l) if l.starts_with("ultron.rlm-frame.") => Lane::Frames,
        Some(l) if l.starts_with("ultron.rlm-child.") => Lane::Subagents,
        _ => Lane::Other,
    }
}

#[derive(Clone, Debug, Default)]
struct Bucket {
    responses: f64,
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write: f64,
    total_tokens: f64,
    reported_usd: Option<f64>,
    subscription_usd: Option<f64>,
    unpriced: f64,
    unmeasured: f64,
}

impl Bucket {
    fn add(&mut self, usage: &LogUsage, subscription: bool) {
        self.responses += 1.0;
        self.input += usage.input;
        self.output += usage.output;
        self.cache_read += usage.cache_read;
        self.cache_write += usage.cache_write;
        self.total_tokens += usage.total_tokens;
        if subscription {
            self.subscription_usd = Some(self.subscription_usd.unwrap_or(0.0) + usage.cost.unwrap_or(0.0));
        } else if let Some(cost) = usage.cost.filter(|c| *c > 0.0) {
            self.reported_usd = Some(self.reported_usd.unwrap_or(0.0) + cost);
        } else if usage.total_tokens > 0.0 {
            self.unpriced += 1.0;
        }
    }

    /// Usage known only as totals (the host's ledger): responses, tokens and cost, with no input/output split.
    fn add_totals(&mut self, responses: f64, tokens: f64, cost: f64, subscription: bool) {
        self.responses += responses;
        self.total_tokens += tokens;
        if cost > 0.0 {
            let slot = if subscription {
                &mut self.subscription_usd
            } else {
                &mut self.reported_usd
            };
            *slot = Some(slot.unwrap_or(0.0) + cost);
        } else if tokens > 0.0 {
            self.unpriced += responses;
        }
    }

    fn fields(&self) -> Map<String, Value> {
        let opt = |v: Option<f64>| v.map_or(Value::Null, num);
        let Value::Object(map) = json!({
            "responses": num(self.responses),
            "input": num(self.input),
            "output": num(self.output),
            "cacheRead": num(self.cache_read),
            "cacheWrite": num(self.cache_write),
            "totalTokens": num(self.total_tokens),
            "cost": {
                "reportedUsd": opt(self.reported_usd),
                "subscriptionUsd": opt(self.subscription_usd),
                "unpricedResponses": num(self.unpriced),
            },
            "unmeasured": num(self.unmeasured),
        }) else {
            unreachable!()
        };
        map
    }
}

struct Buckets<'s> {
    /// Providers whose costs are notional (a subscription login).
    subscription: &'s HashSet<&'s str>,
    lanes: [Bucket; 4],
    models: IndexMap<String, Bucket>,
    by_lane: HashMap<String, Bucket>,
    total: Bucket,
}

fn provider_of(model: &str) -> &str {
    model.split_once('/').map_or(model, |(p, _)| p)
}

impl Buckets<'_> {
    fn index(kind: Lane) -> usize {
        kind as usize
    }

    fn add(&mut self, kind: Lane, model: &str, usage: &LogUsage, lane: Option<&str>) {
        let subscription = self.subscription.contains(provider_of(model));
        self.lanes[Self::index(kind)].add(usage, subscription);
        self.models
            .entry(model.to_string())
            .or_default()
            .add(usage, subscription);
        self.total.add(usage, subscription);
        if let Some(lane) = lane {
            self.by_lane
                .entry(lane.to_string())
                .or_default()
                .add(usage, subscription);
        }
    }

    fn add_totals(&mut self, kind: Lane, model: &str, responses: f64, tokens: f64, cost: f64, subscription: bool) {
        self.lanes[Self::index(kind)].add_totals(responses, tokens, cost, subscription);
        self.models
            .entry(model.to_string())
            .or_default()
            .add_totals(responses, tokens, cost, subscription);
        self.total.add_totals(responses, tokens, cost, subscription);
    }

    fn unmeasured(&mut self, kind: Lane, model: &str) {
        self.lanes[Self::index(kind)].unmeasured += 1.0;
        self.models.entry(model.to_string()).or_default().unmeasured += 1.0;
        self.total.unmeasured += 1.0;
    }
}

fn plural(count: f64, one: &str) -> String {
    format!(
        "{} {}",
        crate::js::number(count),
        if count == 1.0 {
            one.to_string()
        } else {
            format!("{one}s")
        }
    )
}

/// The one-line answer to "did this session use depth or only the root?".
fn depth_verdict(level: f64, frames: f64, subagents: f64, nested: f64, typed: f64, background: f64) -> String {
    if level == 0.0 {
        return "root only".into();
    }
    let mut parts = vec![plural(frames, "frame")];
    parts.push(format!(
        "{}{}",
        plural(subagents, "sub-agent"),
        if nested > 0.0 {
            format!(" ({} nested)", crate::js::number(nested))
        } else {
            String::new()
        }
    ));
    if typed > 0.0 {
        parts.push(plural(typed, "typed-agent task"));
    }
    if background > 0.0 {
        parts.push(plural(background, "background job"));
    }
    format!("depth {}: {}", crate::js::number(level), parts.join(", "))
}

/// Whether nothing ran in the session: no entry, no task, and no runtime value beyond the skill catalog.
pub fn session_is_empty(log: &SessionLog) -> bool {
    log.entries.is_empty()
        && !log
            .values
            .keys()
            .any(|address| address.starts_with("ultron.") && address != "ultron.module/skills")
}

#[derive(Clone, Debug)]
struct Task {
    id: String,
    definition: String,
    state: String,
    parent_id: Option<String>,
    result: Option<Map<String, Value>>,
}

fn definition_name(definition: Option<&Value>) -> String {
    let text = match definition {
        None | Some(Value::Null) => "unknown".to_string(),
        Some(v) => string(v),
    };
    // `.replace(/@\d+$/, "")`
    if let Some(at) = text.rfind('@') {
        let digits = &text[at + 1..];
        if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
            return text[..at].to_string();
        }
    }
    text
}

fn tasks_of(log: &SessionLog) -> Vec<Task> {
    let list = record(log.value("ultron.tasks", "root"))
        .and_then(|t| t.get("tasks"))
        .and_then(Value::as_array);
    list.into_iter()
        .flatten()
        .filter_map(|item| {
            let task = item.as_object()?;
            Some(Task {
                id: task.get("id")?.as_str()?.to_string(),
                definition: definition_name(task.get("definition")),
                state: task
                    .get("state")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string(),
                parent_id: task.get("parentId").and_then(Value::as_str).map(str::to_string),
                result: record(task.get("result")).cloned(),
            })
        })
        .collect()
}

fn model_name(config: &Value) -> Option<String> {
    let model = record(Some(config))?.get("model")?.as_object()?;
    Some(format!(
        "{}/{}",
        model.get("provider")?.as_str()?,
        model.get("modelId")?.as_str()?
    ))
}

#[derive(Clone, Default)]
struct ModelCount {
    count: f64,
    tokens: f64,
    unmeasured: f64,
}

fn by_model_list(models: IndexMap<String, ModelCount>) -> Value {
    let mut list: Vec<(String, ModelCount)> = models.into_iter().collect();
    list.sort_by(|a, b| {
        b.1.count
            .partial_cmp(&a.1.count)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| locale_compare(&a.0, &b.0))
    });
    Value::Array(
        list.into_iter()
            .map(|(model, c)| {
                json!({"model": model, "count": num(c.count), "tokens": num(c.tokens), "unmeasured": num(c.unmeasured)})
            })
            .collect(),
    )
}

fn apis_value(apis: &IndexMap<String, f64>) -> Value {
    let mut ordered = Map::new();
    for api in CELL_APIS {
        if let Some(n) = apis.get(api) {
            ordered.insert(api.to_string(), num(*n));
        }
    }
    let mut unknown: Vec<&String> = apis.keys().filter(|k| !CELL_APIS.contains(&k.as_str())).collect();
    unknown.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
    for api in unknown {
        ordered.insert(api.clone(), num(apis[api]));
    }
    Value::Object(ordered)
}

fn cells_value(cells: &Cells) -> Value {
    json!({"count": num(cells.count), "failed": num(cells.failed), "apis": apis_value(&cells.apis)})
}

/// Build the report from a session file's digest.
pub fn build_session_report(log: &SessionLog) -> Value {
    build_session_report_with(log, &[])
}

/// [`build_session_report`] with the providers whose costs are notional besides Claude Code (`subscriptionProviders`:
/// every provider the session's runtime reaches through a subscription login).
pub fn build_session_report_with(log: &SessionLog, subscription_providers: &[String]) -> Value {
    let subscription: HashSet<&str> = SUBSCRIPTION_PROVIDERS
        .into_iter()
        .chain(subscription_providers.iter().map(String::as_str))
        .collect();
    let mut unrecorded = Map::new();
    let tasks = tasks_of(log);
    let mut task_by_id: HashMap<&str, &Task> = HashMap::new();
    for task in &tasks {
        task_by_id.insert(&task.id, task);
    }

    // Entries belong to the lane whose branch they are on.
    let mut entry_by_id: HashMap<&str, &LogEntry> = HashMap::new();
    for entry in &log.entries {
        entry_by_id.insert(&entry.id, entry);
    }
    let mut root_ids: HashMap<String, String> = HashMap::new();
    let tree_root = |id: &str, root_ids: &mut HashMap<String, String>| -> String {
        let mut path: IndexSet<String> = IndexSet::new();
        let mut current = Some(id.to_string());
        let mut root = id.to_string();
        while let Some(c) = current {
            if let Some(known) = root_ids.get(&c) {
                root = known.clone();
                break;
            }
            path.insert(c.clone());
            root = c.clone();
            let parent = entry_by_id.get(c.as_str()).and_then(|e| e.parent_id.clone());
            current = parent.filter(|p| !path.contains(p));
        }
        for visited in path {
            root_ids.insert(visited, root.clone());
        }
        root
    };
    let mut tree_lane: HashMap<String, String> = HashMap::new();
    let mut claim = |entry_id: Option<&str>, lane: &str, root_ids: &mut HashMap<String, String>| {
        let Some(entry_id) = entry_id.filter(|id| entry_by_id.contains_key(id)) else {
            return;
        };
        let root = tree_root(entry_id, root_ids);
        if !tree_lane.contains_key(&root) || lane == "main" {
            tree_lane.insert(root, lane.to_string());
        }
    };
    for (lane, tip) in log.values_of("pi.branch.tip") {
        claim(tip.as_str(), lane, &mut root_ids);
    }
    for result in &log.results {
        let Some(op) = log.operations.get(&result.operation_id) else {
            continue;
        };
        claim(result.tip_id.as_deref(), &op.lane, &mut root_ids);
        claim(result.from_tip_id.as_deref(), &op.lane, &mut root_ids);
    }
    let lanes: Vec<Option<String>> = log
        .entries
        .iter()
        .map(|entry| tree_lane.get(&tree_root(&entry.id, &mut root_ids)).cloned())
        .collect();
    let mut lane_models: HashMap<&str, String> = HashMap::new();
    for (lane, config) in log.values_of("pi.lane.config") {
        if let Some(model) = model_name(config) {
            lane_models.insert(lane, model);
        }
    }

    // Mode.
    let main_entries: Vec<&LogEntry> = log
        .entries
        .iter()
        .zip(&lanes)
        .filter(|(_, lane)| lane.as_deref() == Some("main"))
        .map(|(e, _)| e)
        .collect();
    let main_runs: Vec<_> = log
        .results
        .iter()
        .filter(|r| r.kind == "run" && log.operations.get(&r.operation_id).map(|o| o.lane.as_str()) == Some("main"))
        .collect();
    let name = log.value("pi.session.name", "").and_then(Value::as_str);
    let empty_roots = Map::new();
    let usage_roots =
        record(record(log.value("ultron.usage", "root")).and_then(|u| u.get("roots"))).unwrap_or(&empty_roots);
    let external_marks = name == Some("claude code")
        || name.is_some_and(|n| n.starts_with("claude subagent ("))
        || usage_roots.keys().any(|id| id.starts_with("turn:cc-"));
    let mut first_activity = f64::INFINITY;
    let mut happened = |at: Option<f64>| {
        if let Some(at) = at
            && at > 0.0
            && at < first_activity
        {
            first_activity = at;
        }
    };
    for entry in &log.entries {
        happened(Some(entry.timestamp));
    }
    for op in log.operations.values() {
        happened(Some(op.started_at));
    }
    for root in usage_roots.values() {
        happened(root.get("startedAt").and_then(Value::as_f64));
    }
    let nothing_ran = first_activity == f64::INFINITY && tasks.is_empty() && !external_marks;
    let stats: Option<Stats> = parse_session_stats(log.value("ultron.module", "stats"))
        .or_else(|| nothing_ran.then(|| Stats::empty(log.header.created_at)));
    let mode = if stats.as_ref().is_some_and(|s| s.external)
        || (stats.is_none() && main_entries.is_empty() && external_marks)
    {
        "ultron claude"
    } else if !log.values_of("ultron.claude-code.lanes").is_empty()
        || main_entries
            .iter()
            .any(|e| e.kind == EntryKind::Assistant && e.provider.as_deref() == Some("claude-code"))
    {
        "ultron --claude"
    } else {
        "ultron"
    };
    let external = mode == "ultron claude";
    let stats_cover = stats
        .as_ref()
        .is_some_and(|s| s.since <= first_activity + COUNTER_SLACK_MS);

    // Turns.
    let turns = if external {
        match &stats {
            None => {
                unrecorded.insert(
                    "turns".into(),
                    "Claude Code owns the root conversation; this session predates Ultron's turn count".into(),
                );
                Value::Null
            }
            Some(s) => json!({
                "count": num(s.external_turns.0),
                "completed": num(s.external_turns.0),
                "aborted": 0,
                "failed": 0,
                "running": 0,
                "wallMs": num(s.external_turns.1),
            }),
        }
    } else {
        let ended: HashSet<&str> = log.results.iter().map(|r| r.operation_id.as_str()).collect();
        let running = log
            .operations
            .values()
            .filter(|o| o.lane == "main" && o.kind == "run" && !ended.contains(o.id.as_str()))
            .count() as f64;
        let with = |f: &dyn Fn(&str) -> bool| main_runs.iter().filter(|r| f(&r.status)).count() as f64;
        json!({
            "count": num(main_runs.len() as f64 + running),
            "completed": num(with(&|s| s == "completed")),
            "aborted": num(with(&|s| s == "aborted")),
            "failed": num(with(&|s| s == "failed" || s == "declined")),
            "running": num(running),
            "wallMs": num(main_runs.iter().fold(0.0, |t, r| t + (r.ended_at - r.started_at).max(0.0))),
        })
    };

    // Usage per lane kind and model.
    let mut buckets = Buckets {
        subscription: &subscription,
        lanes: Default::default(),
        models: IndexMap::new(),
        by_lane: HashMap::new(),
        total: Bucket::default(),
    };
    let mut root_models: IndexMap<String, f64> = IndexMap::new();
    let mut attributed: HashSet<&str> = HashSet::new();
    for (entry, lane) in log.entries.iter().zip(&lanes) {
        let Some(usage) = &entry.usage else { continue };
        let lane = lane.as_deref();
        let kind = lane_kind(lane);
        if entry.kind == EntryKind::Assistant {
            let model = format!(
                "{}/{}",
                entry.provider.as_deref().unwrap_or("unknown"),
                entry.model.as_deref().unwrap_or("unknown")
            );
            buckets.add(kind, &model, usage, lane);
            if kind == Lane::Root {
                *root_models.entry(model).or_default() += 1.0;
            }
        } else {
            let model = lane
                .and_then(|l| lane_models.get(l))
                .map_or("unknown (summaries)", String::as_str);
            buckets.add(kind, model, usage, lane);
        }
        attributed.insert(&entry.id);
    }
    let lane_of_id: HashMap<&str, Option<&str>> = log
        .entries
        .iter()
        .zip(&lanes)
        .map(|(e, l)| (e.id.as_str(), l.as_deref()))
        .collect();
    for row in &log.usage_rows {
        if row.entry_id.as_deref().is_some_and(|id| attributed.contains(id)) {
            continue;
        }
        match row.entry_id.as_deref().filter(|id| entry_by_id.contains_key(id)) {
            None => buckets.add(
                Lane::Other,
                if row.adjustment {
                    "imported history"
                } else {
                    "unattributed"
                },
                &row.usage,
                None,
            ),
            Some(id) => {
                let lane = lane_of_id.get(id).copied().flatten();
                let model = lane.and_then(|l| lane_models.get(l)).map_or("unknown", String::as_str);
                buckets.add(lane_kind(lane), model, &row.usage, lane);
            }
        }
    }
    let mut child_models: HashMap<&str, String> = HashMap::new();
    let ledger_calls: Vec<&Value> = usage_roots
        .values()
        .filter_map(|root| root.get("calls").and_then(Value::as_array))
        .flatten()
        .collect();
    let mut process_children: HashSet<&str> = HashSet::new();
    for task in &tasks {
        if !external
            || task.definition != "rlm-child"
            || lane_models.contains_key(format!("ultron.rlm-child.{}", task.id).as_str())
        {
            continue;
        }
        process_children.insert(&task.id);
        let model = stats
            .as_ref()
            .and_then(|s| s.child_models.get(&task.id).cloned())
            .unwrap_or_else(|| "claude-code/(model not recorded)".into());
        child_models.insert(&task.id, model.clone());
        let call = ledger_calls.iter().find(|item| {
            item.get("kind").and_then(Value::as_str) == Some("model")
                && item.get("taskId").and_then(Value::as_str) == Some(&task.id)
        });
        let measured = record(call.and_then(|c| c.get("usage")));
        let Some(measured) = measured.filter(|m| m.get("totalTokens").is_some_and(Value::is_number)) else {
            if task.state != "admitted" && task.state != "running" {
                buckets.unmeasured(Lane::Subagents, &model);
            }
            continue;
        };
        let mut usage = Map::new();
        for (to, from) in [
            ("input", "inputTokens"),
            ("output", "outputTokens"),
            ("totalTokens", "totalTokens"),
        ] {
            if let Some(v) = measured.get(from) {
                usage.insert(to.into(), v.clone());
            }
        }
        if let Some(cost) = measured.get("cost").filter(|c| c.is_number()) {
            usage.insert("cost".into(), json!({"total": cost}));
        }
        if let Some(usage) = log_usage(Some(&Value::Object(usage))) {
            buckets.add(
                Lane::Subagents,
                &model,
                &usage,
                Some(&format!("ultron.rlm-child.{}", task.id)),
            );
        }
    }

    // Durable children (ultron-rs `rlm.spawn`, kept or not, typed agents) answer in their own conversations, which are
    // not session entries: their responses are `pi.assistant` entries of the durable records, with model, split and
    // cost. A conversation a task owns is a sub-agent's; another root conversation's goes to "other".
    let mut durable_other = Bucket::default();
    if !external {
        for response in &log.durable_responses {
            let kind = match lane_kind(response.lane.as_deref()) {
                Lane::Frames => Lane::Frames,
                _ if response.owned => Lane::Subagents,
                _ => Lane::Other,
            };
            buckets.add(kind, &response.model, &response.usage, response.lane.as_deref());
            if kind == Lane::Other {
                let notional = subscription.contains(provider_of(&response.model));
                durable_other.add(&response.usage, notional);
            }
        }
    }

    // The host's usage ledger counts every response of a budget root's tree (a turn's root carries its root lane and
    // the children it spawned; kept children are charged to `ultron-root`). What it holds beyond every response the
    // file records (root, frame and sub-agent lanes, other conversations) is usage the records lack: totals only.
    if !external {
        let field = |root: &Value, name: &str| {
            root.get("turns")
                .and_then(|t| t.get(name))
                .and_then(Value::as_f64)
                .unwrap_or(0.0)
        };
        let (ledger_responses, ledger_tokens, ledger_cost) =
            usage_roots.values().fold((0.0, 0.0, 0.0), |(r, t, c), root| {
                (
                    r + field(root, "count"),
                    t + field(root, "tokens"),
                    c + field(root, "cost"),
                )
            });
        let mut seen: Vec<&Bucket> = [Lane::Root, Lane::Frames, Lane::Subagents]
            .map(|kind| &buckets.lanes[Buckets::index(kind)])
            .to_vec();
        seen.push(&durable_other);
        let seen_tokens: f64 = seen.iter().map(|b| b.total_tokens).sum();
        let seen_responses: f64 = seen.iter().map(|b| b.responses).sum();
        let seen_cost: f64 = seen
            .iter()
            .map(|b| b.reported_usd.unwrap_or(0.0) + b.subscription_usd.unwrap_or(0.0))
            .sum();
        if ledger_tokens > seen_tokens && ledger_responses > seen_responses {
            let notional = root_models
                .keys()
                .next()
                .is_some_and(|model| subscription.contains(provider_of(model)));
            buckets.add_totals(
                Lane::Subagents,
                DURABLE_CHILDREN_MODEL,
                ledger_responses - seen_responses,
                ledger_tokens - seen_tokens,
                (ledger_cost - seen_cost).max(0.0),
                notional,
            );
        }
    }

    // Cells.
    let mut transcript: [Cells; 3] = Default::default();
    let mut other_tools: IndexMap<String, (f64, f64)> = IndexMap::new();
    let mut redactions = 0.0;
    let mut nudges = [0.0; 3];
    for (entry, lane) in log.entries.iter().zip(&lanes) {
        let kind = match lane_kind(lane.as_deref()) {
            Lane::Root => 0,
            Lane::Subagents => 1,
            Lane::Frames | Lane::Other => 2,
        };
        match entry.kind {
            EntryKind::Assistant => {
                for call in &entry.tool_calls {
                    if call.name == "rlm" {
                        transcript[kind].count += 1.0;
                        for api in call.apis.iter().flatten() {
                            *transcript[kind].apis.entry(api.to_string()).or_default() += 1.0;
                        }
                    } else {
                        other_tools.entry(call.name.clone()).or_default().0 += 1.0;
                    }
                }
            }
            EntryKind::ToolResult => {
                redactions += entry.redactions;
                if !entry.is_error {
                    continue;
                }
                match entry.tool_name.as_deref() {
                    Some("rlm") => transcript[kind].failed += 1.0,
                    Some(name) => other_tools.entry(name.to_string()).or_default().1 += 1.0,
                    None => {}
                }
            }
            EntryKind::User => {
                if let Some(nudge) = entry.nudge {
                    nudges[["toolRounds", "wait", "skill"]
                        .iter()
                        .position(|n| *n == nudge)
                        .unwrap_or(0)] += 1.0;
                }
            }
            _ => {}
        }
    }
    let other_tools_value = Value::Object(
        other_tools
            .iter()
            .map(|(name, (calls, failed))| (name.clone(), json!({"calls": num(*calls), "failed": num(*failed)})))
            .collect(),
    );
    let cells_from = |source: &str, by: &[Cells; 3]| -> Value {
        let mut total = Cells::default();
        for part in by {
            total.count += part.count;
            total.failed += part.failed;
            for (api, n) in &part.apis {
                *total.apis.entry(api.clone()).or_default() += n;
            }
        }
        json!({
            "source": source,
            "total": cells_value(&total),
            "root": cells_value(&by[0]),
            "subagents": cells_value(&by[1]),
            "other": cells_value(&by[2]),
            "otherTools": other_tools_value,
        })
    };
    let cells = if !external {
        cells_from("transcript", &transcript)
    } else if let Some(s) = &stats {
        cells_from("counters", &s.cells)
    } else {
        unrecorded.insert(
            "cells".into(),
            "Claude Code owns the transcript; this session predates Ultron's cell count".into(),
        );
        Value::Null
    };

    // Depth: subagents.
    let children: Vec<&Task> = tasks.iter().filter(|t| t.definition == "rlm-child").collect();
    let child_depth = |task: &Task| -> f64 {
        let mut depth = 0.0;
        let mut seen: HashSet<&str> = HashSet::new();
        let mut current = Some(task);
        while let Some(c) = current {
            if seen.contains(c.id.as_str()) {
                break;
            }
            seen.insert(&c.id);
            if c.definition == "rlm-child" {
                depth += 1.0;
            }
            current = c.parent_id.as_deref().and_then(|p| task_by_id.get(p).copied());
        }
        depth
    };
    let level_of_task =
        |id: Option<&str>| -> f64 { id.and_then(|i| task_by_id.get(i)).map_or(0.0, |t| child_depth(t)) };
    let state =
        |items: &[&Task], states: &[&str]| items.iter().filter(|t| states.contains(&t.state.as_str())).count() as f64;
    // verified, contradicted, unverified, invalid, unchecked, none
    let mut verdicts = [0.0; 6];
    // passed, failed, blocked
    let mut claims = [0.0; 3];
    let mut worktrees = Vec::new();
    let mut child_by_model: IndexMap<String, ModelCount> = IndexMap::new();
    // A durable child's model: its first response's.
    let mut durable_models: HashMap<&str, &str> = HashMap::new();
    for response in &log.durable_responses {
        if let Some(lane) = &response.lane {
            durable_models.entry(lane.as_str()).or_insert(response.model.as_str());
        }
    }
    for child in &children {
        let lane = format!("ultron.rlm-child.{}", child.id);
        let model = lane_models
            .get(lane.as_str())
            .cloned()
            .or_else(|| durable_models.get(lane.as_str()).map(|m| m.to_string()))
            .or_else(|| child_models.get(child.id.as_str()).cloned())
            .or_else(|| stats.as_ref().and_then(|s| s.child_models.get(&child.id).cloned()))
            .unwrap_or_else(|| "not recorded".into());
        let tokens = buckets.by_lane.get(&lane).map(|b| b.total_tokens);
        let by_model = child_by_model.entry(model).or_default();
        by_model.count += 1.0;
        by_model.tokens += tokens.unwrap_or(0.0);
        if tokens.is_none() && process_children.contains(child.id.as_str()) && child.result.is_some() {
            by_model.unmeasured += 1.0;
        }
        let Some(result) = &child.result else { continue };
        let verdict = record(result.get("verdict"));
        let outcome = record(result.get("check"))
            .and_then(|c| c.get("outcome"))
            .and_then(Value::as_str);
        match (verdict, outcome) {
            (None, Some("invalid")) => verdicts[3] += 1.0,
            (None, _) => verdicts[5] += 1.0,
            (Some(_), Some("verified")) => verdicts[0] += 1.0,
            (Some(_), Some("contradicted")) => verdicts[1] += 1.0,
            (Some(_), _) => verdicts[4] += 1.0,
        }
        match verdict.and_then(|v| v.get("status")).and_then(Value::as_str) {
            Some("passed") => claims[0] += 1.0,
            Some("failed") => claims[1] += 1.0,
            Some("blocked") => claims[2] += 1.0,
            _ => {}
        }
        let worktree = record(result.get("worktree"));
        if let Some(w) = worktree
            && let Some(branch) = w.get("branch").and_then(Value::as_str)
        {
            let merge = stats
                .as_ref()
                .and_then(|s| s.merges.get(&child.id).cloned())
                .map(Value::String)
                .unwrap_or_else(|| {
                    if w.get("commit") == Some(&Value::Null) {
                        Value::String("empty".into())
                    } else {
                        Value::Null
                    }
                });
            worktrees.push(json!({
                "task": child.id,
                "branch": branch,
                "changedFiles": num(w.get("changed_files").and_then(Value::as_array).map_or(0, Vec::len) as f64),
                "merge": merge,
            }));
        }
    }
    verdicts[2] = verdicts[3] + verdicts[4] + verdicts[5];
    let depths: Vec<f64> = children.iter().map(|c| child_depth(c)).collect();
    let max_child_depth = depths.iter().fold(0.0_f64, |m, d| m.max(*d));

    // Depth: frames.
    let mut trace_by_id: IndexMap<String, &Map<String, Value>> = IndexMap::new();
    for (key, value) in log.values_of("ultron.rlm.frames") {
        if key == "index" {
            continue;
        }
        if let Some(trace) = value.as_object()
            && let Some(id) = trace.get("id").and_then(Value::as_str)
        {
            trace_by_id.insert(id.to_string(), trace);
        }
    }
    let frame_tasks: Vec<&Task> = tasks.iter().filter(|t| t.definition == "rlm-frame").collect();
    let mut traced: HashSet<&str> = HashSet::new();
    // count, complete, incomplete, failed, cancelled, running, nested
    let mut frames = [0.0; 7];
    let mut incomplete_reasons: IndexMap<String, f64> = IndexMap::new();
    let mut budget_tokens = 0.0;
    let mut frame_by_model: IndexMap<String, ModelCount> = IndexMap::new();
    let mut frame_levels: HashMap<String, f64> = HashMap::new();
    fn frame_level(
        trace: &Map<String, Value>,
        seen: &mut HashSet<String>,
        levels: &mut HashMap<String, f64>,
        traces: &IndexMap<String, &Map<String, Value>>,
        level_of_task: &dyn Fn(Option<&str>) -> f64,
    ) -> f64 {
        let id = trace.get("id").map(string).unwrap_or_default();
        if let Some(known) = levels.get(&id) {
            return *known;
        }
        seen.insert(id.clone());
        let parent = trace
            .get("parentFrame")
            .and_then(Value::as_str)
            .filter(|p| !seen.contains(*p))
            .and_then(|p| traces.get(p).copied());
        let level = match parent {
            Some(parent) => frame_level(parent, seen, levels, traces, level_of_task) + 1.0,
            None => level_of_task(trace.get("callerTaskId").and_then(Value::as_str)) + 1.0,
        };
        levels.insert(id, level);
        level
    }
    let mut max_frame_level: f64 = 0.0;
    let count_frame = |model: String, lane: Option<&str>, by_model: &mut IndexMap<String, ModelCount>| {
        let tokens = lane.and_then(|l| buckets.by_lane.get(l)).map(|b| b.total_tokens);
        let entry = by_model.entry(model).or_default();
        entry.count += 1.0;
        entry.tokens += tokens.unwrap_or(0.0);
    };
    for trace in trace_by_id.values() {
        frames[0] += 1.0;
        if let Some(task_id) = trace.get("taskId").and_then(Value::as_str) {
            traced.insert(task_id);
        }
        match trace.get("status").and_then(Value::as_str) {
            Some("complete") => frames[1] += 1.0,
            Some("incomplete") => {
                frames[2] += 1.0;
                let reason = trace.get("reason").and_then(Value::as_str).unwrap_or("unknown");
                *incomplete_reasons.entry(reason.to_string()).or_default() += 1.0;
            }
            Some("error") => frames[3] += 1.0,
            _ => frames[5] += 1.0,
        }
        if let Some(spent) = record(trace.get("spent"))
            .and_then(|s| s.get("tokens"))
            .and_then(Value::as_f64)
            && spent.is_finite()
        {
            budget_tokens += spent;
        }
        let level = frame_level(
            trace,
            &mut HashSet::new(),
            &mut frame_levels,
            &trace_by_id,
            &level_of_task,
        );
        if level > 1.0 {
            frames[6] += 1.0;
        }
        max_frame_level = max_frame_level.max(level);
        let lane = match (
            trace.get("lane").and_then(Value::as_str),
            trace.get("taskId").and_then(Value::as_str),
        ) {
            (Some(lane), _) => Some(lane.to_string()),
            (None, Some(task)) => Some(format!("ultron.rlm-frame.{task}")),
            _ => None,
        };
        let model = trace
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| lane.as_deref().and_then(|l| lane_models.get(l).cloned()))
            .unwrap_or_else(|| "not recorded".into());
        count_frame(model, lane.as_deref(), &mut frame_by_model);
    }
    for task in &frame_tasks {
        if traced.contains(task.id.as_str()) {
            continue;
        }
        frames[0] += 1.0;
        match task.state.as_str() {
            "completed" => frames[1] += 1.0,
            "cancelled" | "interrupted" => frames[4] += 1.0,
            "failed" => frames[3] += 1.0,
            _ => frames[5] += 1.0,
        }
        let level = level_of_task(task.parent_id.as_deref()) + 1.0;
        if level > 1.0 {
            frames[6] += 1.0;
        }
        max_frame_level = max_frame_level.max(level);
        let lane = format!("ultron.rlm-frame.{}", task.id);
        let model = lane_models
            .get(lane.as_str())
            .cloned()
            .unwrap_or_else(|| "not recorded".into());
        count_frame(model, Some(&lane), &mut frame_by_model);
    }

    let typed: Vec<&Task> = tasks
        .iter()
        .filter(|t| !["rlm-child", "rlm-frame", "background-job"].contains(&t.definition.as_str()))
        .collect();
    let mut by_definition: IndexMap<String, f64> = IndexMap::new();
    for task in &typed {
        *by_definition.entry(task.definition.clone()).or_default() += 1.0;
    }
    let background: Vec<&Task> = tasks.iter().filter(|t| t.definition == "background-job").collect();
    let other_level = typed
        .iter()
        .chain(&background)
        .fold(0.0_f64, |m, t| m.max(level_of_task(t.parent_id.as_deref()) + 1.0));
    let level = max_child_depth.max(max_frame_level).max(other_level);
    let nested_children = depths.iter().filter(|d| **d > 1.0).count() as f64;
    if !stats_cover {
        let reason = if stats.is_none() {
            "this session predates Ultron's runtime counters"
        } else {
            "the runtime counters began after this session did"
        };
        unrecorded.insert("depth.workflows".into(), reason.into());
        unrecorded.insert("depth.frames.calls".into(), reason.into());
    }

    // Guardrails.
    let mut hints: IndexMap<String, f64> = IndexMap::new();
    if let Some(lanes) = record(record(log.value("ultron.module", "hints")).and_then(|h| h.get("lanes"))) {
        for lane in lanes.values() {
            if let Some(fired) = record(lane.get("fired")) {
                for (tag, n) in fired {
                    if let Some(n) = n.as_f64().filter(|n| *n > 0.0) {
                        *hints.entry(tag.clone()).or_default() += n;
                    }
                }
            }
        }
    }
    if stats.is_none() {
        unrecorded.insert(
            "guardrails.guards".into(),
            "kept only in the worker's memory when this session ran".into(),
        );
        unrecorded.insert(
            "guardrails.usageLimitBlocks".into(),
            "this session predates the count".into(),
        );
    }
    let nudges_value = |n: &[f64; 3]| json!({"toolRounds": num(n[0]), "wait": num(n[1]), "skill": num(n[2])});
    let (secrets_masked, reported_nudges) = if !external {
        (num(redactions), nudges_value(&nudges))
    } else if let Some(s) = &stats {
        (num(s.secrets_masked), nudges_value(&s.nudges))
    } else {
        unrecorded.insert(
            "guardrails.secretsMasked".into(),
            "cell output went to Claude Code; this session predates the count".into(),
        );
        unrecorded.insert(
            "guardrails.nudges".into(),
            "steers went to Claude Code with a cell result; this session predates the count".into(),
        );
        (Value::Null, Value::Null)
    };

    // Memory.
    let memory = record(log.value("ultron.memory.state", "root"))
        .and_then(|m| m.get("operations"))
        .and_then(Value::as_array)
        .filter(|ops| !ops.is_empty());
    let operations = match memory {
        Some(ops) => {
            let mut counted: IndexMap<String, f64> = IndexMap::new();
            for op in ops {
                let field = |k: &str| match op.as_object().and_then(|o| o.get(k)) {
                    None | Some(Value::Null) => "unknown".to_string(),
                    Some(v) => string(v),
                };
                *counted
                    .entry(format!("{}.{}", field("kind"), field("state")))
                    .or_default() += 1.0;
            }
            counts_value(&counted)
        }
        None => {
            unrecorded.insert(
                "memory.operations".into(),
                "no memory operation recorded: Hindsight not configured, or memory unused".into(),
            );
            Value::Null
        }
    };
    if external {
        unrecorded.insert(
            "root.models".into(),
            "Claude Code owns the root conversation: its model, tokens and cost are Claude Code's".into(),
        );
    }

    let mut root_model_list: Vec<(String, f64)> = root_models.into_iter().collect();
    root_model_list.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| locale_compare(&a.0, &b.0))
    });
    let mut models: Vec<(String, Bucket)> = buckets.models.clone().into_iter().collect();
    models.sort_by(|a, b| {
        b.1.total_tokens
            .partial_cmp(&a.1.total_tokens)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| locale_compare(&a.0, &b.0))
    });
    let mut frames_value = json!({
        "count": num(frames[0]),
        "complete": num(frames[1]),
        "incomplete": num(frames[2]),
        "failed": num(frames[3]),
        "cancelled": num(frames[4]),
        "running": num(frames[5]),
        "nested": num(frames[6]),
        "incompleteReasons": counts_value(&incomplete_reasons),
        "budgetTokens": num(budget_tokens),
        "byModel": by_model_list(frame_by_model),
    });
    let host_call = |name: &str| {
        num(stats
            .as_ref()
            .and_then(|s| s.host_calls.get(name).copied())
            .unwrap_or(0.0))
    };
    frames_value["calls"] = if stats_cover {
        json!({"infer": host_call("rlm.infer"), "map": host_call("rlm.map")})
    } else {
        Value::Null
    };
    let guards = stats.as_ref().map_or(Value::Null, |s| {
        Value::Object(
            s.guards
                .iter()
                .map(|(name, g)| {
                    (
                        name.clone(),
                        json!({
                            "checks": num(g.checks),
                            "blocked": num(g.blocked),
                            "unchecked": num(g.unchecked),
                            "afterChecks": num(g.after_checks),
                            "afterFindings": num(g.after_findings),
                            "ms": num(g.ms),
                        }),
                    )
                })
                .collect(),
        )
    });
    json!({
        "schema": SESSION_REPORT_SCHEMA,
        "session": {
            "id": log.header.id,
            "path": log.path.display().to_string(),
            "cwd": log.header.cwd,
            "name": name,
            "createdAt": num(log.header.created_at),
            "modifiedAt": num(log.modified_at),
            "bytes": num(log.bytes as f64),
            "parentSessionId": log.header.parent_session_id,
        },
        "mode": mode,
        "turns": turns,
        "root": {
            "models": root_model_list
                .into_iter()
                .map(|(model, responses)| json!({"model": model, "responses": num(responses)}))
                .collect::<Vec<_>>(),
        },
        "cells": cells,
        "depth": {
            "verdict": depth_verdict(
                level,
                frames[0],
                children.len() as f64,
                nested_children,
                typed.len() as f64,
                background.len() as f64,
            ),
            "level": num(level),
            "frames": frames_value,
            "subagents": {
                "count": num(children.len() as f64),
                "maxDepth": num(max_child_depth),
                "nested": num(nested_children),
                "completed": num(state(&children, &["completed"])),
                "failed": num(state(&children, &["failed"])),
                "cancelled": num(state(&children, &["cancelled"])),
                "interrupted": num(state(&children, &["interrupted"])),
                "running": num(state(&children, &["admitted", "running"])),
                "byModel": by_model_list(child_by_model),
                "verdicts": {
                    "verified": num(verdicts[0]),
                    "contradicted": num(verdicts[1]),
                    "unverified": num(verdicts[2]),
                    "invalid": num(verdicts[3]),
                    "unchecked": num(verdicts[4]),
                    "none": num(verdicts[5]),
                },
                "claims": {"passed": num(claims[0]), "failed": num(claims[1]), "blocked": num(claims[2])},
                "worktrees": worktrees,
            },
            "workflows": if stats_cover { json!({"runs": host_call("workflows.run")}) } else { Value::Null },
            "typedAgents": {"count": num(typed.len() as f64), "byDefinition": counts_value(&by_definition)},
            "backgroundJobs": {
                "count": num(background.len() as f64),
                "completed": num(state(&background, &["completed"])),
                "failed": num(state(&background, &["failed"])),
                "cancelled": num(state(&background, &["cancelled", "interrupted"])),
                "running": num(state(&background, &["admitted", "running"])),
            },
        },
        "usage": {
            "lanes": {
                "root": buckets.lanes[0].fields(),
                "frames": buckets.lanes[1].fields(),
                "subagents": buckets.lanes[2].fields(),
                "other": buckets.lanes[3].fields(),
            },
            "models": models
                .into_iter()
                .map(|(model, bucket)| {
                    let mut map = Map::new();
                    map.insert("model".into(), Value::String(model));
                    map.extend(bucket.fields());
                    Value::Object(map)
                })
                .collect::<Vec<_>>(),
            "total": buckets.total.fields(),
        },
        "guardrails": {
            "guards": guards,
            "secretsMasked": secrets_masked,
            "hints": counts_value(&hints),
            "nudges": reported_nudges,
            "usageLimitBlocks": stats.as_ref().map_or(Value::Null, |s| num(s.usage_limit_blocks)),
            "countersSince": match &stats {
                Some(s) if !stats_cover => num(s.since),
                _ => Value::Null,
            },
        },
        "memory": {"operations": operations},
        "unrecorded": unrecorded,
    })
}

fn counts_value(counts: &IndexMap<String, f64>) -> Value {
    Value::Object(counts.iter().map(|(k, v)| (k.clone(), num(*v))).collect())
}
