//! The fake Hindsight server (fixtures/fake_hindsight.py, the one the TypeScript generator recorded against) and
//! the fixture's normalization.
#![allow(dead_code)]

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};

pub struct FakeHindsight {
    child: Child,
    pub port: u16,
}

impl FakeHindsight {
    pub fn start() -> FakeHindsight {
        let python = std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into());
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/fake_hindsight.py");
        let mut child = Command::new(python)
            .arg("-I")
            .arg(script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start the fake Hindsight server");
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let port = line.trim().parse().expect("the fake server prints its port");
        FakeHindsight { child, port }
    }

    pub fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn control(&self, method: &str, path: &str) -> Value {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nhost: fake\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
        )
        .unwrap();
        let mut text = String::new();
        stream.read_to_string(&mut text).unwrap();
        let body = text.split_once("\r\n\r\n").unwrap().1;
        serde_json::from_str(body).unwrap()
    }

    pub fn reset(&self) {
        self.control("POST", "/__reset");
    }

    /// Every request since the last reset: request line, lower-cased headers other than `transport` sorted by
    /// name, and the body.
    pub fn requests(&self, transport: &[String]) -> Value {
        let log = self.control("GET", "/__log");
        Value::Array(
            log.as_array()
                .unwrap()
                .iter()
                .map(|entry| {
                    let mut headers: Vec<(String, String)> = entry["headers"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|pair| {
                            (
                                pair[0].as_str().unwrap().to_lowercase(),
                                pair[1].as_str().unwrap().to_string(),
                            )
                        })
                        .filter(|(name, _)| !transport.contains(name))
                        .collect();
                    headers.sort_by(|a, b| a.0.cmp(&b.0));
                    json!({"line": entry["line"], "headers": headers, "body": entry["body"]})
                })
                .collect(),
        )
    }

    /// Every request since the last reset, with all headers, as received.
    pub fn raw_requests(&self) -> Value {
        self.control("GET", "/__log")
    }
}

impl Drop for FakeHindsight {
    fn drop(&mut self) {
        drop(self.child.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A port nothing listens on.
pub fn closed_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

pub fn fixture() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/exchanges.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// `{base}` / `{closed}` in strings → this run's addresses.
pub fn fill(value: &Value, base: &str, closed: u16) -> Value {
    let text = serde_json::to_string(value)
        .unwrap()
        .replace("{base}", base)
        .replace("{closed}", &format!("http://127.0.0.1:{closed}"));
    serde_json::from_str(&text).unwrap()
}

/// The generator's normalization: ports, uuids numbered by first appearance, ISO times.
pub struct Normalizer {
    port: u16,
    closed: u16,
}

impl Normalizer {
    pub fn new(port: u16, closed: u16) -> Normalizer {
        Normalizer { port, closed }
    }

    pub fn apply(&self, value: &Value) -> Value {
        let text = serde_json::to_string(value)
            .unwrap()
            .replace(&format!("127.0.0.1:{}", self.port), "127.0.0.1:<port>")
            .replace(&format!("127.0.0.1:{}", self.closed), "127.0.0.1:<closed>");
        let uuid = regex::Regex::new(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}").unwrap();
        let mut ids: Vec<String> = Vec::new();
        let text = uuid.replace_all(&text, |found: &regex::Captures<'_>| {
            let id = found[0].to_string();
            let at = ids.iter().position(|seen| *seen == id).unwrap_or_else(|| {
                ids.push(id);
                ids.len() - 1
            });
            format!("<uuid-{}>", at + 1)
        });
        let time = regex::Regex::new(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z").unwrap();
        serde_json::from_str(&time.replace_all(&text, "<time>")).unwrap()
    }
}

pub fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect()
}

/// A memory error's code, else the message (the generator records `error.code ?? error.message`).
pub fn error_text(text: &str) -> String {
    text.strip_prefix("Memory operation did not complete (")
        .and_then(|rest| rest.strip_suffix(')'))
        .unwrap_or(text)
        .to_string()
}
