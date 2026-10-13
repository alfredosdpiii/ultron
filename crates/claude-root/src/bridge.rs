//! The MCP bridge between a `claude -p` lane process and the session worker that runs the lane (tool-bridge.ts).
//! The worker listens on an owner-only Unix socket ([`start_bridge_server`]); Claude Code starts `ultron mcp
//! --bridge <socket> --token <token>` ([`run_mcp_bridge`]) as its only MCP server, which lists the lane's tools and
//! forwards each `tools/call` (with Claude Code's tool-use id) to the worker.
//!
//! Protocol: the control socket's newline-delimited JSON, ops `tools` and `call`, each naming the lane by token.

use crate::control::ControlClient;
use crate::mcp_protocol::{McpHandlers, serve};
use ai::AbortSignal;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

const MAX_LINE: usize = 64 * 1024 * 1024;
/// How long the bridge waits for one call's result (the CLI's own MCP timeout is two hours).
const CALL_TIMEOUT: Duration = Duration::from_secs(3 * 60 * 60);

/// One lane process's side of the bridge: the worker routes MCP calls here by token.
pub trait LaneBridge: Send + Sync {
    fn tools(&self) -> Vec<Value>;
    /// `{content, isError}`; `signal` aborts when the bridge connection goes away.
    fn call(
        &self,
        tool_use_id: Option<&str>,
        name: &str,
        args: Map<String, Value>,
        signal: &AbortSignal,
    ) -> Result<Value, String>;
}

type Lanes = Arc<Mutex<HashMap<String, Arc<dyn LaneBridge>>>>;

/// The worker's listener for the bridges of its lane processes.
pub struct BridgeServer {
    pub socket: PathBuf,
    lanes: Lanes,
}

impl BridgeServer {
    /// Route bridge calls for `token` to `lane` until [`BridgeServer::unregister`].
    pub fn register(&self, token: &str, lane: Arc<dyn LaneBridge>) {
        self.lanes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(token.into(), lane);
    }

    pub fn unregister(&self, token: &str) {
        self.lanes.lock().unwrap_or_else(PoisonError::into_inner).remove(token);
    }

    pub fn close(&self) {
        let _ = std::fs::remove_file(&self.socket);
        let _ = UnixStream::connect(&self.socket);
    }
}

impl Drop for BridgeServer {
    fn drop(&mut self) {
        self.close();
    }
}

fn handle(lanes: &Lanes, request: &Map<String, Value>, signal: &AbortSignal) -> Result<Value, String> {
    let lane = request
        .get("token")
        .and_then(Value::as_str)
        .and_then(|token| lanes.lock().unwrap_or_else(PoisonError::into_inner).get(token).cloned())
        .ok_or("unknown or finished Claude Code lane")?;
    match request.get("op").and_then(Value::as_str) {
        Some("tools") => Ok(Value::Array(lane.tools())),
        Some("call") => {
            let args = match request.get("args") {
                Some(Value::Object(args)) => args.clone(),
                _ => Map::new(),
            };
            let name = match request.get("name") {
                None | Some(Value::Null) => String::new(),
                other => crate::mcp_protocol::js_string(other),
            };
            lane.call(request.get("toolUseId").and_then(Value::as_str), &name, args, signal)
        }
        _ => Err(format!(
            "unknown bridge op: {}",
            crate::mcp_protocol::js_string(request.get("op"))
        )),
    }
}

/// Listen in `dir` (`w-<pid>-<hex>.sock`, owner-only).
pub fn start_bridge_server(dir: &Path) -> Result<BridgeServer, String> {
    use std::os::unix::fs::PermissionsExt;
    let socket = dir.join(format!("w-{}-{}.sock", std::process::id(), crate::random_hex(4)));
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).map_err(|e| format!("listen {}: {e}", socket.display()))?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    let lanes: Lanes = Arc::default();
    let served = lanes.clone();
    let path = socket.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            if !path.exists() {
                break;
            }
            let Ok(stream) = stream else { continue };
            let lanes = served.clone();
            std::thread::spawn(move || serve_bridge_connection(stream, lanes));
        }
    });
    Ok(BridgeServer { socket, lanes })
}

fn serve_bridge_connection(stream: UnixStream, lanes: Lanes) {
    let Ok(writer) = stream.try_clone() else { return };
    let writer = Arc::new(Mutex::new(writer));
    // A bridge that goes away (Claude Code exited) cancels its open calls.
    let closed = AbortSignal::new();
    let mut reader = BufReader::new(stream);
    loop {
        let mut line = Vec::new();
        match (&mut reader).take(MAX_LINE as u64 + 1).read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) if line.len() > MAX_LINE => break,
            Ok(_) => {}
        }
        let Ok(Value::Object(request)) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        let (lanes, writer, closed) = (lanes.clone(), writer.clone(), closed.clone());
        std::thread::spawn(move || {
            let id = request.get("id").cloned().unwrap_or(Value::Null);
            let response = match handle(&lanes, &request, &closed) {
                Ok(result) => json!({"id": id, "ok": true, "result": result}),
                Err(error) => json!({"id": id, "ok": false, "error": error}),
            };
            let mut writer = writer.lock().unwrap_or_else(PoisonError::into_inner);
            let _ = writer.write_all(format!("{response}\n").as_bytes());
        });
    }
    closed.abort();
}

struct Bridge {
    socket: PathBuf,
    token: String,
    tools: Vec<Value>,
}

impl McpHandlers for Bridge {
    fn name(&self) -> String {
        "ultron".into()
    }
    fn version(&self) -> String {
        crate::cli::version()
    }
    fn tools(&self) -> Vec<Value> {
        self.tools.clone()
    }
    fn call_tool(
        &self,
        name: &str,
        args: Map<String, Value>,
        _signal: &AbortSignal,
        meta: Map<String, Value>,
    ) -> Result<Value, String> {
        let mut fields = Map::new();
        fields.insert("token".into(), self.token.clone().into());
        fields.insert("name".into(), name.into());
        fields.insert("args".into(), Value::Object(args));
        if let Some(Value::String(id)) = meta.get("claudecode/toolUseId") {
            fields.insert("toolUseId".into(), id.clone().into());
        }
        // One connection per call, so parallel calls do not wait on each other.
        let result = ControlClient::connect(&self.socket, Duration::ZERO)
            .and_then(|mut client| client.request("call", fields, CALL_TIMEOUT));
        Ok(match result {
            Ok(result) => {
                let mut reply = Map::new();
                if let Some(content) = result.get("content") {
                    reply.insert("content".into(), content.clone());
                }
                if result.get("isError") == Some(&Value::Bool(true)) {
                    reply.insert("isError".into(), true.into());
                }
                Value::Object(reply)
            }
            Err(error) => json!({"content": [{"type": "text", "text": error}], "isError": true}),
        })
    }
}

/// `ultron mcp --bridge <socket> --token <token>`: serve one lane's tools to Claude Code over stdio.
pub fn run_mcp_bridge(
    socket: &Path,
    token: &str,
    stdin: &mut dyn std::io::BufRead,
    stdout: Box<dyn Write + Send>,
) -> Result<(), String> {
    let mut control = ControlClient::connect(socket, Duration::from_secs(5))?;
    let mut fields = Map::new();
    fields.insert("token".into(), token.into());
    let tools = control.request("tools", fields, Duration::from_secs(30))?;
    let bridge = Bridge {
        socket: socket.to_path_buf(),
        token: token.to_string(),
        tools: tools.as_array().cloned().unwrap_or_default(),
    };
    serve(Arc::new(bridge), stdin, stdout);
    Ok(())
}
