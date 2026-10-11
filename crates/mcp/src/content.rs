//! Tool results as model content (`protocol/content.ts`): text and base64 images pass through, other blocks
//! become short text placeholders.

use serde_json::{Value, json};

fn text(text: impl Into<String>) -> Value {
    json!({ "type": "text", "text": text.into() })
}

fn image(data: &Value, mime_type: &Value) -> Value {
    json!({ "type": "image", "data": data, "mimeType": mime_type })
}

fn str_of<'a>(block: &'a Value, key: &str) -> &'a str {
    block.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn block_to_llm_content(block: &Value) -> Value {
    match str_of(block, "type") {
        "text" => text(str_of(block, "text")),
        "image" => image(&block["data"], &block["mimeType"]),
        "audio" => text(format!("[audio {} omitted]", str_of(block, "mimeType"))),
        "resource_link" => text(format!("{}: {}", str_of(block, "name"), str_of(block, "uri"))),
        "resource" => {
            let resource = &block["resource"];
            if let Some(body) = resource.get("text") {
                return text(body.as_str().unwrap_or_default());
            }
            match resource.get("mimeType").and_then(Value::as_str) {
                Some(mime) if mime.starts_with("image/") => image(&resource["blob"], &resource["mimeType"]),
                mime => text(format!(
                    "[binary resource {} ({}) omitted]",
                    str_of(resource, "uri"),
                    mime.unwrap_or("unknown type")
                )),
            }
        }
        other => text(format!("[unsupported MCP content {other}]")),
    }
}

/// `JSON.stringify(value, null, 2)`.
fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

/// Converts a `tools/call` result to model content; a result with no blocks but `structuredContent` becomes its JSON.
pub fn to_llm_content(result: &Value) -> Vec<Value> {
    let mut content: Vec<Value> = result
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(block_to_llm_content)
        .collect();
    if content.is_empty()
        && let Some(structured) = result.get("structuredContent")
    {
        content.push(text(pretty(structured)));
    }
    content
}
