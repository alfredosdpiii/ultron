//! Provider request traces for offline evals (Rust-only feature; the idea is reflection-oss/mirror's
//! `*.model-calls.jsonl`, `mirror-agent/src/http/traced_model.rs`): with `ULTRON_REQUEST_TRACE=<file>` every
//! conversation request (the root lane's and each subagent conversation's) appends one JSON line to `<file>` once
//! its response arrived, holding only the DELTA from the previous request of the same lane:
//!
//! - `kind: "start"`: the lane's first request, all its messages;
//! - `kind: "append"`: the previous request's messages are a prefix of this one's; `messages` are the ones added
//!   after `base` of them;
//! - `kind: "reset"`: the prefix changed (a prompt-cache miss): `reason` (`model`, `system`, `tools`,
//!   `compaction`, `branch_summary`, `context_edit` for `ctx.forget` and other context edits, `truncated`,
//!   `prefix_changed`), `at` (the first message that differs) and all the messages.
//!
//! Plus `session`, `lane`, `request` (per lane, from 1), `model` (`provider/id`), `usage` and `stopReason` of the
//! response, and `timestamp`. [`check`] (`ultron trace check <file>`) reports every reset with its reason. The file
//! is opened for appending (several sessions may share one) with mode 0600; a failed write disables the trace
//! without touching the request.

use ai::{AssistantMessage, Model, TranscriptContext};
use serde_json::{Map, Value, json};
use session_journal::context::{BRANCH_SUMMARY_PREFIX, COMPACTION_SUMMARY_PREFIX, CONTEXT_OMITTED_TOOL_RESULT_TEXT};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

pub const TRACE_ENV: &str = "ULTRON_REQUEST_TRACE";
/// The root conversation's lane.
pub const ROOT_LANE: &str = "main";

/// How a request relates to the previous request of its lane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Delta {
    Start,
    Append { base: usize },
    Reset { reason: &'static str, at: usize },
}

/// One message's user text (string content or text blocks), for the summary markers.
fn user_text(message: &Value) -> Option<String> {
    if message["role"] != "user" {
        return None;
    }
    match &message["content"] {
        Value::String(text) => Some(text.clone()),
        Value::Array(blocks) => Some(
            blocks
                .iter()
                .filter_map(|b| b["text"].as_str())
                .collect::<Vec<_>>()
                .join(""),
        ),
        _ => None,
    }
}

fn without_tools(message: &Value) -> Value {
    let mut out = message.clone();
    if let Some(map) = out.as_object_mut() {
        map.remove("toolsAdded");
        map.remove("toolsRemoved");
        map.remove("tools_added");
        map.remove("tools_removed");
    }
    out
}

fn mentions_omission(message: &Value) -> bool {
    message.to_string().contains(CONTEXT_OMITTED_TOOL_RESULT_TEXT)
}

/// The delta of `current` (on `model`) from `previous` (the lane's last request and its model).
pub fn delta(previous: Option<(&str, &[Value])>, model: &str, current: &[Value]) -> Delta {
    let Some((previous_model, previous)) = previous else {
        return Delta::Start;
    };
    let at = previous
        .iter()
        .zip(current)
        .position(|(p, c)| p != c)
        .unwrap_or(previous.len().min(current.len()));
    if previous_model != model {
        return Delta::Reset { reason: "model", at };
    }
    if at == previous.len() {
        return Delta::Append { base: at };
    }
    if at == current.len() {
        return Delta::Reset {
            reason: "truncated",
            at,
        };
    }
    let (p, c) = (&previous[at], &current[at]);
    let reason = if p["role"] == "system" && c["role"] == "system" {
        if without_tools(p) == without_tools(c) {
            "tools"
        } else {
            "system"
        }
    } else if new_marker(previous, &current[at..], COMPACTION_SUMMARY_PREFIX) {
        "compaction"
    } else if new_marker(previous, &current[at..], BRANCH_SUMMARY_PREFIX) {
        "branch_summary"
    } else if (p["role"] == c["role"] && p["timestamp"] == c["timestamp"])
        || previous[at + 1..].contains(c)
        || mentions_omission(c)
    {
        // The same message with other content (`ctx.forget` stubs, context edits), or messages dropped.
        "context_edit"
    } else {
        "prefix_changed"
    };
    Delta::Reset { reason, at }
}

/// Whether `tail` holds a summary message (`prefix`) that `previous` does not.
fn new_marker(previous: &[Value], tail: &[Value], prefix: &str) -> bool {
    tail.iter()
        .any(|m| user_text(m).is_some_and(|t| t.starts_with(prefix)) && !previous.contains(m))
}

#[derive(Default)]
struct LaneState {
    requests: u64,
    model: String,
    messages: Vec<Value>,
}

struct TraceState {
    file: Option<std::fs::File>,
    lanes: HashMap<(String, String), LaneState>,
}

/// An open request trace.
pub struct RequestTrace {
    path: PathBuf,
    state: Mutex<TraceState>,
}

impl RequestTrace {
    /// The trace `ULTRON_REQUEST_TRACE` names (`None` when unset or empty, or when the file cannot be opened).
    pub fn from_env(var: &dyn Fn(&str) -> Option<String>) -> Option<Arc<RequestTrace>> {
        let path = var(TRACE_ENV).filter(|p| !p.trim().is_empty())?;
        RequestTrace::open(Path::new(&path)).ok().map(Arc::new)
    }

    pub fn open(path: &Path) -> std::io::Result<RequestTrace> {
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)?;
        Ok(RequestTrace {
            path: path.to_path_buf(),
            state: Mutex::new(TraceState {
                file: Some(file),
                lanes: HashMap::new(),
            }),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Record one answered request of `lane` in `session`.
    pub fn record(
        &self,
        session: &str,
        lane: &str,
        model: &Model,
        context: &TranscriptContext,
        response: &AssistantMessage,
    ) {
        let messages: Vec<Value> = context
            .messages
            .iter()
            .map(|m| serde_json::to_value(m).unwrap_or(Value::Null))
            .collect();
        let model_key = format!("{}/{}", model.provider, model.id);
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let lane_state = state.lanes.entry((session.to_string(), lane.to_string())).or_default();
        let previous = (lane_state.requests > 0).then_some((lane_state.model.as_str(), lane_state.messages.as_slice()));
        let delta = delta(previous, &model_key, &messages);
        lane_state.requests += 1;
        let mut line = Map::new();
        line.insert("session".into(), json!(session));
        line.insert("lane".into(), json!(lane));
        line.insert("request".into(), json!(lane_state.requests));
        let shown = match &delta {
            Delta::Start => {
                line.insert("kind".into(), json!("start"));
                line.insert("base".into(), json!(0));
                &messages[..]
            }
            Delta::Append { base } => {
                line.insert("kind".into(), json!("append"));
                line.insert("base".into(), json!(base));
                &messages[*base..]
            }
            Delta::Reset { reason, at } => {
                line.insert("kind".into(), json!("reset"));
                line.insert("reason".into(), json!(reason));
                line.insert("at".into(), json!(at));
                line.insert("base".into(), json!(0));
                &messages[..]
            }
        };
        line.insert("model".into(), json!(model_key));
        line.insert("messages".into(), json!(shown));
        line.insert(
            "usage".into(),
            serde_json::to_value(&response.usage).unwrap_or(Value::Null),
        );
        line.insert(
            "stopReason".into(),
            serde_json::to_value(response.stop_reason).unwrap_or(Value::Null),
        );
        line.insert("timestamp".into(), json!(crate::lane::now_ms()));
        lane_state.model = model_key;
        lane_state.messages = messages;
        let mut text = Value::Object(line).to_string();
        text.push('\n');
        // Never append after a possibly partial line: a failed write ends the trace.
        let failed = state
            .file
            .as_mut()
            .is_some_and(|file| file.write_all(text.as_bytes()).and_then(|()| file.flush()).is_err());
        if failed {
            state.file = None;
        }
    }
}

/// One reset [`check`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reset {
    /// 1-based line number.
    pub line: usize,
    pub session: String,
    pub lane: String,
    pub request: u64,
    pub reason: String,
    pub at: u64,
}

/// What [`check`] found in a trace.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Report {
    pub requests: usize,
    pub lanes: usize,
    pub resets: Vec<Reset>,
}

/// Read a trace: every request line counted, every reset listed. Malformed lines are an error naming the line.
pub fn check(text: &str) -> Result<Report, String> {
    let mut report = Report::default();
    let mut lanes = std::collections::HashSet::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let value: Value =
            serde_json::from_str(line).map_err(|e| format!("line {}: not a trace record ({e})", index + 1))?;
        let (Some(kind), Some(lane)) = (value["kind"].as_str(), value["lane"].as_str()) else {
            return Err(format!("line {}: not a trace record (no kind or lane)", index + 1));
        };
        let session = value["session"].as_str().unwrap_or("").to_string();
        lanes.insert((session.clone(), lane.to_string()));
        report.requests += 1;
        if kind == "reset" {
            report.resets.push(Reset {
                line: index + 1,
                session,
                lane: lane.to_string(),
                request: value["request"].as_u64().unwrap_or(0),
                reason: value["reason"].as_str().unwrap_or("unknown").to_string(),
                at: value["at"].as_u64().unwrap_or(0),
            });
        }
    }
    report.lanes = lanes.len();
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn system(prompt: &str, tools: &[&str]) -> Value {
        json!({"role": "system", "content": prompt, "toolsAdded": tools, "timestamp": 1})
    }
    fn user(text: &str, at: i64) -> Value {
        json!({"role": "user", "content": text, "timestamp": at})
    }
    fn result(text: &str, at: i64) -> Value {
        json!({"role": "toolResult", "toolCallId": "c1", "content": [{"type": "text", "text": text}], "timestamp": at})
    }
    const M: &str = "anthropic/claude-sonnet-4-5";

    #[test]
    fn session_core_request_trace_delta_start_and_append() {
        let first = [system("S", &["rlm"]), user("a", 2)];
        assert_eq!(delta(None, M, &first), Delta::Start);
        let second = [system("S", &["rlm"]), user("a", 2), user("b", 3)];
        assert_eq!(delta(Some((M, &first)), M, &second), Delta::Append { base: 2 });
        // A repeated request (a retry) appends nothing.
        assert_eq!(delta(Some((M, &second)), M, &second), Delta::Append { base: 3 });
    }

    #[test]
    fn session_core_request_trace_delta_reset_reasons() {
        let base = [
            system("S", &["rlm"]),
            user("a", 2),
            result("long output", 3),
            user("b", 4),
        ];
        let reset = |current: &[Value], model: &str| delta(Some((M, &base)), model, current);
        assert_eq!(
            reset(
                &[base[0].clone(), base[1].clone(), base[2].clone(), base[3].clone()],
                "openai/gpt"
            ),
            Delta::Reset { reason: "model", at: 4 }
        );
        assert_eq!(
            reset(&[system("S2", &["rlm"]), base[1].clone()], M),
            Delta::Reset {
                reason: "system",
                at: 0
            }
        );
        assert_eq!(
            reset(&[system("S", &["rlm", "x"]), base[1].clone()], M),
            Delta::Reset { reason: "tools", at: 0 }
        );
        let summary = user(&format!("{COMPACTION_SUMMARY_PREFIX}gist</summary>"), 5);
        assert_eq!(
            reset(&[base[0].clone(), summary, base[3].clone()], M),
            Delta::Reset {
                reason: "compaction",
                at: 1
            }
        );
        let branch = user(&format!("{BRANCH_SUMMARY_PREFIX}gist</summary>"), 5);
        assert_eq!(
            reset(&[base[0].clone(), base[1].clone(), branch], M),
            Delta::Reset {
                reason: "branch_summary",
                at: 2
            }
        );
        // `ctx.forget`: the tool result's content replaced by the omission stub (same message, same timestamp).
        assert_eq!(
            reset(
                &[
                    base[0].clone(),
                    base[1].clone(),
                    result(CONTEXT_OMITTED_TOOL_RESULT_TEXT, 3),
                    base[3].clone()
                ],
                M
            ),
            Delta::Reset {
                reason: "context_edit",
                at: 2
            }
        );
        // A message dropped from the middle.
        assert_eq!(
            reset(&[base[0].clone(), base[1].clone(), base[3].clone()], M),
            Delta::Reset {
                reason: "context_edit",
                at: 2
            }
        );
        assert_eq!(
            reset(&[base[0].clone(), base[1].clone()], M),
            Delta::Reset {
                reason: "truncated",
                at: 2
            }
        );
        assert_eq!(
            reset(&[base[0].clone(), user("other", 9)], M),
            Delta::Reset {
                reason: "prefix_changed",
                at: 1
            }
        );
    }

    #[test]
    fn session_core_request_trace_check_lists_resets() {
        let text = [
            r#"{"session":"s","lane":"main","request":1,"kind":"start","base":0}"#,
            r#"{"session":"s","lane":"main","request":2,"kind":"append","base":2}"#,
            "",
            r#"{"session":"s","lane":"conversation:2","request":1,"kind":"start","base":0}"#,
            r#"{"session":"s","lane":"main","request":3,"kind":"reset","reason":"compaction","at":1}"#,
        ]
        .join("\n");
        let report = check(&text).unwrap();
        assert_eq!(report.requests, 4);
        assert_eq!(report.lanes, 2);
        assert_eq!(
            report.resets,
            vec![Reset {
                line: 5,
                session: "s".into(),
                lane: "main".into(),
                request: 3,
                reason: "compaction".into(),
                at: 1,
            }]
        );
        assert_eq!(
            check("{\"kind\":1}").unwrap_err(),
            "line 1: not a trace record (no kind or lane)"
        );
        assert!(check("nope").unwrap_err().starts_with("line 1: not a trace record ("));
    }

    #[test]
    fn session_core_request_trace_opens_only_when_named() {
        assert!(RequestTrace::from_env(&|_| None).is_none());
        assert!(RequestTrace::from_env(&|_| Some("  ".into())).is_none());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let wanted = path.display().to_string();
        let trace = RequestTrace::from_env(&|k| (k == TRACE_ENV).then(|| wanted.clone())).unwrap();
        assert_eq!(trace.path(), path);
        assert!(path.exists());
    }
}
