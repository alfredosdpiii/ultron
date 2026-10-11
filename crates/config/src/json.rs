//! `JSON.stringify(value, null, 2)` and JavaScript property order for `serde_json` values.

use serde_json::{Map, Value};

/// Serializes like `JSON.stringify(value, null, 2)`.
pub fn stringify_pretty(value: &Value) -> String {
    let mut out = String::new();
    write(&mut out, value, 0);
    out
}

/// A JavaScript array index ("0" to "4294967294" in canonical form): such keys come first, ascending.
fn array_index(key: &str) -> Option<u32> {
    let n: u32 = key.parse().ok()?;
    (n != u32::MAX && n.to_string() == key).then_some(n)
}

/// Keys in the order a JavaScript object enumerates them: array indices ascending, then insertion order.
pub fn js_key_order(map: &Map<String, Value>) -> Vec<&String> {
    let mut indices: Vec<(u32, &String)> = map.keys().filter_map(|k| array_index(k).map(|n| (n, k))).collect();
    indices.sort_by_key(|(n, _)| *n);
    let mut keys: Vec<&String> = indices.into_iter().map(|(_, k)| k).collect();
    keys.extend(map.keys().filter(|k| array_index(k).is_none()));
    keys
}

/// Formats a number the way JavaScript's `Number.prototype.toString` does.
pub fn js_number(n: &serde_json::Number) -> String {
    if n.is_i64() || n.is_u64() {
        return n.to_string();
    }
    let f = n.as_f64().unwrap_or(0.0);
    if f == 0.0 {
        return "0".into();
    }
    let abs = f.abs();
    if (1e-6..1e21).contains(&abs) {
        // Rust's shortest round-trip digits and plain notation match JavaScript's in this range.
        return format!("{f}");
    }
    let s = format!("{f:e}");
    match s.split_once('e') {
        Some((mantissa, exp)) if !exp.starts_with('-') => format!("{mantissa}e+{exp}"),
        _ => s,
    }
}

fn indent(out: &mut String, level: usize) {
    out.push('\n');
    for _ in 0..level {
        out.push_str("  ");
    }
}

fn write(out: &mut String, value: &Value, level: usize) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&js_number(n)),
        Value::String(s) => out.push_str(&Value::String(s.clone()).to_string()),
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                indent(out, level + 1);
                write(out, item, level + 1);
            }
            indent(out, level);
            out.push(']');
        }
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (i, key) in js_key_order(map).into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                indent(out, level + 1);
                out.push_str(&Value::String(key.clone()).to_string());
                out.push_str(": ");
                write(out, &map[key], level + 1);
            }
            indent(out, level);
            out.push('}');
        }
    }
}

/// `JSON.parse(stripBom(text))`.
pub fn parse(text: &str) -> Result<Value, serde_json::Error> {
    serde_json::from_str(text.strip_prefix('\u{feff}').unwrap_or(text))
}
