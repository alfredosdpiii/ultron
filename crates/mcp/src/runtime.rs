//! Transport construction for one configured server (`extensions/mcp/runtime.ts` `createDefaultTransport`):
//! `env` and `headers` values resolve `$VAR`/`${VAR}`/`!cmd`, stdio paths expand `~`, and the resolved values are
//! returned so the host can register them for secret masking. Also the runtime's transient-error rule.

use crate::http::{HttpOptions, http_error};
use crate::jsonrpc::Error;
use crate::resolve::{expand_home, resolve_config_value_or_err, resolve_headers_or_err};
use crate::stdio::StdioOptions;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

/// Delays between attempts to connect to an HTTP server that failed with a transient error.
pub const CONNECT_RETRY_DELAYS_MS: [u64; 2] = [250, 1_000];

/// A transport to build, and the secret values it carries (env values, header credentials).
pub enum TransportSpec {
    Stdio(StdioOptions),
    Http(HttpOptions),
}

/// `path.resolve(base, path)`: absolute and lexically normalized.
fn resolve_path(base: &Path, path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in base.join(path).components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

fn string_pairs(value: Option<&Value>) -> Vec<(String, String)> {
    value
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
        .collect()
}

/// `createDefaultTransport` for a validated server config; `env_overlay` overlays the process environment for value
/// resolution (empty in production). Returns the transport options and the secret values.
pub fn transport_spec(
    name: &str,
    config: &Map<String, Value>,
    cwd: &Path,
    home: &Path,
    env_overlay: &HashMap<String, String>,
) -> Result<(TransportSpec, Vec<String>), String> {
    if let Some(url) = config.get("url").and_then(Value::as_str) {
        let headers = resolve_headers_or_err(
            &string_pairs(config.get("headers")),
            &format!("MCP server \"{name}\""),
            env_overlay,
        )?
        .unwrap_or_default();
        let secrets = headers.iter().map(|(_, v)| strip_auth_scheme(v)).collect();
        let options = HttpOptions {
            url: url.into(),
            headers,
            ..HttpOptions::default()
        };
        return Ok((TransportSpec::Http(options), secrets));
    }
    let mut env = Vec::new();
    for (key, value) in string_pairs(config.get("env")) {
        env.push((
            key.clone(),
            resolve_config_value_or_err(&value, &format!("MCP server \"{name}\" env \"{key}\""), env_overlay)?,
        ));
    }
    let secrets = env.iter().map(|(_, v)| v.clone()).collect();
    let home_str = |v: &str| expand_home(v, home).to_string_lossy().into_owned();
    let options = StdioOptions {
        command: home_str(config.get("command").and_then(Value::as_str).unwrap_or_default()),
        args: config
            .get("args")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|a| home_str(a.as_str().unwrap_or_default()))
            .collect(),
        cwd: Some(resolve_path(
            cwd,
            &expand_home(config.get("cwd").and_then(Value::as_str).unwrap_or("."), home),
        )),
        env,
        inherit_env: (config.get("inheritEnv") == Some(&Value::Bool(false))).then_some(false),
        ..StdioOptions::default()
    };
    Ok((TransportSpec::Stdio(options), secrets))
}

/// `Bearer x` and `Basic x` register only `x` as the secret.
fn strip_auth_scheme(value: &str) -> String {
    for scheme in ["bearer", "basic"] {
        if value.len() > scheme.len() && value[..scheme.len()].eq_ignore_ascii_case(scheme) {
            let rest = &value[scheme.len()..];
            let trimmed = rest.trim_start();
            if trimmed.len() < rest.len() {
                return trimmed.into();
            }
        }
    }
    value.into()
}

/// Network failures and overloaded or restarting servers (408, 429, 5xx except 501), which are worth another attempt.
pub fn is_transient_error(error: &Error) -> bool {
    match http_error(error) {
        Some(("TypeError", _)) => true,
        Some((_, status)) => status == 408 || status == 429 || (status >= 500 && status != 501),
        None => false,
    }
}
