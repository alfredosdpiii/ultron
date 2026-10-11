//! JSON values on the CBOR wire: protocol payloads are JSON, so both directions are total.

use protocol::Value;
use serde_json::Value as Json;

pub fn to_cbor(json: &Json) -> Value {
    match json {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Bool(*b),
        Json::Number(n) => n
            .as_i64()
            .map_or_else(|| Value::Float(n.as_f64().unwrap_or(f64::NAN)), Value::Int),
        Json::String(s) => Value::Text(s.clone()),
        Json::Array(items) => Value::Array(items.iter().map(to_cbor).collect()),
        Json::Object(map) => Value::Map(map.iter().map(|(k, v)| (k.clone(), to_cbor(v))).collect()),
    }
}

/// `None` for byte strings and non-finite floats, which are not JSON.
pub fn to_json(value: &Value) -> Option<Json> {
    Some(match value {
        Value::Null => Json::Null,
        Value::Bool(b) => Json::Bool(*b),
        Value::Int(n) => Json::from(*n),
        Value::Float(f)
            if f.fract() == 0.0 && f.abs() < 9_007_199_254_740_992.0 && !(*f == 0.0 && f.is_sign_negative()) =>
        {
            Json::from(*f as i64)
        }
        Value::Float(f) => Json::from(serde_json::Number::from_f64(*f)?),
        Value::Text(s) => Json::String(s.clone()),
        Value::Bytes(_) => return None,
        Value::Array(items) => Json::Array(items.iter().map(to_json).collect::<Option<_>>()?),
        Value::Map(entries) => Json::Object(
            entries
                .iter()
                .map(|(k, v)| Some((k.clone(), to_json(v)?)))
                .collect::<Option<_>>()?,
        ),
    })
}
