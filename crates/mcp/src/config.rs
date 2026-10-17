//! `mcp.json` server configuration (`core/mcp-servers.ts`, `extensions/mcp/config.ts`).
//!
//! Servers come from `<agentDir>/mcp.json` and, for trusted projects, `<cwd>/.pi/mcp.json`; project entries replace
//! global ones with the same name. Entries stay JSON objects so unknown fields survive. In Ultron MCP tools are never
//! model tools: the kernel's `mcp` namespace reaches them, and of `exposure` only `hidden` has an effect.

use serde_json::{Map, Value, json};
use std::path::Path;
use url::Url;

const EXPOSURES: [&str; 4] = ["codemode", "deferred", "direct", "hidden"];
const LIFECYCLES: [&str; 4] = ["lazy", "eager", "keep-alive", "lazy-keep-alive"];
const LOOPBACK_HOSTS: [&str; 3] = ["localhost", "127.0.0.1", "[::1]"];
pub const CONFIG_DIR_NAME: &str = ".pi";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Global,
    Project,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Global => "global",
            Scope::Project => "project",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ServerEntry {
    pub name: String,
    /// The validated server object (`command` = stdio, `url` = streamable HTTP).
    pub config: Map<String, Value>,
    pub source: String,
    pub scope: Scope,
}

impl ServerEntry {
    pub fn enabled(&self) -> bool {
        self.config.get("enabled") != Some(&Value::Bool(false))
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LoadedConfig {
    pub servers: Vec<ServerEntry>,
    pub errors: Vec<String>,
}

/// Namespace of a server's tools: `mcp__<server>` with `-` replaced by `_`.
pub fn mcp_namespace(server: &str) -> String {
    format!("mcp__{}", server.replace('-', "_"))
}

/// A JavaScript number as JSON: whole values print without a fraction, like `JSON.stringify`.
fn js_number(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() < 9_007_199_254_740_992.0 {
        json!(value as i64)
    } else {
        json!(value)
    }
}

fn is_string_record(value: &Value) -> bool {
    value.as_object().is_some_and(|m| m.values().all(Value::is_string))
}

fn is_exposure(value: &Value) -> bool {
    value.as_str().is_some_and(|v| EXPOSURES.contains(&v))
}

fn resolve_alias(value: &Value) -> Value {
    if value == "codemode-deferred" {
        json!("codemode")
    } else {
        value.clone()
    }
}

/// `new URL(value)` when it parses (`URL.canParse`).
fn parse_url(value: &Value) -> Option<Url> {
    value.as_str().and_then(|s| Url::parse(s).ok())
}

fn hostname(url: &Url) -> String {
    url.host_str().unwrap_or_default().to_string()
}

fn is_loopback(url: &Url) -> bool {
    LOOPBACK_HOSTS.contains(&hostname(url).as_str())
}

/// Whether a redirect URI can be served by the loopback callback server.
pub fn is_loopback_redirect_uri(value: &str) -> bool {
    Url::parse(value).is_ok_and(|url| {
        url.scheme() == "http"
            && is_loopback(&url)
            && url.query().is_none_or(str::is_empty)
            && url.fragment().is_none_or(str::is_empty)
    })
}

fn validate_oauth(value: Option<&Value>) -> Option<String> {
    let value = value?;
    let Some(oauth) = value.as_object() else {
        return Some("oauth must be an object".into());
    };
    let not_string = |key: &str| oauth.get(key).is_some_and(|v| !v.is_string());
    if not_string("clientId") {
        return Some("oauth.clientId must be a string".into());
    }
    if not_string("clientSecret") {
        return Some("oauth.clientSecret must be a string".into());
    }
    let port = oauth.get("callbackPort");
    if let Some(port) = port
        && !port
            .as_f64()
            .is_some_and(|p| p.fract() == 0.0 && (1.0..=65535.0).contains(&p))
    {
        return Some("oauth.callbackPort must be a port number".into());
    }
    if let Some(callback) = oauth.get("callbackUrl") {
        let Some(url) = callback
            .as_str()
            .filter(|s| is_loopback_redirect_uri(s))
            .and_then(|s| Url::parse(s).ok())
        else {
            return Some(
                "oauth.callbackUrl must be an http URI on localhost, 127.0.0.1, or [::1] without query or fragment"
                    .into(),
            );
        };
        if let (Some(url_port), Some(port)) = (url.port(), port.and_then(Value::as_f64))
            && f64::from(url_port) != port
        {
            return Some("oauth.callbackUrl and oauth.callbackPort name different ports".into());
        }
    }
    if not_string("scope") {
        return Some("oauth.scope must be a string".into());
    }
    if oauth
        .get("clientName")
        .is_some_and(|v| v.as_str().is_none_or(|s| s.trim().is_empty()))
    {
        return Some("oauth.clientName must be a non-empty string".into());
    }
    if let Some(metadata) = oauth.get("authServerMetadataUrl") {
        let ok = parse_url(metadata)
            .is_some_and(|url| url.scheme() == "https" || (url.scheme() == "http" && is_loopback(&url)));
        if !ok {
            return Some(
                "oauth.authServerMetadataUrl must be an https URL, or http on localhost, 127.0.0.1, or [::1]".into(),
            );
        }
    }
    None
}

/// One server entry as pi-mcp-adapter wrote it, translated to the validated shape. `defaults` are the
/// adapter's top-level `settings`.
pub fn normalize_adapter_server_config(raw: &Value, defaults: &Value) -> Value {
    let Some(raw) = raw.as_object() else { return raw.clone() };
    let mut value = raw.clone();
    if value.get("disabled") == Some(&Value::Bool(true)) && !value.contains_key("enabled") {
        value.insert("enabled".into(), Value::Bool(false));
    }
    value.shift_remove("disabled");
    let timeout_ms = value
        .get("requestTimeoutMs")
        .or_else(|| defaults.get("requestTimeoutMs"))
        .and_then(Value::as_f64);
    if !value.contains_key("timeout")
        && let Some(ms) = timeout_ms.filter(|ms| *ms > 0.0)
    {
        value.insert("timeout".into(), js_number(ms / 1000.0));
    }
    value.shift_remove("requestTimeoutMs");
    if value.get("auth").is_some_and(Value::is_string) {
        value.shift_remove("auth");
    }
    let non_empty = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let bearer = non_empty("bearerToken").or_else(|| non_empty("bearerTokenEnv").map(|env| format!("${{{env}}}")));
    if let Some(bearer) = bearer
        && value.get("url").is_some_and(Value::is_string)
    {
        let mut headers = value
            .get("headers")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let has_authorization = headers.keys().any(|h| h.eq_ignore_ascii_case("authorization"));
        // A `!command` value runs as a whole, so it cannot take the `Bearer ` prefix.
        if !has_authorization && !bearer.starts_with('!') {
            headers.insert("Authorization".into(), json!(format!("Bearer {bearer}")));
            value.insert("headers".into(), Value::Object(headers));
        }
    }
    value.shift_remove("bearerToken");
    value.shift_remove("bearerTokenEnv");
    Value::Object(value)
}

/// `*` matches any characters; everything else is literal.
fn pattern_matches(pattern: &str, name: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    let (first, rest) = (parts[0], &parts[1..]);
    let Some(mut remaining) = name.strip_prefix(first) else {
        return false;
    };
    for (i, part) in rest.iter().enumerate() {
        if i == rest.len() - 1 {
            return remaining.ends_with(part);
        }
        match remaining.find(part) {
            Some(at) => remaining = &remaining[at + part.len()..],
            None => return false,
        }
    }
    remaining.is_empty()
}

/// Exposure of one tool: its `toolExposure` entry (exact name first, then the first matching pattern), else the
/// server's `exposure`, else `codemode`.
pub fn tool_exposure(config: &Map<String, Value>, tool: &str) -> String {
    let overrides = config.get("toolExposure").and_then(Value::as_object);
    if let Some(overrides) = overrides {
        if let Some(exact) = overrides.get(tool).and_then(Value::as_str) {
            return exact.into();
        }
        for (pattern, exposure) in overrides {
            if pattern.contains('*') && pattern_matches(pattern, tool) {
                return exposure.as_str().unwrap_or_default().into();
            }
        }
    }
    config
        .get("exposure")
        .and_then(Value::as_str)
        .unwrap_or("codemode")
        .into()
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Validates one `mcpServers` entry; returns the entry with exposure aliases resolved, or the error message.
pub fn validate_server_config(name: &str, raw: &Value) -> Result<Map<String, Value>, String> {
    if !valid_name(name) {
        return Err(format!(
            "invalid server name \"{name}\" (use letters, digits, \"_\" and \"-\")"
        ));
    }
    let Some(raw) = raw.as_object() else {
        return Err(format!("server \"{name}\" must be an object"));
    };
    let mut value = raw.clone();
    if let Some(exposure) = value.get("exposure").map(resolve_alias) {
        value.insert("exposure".into(), exposure);
    }
    if let Some(Value::Object(tools)) = value.get_mut("toolExposure") {
        for entry in tools.values_mut() {
            *entry = resolve_alias(entry);
        }
    }
    let quoted = |list: &[&str]| list.iter().map(|v| format!("\"{v}\"")).collect::<Vec<_>>().join(", ");
    let exposures = quoted(&EXPOSURES);
    if value
        .get("lifecycle")
        .is_some_and(|l| !l.as_str().is_some_and(|l| LIFECYCLES.contains(&l)))
    {
        return Err(format!(
            "server \"{name}\": lifecycle must be one of {}",
            quoted(&LIFECYCLES)
        ));
    }
    if value.get("exposure").is_some_and(|e| !is_exposure(e)) {
        return Err(format!("server \"{name}\": exposure must be one of {exposures}"));
    }
    if let Some(tools) = value.get("toolExposure") {
        let Some(tools) = tools.as_object() else {
            return Err(format!(
                "server \"{name}\": toolExposure must map tool names to exposures"
            ));
        };
        if let Some((tool, _)) = tools.iter().find(|(_, v)| !is_exposure(v)) {
            return Err(format!(
                "server \"{name}\": toolExposure \"{tool}\" must be one of {exposures}"
            ));
        }
    }
    if value.get("enabled").is_some_and(|v| !v.is_boolean()) {
        return Err(format!("server \"{name}\": enabled must be a boolean"));
    }
    if value.get("description").is_some_and(|v| !v.is_string()) {
        return Err(format!("server \"{name}\": description must be a string"));
    }
    if value
        .get("timeout")
        .is_some_and(|v| !v.as_f64().is_some_and(|t| t > 0.0))
    {
        return Err(format!(
            "server \"{name}\": timeout must be a positive number of seconds"
        ));
    }
    let kind = value.get("type").and_then(Value::as_str);
    let has_type = value.contains_key("type");
    if kind == Some("sse") {
        return Err(format!(
            "server \"{name}\": legacy SSE transport is not supported; use the streamable HTTP URL"
        ));
    }
    if value.get("url").is_some_and(Value::is_string) && (!has_type || matches!(kind, Some("http" | "streamable-http")))
    {
        let Some(url) = parse_url(&value["url"]).filter(|u| u.scheme() == "http" || u.scheme() == "https") else {
            return Err(format!("server \"{name}\": url must be an http or https URL"));
        };
        if value.get("headers").is_some_and(|h| !is_string_record(h)) {
            return Err(format!("server \"{name}\": headers must map names to strings"));
        }
        if let Some(error) = validate_oauth(value.get("oauth")) {
            return Err(format!("server \"{name}\": {error}"));
        }
        if let Some(auth) = value.get("auth") {
            if !auth
                .get("provider")
                .and_then(Value::as_str)
                .is_some_and(|p| !p.is_empty())
                || !auth.is_object()
            {
                return Err(format!("server \"{name}\": auth.provider must be a provider name"));
            }
            if url.scheme() != "https" && !is_loopback(&url) {
                return Err(format!(
                    "server \"{name}\": auth requires an https URL, or http on localhost, 127.0.0.1, or [::1]"
                ));
            }
        }
        return Ok(value);
    }
    if value.get("command").is_some_and(Value::is_string) && (!has_type || kind == Some("stdio")) {
        if value
            .get("args")
            .is_some_and(|a| !a.as_array().is_some_and(|a| a.iter().all(Value::is_string)))
        {
            return Err(format!("server \"{name}\": args must be an array of strings"));
        }
        if value.get("env").is_some_and(|e| !is_string_record(e)) {
            return Err(format!("server \"{name}\": env must map names to strings"));
        }
        if value.get("cwd").is_some_and(|c| !c.is_string()) {
            return Err(format!("server \"{name}\": cwd must be a string"));
        }
        if value.get("inheritEnv").is_some_and(|c| !c.is_boolean()) {
            return Err(format!("server \"{name}\": inheritEnv must be a boolean"));
        }
        return Ok(value);
    }
    Err(format!(
        "server \"{name}\" needs either \"command\" (stdio) or \"url\" (streamable HTTP)"
    ))
}

fn read_config_file(path: &Path, scope: Scope, loaded: &mut LoadedConfig) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let shown = path.display();
    let parsed: Value = match serde_json::from_str(&text) {
        Ok(parsed) => parsed,
        Err(error) => {
            loaded.errors.push(format!("{shown}: {error}"));
            return;
        }
    };
    let servers = parsed.get("mcpServers");
    if !parsed.is_object() || servers.is_some_and(|s| !s.is_object()) {
        loaded
            .errors
            .push(format!("{shown}: expected an object with an \"mcpServers\" object"));
        return;
    }
    // config.ts validates `autoEnableCodemode`; its value only turns on a codemode tool, and the model's one tool
    // is `rlm`, so nothing reads it.
    if parsed
        .get("autoEnableCodemode")
        .is_some_and(|value| !value.is_boolean())
    {
        loaded
            .errors
            .push(format!("{shown}: autoEnableCodemode must be a boolean"));
    }
    // pi-mcp-adapter's top-level `settings` carry defaults for its per-server fields.
    let defaults = parsed
        .get("settings")
        .filter(|s| s.is_object())
        .cloned()
        .unwrap_or(json!({}));
    for (name, raw) in servers.and_then(Value::as_object).into_iter().flatten() {
        let config = match validate_server_config(name, &normalize_adapter_server_config(raw, &defaults)) {
            Ok(config) => config,
            Err(error) => {
                loaded.errors.push(format!("{shown}: {error}"));
                continue;
            }
        };
        // Names that differ only in `-` and `_` would share a namespace.
        let namespace = mcp_namespace(name);
        if let Some(clash) = loaded
            .servers
            .iter()
            .find(|s| &s.name != name && mcp_namespace(&s.name) == namespace)
        {
            loaded
                .errors
                .push(format!("{shown}: server \"{name}\" conflicts with \"{}\"", clash.name));
            continue;
        }
        if scope == Scope::Project && config.contains_key("url") && config.contains_key("auth") {
            loaded.errors.push(format!(
                "{shown}: server \"{name}\": auth is only allowed in the global mcp.json"
            ));
            continue;
        }
        let entry = ServerEntry {
            name: name.clone(),
            config,
            source: shown.to_string(),
            scope,
        };
        match loaded.servers.iter_mut().find(|s| s.name == *name) {
            Some(existing) => *existing = entry,
            None => loaded.servers.push(entry),
        }
    }
}

/// Global and (when trusted) project configuration. Disabled servers are included with `enabled: false`.
pub fn load_config(agent_dir: &Path, cwd: &Path, project_trusted: bool) -> LoadedConfig {
    let mut loaded = LoadedConfig::default();
    read_config_file(&agent_dir.join("mcp.json"), Scope::Global, &mut loaded);
    if project_trusted {
        read_config_file(&cwd.join(CONFIG_DIR_NAME).join("mcp.json"), Scope::Project, &mut loaded);
    }
    loaded
}
