//! The session report as text (`ultron/session-report-text.ts`): the full report of one session and the
//! one-line-per-session table. A number the session did not keep prints as `not recorded` (`n/r` in the table), and
//! a cost nobody reported as `unknown`: neither is ever printed as zero. Plain style only (the CLI prints no color).

use crate::js::{len16, number, pad_end, pad_start, tail16, to_fixed};
use crate::localtime::Zone;
use serde_json::{Map, Value};

const NOT_RECORDED: &str = "not recorded";

pub struct TextOptions<'a> {
    pub utc: bool,
    pub home: &'a str,
    pub zone: &'a Zone,
}

pub fn format_tokens(tokens: f64) -> String {
    if tokens < 1000.0 {
        return number(tokens);
    }
    if tokens < 999_950.0 {
        return format!("{}k", to_fixed(tokens / 1000.0, 1));
    }
    format!(
        "{}M",
        to_fixed(tokens / 1_000_000.0, if tokens < 99_950_000.0 { 2 } else { 1 })
    )
}

pub fn format_elapsed(ms: f64) -> String {
    let seconds = crate::js::round(ms / 1000.0);
    if seconds < 60.0 {
        return format!("{}s", number(seconds));
    }
    let minutes = (seconds / 60.0).floor();
    if minutes < 60.0 {
        return format!("{}m{:0>2}s", number(minutes), number(seconds % 60.0));
    }
    format!("{}h{:0>2}m", number((minutes / 60.0).floor()), number(minutes % 60.0))
}

fn usd(amount: f64) -> String {
    format!("${}", to_fixed(amount, if amount >= 10.0 { 2 } else { 3 }))
}

fn n(value: &Value, pointer: &str) -> f64 {
    value.pointer(pointer).and_then(Value::as_f64).unwrap_or(0.0)
}

/// `$1.234`, `$3.287 (sub)`, `unknown`, or a sum of those; `-` when nothing was spent.
fn format_cost(cost: &Value, tokens: f64) -> String {
    let mut parts = Vec::new();
    if let Some(reported) = cost["reportedUsd"].as_f64() {
        parts.push(usd(reported));
    }
    if let Some(sub) = cost["subscriptionUsd"].as_f64() {
        parts.push(if sub > 0.0 {
            format!("{} (sub)", usd(sub))
        } else {
            "subscription".into()
        });
    }
    let unpriced = n(cost, "/unpricedResponses");
    if unpriced > 0.0 {
        parts.push(if parts.is_empty() {
            "unknown".into()
        } else {
            format!("unknown ({} unpriced)", number(unpriced))
        });
    }
    if parts.is_empty() {
        return if tokens > 0.0 { "unknown".into() } else { "-".into() };
    }
    parts.join(" + ")
}

fn format_date(at: f64, options: &TextOptions) -> String {
    let f = if options.utc {
        crate::localtime::fields(at, 0)
    } else {
        options.zone.fields(at)
    };
    format!("{}-{:02}-{:02} {:02}:{:02}", f.year, f.month, f.day, f.hour, f.minute)
}

fn short_path(path: &str, home: &str) -> String {
    if !home.is_empty() && (path == home || path.starts_with(&format!("{home}/"))) {
        format!("~{}", &path[home.len()..])
    } else {
        path.to_string()
    }
}

fn format_bytes(bytes: f64) -> String {
    if bytes < 1024.0 {
        return format!("{} B", number(bytes));
    }
    if bytes < 1024.0 * 1024.0 {
        return format!("{} kB", to_fixed(bytes / 1024.0, 1));
    }
    format!("{} MB", to_fixed(bytes / (1024.0 * 1024.0), 1))
}

fn plural(count: f64, one: &str) -> String {
    format!("{} {}{}", number(count), one, if count == 1.0 { "" } else { "s" })
}

fn non_zero(pairs: &[(&str, f64)]) -> String {
    pairs
        .iter()
        .filter(|(_, c)| *c > 0.0)
        .map(|(label, c)| format!("{} {label}", number(*c)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn entries(value: &Value) -> Vec<(&String, &Value)> {
    value
        .as_object()
        .map(|m| config::json::js_key_order(m).into_iter().map(|k| (k, &m[k])).collect())
        .unwrap_or_default()
}

fn apis_line(apis: &Value) -> String {
    entries(apis)
        .into_iter()
        .filter(|(_, c)| c.as_f64().unwrap_or(0.0) > 0.0)
        .map(|(name, c)| format!("{name} {}", number(c.as_f64().unwrap_or(0.0))))
        .collect::<Vec<_>>()
        .join(" · ")
}

fn cells_line(cells: &Value) -> String {
    let count = n(cells, "/count");
    format!(
        "{}{}",
        plural(count, "cell"),
        if count > 0.0 {
            format!(", {} failed", number(n(cells, "/failed")))
        } else {
            String::new()
        }
    )
}

fn tokens_text(bucket: &Value) -> String {
    let unmeasured = n(bucket, "/unmeasured");
    if n(bucket, "/responses") == 0.0 && unmeasured > 0.0 {
        return NOT_RECORDED.into();
    }
    format!(
        "{}{}",
        format_tokens(n(bucket, "/totalTokens")),
        if unmeasured > 0.0 { " +?" } else { "" }
    )
}

fn table(rows: &[Vec<String>], right: &[usize]) -> Vec<String> {
    let mut widths: Vec<usize> = Vec::new();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if widths.len() <= i {
                widths.push(0);
            }
            widths[i] = widths[i].max(len16(cell));
        }
    }
    rows.iter()
        .map(|row| {
            row.iter()
                .enumerate()
                .map(|(i, cell)| {
                    if right.contains(&i) {
                        pad_start(cell, widths[i])
                    } else {
                        pad_end(cell, widths[i])
                    }
                })
                .collect::<Vec<_>>()
                .join("  ")
                .trim_end_matches(|c: char| crate::js::is_space(c))
                .to_string()
        })
        .collect()
}

fn str_of<'a>(value: &'a Value, pointer: &str) -> &'a str {
    value.pointer(pointer).and_then(Value::as_str).unwrap_or("")
}

/// The full report of one session, as lines.
pub fn render_session_report(report: &Value, options: &TextOptions) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let empty = Map::new();
    let unrecorded = report["unrecorded"].as_object().unwrap_or(&empty);
    let missing = |path: &str| match unrecorded.get(path).and_then(Value::as_str).filter(|s| !s.is_empty()) {
        Some(why) => format!("{NOT_RECORDED} ({why})"),
        None => NOT_RECORDED.into(),
    };
    let row = |lines: &mut Vec<String>, name: &str, text: &str| lines.push(format!("{}{text}", pad_end(name, 11)));
    let more = |lines: &mut Vec<String>, text: &str| lines.push(format!("{}{text}", " ".repeat(11)));
    let session = &report["session"];
    let depth = &report["depth"];
    let mode = str_of(report, "/mode");
    let name = str_of(session, "/name");
    lines.push(format!(
        "Session {}{}  [{mode}]",
        str_of(session, "/id"),
        if name.is_empty() {
            String::new()
        } else {
            format!("  {name}")
        }
    ));
    lines.push(format!(
        "{}  ·  {} to {}{}  ·  {}",
        short_path(str_of(session, "/cwd"), options.home),
        format_date(n(session, "/createdAt"), options),
        format_date(n(session, "/modifiedAt"), options),
        if options.utc { " UTC" } else { "" },
        format_bytes(n(session, "/bytes"))
    ));
    lines.push(String::new());
    row(&mut lines, "Depth", str_of(depth, "/verdict"));
    let counters_since = report.pointer("/guardrails/countersSince").and_then(Value::as_f64);
    let partial = match counters_since {
        Some(since) if mode == "ultron claude" => format!(" (counted since {})", format_date(since, options)),
        _ => String::new(),
    };
    let turns = &report["turns"];
    if turns.is_null() {
        row(&mut lines, "Turns", &missing("turns"));
    } else {
        let ended = non_zero(&[
            ("aborted", n(turns, "/aborted")),
            ("failed", n(turns, "/failed")),
            ("running", n(turns, "/running")),
        ]);
        row(
            &mut lines,
            "Turns",
            &format!(
                "{}, {} wall{}{partial}",
                plural(n(turns, "/count"), "turn"),
                format_elapsed(n(turns, "/wallMs")),
                if ended.is_empty() {
                    String::new()
                } else {
                    format!(" ({ended})")
                }
            ),
        );
    }
    let root_models = report
        .pointer("/root/models")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let root_text = if !root_models.is_empty() {
        root_models
            .iter()
            .map(|m| format!("{} ({})", str_of(m, "/model"), plural(n(m, "/responses"), "response")))
            .collect::<Vec<_>>()
            .join(", ")
    } else if unrecorded
        .get("root.models")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty())
    {
        missing("root.models")
    } else {
        "no model response yet".into()
    };
    row(&mut lines, "Root", &root_text);
    let cells = &report["cells"];
    if cells.is_null() {
        row(&mut lines, "Cells", &missing("cells"));
    } else {
        row(
            &mut lines,
            "Cells",
            &format!(
                "{}{}{partial}",
                cells_line(&cells["total"]),
                if str_of(cells, "/source") == "counters" {
                    "  (runtime count)"
                } else {
                    ""
                }
            ),
        );
        let apis = apis_line(&cells["total"]["apis"]);
        if !apis.is_empty() {
            more(&mut lines, &apis);
        }
        let (sub, other) = (n(cells, "/subagents/count"), n(cells, "/other/count"));
        if sub > 0.0 || other > 0.0 {
            more(
                &mut lines,
                &format!(
                    "root {} · sub-agents {}{}",
                    number(n(cells, "/root/count")),
                    number(sub),
                    if other > 0.0 {
                        format!(" · other lanes {}", number(other))
                    } else {
                        String::new()
                    }
                ),
            );
        }
        let tools = entries(&cells["otherTools"]);
        if !tools.is_empty() {
            let text = tools
                .iter()
                .map(|(name, tool)| {
                    let failed = n(tool, "/failed");
                    format!(
                        "{name} {}{}",
                        number(n(tool, "/calls")),
                        if failed > 0.0 {
                            format!(" ({} failed)", number(failed))
                        } else {
                            String::new()
                        }
                    )
                })
                .collect::<Vec<_>>()
                .join(" · ");
            more(&mut lines, &format!("other tools: {text}"));
        }
    }

    // Depth.
    let frames = &depth["frames"];
    let subagents = &depth["subagents"];
    if n(frames, "/count") == 0.0 {
        row(&mut lines, "Frames", "0");
    } else {
        let states = non_zero(&[
            ("complete", n(frames, "/complete")),
            ("incomplete", n(frames, "/incomplete")),
            ("failed", n(frames, "/failed")),
            ("cancelled", n(frames, "/cancelled")),
            ("running", n(frames, "/running")),
        ]);
        let nested = n(frames, "/nested");
        row(
            &mut lines,
            "Frames",
            &format!(
                "{}: {states}{} · {} tokens",
                number(n(frames, "/count")),
                if nested > 0.0 {
                    format!(" · {} nested", number(nested))
                } else {
                    String::new()
                },
                tokens_text(&report["usage"]["lanes"]["frames"])
            ),
        );
        let calls = &frames["calls"];
        if !calls.is_null() {
            more(
                &mut lines,
                &format!(
                    "from {} and {}",
                    plural(n(calls, "/map"), "rlm.map call"),
                    plural(n(calls, "/infer"), "rlm.infer call")
                ),
            );
        }
        let reasons = entries(&frames["incompleteReasons"]);
        if !reasons.is_empty() {
            let text = reasons
                .iter()
                .map(|(r, c)| format!("{r} {}", number(c.as_f64().unwrap_or(0.0))))
                .collect::<Vec<_>>()
                .join(" · ");
            more(&mut lines, &format!("incomplete: {text}"));
        }
        for model in frames["byModel"].as_array().into_iter().flatten() {
            more(
                &mut lines,
                &format!(
                    "{}  {}  {} tokens",
                    str_of(model, "/model"),
                    plural(n(model, "/count"), "frame"),
                    format_tokens(n(model, "/tokens"))
                ),
            );
        }
    }
    if n(subagents, "/count") == 0.0 {
        row(&mut lines, "Sub-agents", "0");
    } else {
        let states = non_zero(&[
            ("completed", n(subagents, "/completed")),
            ("failed", n(subagents, "/failed")),
            ("cancelled", n(subagents, "/cancelled")),
            ("interrupted", n(subagents, "/interrupted")),
            ("running", n(subagents, "/running")),
        ]);
        let nested = n(subagents, "/nested");
        row(
            &mut lines,
            "Sub-agents",
            &format!(
                "{}: {states} · max depth {}{}",
                number(n(subagents, "/count")),
                number(n(subagents, "/maxDepth")),
                if nested > 0.0 {
                    format!(" ({} nested)", number(nested))
                } else {
                    String::new()
                }
            ),
        );
        let v = &subagents["verdicts"];
        let unverified = n(v, "/unverified");
        more(
            &mut lines,
            &format!(
                "verdicts: {} verified · {} contradicted · {} unverified{}",
                number(n(v, "/verified")),
                number(n(v, "/contradicted")),
                number(unverified),
                if unverified > 0.0 {
                    format!(
                        " ({})",
                        non_zero(&[
                            ("without a verdict", n(v, "/none")),
                            ("unchecked", n(v, "/unchecked")),
                            ("invalid", n(v, "/invalid")),
                        ])
                    )
                } else {
                    String::new()
                }
            ),
        );
        for model in subagents["byModel"].as_array().into_iter().flatten() {
            let (count, unmeasured) = (n(model, "/count"), n(model, "/unmeasured"));
            let tokens = if unmeasured == count {
                format!("tokens {NOT_RECORDED}")
            } else {
                format!(
                    "{} tokens{}",
                    format_tokens(n(model, "/tokens")),
                    if unmeasured > 0.0 {
                        format!(" ({} {NOT_RECORDED})", number(unmeasured))
                    } else {
                        String::new()
                    }
                )
            };
            more(
                &mut lines,
                &format!("{}  {}  {tokens}", str_of(model, "/model"), plural(count, "sub-agent")),
            );
        }
        for worktree in subagents["worktrees"].as_array().into_iter().flatten() {
            more(
                &mut lines,
                &format!(
                    "worktree {}: {}, merge {}",
                    str_of(worktree, "/branch"),
                    plural(n(worktree, "/changedFiles"), "file"),
                    worktree["merge"].as_str().unwrap_or(NOT_RECORDED)
                ),
            );
        }
    }
    let workflows = &depth["workflows"];
    row(
        &mut lines,
        "Workflows",
        &if workflows.is_null() {
            missing("depth.workflows")
        } else {
            plural(n(workflows, "/runs"), "run")
        },
    );
    let typed = entries(&depth["typedAgents"]["byDefinition"]);
    row(
        &mut lines,
        "Other work",
        &format!(
            "{}{} · {}",
            plural(n(depth, "/typedAgents/count"), "typed-agent task"),
            if typed.is_empty() {
                String::new()
            } else {
                format!(
                    " ({})",
                    typed
                        .iter()
                        .map(|(d, c)| format!("{d} {}", number(c.as_f64().unwrap_or(0.0))))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            },
            plural(n(depth, "/backgroundJobs/count"), "background job")
        ),
    );

    // Tokens and cost.
    lines.push(String::new());
    lines.push("Tokens and cost".into());
    let usage_row = |name: &str, bucket: &Value| -> Vec<String> {
        vec![
            format!("  {name}"),
            number(n(bucket, "/responses")),
            format_tokens(n(bucket, "/input")),
            format_tokens(n(bucket, "/output")),
            format_tokens(n(bucket, "/cacheRead")),
            format_tokens(n(bucket, "/cacheWrite")),
            tokens_text(bucket),
            format_cost(&bucket["cost"], n(bucket, "/totalTokens")),
        ]
    };
    let header = |first: &str| -> Vec<String> {
        [
            first,
            "responses",
            "input",
            "output",
            "cache read",
            "cache write",
            "total",
            "cost",
        ]
        .map(String::from)
        .to_vec()
    };
    let usage = &report["usage"];
    let lanes = &usage["lanes"];
    let mut rows = vec![header("  lane")];
    if mode == "ultron claude" && n(lanes, "/root/responses") == 0.0 {
        let mut r = vec!["  root".to_string()];
        r.extend(std::iter::repeat_n("n/r".to_string(), 7));
        rows.push(r);
    } else {
        rows.push(usage_row("root", &lanes["root"]));
    }
    rows.push(usage_row("frames", &lanes["frames"]));
    rows.push(usage_row("sub-agents", &lanes["subagents"]));
    if n(lanes, "/other/responses") > 0.0 || n(lanes, "/other/unmeasured") > 0.0 {
        rows.push(usage_row("other", &lanes["other"]));
    }
    rows.push(usage_row("total", &usage["total"]));
    let numeric = [1, 2, 3, 4, 5, 6];
    lines.extend(table(&rows, &numeric));
    let models = usage["models"].as_array().cloned().unwrap_or_default();
    if !models.is_empty() {
        let mut model_rows = vec![header("  model")];
        for model in &models {
            model_rows.push(usage_row(str_of(model, "/model"), model));
        }
        lines.extend(table(&model_rows, &numeric));
    }
    let total = &usage["total"];
    let mut notes = Vec::new();
    if mode == "ultron claude" {
        notes.push(
            "root n/r: Claude Code owns the root conversation; its tokens and cost are Claude Code's, not in this session."
                .to_string(),
        );
    }
    if !total["cost"]["subscriptionUsd"].is_null() {
        notes.push(
            "(sub): made on a subscription login; the amount is the provider's notional figure, not a charge.".into(),
        );
    }
    let unpriced = n(total, "/cost/unpricedResponses");
    if unpriced > 0.0 {
        notes.push(format!(
            "unknown: {} used tokens and reported no price.",
            plural(unpriced, "response")
        ));
    }
    let unmeasured = n(total, "/unmeasured");
    if unmeasured > 0.0 {
        notes.push(format!(
            "+?: {} reported no usage; their tokens are not in the totals.",
            plural(unmeasured, "sub-agent")
        ));
    }
    for note in notes {
        lines.push(format!("  {note}"));
    }

    // Guardrails.
    lines.push(String::new());
    lines.push("Guardrails".into());
    let guardrails = &report["guardrails"];
    let since = counters_since.map_or(String::new(), |s| {
        format!(" (counted since {})", format_date(s, options))
    });
    let guards = &guardrails["guards"];
    if guards.is_null() {
        row(&mut lines, "  Loki", &missing("guardrails.guards"));
    } else if entries(guards).is_empty() {
        row(&mut lines, "  Loki", &format!("no write was checked{since}"));
    } else {
        for (name, g) in entries(guards) {
            row(
                &mut lines,
                &format!("  {name}"),
                &format!(
                    "{}, {} blocked, {} unchecked · after cells: {}, {} · {}{since}",
                    plural(n(g, "/checks"), "check"),
                    number(n(g, "/blocked")),
                    number(n(g, "/unchecked")),
                    plural(n(g, "/afterChecks"), "check"),
                    plural(n(g, "/afterFindings"), "finding"),
                    format_elapsed(n(g, "/ms"))
                ),
            );
        }
    }
    let secrets = &guardrails["secretsMasked"];
    row(
        &mut lines,
        "  Secrets",
        &if secrets.is_null() {
            missing("guardrails.secretsMasked")
        } else {
            format!("{} masked", number(secrets.as_f64().unwrap_or(0.0)))
        },
    );
    let hints = entries(&guardrails["hints"]);
    let stuck = guardrails["hints"]["stuck-loop"].as_f64().unwrap_or(0.0);
    row(
        &mut lines,
        "  Hints",
        &format!(
            "stuck-loop {}{}",
            number(stuck),
            hints
                .iter()
                .filter(|(tag, _)| tag.as_str() != "stuck-loop")
                .map(|(tag, c)| format!(" · {tag} {}", number(c.as_f64().unwrap_or(0.0))))
                .collect::<String>()
        ),
    );
    let nudges = &guardrails["nudges"];
    row(
        &mut lines,
        "  Nudges",
        &if nudges.is_null() {
            missing("guardrails.nudges")
        } else {
            format!(
                "tool rounds {} · wait {} · skill {}",
                number(n(nudges, "/toolRounds")),
                number(n(nudges, "/wait")),
                number(n(nudges, "/skill"))
            )
        },
    );
    let limits = &guardrails["usageLimitBlocks"];
    row(
        &mut lines,
        "  Limits",
        &if limits.is_null() {
            missing("guardrails.usageLimitBlocks")
        } else {
            format!("{}{since}", plural(limits.as_f64().unwrap_or(0.0), "usage-limit block"))
        },
    );

    // Memory.
    lines.push(String::new());
    lines.push("Memory".into());
    let operations = &report["memory"]["operations"];
    if operations.is_null() {
        row(&mut lines, "  Store", &missing("memory.operations"));
    } else {
        let text = entries(operations)
            .iter()
            .map(|(op, c)| format!("{op} {}", number(c.as_f64().unwrap_or(0.0))))
            .collect::<Vec<_>>()
            .join(" · ");
        row(&mut lines, "  Store", &text);
    }
    lines
}

fn table_model(report: &Value) -> String {
    let models = report.pointer("/root/models").and_then(Value::as_array);
    match models.and_then(|m| m.first()) {
        Some(first) => {
            let model = str_of(first, "/model").to_string();
            let count = models.map_or(0, Vec::len);
            if count > 1 {
                format!("{model} +{}", count - 1)
            } else {
                model
            }
        }
        None if str_of(report, "/mode") == "ultron claude" => "Claude Code".into(),
        None => "-".into(),
    }
}

fn clip(text: &str, width: usize) -> String {
    if len16(text) <= width {
        text.to_string()
    } else {
        format!("…{}", tail16(text, len16(text) - width + 1))
    }
}

/// One line per session, in the order given.
pub fn render_session_table(reports: &[Value], options: &TextOptions) -> Vec<String> {
    let mut rows: Vec<Vec<String>> = vec![
        [
            "LAST ACTIVE",
            "ID",
            "CWD",
            "MODE",
            "MODEL",
            "TURNS",
            "CELLS",
            "FRAMES",
            "SUBS",
            "TOKENS",
            "COST",
            "DEPTH",
        ]
        .map(String::from)
        .to_vec(),
    ];
    let mut unrecorded = false;
    for report in reports {
        let mut count = |value: Option<f64>| match value {
            None => {
                unrecorded = true;
                "n/r".to_string()
            }
            Some(v) => number(v),
        };
        let total = &report["usage"]["total"];
        let unknown_usage = str_of(report, "/mode") == "ultron claude"
            && n(total, "/responses") == 0.0
            && n(total, "/unmeasured") == 0.0;
        let id = str_of(report, "/session/id");
        let turns = report.pointer("/turns/count").and_then(Value::as_f64);
        let cells = report.pointer("/cells/total/count").and_then(Value::as_f64);
        let turns = count(turns);
        let cells = count(cells);
        let (tokens, cost) = if unknown_usage {
            (count(None), count(None))
        } else {
            (
                tokens_text(total),
                format_cost(&total["cost"], n(total, "/totalTokens")),
            )
        };
        rows.push(vec![
            format_date(n(report, "/session/modifiedAt"), options),
            crate::js::head16(id, 13).to_string(),
            clip(&short_path(str_of(report, "/session/cwd"), options.home), 32),
            str_of(report, "/mode").to_string(),
            clip(&table_model(report), 34),
            turns,
            cells,
            number(n(report, "/depth/frames/count")),
            number(n(report, "/depth/subagents/count")),
            tokens,
            cost,
            str_of(report, "/depth/verdict").to_string(),
        ]);
    }
    let mut lines = table(&rows, &[5, 6, 7, 8, 9]);
    if unrecorded {
        lines.push(
            "n/r: not recorded by that session (never zero). Tokens and cost of an `ultron claude` session leave out the root, which is Claude Code's."
                .into(),
        );
    }
    lines
}
