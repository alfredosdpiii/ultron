//! The user's `models.json` (`coding-agent/src/core/model-config.ts`): one immutable, credential-blind load with the
//! upstream error strings ("Failed to load", "Failed to parse", "Invalid models.json schema" with TypeBox's error list),
//! plus `models-store.ts` (persistent dynamic catalogs) and `model-catalog.ts` (`flattenModelCatalog`).
//!
//! Evidence: `fixtures/generate-model-config.mjs` runs the TypeScript loader over files in a temp dir and records
//! errors and providers; `tests/model_config.rs` replays them.

use crate::types::Model;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// One immutable load of models.json: raw provider objects (keys in JavaScript property order) and the load error.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModelConfig {
    providers: Map<String, Value>,
    error: Option<String>,
}

impl ModelConfig {
    /// `ModelConfig.load`, expanding `~` against `$HOME`.
    pub fn load(models_json_path: Option<&str>) -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        Self::load_with_home(models_json_path, &home)
    }

    /// `ModelConfig.load` with an explicit home directory for `~` expansion.
    pub fn load_with_home(models_json_path: Option<&str>, home: &Path) -> Self {
        let Some(input) = models_json_path.filter(|p| !p.is_empty()) else {
            return Self::default();
        };
        let path = normalize_path(input, home);
        let shown = path.display().to_string();
        let failed = |error: String| Self {
            providers: Map::new(),
            error: Some(error),
        };
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == ErrorKind::NotFound => return Self::default(),
            Err(e) => {
                return failed(format!(
                    "Failed to load models.json: {}\n\nFile: {shown}",
                    node_fs_error(&e, &shown)
                ));
            }
        };
        let text = String::from_utf8_lossy(&bytes);
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
        let parsed: Value = match serde_json::from_str(&strip_json_comments(text)) {
            Ok(value) => js_order(crate::json::normalize_numbers(value)),
            Err(e) => {
                // ponytail: serde_json's wording, not V8's JSON.parse message.
                return failed(format!("Failed to parse models.json: {e}\n\nFile: {shown}"));
            }
        };
        let mut errors = Vec::new();
        if !validate(&models_config_schema(), &parsed, "", &mut errors) {
            let list = errors
                .iter()
                .map(|e| format!("  - {}: {}", e.path, e.message))
                .collect::<Vec<_>>()
                .join("\n");
            let list = if list.is_empty() {
                "Unknown schema error".to_owned()
            } else {
                list
            };
            return failed(format!("Invalid models.json schema:\n{list}\n\nFile: {shown}"));
        }
        let providers = parsed
            .get("providers")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        Self { providers, error: None }
    }

    pub fn provider(&self, provider_id: &str) -> Option<&Value> {
        self.providers.get(provider_id)
    }

    pub fn provider_ids(&self) -> Vec<&str> {
        self.providers.keys().map(String::as_str).collect()
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
}

/// The warning `ultron --list-models` prints when models.json failed to load (`cli/list-models.ts`, without color).
pub fn models_json_warning(error: &str) -> String {
    format!("Warning: errors loading models.json:\n{error}")
}

/// `normalizePath` with its defaults: `~` and `~/` expand to `home`, `file://` URLs become paths.
fn normalize_path(input: &str, home: &Path) -> PathBuf {
    if input == "~" {
        return home.to_path_buf();
    }
    if let Some(rest) = input.strip_prefix("~/") {
        return home.join(rest);
    }
    if input.starts_with("file://")
        && let Some(path) = reqwest::Url::parse(input).ok().and_then(|u| u.to_file_path().ok())
    {
        return path;
    }
    PathBuf::from(input)
}

/// Node's `fs.readFile` error message for the common errno values.
fn node_fs_error(error: &std::io::Error, path: &str) -> String {
    let (code, description, syscall) = match error.raw_os_error() {
        Some(21) => ("EISDIR", "illegal operation on a directory", "read"),
        Some(13) => ("EACCES", "permission denied", "open"),
        Some(20) => ("ENOTDIR", "not a directory", "open"),
        Some(40) => ("ELOOP", "too many symbolic links encountered", "open"),
        Some(36) => ("ENAMETOOLONG", "name too long", "open"),
        // ponytail: other errno values keep the OS wording.
        _ => return error.to_string(),
    };
    format!("{code}: {description}, {syscall} '{path}'")
}

/// `stripJsonComments` (`coding-agent/src/utils/json.ts`): drops `//` line comments, then commas before `}`/`]`,
/// leaving string literals untouched. Two passes, like the two upstream regexes.
pub fn strip_json_comments(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut first = String::with_capacity(input.len());
    let mut i = 0;
    while i < chars.len() {
        if let Some(end) = string_literal_end(&chars, i) {
            first.extend(&chars[i..end]);
            i = end;
        } else if chars[i] == '/' && chars.get(i + 1) == Some(&'/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else {
            first.push(chars[i]);
            i += 1;
        }
    }
    let chars: Vec<char> = first.chars().collect();
    let mut out = String::with_capacity(first.len());
    let mut i = 0;
    while i < chars.len() {
        if let Some(end) = string_literal_end(&chars, i) {
            out.extend(&chars[i..end]);
            i = end;
            continue;
        }
        if chars[i] == ',' {
            let mut j = i + 1;
            while j < chars.len() && is_js_whitespace(chars[j]) {
                j += 1;
            }
            if matches!(chars.get(j), Some('}' | ']')) {
                out.extend(&chars[i + 1..=j]);
                i = j + 1;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// End (exclusive) of `/"(?:\\.|[^"\\])*"/` matched at `start`; `.` excludes JavaScript line terminators.
fn string_literal_end(chars: &[char], start: usize) -> Option<usize> {
    if chars.get(start) != Some(&'"') {
        return None;
    }
    let mut i = start + 1;
    loop {
        match *chars.get(i)? {
            '"' => return Some(i + 1),
            '\\' => match chars.get(i + 1) {
                Some(c) if !is_line_terminator(*c) => i += 2,
                _ => return None,
            },
            _ => i += 1,
        }
    }
}

fn is_line_terminator(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

/// JavaScript's `\s`.
fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'
    )
}

/// A canonical array index (`0`..=`2^32 - 2`): such keys come first, ascending, in JavaScript objects.
fn array_index(key: &str) -> Option<u32> {
    if key.is_empty() || (key.len() > 1 && key.starts_with('0')) || !key.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    key.parse::<u64>()
        .ok()
        .filter(|n| *n < u64::from(u32::MAX))
        .map(|n| n as u32)
}

/// Reorders every object's keys the way a JavaScript object enumerates them.
pub fn js_order(value: Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.into_iter().map(js_order).collect()),
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> = map.into_iter().map(|(k, v)| (k, js_order(v))).collect();
            // Stable: non-index keys keep insertion order after the ascending index keys.
            entries.sort_by_key(|(k, _)| array_index(k).map_or((1, 0), |n| (0, n)));
            Value::Object(entries.into_iter().collect())
        }
        other => other,
    }
}

// ---------------------------------------------------------------------------------------------------------------
// The TypeBox schema of models.json and the subset of TypeBox's error walk it needs (`schema/engine/schema.mjs`).
// ---------------------------------------------------------------------------------------------------------------

enum Schema {
    Unknown,
    Str {
        min_length: Option<usize>,
    },
    /// `Type.Literal`: `{type: "string", const}`.
    Literal(&'static str),
    Num {
        exclusive_minimum: Option<f64>,
    },
    Int {
        minimum: Option<f64>,
        maximum: Option<f64>,
    },
    Bool,
    Null,
    Obj(Vec<(&'static str, bool, Schema)>),
    /// `Type.Record(Type.String(), ...)`: `patternProperties: {"^.*$": ...}`.
    Record(Box<Schema>),
    Array {
        items: Box<Schema>,
        max_items: Option<usize>,
    },
    AnyOf(Vec<Schema>),
}

struct SchemaError {
    path: String,
    message: String,
}

/// TypeBox's `maxErrors` default; logical keywords give each operand its own context of this size.
const MAX_ERRORS: usize = 8;

fn push(errors: &mut Vec<SchemaError>, instance_path: &str, message: String) {
    if errors.len() < MAX_ERRORS {
        errors.push(SchemaError {
            path: display_path(instance_path),
            message,
        });
    }
}

/// `formatValidationPath` for non-`required` errors.
fn display_path(instance_path: &str) -> String {
    let path = instance_path
        .strip_prefix('/')
        .unwrap_or(instance_path)
        .replace('/', ".");
    if path.is_empty() { "root".into() } else { path }
}

fn js_number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e21 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

/// `ErrorSchema`: records errors in TypeBox's keyword order and returns whether `value` is valid.
fn validate(schema: &Schema, value: &Value, path: &str, errors: &mut Vec<SchemaError>) -> bool {
    if errors.len() >= MAX_ERRORS {
        return false;
    }
    let type_error = |name: &str, errors: &mut Vec<SchemaError>| {
        push(errors, path, format!("must be {name}"));
        false
    };
    match schema {
        Schema::Unknown => true,
        Schema::Bool => value.is_boolean() || type_error("boolean", errors),
        Schema::Null => value.is_null() || type_error("null", errors),
        Schema::Str { min_length } => {
            let Some(s) = value.as_str() else {
                return type_error("string", errors);
            };
            match min_length {
                Some(min) if s.encode_utf16().count() < *min => {
                    push(errors, path, format!("must not have fewer than {min} characters"));
                    false
                }
                _ => true,
            }
        }
        Schema::Literal(constant) => {
            let typed = value.is_string() || type_error("string", errors);
            let equal = value.as_str() == Some(constant);
            if !equal {
                push(errors, path, "must be equal to constant".into());
            }
            typed && equal
        }
        Schema::Num { exclusive_minimum } => {
            let Some(n) = value.as_f64() else {
                return type_error("number", errors);
            };
            match exclusive_minimum {
                Some(limit) if n <= *limit => {
                    push(errors, path, format!("must be > {}", js_number(*limit)));
                    false
                }
                _ => true,
            }
        }
        Schema::Int { minimum, maximum } => {
            let Some(n) = value.as_f64() else {
                return type_error("integer", errors);
            };
            let mut ok = n.fract() == 0.0 || type_error("integer", errors);
            if let Some(limit) = maximum
                && n > *limit
            {
                push(errors, path, format!("must be <= {}", js_number(*limit)));
                ok = false;
            }
            if let Some(limit) = minimum
                && n < *limit
            {
                push(errors, path, format!("must be >= {}", js_number(*limit)));
                ok = false;
            }
            ok
        }
        Schema::Obj(properties) => {
            let Some(map) = value.as_object() else {
                return type_error("object", errors);
            };
            let missing: Vec<&str> = properties
                .iter()
                .filter(|(key, required, _)| *required && !map.contains_key(*key))
                .map(|(key, ..)| *key)
                .collect();
            let mut ok = true;
            if let Some(first) = missing.first() {
                let base = display_path(path);
                let shown = if path.is_empty() {
                    (*first).to_owned()
                } else {
                    format!("{base}.{first}")
                };
                if errors.len() < MAX_ERRORS {
                    errors.push(SchemaError {
                        path: shown,
                        message: format!("must have required properties {}", missing.join(", ")),
                    });
                }
                ok = false;
            }
            for (key, _, schema) in properties {
                if let Some(child) = map.get(*key) {
                    ok &= validate(schema, child, &format!("{path}/{key}"), errors);
                }
            }
            ok
        }
        Schema::Record(schema) => {
            let Some(map) = value.as_object() else {
                return type_error("object", errors);
            };
            let mut ok = true;
            for (key, child) in map {
                // `^.*$` without flags: `.` stops at line terminators, so such keys are not validated.
                if key.chars().any(is_line_terminator) {
                    continue;
                }
                ok &= validate(schema, child, &format!("{path}/{key}"), errors);
            }
            ok
        }
        Schema::Array { items, max_items } => {
            let Some(list) = value.as_array() else {
                return type_error("array", errors);
            };
            let mut ok = true;
            for (i, item) in list.iter().enumerate() {
                ok &= validate(items, item, &format!("{path}/{i}"), errors);
            }
            if let Some(max) = max_items
                && list.len() > *max
            {
                push(errors, path, format!("must not have more than {max} items"));
                ok = false;
            }
            ok
        }
        Schema::AnyOf(options) => {
            let mut failed = Vec::new();
            for option in options {
                let mut branch = Vec::new();
                if validate(option, value, path, &mut branch) {
                    return true;
                }
                failed.push(branch);
            }
            for error in failed.into_iter().flatten() {
                if errors.len() < MAX_ERRORS {
                    errors.push(error);
                }
            }
            push(errors, path, "must match a schema in anyOf".into());
            false
        }
    }
}

fn opt(key: &'static str, schema: Schema) -> (&'static str, bool, Schema) {
    (key, false, schema)
}

fn req(key: &'static str, schema: Schema) -> (&'static str, bool, Schema) {
    (key, true, schema)
}

fn string() -> Schema {
    Schema::Str { min_length: None }
}

fn non_empty() -> Schema {
    Schema::Str { min_length: Some(1) }
}

fn number() -> Schema {
    Schema::Num {
        exclusive_minimum: None,
    }
}

fn int(minimum: Option<f64>, maximum: Option<f64>) -> Schema {
    Schema::Int { minimum, maximum }
}

fn array(items: Schema) -> Schema {
    Schema::Array {
        items: Box::new(items),
        max_items: None,
    }
}

fn literals(values: &[&'static str]) -> Schema {
    Schema::AnyOf(values.iter().map(|v| Schema::Literal(v)).collect())
}

fn record(schema: Schema) -> Schema {
    Schema::Record(Box::new(schema))
}

fn bools(keys: &[&'static str]) -> Vec<(&'static str, bool, Schema)> {
    keys.iter().map(|k| opt(k, Schema::Bool)).collect()
}

fn percentile_cutoffs() -> Schema {
    Schema::Obj(vec![
        opt("p50", number()),
        opt("p75", number()),
        opt("p90", number()),
        opt("p99", number()),
    ])
}

fn open_router_routing() -> Schema {
    let num_or_string = || Schema::AnyOf(vec![number(), string()]);
    let num_or_cutoffs = || Schema::AnyOf(vec![number(), percentile_cutoffs()]);
    Schema::Obj(vec![
        opt("allow_fallbacks", Schema::Bool),
        opt("require_parameters", Schema::Bool),
        opt("data_collection", literals(&["deny", "allow"])),
        opt("zdr", Schema::Bool),
        opt("enforce_distillable_text", Schema::Bool),
        opt("order", array(string())),
        opt("only", array(string())),
        opt("ignore", array(string())),
        opt("quantizations", array(string())),
        opt(
            "sort",
            Schema::AnyOf(vec![
                string(),
                Schema::Obj(vec![
                    opt("by", string()),
                    opt("partition", Schema::AnyOf(vec![string(), Schema::Null])),
                ]),
            ]),
        ),
        opt(
            "max_price",
            Schema::Obj(vec![
                opt("prompt", num_or_string()),
                opt("completion", num_or_string()),
                opt("image", num_or_string()),
                opt("audio", num_or_string()),
                opt("request", num_or_string()),
            ]),
        ),
        opt("preferred_min_throughput", num_or_cutoffs()),
        opt("preferred_max_latency", num_or_cutoffs()),
    ])
}

fn chat_template_kwargs() -> Schema {
    record(Schema::AnyOf(vec![
        Schema::AnyOf(vec![string(), number(), Schema::Bool, Schema::Null]),
        Schema::Obj(vec![
            req("$var", literals(&["thinking.enabled", "thinking.effort"])),
            opt("omitWhenOff", Schema::Bool),
        ]),
    ]))
}

fn session_affinity_format() -> Schema {
    literals(&["openai", "openai-nosession", "openrouter"])
}

fn cost_rates(mut head: Vec<(&'static str, bool, Schema)>) -> Vec<(&'static str, bool, Schema)> {
    head.extend(["input", "output", "cacheRead", "cacheWrite"].map(|k| req(k, number())));
    head
}

fn cost_tier() -> Schema {
    Schema::Obj(cost_rates(vec![req("inputTokensAbove", number())]))
}

fn model_cost() -> Schema {
    let mut fields = cost_rates(Vec::new());
    fields.push(opt("tiers", array(cost_tier())));
    Schema::Obj(fields)
}

fn provider_compat() -> Schema {
    let mut completions = bools(&[
        "supportsStore",
        "supportsDeveloperRole",
        "supportsReasoningEffort",
        "supportsUsageInStreaming",
        "supportsFinishReason",
    ]);
    completions.push(opt(
        "maxTokensField",
        literals(&["max_completion_tokens", "max_tokens"]),
    ));
    completions.extend(bools(&[
        "requiresToolResultName",
        "requiresAssistantAfterToolResult",
        "requiresThinkingAsText",
        "requiresReasoningContentOnAssistantMessages",
    ]));
    completions.push(opt(
        "thinkingFormat",
        literals(&[
            "openai",
            "openrouter",
            "together",
            "baseten",
            "deepseek",
            "zai",
            "qwen",
            "chat-template",
            "qwen-chat-template",
            "string-thinking",
            "ant-ling",
        ]),
    ));
    completions.push(opt("chatTemplateKwargs", chat_template_kwargs()));
    completions.push(opt("chatTemplateArgs", chat_template_kwargs()));
    completions.push(opt("cacheControlFormat", Schema::Literal("anthropic")));
    completions.push(opt("openRouterRouting", open_router_routing()));
    completions.push(opt(
        "vercelGatewayRouting",
        Schema::Obj(vec![opt("only", array(string())), opt("order", array(string()))]),
    ));
    completions.extend(bools(&[
        "supportsOpenAIGrammarTools",
        "supportsStrictMode",
        "sendSessionAffinityHeaders",
    ]));
    completions.push(opt("sessionAffinityFormat", session_affinity_format()));
    completions.push(opt("supportsLongCacheRetention", Schema::Bool));
    completions.push(opt("vllmPriority", number()));

    let responses = vec![
        opt("supportsDeveloperRole", Schema::Bool),
        opt("sessionAffinityFormat", session_affinity_format()),
        opt("supportsLongCacheRetention", Schema::Bool),
        opt("supportsStrictMode", Schema::Bool),
        opt("supportsOpenAIGrammarTools", Schema::Bool),
        opt("supportsMaxOutputTokens", Schema::Bool),
    ];

    let mut anthropic = bools(&[
        "supportsEagerToolInputStreaming",
        "supportsLongCacheRetention",
        "sendSessionAffinityHeaders",
        "supportsCacheControlOnTools",
        "supportsTemperature",
        "forceAdaptiveThinking",
        "allowEmptySignature",
        "supportsStrictTools",
        "supportsMidConvoEffort",
    ]);
    anthropic.push(opt(
        "allowedFallbackModels",
        Schema::Array {
            items: Box::new(Schema::Obj(vec![
                req("provider", non_empty()),
                req("model", non_empty()),
                req("cost", model_cost()),
            ])),
            max_items: Some(3),
        },
    ));
    Schema::AnyOf(vec![
        Schema::Obj(completions),
        Schema::Obj(responses),
        Schema::Obj(anthropic),
    ])
}

fn thinking_level_map() -> Schema {
    Schema::Obj(
        ["off", "minimal", "low", "medium", "high", "xhigh", "max"]
            .map(|k| opt(k, Schema::AnyOf(vec![string(), Schema::Null])))
            .into(),
    )
}

fn input_kinds() -> Schema {
    array(literals(&["text", "image"]))
}

fn input_limits() -> Schema {
    let positive = || int(Some(1.0), None);
    Schema::Obj(vec![
        opt("maxRequestBytes", positive()),
        opt(
            "images",
            Schema::Obj(vec![
                opt(
                    "resize",
                    Schema::Obj(vec![
                        opt("maxWidth", positive()),
                        opt("maxHeight", positive()),
                        opt("maxBytes", positive()),
                        opt("jpegQuality", int(Some(1.0), Some(100.0))),
                    ]),
                ),
                opt("maxPerMessage", positive()),
                opt("maxPerRequest", positive()),
            ]),
        ),
    ])
}

fn prompt_cache() -> Schema {
    let positive = || Schema::Num {
        exclusive_minimum: Some(0.0),
    };
    Schema::Obj(vec![opt("short", positive()), opt("long", positive())])
}

fn model_definition() -> Schema {
    Schema::Obj(vec![
        req("id", non_empty()),
        opt("name", non_empty()),
        opt("api", non_empty()),
        opt("baseUrl", non_empty()),
        opt("reasoning", Schema::Bool),
        opt("thinkingLevelMap", thinking_level_map()),
        opt("input", input_kinds()),
        opt("inputLimits", input_limits()),
        opt("cost", model_cost()),
        opt("promptCache", prompt_cache()),
        opt("contextWindow", number()),
        opt("maxTokens", number()),
        opt("samplingParams", record(Schema::Unknown)),
        opt("headers", record(string())),
        opt("compat", provider_compat()),
    ])
}

fn model_override() -> Schema {
    let mut cost: Vec<_> = ["input", "output", "cacheRead", "cacheWrite"]
        .map(|k| opt(k, number()))
        .into();
    cost.push(opt("tiers", array(cost_tier())));
    Schema::Obj(vec![
        opt("name", non_empty()),
        opt("reasoning", Schema::Bool),
        opt("thinkingLevelMap", thinking_level_map()),
        opt("input", input_kinds()),
        opt("inputLimits", input_limits()),
        opt("cost", Schema::Obj(cost)),
        opt("promptCache", prompt_cache()),
        opt("contextWindow", number()),
        opt("maxTokens", number()),
        opt("samplingParams", record(Schema::Unknown)),
        opt("headers", record(string())),
        opt("compat", provider_compat()),
    ])
}

fn models_config_schema() -> Schema {
    let provider = Schema::Obj(vec![
        opt("name", non_empty()),
        opt("baseUrl", non_empty()),
        opt("apiKey", non_empty()),
        opt("api", non_empty()),
        opt("oauth", Schema::Literal("radius")),
        opt("headers", record(string())),
        opt("compat", provider_compat()),
        opt("authHeader", Schema::Bool),
        opt("models", array(model_definition())),
        opt("modelOverrides", record(model_override())),
    ]);
    Schema::Obj(vec![req("providers", record(provider))])
}

// ---------------------------------------------------------------------------------------------------------------
// models-store.ts and model-catalog.ts
// ---------------------------------------------------------------------------------------------------------------

/// A persisted dynamic catalog (`ModelsStoreEntry`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelsStoreEntry {
    pub models: Vec<Model>,
    /// Unix timestamp from the remote catalog's Last-Modified header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<f64>,
    /// Unix timestamp of the last completed remote check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<f64>,
    /// The remote catalog's ETag, quotes included, echoed back as If-None-Match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
}

/// Persistent model catalogs keyed by provider id.
pub trait ModelsStore: Send + Sync {
    fn read(&self, provider_id: &str) -> Option<ModelsStoreEntry>;
    fn write(&self, provider_id: &str, entry: &ModelsStoreEntry);
    fn delete(&self, provider_id: &str);
}

/// `InMemoryModelsStore`: entries are copied in and out, so callers never share state with the store.
#[derive(Default)]
pub struct InMemoryModelsStore {
    entries: Mutex<HashMap<String, ModelsStoreEntry>>,
}

impl ModelsStore for InMemoryModelsStore {
    fn read(&self, provider_id: &str) -> Option<ModelsStoreEntry> {
        self.entries.lock().ok()?.get(provider_id).cloned()
    }

    fn write(&self, provider_id: &str, entry: &ModelsStoreEntry) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.insert(provider_id.to_owned(), entry.clone());
        }
    }

    fn delete(&self, provider_id: &str) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.remove(provider_id);
        }
    }
}
