//! The inspection commands (`/agents`, `/memory`, `/skills`, `/experiments`, `/goals`, `/progress`;
//! experimental/services/inspection-commands.ts): each maps its argument text to one read-only host request, which
//! the session's inspector answers, and the answer is rendered as indented `key: value` lines.

use serde_json::{Value, json};

const MAX_LINES: usize = 40;

/// The inspection command names, in their registration order.
pub const INSPECTORS: [&str; 6] = ["agents", "memory", "skills", "experiments", "goals", "progress"];

fn words(args: &str) -> Vec<&str> {
    args.split_whitespace().collect()
}

/// The host request of `/name args` (args trimmed), or the command's usage error; `None` when `name` is not an
/// inspection command.
pub fn inspection_request(name: &str, args: &str) -> Option<Result<(String, Value), String>> {
    let request = |kind: &str, payload: Value| Ok((kind.to_string(), payload));
    let words = words(args);
    let (verb, target) = (words.first().copied(), words.get(1).copied());
    Some(match name {
        "agents" if args.is_empty() => request("agents.status", json!({})),
        "agents" => request("agents.inspect", json!({ "id": args })),
        "memory" => match (verb, target) {
            (Some("why"), Some(target)) => request("memory.why", json!({ "taskId": target })),
            (None | Some("list"), _) => request("memory.list", json!({})),
            _ => Err("Usage: /memory why <task-id> | /memory list".into()),
        },
        "skills" => match (verb, target) {
            (Some("why"), Some(target)) => request("skills.why", json!({ "decision_id": target })),
            (None | Some("list"), _) => request("skills.list", json!({})),
            _ => Err("Usage: /skills why <decision-id> | /skills list".into()),
        },
        "experiments" => request("experiments.list", json!({})),
        "goals" if args.is_empty() => request("goals.list", json!({})),
        "goals" => request("goals.get", json!({ "id": args })),
        "progress" if args.is_empty() => Err("Usage: /progress <task-id>".into()),
        "progress" => request("progress.history", json!({ "task_id": args })),
        _ => return None,
    })
}

/// `JSON.stringify` of a scalar; strings as they are.
fn scalar(item: &Value) -> String {
    match item {
        Value::String(text) => text.clone(),
        other => session_file::js::stringify(other),
    }
}

fn render(lines: &mut Vec<String>, item: &Value, indent: &str, label: Option<&str>) {
    let head = match label {
        None => indent.to_string(),
        Some(label) => format!("{indent}{label}: "),
    };
    let empty = match item {
        Value::Array(items) => items.is_empty(),
        Value::Object(map) => map.is_empty(),
        _ => {
            lines.push(format!("{head}{}", scalar(item)));
            return;
        }
    };
    if empty {
        lines.push(format!("{head}(none)"));
        return;
    }
    if let Some(label) = label {
        lines.push(format!("{indent}{label}:"));
    }
    let inner = match label {
        None => indent.to_string(),
        Some(_) => format!("{indent}  "),
    };
    match item {
        Value::Array(items) => {
            for child in items {
                if child.is_array() || child.is_object() {
                    lines.push(format!("{inner}-"));
                    render(lines, child, &format!("{inner}  "), None);
                } else {
                    lines.push(format!("{inner}- {}", scalar(child)));
                }
            }
        }
        Value::Object(map) => {
            // `Object.entries`: array-index keys first, ascending, then insertion order.
            for (key, child) in session_file::js::object_entries(map) {
                render(lines, child, &inner, Some(key));
            }
        }
        _ => {}
    }
}

/// `formatInspection`: JSON as indented `key: value` lines under `title`, at most 40 lines and a count of the rest,
/// so a large record cannot flood the screen.
pub fn format_inspection(title: &str, value: &Value) -> String {
    let mut lines = vec![title.to_string()];
    render(&mut lines, value, "", None);
    if lines.len() <= MAX_LINES {
        return lines.join("\n");
    }
    let more = lines.len() - MAX_LINES;
    lines.truncate(MAX_LINES);
    lines.push(format!("… {more} more lines"));
    lines.join("\n")
}
