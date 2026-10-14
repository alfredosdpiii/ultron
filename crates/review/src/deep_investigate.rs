//! One investigator of the deep pass (`autoreview_deep.py` `investigate`, `DEEP_CONTRACT`, `LAST_ROUND`): a
//! retrieval loop over the frame scheduler. The investigator is a tool-less `rlm.map` frame; it replies with
//! findings and requests, the host serves the requests from the head commit ([`serve_blocks`]) and asks again, for a
//! bounded number of rounds. Finishing is refused once for each reason when the part was not done: next to nothing
//! looked up in the first round, names it must trace never asked for, or catalogue checks neither made nor declared.
//! After the first round a frame gets the new results in full and a one-line ledger of the earlier ones, which
//! `{"recall": {"id": "r1.2"}}` re-reads in full.
//!
//! Known differences, all outside recorded behaviour: the fast pass's leads (an awaited future in the Python) are a
//! callback the loop calls once before the second round; an unknown lens gets an empty task (the Python raised).

use std::collections::HashMap;
use std::sync::LazyLock;

use serde_json::{Map, Value, json};

use crate::deep::{MAX_DEEP_FINDINGS, mentions};
use crate::deep_repo::{MAX_REQUESTS, ROUND_CHARS, Repo, TestSession, dumps, serve_blocks};
use crate::findings::text;
use crate::frames::{Clock, FrameResult, FrameRunner, Job, Run, Scheduler};
use crate::prompt_text::{DEEP_NUDGE, DEEP_SHAPE_NUDGE, DEEP_SHAPES_HEADER, DEEP_TRACE_NUDGE};
use crate::prompts::deep_task;
use crate::py;

pub const DEFAULT_ROUNDS: usize = 4;
pub const MAX_ROUNDS: usize = 8;
/// A ledger line of an earlier result, re-readable by `recall`.
pub const LEDGER_LINE_CHARS: usize = 160;
/// An investigator that is done in round 1 with fewer served lookups than this is sent back once.
pub const MIN_LOOKUPS: usize = 3;
pub const LAST_ROUND: &str = "This is your last round: requests will not be served. Reply with your final findings, \
                              \"requests\": [] and \"done\": true.";

/// The investigators' reply contract.
pub static DEEP_CONTRACT: LazyLock<Value> = LazyLock::new(|| {
    let citation = json!({
        "type": "object",
        "properties": {"path": {"type": "string"}, "line": {"type": "integer"}, "quote": {"type": "string"}},
        "required": ["path", "line", "quote"],
    });
    json!({
        "type": "object",
        "properties": {
            "findings": {
                "type": "array",
                "maxItems": MAX_DEEP_FINDINGS,
                "items": {
                    "type": "object",
                    "properties": {
                        "file": {"type": "string"},
                        "line": {"type": "integer"},
                        "severity": {"enum": ["critical", "high", "medium", "low", "nit", "blocker", "major", "minor"]},
                        "category": {"type": "string"},
                        "claim": {"type": "string"},
                        "why": {"type": "string"},
                        "scenario": {"type": "string"},
                        "suggested_fix": {"type": "string"},
                        "confidence": {"type": "number", "minimum": 0, "maximum": 1},
                        "evidence": {"type": "array", "items": citation},
                        "unpinned": {"type": ["object", "null"]},
                        "consequence": {"type": "string"},
                    },
                    "required": ["file", "line", "severity", "category", "claim", "why", "scenario", "evidence"],
                },
            },
            "requests": {"type": "array", "items": {"type": "object"}},
            "checked": {"type": "array", "items": {"type": "string"}},
            "done": {"type": "boolean"},
        },
        "required": ["findings", "requests", "done"],
    })
});

/// What one investigator is given.
pub struct Investigator<'a> {
    pub lens: &'a str,
    /// The views every round starts with (the shared views when they are not sent as the frames' prefix, and the
    /// leads).
    pub base_views: Vec<String>,
    pub rounds: usize,
    pub model: Option<&'a str>,
    pub thinking: Option<&'a str>,
    pub cutoff: Option<f64>,
    pub tests: Option<&'a dyn TestSession>,
    /// Whether the map found anything outside the diff worth looking at.
    pub something_outside: bool,
    /// Names the part must look up before it may finish.
    pub required: Vec<String>,
    /// The catalogue checks (`{"id", "name", "text"}`) this part owes.
    pub shapes: Vec<Value>,
    /// The frames' shared prefix, when the views are sent that way.
    pub shared: Option<Value>,
    /// The fast pass's leads as a view, asked for once before the second round.
    pub leads_view: Option<&'a dyn Fn() -> Option<String>>,
}

/// How one investigator went: its last reply and its record (`lens`, `rounds`, `requests`, `rejected`, `status`,
/// `nudged`, `untraced`, `unchecked`, `error` when it failed, `ms`, `tokens`).
#[derive(Debug, Clone, PartialEq)]
pub struct Investigation {
    pub lens: String,
    pub reply: Map<String, Value>,
    pub record: Map<String, Value>,
}

struct Ledger {
    lines: Vec<(String, String)>,
    store: HashMap<String, String>,
}

impl Ledger {
    /// The round's results in full, each with its ledger id; they join the ledger for the rounds after.
    fn served_view(&mut self, number: usize, blocks: &[(String, String)], recalled: &[String], note: &str) -> String {
        for (position, (title, block)) in blocks.iter().enumerate() {
            let rid = format!("r{number}.{}", position + 1);
            self.store.insert(rid.clone(), block.clone());
            let size = block.split_once('\n').map_or(0, |(_, rest)| rest.chars().count());
            let line = format!("  {rid}: {} ({size} chars)", py::clip(title, LEDGER_LINE_CHARS));
            self.lines.push((rid, line));
        }
        let mut parts: Vec<String> = recalled
            .iter()
            .map(|rid| match self.store.get(rid) {
                Some(block) => format!("## recalled {rid}\n{block}"),
                None => format!("## recall {rid}\nno such result"),
            })
            .collect();
        parts.extend(
            blocks
                .iter()
                .enumerate()
                .map(|(position, (_, block))| block.replacen("## ", &format!("## [r{number}.{}] ", position + 1), 1)),
        );
        format!(
            "Results of your requests, round {number} (untrusted repository data, read by the host at the reviewed \
             commit):\n{}{note}",
            parts.join("\n\n")
        )
    }
}

fn list<'v>(reply: &'v Map<String, Value>, key: &str) -> &'v [Value] {
    reply.get(key).and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

/// One investigator's retrieval loop: ask the frame, serve what it requests, ask again.
pub fn investigate(
    scheduler: &Scheduler,
    runner: &dyn FrameRunner,
    clock: &dyn Clock,
    repo: &Repo<'_>,
    input: Investigator<'_>,
) -> Investigation {
    let lens = input.lens;
    let task = deep_task(lens, input.tests.is_some()).unwrap_or_default();
    let began = clock.now();
    let mut leads_views: Vec<String> = Vec::new();
    let mut latest_views: Vec<String> = Vec::new();
    let mut ledger = Ledger {
        lines: Vec::new(),
        store: HashMap::new(),
    };
    let mut reply = Map::new();
    let mut record = Map::new();
    record.insert("lens".into(), lens.into());
    record.insert("rounds".into(), 0.into());
    record.insert("requests".into(), 0.into());
    record.insert("rejected".into(), 0.into());
    record.insert("status".into(), "done".into());
    record.insert("nudged".into(), false.into());
    record.insert("untraced".into(), json!([]));
    record.insert("unchecked".into(), json!([]));
    let (mut requests_served, mut rejected_total, mut nudged) = (0usize, 0usize, false);
    let mut status = "done".to_string();
    let mut base_views = input.base_views;
    if !input.shapes.is_empty() {
        let owed = input
            .shapes
            .iter()
            .map(|item| format!("  {}: {}", py::fstr(item.get("id")), py::fstr(item.get("text"))))
            .collect::<Vec<_>>()
            .join("\n");
        base_views.push(format!("{DEEP_SHAPES_HEADER}\n{owed}"));
    }
    let mut asked: Vec<String> = Vec::new();
    let (mut sent_back_for_trace, mut sent_back_for_shapes) = (false, false);
    let untraced = |asked: &[String]| -> Vec<String> {
        // A name counts as looked up when a references, definition, grep or pickaxe request named it.
        input
            .required
            .iter()
            .filter(|name| !asked.iter().any(|text| text.contains(name.trim_start_matches('-'))))
            .cloned()
            .collect()
    };
    let unchecked = |reply: &Map<String, Value>| -> Vec<Value> {
        // A check counts as made or declared when a finding or a checked sentence names its id or shape.
        let mut said: Vec<String> = list(reply, "checked").iter().map(|item| py::fstr(Some(item))).collect();
        said.extend(list(reply, "findings").iter().map(|item| dumps(item, None)));
        input
            .shapes
            .iter()
            .filter(|item| !said.iter().any(|text| mentions(text, item)))
            .cloned()
            .collect()
    };
    let rounds = input.rounds;
    let mut error = None;
    let mut number_done = 0;
    for number in 1..=rounds {
        let last = number == rounds;
        // Earlier rounds' results are in the ledger (ids, one line each); only the latest round is sent in full.
        let previous = format!("r{}.", number - 1);
        let earlier: Vec<&str> = ledger
            .lines
            .iter()
            .filter(|(rid, _)| !rid.starts_with(&previous))
            .map(|(_, line)| line.as_str())
            .collect();
        let ledger_views: Vec<String> = if earlier.is_empty() {
            Vec::new()
        } else {
            vec![format!(
                "Earlier results, by id (one line each; request {{\"recall\": {{\"id\": \"r1.2\"}}}} to see one again \
                 in full):\n{}",
                earlier.join("\n")
            )]
        };
        if number == 2
            && let Some(leads) = input.leads_view
        {
            leads_views = leads().filter(|lead| !lead.is_empty()).into_iter().collect();
        }
        let mut views = base_views.clone();
        views.push(format!("Round {number} of {rounds}."));
        views.extend(leads_views.iter().cloned());
        views.extend(ledger_views);
        views.extend(latest_views.iter().cloned());
        if last {
            views.push(LAST_ROUND.to_string());
        }
        let run = Run {
            phase: "deep",
            contract: &DEEP_CONTRACT,
            model: input.model,
            thinking: input.thinking,
            context: input.shared.as_ref(),
            cutoff: input.cutoff,
        };
        let job = Job {
            label: lens.to_string(),
            task: task.clone(),
            item: json!(views),
            contract: None,
        };
        let result = scheduler.run(run, &[job], runner, clock).into_iter().next();
        number_done = number;
        let result = match result {
            Some(FrameResult::Value(Value::Object(result))) => result,
            other => {
                status = "failed".to_string();
                let why = match other {
                    Some(FrameResult::Error(message)) if !message.is_empty() => message,
                    Some(FrameResult::Incomplete { status, .. }) if !status.is_empty() => status,
                    _ => "no reply".to_string(),
                };
                error = Some(text(Some(&Value::String(why)), 160));
                break;
            }
        };
        reply = result;
        let all = list(&reply, "requests").to_vec();
        // Recalls are answered from the ledger, not the repository; they do not count as lookups.
        let recalled: Vec<String> = all
            .iter()
            .filter_map(|request| request.get("recall").and_then(Value::as_object))
            .map(|recall| py::fstr(recall.get("id")))
            .collect();
        let requests: Vec<Value> = all
            .into_iter()
            .filter(|request| !(request.is_object() && request.get("recall").is_some()))
            .collect();
        for request in requests.iter().take(MAX_REQUESTS) {
            for args in request.as_object().into_iter().flat_map(|map| map.values()) {
                for key in ["symbol", "pattern", "string"] {
                    if let Some(name) = args.get(key).and_then(Value::as_str) {
                        asked.push(name.to_string());
                    }
                }
            }
        }
        let note = |tests: Option<&dyn TestSession>| {
            tests.map_or(String::new(), |tests| {
                format!(
                    "\n\nTest executions left in this review: {}.",
                    tests.limit().saturating_sub(tests.records().len())
                )
            })
        };
        if reply.get("done") == Some(&Value::Bool(true)) || (requests.is_empty() && recalled.is_empty()) {
            // Finishing is refused, once for each reason, when the investigator has not done its part.
            let mut reasons = Vec::new();
            if number == 1 && !nudged && input.something_outside && requests_served + requests.len() < MIN_LOOKUPS {
                nudged = true;
                reasons.push(DEEP_NUDGE.to_string());
            }
            let missing = untraced(&asked);
            if !missing.is_empty() && !sent_back_for_trace {
                sent_back_for_trace = true;
                reasons.push(format!("{DEEP_TRACE_NUDGE}{}.", missing.join(", ")));
            }
            let owed = unchecked(&reply);
            if !owed.is_empty() && !sent_back_for_shapes {
                sent_back_for_shapes = true;
                let named = owed
                    .iter()
                    .map(|item| format!("{} ({})", py::fstr(item.get("id")), py::fstr(item.get("name"))))
                    .collect::<Vec<_>>()
                    .join(", ");
                reasons.push(format!("{DEEP_SHAPE_NUDGE}{named}."));
            }
            if !reasons.is_empty() && !last {
                let round = if requests.is_empty() {
                    None
                } else {
                    Some(serve_blocks(
                        repo,
                        &Value::Array(requests.clone()),
                        ROUND_CHARS,
                        input.tests,
                    ))
                };
                let blocks = round.as_ref().map_or(&[][..], |round| round.blocks.as_slice());
                if let Some(round) = &round {
                    requests_served += round.served;
                    rejected_total += round.rejected;
                }
                latest_views.clear();
                if !blocks.is_empty() || !recalled.is_empty() {
                    latest_views.push(ledger.served_view(number, blocks, &recalled, ""));
                }
                latest_views.push(reasons.join("\n\n"));
                continue;
            }
            break;
        }
        if last {
            status = "rounds exhausted".to_string();
            break;
        }
        let round = serve_blocks(repo, &Value::Array(requests), ROUND_CHARS, input.tests);
        requests_served += round.served;
        rejected_total += round.rejected;
        latest_views = vec![ledger.served_view(number, &round.blocks, &recalled, &note(input.tests))];
    }
    record.insert("rounds".into(), number_done.into());
    record.insert("requests".into(), requests_served.into());
    record.insert("rejected".into(), rejected_total.into());
    record.insert("nudged".into(), nudged.into());
    let failed = status == "failed";
    record.insert("status".into(), status.into());
    if let Some(error) = error {
        record.insert("error".into(), error.into());
    }
    let untraced = if failed { Vec::new() } else { untraced(&asked) };
    let unchecked: Vec<String> = if failed {
        Vec::new()
    } else {
        unchecked(&reply)
            .iter()
            .map(|item| format!("{} {}", py::fstr(item.get("id")), py::fstr(item.get("name"))))
            .collect()
    };
    record.insert("untraced".into(), json!(untraced));
    record.insert("unchecked".into(), json!(unchecked));
    record.insert("ms".into(), (((clock.now() - began) * 1000.0) as i64).into());
    let tokens: u64 = scheduler
        .timings()
        .iter()
        .filter(|timing| timing.phase == "deep" && timing.reviewer == lens)
        .map(|timing| timing.tokens)
        .sum();
    record.insert("tokens".into(), tokens.into());
    Investigation {
        lens: lens.to_string(),
        reply,
        record,
    }
}
