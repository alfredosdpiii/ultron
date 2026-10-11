//! Provider-side constrained sampling (`api/constrained-sampling.ts`): strict JSON-schema conversion and OpenAI
//! grammar tools.

use crate::json::stringify;
use crate::types::{ConstrainedSamplingConfig, StrictMode, Tool};
use indexmap::IndexMap;
use serde_json::{Value, json};

const UNSUPPORTED_STRICT_SCHEMA_KEYS: &[&str] = &[
    "$ref",
    "$defs",
    "definitions",
    "allOf",
    "oneOf",
    "patternProperties",
    "dependentSchemas",
    "dependencies",
    "unevaluatedProperties",
    "propertyNames",
    "contains",
    "prefixItems",
    "not",
    "if",
    "then",
    "else",
];

fn is_structured(schema: &Value) -> bool {
    let Some(object) = schema.as_object() else { return false };
    let types: Vec<&str> = match object.get("type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).collect(),
        _ => vec![],
    };
    types.contains(&"object")
        || types.contains(&"array")
        || object.contains_key("properties")
        || object.contains_key("items")
}

fn allows_null(schema: &Value) -> bool {
    let Some(object) = schema.as_object() else { return false };
    let type_null = match object.get("type") {
        Some(Value::String(t)) => t == "null",
        Some(Value::Array(items)) => items.iter().any(|t| t == "null"),
        _ => false,
    };
    type_null
        || object.get("const").is_some_and(Value::is_null)
        || object
            .get("enum")
            .and_then(Value::as_array)
            .is_some_and(|e| e.iter().any(Value::is_null))
        || object
            .get("anyOf")
            .and_then(Value::as_array)
            .is_some_and(|v| v.iter().any(allows_null))
}

fn make_strict(schema: &mut Value) -> Result<(), String> {
    let Some(object) = schema.as_object_mut() else {
        return Err("boolean schemas are unsupported".into());
    };
    for key in UNSUPPORTED_STRICT_SCHEMA_KEYS {
        if object.get(*key).is_some() {
            return Err(format!("{key} schemas are unsupported"));
        }
    }
    if let Some(any_of) = object.get_mut("anyOf") {
        let Some(variants) = any_of.as_array_mut().filter(|v| !v.is_empty()) else {
            return Err("anyOf must contain at least one schema".into());
        };
        for variant in variants {
            if is_structured(variant) {
                return Err("object and array unions are unsupported".into());
            }
            make_strict(variant)?;
        }
    }
    if let Some(items) = object.get_mut("items") {
        if items.is_array() {
            return Err("tuple schemas are unsupported".into());
        }
        make_strict(items)?;
    }
    let is_object = object.get("type").and_then(Value::as_str) == Some("object");
    if object.contains_key("properties") && !is_object {
        return Err("properties require type object".into());
    }
    if !is_object {
        return Ok(());
    }
    if object
        .get("additionalProperties")
        .is_some_and(|v| *v != Value::Bool(false))
    {
        return Err("schema-valued or true additionalProperties is unsupported".into());
    }
    if object.get("properties").is_some_and(|p| !p.is_object()) {
        return Err("object properties must be a schema map".into());
    }
    let required: Option<Vec<String>> = match object.get("required") {
        None => None,
        Some(Value::Array(items)) if items.iter().all(Value::is_string) => {
            Some(items.iter().filter_map(Value::as_str).map(str::to_string).collect())
        }
        Some(_) => return Err("object required must be a string array".into()),
    };
    let names: Vec<String> = object
        .get("properties")
        .and_then(Value::as_object)
        .map(|p| p.keys().cloned().collect())
        .unwrap_or_default();
    let required = required.unwrap_or_default();
    if required.iter().any(|key| !names.contains(key)) {
        return Err("required contains an unknown property".into());
    }
    if let Some(Value::Object(properties)) = object.get_mut("properties") {
        for (key, property) in properties.iter_mut() {
            make_strict(property)?;
            if !required.contains(key) && !allows_null(property) {
                *property = json!({ "anyOf": [property.clone(), { "type": "null" }] });
            }
        }
    }
    object.insert(
        "required".into(),
        Value::Array(names.into_iter().map(Value::String).collect()),
    );
    object.insert("additionalProperties".into(), Value::Bool(false));
    Ok(())
}

/// Convert a tool schema to the strict subset used by provider constrained sampling.
pub fn make_strict_json_schema(schema: &Value) -> Result<Value, String> {
    let mut cloned = schema.clone();
    if !cloned.is_object() {
        return Err("root schema must have type object".into());
    }
    make_strict(&mut cloned)?;
    if cloned.get("type").and_then(Value::as_str) != Some("object") {
        return Err("root schema must have type object".into());
    }
    Ok(cloned)
}

pub fn json_schema_tool_parameters(tool: &Tool, strict: Option<bool>) -> Result<Value, String> {
    if strict == Some(true) {
        make_strict_json_schema(&tool.parameters)
    } else {
        Ok(tool.parameters.clone())
    }
}

/// `resolveJsonSchemaStrictSampling`: `Some(true)` to send strict, `None` to send the plain schema; `Err` when the
/// tool requires strict sampling the provider or schema cannot honor.
pub fn resolve_json_schema_strict_sampling(tool: &Tool, supports_strict_mode: bool) -> Result<Option<bool>, String> {
    let Some(ConstrainedSamplingConfig::JsonSchema { strict }) = tool.sampling_config() else {
        return Ok(None);
    };
    if supports_strict_mode {
        return match make_strict_json_schema(&tool.parameters) {
            Ok(_) => Ok(Some(true)),
            Err(_) if *strict != StrictMode::Require => Ok(None),
            Err(message) => Err(format!(
                "Tool \"{}\" requires JSON-schema constrained sampling, but {message}.",
                tool.name
            )),
        };
    }
    if *strict == StrictMode::Require {
        return Err(format!(
            "Tool \"{}\" requires JSON-schema constrained sampling, but strict tools are unsupported.",
            tool.name
        ));
    }
    Ok(None)
}

#[derive(Clone, Debug, PartialEq)]
pub struct GrammarConstrainedSampling {
    pub format: &'static str,
    pub definition: String,
    pub input_property: String,
}

fn infer_grammar_input_property(tool: &Tool) -> Result<String, String> {
    let schema = &tool.parameters;
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        return Err("grammar constrained sampling requires an object parameter schema".into());
    }
    let required = schema.get("required").and_then(Value::as_array);
    let Some(property) = required.filter(|r| r.len() == 1).and_then(|r| r[0].as_str()) else {
        return Err("grammar constrained sampling requires exactly one required string property".into());
    };
    let Some(definition) = schema
        .pointer(&format!(
            "/properties/{}",
            property.replace('~', "~0").replace('/', "~1")
        ))
        .filter(|d| !d.is_null())
    else {
        return Err(format!(
            "grammar constrained sampling requires a properties entry for {property}"
        ));
    };
    if definition.get("type").and_then(Value::as_str) != Some("string") {
        return Err(format!(
            "grammar constrained sampling property {property} must have type string"
        ));
    }
    Ok(property.to_string())
}

pub fn resolve_grammar_constrained_sampling(
    tool: &Tool,
    supports_openai_grammar_tools: bool,
) -> Result<Option<GrammarConstrainedSampling>, String> {
    let Some(ConstrainedSamplingConfig::Grammar { variants }) = tool.sampling_config() else {
        return Ok(None);
    };
    if !supports_openai_grammar_tools {
        return Ok(None);
    }
    let lark = variants.openai_lark.as_deref().filter(|d| !d.trim().is_empty());
    let regex = variants.openai_regex.as_deref().filter(|d| !d.trim().is_empty());
    let (format, definition) = match (lark, regex) {
        (Some(lark), _) => ("lark", lark),
        (None, Some(regex)) => ("regex", regex),
        (None, None) => {
            return Err(format!(
                "Tool \"{}\" cannot use grammar constrained sampling: no supported grammar variant was provided.",
                tool.name
            ));
        }
    };
    let input_property = infer_grammar_input_property(tool).map_err(|message| {
        format!(
            "Tool \"{}\" cannot use grammar constrained sampling: {message}.",
            tool.name
        )
    })?;
    Ok(Some(GrammarConstrainedSampling {
        format,
        definition: definition.to_string(),
        input_property,
    }))
}

pub fn grammar_tool_input_properties(
    tools: &[Tool],
    supports_openai_grammar_tools: bool,
) -> Result<IndexMap<String, String>, String> {
    let mut properties = IndexMap::new();
    for tool in tools {
        if let Some(grammar) = resolve_grammar_constrained_sampling(tool, supports_openai_grammar_tools)? {
            properties.insert(tool.name.clone(), grammar.input_property);
        }
    }
    Ok(properties)
}

pub fn grammar_tool_input(tool_name: &str, arguments: &Value, property: &str) -> Result<String, String> {
    match arguments.get(property) {
        Some(Value::String(input)) => Ok(input.clone()),
        _ => Err(format!(
            "Grammar tool call \"{tool_name}\" requires argument \"{property}\" to be a string."
        )),
    }
}

/// Re-encodes streamed grammar input as a JSON-object argument delta (`appendGrammarToolInputJsonDelta`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GrammarInputBuffer {
    pub input: String,
    pub started: bool,
    pub closed: bool,
}

impl GrammarInputBuffer {
    pub fn append(&mut self, property: &str, next: &str, close: bool) -> Result<Option<String>, String> {
        if self.closed {
            if close && next == self.input {
                return Ok(None);
            }
            return Err(format!(
                "grammar tool input for property \"{property}\" changed after it was closed"
            ));
        }
        let Some(input_delta) = next.strip_prefix(self.input.as_str()) else {
            return Err(format!(
                "grammar tool input for property \"{property}\" changed non-monotonically"
            ));
        };
        if !close && input_delta.is_empty() {
            return Ok(None);
        }
        let mut delta = String::new();
        if !self.started {
            delta.push_str(&format!("{{{}:\"", stringify(&Value::String(property.to_string()))));
            self.started = true;
        }
        let encoded = stringify(&Value::String(input_delta.to_string()));
        delta.push_str(&encoded[1..encoded.len() - 1]);
        self.input = next.to_string();
        if close {
            delta.push_str("\"}");
            self.closed = true;
        }
        Ok(Some(delta))
    }
}
