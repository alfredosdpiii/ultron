//! Chord `parseServiceCall` (packages/chord/src/services/wire.ts): the server rejects any request whose
//! opaque `call` is not a strict service call before routing it.

use crate::cbor::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct ServiceCall {
    pub service_id: String,
    /// Keyed instance address: `(key, generation)`.
    pub instance: Option<(String, i64)>,
    pub member: String,
    pub args: Vec<Value>,
}

fn strict(value: &Value, required: &[&str], optional: &[&str]) -> bool {
    let Value::Map(entries) = value else { return false };
    entries
        .iter()
        .all(|(k, _)| required.contains(&k.as_str()) || optional.contains(&k.as_str()))
        && required.iter().all(|k| value.get(k).is_some())
}

fn id(value: Option<&Value>) -> Option<String> {
    value?.as_str().filter(|s| !s.is_empty()).map(str::to_owned)
}

fn generation(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Int(n) if *n >= 1 => Some(*n),
        _ => None,
    }
}

pub fn parse_service_call(value: &Value) -> Option<ServiceCall> {
    if !strict(value, &["serviceId", "member", "args"], &["instance"]) {
        return None;
    }
    let Some(Value::Array(args)) = value.get("args") else {
        return None;
    };
    let instance = match value.get("instance") {
        None => None,
        Some(address) => {
            if !strict(address, &["key", "generation"], &[]) {
                return None;
            }
            Some((id(address.get("key"))?, generation(address.get("generation"))?))
        }
    };
    Some(ServiceCall {
        service_id: id(value.get("serviceId"))?,
        instance,
        member: id(value.get("member"))?,
        args: args.clone(),
    })
}
