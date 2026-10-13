//! The local control socket of one `ultron mcp` server (control-socket.ts): Claude Code hooks (`ultron hook`), the
//! viewer (`ultron watch`) and subagent servers reach the server's runtime through it. Newline-delimited JSON, one
//! request and one response per line:
//!
//! ```text
//! {"id": 1, "op": "status"}  ->  {"id": 1, "ok": true, "result": {...}}
//! ```
//!
//! Sockets live in a per-user runtime directory (`$XDG_RUNTIME_DIR/ultron-claude`, else `ultron-claude-<uid>` in the
//! temp dir), 0700, the socket itself 0600. Each server also writes `<name>.json` there (pid, Claude Code session id,
//! cwd, socket), which is how hooks and the viewer find a server they were not told about.

use serde_json::{Map, Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Lines longer than this are refused.
pub const MAX_CONTROL_LINE: usize = 4 * 1024 * 1024;

/// The per-user directory for control sockets and registry files (created 0700, owned by this user).
pub fn control_dir(var: &dyn Fn(&str) -> Option<String>) -> Result<PathBuf, String> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    let uid = rustix::process::getuid().as_raw();
    let dir = match var("XDG_RUNTIME_DIR")
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    {
        Some(runtime) => Path::new(&runtime).join("ultron-claude"),
        None => crate::tmpdir(var).join(format!("ultron-claude-{uid}")),
    };
    let _ = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir);
    let meta = std::fs::symlink_metadata(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if !meta.is_dir() || meta.uid() != uid {
        return Err(format!("{} is not a directory owned by this user", dir.display()));
    }
    if meta.mode() & 0o077 != 0 {
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
    }
    Ok(dir)
}

/// A registry name safe as a file name.
pub fn safe_name(name: &str) -> String {
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .take(120)
        .collect();
    if safe.is_empty() { "session".into() } else { safe }
}

/// The socket path for a registry name.
pub fn socket_path_for(name: &str, dir: &Path) -> PathBuf {
    dir.join(format!("{}.sock", safe_name(name)))
}

/// One server's registry record.
#[derive(Clone, Debug, PartialEq)]
pub struct ServerRecord {
    /// The Claude Code session id for a root, `child-<pid>-<random>` for a subagent server.
    pub name: String,
    pub pid: u32,
    pub socket: String,
    pub cwd: String,
    pub started_at: i64,
    pub claude_session_id: Option<String>,
    pub claude_pid: Option<u64>,
    pub session_id: Option<String>,
    /// Set for a subagent server: the parent server's registry name.
    pub parent: Option<String>,
}

impl ServerRecord {
    pub fn to_json(&self) -> Value {
        let mut record = Map::new();
        record.insert("name".into(), self.name.clone().into());
        record.insert("pid".into(), self.pid.into());
        record.insert("socket".into(), self.socket.clone().into());
        record.insert("cwd".into(), self.cwd.clone().into());
        record.insert("startedAt".into(), self.started_at.into());
        for (key, value) in [
            ("claudeSessionId", self.claude_session_id.clone().map(Value::from)),
            ("claudePid", self.claude_pid.map(Value::from)),
            ("sessionId", self.session_id.clone().map(Value::from)),
            ("parent", self.parent.clone().map(Value::from)),
        ] {
            if let Some(value) = value {
                record.insert(key.into(), value);
            }
        }
        Value::Object(record)
    }

    pub fn from_json(value: &Value) -> Option<ServerRecord> {
        let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
        Some(ServerRecord {
            name: text("name").unwrap_or_default(),
            pid: u32::try_from(value.get("pid")?.as_f64()? as i64).ok()?,
            socket: text("socket")?,
            cwd: text("cwd").unwrap_or_default(),
            started_at: value.get("startedAt").and_then(Value::as_f64).unwrap_or(0.0) as i64,
            claude_session_id: text("claudeSessionId"),
            claude_pid: value.get("claudePid").and_then(Value::as_u64),
            session_id: text("sessionId"),
            parent: text("parent"),
        })
    }
}

/// `process.kill(pid, 0)` succeeds or fails with EPERM.
pub fn pid_alive(pid: u32) -> bool {
    let Some(pid) = i32::try_from(pid).ok().and_then(rustix::process::Pid::from_raw) else {
        return false;
    };
    match rustix::process::test_kill_process(pid) {
        Ok(()) => true,
        Err(e) => e == rustix::io::Errno::PERM,
    }
}

/// Live servers in the registry, newest first; records of dead processes are removed.
pub fn list_servers(dir: &Path) -> Vec<ServerRecord> {
    let mut records = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return records;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !entry.file_name().to_string_lossy().ends_with(".json") {
            continue;
        }
        let Some(record) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|value| ServerRecord::from_json(&value))
        else {
            continue;
        };
        if !pid_alive(record.pid) {
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(&record.socket);
            continue;
        }
        records.push(record);
    }
    records.sort_by_key(|r| std::cmp::Reverse(r.started_at));
    records
}

/// What [`find_server`] looks for.
#[derive(Clone, Debug, Default)]
pub struct ServerQuery {
    pub claude_session_id: Option<String>,
    pub cwd: Option<String>,
    pub name: Option<String>,
}

/// The server of a Claude Code session, or the newest root server in `cwd`.
pub fn find_server(query: &ServerQuery, dir: &Path) -> Option<ServerRecord> {
    let servers = list_servers(dir);
    if let Some(name) = &query.name {
        return servers.into_iter().find(|s| &s.name == name);
    }
    if let Some(id) = &query.claude_session_id
        && let Some(found) = servers
            .iter()
            .find(|s| s.claude_session_id.as_ref() == Some(id) && s.parent.is_none())
    {
        return Some(found.clone());
    }
    let cwd = query.cwd.as_ref()?;
    servers.into_iter().find(|s| &s.cwd == cwd && s.parent.is_none())
}

/// Answers one request (`op` and its fields); `Err` becomes `{"ok": false, "error": ...}`.
pub type ControlHandler = Arc<dyn Fn(Map<String, Value>) -> Result<Value, String> + Send + Sync>;

/// A listening control server; [`ControlServer::close`] (or drop) stops it and unregisters.
pub struct ControlServer {
    pub socket: PathBuf,
    registry: Option<PathBuf>,
    closed: Arc<AtomicBool>,
    connections: Arc<Mutex<Vec<UnixStream>>>,
}

impl ControlServer {
    pub fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        for stream in self.connections.lock().unwrap_or_else(|p| p.into_inner()).drain(..) {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
        if let Some(registry) = &self.registry {
            let _ = std::fs::remove_file(registry);
        }
        let _ = std::fs::remove_file(&self.socket);
        // Wake the accept loop so it sees `closed`.
        let _ = UnixStream::connect(&self.socket);
    }
}

impl Drop for ControlServer {
    fn drop(&mut self) {
        self.close();
    }
}

fn write_line(stream: &Mutex<UnixStream>, value: &Value) {
    let mut stream = stream.lock().unwrap_or_else(|p| p.into_inner());
    let _ = stream.write_all(format!("{value}\n").as_bytes());
}

/// Serve newline-delimited JSON requests on `socket` (owner-only); each request is answered on its own thread.
pub fn listen(socket: &Path, handle: ControlHandler) -> Result<ControlServer, String> {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::remove_file(socket);
    let listener = UnixListener::bind(socket).map_err(|e| format!("listen {}: {e}", socket.display()))?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    let closed = Arc::new(AtomicBool::new(false));
    let connections: Arc<Mutex<Vec<UnixStream>>> = Arc::default();
    let (stop, conns) = (closed.clone(), connections.clone());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            let Ok(stream) = stream else { continue };
            if let Ok(clone) = stream.try_clone() {
                conns.lock().unwrap_or_else(|p| p.into_inner()).push(clone);
            }
            let handle = handle.clone();
            std::thread::spawn(move || serve_connection(stream, handle));
        }
    });
    Ok(ControlServer {
        socket: socket.to_path_buf(),
        registry: None,
        closed,
        connections,
    })
}

fn serve_connection(stream: UnixStream, handle: ControlHandler) {
    let Ok(writer) = stream.try_clone() else { return };
    let writer = Arc::new(Mutex::new(writer));
    let mut reader = BufReader::new(stream);
    loop {
        let mut line = Vec::new();
        match reader
            .by_ref()
            .take(MAX_CONTROL_LINE as u64 + 1)
            .read_until(b'\n', &mut line)
        {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        if line.len() > MAX_CONTROL_LINE {
            let _ = writer
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .shutdown(std::net::Shutdown::Both);
            return;
        }
        let text = String::from_utf8_lossy(&line);
        if text.trim().is_empty() {
            continue;
        }
        let request = match serde_json::from_str::<Value>(text.trim_end_matches('\n')) {
            Ok(Value::Object(request)) => request,
            Ok(_) | Err(_) => {
                write_line(&writer, &json!({"id": null, "ok": false, "error": "invalid JSON"}));
                continue;
            }
        };
        let (handle, writer) = (handle.clone(), writer.clone());
        std::thread::spawn(move || {
            let id = request.get("id").cloned().unwrap_or(Value::Null);
            let response = match handle(request) {
                Ok(result) => json!({"id": id, "ok": true, "result": result}),
                Err(error) => json!({"id": id, "ok": false, "error": error}),
            };
            write_line(&writer, &response);
        });
    }
}

/// Listen on `record.socket` and register the server in `dir`.
pub fn start_control_server(
    record: &ServerRecord,
    handle: ControlHandler,
    dir: &Path,
) -> Result<ControlServer, String> {
    let mut server = listen(Path::new(&record.socket), handle)?;
    let registry = dir.join(format!("{}.json", safe_name(&record.name)));
    crate::write_private(&registry, &format!("{}\n", record.to_json())).map_err(|e| e.to_string())?;
    server.registry = Some(registry);
    Ok(server)
}

/// A connection to a control socket.
pub struct ControlClient {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
    next: u64,
}

impl ControlClient {
    /// Connect, retrying while the socket does not accept yet (up to `wait`).
    pub fn connect(socket: &Path, wait: Duration) -> Result<ControlClient, String> {
        let deadline = Instant::now() + wait;
        loop {
            match UnixStream::connect(socket) {
                Ok(stream) => {
                    let reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
                    return Ok(ControlClient {
                        stream,
                        reader,
                        next: 1,
                    });
                }
                Err(e) if Instant::now() >= deadline => return Err(format!("connect {}: {e}", socket.display())),
                Err(_) => std::thread::sleep(Duration::from_millis(100)),
            }
        }
    }

    /// Send `op` with `fields` and wait (up to `timeout`) for its response.
    pub fn request(&mut self, op: &str, fields: Map<String, Value>, timeout: Duration) -> Result<Value, String> {
        let id = self.next;
        self.next += 1;
        let mut request = fields;
        request.insert("id".into(), id.into());
        request.insert("op".into(), op.into());
        self.stream
            .write_all(format!("{}\n", Value::Object(request)).as_bytes())
            .map_err(|e| e.to_string())?;
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(format!("control request {op} timed out"));
            }
            self.stream.set_read_timeout(Some(left)).map_err(|e| e.to_string())?;
            let mut line = String::new();
            match self.reader.read_line(&mut line) {
                Ok(0) => return Err("control socket closed".into()),
                Ok(_) => {}
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                    return Err(format!("control request {op} timed out"));
                }
                Err(e) => return Err(e.to_string()),
            }
            let Ok(response) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if response.get("id") != Some(&Value::from(id)) {
                continue;
            }
            return if response["ok"] == true {
                Ok(response.get("result").cloned().unwrap_or(Value::Null))
            } else {
                Err(response["error"]
                    .as_str()
                    .unwrap_or("control request failed")
                    .to_string())
            };
        }
    }
}
