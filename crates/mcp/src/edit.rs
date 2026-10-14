//! Editing an `mcp.json` (`extensions/mcp/config.ts` `addMcpServerConfig`, `removeMcpServerConfig`): the file is
//! read (an empty config when missing), its `mcpServers` changed, and written back with the file's own indentation
//! (`JSON.stringify(parsed, null, indent)`); other content is kept.

use serde_json::{Map, Value};
use std::path::Path;

/// `JSON.stringify(value, null, indent)`: JavaScript number formatting and key order (array-index keys first).
pub fn stringify_indented(value: &Value, indent: &str) -> String {
    let mut out = String::new();
    write(&mut out, value, indent, 0);
    out
}

fn number(n: &serde_json::Number) -> String {
    if n.is_i64() || n.is_u64() {
        return n.to_string();
    }
    let f = n.as_f64().unwrap_or(0.0);
    if f == 0.0 {
        return "0".into();
    }
    if (1e-6..1e21).contains(&f.abs()) {
        return format!("{f}");
    }
    let s = format!("{f:e}");
    match s.split_once('e') {
        Some((m, e)) if !e.starts_with('-') => format!("{m}e+{e}"),
        _ => s,
    }
}

fn index_key(key: &str) -> Option<u32> {
    let n: u32 = key.parse().ok()?;
    (n != u32::MAX && n.to_string() == key).then_some(n)
}

fn keys(map: &Map<String, Value>) -> Vec<&String> {
    let mut indices: Vec<(u32, &String)> = map.keys().filter_map(|k| index_key(k).map(|n| (n, k))).collect();
    indices.sort_by_key(|(n, _)| *n);
    let mut out: Vec<&String> = indices.into_iter().map(|(_, k)| k).collect();
    out.extend(map.keys().filter(|k| index_key(k).is_none()));
    out
}

fn newline(out: &mut String, indent: &str, level: usize) {
    out.push('\n');
    for _ in 0..level {
        out.push_str(indent);
    }
}

fn write(out: &mut String, value: &Value, indent: &str, level: usize) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&number(n)),
        Value::String(s) => out.push_str(&Value::String(s.clone()).to_string()),
        Value::Array(items) if items.is_empty() => out.push_str("[]"),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, indent, level + 1);
                write(out, item, indent, level + 1);
            }
            newline(out, indent, level);
            out.push(']');
        }
        Value::Object(map) if map.is_empty() => out.push_str("{}"),
        Value::Object(map) => {
            out.push('{');
            for (i, key) in keys(map).into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, indent, level + 1);
                out.push_str(&Value::String(key.clone()).to_string());
                out.push_str(": ");
                write(out, &map[key], indent, level + 1);
            }
            newline(out, indent, level);
            out.push('}');
        }
    }
}

/// `/^([ \t]+)\S/m` over the file text, else two spaces.
fn detect_indent(text: Option<&str>) -> String {
    for line in text.unwrap_or("").split('\n') {
        let lead = line.len() - line.trim_start_matches([' ', '\t']).len();
        if lead > 0 && line[lead..].chars().next().is_some_and(|c| !c.is_whitespace()) {
            return line[..lead].to_string();
        }
    }
    "  ".into()
}

fn edit_servers(path: &Path, edit: impl FnOnce(&mut Map<String, Value>) -> bool) -> Result<(), String> {
    let text = if path.exists() {
        Some(std::fs::read_to_string(path).map_err(|e| e.to_string())?)
    } else {
        None
    };
    let mut parsed: Value = match &text {
        None => Value::Object(Map::new()),
        Some(t) => serde_json::from_str(t).map_err(|e| e.to_string())?,
    };
    let shape = format!("{}: expected an object with an \"mcpServers\" object", path.display());
    let Some(object) = parsed.as_object_mut() else {
        return Err(shape);
    };
    if object.get("mcpServers").is_some_and(|s| !s.is_object()) {
        return Err(shape);
    }
    if !edit(object) {
        return Ok(());
    }
    let indent = detect_indent(text.as_deref());
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(path, format!("{}\n", stringify_indented(&parsed, &indent))).map_err(|e| e.to_string())
}

/// Add or replace a server; returns whether it replaced one.
pub fn add_server_config(path: &Path, name: &str, config: Map<String, Value>) -> Result<bool, String> {
    let mut replaced = false;
    edit_servers(path, |object| {
        let servers = object.entry("mcpServers").or_insert_with(|| Value::Object(Map::new()));
        if let Some(servers) = servers.as_object_mut() {
            replaced = servers.insert(name.to_string(), Value::Object(config)).is_some();
        }
        true
    })?;
    Ok(replaced)
}

/// Remove a server; false when the file does not define it.
pub fn remove_server_config(path: &Path, name: &str) -> Result<bool, String> {
    if !path.exists() {
        return Ok(false);
    }
    let mut removed = false;
    edit_servers(path, |object| {
        let Some(servers) = object.get_mut("mcpServers").and_then(Value::as_object_mut) else {
            return false;
        };
        if servers.shift_remove(name).is_none() {
            return false;
        }
        removed = true;
        true
    })?;
    Ok(removed)
}

/// `updateMcpServerConfig` with `enabled`: `true` removes the key (the default), `false` sets it.
pub fn update_server_enabled(path: &Path, name: &str, enabled: bool) -> Result<(), String> {
    let mut missing = false;
    edit_servers(path, |object| {
        let Some(server) = object
            .get_mut("mcpServers")
            .and_then(Value::as_object_mut)
            .and_then(|servers| servers.get_mut(name))
            .and_then(Value::as_object_mut)
        else {
            missing = true;
            return false;
        };
        if enabled {
            server.shift_remove("enabled");
        } else {
            // An existing key keeps its place, as a JavaScript assignment does.
            server.insert("enabled".into(), Value::Bool(false));
        }
        true
    })?;
    if missing {
        return Err(format!("{} does not define MCP server \"{name}\"", path.display()));
    }
    Ok(())
}
