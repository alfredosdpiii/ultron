//! OAuth for remote MCP servers (`packages/mcp/src/oauth`, adapted there from the MCP TypeScript SDK, MIT; and
//! `extensions/mcp/oauth.ts`): discovery of the protected resource and its authorization server, dynamic client
//! registration, the PKCE authorization code flow against a loopback callback, refresh with rotating refresh tokens,
//! and the per-server credential store in `<agentDir>/mcp-auth.json`.
//!
//! Connections never start a browser flow on their own: they send the stored access token and, after a 401, try the
//! stored refresh token; when that is not possible they need sign-in, which `/mcp login` runs.

pub mod callback;
pub mod flow;
pub mod signin;
pub mod store;

use serde_json::{Map, Value};
use std::fmt;

/// An OAuth failure, displayed as the TypeScript error's message.
#[derive(Debug, Clone, PartialEq)]
pub enum OAuthError {
    /// A network failure (`TypeError` from fetch).
    Network(String),
    /// An OAuth error response (`OAuthError`): `code` is the `error` field.
    OAuth {
        code: String,
        message: String,
    },
    /// `OAuthIssuerMismatchError`, `OAuthInsecureEndpointError`, `OAuthRegistrationError`: the message.
    Refused(String),
    /// `McpOAuthAuthorizationRequiredError`: the user has to sign in.
    AuthorizationRequired,
    /// `McpSignInCancelledError`.
    Cancelled,
    Other(String),
}

impl fmt::Display for OAuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OAuthError::Network(message)
            | OAuthError::OAuth { message, .. }
            | OAuthError::Refused(message)
            | OAuthError::Other(message) => f.write_str(message),
            OAuthError::AuthorizationRequired => f.write_str("MCP OAuth authorization requires user interaction"),
            OAuthError::Cancelled => f.write_str("Sign-in cancelled"),
        }
    }
}

impl From<String> for OAuthError {
    fn from(message: String) -> Self {
        OAuthError::Other(message)
    }
}

/// A server's `WWW-Authenticate` challenge (`OAuthChallenge`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Challenge {
    pub resource_metadata_url: Option<String>,
    pub scope: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

/// One `name=value` or `name="value"` field of a challenge; an empty value counts as absent.
fn field(header: &str, name: &str) -> Option<String> {
    let lower = header.to_ascii_lowercase();
    let needle = format!("{}=", name.to_ascii_lowercase());
    let mut from = 0;
    while let Some(found) = lower[from..].find(&needle) {
        let at = from + found;
        let boundary = at == 0 || matches!(header.as_bytes()[at - 1], b',' | b' ' | b'\t' | b'\n' | b'\r');
        from = at + needle.len();
        if !boundary {
            continue;
        }
        let rest = &header[from..];
        let unquoted = || rest.split(|c: char| c.is_whitespace() || c == ',').next().unwrap_or("");
        let value = match rest.strip_prefix('"') {
            Some(quoted) if quoted.contains('"') => quoted.split('"').next().unwrap_or(""),
            _ => unquoted(),
        };
        return (!value.is_empty()).then(|| value.to_string());
    }
    None
}

/// `parseWwwAuthenticate`: a Bearer (or DPoP) challenge's resource metadata URL, scope and error.
pub fn parse_www_authenticate(header: Option<&str>) -> Challenge {
    let Some(header) = header.filter(|h| !h.is_empty()) else {
        return Challenge::default();
    };
    let scheme = header
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if scheme != "bearer" && scheme != "dpop" {
        return Challenge::default();
    }
    Challenge {
        resource_metadata_url: field(header, "resource_metadata")
            .and_then(|u| url::Url::parse(&u).ok())
            .map(|u| u.to_string()),
        scope: field(header, "scope"),
        error: field(header, "error"),
        error_description: field(header, "error_description"),
    }
}

// ---- metadata and response parsing (`oauth/types.ts`) ----

/// `null` and `""` count as absent: servers send them for fields they have no value for.
fn absent(value: Option<&Value>) -> bool {
    matches!(value, None | Some(Value::Null)) || value.and_then(Value::as_str) == Some("")
}

fn required_string(value: Option<&Value>, name: &str) -> Result<String, String> {
    value
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .ok_or_else(|| format!("Invalid {name}"))
}

fn optional_string(value: Option<&Value>, name: &str) -> Result<Option<Value>, String> {
    if absent(value) {
        return Ok(None);
    }
    required_string(value, name).map(|s| Some(Value::String(s)))
}

fn optional_strings(value: Option<&Value>, name: &str) -> Result<Option<Vec<String>>, String> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) if items.iter().all(Value::is_string) => {
            Ok(Some(items.iter().filter_map(Value::as_str).map(String::from).collect()))
        }
        Some(_) => Err(format!("Invalid {name}")),
    }
}

/// A URL that parses and is not `javascript:`, `data:` or `vbscript:`.
fn safe_url(value: Option<&Value>, name: &str) -> Result<String, String> {
    let text = required_string(value, name)?;
    let url = url::Url::parse(&text).map_err(|_| format!("Invalid {name}"))?;
    if ["javascript", "data", "vbscript"].contains(&url.scheme()) {
        return Err(format!("Invalid {name}"));
    }
    Ok(text)
}

fn optional_url(value: Option<&Value>, name: &str) -> Result<Option<Value>, String> {
    if absent(value) {
        return Ok(None);
    }
    safe_url(value, name).map(|s| Some(Value::String(s)))
}

fn object<'a>(value: &'a Value, name: &str) -> Result<&'a Map<String, Value>, String> {
    value.as_object().ok_or_else(|| format!("Invalid {name}"))
}

/// `compact({...input, key: value})`: a value replaces the key in place (or is appended); `None` removes it.
fn set(map: &mut Map<String, Value>, key: &str, value: Option<Value>) {
    match value {
        Some(value) => {
            map.insert(key.into(), value);
        }
        None => {
            map.shift_remove(key);
        }
    }
}

fn strings(items: Option<Vec<String>>) -> Option<Value> {
    items.map(|items| Value::Array(items.into_iter().map(Value::String).collect()))
}

/// `parseProtectedResourceMetadata`.
pub fn parse_protected_resource_metadata(value: &Value) -> Result<Value, String> {
    let input = object(value, "OAuth protected resource metadata")?;
    let mut out = input.clone();
    let resource = safe_url(input.get("resource"), "OAuth protected resource metadata resource")?;
    set(&mut out, "resource", Some(Value::String(resource)));
    let servers = optional_strings(input.get("authorization_servers"), "authorization_servers")?
        .map(|servers| {
            servers
                .iter()
                .map(|url| safe_url(Some(&Value::String(url.clone())), "authorization server URL"))
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?;
    set(&mut out, "authorization_servers", strings(servers));
    let scopes = optional_strings(input.get("scopes_supported"), "scopes_supported")?;
    set(&mut out, "scopes_supported", strings(scopes));
    Ok(Value::Object(out))
}

/// `parseAuthorizationServerMetadata`.
pub fn parse_authorization_server_metadata(value: &Value) -> Result<Value, String> {
    let input = object(value, "authorization server metadata")?;
    let response_types = optional_strings(input.get("response_types_supported"), "response_types_supported")?
        .ok_or("Invalid response_types_supported")?;
    let mut out = input.clone();
    set(
        &mut out,
        "issuer",
        Some(Value::String(safe_url(
            input.get("issuer"),
            "authorization server issuer",
        )?)),
    );
    set(
        &mut out,
        "authorization_endpoint",
        Some(Value::String(safe_url(
            input.get("authorization_endpoint"),
            "authorization endpoint",
        )?)),
    );
    set(
        &mut out,
        "token_endpoint",
        Some(Value::String(safe_url(input.get("token_endpoint"), "token endpoint")?)),
    );
    let registration = optional_url(input.get("registration_endpoint"), "registration endpoint")?;
    set(&mut out, "registration_endpoint", registration);
    let scopes = optional_strings(input.get("scopes_supported"), "scopes_supported")?;
    set(&mut out, "scopes_supported", strings(scopes));
    set(&mut out, "response_types_supported", strings(Some(response_types)));
    for key in [
        "grant_types_supported",
        "token_endpoint_auth_methods_supported",
        "code_challenge_methods_supported",
    ] {
        let items = optional_strings(input.get(key), key)?;
        set(&mut out, key, strings(items));
    }
    for key in [
        "client_id_metadata_document_supported",
        "authorization_response_iss_parameter_supported",
    ] {
        let flag = input.get(key).filter(|v| v.is_boolean()).cloned();
        set(&mut out, key, flag);
    }
    Ok(Value::Object(out))
}

/// `parseOAuthTokens`.
pub fn parse_tokens(value: &Value) -> Result<Value, String> {
    let input = object(value, "OAuth token response")?;
    let expires = if absent(input.get("expires_in")) {
        None
    } else {
        let n = session_file::js::to_number(&input["expires_in"]);
        if !n.is_finite() {
            return Err("Invalid expires_in".into());
        }
        Some(crate::js_json::js_number(n))
    };
    let mut out = Map::new();
    out.insert(
        "access_token".into(),
        Value::String(required_string(input.get("access_token"), "access_token")?),
    );
    out.insert(
        "token_type".into(),
        Value::String(required_string(input.get("token_type"), "token_type")?),
    );
    set(&mut out, "expires_in", expires);
    for key in ["scope", "refresh_token", "id_token"] {
        let item = optional_string(input.get(key), key)?;
        set(&mut out, key, item);
    }
    Ok(Value::Object(out))
}

/// `parseClientInformation`.
pub fn parse_client_information(value: &Value) -> Result<Value, String> {
    let input = object(value, "OAuth client registration response")?;
    let mut out = input.clone();
    set(
        &mut out,
        "client_id",
        Some(Value::String(required_string(input.get("client_id"), "client_id")?)),
    );
    let secret = optional_string(input.get("client_secret"), "client_secret")?;
    set(&mut out, "client_secret", secret);
    for key in ["client_id_issued_at", "client_secret_expires_at"] {
        let number = input.get(key).filter(|v| v.is_number()).cloned();
        set(&mut out, key, number);
    }
    let uris = optional_strings(input.get("redirect_uris"), "redirect_uris")?.unwrap_or_default();
    set(&mut out, "redirect_uris", strings(Some(uris)));
    Ok(Value::Object(out))
}
