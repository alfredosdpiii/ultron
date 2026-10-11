//! Bug-report payloads ported from `packages/coding-agent/src/core/bug-report.ts`: credential redaction, the
//! `report.json` metadata, the content-free `diagnostics.json`, the bundle's files and its zip archive.
//! Everything here is pure; the caller supplies the clock, the report id, host details and the environment.
//! The model-written summary (`generateBugReportSummary`) needs a model call and is not part of this crate.

use crate::crash_log::{CrashRecord, Extension};
use crate::js;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::io;
use std::path::Path;
use url::Url;

pub const BUG_REPORT_CUSTOM_ENTRY_TYPE: &str = "pi.bug-report";
const SCHEMA_VERSION: u64 = 1;
const REDACTED: &str = "<redacted>";
const SENSITIVE_WORDS: &[&str] = &[
    "apikey",
    "api-key",
    "api_key",
    "secret",
    "token",
    "password",
    "passwd",
    "credential",
    "authorization",
    "cookie",
];

/// `/(?:^|[-_])(api[-_]?key|secret|token|password|passwd|credential|authorization|cookie)(?:$|[-_])/i` after
/// splitting camelCase (`fooBar` -> `foo_Bar`).
pub fn is_sensitive_key(key: &str) -> bool {
    let chars: Vec<char> = key.chars().collect();
    let mut split = String::with_capacity(key.len() + 4);
    let mut i = 0;
    while i < chars.len() {
        split.push(chars[i]);
        if (chars[i].is_ascii_lowercase() || chars[i].is_ascii_digit())
            && chars.get(i + 1).is_some_and(char::is_ascii_uppercase)
        {
            split.push('_');
            split.push(chars[i + 1]);
            i += 1;
        }
        i += 1;
    }
    let lower = split.to_ascii_lowercase();
    let boundary = |c: Option<u8>| matches!(c, None | Some(b'-' | b'_'));
    let bytes = lower.as_bytes();
    (0..bytes.len()).any(|at| {
        (at == 0 || boundary(Some(bytes[at - 1])))
            && SENSITIVE_WORDS
                .iter()
                .any(|word| lower[at..].starts_with(word) && boundary(bytes.get(at + word.len()).copied()))
    })
}

fn is_scheme(value: &str) -> bool {
    let mut chars = value.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
}

/// `^([a-z][a-z0-9+.-]*:)([a-z][a-z0-9+.-]*:\/\/.*)$`/i: a scheme prefix in front of a URL, as in `git:https://...`.
fn split_nested(value: &str) -> Option<(&str, &str)> {
    let colon = value.find(':')?;
    let (prefix, rest) = (&value[..=colon], &value[colon + 1..]);
    let inner = rest.find(':')?;
    let nested = is_scheme(&value[..colon])
        && is_scheme(&rest[..inner])
        && rest[inner..].starts_with("://")
        && !rest.contains(['\n', '\r', '\u{2028}', '\u{2029}']);
    nested.then_some((prefix, rest))
}

/// `redactUrl`: strips credentials and secret-looking query parameters from a URL; anything else is returned as is.
pub fn redact_url(value: &str) -> String {
    if let Some((prefix, rest)) = split_nested(value) {
        return format!("{prefix}{}", redact_url(rest));
    }
    let Ok(mut url) = Url::parse(value) else {
        return value.to_owned();
    };
    let mut changed = false;
    if !url.username().is_empty() || url.password().is_some_and(|p| !p.is_empty()) {
        let _ = url.set_password(None);
        let _ = url.set_username("");
        changed = true;
    }
    if let Some(query) = url.query() {
        let mut pairs: Vec<(String, String)> = form_urlencoded::parse(query.as_bytes()).into_owned().collect();
        let mut modified = false;
        let mut i = 0;
        // URLSearchParams.set while iterating keys(): the first pair with the name takes the value, later ones go.
        while i < pairs.len() {
            let key = pairs[i].0.clone();
            if is_sensitive_key(&key) {
                let first = pairs.iter().position(|(k, _)| *k == key).unwrap_or(i);
                pairs[first].1 = REDACTED.to_owned();
                let mut seen = 0;
                pairs.retain(|(k, _)| {
                    seen += 1;
                    *k != key || seen - 1 == first
                });
                modified = true;
            }
            i += 1;
        }
        if modified {
            let serialized = form_urlencoded::Serializer::new(String::new())
                .extend_pairs(&pairs)
                .finish();
            url.set_query(Some(&serialized));
            changed = true;
        }
    }
    if changed { url.to_string() } else { value.to_owned() }
}

fn redact_value(key: &str, value: &Value) -> Value {
    if !value.is_null() && is_sensitive_key(key) {
        return Value::String(REDACTED.to_owned());
    }
    match value {
        Value::String(s) => Value::String(redact_url(s)),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .enumerate()
                .map(|(index, item)| redact_value(&index.to_string(), item))
                .collect(),
        ),
        Value::Object(map) => Value::Object(map.iter().map(|(k, v)| (k.clone(), redact_value(k, v))).collect()),
        other => other.clone(),
    }
}

/// `redactJsonValue`: replaces non-null values under sensitive keys and redacts every string as a URL.
pub fn redact_json_value(value: &Value) -> Value {
    redact_value("", value)
}

fn redact_settings(settings: &Value) -> Value {
    let mut settings = settings.clone();
    if let Value::Object(map) = &mut settings {
        map.shift_remove("trackingId");
    }
    redact_json_value(&settings)
}

/// Host details the TypeScript reads from `process` and `os`.
#[derive(Debug, Clone)]
pub struct Host {
    pub version: String,
    /// `node/v24.0.0` in TypeScript.
    pub runtime: String,
    pub platform: String,
    pub arch: String,
    pub os_release: String,
    pub os_version: String,
}

/// `getPiUserAgent`.
pub fn user_agent(host: &Host) -> String {
    format!(
        "pi/{} ({}; {}; {})",
        host.version, host.platform, host.runtime, host.arch
    )
}

/// `collectEnvironment`: host details, terminal hints and the names (never the values) of `PI_*` variables.
pub fn collect_environment(host: &Host, env: &BTreeMap<String, String>) -> Value {
    let get = |name: &str| env.get(name).filter(|v| !v.is_empty());
    let nullable = |name: &str| get(name).map_or(Value::Null, |v| Value::String(v.clone()));
    let shell = get("SHELL")
        .and_then(|s| s.split(['/', '\\']).next_back())
        .filter(|s| !s.is_empty());
    let mut pi_names: Vec<&String> = env.keys().filter(|name| name.starts_with("PI_")).collect();
    pi_names.sort_by(|a, b| js::utf16_cmp(a, b));
    json!({
        "version": host.version,
        "userAgent": user_agent(host),
        "runtime": host.runtime,
        "platform": host.platform,
        "arch": host.arch,
        "osRelease": host.os_release,
        "osVersion": host.os_version,
        "shell": shell,
        "terminal": {
            "term": nullable("TERM"),
            "program": nullable("TERM_PROGRAM"),
            "programVersion": nullable("TERM_PROGRAM_VERSION"),
            "colorterm": nullable("COLORTERM"),
            "tmux": get("TMUX").is_some(),
            "ssh": get("SSH_CONNECTION").or(get("SSH_CLIENT")).or(get("SSH_TTY")).is_some(),
            "ci": get("CI").is_some(),
        },
        "piEnvironmentVariables": pi_names,
    })
}

fn sorted_keys(value: Option<&Value>) -> Value {
    let mut names: Vec<&String> = value
        .and_then(Value::as_object)
        .map(|m| m.keys().collect())
        .unwrap_or_default();
    names.sort_by(|a, b| js::utf16_cmp(a, b));
    json!(names)
}

/// Copies `key` when present (JSON.stringify drops undefined, keeps null).
fn copy(out: &mut Map<String, Value>, from: &Value, key: &str) {
    if let Some(value) = from.get(key) {
        out.insert(key.to_owned(), value.clone());
    }
}

fn redact_url_value(value: &Value) -> Value {
    match value {
        Value::String(s) => Value::String(redact_url(s)),
        other => other.clone(),
    }
}

/// `describeModel`.
pub fn describe_model(model: &Value) -> Value {
    let mut out = Map::new();
    for key in ["provider", "id", "name", "api"] {
        copy(&mut out, model, key);
    }
    if let Some(base_url) = model.get("baseUrl") {
        out.insert("baseUrl".into(), redact_url_value(base_url));
    }
    for key in ["reasoning", "input", "contextWindow", "maxTokens"] {
        copy(&mut out, model, key);
    }
    for key in ["samplingParams", "compat"] {
        let value = model.get(key);
        out.insert(
            key.into(),
            if js::truthy(value) {
                redact_json_value(value.unwrap_or(&Value::Null))
            } else {
                Value::Null
            },
        );
    }
    out.insert(
        "thinkingLevelMap".into(),
        model.get("thinkingLevelMap").cloned().unwrap_or(Value::Null),
    );
    out.insert("headerNames".into(), sorted_keys(model.get("headers")));
    Value::Object(out)
}

/// What `describeProvider` reads from the provider definition and the model runtime.
#[derive(Debug, Clone)]
pub struct ProviderInfo {
    /// The provider definition: `id`, `name`, optional `baseUrl` and `headers`, and `auth`.
    pub provider: Value,
    /// `modelRuntime.getProviderAuthStatus(id)`.
    pub auth_status: Value,
    pub using_oauth: bool,
    pub registered_by_extension: bool,
}

/// `describeProvider`.
pub fn describe_provider(info: &ProviderInfo) -> Value {
    let provider = &info.provider;
    let auth = provider.get("auth");
    let mut auth_types = Vec::new();
    if js::truthy(auth.and_then(|a| a.get("apiKey"))) {
        auth_types.push("api_key");
    }
    if js::truthy(auth.and_then(|a| a.get("oauth"))) {
        auth_types.push("oauth");
    }
    let mut out = Map::new();
    copy(&mut out, provider, "id");
    copy(&mut out, provider, "name");
    let base_url = provider.get("baseUrl");
    out.insert(
        "baseUrl".into(),
        if js::truthy(base_url) {
            redact_url_value(base_url.unwrap_or(&Value::Null))
        } else {
            Value::Null
        },
    );
    out.insert("headerNames".into(), sorted_keys(provider.get("headers")));
    out.insert("authTypes".into(), json!(auth_types));
    out.insert("authStatus".into(), info.auth_status.clone());
    out.insert("usingOAuth".into(), json!(info.using_oauth));
    out.insert("registeredByExtension".into(), json!(info.registered_by_extension));
    Value::Object(out)
}

/// `describeExtension`.
pub fn describe_extension(extension: &Extension) -> Value {
    json!({
        "path": extension.path,
        "source": redact_url(&extension.source_info.source),
        "scope": extension.source_info.scope,
        "origin": extension.source_info.origin,
        "hidden": extension.hidden == Some(true),
    })
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ExtensionError {
    pub path: String,
    pub error: String,
}

/// Inputs of `collectBugReportMetadata`. `id` and `created_at` replace the TypeScript `uuidv7()` and clock.
#[derive(Debug, Clone)]
pub struct MetadataInput<'a> {
    pub id: &'a str,
    pub created_at: &'a str,
    pub hint: Option<&'a str>,
    pub session_id: &'a str,
    pub cwd: &'a str,
    pub include_session: bool,
    pub include_summary: bool,
    pub message_count: u64,
    pub model: Option<&'a Value>,
    /// The model's provider, if the runtime knows it; ignored without a model.
    pub provider: Option<&'a ProviderInfo>,
    pub thinking_level: &'a str,
    pub extensions: &'a [Extension],
    pub extension_errors: &'a [ExtensionError],
    pub global_settings: &'a Value,
    pub project_settings: &'a Value,
    pub host: &'a Host,
    pub env: &'a BTreeMap<String, String>,
}

/// `collectBugReportMetadata`: `report.json`. Settings lose `trackingId` and every credential; the cwd is only
/// included with the session.
pub fn collect_bug_report_metadata(input: &MetadataInput<'_>) -> Value {
    let hint = input.hint.map(js::trim).filter(|h| !h.is_empty());
    let mut session = Map::new();
    session.insert("id".into(), json!(input.session_id));
    session.insert("included".into(), json!(input.include_session));
    session.insert("summaryIncluded".into(), json!(input.include_summary));
    session.insert("messageCount".into(), json!(input.message_count));
    if input.include_session {
        session.insert("cwd".into(), json!(input.cwd));
    }
    let provider = input.model.and(input.provider).map(describe_provider);
    json!({
        "schemaVersion": SCHEMA_VERSION,
        "id": input.id,
        "createdAt": input.created_at,
        "hint": hint,
        "environment": collect_environment(input.host, input.env),
        "session": session,
        "model": input.model.map(describe_model),
        "provider": provider,
        "thinkingLevel": input.thinking_level,
        "extensions": input.extensions.iter().map(describe_extension).collect::<Vec<_>>(),
        "extensionErrors": input.extension_errors.iter().map(|e| json!({ "path": e.path, "error": e.error })).collect::<Vec<_>>(),
        "settings": {
            "global": redact_settings(input.global_settings),
            "project": redact_settings(input.project_settings),
        },
    })
}

fn is_empty_like(value: &Value) -> bool {
    match value {
        Value::Array(items) => items.is_empty(),
        Value::String(s) => s.is_empty(),
        _ => false,
    }
}

/// `collectBugReportDiagnostics`: failed or diagnosed assistant turns and recent crashes, without conversation content.
pub fn collect_bug_report_diagnostics(session_id: &str, entries: &[Value], crashes: &[CrashRecord]) -> Value {
    let mut assistant = Vec::new();
    let mut assistant_count = 0u64;
    for entry in entries {
        let Some(message) = entry.get("message") else { continue };
        if entry.get("type").and_then(Value::as_str) != Some("message")
            || message.get("role").and_then(Value::as_str) != Some("assistant")
        {
            continue;
        }
        assistant_count += 1;
        let diagnostics = match message.get("diagnostics") {
            None | Some(Value::Null) => Value::Array(Vec::new()),
            Some(value) => value.clone(),
        };
        let stop_reason = message.get("stopReason").and_then(Value::as_str);
        if is_empty_like(&diagnostics)
            && stop_reason != Some("error")
            && stop_reason != Some("aborted")
            && !js::truthy(message.get("errorMessage"))
        {
            continue;
        }
        let mut out = Map::new();
        if let Some(id) = entry.get("id") {
            out.insert("entryId".into(), id.clone());
        }
        copy(&mut out, entry, "timestamp");
        for key in [
            "provider",
            "model",
            "api",
            "stopReason",
            "rawStopReason",
            "errorMessage",
        ] {
            copy(&mut out, message, key);
        }
        out.insert("diagnostics".into(), diagnostics);
        assistant.push(Value::Object(out));
    }
    let crashes: Vec<Value> = crashes
        .iter()
        .map(|record| {
            let mut fields = record.0.clone();
            fields.shift_remove("notified");
            Value::Object(fields)
        })
        .collect();
    json!({
        "schemaVersion": SCHEMA_VERSION,
        "sessionId": session_id,
        "entryCount": entries.len(),
        "assistantMessageCount": assistant_count,
        "assistant": assistant,
        "crashes": crashes,
    })
}

#[derive(Debug, Clone)]
pub struct BugReportBundle {
    pub metadata: Value,
    pub diagnostics: Value,
    pub session_jsonl: Option<String>,
    pub summary: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BugReportFile {
    pub name: String,
    pub content_type: String,
    pub data: String,
}

/// `JSON.stringify(value, null, 2)`.
pub(crate) fn pretty(value: &impl serde::Serialize) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

/// `bugReportFiles`: the files shared by upload and zip export.
pub fn bug_report_files(bundle: &BugReportBundle) -> Vec<BugReportFile> {
    let file = |name: &str, content_type: &str, data: String| BugReportFile {
        name: name.into(),
        content_type: content_type.into(),
        data,
    };
    let mut files = vec![
        file(
            "report.json",
            "application/json",
            format!("{}\n", pretty(&bundle.metadata)),
        ),
        file(
            "diagnostics.json",
            "application/json",
            format!("{}\n", pretty(&bundle.diagnostics)),
        ),
    ];
    if let Some(session) = &bundle.session_jsonl {
        files.push(file("session.jsonl", "application/x-ndjson", session.clone()));
    }
    if let Some(summary) = &bundle.summary {
        let data = if summary.ends_with('\n') {
            summary.clone()
        } else {
            format!("{summary}\n")
        };
        files.push(file("summary.md", "text/markdown", data));
    }
    files
}

/// `bugReportArchiveFileName`.
pub fn bug_report_archive_file_name(id: &str) -> String {
    format!("pi-bug-report-{id}.zip")
}

/// `writeBugReportArchive`: the bundle's files as a deflated zip stamped with local wall-clock time `at`.
pub fn write_bug_report_archive(
    bundle: &BugReportBundle,
    path: &Path,
    at: crate::zip::LocalDateTime,
) -> io::Result<()> {
    let files = bug_report_files(bundle);
    let entries: Vec<(&str, &[u8])> = files.iter().map(|f| (f.name.as_str(), f.data.as_bytes())).collect();
    std::fs::write(path, crate::zip::create_zip_archive(&entries, at))
}
