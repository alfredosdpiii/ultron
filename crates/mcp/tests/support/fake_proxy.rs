//! A fake HTTP proxy (and MCP server) on 127.0.0.1 for proxy tests: it records the request line of every request it
//! gets and answers an MCP streamable HTTP exchange itself, so a server URL under `.invalid` (which never resolves)
//! works only through it. `initialize` and `tools/list` get results, notifications 202, the GET stream 405,
//! `CONNECT` 403 and everything else (OAuth discovery, registration, tokens) 404. Every answer closes the connection.
//! [`FakeProxy::requiring_auth`] answers MCP requests without an `Authorization` header with 401.

#![allow(dead_code)]

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::time::Duration;

/// Only a hang guard: an event that is due arrives at once.
pub const HANG_GUARD: Duration = Duration::from_secs(120);

pub struct FakeProxy {
    pub port: u16,
    seen: mpsc::Receiver<String>,
}

impl FakeProxy {
    pub fn start() -> FakeProxy {
        FakeProxy::start_with(false)
    }

    pub fn requiring_auth() -> FakeProxy {
        FakeProxy::start_with(true)
    }

    fn start_with(auth_required: bool) -> FakeProxy {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, seen) = mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                let tx = tx.clone();
                std::thread::spawn(move || serve(stream, &tx, auth_required));
            }
        });
        FakeProxy { port, seen }
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// The next request line, waiting up to the hang guard.
    pub fn next(&self) -> Option<String> {
        self.seen.recv_timeout(HANG_GUARD).ok()
    }

    /// Request lines until `line` arrives (`None` if it never does); the lines before it are returned too.
    pub fn until(&self, line: &str) -> Result<Vec<String>, Vec<String>> {
        let mut lines = Vec::new();
        while let Some(next) = self.next() {
            let done = next == line;
            lines.push(next);
            if done {
                return Ok(lines);
            }
        }
        Err(lines)
    }

    /// The request lines that already arrived.
    pub fn drain(&self) -> Vec<String> {
        self.seen.try_iter().collect()
    }
}

fn respond(stream: &mut TcpStream, status: &str, content_type: &str, body: &str) {
    let head = format!(
        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.flush();
}

fn serve(mut stream: TcpStream, tx: &mpsc::Sender<String>, auth_required: bool) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let line = line.trim_end().to_string();
    let mut length = 0usize;
    let mut authorized = false;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).unwrap_or(0) == 0 || header == "\r\n" {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().unwrap_or(0);
        }
        authorized |= header.to_ascii_lowercase().starts_with("authorization:");
    }
    let mut body = vec![0; length];
    let _ = reader.read_exact(&mut body);
    let message: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let _ = tx.send(line.clone());
    let method = line.split(' ').next().unwrap_or_default();
    match method {
        "CONNECT" => respond(&mut stream, "403 Forbidden", "text/plain", ""),
        "POST" if line.contains("/mcp ") && auth_required && !authorized => {
            respond(&mut stream, "401 Unauthorized", "text/plain", "")
        }
        "POST" if line.contains("/mcp ") => {
            let result = match message["method"].as_str() {
                Some("initialize") => json!({
                    "protocolVersion": message["params"]["protocolVersion"],
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "proxied", "version": "1.0.0" },
                }),
                Some("tools/list") => json!({
                    "tools": [{ "name": "echo", "description": "Echo", "inputSchema": { "type": "object" } }],
                }),
                _ if message.get("id").is_none() => {
                    return respond(&mut stream, "202 Accepted", "text/plain", "");
                }
                _ => json!({}),
            };
            let reply = json!({ "jsonrpc": "2.0", "id": message["id"], "result": result });
            respond(&mut stream, "200 OK", "application/json", &reply.to_string());
        }
        "GET" if line.contains("/mcp ") => respond(&mut stream, "405 Method Not Allowed", "text/plain", ""),
        "DELETE" => respond(&mut stream, "200 OK", "text/plain", ""),
        _ => respond(&mut stream, "404 Not Found", "text/plain", "not found"),
    }
}
