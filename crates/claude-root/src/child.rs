//! Claude Code as an Ultron subagent (child.ts): an `rlm.spawn` child of an `ultron mcp` root runs as its own
//! `claude -p` process whose only tool is the `rlm` tool of its own `ultron mcp --child` server. Its final reply is
//! the task's result and its reported usage is charged to the task.
//!
//! Ported here: the model rule, the `claude` arguments and MCP config of a child, the stream-json fold and the
//! process run with its result. Not yet wired: the session runtime builds its subagent runner itself
//! (session-core `Stack`), so `ultron mcp` children still run as native lanes until session-core takes a runner.

use crate::claude_cli::skip_permissions_args;
use crate::guide::CLAUDE_RLM_TOOL;
use crate::self_cmd::SelfCommand;
use serde_json::{Map, Value, json};
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

/// Environment a child Claude Code must not inherit from the session that started its parent server.
pub const PARENT_ONLY_ENV: [&str; 7] = [
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_PID",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_CODE_SSE_PORT",
    "CLAUDE_PROJECT_DIR",
    "CLAUDE_ENV_FILE",
];

/// `claude-code/opus` -> `opus`; another provider's model cannot drive a Claude Code child.
pub fn child_claude_model(requested: Option<&str>, fallback: &str) -> Result<String, String> {
    let Some(trimmed) = requested.map(str::trim).filter(|r| !r.is_empty()) else {
        return Ok(fallback.to_string());
    };
    if let Some(model) = trimmed.strip_prefix("claude-code/") {
        return Ok(model.to_string());
    }
    if !trimmed.contains('/') {
        return Ok(trimmed.to_string());
    }
    Err(format!(
        "rlm.spawn model={trimmed}: subagents here are Claude Code processes; pass claude-code/<claude-opus-5-5|opus|sonnet|haiku> or leave model unset"
    ))
}

/// The `claude` arguments of a child (the brief goes in on stdin).
pub fn child_claude_args(
    model: &str,
    mcp_config: &str,
    system_prompt_file: &str,
    skip_permissions: &[String],
) -> Vec<String> {
    let mut args: Vec<String> = [
        "-p",
        "--output-format",
        "stream-json",
        "--verbose",
        "--no-session-persistence",
        "--model",
        model,
        "--strict-mcp-config",
        "--mcp-config",
        mcp_config,
        "--tools",
        "",
        "--allowedTools",
        CLAUDE_RLM_TOOL,
        "--setting-sources",
        "",
        "--disable-slash-commands",
    ]
    .map(String::from)
    .to_vec();
    args.extend(skip_permissions.iter().cloned());
    args.extend(["--system-prompt-file".to_string(), system_prompt_file.to_string()]);
    args
}

/// What one child run is asked to do (`NativeExternalChildRun`).
#[derive(Clone, Debug, Default)]
pub struct ChildRun {
    pub lane_name: String,
    pub prompt: String,
    pub level: u64,
    pub allowance: u64,
    pub model: Option<String>,
    pub cwd: Option<String>,
    pub env: Vec<(String, String)>,
}

/// The `ultron mcp --child` server config of a child.
pub struct ChildServer<'a> {
    pub self_cmd: &'a SelfCommand,
    pub parent_socket: &'a str,
    pub parent_name: &'a str,
    pub frame_model: Option<&'a str>,
    pub no_context_files: bool,
    pub no_skills: bool,
}

/// The child's MCP config: its own `ultron mcp --child` server; the token goes in the server's environment, out of
/// process listings.
pub fn child_mcp_config(server: &ChildServer, run: &ChildRun, model: &str, token: &str, cwd: &str) -> String {
    let mut args: Vec<String> = server.self_cmd.args.clone();
    args.extend(
        [
            "mcp",
            "--child",
            "--no-instructions",
            "--parent-socket",
            server.parent_socket,
            "--parent-name",
            server.parent_name,
            "--parent-lane",
            &run.lane_name,
            "--level",
            &run.level.to_string(),
            "--allowance",
            &run.allowance.to_string(),
            "--child-model",
            model,
        ]
        .map(String::from),
    );
    if let Some(frame) = server.frame_model {
        args.extend(["--frame-model".to_string(), frame.to_string()]);
    }
    if server.no_context_files {
        args.push("--no-context-files".into());
    }
    if server.no_skills {
        args.push("--no-skills".into());
    }
    let mut env = Map::new();
    env.insert("ULTRON_PARENT_TOKEN".into(), token.into());
    for (key, value) in &run.env {
        env.insert(key.clone(), value.clone().into());
    }
    if run.cwd.is_some() {
        env.insert("ULTRON_CHILD_CWD".into(), cwd.into());
    }
    session_file::js::stringify(&json!({
        "mcpServers": {"ultron": {"type": "stdio", "command": server.self_cmd.command, "args": args, "env": env}}
    }))
}

/// Running totals of a child's stream-json output.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StreamStats {
    pub turns: u64,
    pub tool_calls: u64,
    pub text: String,
    pub model: Option<String>,
    messages: Option<HashSet<String>>,
}

/// Fold one stream-json line of `claude -p` into `stats`; returns the final result message if it is one.
pub fn fold_stream_line(line: &str, stats: &mut StreamStats) -> Option<Map<String, Value>> {
    let Ok(Value::Object(message)) = serde_json::from_str::<Value>(line) else {
        return None;
    };
    if message.get("type") == Some(&json!("system"))
        && message.get("subtype") == Some(&json!("init"))
        && let Some(Value::String(model)) = message.get("model")
    {
        stats.model = Some(format!("claude-code/{model}"));
    }
    if message.get("type") == Some(&json!("assistant"))
        && let Some(body) = message.get("message")
        && let Some(content) = body.get("content").and_then(Value::as_array)
    {
        // Claude Code streams one message per content block; blocks of one response share its id.
        let messages = stats.messages.get_or_insert_with(HashSet::new);
        let id = body
            .get("id")
            .and_then(Value::as_str)
            .map_or_else(|| format!("line-{}", messages.len()), str::to_string);
        if messages.insert(id) {
            stats.turns += 1;
        }
        for part in content {
            if part.get("type") == Some(&json!("tool_use")) {
                stats.tool_calls += 1;
            }
            if part.get("type") == Some(&json!("text"))
                && let Some(text) = part.get("text").and_then(Value::as_str)
                && !text.trim().is_empty()
            {
                stats.text = text.to_string();
            }
        }
    }
    (message.get("type") == Some(&json!("result"))).then_some(message)
}

/// A finished child (`NativeExternalChildResult`).
#[derive(Clone, Debug, PartialEq)]
pub struct ChildResult {
    pub text: String,
    pub turns: u64,
    pub tool_calls: u64,
    pub model: Option<String>,
    pub input_tokens: f64,
    pub output_tokens: f64,
    pub cost: Option<f64>,
    pub wall_ms: Option<f64>,
}

/// The task result from the final `result` message (or why the child failed).
pub fn child_result(
    result: Option<&Map<String, Value>>,
    stats: &StreamStats,
    code: Option<i32>,
    stderr: &str,
) -> Result<ChildResult, String> {
    let Some(result) = result else {
        let code = code.map_or_else(|| "?".to_string(), |c| c.to_string());
        let tail = if stderr.is_empty() {
            String::new()
        } else {
            format!(": {}", stderr.trim())
        };
        return Err(format!("claude exited with code {code} without a result{tail}"));
    };
    let text = result
        .get("result")
        .and_then(Value::as_str)
        .map_or_else(|| stats.text.clone(), str::to_string);
    if result.get("is_error") == Some(&Value::Bool(true)) && text.trim().is_empty() {
        let subtype = match result.get("subtype") {
            None | Some(Value::Null) => "error".to_string(),
            other => crate::mcp_protocol::js_string(other),
        };
        return Err(format!("claude child failed: {subtype}"));
    }
    let usage = result.get("usage").cloned().unwrap_or(json!({}));
    let number = |key: &str| {
        usage
            .get(key)
            .and_then(Value::as_f64)
            .filter(|n| n.is_finite())
            .unwrap_or(0.0)
    };
    let input = number("input_tokens") + number("cache_creation_input_tokens") + number("cache_read_input_tokens");
    Ok(ChildResult {
        text,
        turns: result.get("num_turns").and_then(Value::as_u64).unwrap_or(stats.turns),
        tool_calls: stats.tool_calls,
        model: stats.model.clone(),
        input_tokens: input,
        output_tokens: number("output_tokens"),
        cost: result.get("total_cost_usd").and_then(Value::as_f64),
        wall_ms: result.get("duration_ms").and_then(Value::as_f64),
    })
}

/// What [`run_claude_child`] needs besides the run.
pub struct ChildOptions<'a> {
    pub claude: &'a str,
    pub server: ChildServer<'a>,
    pub cwd: &'a str,
    /// The child's model when the spawn named none.
    pub model: &'a str,
    pub system_prompt: String,
    pub skip_permissions: Vec<String>,
    pub var: &'a dyn Fn(&str) -> Option<String>,
}

/// Run one subagent as `claude -p` attached to its own `ultron mcp --child` server; `token` is the child's
/// one-time verdict token; `signal` aborts the process group.
pub fn run_claude_child(
    options: &ChildOptions,
    run: &ChildRun,
    token: &str,
    signal: &kernel::Signal,
    progress: &dyn Fn(&StreamStats),
) -> Result<ChildResult, String> {
    use std::os::unix::process::CommandExt;
    let model = child_claude_model(run.model.as_deref(), options.model)?;
    let cwd = run.cwd.clone().unwrap_or_else(|| options.cwd.to_string());
    let dir = crate::temp_dir("ultron-child-", options.var)?;
    let cleanup = scopeguard(dir.clone());
    let prompt_file = dir.join("system-prompt.md");
    crate::write_private(&prompt_file, &options.system_prompt).map_err(|e| e.to_string())?;
    let mcp_config = child_mcp_config(&options.server, run, &model, token, &cwd);
    let args = child_claude_args(
        &model,
        &mcp_config,
        &prompt_file.display().to_string(),
        &options.skip_permissions,
    );
    let mut command = Command::new(options.claude);
    command
        .args(&args)
        .current_dir(&cwd)
        .envs(run.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    for name in PARENT_ONLY_ENV {
        command.env_remove(name);
    }
    let mut child = command.spawn().map_err(|e| e.to_string())?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(run.prompt.as_bytes());
    }
    let mut stderr_pipe = child.stderr.take().expect("piped stderr");
    let stderr_reader = std::thread::spawn(move || {
        let mut text = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut text);
        let text = String::from_utf8_lossy(&text).into_owned();
        let mut start = text.len().saturating_sub(4000);
        while !text.is_char_boundary(start) {
            start += 1;
        }
        text[start..].to_string()
    });
    let pid = child.id();
    let killer_signal = signal.clone();
    let killer = std::thread::spawn(move || {
        while !killer_signal.wait(Duration::from_millis(200)) {
            if !crate::control::pid_alive(pid) {
                return;
            }
        }
        let group = i32::try_from(pid).ok().and_then(rustix::process::Pid::from_raw);
        if let Some(group) = group {
            let _ = rustix::process::kill_process_group(group, rustix::process::Signal::TERM);
            std::thread::sleep(Duration::from_secs(5));
            let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
        }
    });
    let mut stats = StreamStats::default();
    let mut result = None;
    for line in BufReader::new(child.stdout.take().expect("piped stdout")).lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        if let Some(found) = fold_stream_line(&line, &mut stats) {
            result = Some(found);
        }
        progress(&stats);
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    let stderr = stderr_reader.join().unwrap_or_default();
    drop(cleanup);
    if signal.is_aborted() {
        let _ = killer.join();
        return Err(signal.reason().unwrap_or_else(|| "subagent cancelled".into()));
    }
    child_result(result.as_ref(), &stats, status.code(), &stderr)
}

/// Removes a directory when dropped.
struct DirGuard(std::path::PathBuf);

impl Drop for DirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn scopeguard(dir: std::path::PathBuf) -> DirGuard {
    DirGuard(dir)
}

/// Skip-permissions arguments for a child, as for the root.
pub fn child_skip_permissions(var: &dyn Fn(&str) -> Option<String>, setting: Option<bool>) -> Vec<String> {
    skip_permissions_args(var, crate::claude_cli::current_uid(), setting)
}
