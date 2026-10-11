//! MCP tool results and resources for the REPL (`extensions/mcp/tools.ts`, `extensions/mcp/resources.ts`): binary
//! resources are saved to files, resource links name `mcp.read`, MCP App resources are hidden from listings.

use crate::content::to_llm_content;
use base64::Engine;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use serde_json::{Map, Value, json};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

/// Saves a binary resource and returns the file path. The extension includes the dot, for example `.bin`.
pub type OutputSaver<'a> = &'a dyn Fn(&[u8], &str) -> Result<String, String>;

/// `Buffer.from(blob, "base64")` accepts missing padding and stray whitespace.
const LENIENT: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new()
        .with_decode_allow_trailing_bits(true)
        .with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// Writes `data` to `<tmp>/ultron-mcp-<16 hex><extension>`, readable only by the user (results can carry private data).
pub fn save_to_temp_file(data: &[u8], extension: &str) -> Result<String, String> {
    let mut random = [0u8; 8];
    getrandom::fill(&mut random).map_err(|e| e.to_string())?;
    let hex: String = random.iter().map(|b| format!("{b:02x}")).collect();
    let path: PathBuf = std::env::temp_dir().join(format!("ultron-mcp-{hex}{extension}"));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| e.to_string())?;
    file.write_all(data).map_err(|e| e.to_string())?;
    Ok(path.to_string_lossy().into_owned())
}

/// `formatSize` of `core/tools/truncate.ts`.
pub fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// The texts of `content`, joined by newlines.
pub fn text_of(content: &[Value]) -> String {
    content
        .iter()
        .filter(|b| b["type"] == "text")
        .map(|b| b["text"].as_str().unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The extension a saved binary resource gets: the one its URI path ends in (1-8 alphanumerics), else `.bin`.
fn extension_of(uri: &str) -> String {
    let parsed = url::Url::parse(uri).ok();
    let path = parsed.as_ref().map_or(uri, |u| u.path());
    if let Some(dot) = path.rfind('.') {
        let ext = &path[dot + 1..];
        if (1..=8).contains(&ext.len()) && ext.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return path[dot..].to_string();
        }
    }
    ".bin".into()
}

fn is_text_mime_type(mime: Option<&str>) -> bool {
    let Some(mime) = mime.filter(|m| !m.is_empty()) else {
        return false;
    };
    let kind = mime.split(';').next().unwrap_or_default().trim().to_lowercase();
    kind.starts_with("text/") || kind == "application/json" || kind.ends_with("+json") || kind.ends_with("+xml")
}

fn text(text: String) -> Value {
    json!({ "type": "text", "text": text })
}

/// A non-empty string field (JavaScript truthiness).
fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn block_to_content(server: &str, block: &Value, readable: bool, save: OutputSaver) -> Vec<Value> {
    if block["type"] == "resource_link" {
        let mut details: Vec<String> = str_field(block, "mimeType").map(String::from).into_iter().collect();
        if let Some(size) = block.get("size").and_then(Value::as_f64) {
            details.push(format_size(size as usize));
        }
        let read = if readable {
            format!(". Read it with await mcp.read(\"{server}\", uri)")
        } else {
            String::new()
        };
        let description = str_field(block, "description")
            .map(|d| format!(": {d}"))
            .unwrap_or_default();
        let title = block
            .get("title")
            .and_then(Value::as_str)
            .or_else(|| block.get("name").and_then(Value::as_str))
            .unwrap_or("undefined");
        let details = if details.is_empty() {
            String::new()
        } else {
            format!(" ({})", details.join(", "))
        };
        let uri = block["uri"].as_str().unwrap_or_default();
        return vec![text(format!(
            "[Resource {uri} \"{title}\"{details}{description}{read}]"
        ))];
    }
    let resource = &block["resource"];
    let mime = resource.get("mimeType").and_then(Value::as_str);
    if block["type"] == "resource"
        && let Some(blob) = resource.get("blob").and_then(Value::as_str)
        && !mime.is_some_and(|m| m.starts_with("image/"))
    {
        let uri = resource["uri"].as_str().unwrap_or_default();
        let clean: String = blob.chars().filter(|c| !c.is_whitespace()).collect();
        let data = LENIENT
            .decode(clean.replace('-', "+").replace('_', "/"))
            .unwrap_or_default();
        if is_text_mime_type(mime) {
            return vec![text(String::from_utf8_lossy(&data).into_owned())];
        }
        let kind = format!("{}, {}", mime.unwrap_or("unknown type"), format_size(data.len()));
        return vec![text(match save(&data, &extension_of(uri)) {
            Ok(path) => format!("[Binary resource {uri} ({kind}) saved to {path}]"),
            Err(reason) => format!("[Binary resource {uri} ({kind}) could not be saved: {reason}]"),
        })];
    }
    to_llm_content(&json!({ "content": [block] }))
}

/// `toModelContent`: model-facing content of a server's content blocks.
pub fn to_model_content(server: &str, blocks: &[Value], readable: bool, save: OutputSaver) -> Vec<Value> {
    blocks
        .iter()
        .flat_map(|b| block_to_content(server, b, readable, save))
        .collect()
}

/// `convertMcpResult`: content for the REPL and whether the tool reported an error. Without content blocks the
/// structured content is returned as JSON; an error without text gets a generic message.
pub fn convert_mcp_result(server: &str, tool: &str, result: &Value, readable: bool, save: OutputSaver) -> Value {
    let blocks = result
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut content = if blocks.is_empty() {
        to_llm_content(result)
    } else {
        to_model_content(server, &blocks, readable, save)
    };
    let is_error = result.get("isError") == Some(&Value::Bool(true));
    if is_error && text_of(&content).is_empty() {
        content.push(text(format!("MCP tool {server}/{tool} returned an error")));
    }
    json!({ "content": content, "isError": is_error })
}

/// MCP App user interfaces (`ui://` URIs or the `mcp-app` profile), which only hosts that render them can use.
pub fn is_mcp_app_resource(item: &Value) -> bool {
    let uri = item
        .get("uri")
        .or_else(|| item.get("uriTemplate"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if uri.starts_with("ui://") {
        return true;
    }
    let mime = item
        .get("mimeType")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_lowercase();
    mime.split(';').skip(1).any(|param| {
        let Some((key, value)) = param.split_once('=') else {
            return false;
        };
        key.trim() == "profile" && {
            let v = value.trim_start();
            let v = v.strip_prefix('"').unwrap_or(v);
            v.starts_with("mcp-app")
        }
    })
}

/// A listed resource or template without `_meta` and icons, tagged with its server.
pub fn listed(server: &str, item: &Value) -> Value {
    let mut out = Map::new();
    out.insert("server".into(), Value::String(server.into()));
    if let Some(fields) = item.as_object() {
        for (key, value) in fields {
            if key != "_meta" && key != "icons" {
                out.insert(key.clone(), value.clone());
            }
        }
    }
    Value::Object(out)
}
