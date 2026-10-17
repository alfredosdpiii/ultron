//! What the session adds to the typed-agent host's `agents.status` (session-worker.ts `statusExtras`): the shell
//! jobs and the REPL tool calls the calling lane may see (the root lane all of them, another lane its own), bounded,
//! for the TUI's graph view (its job and tool nodes).

use serde_json::{Map, Value, json};

/// `text.slice(0, n)` of a JavaScript string: the first `n` UTF-16 code units (a surrogate pair is not split).
fn js_head(text: &str, n: usize) -> String {
    let mut units = 0;
    let mut out = String::new();
    for c in text.chars() {
        units += c.len_utf16();
        if units > n {
            break;
        }
        out.push(c);
    }
    out
}

/// `text.slice(-n)`: the last `n` UTF-16 code units.
fn js_tail(text: &str, n: usize) -> String {
    let mut units = 0;
    let mut start = text.len();
    for (index, c) in text.char_indices().rev() {
        units += c.len_utf16();
        if units > n {
            break;
        }
        start = index;
    }
    text[start..].to_string()
}

/// `statusExtras(caller)`: `{jobs, toolCalls}` for `lane`.
pub fn status_extras(host: &host::Host, tool_calls: &kernel_apis::ToolCalls, lane: &str) -> Map<String, Value> {
    let jobs: Vec<Value> = host
        .job_records(lane)
        .into_iter()
        .take(20)
        .map(|job| {
            json!({
                "id": job.id,
                "lane": job.lane,
                "command": js_head(&job.command, 200),
                "status": job.status,
                "exitCode": job.exit_code,
                "startedAt": job.started_at,
                "endedAt": job.ended_at,
                "outputBytes": job.output_bytes,
                // Last output, bounded, for the TUI graph's job details.
                "tail": js_tail(&job.tail, 160),
            })
        })
        .collect();
    let calls: Vec<Value> = tool_calls
        .list()
        .into_iter()
        .filter(|call| lane == crate::lane::MAIN || call["lane"] == lane)
        .take(40)
        .map(|call| {
            let mut out = Map::new();
            for key in [
                "id",
                "lane",
                "source",
                "name",
                "label",
                "status",
                "startedAt",
                "endedAt",
                "input",
            ] {
                out.insert(key.into(), call.get(key).cloned().unwrap_or(Value::Null));
            }
            for key in ["preview", "error"] {
                if let Some(value) = call.get(key).filter(|value| !value.is_null()) {
                    out.insert(key.into(), value.clone());
                }
            }
            Value::Object(out)
        })
        .collect();
    let mut extras = Map::new();
    extras.insert("jobs".into(), Value::Array(jobs));
    extras.insert("toolCalls".into(), Value::Array(calls));
    extras
}

#[cfg(test)]
mod tests {
    use super::{js_head, js_tail};

    #[test]
    fn session_core_status_extras_slices_like_javascript() {
        assert_eq!(js_head("abcdef", 3), "abc");
        assert_eq!(js_tail("abcdef", 2), "ef");
        // An astral character is two UTF-16 units and is never split.
        assert_eq!(js_head("a😀b", 2), "a");
        assert_eq!(js_head("a😀b", 3), "a😀");
        assert_eq!(js_tail("a😀b", 2), "b");
        assert_eq!(js_tail("a😀b", 3), "😀b");
        assert_eq!(js_tail("ab", 160), "ab");
    }
}
