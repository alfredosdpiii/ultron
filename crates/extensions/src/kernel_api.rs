//! Extension functions as kernel-API functions. The model has exactly one tool, the `rlm` REPL; an extension's
//! `functions` (TypeScript `registerTool`) are called from a Python cell as `ext.<extension>.<function>(**kwargs)`
//! and are listed in the runtime guide, never added to the model's tool list. Nothing in this crate produces a
//! model tool declaration.
//!
//! The kernel reaches them through one host operation, `ext.call` `{ extension, function, arguments }`
//! ([`call`]), and lists them with `ext.list` ([`listing`]); [`handle`] serves both. The runtime (`rlm/ext_api.py`)
//! binds `ext` from the listing the host puts in the kernel's environment ([`kernel_env`]), so `help(ext)` lists the
//! functions without a host round trip; with no functions `ext` is absent.

use crate::process::{ExtensionHost, FunctionSpec};
use crate::rpc::{CancelToken, RpcError};
use serde_json::{Value, json};
use std::time::Duration;

/// `name: type` for each top-level parameter in the function's JSON schema, in declaration order.
fn signature(parameters: &Value) -> String {
    let Some(properties) = parameters.get("properties").and_then(Value::as_object) else {
        return "**kwargs".into();
    };
    properties
        .iter()
        .map(|(name, schema)| match schema.get("type").and_then(Value::as_str) {
            Some(kind) => format!("{name}: {kind}"),
            None => name.clone(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The runtime-guide section listing every extension function; `None` when no extension registered any.
pub fn guide_section(functions: &[(String, FunctionSpec)]) -> Option<String> {
    if functions.is_empty() {
        return None;
    }
    let mut out = String::from(
        "Extension functions (call them in a cell, e.g. `await ext.<extension>.<function>(...)`; they are not tools):",
    );
    for (extension, function) in functions {
        out.push_str(&format!(
            "\n- ext.{extension}.{}({})",
            function.name,
            signature(&function.parameters)
        ));
        if !function.description.is_empty() {
            out.push_str(&format!(" - {}", function.description));
        }
    }
    Some(out)
}

/// `ext.list`: what the kernel installs under `ext` (and what `help(ext)` shows).
pub fn listing(host: &ExtensionHost) -> Value {
    Value::Array(
        host.functions()
            .into_iter()
            .map(|(extension, f)| {
                json!({ "extension": extension, "name": f.name, "description": f.description, "parameters": f.parameters })
            })
            .collect(),
    )
}

/// The kernel's environment variable holding the function listing.
pub const KERNEL_ENV: &str = "ULTRON_EXT_FUNCTIONS";

/// The `(name, value)` to add to the kernel's environment so the runtime binds `ext`: a compact listing
/// `[{extension, name, description, signature}]` (signatures as in [`guide_section`], not whole schemas, to keep the
/// variable small). `None` when no extension registered a function: `ext` is then absent.
pub fn kernel_env(functions: &[(String, FunctionSpec)]) -> Option<(String, String)> {
    if functions.is_empty() {
        return None;
    }
    let listing: Vec<Value> = functions
        .iter()
        .map(|(extension, f)| {
            json!({ "extension": extension, "name": f.name, "description": f.description, "signature": signature(&f.parameters) })
        })
        .collect();
    Some((KERNEL_ENV.into(), Value::Array(listing).to_string()))
}

/// Serves the extension host requests a cell makes (`ext.call`, `ext.list`); `None` for any other kind.
pub fn handle(
    host: &ExtensionHost,
    kind: &str,
    params: &Value,
    timeout: Option<Duration>,
    cancel: Option<&CancelToken>,
) -> Option<Result<Value, RpcError>> {
    match kind {
        "ext.call" => Some(call(host, params, timeout, cancel)),
        "ext.list" => Some(Ok(listing(host))),
        _ => None,
    }
}

/// `ext.call` `{ extension, function, arguments }` from the kernel.
pub fn call(
    host: &ExtensionHost,
    params: &Value,
    timeout: Option<Duration>,
    cancel: Option<&CancelToken>,
) -> Result<Value, RpcError> {
    let field = |key: &str| {
        params
            .get(key)
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::Other(format!("ext.call needs a string \"{key}\"")))
    };
    let arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
    if !arguments.is_object() {
        return Err(RpcError::Other("ext.call \"arguments\" must be an object".into()));
    }
    host.call_function(field("extension")?, field("function")?, arguments, timeout, cancel)
}
