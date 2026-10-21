//! `ultron acp` driven as an editor drives it: the binary as a subprocess speaking ACP JSON-RPC over stdio, a scripted
//! Anthropic provider on 127.0.0.1, private agent/server/home dirs. Every message the agent writes is checked against
//! the ACP v1 JSON schema (`fixtures/acp/schema.json`, vendored from agentclientprotocol/agent-client-protocol
//! @2c8270462725d07d9d7cd845beb21dc99c9eae68 `schema/v1/schema.json`, Apache-2.0, `fixtures/acp/LICENSE`), and stdout
//! must carry nothing but JSON-RPC 2.0 lines.
//!
//! The client tests replay the exact message sequences CodeCompanion.nvim and avante.nvim send (recorded from their
//! sources in `fixtures/acp/codecompanion.json` and `fixtures/acp/avante.json`) and check the replies with those
//! clients' own acceptance rules, ported from their Lua.

mod common;

use common::{Naming, Sandbox, anthropic_text, anthropic_tool, mock_server, tool_result_text};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Hang guard for any one wait.
const HANG: Duration = Duration::from_secs(120);

// ---------------------------------------------------------------------------------------------------------------
// A JSON-schema checker for the subset the ACP schema uses: $ref, allOf, anyOf, oneOf, type, const, enum,
// properties, required, additionalProperties, items, minimum, maximum.

fn schema() -> &'static Value {
    static SCHEMA: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    SCHEMA.get_or_init(|| {
        let text =
            std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/acp/schema.json")).unwrap();
        serde_json::from_str(&text).unwrap()
    })
}

fn type_matches(kind: &str, value: &Value) -> bool {
    match kind {
        "null" => value.is_null(),
        "boolean" => value.is_boolean(),
        "string" => value.is_string(),
        "integer" => value.is_i64() || value.is_u64(),
        "number" => value.is_number(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => false,
    }
}

fn check(node: &Value, value: &Value, at: &str) -> Result<(), String> {
    let node = match node {
        Value::Bool(true) => return Ok(()),
        Value::Bool(false) => return Err(format!("{at}: nothing allowed")),
        Value::Object(map) => map,
        _ => return Ok(()),
    };
    if let Some(reference) = node.get("$ref").and_then(Value::as_str) {
        let name = reference.trim_start_matches("#/$defs/");
        check(&schema()["$defs"][name], value, &format!("{at}<{name}>"))?;
    }
    if let Some(all) = node.get("allOf").and_then(Value::as_array) {
        for part in all {
            check(part, value, at)?;
        }
    }
    if let Some(any) = node.get("anyOf").and_then(Value::as_array) {
        let errors: Vec<String> = any.iter().filter_map(|p| check(p, value, at).err()).collect();
        if errors.len() == any.len() {
            return Err(format!("{at}: no anyOf branch matches: {}", errors.join(" | ")));
        }
    }
    if let Some(one) = node.get("oneOf").and_then(Value::as_array) {
        let passing = one.iter().filter(|p| check(p, value, at).is_ok()).count();
        if passing != 1 {
            let errors: Vec<String> = one.iter().filter_map(|p| check(p, value, at).err()).collect();
            return Err(format!("{at}: {passing} oneOf branches match: {}", errors.join(" | ")));
        }
    }
    match node.get("type") {
        Some(Value::String(kind)) if !type_matches(kind, value) => {
            return Err(format!("{at}: expected {kind}, got {value}"));
        }
        Some(Value::Array(kinds)) if !kinds.iter().any(|k| type_matches(k.as_str().unwrap_or(""), value)) => {
            return Err(format!("{at}: expected one of {kinds:?}, got {value}"));
        }
        _ => {}
    }
    if let Some(expected) = node.get("const")
        && expected != value
    {
        return Err(format!("{at}: expected {expected}, got {value}"));
    }
    if let Some(options) = node.get("enum").and_then(Value::as_array)
        && !options.contains(value)
    {
        return Err(format!("{at}: {value} not in {options:?}"));
    }
    if let (Some(min), Some(n)) = (node.get("minimum").and_then(Value::as_f64), value.as_f64())
        && n < min
    {
        return Err(format!("{at}: {n} < {min}"));
    }
    if let (Some(max), Some(n)) = (node.get("maximum").and_then(Value::as_f64), value.as_f64())
        && n > max
    {
        return Err(format!("{at}: {n} > {max}"));
    }
    if let Some(object) = value.as_object() {
        let properties = node.get("properties").and_then(Value::as_object);
        for required in node.get("required").and_then(Value::as_array).into_iter().flatten() {
            let key = required.as_str().unwrap_or_default();
            if !object.contains_key(key) {
                return Err(format!("{at}: missing required {key}"));
            }
        }
        for (key, item) in object {
            match properties.and_then(|p| p.get(key)) {
                Some(property) => check(property, item, &format!("{at}.{key}"))?,
                None => match node.get("additionalProperties") {
                    Some(Value::Bool(false)) => return Err(format!("{at}: unexpected {key}")),
                    Some(extra @ Value::Object(_)) => check(extra, item, &format!("{at}.{key}"))?,
                    _ => {}
                },
            }
        }
    }
    if let (Some(items), Some(array)) = (node.get("items"), value.as_array()) {
        for (i, item) in array.iter().enumerate() {
            check(items, item, &format!("{at}[{i}]"))?;
        }
    }
    Ok(())
}

fn check_def(name: &str, value: &Value) -> Result<(), String> {
    check(&json!({"$ref": format!("#/$defs/{name}")}), value, "")
}

/// The response type of each agent method.
fn response_def(method: &str) -> &'static str {
    match method {
        "initialize" => "InitializeResponse",
        "authenticate" => "AuthenticateResponse",
        "session/new" => "NewSessionResponse",
        "session/load" => "LoadSessionResponse",
        "session/list" => "ListSessionsResponse",
        "session/prompt" => "PromptResponse",
        "session/set_config_option" => "SetSessionConfigOptionResponse",
        other => panic!("no response type for {other}"),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// The client.

struct Acp {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<Option<String>>,
    stderr: Arc<Mutex<String>>,
    /// Every message the agent wrote, in order.
    seen: Vec<Value>,
    /// Methods of the requests sent, by id (to check their responses).
    methods: HashMap<String, String>,
    next_id: u64,
    /// Waits look from here on.
    cursor: usize,
}

impl Acp {
    fn start(sandbox: &Sandbox, args: &[&str]) -> Acp {
        common::assert_private_home(&sandbox.root);
        let mut command = sandbox.ultron();
        command
            .arg("acp")
            .args(["--model", "anthropic/claude-sonnet-4-5"])
            .args(args)
            .env("ULTRON_HINDSIGHT_URL", "off")
            .env("PI_OFFLINE", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut err = child.stderr.take().unwrap();
        let stderr = Arc::new(Mutex::new(String::new()));
        let collected = stderr.clone();
        std::thread::spawn(move || {
            let mut text = String::new();
            let _ = err.read_to_string(&mut text);
            collected.lock().unwrap().push_str(&text);
        });
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(Some(line)).is_err() {
                    return;
                }
            }
            let _ = tx.send(None);
        });
        Acp {
            stdin: child.stdin.take(),
            child,
            lines,
            stderr,
            seen: Vec::new(),
            methods: HashMap::new(),
            next_id: 100,
            cursor: 0,
        }
    }

    fn stderr(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }

    /// Check one line the agent wrote: JSON-RPC 2.0, and the ACP type its method or request says.
    fn accept(&mut self, line: &str) -> Value {
        let message: Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("stdout line is not JSON ({e}): {line:?}\nstderr: {}", self.stderr()));
        assert_eq!(message["jsonrpc"], "2.0", "not JSON-RPC 2.0: {line}");
        let checked = match message["method"].as_str() {
            Some("session/update") => check_def("SessionNotification", &message["params"]),
            Some(other) => Err(format!("unexpected agent method {other}")),
            None => {
                let id = message["id"].to_string();
                match (message.get("result"), message.get("error")) {
                    (Some(result), None) => {
                        let method = self.methods.get(&id).cloned().unwrap_or_default();
                        check_def(response_def(&method), result)
                    }
                    (None, Some(error)) => check_def("Error", error),
                    _ => Err("a response needs exactly one of result and error".into()),
                }
            }
        };
        if let Err(error) = checked {
            panic!("schema violation{error}\nmessage: {message}");
        }
        message
    }

    /// Receive until a message from the cursor on matches; returns its index.
    fn wait_index(&mut self, matches: &dyn Fn(&Value) -> bool) -> usize {
        let deadline = Instant::now() + HANG;
        loop {
            if let Some(offset) = self.seen[self.cursor..].iter().position(matches) {
                return self.cursor + offset;
            }
            match self
                .lines
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(Some(line)) => {
                    let message = self.accept(&line);
                    self.seen.push(message);
                }
                Ok(None) | Err(RecvTimeoutError::Disconnected) => {
                    panic!("agent closed stdout; seen {:#?}\nstderr: {}", self.seen, self.stderr())
                }
                Err(RecvTimeoutError::Timeout) => {
                    panic!("hang guard; seen {:#?}\nstderr: {}", self.seen, self.stderr())
                }
            }
        }
    }

    fn wait(&mut self, matches: &dyn Fn(&Value) -> bool) -> Value {
        let index = self.wait_index(matches);
        self.seen[index].clone()
    }

    fn write(&mut self, message: &Value) {
        if let (Some(id), Some(method)) = (message.get("id"), message["method"].as_str()) {
            self.methods.insert(id.to_string(), method.to_string());
        }
        let stdin = self.stdin.as_mut().unwrap();
        stdin.write_all(format!("{message}\n").as_bytes()).unwrap();
        stdin.flush().unwrap();
    }

    /// Send a request as given and wait for its response.
    fn call_raw(&mut self, message: Value) -> Value {
        let id = message["id"].clone();
        self.write(&message);
        self.wait(&|m| m.get("method").is_none() && m["id"] == id)
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        self.call_raw(json!({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params}))
    }

    /// Send a request without waiting; returns its id.
    fn send(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = json!(self.next_id);
        self.write(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        id
    }

    fn response_to(&mut self, id: &Value) -> Value {
        let id = id.clone();
        self.wait(&|m| m.get("method").is_none() && m["id"] == id)
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.write(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    /// Later waits see only messages from here on.
    fn mark(&mut self) {
        self.cursor = self.seen.len();
    }

    /// The session updates since `from`.
    fn updates(&self, from: usize) -> Vec<Value> {
        self.seen[from..]
            .iter()
            .filter(|m| m["method"] == "session/update")
            .map(|m| m["params"]["update"].clone())
            .collect()
    }

    /// Close stdin and wait for the exit (a kill only after the hang guard).
    fn stop(mut self) -> Option<i32> {
        drop(self.stdin.take());
        let deadline = Instant::now() + HANG;
        loop {
            if let Ok(Some(status)) = self.child.try_wait() {
                // Everything the agent wrote is still checked.
                while let Ok(Some(line)) = self.lines.recv_timeout(Duration::from_secs(5)) {
                    let message = self.accept(&line);
                    self.seen.push(message);
                }
                return status.code();
            }
            if Instant::now() > deadline {
                let _ = self.child.kill();
                return self.child.wait().ok().and_then(|s| s.code());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn result(response: &Value) -> Value {
    assert!(response.get("error").is_none(), "error response: {response}");
    response["result"].clone()
}

fn initialize(acp: &mut Acp) -> Value {
    result(&acp.call(
        "initialize",
        json!({"protocolVersion": 1, "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false}}),
    ))
}

fn new_session(acp: &mut Acp, cwd: &Path) -> String {
    let created = result(&acp.call("session/new", json!({"cwd": cwd, "mcpServers": []})));
    created["sessionId"].as_str().unwrap().to_string()
}

fn text_of(updates: &[Value], kind: &str) -> String {
    updates
        .iter()
        .filter(|u| u["sessionUpdate"] == kind)
        .filter_map(|u| u["content"]["text"].as_str())
        .collect()
}

fn of_call<'u>(updates: &'u [Value], id: &str) -> Vec<&'u Value> {
    updates.iter().filter(|u| u["toolCallId"] == id).collect()
}

// ---------------------------------------------------------------------------------------------------------------

#[test]
fn cli_acp_initialize_new_session_and_a_prompt_with_cells() {
    let (port, seen) = mock_server(
        vec![
            anthropic_tool("toolu_1", "x = 6 * 7\nprint('x is', x)\nx + 1"),
            anthropic_tool("toolu_2", "open('hello.txt', 'w').write('hi\\n')"),
            anthropic_text("The answer is 43."),
        ],
        Naming::Title("Acp test".into()),
    );
    let sandbox = Sandbox::new(port);
    let mut acp = Acp::start(&sandbox, &[]);
    let init = initialize(&mut acp);
    assert_eq!(init["protocolVersion"], 1);
    assert_eq!(init["authMethods"], json!([]));
    assert_eq!(init["agentCapabilities"]["loadSession"], true);
    assert_eq!(
        init["agentCapabilities"]["promptCapabilities"],
        json!({"image": true, "audio": false, "embeddedContext": true})
    );
    assert_eq!(init["agentCapabilities"]["sessionCapabilities"]["list"], json!({}));
    assert_eq!(init["agentInfo"]["name"], "ultron");
    assert_eq!(
        result(&acp.call("authenticate", json!({"methodId": "none"}))),
        json!({})
    );

    let created = result(&acp.call("session/new", json!({"cwd": sandbox.cwd, "mcpServers": []})));
    let session = created["sessionId"].as_str().unwrap().to_string();
    assert!(!session.is_empty());
    let options = created["configOptions"].as_array().unwrap();
    assert_eq!(options[0]["id"], "model");
    assert_eq!(options[0]["currentValue"], "anthropic/claude-sonnet-4-5");

    acp.mark();
    let from = acp.seen.len();
    let prompt = acp.send(
        "session/prompt",
        json!({"sessionId": session, "prompt": [
            {"type": "text", "text": "Compute "},
            {"type": "resource_link", "uri": "file:///tmp/notes.txt", "name": "notes.txt"},
            {"type": "resource", "resource": {"uri": "file:///tmp/a.py", "text": "print(1)", "mimeType": "text/x-python"}},
        ]}),
    );
    let response = acp.response_to(&prompt);
    assert_eq!(result(&response), json!({"stopReason": "end_turn"}));
    let updates = acp.updates(from);
    for message in acp.seen[from..].iter().filter(|m| m["method"] == "session/update") {
        assert_eq!(message["params"]["sessionId"], session.as_str());
        assert_ne!(
            message["params"]["update"]["sessionUpdate"], "user_message_chunk",
            "{message}"
        );
    }

    // Cell 1: an execute tool call carrying its code, then its output.
    let first = of_call(&updates, "toolu_1");
    assert_eq!(first[0]["sessionUpdate"], "tool_call");
    assert_eq!(first[0]["kind"], "execute");
    assert_eq!(first[0]["status"], "in_progress");
    assert_eq!(first[0]["title"], "`x = 6 * 7`");
    assert_eq!(first[0]["rawInput"]["code"], "x = 6 * 7\nprint('x is', x)\nx + 1");
    assert_eq!(
        first[0]["content"][0]["content"]["text"],
        "```python\nx = 6 * 7\nprint('x is', x)\nx + 1\n```"
    );
    let end = first.last().unwrap();
    assert_eq!(end["sessionUpdate"], "tool_call_update");
    assert_eq!(end["status"], "completed");
    assert_eq!(end["content"][1]["content"]["text"], "```\nx is 42\n43\n```");
    assert_eq!(end["rawOutput"], json!({"output": "x is 42\n43", "isError": false}));
    assert!(end.get("locations").is_none(), "{end}");

    // Cell 2 wrote a file: an edit with its location and a diff from nothing.
    let second = of_call(&updates, "toolu_2");
    let end = second.last().unwrap();
    assert_eq!(end["status"], "completed");
    assert_eq!(end["kind"], "edit");
    let written = sandbox.cwd.join("hello.txt");
    assert_eq!(end["locations"], json!([{"path": written}]));
    assert!(
        end["content"]
            .as_array()
            .unwrap()
            .contains(&json!({"type": "diff", "path": written, "oldText": null, "newText": "hi\n"})),
        "{end}"
    );

    assert_eq!(text_of(&updates, "agent_message_chunk"), "The answer is 43.");
    let usage = updates.iter().find(|u| u["sessionUpdate"] == "usage_update").unwrap();
    assert!(
        usage["size"].as_u64().unwrap() > 0 && usage["used"].as_u64().unwrap() > 0,
        "{usage}"
    );

    // The model saw one user message: inline text and link, then the embedded resource as context.
    let requests = seen.lock().unwrap().requests.clone();
    let user = requests[0]["body"]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|m| m["role"] == "user")
        .unwrap()
        .to_string();
    assert!(
        user.contains(
            "Compute [@notes.txt](file:///tmp/notes.txt)\\n<context ref=\\\"file:///tmp/a.py\\\">\\nprint(1)\\n</context>"
        ),
        "{user}"
    );
    assert_eq!(
        tool_result_text(&requests[1]["body"], "toolu_1").as_deref(),
        Some("x is 42\n43")
    );
    assert_eq!(std::fs::read_to_string(&written).unwrap(), "hi\n");

    // Unknown methods and sessions are errors; a second prompt during a turn is refused.
    let unknown = acp.call("session/set_mode", json!({"sessionId": session, "modeId": "x"}));
    assert_eq!(unknown["error"]["code"], -32601);
    let stale = acp.call(
        "session/prompt",
        json!({"sessionId": "nope", "prompt": [{"type": "text", "text": "x"}]}),
    );
    assert_eq!(stale["error"]["code"], -32603);
    assert_eq!(stale["error"]["data"]["details"], "Session not found: nope");
    assert_eq!(acp.stop(), Some(0));
}

#[test]
fn cli_acp_cancel_mid_turn_ends_with_cancelled() {
    let (port, seen) = mock_server(
        vec![
            anthropic_tool("toolu_s", "import time\ntime.sleep(60)"),
            anthropic_text("Back again."),
        ],
        Naming::Title("Acp cancel".into()),
    );
    let sandbox = Sandbox::new(port);
    let mut acp = Acp::start(&sandbox, &[]);
    initialize(&mut acp);
    let session = new_session(&mut acp, &sandbox.cwd);
    acp.mark();
    let from = acp.seen.len();
    let prompt = acp.send(
        "session/prompt",
        json!({"sessionId": session, "prompt": [{"type": "text", "text": "sleep"}]}),
    );
    // The cell is running once its tool call is reported.
    acp.wait(&|m| m["params"]["update"]["sessionUpdate"] == "tool_call");
    acp.notify("session/cancel", json!({"sessionId": session}));
    let response = acp.response_to(&prompt);
    assert_eq!(result(&response), json!({"stopReason": "cancelled"}));
    let updates = acp.updates(from);
    let cell = of_call(&updates, "toolu_s");
    assert_eq!(cell.last().unwrap()["status"], "failed", "{updates:#?}");
    assert_eq!(seen.lock().unwrap().requests.len(), 1, "the turn stopped at the cell");

    // The session takes the next prompt.
    let reply = acp.call(
        "session/prompt",
        json!({"sessionId": session, "prompt": [{"type": "text", "text": "again"}]}),
    );
    assert_eq!(result(&reply)["stopReason"], "end_turn");
    assert_eq!(seen.lock().unwrap().requests.len(), 2);
    assert_eq!(acp.stop(), Some(0));
}

#[test]
fn cli_acp_load_replays_the_conversation_then_continues() {
    let (port, seen) = mock_server(
        vec![
            anthropic_tool("toolu_a", "print('first')"),
            anthropic_text("First done."),
            anthropic_text("Second done."),
        ],
        Naming::Title("Acp load".into()),
    );
    let sandbox = Sandbox::new(port);
    let mut acp = Acp::start(&sandbox, &[]);
    initialize(&mut acp);
    let session = new_session(&mut acp, &sandbox.cwd);
    let reply = acp.call(
        "session/prompt",
        json!({"sessionId": session, "prompt": [{"type": "text", "text": "Do the first thing"}]}),
    );
    assert_eq!(result(&reply)["stopReason"], "end_turn");
    assert_eq!(acp.stop(), Some(0));

    let mut acp = Acp::start(&sandbox, &[]);
    let init = initialize(&mut acp);
    assert_eq!(init["agentCapabilities"]["loadSession"], true);
    let listed = result(&acp.call("session/list", json!({"cwd": sandbox.cwd})));
    let ids: Vec<&str> = listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["sessionId"].as_str())
        .collect();
    assert_eq!(ids, vec![session.as_str()]);
    assert_eq!(listed["sessions"][0]["cwd"], json!(sandbox.cwd));

    acp.mark();
    let from = acp.seen.len();
    let load = acp.send(
        "session/load",
        json!({"sessionId": session, "cwd": sandbox.cwd, "mcpServers": []}),
    );
    let answered = acp.wait_index(&|m| m.get("method").is_none() && m["id"] == load);
    assert!(acp.seen[answered].get("error").is_none(), "{}", acp.seen[answered]);
    // The replay comes before the answer (avante marks updates received during the load as replayed).
    let replay: Vec<Value> = acp.seen[from..answered]
        .iter()
        .filter(|m| m["method"] == "session/update")
        .map(|m| m["params"]["update"].clone())
        .collect();
    let kinds: Vec<&str> = replay.iter().filter_map(|u| u["sessionUpdate"].as_str()).collect();
    assert_eq!(
        kinds,
        vec![
            "user_message_chunk",
            "tool_call",
            "tool_call_update",
            "agent_message_chunk"
        ],
        "{replay:#?}"
    );
    assert!(
        replay[0]["content"]["text"]
            .as_str()
            .unwrap()
            .ends_with("Do the first thing")
    );
    assert_eq!(replay[1]["rawInput"]["code"], "print('first')");
    assert_eq!(replay[2]["status"], "completed");
    assert_eq!(replay[2]["content"][1]["content"]["text"], "```\nfirst\n```");
    assert_eq!(replay[3]["content"]["text"], "First done.");
    for message in &acp.seen[from..answered] {
        assert_eq!(message["params"]["sessionId"], session.as_str());
    }

    let reply = acp.call(
        "session/prompt",
        json!({"sessionId": session, "prompt": [{"type": "text", "text": "And the second"}]}),
    );
    assert_eq!(result(&reply)["stopReason"], "end_turn");
    // The resumed session sent the whole conversation.
    let requests = seen.lock().unwrap().requests.clone();
    let last = requests.last().unwrap()["body"].to_string();
    assert!(last.contains("Do the first thing") && last.contains("First done.") && last.contains("And the second"));
    assert_eq!(acp.stop(), Some(0));
}

// ---------------------------------------------------------------------------------------------------------------
// The Neovim clients.

fn client_fixture(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/acp").join(name);
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// A step's message with the session's cwd and id filled in.
fn fill(message: &Value, cwd: &Path, session: &str) -> Value {
    let text = message
        .to_string()
        .replace("<CWD>", &cwd.display().to_string())
        .replace("<SESSION>", session);
    serde_json::from_str(&text).unwrap()
}

/// CodeCompanion's `parse_title` + `build_label` (interactions/chat/acp/formatters.lua) for a call without paths.
fn codecompanion_label(call: &Value) -> String {
    let kind = call["kind"].as_str().unwrap_or("other").replace('_', " ");
    let kind = format!("{}{}", kind[..1].to_uppercase(), &kind[1..]);
    if let Some(path) = call["locations"][0]["path"].as_str() {
        return format!("{kind}: {path}");
    }
    let title = call["title"].as_str().unwrap_or("Tool call").trim_start();
    let target = match title.strip_prefix('`').and_then(|rest| rest.split_once('`')) {
        Some((inner, _)) if !inner.is_empty() => inner.to_string(),
        _ => title.to_string(),
    };
    format!("{kind}: {target}")
}

/// What CodeCompanion's connection accepts of an agent line (acp/init.lua `handle_rpc_message`,
/// `handle_incoming_request_or_notification`, prompt_builder.lua `handle_session_update`).
fn codecompanion_accepts(message: &Value, session: &str) {
    let line = message.to_string();
    assert!(line.trim_start().starts_with('{'), "{line}");
    if message["method"] == "session/update" {
        assert_eq!(
            message["params"]["sessionId"], session,
            "CodeCompanion drops other sessions' updates"
        );
        let update = &message["params"]["update"];
        match update["sessionUpdate"].as_str().unwrap() {
            "agent_message_chunk" | "agent_thought_chunk" => assert_eq!(update["content"]["type"], "text"),
            // A pending call is held back from the chat buffer: cells are never left pending.
            "tool_call" | "tool_call_update" => {
                assert_ne!(update["status"], "pending", "{update}");
                assert!(update["toolCallId"].is_string());
            }
            "usage_update" => assert!(update["used"].is_u64()),
            other => panic!("CodeCompanion ignores {other}"),
        }
    }
}

#[test]
fn cli_acp_codecompanion_message_sequence() {
    let fixture = client_fixture("codecompanion.json");
    let steps = fixture["steps"].as_array().unwrap();
    let (port, seen) = mock_server(
        vec![
            anthropic_tool("toolu_1", "x = 6 * 7\nprint('x is', x)\nx + 1"),
            anthropic_text("The answer is 43."),
            anthropic_tool("toolu_2", "import time\ntime.sleep(60)"),
        ],
        Naming::Title("CodeCompanion".into()),
    );
    let sandbox = Sandbox::new(port);
    let mut acp = Acp::start(&sandbox, &[]);
    let cwd = sandbox.cwd.clone();

    // initialize: CodeCompanion authenticates only when authMethods is not empty.
    let init = result(&acp.call_raw(fill(&steps[0]["send"], &cwd, "")));
    assert_eq!(init["protocolVersion"], steps[0]["send"]["params"]["protocolVersion"]);
    assert_eq!(init["authMethods"].as_array().map(Vec::len), Some(0));
    assert_eq!(init["agentCapabilities"]["loadSession"], true);
    // session/new: `new_session.sessionId`.
    let created = result(&acp.call_raw(fill(&steps[1]["send"], &cwd, "")));
    let session = created["sessionId"].as_str().unwrap().to_string();
    // configOptions with a `model` select: CodeCompanion's model picker (`_find_config_option("model")`).
    assert!(
        created["configOptions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|o| o["category"] == "model" && o["type"] == "select")
    );

    // session/prompt: its response's `result.stopReason` ends the turn (`handle_done`).
    acp.mark();
    let from = acp.seen.len();
    let reply = acp.call_raw(fill(&steps[2]["send"], &cwd, &session));
    assert_eq!(result(&reply)["stopReason"], "end_turn");
    for message in &acp.seen[from..] {
        codecompanion_accepts(message, &session);
    }
    let updates = acp.updates(from);
    let call = of_call(&updates, "toolu_1");
    assert_eq!(codecompanion_label(call[0]), "Execute: x = 6 * 7");
    assert_eq!(call.last().unwrap()["status"], "completed");
    assert_eq!(text_of(&updates, "agent_message_chunk"), "The answer is 43.");
    let first = seen.lock().unwrap().requests[0]["body"].to_string();
    assert!(
        first.contains(&format!(
            "Sharing the following file as context: {}/notes.txt\\n\\nCompute the answer",
            cwd.display()
        )),
        "{first}"
    );

    // A prompt cancelled once its cell runs (PromptBuilder:cancel sends session/cancel).
    acp.mark();
    let from = acp.seen.len();
    let prompt = fill(&steps[3]["send"], &cwd, &session);
    acp.write(&prompt);
    acp.wait(&|m| m["params"]["update"]["sessionUpdate"] == "tool_call");
    acp.write(&fill(&steps[4]["cancel"], &cwd, &session));
    let reply = acp.response_to(&prompt["id"]);
    assert_eq!(result(&reply)["stopReason"], "cancelled");
    for message in &acp.seen[from..] {
        codecompanion_accepts(message, &session);
    }
    assert_eq!(acp.stop(), Some(0));
}

/// What avante's client accepts of an agent line (libs/acp_client.lua `_handle_message`, `_handle_session_update`,
/// llm.lua `on_session_update`).
fn avante_accepts(message: &Value, session: &str) {
    // `_handle_message`: a notification has a method and no result/error; a response an id and result or error.
    let is_notification =
        message.get("method").is_some() && message.get("result").is_none() && message.get("error").is_none();
    let is_response = message.get("id").is_some() && (message["result"].is_object() || message["error"].is_object());
    assert!(is_notification || is_response, "avante: unknown message type {message}");
    if message["method"] == "session/update" {
        assert_eq!(message["params"]["sessionId"], session);
        let update = &message["params"]["update"];
        assert!(update.is_object());
        if ["agent_message_chunk", "agent_thought_chunk"].contains(&update["sessionUpdate"].as_str().unwrap()) {
            assert_eq!(update["content"]["type"], "text");
            assert!(update["content"]["text"].is_string());
        }
    }
}

#[test]
fn cli_acp_avante_message_sequence() {
    let fixture = client_fixture("avante.json");
    let steps = fixture["steps"].as_array().unwrap();
    let (port, seen) = mock_server(
        vec![
            anthropic_tool("toolu_1", "x = 6 * 7\nprint('x is', x)\nx + 1"),
            anthropic_text("The answer is 43."),
        ],
        Naming::Title("Avante".into()),
    );
    let sandbox = Sandbox::new(port);
    let mut acp = Acp::start(&sandbox, &[]);
    let cwd = sandbox.cwd.clone();

    // initialize (id 0): avante stops unless the agent answers its exact protocol version.
    let reply = acp.call_raw(fill(&steps[0]["send"], &cwd, ""));
    avante_accepts(&reply, "");
    let init = result(&reply);
    assert_eq!(init["protocolVersion"], json!(1));
    // `load_session` requires loadSession == true; http/sse MCP servers are refused unless advertised.
    assert_eq!(init["agentCapabilities"]["loadSession"], true);
    assert_eq!(
        init["agentCapabilities"]["mcpCapabilities"],
        json!({"http": false, "sse": false})
    );

    // session/new: a non-empty string sessionId in an object result.
    let reply = acp.call_raw(fill(&steps[1]["send"], &cwd, ""));
    avante_accepts(&reply, "");
    let session = result(&reply)["sessionId"].as_str().unwrap().to_string();
    assert!(!session.is_empty());

    // session/prompt with the system prompt and the user's message as text blocks.
    acp.mark();
    let from = acp.seen.len();
    let reply = acp.call_raw(fill(&steps[2]["send"], &cwd, &session));
    assert_eq!(result(&reply)["stopReason"], "end_turn");
    for message in &acp.seen[from..] {
        avante_accepts(message, &session);
    }
    let updates = acp.updates(from);
    // `tool_call` comes first (`add_tool_call_message` names the call by kind, input rawInput), its updates after.
    let call = of_call(&updates, "toolu_1");
    assert_eq!(call[0]["sessionUpdate"], "tool_call");
    assert_eq!(call[0]["kind"], "execute");
    assert!(call[0]["rawInput"].is_object());
    assert!(call[1..].iter().all(|u| u["sessionUpdate"] == "tool_call_update"));
    assert_eq!(call.last().unwrap()["status"], "completed");
    assert_eq!(text_of(&updates, "agent_message_chunk"), "The answer is 43.");
    let first = seen.lock().unwrap().requests[0]["body"].to_string();
    assert!(first.contains("You are a helpful coding assistant working in Neovim.\\n\\nCompute the answer"));

    // A prompt to a session this agent does not know: avante's session recovery reads code -32603 and
    // `data.details` starting "Session not found".
    let reply = acp.call_raw(fill(&steps[3]["send"], &cwd, &session));
    avante_accepts(&reply, &session);
    assert_eq!(reply["error"]["code"], -32603);
    assert!(
        reply["error"]["data"]["details"]
            .as_str()
            .unwrap()
            .starts_with("Session not found:")
    );
    assert_eq!(acp.stop(), Some(0));
}

#[test]
fn cli_acp_stdout_is_only_json_rpc_and_bad_input_is_answered() {
    let (port, _) = mock_server(Vec::new(), Naming::Title("x".into()));
    let sandbox = Sandbox::new(port);
    let mut acp = Acp::start(&sandbox, &[]);
    acp.write_raw("this is not json");
    let parse = acp.wait(&|m| m["error"]["code"] == -32700);
    assert_eq!(parse["id"], Value::Null);
    let bad = acp.call("initialize", json!({}));
    assert_eq!(bad["error"]["code"], -32602);
    let missing_cwd = acp.call("session/new", json!({"cwd": "relative/dir", "mcpServers": []}));
    assert_eq!(missing_cwd["error"]["code"], -32602);
    assert_eq!(acp.stop(), Some(0));
}

impl Acp {
    fn write_raw(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().unwrap();
        stdin.write_all(format!("{line}\n").as_bytes()).unwrap();
        stdin.flush().unwrap();
    }
}

#[test]
fn cli_acp_schema_checker_rejects_bad_messages() {
    // The checker itself: the shapes the tests rely on it to refuse.
    assert!(check_def("PromptResponse", &json!({"stopReason": "end_turn"})).is_ok());
    assert!(check_def("PromptResponse", &json!({"stopReason": "done"})).is_err());
    assert!(check_def("PromptResponse", &json!({})).is_err());
    let update = |u: Value| json!({"sessionId": "s", "update": u});
    assert!(
        check_def(
            "SessionNotification",
            &update(json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "x"}}))
        )
        .is_ok()
    );
    assert!(
        check_def(
            "SessionNotification",
            &update(json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text"}}))
        )
        .is_err()
    );
    assert!(
        check_def(
            "SessionNotification",
            &update(json!({"sessionUpdate": "tool_call", "toolCallId": "t"}))
        )
        .is_err()
    );
    assert!(
        check_def(
            "SessionNotification",
            &update(json!({"sessionUpdate": "tool_call_update", "toolCallId": "t", "status": "running"}))
        )
        .is_err()
    );
    assert!(check_def("SessionNotification", &update(json!({"sessionUpdate": "tool_call_update", "toolCallId": "t", "content": [{"type": "diff", "path": "/a"}]}))).is_err());
    assert!(check_def("InitializeResponse", &json!({"protocolVersion": -1})).is_err());
}

/// W25E: `/persona`, `/keepgoing` and `/goal` typed in the editor are the session's commands, answered by the worker
/// (as one agent message, the turn ending at once), never a model turn that reads them as text.
#[test]
fn cli_acp_session_commands_are_answered_by_the_worker_not_the_model() {
    let (port, seen) = mock_server(Vec::new(), Naming::Title("Acp commands".into()));
    let sandbox = Sandbox::new(port);
    let mut acp = Acp::start(&sandbox, &[]);
    initialize(&mut acp);
    let session = new_session(&mut acp, &sandbox.cwd);
    for (typed, answer) in [
        ("/persona on", "Persona is on for this session; keep-going off"),
        ("/keepgoing off", "Persona is on for this session; keep-going off"),
        (
            "/persona auto",
            "Persona for this session follows the settings (off); keep-going off",
        ),
        ("/goal clear", "No goal to clear."),
    ] {
        acp.mark();
        let from = acp.seen.len();
        let reply = acp.call(
            "session/prompt",
            json!({"sessionId": session, "prompt": [{"type": "text", "text": typed}]}),
        );
        assert_eq!(result(&reply), json!({"stopReason": "end_turn"}), "{typed}");
        assert_eq!(text_of(&acp.updates(from), "agent_message_chunk"), answer, "{typed}");
    }
    assert!(seen.lock().unwrap().requests.is_empty(), "a command reached the model");
    assert_eq!(acp.stop(), Some(0));
}

/// W25E: `--name` names the first new session only (a second directory's session is not named too), and session
/// selectors are refused: the client opens its sessions itself.
#[test]
fn cli_acp_name_is_the_first_sessions_and_selectors_are_refused() {
    let (port, _seen) = mock_server(Vec::new(), Naming::Title("unused".into()));
    let sandbox = Sandbox::new(port);
    let other = sandbox.root.join("other");
    std::fs::create_dir_all(&other).unwrap();
    let mut acp = Acp::start(&sandbox, &["--name", "Editor work"]);
    initialize(&mut acp);
    let first = new_session(&mut acp, &sandbox.cwd);
    let second = new_session(&mut acp, &sandbox.cwd);
    let third = new_session(&mut acp, &other);
    assert_eq!(acp.stop(), Some(0));
    // The sessions' names as their files record them (every session of this agent dir, whatever its cwd).
    let names: HashMap<String, Option<String>> = [sandbox.cwd.clone(), other.clone()]
        .iter()
        .flat_map(|cwd| session_journal::names::list_sessions(&sandbox.sessions_dir(), &cwd.display().to_string()))
        .map(|s| (s.metadata.id.clone(), s.name.clone()))
        .collect();
    assert_eq!(names.get(&first), Some(&Some("Editor work".to_string())), "{names:?}");
    assert_eq!(names.get(&second), Some(&None), "{names:?}");
    assert_eq!(names.get(&third), Some(&None), "{names:?}");

    for (selector, extra) in [
        ("--continue", None),
        ("--resume", None),
        ("--session", Some("abc")),
        ("--session-id", Some("abc")),
        ("--fork", Some("abc")),
    ] {
        let mut command = sandbox.ultron();
        command.args(["acp", "--model", "anthropic/claude-sonnet-4-5", selector]);
        if let Some(extra) = extra {
            command.arg(extra);
        }
        let out = command.stdin(Stdio::null()).output().unwrap();
        assert_eq!(out.status.code(), Some(1), "{selector}");
        assert_eq!(
            String::from_utf8_lossy(&out.stderr),
            format!(
                "Error: `ultron acp` opens the sessions its client asks for (session/new, session/load): {selector} is not supported\n"
            )
        );
        assert!(out.stdout.is_empty());
    }
}
