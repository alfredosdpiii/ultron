//! Format-4 JSONL codec (`harness/session/jsonl/{codec,io,types}.ts`): the header, the legacy v3 header, and one
//! transaction per line (a single write is written bare, several as an array).

use serde_json::{Map, Value};
use session_file::js;

pub const JSONL_FORMAT_VERSION: i64 = 4;
pub const JSONL_STORAGE_VERSION: i64 = 1;

/// `Number.isSafeInteger(value) && value >= minimum`.
pub(crate) fn safe_integer_at_least(value: Option<&Value>, minimum: i64) -> bool {
    match value {
        Some(Value::Number(n)) => match (n.as_i64(), n.as_f64()) {
            (Some(i), _) => i >= minimum && i.unsigned_abs() < (1 << 53),
            (None, Some(f)) => f.fract() == 0.0 && f.abs() <= 9_007_199_254_740_991.0 && f >= minimum as f64,
            _ => false,
        },
        _ => false,
    }
}

fn optional_string(map: &Map<String, Value>, key: &str) -> bool {
    map.get(key).is_none_or(Value::is_string)
}

/// `isJsonlStorageHeader`.
pub fn is_v4_header(value: &Value) -> bool {
    let Some(map) = value.as_object() else {
        return false;
    };
    map.get("kind").and_then(Value::as_str) == Some("header")
        && map.get("v").and_then(Value::as_f64) == Some(JSONL_FORMAT_VERSION as f64)
        && map.get("id").is_some_and(Value::is_string)
        && map.get("cwd").is_some_and(Value::is_string)
        && safe_integer_at_least(map.get("storageVersion"), 1)
        && safe_integer_at_least(map.get("createdAt"), 0)
        && (map.get("nextSeq").is_none() || safe_integer_at_least(map.get("nextSeq"), 1))
        && optional_string(map, "parentSessionId")
        && optional_string(map, "legacyParentSessionPath")
}

/// `isLegacyV3SessionHeader`.
pub fn is_v3_header(value: &Value) -> bool {
    let Some(map) = value.as_object() else {
        return false;
    };
    map.get("type").and_then(Value::as_str) == Some("session")
        && map.get("version").and_then(Value::as_f64) == Some(3.0)
        && map.get("id").is_some_and(Value::is_string)
        && map.get("cwd").is_some_and(Value::is_string)
        && map
            .get("timestamp")
            .and_then(Value::as_str)
            .is_some_and(|t| js::parse_date(t).is_some())
        && optional_string(map, "parentSession")
}

/// A parsed first line.
#[derive(Clone, Debug, PartialEq)]
pub enum ParsedHeader {
    V4(Map<String, Value>),
    V3(Map<String, Value>),
}

/// `parseJsonlSessionHeader`.
pub fn parse_header(line: &str) -> Result<ParsedHeader, String> {
    let value: Value =
        serde_json::from_str(line).map_err(|_| "Invalid JSONL session header: not valid JSON".to_string())?;
    if is_v4_header(&value) {
        return Ok(ParsedHeader::V4(into_object(value)));
    }
    if is_v3_header(&value) {
        return Ok(ParsedHeader::V3(into_object(value)));
    }
    Err("Unsupported JSONL session header".into())
}

pub(crate) fn into_object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

fn parse_committed_write(value: Value) -> Result<Value, String> {
    let Some(map) = value.as_object() else {
        return Err("Invalid JSONL transaction write".into());
    };
    if !safe_integer_at_least(map.get("seq"), 1) {
        return Err("Invalid JSONL write seq".into());
    }
    let op = map.get("op").map(js_string).unwrap_or_else(|| "undefined".into());
    match map.get("kind").and_then(Value::as_str) {
        Some("entry") => {
            if !safe_integer_at_least(map.get("timestamp"), 0) {
                return Err("Invalid JSONL entry timestamp".into());
            }
        }
        Some("usage") => {}
        Some("value") if op == "set" || op == "delete" => {}
        Some("value") => return Err(format!("Invalid JSONL value operation: {op}")),
        Some("list") if op == "append" || op == "delete" => {}
        Some("list") => return Err(format!("Invalid JSONL list operation: {op}")),
        _ => {
            let kind = map.get("kind").map(js_string).unwrap_or_else(|| "undefined".into());
            return Err(format!("Invalid JSONL write kind: {kind}"));
        }
    }
    Ok(value)
}

/// `String(value)` for the error messages.
fn js_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => "null".into(),
        Value::Object(_) => "[object Object]".into(),
        other => js::stringify(other),
    }
}

/// `parseJsonlTransaction`: one write object or an array of them.
pub fn parse_transaction(line: &str) -> Result<Vec<Value>, String> {
    let value: Value =
        serde_json::from_str(line).map_err(|_| "Invalid JSONL transaction: not valid JSON".to_string())?;
    match value {
        Value::Array(items) => items.into_iter().map(parse_committed_write).collect(),
        other => Ok(vec![parse_committed_write(other)?]),
    }
}

/// `serializeJsonlTransaction`.
pub fn serialize_transaction(writes: &[Value]) -> String {
    match writes {
        [one] => js::stringify(one),
        many => js::stringify(&Value::Array(many.to_vec())),
    }
}

/// A header field as a string.
pub fn header_str<'a>(header: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    header.get(key).and_then(Value::as_str)
}

/// A header field as an integer.
pub fn header_i64(header: &Map<String, Value>, key: &str) -> Option<i64> {
    header
        .get(key)
        .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
}
