//! TypeBox 1.x `Value.Convert` (typebox/build/value/convert), the conversion `validateToolArguments` applies to a
//! TypeBox tool schema before its JSON-schema coercion. TypeBox dispatches on a schema's hidden `~kind`; a declared
//! tool schema is its JSON form, so the kind is read from the shape: `anyOf` is a Union, `const` a Literal, and
//! otherwise `type` (`object` with `properties`, `array` with one `items` schema, `string`, `number`, `integer`,
//! `boolean`, `null`). Other kinds (Record, Tuple, Ref, Intersect, Enum, BigInt) convert nothing, as TypeBox does for
//! a value of no kind.

use serde_json::Value;
use session_file::js::{format_f64, to_number};

use crate::js_json::js_number;
use crate::validation::valid;

const MAX_SAFE: f64 = 9_007_199_254_740_991.0;

/// `FromType`: the converted value (TypeBox mutates objects in place and returns them; this returns the result).
pub(crate) fn convert(schema: &Value, value: Value) -> Value {
    if let Some(members) = schema["anyOf"].as_array() {
        return from_union(schema, members, value);
    }
    if let Some(constant) = schema.get("const") {
        return from_literal(constant, value);
    }
    match schema["type"].as_str() {
        Some("object") if schema["properties"].is_object() => from_object(schema, value),
        Some("array") if schema["items"].is_object() => {
            let items = match value {
                Value::Array(items) => items,
                other => vec![other],
            };
            Value::Array(items.into_iter().map(|item| convert(&schema["items"], item)).collect())
        }
        Some("string") => try_string(&value).unwrap_or(value),
        Some("number") => try_number(&value).map_or(value, js_number),
        Some("integer") => try_number(&value).map_or(value, |n| js_number(n.trunc())),
        Some("boolean") => try_boolean(&value).map_or(value, Value::Bool),
        Some("null") => {
            if try_null(&value) {
                Value::Null
            } else {
                value
            }
        }
        _ => value,
    }
}

/// `FromObject`: each present property by its schema; then, when `additionalProperties` is a schema, TypeBox's
/// `FromAdditionalProperties`, which converts every key that differs from a property name once per such property
/// (so a known property is converted again by the additional schema whenever another property exists).
fn from_object(schema: &Value, value: Value) -> Value {
    let Value::Object(mut object) = value else {
        return value;
    };
    let properties = schema["properties"].as_object().cloned().unwrap_or_default();
    let keys: Vec<String> = object.keys().cloned().collect();
    for (name, property) in &properties {
        for key in &keys {
            if key == name {
                let item = object[key].take();
                object[key] = convert(property, item);
            }
        }
    }
    if let Some(additional) = schema.get("additionalProperties").filter(|a| a.is_object()) {
        for name in properties.keys() {
            for key in &keys {
                if key != name {
                    let item = object[key].take();
                    object[key] = convert(additional, item);
                }
            }
        }
    }
    Value::Object(object)
}

/// `FromUnion`: unchanged when a member accepts the value; else the first member's conversion the union accepts.
fn from_union(schema: &Value, members: &[Value], value: Value) -> Value {
    if members.iter().any(|member| valid(&value, member)) {
        return value;
    }
    members
        .iter()
        .map(|member| convert(member, value.clone()))
        .find(|candidate| valid(candidate, schema))
        .unwrap_or(value)
}

fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        _ => a == b,
    }
}

/// `FromLiteral`: the literal when the value converts to exactly it.
fn from_literal(constant: &Value, value: Value) -> Value {
    if same(constant, &value) {
        return value;
    }
    let converted = match constant {
        Value::Bool(_) => try_boolean(&value).map(Value::Bool),
        Value::Number(_) => try_number(&value).map(js_number),
        Value::String(_) => try_string(&value),
        _ => None,
    };
    match converted {
        Some(converted) if same(constant, &converted) => converted,
        _ => value,
    }
}

/// `TryString`.
fn try_string(value: &Value) -> Option<Value> {
    match value {
        Value::Bool(b) => Some(Value::String(b.to_string())),
        Value::Number(n) => Some(Value::String(format_f64(n.as_f64().unwrap_or(0.0)))),
        Value::Null => Some(Value::String("null".into())),
        Value::String(_) => Some(value.clone()),
        _ => None,
    }
}

/// `TryNumber`: finite numbers only; a string by unary plus, then `true`/`false`, then a `123n` BigInt literal in the
/// safe range.
fn try_number(value: &Value) -> Option<f64> {
    match value {
        Value::Bool(b) => Some(f64::from(u8::from(*b))),
        Value::Number(n) => n.as_f64().filter(|n| n.is_finite()),
        Value::Null => Some(0.0),
        Value::String(text) => {
            let coerced = to_number(value);
            if coerced.is_finite() {
                return Some(coerced);
            }
            let lower = text.to_lowercase();
            if lower == "false" {
                return Some(0.0);
            }
            if lower == "true" {
                return Some(1.0);
            }
            big_int(text).filter(|n| n.abs() <= MAX_SAFE)
        }
        _ => None,
    }
}

/// `TryBigInt` of a string that `+value` rejected: `-?(0|[1-9]\d*)n`, a decimal, or an integer.
fn big_int(text: &str) -> Option<f64> {
    let integer = |s: &str| {
        let digits = s.strip_prefix('-').unwrap_or(s);
        !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) && (digits == "0" || !digits.starts_with('0'))
    };
    let parse = |s: &str| s.parse::<f64>().ok();
    if let Some(body) = text.strip_suffix('n')
        && integer(body)
    {
        return parse(body);
    }
    if let Some((whole, fraction)) = text.split_once('.')
        && integer(whole)
        && !fraction.is_empty()
        && fraction.bytes().all(|b| b.is_ascii_digit())
    {
        return parse(whole);
    }
    if integer(text) {
        return parse(text);
    }
    match text.to_lowercase().as_str() {
        "false" => Some(0.0),
        "true" => Some(1.0),
        _ => None,
    }
}

/// `TryBoolean`.
fn try_boolean(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(b) => Some(*b),
        Value::Number(n) if n.as_f64() == Some(0.0) => Some(false),
        Value::Number(n) if n.as_f64() == Some(1.0) => Some(true),
        Value::Null => Some(false),
        Value::String(text) => match text.to_lowercase().as_str() {
            "false" => Some(false),
            "true" => Some(true),
            _ if text == "0" => Some(false),
            _ if text == "1" => Some(true),
            _ => None,
        },
        _ => None,
    }
}

/// `TryNull`.
fn try_null(value: &Value) -> bool {
    match value {
        Value::Bool(b) => !b,
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::Null => true,
        Value::String(text) => {
            let lower = text.to_lowercase();
            lower == "undefined" || lower == "null" || text.is_empty() || text == "0"
        }
        _ => false,
    }
}
