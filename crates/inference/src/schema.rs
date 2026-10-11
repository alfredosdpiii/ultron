//! Contract schemas: the meta-check of `validateJsonSchema` (definition-registry.ts) and the subset of typebox's
//! `Check`/`Errors` that frame replies are validated with, with typebox's messages and order.

use serde_json::{Map, Value};

const TYPES: [&str; 7] = ["null", "boolean", "object", "array", "number", "integer", "string"];

/// Keywords `validateJsonSchema` accepts but this checker does not evaluate; a contract using one is refused
/// rather than silently under-checked.
const UNCHECKED: [&str; 19] = [
    "pattern",
    "format",
    "patternProperties",
    "dependentSchemas",
    "dependentRequired",
    "dependencies",
    "prefixItems",
    "additionalItems",
    "unevaluatedProperties",
    "unevaluatedItems",
    "propertyNames",
    "contains",
    "not",
    "if",
    "allOf",
    "oneOf",
    "multipleOf",
    "minProperties",
    "maxProperties",
];

fn failure(path: &str, detail: &str) -> String {
    format!("Invalid JSON schema at {path}: {detail}")
}

fn string_array(value: &Value, path: &str) -> Result<(), String> {
    let Some(items) = value
        .as_array()
        .filter(|items| items.iter().all(|i| i.as_str().is_some_and(|s| !s.is_empty())))
    else {
        return Err(failure(path, "must be an array of nonempty strings"));
    };
    for (index, item) in items.iter().enumerate() {
        if items[..index].contains(item) {
            return Err(failure(path, "must not contain duplicates"));
        }
    }
    Ok(())
}

fn schema_map(value: &Value, path: &str) -> Result<(), String> {
    let map = value.as_object().ok_or_else(|| failure(path, "must be an object"))?;
    map.iter()
        .try_for_each(|(key, nested)| validate_schema(nested, &format!("{path}.{key}")))
}

fn non_negative_integer(value: &Value) -> bool {
    value.as_u64().is_some_and(|n| n <= 9_007_199_254_740_991)
        || value
            .as_f64()
            .is_some_and(|n| n >= 0.0 && n.fract() == 0.0 && n <= 9_007_199_254_740_991.0)
}

/// `validateJsonSchema(value, path)`: the shape checks of definition-registry.ts. Cycles cannot occur in JSON.
pub fn validate_schema(value: &Value, path: &str) -> Result<(), String> {
    // typebox's IsSchema also takes an array (and validates nothing in it).
    if value.is_array() {
        return Ok(());
    }
    let schema = value
        .as_object()
        .ok_or_else(|| failure(path, "must be a JSON schema object"))?;
    for (keyword, item) in schema {
        let at = format!("{path}.{keyword}");
        match keyword.as_str() {
            "$schema" | "$id" | "$anchor" | "$dynamicAnchor" | "$comment" | "title" | "description" | "format"
            | "pattern" | "contentEncoding" | "contentMediaType" => {
                let Some(text) = item.as_str() else {
                    return Err(failure(&at, "must be a string"));
                };
                if keyword == "pattern" && !balanced(text) {
                    return Err(failure(&at, "must be a valid regular expression"));
                }
            }
            "$ref" | "$dynamicRef" => return Err(failure(&at, "references are not supported for runtime definitions")),
            "$defs" | "definitions" | "properties" | "patternProperties" | "dependentSchemas" => schema_map(item, &at)?,
            "type" => match item {
                Value::String(name) if TYPES.contains(&name.as_str()) => {}
                Value::String(_) => return Err(failure(&at, "contains an unknown type")),
                Value::Array(names) => {
                    if names.is_empty() || names.iter().any(|n| !n.as_str().is_some_and(|n| TYPES.contains(&n))) {
                        return Err(failure(&at, "must contain only JSON Schema types"));
                    }
                    if names.iter().enumerate().any(|(i, n)| names[..i].contains(n)) {
                        return Err(failure(&at, "must not contain duplicate types"));
                    }
                }
                _ => return Err(failure(&at, "must be a JSON Schema type or array of types")),
            },
            "required" => string_array(item, &at)?,
            "enum" => {
                if !item.as_array().is_some_and(|items| !items.is_empty()) {
                    return Err(failure(&at, "must be a nonempty JSON array"));
                }
            }
            "const" => {}
            "items" | "additionalItems" => {
                if !item.is_boolean() {
                    validate_schema(item, &at)?;
                }
            }
            "prefixItems" => {
                let items = item.as_array().ok_or_else(|| failure(&at, "must be an array"))?;
                for (index, nested) in items.iter().enumerate() {
                    validate_schema(nested, &format!("{at}[{index}]"))?;
                }
            }
            "additionalProperties"
            | "unevaluatedProperties"
            | "propertyNames"
            | "contains"
            | "not"
            | "if"
            | "then"
            | "else"
            | "unevaluatedItems" => {
                if !(item.is_boolean() && (keyword == "additionalProperties" || keyword == "unevaluatedProperties")) {
                    validate_schema(item, &at)?;
                }
            }
            "allOf" | "anyOf" | "oneOf" => {
                let items = item.as_array().filter(|items| !items.is_empty());
                let items = items.ok_or_else(|| failure(&at, "must be a nonempty schema array"))?;
                for (index, nested) in items.iter().enumerate() {
                    validate_schema(nested, &format!("{at}[{index}]"))?;
                }
            }
            "dependentRequired" => {
                let map = item.as_object().ok_or_else(|| failure(&at, "must be an object"))?;
                map.iter()
                    .try_for_each(|(key, nested)| string_array(nested, &format!("{at}.{key}")))?;
            }
            "dependencies" => {
                let map = item.as_object().ok_or_else(|| failure(&at, "must be an object"))?;
                for (key, nested) in map {
                    let at = format!("{at}.{key}");
                    if nested.is_array() {
                        string_array(nested, &at)?
                    } else {
                        validate_schema(nested, &at)?
                    }
                }
            }
            "minLength" | "maxLength" | "minItems" | "maxItems" | "minContains" | "maxContains" | "minProperties"
            | "maxProperties" => {
                if !non_negative_integer(item) {
                    return Err(failure(&at, "must be a nonnegative integer"));
                }
            }
            "minimum" | "maximum" | "exclusiveMinimum" | "exclusiveMaximum" => {
                if !item.is_number() {
                    return Err(failure(&at, "must be a finite number"));
                }
            }
            "multipleOf" => {
                if !item.as_f64().is_some_and(|n| n > 0.0) {
                    return Err(failure(&at, "must be a positive finite number"));
                }
            }
            "default" => {}
            "uniqueItems" | "readOnly" | "writeOnly" | "deprecated" => {
                if !item.is_boolean() {
                    return Err(failure(&at, "must be a boolean"));
                }
            }
            "examples" => {
                if !item.is_array() {
                    return Err(failure(&at, "must be a JSON array"));
                }
            }
            _ if keyword.starts_with("x-") => {}
            _ => return Err(failure(&at, "is not a supported JSON Schema keyword")),
        }
    }
    Ok(())
}

// ponytail: bracket balance only stands in for `new RegExp(pattern)`; patterns are refused by `checkable` anyway.
fn balanced(pattern: &str) -> bool {
    let mut depth = 0i64;
    let mut escaped = false;
    for c in pattern.chars() {
        match (escaped, c) {
            (true, _) => escaped = false,
            (false, '\\') => escaped = true,
            (false, '(') => depth += 1,
            (false, ')') => depth -= 1,
            _ => {}
        }
        if depth < 0 {
            return false;
        }
    }
    depth == 0 && !escaped
}

/// The first keyword (as `$.contract.<path>`) this checker cannot evaluate, if any.
pub fn unchecked_keyword(value: &Value, path: &str) -> Option<String> {
    let schema = value.as_object()?;
    for (keyword, item) in schema {
        let at = format!("{path}.{keyword}");
        if UNCHECKED.contains(&keyword.as_str()) {
            return Some(at);
        }
        let nested = match keyword.as_str() {
            "properties" => item
                .as_object()
                .map(|map| map.iter().map(|(k, v)| (format!("{at}.{k}"), v)).collect()),
            "anyOf" => item.as_array().map(|items| {
                items
                    .iter()
                    .enumerate()
                    .map(|(i, v)| (format!("{at}[{i}]"), v))
                    .collect()
            }),
            "items" | "additionalProperties" => Some(vec![(at, item)]),
            _ => None,
        };
        let found = nested
            .unwrap_or_default()
            .into_iter()
            .find_map(|(at, item): (String, &Value)| unchecked_keyword(item, &at));
        if found.is_some() {
            return found;
        }
    }
    None
}

/// JSON value equality with JS number semantics (1 equals 1.0) and unordered object keys.
pub fn json_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(x, y)| json_equal(x, y)),
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| json_equal(v, w)))
        }
        _ => a == b,
    }
}

fn type_matches(name: &str, value: &Value) -> bool {
    match name {
        "null" => value.is_null(),
        "boolean" => value.is_boolean(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        "number" => value.is_number(),
        "integer" => value.is_i64() || value.is_u64() || value.as_f64().is_some_and(|n| n.fract() == 0.0),
        "string" => value.is_string(),
        _ => false,
    }
}

/// A JS number's `String(n)`.
fn js_number(value: &Value) -> String {
    match value.as_f64() {
        Some(n) if n.fract() == 0.0 && n.abs() < 1e21 => format!("{}", n as i64),
        _ => value.to_string(),
    }
}

/// typebox `Errors(schema, value)` as `"<instancePath or value> <message>"`, in typebox's order (empty when it checks).
pub fn errors(schema: &Value, value: &Value) -> Vec<String> {
    let mut out = Vec::new();
    collect(schema, value, "", &mut out);
    out
}

/// A numeric bound keyword, its message operator, and whether a value satisfies it.
type Bound = (&'static str, &'static str, fn(f64, f64) -> bool);

fn push(out: &mut Vec<String>, path: &str, message: &str) {
    out.push(format!("{} {message}", if path.is_empty() { "value" } else { path }));
}

fn collect(schema: &Value, value: &Value, path: &str, out: &mut Vec<String>) {
    let Some(schema) = schema.as_object() else {
        if schema == &Value::Bool(false) {
            push(out, path, "schema is false");
        }
        return;
    };
    match schema.get("type") {
        Some(Value::String(name)) if !type_matches(name, value) => push(out, path, &format!("must be {name}")),
        Some(Value::Array(names)) if !names.iter().any(|n| n.as_str().is_some_and(|n| type_matches(n, value))) => {
            let names: Vec<&str> = names.iter().filter_map(Value::as_str).collect();
            push(out, path, &format!("must be either {}", names.join(" or ")));
        }
        _ => {}
    }
    match value {
        Value::Object(object) => object_errors(schema, object, path, out),
        Value::Array(items) => array_errors(schema, items, path, out),
        Value::String(text) => {
            let length = text.chars().count() as u64;
            if let Some(min) = schema
                .get("minLength")
                .and_then(Value::as_u64)
                .filter(|min| length < *min)
            {
                push(out, path, &format!("must not have fewer than {min} characters"));
            }
            if let Some(max) = schema
                .get("maxLength")
                .and_then(Value::as_u64)
                .filter(|max| length > *max)
            {
                push(out, path, &format!("must not have more than {max} characters"));
            }
        }
        Value::Number(number) => {
            let n = number.as_f64().unwrap_or(f64::NAN);
            let bounds: [Bound; 4] = [
                ("minimum", ">=", |n, b| n >= b),
                ("maximum", "<=", |n, b| n <= b),
                ("exclusiveMinimum", ">", |n, b| n > b),
                ("exclusiveMaximum", "<", |n, b| n < b),
            ];
            for (keyword, op, holds) in bounds {
                if let Some(bound) = schema.get(keyword).filter(|b| !b.as_f64().is_some_and(|b| holds(n, b))) {
                    push(out, path, &format!("must be {op} {}", js_number(bound)));
                }
            }
        }
        _ => {}
    }
    if schema.get("const").is_some_and(|constant| !json_equal(constant, value)) {
        push(out, path, "must be equal to constant");
    }
    if let Some(options) = schema.get("enum").and_then(Value::as_array)
        && !options.iter().any(|option| json_equal(option, value))
    {
        push(out, path, "must be equal to one of the allowed values");
    }
    if let Some(options) = schema.get("anyOf").and_then(Value::as_array) {
        let nested: Vec<Vec<String>> = options.iter().map(|option| errors_at(option, value, path)).collect();
        if nested.iter().all(|errors| !errors.is_empty()) {
            out.extend(nested.into_iter().flatten());
            push(out, path, "must match a schema in anyOf");
        }
    }
}

fn errors_at(schema: &Value, value: &Value, path: &str) -> Vec<String> {
    let mut out = Vec::new();
    collect(schema, value, path, &mut out);
    out
}

fn object_errors(schema: &Map<String, Value>, object: &Map<String, Value>, path: &str, out: &mut Vec<String>) {
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        let missing: Vec<&str> = required
            .iter()
            .filter_map(Value::as_str)
            .filter(|key| !object.contains_key(*key))
            .collect();
        if !missing.is_empty() {
            push(
                out,
                path,
                &format!("must have required properties {}", missing.join(", ")),
            );
        }
    }
    let properties = schema.get("properties").and_then(Value::as_object);
    if let Some(additional) = schema.get("additionalProperties") {
        let mut failed = false;
        for (key, item) in object {
            if properties.is_some_and(|p| p.contains_key(key)) {
                continue;
            }
            let nested = errors_at(additional, item, &format!("{path}/{key}"));
            failed |= !nested.is_empty();
            out.extend(nested);
        }
        if failed {
            push(out, path, "must not have additional properties");
        }
    }
    // typebox reports the declared properties after additionalProperties.
    for (key, nested) in properties.into_iter().flatten() {
        if let Some(item) = object.get(key) {
            collect(nested, item, &format!("{path}/{key}"), out);
        }
    }
}

fn array_errors(schema: &Map<String, Value>, items: &[Value], path: &str, out: &mut Vec<String>) {
    if let Some(each) = schema.get("items").filter(|each| !each.is_boolean()) {
        for (index, item) in items.iter().enumerate() {
            collect(each, item, &format!("{path}/{index}"), out);
        }
    }
    let length = items.len() as u64;
    if let Some(min) = schema
        .get("minItems")
        .and_then(Value::as_u64)
        .filter(|min| length < *min)
    {
        push(out, path, &format!("must not have fewer than {min} items"));
    }
    if let Some(max) = schema
        .get("maxItems")
        .and_then(Value::as_u64)
        .filter(|max| length > *max)
    {
        push(out, path, &format!("must not have more than {max} items"));
    }
    if schema.get("uniqueItems") == Some(&Value::Bool(true))
        && items
            .iter()
            .enumerate()
            .any(|(i, a)| items[..i].iter().any(|b| json_equal(a, b)))
    {
        push(out, path, "must not have duplicate items");
    }
}
