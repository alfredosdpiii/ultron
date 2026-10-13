//! JSON Schema for typed agent definitions: the meta-check of `validateJsonSchema` (definition-registry.ts) and
//! typebox 1.3's `Value.Check` / `Value.Errors` (the validator the registry checks inputs and outputs with), with
//! typebox's keyword order, error messages and its eight-errors-per-context cap.
//!
//! Checked like typebox: every keyword the meta-check accepts, the formats `date`, `time`, `date-time`, `email`,
//! `uuid`, `ipv4`, `duration` and `json-pointer`, and string lengths in typebox's grapheme count. Refused at
//! registration instead of being checked differently ([`unchecked_keyword`]): `unevaluatedProperties`,
//! `unevaluatedItems` (evaluated-key tracking) and the formats typebox checks with grammars not ported here
//! (`hostname`, `ipv6`, `uri`, ...). Unknown format names pass, as in typebox.

use fancy_regex::Regex;
use serde_json::{Map, Value, json};

const TYPES: [&str; 7] = ["null", "boolean", "object", "array", "number", "integer", "string"];
/// typebox `Settings.maxErrors`.
const MAX_ERRORS: usize = 8;
/// Formats typebox registers whose checks are not ported: a schema naming one is refused at registration.
const UNPORTED_FORMATS: [&str; 13] = [
    "hostname",
    "idn-email",
    "idn-hostname",
    "ipv6",
    "iri-reference",
    "iri",
    "json-pointer-uri-fragment",
    "regex",
    "relative-json-pointer",
    "uri-reference",
    "uri-template",
    "uri",
    "url",
];

fn failure(path: &str, detail: &str) -> String {
    format!("Invalid JSON schema at {path}: {detail}")
}

fn non_negative_integer(value: &Value) -> bool {
    value
        .as_f64()
        .is_some_and(|n| n >= 0.0 && n.fract() == 0.0 && n <= 9_007_199_254_740_991.0)
}

fn string_array(value: &Value, path: &str) -> Result<(), String> {
    let Some(items) = value
        .as_array()
        .filter(|items| items.iter().all(|i| i.as_str().is_some_and(|s| !s.is_empty())))
    else {
        return Err(failure(path, "must be an array of nonempty strings"));
    };
    if items.iter().enumerate().any(|(i, item)| items[..i].contains(item)) {
        return Err(failure(path, "must not contain duplicates"));
    }
    Ok(())
}

fn schema_map(value: &Value, path: &str) -> Result<(), String> {
    let map = value.as_object().ok_or_else(|| failure(path, "must be an object"))?;
    map.iter()
        .try_for_each(|(key, nested)| validate_json_schema(nested, &format!("{path}.{key}")))
}

fn schema_list(value: &Value, path: &str) -> Result<(), String> {
    let items = value
        .as_array()
        .filter(|items| !items.is_empty())
        .ok_or_else(|| failure(path, "must be a nonempty schema array"))?;
    items
        .iter()
        .enumerate()
        .try_for_each(|(index, nested)| validate_json_schema(nested, &format!("{path}[{index}]")))
}

/// `validateJsonSchema(value, path)` of definition-registry.ts. JSON cannot be cyclic.
pub fn validate_json_schema(value: &Value, path: &str) -> Result<(), String> {
    // typebox's IsSchema takes an array too; its entries are then read as keywords named by index.
    if let Some(items) = value.as_array() {
        return match items.is_empty() {
            true => Ok(()),
            false => Err(failure(&format!("{path}.0"), "is not a supported JSON Schema keyword")),
        };
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
                if keyword == "pattern" && Regex::new(text).is_err() {
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
                    validate_json_schema(item, &at)?;
                }
            }
            "prefixItems" => {
                let items = item.as_array().ok_or_else(|| failure(&at, "must be an array"))?;
                for (index, nested) in items.iter().enumerate() {
                    validate_json_schema(nested, &format!("{path}.prefixItems[{index}]"))?;
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
                    validate_json_schema(item, &at)?;
                }
            }
            "allOf" | "anyOf" | "oneOf" => schema_list(item, &at)?,
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
                        string_array(nested, &at)?;
                    } else {
                        validate_json_schema(nested, &at)?;
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

/// The first keyword (as `<path>.<keyword>`) this checker would evaluate differently from typebox, if any.
pub fn unchecked_keyword(schema: &Value, path: &str) -> Option<String> {
    let map = schema.as_object()?;
    map.iter().find_map(|(keyword, item)| {
        let at = format!("{path}.{keyword}");
        match keyword.as_str() {
            "unevaluatedProperties" | "unevaluatedItems" => Some(at),
            "format" if item.as_str().is_some_and(|f| UNPORTED_FORMATS.contains(&f)) => Some(at),
            "properties" | "patternProperties" | "$defs" | "definitions" | "dependentSchemas" | "dependencies" => item
                .as_object()?
                .iter()
                .find_map(|(key, nested)| unchecked_keyword(nested, &format!("{at}.{key}"))),
            "allOf" | "anyOf" | "oneOf" | "prefixItems" => item
                .as_array()?
                .iter()
                .enumerate()
                .find_map(|(index, nested)| unchecked_keyword(nested, &format!("{at}[{index}]"))),
            "items" if item.is_array() => item
                .as_array()?
                .iter()
                .enumerate()
                .find_map(|(index, nested)| unchecked_keyword(nested, &format!("{at}[{index}]"))),
            "items"
            | "additionalItems"
            | "additionalProperties"
            | "propertyNames"
            | "contains"
            | "not"
            | "if"
            | "then"
            | "else" => unchecked_keyword(item, &at),
            _ => None,
        }
    })
}

/// One typebox validation error (`TLocalizedValidationError` without `schemaPath`).
#[derive(Clone, Debug, PartialEq)]
pub struct SchemaError {
    pub keyword: String,
    pub instance_path: String,
    pub params: Value,
    pub message: String,
}

impl SchemaError {
    pub fn to_json(&self) -> Value {
        json!({ "keyword": self.keyword, "instancePath": self.instance_path, "params": self.params,
                "message": self.message })
    }
}

/// typebox `Value.Check(schema, value)`.
pub fn check(schema: &Value, value: &Value) -> bool {
    let mut context = Context::checking();
    eval(schema, value, "", &mut context)
}

/// typebox `Value.Errors(schema, value)`, in typebox's order (empty when the value checks).
pub fn errors(schema: &Value, value: &Value) -> Vec<SchemaError> {
    let mut context = Context::collecting();
    eval(schema, value, "", &mut context);
    context.errors.unwrap_or_default()
}

/// An error context: `None` errors when only checking.
struct Context {
    errors: Option<Vec<SchemaError>>,
}

impl Context {
    fn checking() -> Self {
        Context { errors: None }
    }
    fn collecting() -> Self {
        Context {
            errors: Some(Vec::new()),
        }
    }
    /// A fresh context of the same mode (typebox's per-operand context of anyOf, allOf, oneOf, if).
    fn nested(&self) -> Self {
        Context {
            errors: self.errors.as_ref().map(|_| Vec::new()),
        }
    }
    fn at_capacity(&self) -> bool {
        self.errors.as_ref().is_some_and(|errors| errors.len() >= MAX_ERRORS)
    }
    /// `AddError`: records the error (under the cap) and returns false.
    fn add(&mut self, keyword: &str, path: &str, params: Value) -> bool {
        if let Some(errors) = self.errors.as_mut()
            && errors.len() < MAX_ERRORS
        {
            let message = message(keyword, &params);
            errors.push(SchemaError {
                keyword: keyword.into(),
                instance_path: path.into(),
                params,
                message,
            });
        }
        false
    }
    fn absorb(&mut self, other: Context) {
        for error in other.errors.into_iter().flatten() {
            if let Some(errors) = self.errors.as_mut()
                && errors.len() < MAX_ERRORS
            {
                errors.push(error);
            }
        }
    }
}

/// A JavaScript number as a template literal prints it.
fn js_number(value: &Value) -> String {
    match value.as_f64() {
        Some(n) if n.fract() == 0.0 && n.abs() < 1e21 => format!("{}", n as i64),
        Some(n) => format!("{n}"),
        None => value.to_string(),
    }
}

fn joined(value: &Value) -> String {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|item| match item {
                    Value::String(text) => text.clone(),
                    other => js_number(other),
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

/// typebox's `en_US` locale.
fn message(keyword: &str, params: &Value) -> String {
    let text = |key: &str| match &params[key] {
        Value::String(text) => text.clone(),
        other => js_number(other),
    };
    match keyword {
        "additionalProperties" => "must not have additional properties".into(),
        "anyOf" => "must match a schema in anyOf".into(),
        "boolean" => "schema is false".into(),
        "const" => "must be equal to constant".into(),
        "contains" => "must contain at least 1 valid item".into(),
        "dependencies" | "dependentRequired" => format!(
            "must have properties {} when property {} is present",
            joined(&params["dependencies"]),
            text("property")
        ),
        "enum" => "must be equal to one of the allowed values".into(),
        "exclusiveMaximum" | "exclusiveMinimum" | "maximum" | "minimum" => {
            format!("must be {} {}", text("comparison"), text("limit"))
        }
        "format" => format!("must match format \"{}\"", text("format")),
        "if" => format!("must match \"{}\" schema", text("failingKeyword")),
        "maxItems" => format!("must not have more than {} items", text("limit")),
        "maxLength" => format!("must not have more than {} characters", text("limit")),
        "maxProperties" => format!("must not have more than {} properties", text("limit")),
        "minItems" => format!("must not have fewer than {} items", text("limit")),
        "minLength" => format!("must not have fewer than {} characters", text("limit")),
        "minProperties" => format!("must not have fewer than {} properties", text("limit")),
        "multipleOf" => format!("must be multiple of {}", text("multipleOf")),
        "not" => "must not be valid".into(),
        "oneOf" => "must match exactly one schema in oneOf".into(),
        "pattern" => format!("must match pattern \"{}\"", text("pattern")),
        "propertyNames" => format!("property names {} are invalid", joined(&params["propertyNames"])),
        "required" => format!(
            "must have required properties {}",
            joined(&params["requiredProperties"])
        ),
        "type" => match &params["type"] {
            Value::String(name) => format!("must be {name}"),
            names => format!(
                "must be either {}",
                names
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" or ")
            ),
        },
        "uniqueItems" => "must not have duplicate items".into(),
        _ => "an unknown validation error occurred".into(),
    }
}

/// JSON equality with JavaScript number semantics and unordered object keys (typebox `IsDeepEqual`).
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

fn is_integer(value: &Value) -> bool {
    value.is_i64() || value.is_u64() || value.as_f64().is_some_and(|n| n.fract() == 0.0 && n.is_finite())
}

fn type_matches(name: &str, value: &Value) -> bool {
    match name {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "boolean" => value.is_boolean(),
        "integer" => value.is_number() && is_integer(value),
        "number" => value.is_number(),
        "null" => value.is_null(),
        "string" => value.is_string(),
        _ => true,
    }
}

/// typebox's `GraphemeCount` (its own segmentation over UTF-16 code units).
pub fn grapheme_count(text: &str) -> usize {
    let units: Vec<u16> = text.encode_utf16().collect();
    let code_point = |index: usize| -> u32 {
        let first = u32::from(units[index]);
        if (0xD800..=0xDBFF).contains(&first) && index + 1 < units.len() {
            let second = u32::from(units[index + 1]);
            if (0xDC00..=0xDFFF).contains(&second) {
                return 0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00);
            }
        }
        first
    };
    let width = |point: u32| if point > 0xFFFF { 2 } else { 1 };
    let combining = |p: u32| {
        (0x0300..=0x036F).contains(&p)
            || (0x1AB0..=0x1AFF).contains(&p)
            || (0x1DC0..=0x1DFF).contains(&p)
            || (0xFE20..=0xFE2F).contains(&p)
    };
    let variation = |p: u32| (0xFE00..=0xFE0F).contains(&p);
    let regional = |p: u32| (0x1F1E6..=0x1F1FF).contains(&p);
    let modifiers = |mut index: usize| {
        while index < units.len() {
            let point = code_point(index);
            if combining(point) || variation(point) {
                index += width(point);
            } else {
                break;
            }
        }
        index
    };
    let mut count = 0;
    let mut index = 0;
    while index < units.len() {
        let start = code_point(index);
        let mut end = modifiers(index + width(start));
        while end + 1 < units.len() && code_point(end) == 0x200D {
            let next = code_point(end + 1);
            end = modifiers(end + 1 + width(next));
        }
        if regional(start) && end < units.len() && regional(code_point(end)) {
            end += width(code_point(end));
        }
        index = end;
        count += 1;
    }
    count
}

fn multiple_of(dividend: f64, divisor: f64) -> bool {
    if dividend.fract() == 0.0 && (1.0 / divisor).fract() == 0.0 {
        return true;
    }
    let modulo = dividend % divisor;
    modulo.abs().min((modulo - divisor).abs()).min((modulo + divisor).abs()) < 1e-10
}

fn regex_test(pattern: &str, text: &str) -> bool {
    Regex::new(pattern).is_ok_and(|regex| regex.is_match(text).unwrap_or(false))
}

fn leap_year(year: u32) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
}

fn is_date(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    let digits = |range: std::ops::Range<usize>| -> Option<u32> {
        bytes[range.clone()]
            .iter()
            .all(u8::is_ascii_digit)
            .then(|| text[range].parse().ok())
            .flatten()
    };
    let (Some(year), Some(month), Some(day)) = (digits(0..4), digits(5..7), digits(8..10)) else {
        return false;
    };
    const DAYS: [u32; 13] = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    (1..=12).contains(&month)
        && day >= 1
        && day
            <= if month == 2 && leap_year(year) {
                29
            } else {
                DAYS[month as usize]
            }
}

fn is_time(text: &str) -> bool {
    let Ok(regex) = Regex::new(r"^(\d\d):(\d\d):(\d\d)(?:\.\d+)?(?:([Zz])|([+-])(\d\d):(\d\d))?$") else {
        return false;
    };
    let Ok(Some(found)) = regex.captures(text) else {
        return false;
    };
    let number = |index: usize| -> i64 { found.get(index).and_then(|m| m.as_str().parse().ok()).unwrap_or(0) };
    let zulu = found.get(4).is_some();
    let sign = found.get(5).map(|m| m.as_str());
    if !zulu && sign.is_none() {
        return false;
    }
    let (hour, minute, second) = (number(1), number(2), number(3));
    if hour > 23 || minute > 59 || second > 60 {
        return false;
    }
    if sign.is_some() && (number(6) > 23 || number(7) > 59) {
        return false;
    }
    if second < 60 {
        return true;
    }
    let direction = if sign == Some("-") { -1 } else { 1 };
    let total = (hour * 60 + minute) - direction * (number(6) * 60 + number(7));
    (total % 1440 + 1440) % 1440 == 1439
}

/// typebox `Format.Test` for the ported formats; unknown names pass.
fn format_matches(format: &str, text: &str) -> bool {
    match format {
        "date" => is_date(text),
        "time" => is_time(text),
        "date-time" => {
            let parts: Vec<&str> = text.split(['T', 't']).collect();
            parts.len() == 2 && is_date(parts[0]) && is_time(parts[1])
        }
        "email" => regex_test(
            r#"(?i)^(?:[a-z0-9!#$%&'*+/=?^_`{|}~-]+(?:\.[a-z0-9!#$%&'*+/=?^_`{|}~-]+)*|"(?:[^"\\]|\\[\x20-\x7e])*")@(?:[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?(?:\.[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?)*|\[(?:IPv6:[a-f0-9:]+|(?:25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])(?:\.(?:25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9]?[0-9])){3})\])$"#,
            text,
        ),
        "uuid" => regex_test(r"(?i)^[0-9a-f]{8}-(?:[0-9a-f]{4}-){3}[0-9a-f]{12}$", text),
        "ipv4" => regex_test(
            r"^(?:(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)\.){3}(?:25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)$",
            text,
        ),
        "duration" => regex_test(
            r"^P((\d+Y(\d+M(\d+D)?)?|\d+M(\d+D)?|\d+D)(T(\d+H(\d+M(\d+S)?)?|\d+M(\d+S)?|\d+S))?|T(\d+H(\d+M(\d+S)?)?|\d+M(\d+S)?|\d+S)|\d+W)$",
            text,
        ),
        "json-pointer" => regex_test(r"^(?:/(?:[^~/]|~0|~1)*)*$", text),
        _ => true,
    }
}

/// `GetPropertiesPattern`: a key covered by `properties` or a `patternProperties` pattern.
fn declared(schema: &Map<String, Value>, key: &str) -> bool {
    schema
        .get("properties")
        .and_then(Value::as_object)
        .is_some_and(|properties| properties.contains_key(key))
        || schema
            .get("patternProperties")
            .and_then(Value::as_object)
            .is_some_and(|patterns| patterns.keys().any(|pattern| regex_test(pattern, key)))
}

/// `ErrorSchema` / `CheckSchema`: every keyword is evaluated (typebox's bitwise `&`), in typebox's order.
fn eval(schema: &Value, value: &Value, path: &str, context: &mut Context) -> bool {
    if context.at_capacity() {
        return false;
    }
    let schema = match schema {
        Value::Bool(true) => return true,
        Value::Bool(false) => return context.add("boolean", path, json!({})),
        Value::Object(schema) => schema,
        _ => return true,
    };
    let mut ok = true;
    if let Some(types) = schema.get("type") {
        let matches = match types {
            Value::Array(names) => names
                .iter()
                .any(|name| name.as_str().is_some_and(|name| type_matches(name, value))),
            Value::String(name) => type_matches(name, value),
            _ => true,
        };
        if !matches {
            ok &= context.add("type", path, json!({ "type": types }));
        }
    }
    match value {
        Value::Object(object) => ok &= eval_object(schema, object, path, context),
        Value::Array(items) => ok &= eval_array(schema, items, path, context),
        Value::String(text) => ok &= eval_string(schema, text, path, context),
        Value::Number(number) => ok &= eval_number(schema, number.as_f64().unwrap_or(f64::NAN), path, context),
        _ => {}
    }
    if let Some(constant) = schema.get("const")
        && !json_equal(value, constant)
    {
        ok &= context.add("const", path, json!({ "allowedValue": constant }));
    }
    if let Some(options) = schema.get("enum").and_then(Value::as_array)
        && !options.iter().any(|option| json_equal(value, option))
    {
        ok &= context.add("enum", path, json!({ "allowedValues": options }));
    }
    if let Some(condition) = schema.get("if") {
        let then = schema.get("then").unwrap_or(&Value::Bool(true));
        let otherwise = schema.get("else").unwrap_or(&Value::Bool(true));
        let mut inner = context.nested();
        let passed = if eval(condition, value, path, &mut inner) {
            eval(then, value, path, &mut inner) || context.add("if", path, json!({ "failingKeyword": "then" }))
        } else {
            eval(otherwise, value, path, context) || context.add("if", path, json!({ "failingKeyword": "else" }))
        };
        ok &= passed;
    }
    if let Some(negated) = schema.get("not")
        && eval(negated, value, path, &mut Context::checking())
    {
        ok &= context.add("not", path, json!({}));
    }
    if let Some(options) = schema.get("allOf").and_then(Value::as_array) {
        let mut failed = Vec::new();
        for option in options {
            let mut inner = context.nested();
            if !eval(option, value, path, &mut inner) {
                failed.push(inner);
            }
        }
        if !failed.is_empty() {
            for inner in failed {
                context.absorb(inner);
            }
            ok = false;
        }
    }
    if let Some(options) = schema.get("anyOf").and_then(Value::as_array) {
        let mut failed = Vec::new();
        let mut passed = 0;
        for option in options {
            let mut inner = context.nested();
            if eval(option, value, path, &mut inner) {
                passed += 1;
            } else {
                failed.push(inner);
            }
        }
        if passed == 0 {
            for inner in failed {
                context.absorb(inner);
            }
            ok &= context.add("anyOf", path, json!({}));
        }
    }
    if let Some(options) = schema.get("oneOf").and_then(Value::as_array) {
        let mut failed = Vec::new();
        let mut passing = Vec::new();
        for (index, option) in options.iter().enumerate() {
            let mut inner = context.nested();
            if eval(option, value, path, &mut inner) {
                passing.push(index);
            } else {
                failed.push(inner);
            }
        }
        if passing.len() != 1 {
            if passing.is_empty() {
                for inner in failed {
                    context.absorb(inner);
                }
            }
            ok &= context.add("oneOf", path, json!({ "passingSchemas": passing }));
        }
    }
    ok
}

fn eval_object(schema: &Map<String, Value>, object: &Map<String, Value>, path: &str, context: &mut Context) -> bool {
    let mut ok = true;
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        let missing: Vec<&Value> = required
            .iter()
            .filter(|key| key.as_str().is_some_and(|key| !object.contains_key(key)))
            .collect();
        if !missing.is_empty() {
            ok &= context.add("required", path, json!({ "requiredProperties": missing }));
        }
    }
    if let Some(additional) = schema.get("additionalProperties") {
        let mut rejected = Vec::new();
        for (key, item) in object {
            if !declared(schema, key) && !eval(additional, item, &format!("{path}/{key}"), context) {
                rejected.push(key.clone());
            }
        }
        if !rejected.is_empty() {
            ok &= context.add(
                "additionalProperties",
                path,
                json!({ "additionalProperties": rejected }),
            );
        }
    }
    let nonempty = !object.is_empty();
    if let Some(dependencies) = schema.get("dependencies").and_then(Value::as_object) {
        let mut every = true;
        for (key, dependency) in dependencies {
            if !object.contains_key(key) {
                continue;
            }
            match dependency {
                Value::Array(keys) => {
                    for needed in keys {
                        if !needed.as_str().is_some_and(|needed| object.contains_key(needed)) {
                            every &= context.add(
                                "dependencies",
                                path,
                                json!({ "property": key, "dependencies": dependency }),
                            );
                            break;
                        }
                    }
                }
                nested => every &= eval(nested, &Value::Object(object.clone()), path, context),
            }
        }
        ok &= !nonempty || every;
    }
    if let Some(dependencies) = schema.get("dependentRequired").and_then(Value::as_object) {
        let mut every = true;
        for (key, keys) in dependencies {
            if !object.contains_key(key) {
                continue;
            }
            for needed in keys.as_array().into_iter().flatten() {
                if !needed.as_str().is_some_and(|needed| object.contains_key(needed)) {
                    every &= context.add(
                        "dependentRequired",
                        path,
                        json!({ "property": key, "dependencies": keys }),
                    );
                }
            }
        }
        ok &= !nonempty || every;
    }
    if let Some(dependents) = schema.get("dependentSchemas").and_then(Value::as_object) {
        let mut every = true;
        for (key, nested) in dependents {
            if object.contains_key(key) {
                every &= eval(nested, &Value::Object(object.clone()), path, context);
            }
        }
        ok &= !nonempty || every;
    }
    if let Some(patterns) = schema.get("patternProperties").and_then(Value::as_object) {
        for (pattern, nested) in patterns {
            for (key, item) in object {
                if regex_test(pattern, key) {
                    ok &= eval(nested, item, &format!("{path}/{key}"), context);
                }
            }
        }
    }
    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        for (key, nested) in properties {
            if let Some(item) = object.get(key) {
                ok &= eval(nested, item, &format!("{path}/{key}"), context);
            }
        }
    }
    if let Some(names) = schema.get("propertyNames") {
        let mut rejected = Vec::new();
        for key in object.keys() {
            if !eval(names, &Value::String(key.clone()), &format!("{path}/{key}"), context) {
                rejected.push(key.clone());
            }
        }
        if !rejected.is_empty() {
            ok &= context.add("propertyNames", path, json!({ "propertyNames": rejected }));
        }
    }
    let count = object.len() as f64;
    if let Some(limit) = schema
        .get("minProperties")
        .filter(|l| l.as_f64().is_some_and(|l| count < l))
    {
        ok &= context.add("minProperties", path, json!({ "limit": limit }));
    }
    if let Some(limit) = schema
        .get("maxProperties")
        .filter(|l| l.as_f64().is_some_and(|l| count > l))
    {
        ok &= context.add("maxProperties", path, json!({ "limit": limit }));
    }
    ok
}

fn eval_array(schema: &Map<String, Value>, items: &[Value], path: &str, context: &mut Context) -> bool {
    let mut ok = true;
    let at = |index: usize| format!("{path}/{index}");
    if let (Some(additional), Some(Value::Array(sized))) = (schema.get("additionalItems"), schema.get("items")) {
        for (index, item) in items.iter().enumerate().skip(sized.len()) {
            if !eval(additional, item, &at(index), context) {
                ok = false;
                break;
            }
        }
    }
    let contains = schema.get("contains");
    let min_contains = schema.get("minContains").and_then(Value::as_f64);
    let matching = || {
        contains.map_or(0, |wanted| {
            items
                .iter()
                .filter(|item| eval(wanted, item, path, &mut Context::checking()))
                .count()
        })
    };
    if contains.is_some() && min_contains != Some(0.0) && matching() == 0 {
        ok &= context.add("contains", path, json!({ "minContains": 1 }));
    }
    match schema.get("items") {
        Some(Value::Array(sized)) => {
            for (index, nested) in sized.iter().enumerate() {
                if let Some(item) = items.get(index) {
                    ok &= eval(nested, item, &at(index), context);
                }
            }
        }
        Some(each) => {
            let offset = schema.get("prefixItems").and_then(Value::as_array).map_or(0, Vec::len);
            for (index, item) in items.iter().enumerate().skip(offset) {
                ok &= eval(each, item, &at(index), context);
            }
        }
        None => {}
    }
    if let Some(limit) = schema.get("maxContains").and_then(Value::as_f64)
        && contains.is_some()
        && matching() as f64 > limit
    {
        let min = schema.get("minContains").cloned().unwrap_or(json!(1));
        ok &= context.add(
            "contains",
            path,
            json!({ "minContains": min, "maxContains": schema["maxContains"] }),
        );
    }
    let length = items.len() as f64;
    if let Some(limit) = schema
        .get("maxItems")
        .filter(|l| l.as_f64().is_some_and(|l| length > l))
    {
        ok &= context.add("maxItems", path, json!({ "limit": limit }));
    }
    if let Some(limit) = min_contains
        && contains.is_some()
        && (matching() as f64) < limit
    {
        ok &= context.add("contains", path, json!({ "minContains": schema["minContains"] }));
    }
    if let Some(limit) = schema
        .get("minItems")
        .filter(|l| l.as_f64().is_some_and(|l| length < l))
    {
        ok &= context.add("minItems", path, json!({ "limit": limit }));
    }
    if let Some(prefix) = schema.get("prefixItems").and_then(Value::as_array)
        && !items.is_empty()
    {
        for (index, nested) in prefix.iter().enumerate() {
            if let Some(item) = items.get(index) {
                ok &= eval(nested, item, &at(index), context);
            }
        }
    }
    if schema.get("uniqueItems") == Some(&Value::Bool(true)) {
        let duplicates: Vec<usize> = (0..items.len())
            .filter(|&i| items[..i].iter().any(|seen| json_equal(seen, &items[i])))
            .collect();
        if !duplicates.is_empty() {
            ok &= context.add("uniqueItems", path, json!({ "duplicateItems": duplicates }));
        }
    }
    ok
}

fn eval_string(schema: &Map<String, Value>, text: &str, path: &str, context: &mut Context) -> bool {
    let mut ok = true;
    let length = || grapheme_count(text) as f64;
    if let Some(limit) = schema
        .get("maxLength")
        .filter(|l| l.as_f64().is_some_and(|l| length() > l))
    {
        ok &= context.add("maxLength", path, json!({ "limit": limit }));
    }
    if let Some(limit) = schema
        .get("minLength")
        .filter(|l| l.as_f64().is_some_and(|l| length() < l))
    {
        ok &= context.add("minLength", path, json!({ "limit": limit }));
    }
    if let Some(format) = schema.get("format").and_then(Value::as_str)
        && !format_matches(format, text)
    {
        ok &= context.add("format", path, json!({ "format": format }));
    }
    if let Some(pattern) = schema.get("pattern").and_then(Value::as_str)
        && !regex_test(pattern, text)
    {
        ok &= context.add("pattern", path, json!({ "pattern": pattern }));
    }
    ok
}

/// A numeric bound keyword, its comparison, and whether a value satisfies it.
type Bound = (&'static str, &'static str, fn(f64, f64) -> bool);

fn eval_number(schema: &Map<String, Value>, n: f64, path: &str, context: &mut Context) -> bool {
    let mut ok = true;
    let bounds: [Bound; 4] = [
        ("exclusiveMaximum", "<", |n, b| n < b),
        ("exclusiveMinimum", ">", |n, b| n > b),
        ("maximum", "<=", |n, b| n <= b),
        ("minimum", ">=", |n, b| n >= b),
    ];
    for (keyword, comparison, holds) in bounds {
        if let Some(limit) = schema.get(keyword)
            && !limit.as_f64().is_some_and(|b| holds(n, b))
        {
            ok &= context.add(keyword, path, json!({ "comparison": comparison, "limit": limit }));
        }
    }
    if let Some(divisor) = schema.get("multipleOf")
        && !divisor.as_f64().is_some_and(|d| multiple_of(n, d))
    {
        ok &= context.add("multipleOf", path, json!({ "multipleOf": divisor }));
    }
    ok
}
