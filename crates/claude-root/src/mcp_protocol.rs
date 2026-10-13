//! A minimal Model Context Protocol server over stdio (mcp-protocol.ts): newline-delimited JSON-RPC 2.0. It serves
//! tools, prompts and resources and honours `notifications/cancelled`. Tool calls run concurrently (each on its own
//! thread, so their responses may come back in any order); every other request is answered in arrival order.

use ai::AbortSignal;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::io::{BufRead, Read, Write};
use std::sync::{Arc, Mutex};

pub const MCP_PROTOCOL_VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;
/// Lines longer than this are rejected.
pub const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

pub struct McpPrompt {
    pub name: String,
    pub title: Option<String>,
    pub description: String,
    pub text: String,
}

pub struct McpResource {
    pub uri: String,
    pub name: String,
    pub title: Option<String>,
    pub description: String,
    pub mime_type: String,
    pub text: String,
}

/// What a server serves. `prompts`/`resources` returning `None` leave those capabilities out.
pub trait McpHandlers: Send + Sync {
    fn name(&self) -> String;
    fn version(&self) -> String;
    /// Server instructions (Claude Code shows them in its system prompt).
    fn instructions(&self) -> Option<String> {
        None
    }
    /// Tool declarations (`name`, `title`, `description`, `inputSchema`).
    fn tools(&self) -> Vec<Value>;
    /// Run one tool call; the result is `{content, isError?}`, an error becomes a JSON-RPC internal error.
    /// `meta` is the request's `_meta` (Claude Code puts the tool-use id in `claudecode/toolUseId`).
    fn call_tool(
        &self,
        name: &str,
        args: Map<String, Value>,
        signal: &AbortSignal,
        meta: Map<String, Value>,
    ) -> Result<Value, String>;
    fn prompts(&self) -> Option<Vec<McpPrompt>> {
        None
    }
    fn resources(&self) -> Option<Vec<McpResource>> {
        None
    }
    /// The client said hello (after `initialize`).
    fn on_initialized(&self, _client: &Map<String, Value>) {}
}

type Out = Arc<Mutex<Box<dyn Write + Send>>>;

fn send(out: &Out, mut message: Map<String, Value>) {
    let mut line = Map::new();
    line.insert("jsonrpc".into(), "2.0".into());
    line.append(&mut message);
    let mut out = out.lock().unwrap_or_else(|p| p.into_inner());
    let _ = out.write_all(format!("{}\n", session_file::js::stringify(&Value::Object(line))).as_bytes());
    let _ = out.flush();
}

fn reply(out: &Out, id: Value, result: Value) {
    let mut message = Map::new();
    message.insert("id".into(), id);
    message.insert("result".into(), result);
    send(out, message);
}

fn fail(out: &Out, id: Value, code: i64, text: &str) {
    let mut message = Map::new();
    message.insert("id".into(), id);
    message.insert("error".into(), json!({"code": code, "message": text}));
    send(out, message);
}

fn record(value: Option<&Value>) -> Map<String, Value> {
    match value {
        Some(Value::Object(map)) => map.clone(),
        _ => Map::new(),
    }
}

/// `String(value)` of JavaScript for the values JSON can carry (`undefined` when absent).
pub fn js_string(value: Option<&Value>) -> String {
    match value {
        None => "undefined".into(),
        Some(Value::Null) => "null".into(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => session_file::js::number_to_string(n),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| match item {
                Value::Null => String::new(),
                other => js_string(Some(other)),
            })
            .collect::<Vec<_>>()
            .join(","),
        Some(Value::Object(_)) => "[object Object]".into(),
    }
}

struct Server<H: McpHandlers + 'static> {
    handlers: Arc<H>,
    out: Out,
    running: Arc<Mutex<HashMap<String, AbortSignal>>>,
    client: Mutex<Map<String, Value>>,
}

impl<H: McpHandlers + 'static> Server<H> {
    /// A synchronous request's result, or its JSON-RPC error.
    fn request(&self, method: &str, params: &Map<String, Value>) -> Result<Value, (i64, String)> {
        let handlers = &self.handlers;
        match method {
            "initialize" => {
                let requested = params.get("protocolVersion").and_then(Value::as_str);
                let protocol = requested
                    .filter(|v| MCP_PROTOCOL_VERSIONS.contains(v))
                    .unwrap_or(MCP_PROTOCOL_VERSIONS[0]);
                let info = record(params.get("clientInfo"));
                let mut client = Map::new();
                for key in ["name", "version"] {
                    if let Some(Value::String(text)) = info.get(key) {
                        client.insert(key.into(), text.clone().into());
                    }
                }
                *self.client.lock().unwrap_or_else(|p| p.into_inner()) = client;
                let mut capabilities = Map::new();
                capabilities.insert("tools".into(), json!({"listChanged": false}));
                if handlers.prompts().is_some() {
                    capabilities.insert("prompts".into(), json!({"listChanged": false}));
                }
                if handlers.resources().is_some() {
                    capabilities.insert("resources".into(), json!({"listChanged": false, "subscribe": false}));
                }
                let mut result = Map::new();
                result.insert("protocolVersion".into(), protocol.into());
                result.insert("capabilities".into(), Value::Object(capabilities));
                result.insert(
                    "serverInfo".into(),
                    json!({"name": handlers.name(), "version": handlers.version()}),
                );
                if let Some(text) = handlers.instructions().filter(|t| !t.is_empty()) {
                    result.insert("instructions".into(), text.into());
                }
                Ok(Value::Object(result))
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": handlers.tools()})),
            "prompts/list" => Ok(
                json!({"prompts": handlers.prompts().unwrap_or_default().iter().map(|p| {
                let mut item = Map::new();
                item.insert("name".into(), p.name.clone().into());
                if let Some(title) = &p.title { item.insert("title".into(), title.clone().into()); }
                item.insert("description".into(), p.description.clone().into());
                item.insert("arguments".into(), json!([]));
                Value::Object(item)
            }).collect::<Vec<_>>()}),
            ),
            "prompts/get" => {
                let prompts = handlers.prompts().unwrap_or_default();
                let prompt = prompts
                    .iter()
                    .find(|p| params.get("name") == Some(&Value::String(p.name.clone())))
                    .ok_or_else(|| {
                        (
                            INVALID_PARAMS,
                            format!("Unknown prompt: {}", js_string(params.get("name"))),
                        )
                    })?;
                Ok(json!({
                    "description": prompt.description,
                    "messages": [{"role": "user", "content": {"type": "text", "text": prompt.text}}],
                }))
            }
            "resources/list" => Ok(
                json!({"resources": handlers.resources().unwrap_or_default().iter().map(|r| {
                let mut item = Map::new();
                item.insert("uri".into(), r.uri.clone().into());
                item.insert("name".into(), r.name.clone().into());
                if let Some(title) = &r.title { item.insert("title".into(), title.clone().into()); }
                item.insert("description".into(), r.description.clone().into());
                item.insert("mimeType".into(), r.mime_type.clone().into());
                Value::Object(item)
            }).collect::<Vec<_>>()}),
            ),
            "resources/templates/list" => Ok(json!({"resourceTemplates": []})),
            "resources/read" => {
                let resources = handlers.resources().unwrap_or_default();
                let resource = resources
                    .iter()
                    .find(|r| params.get("uri") == Some(&Value::String(r.uri.clone())))
                    .ok_or_else(|| {
                        (
                            INVALID_PARAMS,
                            format!("Unknown resource: {}", js_string(params.get("uri"))),
                        )
                    })?;
                Ok(json!({"contents": [{"uri": resource.uri, "mimeType": resource.mime_type, "text": resource.text}]}))
            }
            other => Err((METHOD_NOT_FOUND, format!("Method not found: {other}"))),
        }
    }

    fn on_message(self: &Arc<Self>, message: Map<String, Value>) {
        let id = message.get("id").cloned().filter(|id| !id.is_null());
        let Some(Value::String(method)) = message.get("method") else {
            // A response to a request we never send, or garbage: answer only what carries an id.
            if let Some(id) = id
                && !message.contains_key("result")
                && !message.contains_key("error")
            {
                fail(&self.out, id, INVALID_REQUEST, "Invalid request");
            }
            return;
        };
        let params = record(message.get("params"));
        let Some(id) = id else {
            if method == "notifications/cancelled" {
                if let Some(request_id) = params.get("requestId").filter(|v| v.is_string() || v.is_number())
                    && let Some(signal) = self
                        .running
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .get(&request_id.to_string())
                {
                    signal.abort();
                }
            } else if method == "notifications/initialized" {
                let client = self.client.lock().unwrap_or_else(|p| p.into_inner()).clone();
                self.handlers.on_initialized(&client);
            }
            return;
        };
        if method != "tools/call" {
            match self.request(method, &params) {
                Ok(result) => reply(&self.out, id, result),
                Err((code, text)) => fail(&self.out, id, code, &text),
            }
            return;
        }
        let Some(Value::String(name)) = params.get("name") else {
            fail(&self.out, id, INVALID_PARAMS, "tools/call needs a tool name");
            return;
        };
        let name = name.clone();
        if !self.handlers.tools().iter().any(|t| t["name"] == name.as_str()) {
            fail(&self.out, id, INVALID_PARAMS, &format!("Unknown tool: {name}"));
            return;
        }
        let signal = AbortSignal::new();
        let key = id.to_string();
        self.running
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(key.clone(), signal.clone());
        let server = self.clone();
        std::thread::spawn(move || {
            let args = record(params.get("arguments"));
            let meta = record(params.get("_meta"));
            let handlers = server.handlers.clone();
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                handlers.call_tool(&name, args, &signal, meta)
            }));
            server.running.lock().unwrap_or_else(|p| p.into_inner()).remove(&key);
            match outcome {
                Ok(Ok(result)) => reply(&server.out, id, result),
                Ok(Err(message)) => fail(&server.out, id, INTERNAL_ERROR, &message),
                Err(_) => fail(&server.out, id, INTERNAL_ERROR, "tool call failed"),
            }
        });
    }
}

/// Serve MCP on `input`/`output` until `input` ends; then running tool calls are aborted.
pub fn serve<H: McpHandlers + 'static>(handlers: Arc<H>, input: &mut dyn BufRead, output: Box<dyn Write + Send>) {
    let server = Arc::new(Server {
        handlers,
        out: Arc::new(Mutex::new(output)),
        running: Arc::default(),
        client: Mutex::new(Map::new()),
    });
    loop {
        let mut line = Vec::new();
        let read = (&mut *input)
            .take(MAX_LINE_BYTES as u64 + 1)
            .read_until(b'\n', &mut line);
        match read {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if line.len() > MAX_LINE_BYTES && line.last() != Some(&b'\n') {
            fail(&server.out, Value::Null, INVALID_REQUEST, "Message too large");
            // Drop the rest of the oversized line.
            let mut rest = Vec::new();
            if input.read_until(b'\n', &mut rest).unwrap_or(0) == 0 {
                break;
            }
            continue;
        }
        let text = String::from_utf8_lossy(&line);
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        let message: Value = match serde_json::from_str(text) {
            Ok(message) => message,
            Err(_) => {
                fail(&server.out, Value::Null, PARSE_ERROR, "Parse error");
                continue;
            }
        };
        // Batches are not part of current MCP; treat each element as a message.
        let items = match message {
            Value::Array(items) => items,
            other => vec![other],
        };
        for item in items {
            server.on_message(record(Some(&item)));
        }
    }
    for signal in server.running.lock().unwrap_or_else(|p| p.into_inner()).values() {
        signal.abort();
    }
}
