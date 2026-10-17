//! `prepareToolArguments` (ultron/rlm/extension-tools.ts over `validateToolArguments` of packages/ai): a REPL call of
//! an extension tool is validated and coerced against the tool's JSON schema as the harness does for a model's call.
//! Optional properties that are `null` are dropped, primitives are coerced to the schema's type (`"5"` to 5 for a
//! number, 5 to `"5"` for a string, a union takes the first member a coerced value satisfies), and what still does
//! not match fails with TypeBox's messages.
//!
//! Coercion follows `type`, `properties`, `additionalProperties`, `items` and `anyOf`/`oneOf`/`allOf`. The check is
//! TypeBox's for the keywords tool schemas use: `type`, `required`, `additionalProperties`, `patternProperties`,
//! `properties`, `min/maxProperties`, `items`, `min/maxItems`, `uniqueItems`, `min/maxLength`, `format` (TypeBox's
//! registered formats, crate::formats), `pattern`, the numeric bounds and `multipleOf`, `const`, `enum`, `anyOf`
//! and boolean schemas. Not checked: `oneOf`/`allOf`/`not`/`if`, `contains`, `prefixItems`, `dependent*`,
//! `propertyNames`, `unevaluated*` and `$ref`.

use crate::edit::stringify_indented;
use crate::js_json::js_number;
use serde_json::{Map, Value};
use session_file::js::{format_f64, trim};

/// Validate and coerce `params` for tool `name` with parameter schema `schema`.
pub fn prepare_tool_arguments(
    name: &str,
    schema: &Value,
    params: &Map<String, Value>,
) -> Result<Map<String, Value>, String> {
    let mut args = Value::Object(params.clone());
    normalize_optional_nulls(&mut args, schema);
    let args = coerce(args, schema);
    let mut errors = Vec::new();
    check(&args, schema, "", &mut errors);
    if errors.is_empty() {
        return Ok(match args {
            Value::Object(map) => map,
            _ => Map::new(),
        });
    }
    Err(failure(name, &errors, &Value::Object(params.clone())))
}

/// `validateToolArguments` for a tool whose schema is a TypeBox schema in TypeScript (the agent loop's tools): as
/// [`prepare_tool_arguments`], with TypeBox's `Value.Convert` ([`crate::typebox_convert`]) applied to an object
/// before the JSON-schema coercion. `args` may be any JSON value; the error shows it as received.
pub fn validate_tool_arguments(name: &str, schema: &Value, args: &Value) -> Result<Value, String> {
    let mut value = args.clone();
    normalize_optional_nulls(&mut value, schema);
    // TypeScript discards `Convert`'s return value: only the in-place conversion of an object schema's object counts.
    if value.is_object()
        && schema["type"] == "object"
        && schema["properties"].is_object()
        && schema.get("anyOf").is_none()
    {
        value = crate::typebox_convert::convert(schema, value);
    }
    let value = coerce(value, schema);
    let mut errors = Vec::new();
    check(&value, schema, "", &mut errors);
    if errors.is_empty() {
        return Ok(value);
    }
    Err(failure(name, &errors, args))
}

fn failure(name: &str, errors: &[(String, String)], received: &Value) -> String {
    let lines: Vec<String> = errors
        .iter()
        .map(|(path, message)| {
            let path = path.trim_start_matches('/').replace('/', ".");
            format!("  - {}: {message}", if path.is_empty() { "root" } else { &path })
        })
        .collect();
    format!(
        "Validation failed for tool \"{name}\":\n{}\n\nReceived arguments:\n{}",
        lines.join("\n"),
        stringify_indented(received, "  ")
    )
}

fn types(schema: &Value) -> Vec<&str> {
    match &schema["type"] {
        Value::String(t) => vec![t.as_str()],
        Value::Array(items) => items.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

fn matches_type(value: &Value, kind: &str) -> bool {
    match kind {
        "number" => value.is_number(),
        "integer" => value.as_f64().is_some_and(|f| f.fract() == 0.0),
        "boolean" => value.is_boolean(),
        "string" => value.is_string(),
        "null" => value.is_null(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => false,
    }
}

pub(crate) fn valid(value: &Value, schema: &Value) -> bool {
    let mut errors = Vec::new();
    check(value, schema, "", &mut errors);
    errors.is_empty()
}

fn normalize_optional_nulls(value: &mut Value, schema: &Value) {
    if let Value::Array(items) = value {
        match &schema["items"] {
            Value::Array(schemas) => {
                for (item, item_schema) in items.iter_mut().zip(schemas) {
                    normalize_optional_nulls(item, item_schema);
                }
            }
            item_schema @ Value::Object(_) => {
                for item in items {
                    normalize_optional_nulls(item, item_schema);
                }
            }
            _ => {}
        }
        return;
    }
    let (Value::Object(object), Some(properties)) = (value, schema["properties"].as_object()) else {
        return;
    };
    let required: Vec<&str> = schema["required"]
        .as_array()
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    for (key, property) in properties {
        let Some(item) = object.get_mut(key) else { continue };
        if item.is_null()
            && !required.contains(&key.as_str())
            && !property["$ref"].is_string()
            && !valid(&Value::Null, property)
        {
            object.shift_remove(key);
        } else {
            normalize_optional_nulls(item, property);
        }
    }
}

/// `Number(text)` for a string that is not blank; `None` when it is not a finite number.
fn finite_number(text: &str) -> Option<f64> {
    if trim(text).is_empty() {
        return None;
    }
    let n = session_file::js::to_number(&Value::String(text.into()));
    n.is_finite().then_some(n)
}

fn coerce_primitive(value: &Value, kind: &str) -> Value {
    match (kind, value) {
        ("number" | "integer", Value::Null) => js_number(0.0),
        ("number", Value::String(s)) => finite_number(s).map_or_else(|| value.clone(), js_number),
        ("integer", Value::String(s)) => finite_number(s)
            .filter(|n| n.fract() == 0.0)
            .map_or_else(|| value.clone(), js_number),
        ("number" | "integer", Value::Bool(b)) => js_number(f64::from(u8::from(*b))),
        ("boolean", Value::Null) => Value::Bool(false),
        ("boolean", Value::String(s)) if s == "true" => Value::Bool(true),
        ("boolean", Value::String(s)) if s == "false" => Value::Bool(false),
        ("boolean", Value::Number(n)) if n.as_f64() == Some(1.0) => Value::Bool(true),
        ("boolean", Value::Number(n)) if n.as_f64() == Some(0.0) => Value::Bool(false),
        ("string", Value::Null) => Value::String(String::new()),
        ("string", Value::Number(n)) => Value::String(format_f64(n.as_f64().unwrap_or(0.0))),
        ("string", Value::Bool(b)) => Value::String(b.to_string()),
        ("null", Value::String(s)) if s.is_empty() => Value::Null,
        ("null", Value::Number(n)) if n.as_f64() == Some(0.0) => Value::Null,
        ("null", Value::Bool(false)) => Value::Null,
        _ => value.clone(),
    }
}

fn coerce_union(value: Value, schemas: &[Value]) -> Value {
    if schemas.iter().any(|schema| valid(&value, schema)) {
        return value;
    }
    for schema in schemas {
        let candidate = coerce(value.clone(), schema);
        if valid(&candidate, schema) {
            return candidate;
        }
    }
    value
}

/// `coerceWithJsonSchema`.
fn coerce(mut value: Value, schema: &Value) -> Value {
    if let Some(all) = schema["allOf"].as_array() {
        for nested in all {
            value = coerce(value, nested);
        }
    }
    if let Some(any) = schema["anyOf"].as_array() {
        value = coerce_union(value, any);
    }
    if let Some(one) = schema["oneOf"].as_array() {
        value = coerce_union(value, one);
    }
    let kinds = types(schema);
    let union_member = kinds.len() > 1 && kinds.iter().any(|k| matches_type(&value, k));
    if !kinds.is_empty() && !union_member {
        for kind in &kinds {
            let candidate = coerce_primitive(&value, kind);
            if candidate != value {
                value = candidate;
                break;
            }
        }
    }
    if kinds.contains(&"object")
        && let Value::Object(object) = &mut value
    {
        let properties = schema["properties"].as_object();
        if let Some(properties) = properties {
            for (key, property) in properties {
                if let Some(item) = object.get_mut(key) {
                    *item = coerce(item.take(), property);
                }
            }
        }
        if let Some(additional) = schema["additionalProperties"].as_object() {
            let additional = Value::Object(additional.clone());
            for (key, item) in object.iter_mut() {
                if properties.is_some_and(|p| p.contains_key(key)) {
                    continue;
                }
                *item = coerce(item.take(), &additional);
            }
        }
    }
    if kinds.contains(&"array")
        && let Value::Array(items) = &mut value
    {
        match &schema["items"] {
            Value::Array(schemas) => {
                for (item, item_schema) in items.iter_mut().zip(schemas) {
                    *item = coerce(item.take(), item_schema);
                }
            }
            item_schema @ Value::Object(_) => {
                for item in items.iter_mut() {
                    *item = coerce(item.take(), item_schema);
                }
            }
            _ => {}
        }
    }
    value
}

/// JavaScript-style deep equality of JSON values (numbers by value: 2 and 2.0 are equal).
fn js_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(x, y)| js_equal(x, y)),
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| js_equal(v, w)))
        }
        _ => a == b,
    }
}

fn limit(n: f64) -> String {
    format_f64(n)
}

/// TypeBox's `GraphemeCount` (its own segmentation: combining marks, variation selectors, ZWJ sequences, regional
/// indicator pairs), over UTF-16 units as JavaScript strings are.
fn grapheme_count(text: &str) -> usize {
    let units: Vec<u16> = text.encode_utf16().collect();
    let point = |i: usize| -> (u32, usize) {
        let unit = u32::from(units[i]);
        if (0xD800..=0xDBFF).contains(&unit)
            && let Some(low) = units
                .get(i + 1)
                .map(|u| u32::from(*u))
                .filter(|l| (0xDC00..=0xDFFF).contains(l))
        {
            return (0x10000 + ((unit - 0xD800) << 10) + (low - 0xDC00), 2);
        }
        (unit, 1)
    };
    let modifier =
        |p: u32| matches!(p, 0x0300..=0x036F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0xFE20..=0xFE2F | 0xFE00..=0xFE0F);
    let regional = |p: u32| (0x1F1E6..=0x1F1FF).contains(&p);
    let consume = |mut i: usize| {
        while i < units.len() {
            let (p, len) = point(i);
            if !modifier(p) {
                break;
            }
            i += len;
        }
        i
    };
    let (mut count, mut index) = (0, 0);
    while index < units.len() {
        let (start, len) = point(index);
        let mut end = consume(index + len);
        while end + 1 < units.len() && point(end).0 == 0x200D {
            let (_, next) = point(end + 1);
            end = consume(end + 1 + next);
        }
        if regional(start) && end < units.len() && regional(point(end).0) {
            end += point(end).1;
        }
        index = end;
        count += 1;
    }
    count
}

/// `IsMultipleOf` with TypeBox's tolerance.
fn is_multiple_of(dividend: f64, divisor: f64) -> bool {
    if dividend.fract() == 0.0 && (1.0 / divisor) % 1.0 == 0.0 {
        return true;
    }
    let m = dividend % divisor;
    m.abs().min((m - divisor).abs()).min((m + divisor).abs()) < 1e-10
}

/// TypeBox's error pass (`ErrorSchema`), collecting `(instancePath, message)` in its order: `type`; for an object
/// `required` (one error naming every missing property, its path the first), `additionalProperties` (each extra
/// property's own errors, then one for the object), `patternProperties`, `properties`, `minProperties`,
/// `maxProperties`; for an array `items`, `maxItems`, `minItems`, `uniqueItems`; for a string `maxLength`,
/// `minLength` (grapheme clusters), `format`, `pattern`; for a number `exclusiveMaximum`, `exclusiveMinimum`,
/// `maximum`, `minimum`, `multipleOf`; then `const`, `enum`, `anyOf`. A failed keyword does not stop the others.
fn check(value: &Value, schema: &Value, path: &str, errors: &mut Vec<(String, String)>) {
    match schema {
        Value::Bool(false) => return errors.push((path.into(), "schema is false".into())),
        Value::Bool(true) => return,
        _ => {}
    }
    let kinds = types(schema);
    if !kinds.is_empty() && !kinds.iter().any(|k| matches_type(value, k)) {
        let message = match kinds.as_slice() {
            [one] => format!("must be {one}"),
            many => format!("must be either {}", many.join(" or ")),
        };
        errors.push((path.into(), message));
    }
    let number = |key: &str| schema[key].as_f64();
    if let Value::Object(object) = value {
        let missing: Vec<&str> = schema["required"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|key| !object.contains_key(*key))
            .collect();
        if let Some(first) = missing.first() {
            errors.push((
                format!("{path}/{first}"),
                format!("must have required properties {}", missing.join(", ")),
            ));
        }
        let properties = schema["properties"].as_object();
        let patterns: Vec<(fancy_regex::Regex, &Value)> = schema["patternProperties"]
            .as_object()
            .into_iter()
            .flatten()
            .filter_map(|(pattern, sub)| crate::formats::js_regex(pattern).map(|re| (re, sub)))
            .collect();
        let named = |key: &str| {
            properties.is_some_and(|p| p.contains_key(key))
                || patterns.iter().any(|(re, _)| re.is_match(key).unwrap_or(false))
        };
        let keys: Vec<&String> = session_file::js::object_entries(object)
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        let additional = &schema["additionalProperties"];
        let ignored = additional.is_null()
            || *additional == Value::Bool(true)
            || additional.as_object().is_some_and(Map::is_empty);
        if !ignored {
            let mut extra = false;
            for key in &keys {
                if named(key) {
                    continue;
                }
                let mut own = Vec::new();
                check(&object[key.as_str()], additional, &format!("{path}/{key}"), &mut own);
                if !own.is_empty() {
                    extra = true;
                    errors.extend(own);
                }
            }
            if extra {
                errors.push((path.into(), "must not have additional properties".into()));
            }
        }
        for (re, sub) in &patterns {
            for key in &keys {
                if re.is_match(key).unwrap_or(false) {
                    check(&object[key.as_str()], sub, &format!("{path}/{key}"), errors);
                }
            }
        }
        if let Some(properties) = properties {
            for (key, property) in properties {
                if let Some(item) = object.get(key) {
                    check(item, property, &format!("{path}/{key}"), errors);
                }
            }
        }
        if let Some(min) = number("minProperties").filter(|m| (object.len() as f64) < *m) {
            errors.push((
                path.into(),
                format!("must not have fewer than {} properties", limit(min)),
            ));
        }
        if let Some(max) = number("maxProperties").filter(|m| (object.len() as f64) > *m) {
            errors.push((
                path.into(),
                format!("must not have more than {} properties", limit(max)),
            ));
        }
    }
    if let Value::Array(items) = value {
        if let item_schema @ (Value::Object(_) | Value::Bool(_)) = &schema["items"] {
            for (index, item) in items.iter().enumerate() {
                check(item, item_schema, &format!("{path}/{index}"), errors);
            }
        }
        if let Some(max) = number("maxItems").filter(|m| (items.len() as f64) > *m) {
            errors.push((path.into(), format!("must not have more than {} items", limit(max))));
        }
        if let Some(min) = number("minItems").filter(|m| (items.len() as f64) < *m) {
            errors.push((path.into(), format!("must not have fewer than {} items", limit(min))));
        }
        let unique = schema.get("uniqueItems").is_some_and(|u| *u != Value::Bool(false));
        if unique
            && items
                .iter()
                .enumerate()
                .any(|(i, a)| items[..i].iter().any(|b| js_equal(a, b)))
        {
            errors.push((path.into(), "must not have duplicate items".into()));
        }
    }
    if let Value::String(text) = value {
        let length = || grapheme_count(text) as f64;
        if let Some(max) = number("maxLength").filter(|m| length() > *m) {
            errors.push((
                path.into(),
                format!("must not have more than {} characters", limit(max)),
            ));
        }
        if let Some(min) = number("minLength").filter(|m| length() < *m) {
            errors.push((
                path.into(),
                format!("must not have fewer than {} characters", limit(min)),
            ));
        }
        if let Some(format) = schema["format"].as_str()
            && !crate::formats::matches(format, text)
        {
            errors.push((path.into(), format!("must match format \"{format}\"")));
        }
        if let Some(pattern) = schema["pattern"].as_str()
            && !crate::formats::js_regex(pattern).is_some_and(|re| re.is_match(text).unwrap_or(false))
        {
            errors.push((path.into(), format!("must match pattern \"{pattern}\"")));
        }
    }
    if let Some(n) = value.as_f64() {
        let bound = |key: &str, failed: fn(f64, f64) -> bool, comparison: &str, errors: &mut Vec<(String, String)>| {
            if let Some(limit_value) = number(key).filter(|l| failed(n, *l)) {
                errors.push((path.into(), format!("must be {comparison} {}", limit(limit_value))));
            }
        };
        bound("exclusiveMaximum", |n, l| n >= l, "<", errors);
        bound("exclusiveMinimum", |n, l| n <= l, ">", errors);
        bound("maximum", |n, l| n > l, "<=", errors);
        bound("minimum", |n, l| n < l, ">=", errors);
        if let Some(divisor) = number("multipleOf").filter(|d| !is_multiple_of(n, *d)) {
            errors.push((path.into(), format!("must be multiple of {}", limit(divisor))));
        }
    }
    if let Some(constant) = schema.get("const")
        && !js_equal(constant, value)
    {
        errors.push((path.into(), "must be equal to constant".into()));
    }
    if let Some(allowed) = schema["enum"].as_array()
        && !allowed.iter().any(|a| js_equal(a, value))
    {
        errors.push((path.into(), "must be equal to one of the allowed values".into()));
    }
    if let Some(any) = schema["anyOf"].as_array() {
        let mut branch_errors = Vec::new();
        let mut matched = false;
        for branch in any {
            let mut own = Vec::new();
            check(value, branch, path, &mut own);
            if own.is_empty() {
                matched = true;
                break;
            }
            branch_errors.extend(own);
        }
        if !matched {
            errors.extend(branch_errors);
            errors.push((path.into(), "must match a schema in anyOf".into()));
        }
    }
}
