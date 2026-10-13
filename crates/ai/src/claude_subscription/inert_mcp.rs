//! The inert MCP inventory server (`inert_mcp.py`): it advertises the request's manifest (exactly Ultron's `rlm`
//! tool) to Claude Code and executes nothing. `tools/call` always answers with a denial; Claude Code never runs a
//! tool (`--permission-mode dontAsk` denies it anyway), Ultron's host runs the cell. Served by the hidden
//! `ultron __internal inert-mcp <manifest>` role over stdio, one JSON-RPC message per line.

use serde_json::{Value, json};
use std::io::{BufRead, Write};

pub const SERVER_NAME: &str = "ultron-inert-inventory";
pub const DENIAL: &str = "Denied: native tools are inert; only Ultron executes tools.";

/// The reply to one request line; `None` for a notification or an unreadable line.
pub fn reply(manifest: &Value, line: &str) -> Option<Value> {
    let row: Value = serde_json::from_str(line).ok()?;
    let id = row.get("id")?.clone();
    let result = match row.get("method").and_then(Value::as_str) {
        Some("initialize") => json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": SERVER_NAME, "version": "1"},
        }),
        Some("tools/list") => json!({"tools": manifest}),
        Some("tools/call") => json!({"isError": true, "content": [{"type": "text", "text": DENIAL}]}),
        _ => json!({}),
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

/// Serve `manifest` until `input` ends.
pub fn serve(manifest: &Value, input: impl BufRead, mut output: impl Write) -> std::io::Result<()> {
    for line in input.lines() {
        if let Some(reply) = reply(manifest, &line?) {
            writeln!(output, "{reply}")?;
            output.flush()?;
        }
    }
    Ok(())
}

/// The role's entry point: `args` is `[<manifest path>]`.
pub fn run(args: &[String]) -> Result<(), String> {
    let [path] = args else {
        return Err("usage: ultron __internal inert-mcp <manifest>".into());
    };
    let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    let manifest: Value = serde_json::from_str(&text).map_err(|e| format!("{path}: {e}"))?;
    serve(&manifest, std::io::stdin().lock(), std::io::stdout().lock()).map_err(|e| e.to_string())
}
