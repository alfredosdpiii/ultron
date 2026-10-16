//! Ported from packages/coding-agent/test/ultron-claude-{launcher,mcp,host,root}.test.ts, with the literal
//! expectations of the TypeScript tests.

use ai::AbortSignal;
use claude_root::child::{StreamStats, child_claude_args, child_claude_model, child_result, fold_stream_line};
use claude_root::claude_cli::{parse_flags, skip_permissions_args};
use claude_root::control::{
    ControlClient, ServerQuery, ServerRecord, control_dir, find_server, list_servers, start_control_server,
};
use claude_root::external_root::{CellRunner, EventSource, ExternalRoot, MAX_INBOX};
use claude_root::guide::{
    Audience, CLAUDE_RLM_TOOL, GuideOptions, PromptResources, claude_runtime_guide, claude_system_prompt, runtime_guide,
};
use claude_root::launcher::{LaunchInput, LauncherOptions, build_claude_launch, parse_launcher_args};
use claude_root::mcp_protocol::{McpHandlers, McpPrompt, McpResource, serve};
use claude_root::mcp_server::parse_mcp_args;
use claude_root::self_cmd::{SelfCommand, shell_quote};
use claude_root::watch::parse_watch_args;
use claude_root::worker_root::{claude_root_requested, prepare_claude_root};
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn no_env(_: &str) -> Option<String> {
    None
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

/// `HELP` of ultron-claude-launcher.test.ts (a `claude --help` excerpt).
const CLAUDE_HELP: &str = "Usage: claude [options] [command] [prompt]

Options:
  --mcp-config <configs...>        Load MCP servers from JSON files or strings (space-separated)
  --strict-mcp-config              Only use MCP servers from --mcp-config, ignoring all other MCP configurations
  --tools <tools...>               Specify the list of available tools from the built-in set.
  --allowedTools, --allowed-tools <tools...>  Comma or space-separated list of tool names to allow
  --system-prompt-file <file>      Read system prompt from a file
  --settings <file-or-json>        Path to a settings JSON file or a JSON string to load additional settings from
  --setting-sources <sources>      Comma-separated list of setting sources to load (user, project, local).
  -p, --print                      Print response and exit (useful for pipes).";

fn launch_options() -> LauncherOptions {
    LauncherOptions {
        hooks: true,
        ..LauncherOptions::default()
    }
}

fn build(
    flags: &[&str],
    options: &LauncherOptions,
    var: &dyn Fn(&str) -> Option<String>,
    skip: Vec<String>,
) -> claude_root::launcher::ClaudeLaunch {
    let flags: BTreeSet<String> = flags.iter().map(|f| f.to_string()).collect();
    let self_cmd = SelfCommand {
        command: "node".into(),
        args: strings(&["cli.js"]),
    };
    let resources = PromptResources::default();
    build_claude_launch(&LaunchInput {
        claude: "claude",
        flags: &flags,
        self_cmd: &self_cmd,
        options,
        cwd: "/w",
        dir: Some(Path::new("/tmp/x")),
        socket: "/run/s.sock",
        var,
        date: "2026-09-30".into(),
        platform: "linux",
        resources: &resources,
        bundled_skills: None,
        hints: false,
        skip_permissions: skip,
    })
}

#[test]
fn claude_root_launcher_argument_parsing_and_flag_detection() {
    let parsed = parse_launcher_args(&strings(&["--watch", "--children", "ultron", "--", "--watch"]))
        .unwrap()
        .unwrap();
    assert!(parsed.watch);
    assert_eq!(parsed.children.as_deref(), Some("ultron"));
    assert_eq!(parsed.claude_args, strings(&["--watch"]));
    assert_eq!(parse_launcher_args(&strings(&["--help"])).unwrap(), None);
    assert!(parse_launcher_args(&strings(&["--children", "x"])).is_err());
    let flags = parse_flags(CLAUDE_HELP);
    for flag in [
        "mcp-config",
        "allowedTools",
        "allowed-tools",
        "system-prompt-file",
        "print",
    ] {
        assert!(flags.contains(flag), "{flag}");
    }
    // Without --system-prompt-file support the guide goes inline.
    let var = |k: &str| match k {
        "MCP_TOOL_TIMEOUT" => Some("5".to_string()),
        "CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS" => Some("120000".to_string()),
        _ => None,
    };
    let built = build(&["mcp-config"], &launch_options(), &var, Vec::new());
    assert!(built.args.contains(&"--system-prompt".to_string()));
    assert!(built.env.is_empty());
    let mut files: Vec<String> = built.files.iter().map(|(p, _)| p.display().to_string()).collect();
    files.sort();
    assert_eq!(files, strings(&["/tmp/x/mcp.json", "/tmp/x/settings.json"]));
}

#[test]
fn claude_root_launcher_runs_claude_with_the_repl_as_the_only_tool() {
    let mut options = launch_options();
    options.claude_args = strings(&["-p", "hello", "--model", "sonnet"]);
    let built = build(&["mcp-config", "system-prompt-file"], &options, &no_env, Vec::new());
    let value = |flag: &str| built.args[built.args.iter().position(|a| a == flag).unwrap() + 1].clone();
    assert_eq!(value("--tools"), "");
    assert_eq!(value("--allowedTools"), CLAUDE_RLM_TOOL);
    assert_eq!(value("--setting-sources"), "");
    assert_eq!(value("--system-prompt-file"), "/tmp/x/system-prompt.md");
    assert!(built.args.contains(&"--strict-mcp-config".to_string()));
    // --model given: no default model is added; the claude args come last.
    assert_eq!(built.args.iter().filter(|a| *a == "--model").count(), 1);
    assert_eq!(
        built.args[built.args.len() - 4..],
        strings(&["-p", "hello", "--model", "sonnet"])
    );
    assert_eq!(
        built.env,
        vec![
            ("MCP_TOOL_TIMEOUT".to_string(), "7200000".to_string()),
            ("CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS".to_string(), "0".to_string()),
        ]
    );
    let mcp: Value = serde_json::from_str(&built.files[0].1).unwrap();
    assert_eq!(
        mcp,
        json!({"mcpServers": {"ultron": {"type": "stdio", "command": "node", "args": ["cli.js", "mcp", "--no-instructions", "--socket", "/run/s.sock"]}}})
    );
    let settings: Value = serde_json::from_str(&built.files[1].1).unwrap();
    assert_eq!(settings["permissions"], json!({"allow": [CLAUDE_RLM_TOOL]}));
    assert_eq!(
        settings["hooks"]["UserPromptSubmit"][0]["hooks"][0],
        json!({"type": "command", "command": "node cli.js hook user-prompt --socket /run/s.sock", "timeout": 60})
    );
    assert_eq!(built.watch_command, "node cli.js watch --socket /run/s.sock");
    // --keep-mcp, --keep-settings and --no-hooks.
    let mut kept = launch_options();
    kept.keep_mcp = true;
    kept.keep_settings = true;
    kept.hooks = false;
    let built = build(&["mcp-config"], &kept, &no_env, Vec::new());
    assert!(!built.args.contains(&"--strict-mcp-config".to_string()));
    assert!(!built.args.contains(&"--setting-sources".to_string()));
    let settings: Value = serde_json::from_str(&built.files[1].1).unwrap();
    assert_eq!(settings, json!({"permissions": {"allow": [CLAUDE_RLM_TOOL]}}));
}

#[test]
fn claude_root_skip_permissions_by_default_except_as_root_or_turned_off() {
    let env = |pairs: &'static [(&'static str, &'static str)]| {
        move |k: &str| pairs.iter().find(|(key, _)| *key == k).map(|(_, v)| v.to_string())
    };
    assert_eq!(
        skip_permissions_args(&no_env, Some(1000), None),
        strings(&["--dangerously-skip-permissions"])
    );
    assert!(skip_permissions_args(&env(&[("ULTRON_CLAUDE_SKIP_PERMISSIONS", "off")]), Some(1000), None).is_empty());
    assert!(skip_permissions_args(&env(&[("ULTRON_CLAUDE_SKIP_PERMISSIONS", " NO ")]), Some(1000), None).is_empty());
    // Claude Code refuses the flag as root unless IS_SANDBOX=1.
    assert!(skip_permissions_args(&no_env, Some(0), None).is_empty());
    assert_eq!(
        skip_permissions_args(&env(&[("IS_SANDBOX", "1")]), Some(0), None),
        strings(&["--dangerously-skip-permissions"])
    );
    // The Rust-only setting turns it off; `true` keeps the default.
    assert!(skip_permissions_args(&no_env, Some(1000), Some(false)).is_empty());
    assert_eq!(skip_permissions_args(&no_env, Some(1000), Some(true)).len(), 1);
    let expected = skip_permissions_args(&no_env, Some(1000), None);
    let launch = |flags: &[&str]| build(flags, &launch_options(), &no_env, expected.clone()).args;
    let with = launch(&["mcp-config", "dangerously-skip-permissions"]);
    assert_eq!(
        with.iter()
            .filter(|a| *a == "--dangerously-skip-permissions")
            .cloned()
            .collect::<Vec<_>>(),
        expected
    );
    // A claude without the flag is not given it.
    assert!(!launch(&["mcp-config"]).contains(&"--dangerously-skip-permissions".to_string()));
    // Subagents get it too.
    let child = child_claude_args("m", "{}", "/p", &expected);
    assert_eq!(
        child
            .iter()
            .filter(|a| *a == "--dangerously-skip-permissions")
            .cloned()
            .collect::<Vec<_>>(),
        expected
    );
}

#[test]
fn claude_root_prepare_claude_root_model_selection() {
    let env = |k: &str| (k == "ULTRON_SELF_COMMAND").then(|| r#"["node","cli.js"]"#.to_string());
    let root = prepare_claude_root(None, None, &env).unwrap();
    assert_eq!(
        (root.provider, root.model.as_str()),
        (None, "claude-code/claude-opus-5-5")
    );
    assert_eq!(root.env, vec![("ULTRON_ROOT".to_string(), "claude".to_string())]);
    assert!(claude_root_requested(
        &|k: &str| (k == "ULTRON_ROOT").then(|| " Claude ".to_string())
    ));
    assert!(!claude_root_requested(&no_env));
    let model = |provider: Option<&str>, model: Option<&str>| prepare_claude_root(provider, model, &env);
    assert_eq!(
        model(None, Some("claude-code/haiku")).unwrap().model,
        "claude-code/haiku"
    );
    assert_eq!(model(None, Some("sonnet")).unwrap().model, "claude-code/sonnet");
    let opus = model(Some("claude-code"), Some("opus")).unwrap();
    assert_eq!(
        (opus.provider.as_deref(), opus.model.as_str()),
        (Some("claude-code"), "opus")
    );
    let with_env = |k: &str| match k {
        "ULTRON_CLAUDE_MODEL" => Some("sonnet".to_string()),
        other => env(other),
    };
    assert_eq!(
        prepare_claude_root(None, None, &with_env).unwrap().model,
        "claude-code/sonnet"
    );
    assert!(
        model(None, Some("openai/gpt-5"))
            .unwrap_err()
            .contains("does not serve openai/gpt-5")
    );
    assert!(
        model(None, Some("gpt-5"))
            .unwrap_err()
            .contains("--model claude-code/<model>")
    );
    assert!(
        model(Some("openai"), None)
            .unwrap_err()
            .contains("does not serve openai")
    );
    // The worker needs to know how to start `ultron mcp --bridge`.
    let fresh = prepare_claude_root(None, None, &no_env).unwrap();
    let command = &fresh.env.iter().find(|(k, _)| k == "ULTRON_SELF_COMMAND").unwrap().1;
    let parts: Vec<String> = serde_json::from_str(command).unwrap();
    assert_eq!(parts[0], std::env::current_exe().unwrap().display().to_string());
}

/// The test server of ultron-claude-mcp.test.ts.
struct Fake;

/// Set once a `wait` call is running (its abort signal exists), so a test cancels it only then.
static WAITING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

impl McpHandlers for Fake {
    fn name(&self) -> String {
        "ultron".into()
    }
    fn version(&self) -> String {
        "1".into()
    }
    fn instructions(&self) -> Option<String> {
        Some("GUIDE".into())
    }
    fn tools(&self) -> Vec<Value> {
        vec![json!({"name": "rlm", "description": "d", "inputSchema": {"type": "object"}})]
    }
    fn call_tool(
        &self,
        _: &str,
        args: Map<String, Value>,
        signal: &AbortSignal,
        _: Map<String, Value>,
    ) -> Result<Value, String> {
        if args.get("code") == Some(&json!("wait")) {
            WAITING.store(true, std::sync::atomic::Ordering::SeqCst);
            while !signal.aborted() {
                std::thread::sleep(Duration::from_millis(5));
            }
            return Err("cancelled".into());
        }
        let code = claude_root::mcp_protocol::js_string(args.get("code"));
        Ok(json!({"content": [{"type": "text", "text": format!("ran {code}")}]}))
    }
    fn prompts(&self) -> Option<Vec<McpPrompt>> {
        Some(vec![McpPrompt {
            name: "ultron-guide".into(),
            title: None,
            description: "g".into(),
            text: "GUIDE".into(),
        }])
    }
    fn resources(&self) -> Option<Vec<McpResource>> {
        Some(vec![McpResource {
            uri: "ultron://guide".into(),
            name: "guide".into(),
            title: None,
            description: "g".into(),
            mime_type: "text/markdown".into(),
            text: "GUIDE".into(),
        }])
    }
}

/// An MCP session over a socket pair: write requests, read responses by id.
struct Session {
    input: UnixStream,
    output: BufReader<UnixStream>,
    seen: Vec<Value>,
    done: std::thread::JoinHandle<()>,
}

impl Session {
    fn new() -> Session {
        let (input, server_in) = UnixStream::pair().unwrap();
        let (server_out, output) = UnixStream::pair().unwrap();
        let done =
            std::thread::spawn(move || serve(Arc::new(Fake), &mut BufReader::new(server_in), Box::new(server_out)));
        Session {
            input,
            output: BufReader::new(output),
            seen: Vec::new(),
            done,
        }
    }

    fn line(&mut self, text: &str) {
        writeln!(self.input, "{text}").unwrap();
    }

    fn next(&mut self) -> Value {
        let mut line = String::new();
        self.output.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    fn request(&mut self, id: i64, method: &str, params: Value) -> Value {
        self.line(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string());
        loop {
            if let Some(found) = self.seen.iter().find(|m| m["id"] == id) {
                return found.clone();
            }
            let message = self.next();
            self.seen.push(message);
        }
    }

    fn end(self) {
        self.input.shutdown(std::net::Shutdown::Both).unwrap();
        self.done.join().unwrap();
    }
}

#[test]
fn claude_root_mcp_initialize_tools_prompts_and_resources() {
    let mut session = Session::new();
    let init = session.request(
        1,
        "initialize",
        json!({"protocolVersion": "2025-06-18", "clientInfo": {"name": "claude-code"}}),
    );
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(init["result"]["serverInfo"]["name"], "ultron");
    assert_eq!(init["result"]["instructions"], "GUIDE");
    for capability in ["tools", "prompts", "resources"] {
        assert!(init["result"]["capabilities"][capability].is_object());
    }
    // An unknown version gets the newest one the server speaks.
    let newest = session.request(2, "initialize", json!({"protocolVersion": "1999-01-01"}));
    assert_eq!(newest["result"]["protocolVersion"], "2025-11-25");
    assert_eq!(
        session.request(3, "tools/list", json!({}))["result"]["tools"][0]["name"],
        "rlm"
    );
    assert_eq!(
        session.request(4, "tools/call", json!({"name": "rlm", "arguments": {"code": "1"}}))["result"],
        json!({"content": [{"type": "text", "text": "ran 1"}]})
    );
    assert_eq!(
        session.request(5, "tools/call", json!({"name": "bash", "arguments": {}}))["error"]["code"],
        -32602
    );
    assert_eq!(
        session.request(6, "prompts/get", json!({"name": "ultron-guide"}))["result"]["messages"],
        json!([{"role": "user", "content": {"type": "text", "text": "GUIDE"}}])
    );
    let read = session.request(7, "resources/read", json!({"uri": "ultron://guide"}));
    assert_eq!(read["result"]["contents"][0]["uri"], "ultron://guide");
    assert_eq!(read["result"]["contents"][0]["text"], "GUIDE");
    assert_eq!(session.request(8, "no/such", json!({}))["error"]["code"], -32601);
    session.end();
}

#[test]
fn claude_root_mcp_cancelled_notification_aborts_the_call() {
    let mut session = Session::new();
    session.line(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "rlm", "arguments": {"code": "wait"}}}).to_string());
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    while !WAITING.load(std::sync::atomic::Ordering::SeqCst) {
        assert!(std::time::Instant::now() < deadline, "the call never started");
        std::thread::sleep(Duration::from_millis(5));
    }
    session
        .line(&json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 1}}).to_string());
    let cancelled = session.next();
    assert_eq!(cancelled["id"], 1);
    assert_eq!(cancelled["error"]["message"], "cancelled");
    session.line("not json");
    let parse = session.next();
    assert_eq!(parse["id"], Value::Null);
    assert_eq!(parse["error"]["code"], -32700);
    session.end();
}

#[test]
fn claude_root_mcp_server_flags_are_checked() {
    let parsed = parse_mcp_args(&strings(&["--no-instructions", "--frame-model", "claude-code/haiku"])).unwrap();
    assert!(!parsed.instructions);
    assert_eq!(parsed.frame_model.as_deref(), Some("claude-code/haiku"));
    assert!(
        parse_mcp_args(&strings(&["--bogus"]))
            .unwrap_err()
            .0
            .contains("unknown option")
    );
    assert!(
        parse_mcp_args(&strings(&["--child"]))
            .unwrap_err()
            .0
            .contains("--parent-socket")
    );
    assert!(
        parse_mcp_args(&strings(&["--children", "gpt"]))
            .unwrap_err()
            .0
            .contains("claude or ultron")
    );
    assert!(
        parse_watch_args(&strings(&["--nope"]))
            .unwrap_err()
            .contains("unknown option")
    );
    // JS Number(): blanks are 0, hex works, fractions are refused.
    assert_eq!(parse_mcp_args(&strings(&["--level", " "])).unwrap().level, 0);
    assert_eq!(
        parse_mcp_args(&strings(&["--allowance", "0x10"])).unwrap().allowance,
        16
    );
    assert!(parse_mcp_args(&strings(&["--level", "1.5"])).is_err());
}

#[test]
fn claude_root_control_socket_owner_only_registry_requests_and_hooks() {
    use std::os::unix::fs::PermissionsExt;
    let runtime = tempfile::Builder::new().prefix("ucr-").tempdir_in("/tmp").unwrap();
    let runtime_dir = runtime.path().display().to_string();
    let var = move |k: &str| (k == "XDG_RUNTIME_DIR").then(|| runtime_dir.clone());
    let dir = control_dir(&var).unwrap();
    assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
    let record = ServerRecord {
        name: "sess-a".into(),
        pid: std::process::id(),
        socket: dir.join("sess-a.sock").display().to_string(),
        cwd: "/work".into(),
        started_at: 1,
        claude_session_id: Some("sess-a".into()),
        claude_pid: None,
        session_id: None,
        parent: None,
    };
    let server = start_control_server(
        &record,
        Arc::new(|request: Map<String, Value>| {
            if request["op"] == "fail" {
                return Err("nope".into());
            }
            Ok(json!({"echo": request["op"]}))
        }),
        &dir,
    )
    .unwrap();
    assert_eq!(
        std::fs::metadata(&record.socket).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(dir.join("sess-a.json")).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let by_session = ServerQuery {
        claude_session_id: Some("sess-a".into()),
        ..ServerQuery::default()
    };
    assert_eq!(find_server(&by_session, &dir).unwrap().socket, record.socket);
    let by_cwd = ServerQuery {
        cwd: Some("/work".into()),
        ..ServerQuery::default()
    };
    assert_eq!(find_server(&by_cwd, &dir).unwrap().name, "sess-a");
    let mut client = ControlClient::connect(Path::new(&record.socket), Duration::ZERO).unwrap();
    assert_eq!(
        client.request("status", Map::new(), Duration::from_secs(120)).unwrap(),
        json!({"echo": "status"})
    );
    assert_eq!(
        client
            .request("fail", Map::new(), Duration::from_secs(120))
            .unwrap_err(),
        "nope"
    );
    // A hook finds the server by the session id in its input and prints the server's answer.
    let output = claude_root::hooks::run_hook("stop", None, r#"{"session_id":"sess-a"}"#, &var);
    assert_eq!(serde_json::from_str::<Value>(&output).unwrap(), json!({"echo": "hook"}));
    server.close();
    assert!(list_servers(&dir).is_empty());
    // Without a server, hooks are quiet.
    let socket = Some(std::path::PathBuf::from(&record.socket));
    assert_eq!(claude_root::hooks::run_hook("stop", socket, "{}", &var), "");
    assert_eq!(claude_root::hooks::run_hook("bogus", None, "{}", &var), "");
}

#[test]
fn claude_root_registry_drops_dead_servers() {
    let dir = tempfile::tempdir().unwrap();
    let dead = json!({"name": "gone", "pid": 999_999_999u64, "socket": dir.path().join("gone.sock").display().to_string(), "cwd": "/w", "startedAt": 5});
    std::fs::write(dir.path().join("gone.json"), dead.to_string()).unwrap();
    std::fs::write(dir.path().join("gone.sock"), "").unwrap();
    let alive = |name: &str, started: i64, parent: Option<&str>| {
        let mut record = json!({"name": name, "pid": std::process::id(), "socket": format!("/s/{name}"), "cwd": "/w", "startedAt": started});
        if let Some(parent) = parent {
            record["parent"] = json!(parent);
        }
        std::fs::write(dir.path().join(format!("{name}.json")), record.to_string()).unwrap();
    };
    alive("old", 1, None);
    alive("new", 9, None);
    alive("kid", 20, Some("new"));
    let names: Vec<String> = list_servers(dir.path()).into_iter().map(|s| s.name).collect();
    assert_eq!(names, strings(&["kid", "new", "old"]));
    assert!(!dir.path().join("gone.json").exists() && !dir.path().join("gone.sock").exists());
    // A subagent server is never the root of a directory.
    let query = ServerQuery {
        cwd: Some("/w".into()),
        ..ServerQuery::default()
    };
    assert_eq!(find_server(&query, dir.path()).unwrap().name, "new");
}

/// The external root of ultron-claude-host.test.ts over fake events and a fake cell runner.
type Shared = Arc<Mutex<Vec<String>>>;

fn controller() -> (ExternalRoot, Shared, Shared) {
    let waiting: Shared = Arc::default();
    let calls: Shared = Arc::default();
    let source = waiting.clone();
    let events: EventSource = Arc::new(move || std::mem::take(&mut *source.lock().unwrap()));
    let seen = calls.clone();
    let runner: CellRunner = Arc::new(move |code: &str, _: &AbortSignal| {
        seen.lock().unwrap().push(code.to_string());
        if code == "raise" {
            return Err("ZeroDivisionError: division by zero".into());
        }
        Ok(vec![json!({"type": "text", "text": format!("ran {code}")})])
    });
    (
        ExternalRoot::new(events, runner, Some("LOKI policy".into())),
        waiting,
        calls,
    )
}

const EVENT: &str = r#"<runtime_event kind="job_done" id="job-1" status="completed" summary="exit 0" fetch="await rlm.job(&quot;job-1&quot;)" />"#;

#[test]
fn claude_root_external_root_waiting_events_lead_the_next_result() {
    let (root, waiting, calls) = controller();
    waiting.lock().unwrap().push(EVENT.into());
    let signal = AbortSignal::new();
    let first = root.run_cell("x = 1", &signal);
    assert_eq!(
        first.content,
        vec![json!({"type": "text", "text": format!("{EVENT}\nran x = 1")})]
    );
    assert!(!first.is_error);
    assert_eq!(
        root.run_cell("x", &signal).content[0],
        json!({"type": "text", "text": "ran x"})
    );
    let failed = root.run_cell("raise", &signal);
    assert!(failed.is_error);
    assert!(failed.content[0]["text"].as_str().unwrap().contains("ZeroDivision"));
    assert_eq!(*calls.lock().unwrap(), strings(&["x = 1", "x", "raise"]));
    assert_eq!(root.cells(), 3);
}

#[test]
fn claude_root_external_root_user_turn_carries_events_and_stop_closes_it() {
    let (root, waiting, _) = controller();
    assert_eq!(root.session_context().as_deref(), Some("LOKI policy"));
    waiting.lock().unwrap().push(EVENT.into());
    assert_eq!(root.pending_events(), 1);
    let context = root.begin_turn().unwrap();
    assert_eq!(context, format!("Runtime events since your last rlm call:\n{EVENT}"));
    let turn = root.turn().unwrap();
    assert!(turn.starts_with("cc-") && turn.ends_with("-1"), "{turn}");
    root.end_turn();
    assert_eq!(root.turn(), None);
    // A turn without events adds no context; turns are numbered.
    assert_eq!(root.begin_turn(), None);
    assert!(root.turn().unwrap().ends_with("-2"));
    // A cell opens a turn when no prompt did.
    root.end_turn();
    root.run_cell("x", &AbortSignal::new());
    assert!(root.turn().unwrap().ends_with("-3"));
}

#[test]
fn claude_root_external_root_inbox_is_bounded_with_a_count() {
    let (root, waiting, _) = controller();
    waiting
        .lock()
        .unwrap()
        .extend((0..MAX_INBOX + 3).map(|i| format!("e{i}")));
    let taken = root.take_events().unwrap();
    let lines: Vec<&str> = taken.lines().collect();
    assert_eq!(lines.len(), MAX_INBOX + 1);
    assert_eq!(lines[0], "e3");
    assert_eq!(
        lines[MAX_INBOX],
        "[3 older runtime event(s) dropped; `await agents.status()` lists all work]"
    );
    assert_eq!(root.take_events(), None);
}

#[test]
fn claude_root_child_models_map_to_claude_code_aliases() {
    assert_eq!(child_claude_model(None, "sonnet").unwrap(), "sonnet");
    assert_eq!(child_claude_model(Some("claude-code/opus"), "sonnet").unwrap(), "opus");
    assert_eq!(child_claude_model(Some("haiku"), "sonnet").unwrap(), "haiku");
    assert!(
        child_claude_model(Some("openai/gpt-5"), "sonnet")
            .unwrap_err()
            .contains("Claude Code processes")
    );
}

#[test]
fn claude_root_child_has_only_the_repl_tool_and_its_own_prompt() {
    let args = child_claude_args("sonnet", "{}", "/tmp/p.md", &[]);
    let value = |flag: &str| args[args.iter().position(|a| a == flag).unwrap() + 1].clone();
    assert_eq!(args[0], "-p");
    assert_eq!(value("--tools"), "");
    assert_eq!(value("--allowedTools"), CLAUDE_RLM_TOOL);
    assert_eq!(value("--setting-sources"), "");
    assert_eq!(value("--system-prompt-file"), "/tmp/p.md");
    assert!(args.contains(&"--strict-mcp-config".to_string()));
    assert!(args.contains(&"--no-session-persistence".to_string()));
}

#[test]
fn claude_root_stream_json_turns_count_responses_not_blocks() {
    let mut stats = StreamStats::default();
    let assistant =
        |id: &str, part: Value| json!({"type": "assistant", "message": {"id": id, "content": [part]}}).to_string();
    fold_stream_line(
        &json!({"type": "system", "subtype": "init", "model": "claude-sonnet-5"}).to_string(),
        &mut stats,
    );
    fold_stream_line(
        &assistant("m1", json!({"type": "text", "text": "Let me look."})),
        &mut stats,
    );
    fold_stream_line(
        &assistant(
            "m1",
            json!({"type": "tool_use", "id": "t1", "name": CLAUDE_RLM_TOOL, "input": {}}),
        ),
        &mut stats,
    );
    fold_stream_line(&assistant("m2", json!({"type": "text", "text": "Done."})), &mut stats);
    let result = fold_stream_line(
        &json!({"type": "result", "result": "Done.", "num_turns": 2}).to_string(),
        &mut stats,
    );
    assert_eq!((stats.turns, stats.tool_calls, stats.text.as_str()), (2, 1, "Done."));
    assert_eq!(stats.model.as_deref(), Some("claude-code/claude-sonnet-5"));
    let result = result.unwrap();
    assert_eq!(result["result"], "Done.");
    // The task result: usage summed over the cache fields, cost and wall time kept.
    let mut final_message = result.clone();
    final_message.insert(
        "usage".into(),
        json!({"input_tokens": 10, "cache_creation_input_tokens": 5, "cache_read_input_tokens": 100, "output_tokens": 7}),
    );
    final_message.insert("total_cost_usd".into(), json!(0.25));
    let done = child_result(Some(&final_message), &stats, Some(0), "").unwrap();
    assert_eq!(
        (done.text.as_str(), done.turns, done.input_tokens, done.output_tokens),
        ("Done.", 2, 115.0, 7.0)
    );
    assert_eq!(done.cost, Some(0.25));
    assert_eq!(
        child_result(None, &stats, Some(3), " boom \n").unwrap_err(),
        "claude exited with code 3 without a result: boom"
    );
    let failed: Map<String, Value> =
        serde_json::from_value(json!({"type": "result", "is_error": true, "subtype": "error_max_turns"})).unwrap();
    let empty = StreamStats::default();
    assert_eq!(
        child_result(Some(&failed), &empty, Some(1), "").unwrap_err(),
        "claude child failed: error_max_turns"
    );
}

#[test]
fn claude_root_guide_wording_for_a_root_nothing_can_wake() {
    let guide = claude_runtime_guide(false);
    assert!(guide.contains("at the top of your next rlm result"));
    assert!(guide.contains("Nothing wakes you after you reply"));
    assert!(!guide.contains("starts a new turn"));
    assert!(!guide.contains("or end your turn: each end arrives"));
    assert!(!guide.contains("`ctx` edits"));
    let native = runtime_guide(Audience::Ultron, &GuideOptions::default(), false);
    assert!(native.contains("starts a new turn if yours ended"));
    assert!(native.contains("`ctx` edits"));
    assert!(native.contains("Pre-imported"));
    let prompt = claude_system_prompt(
        Audience::Claude,
        &GuideOptions {
            cwd: Some("/work".into()),
            platform: Some("linux".into()),
            date: Some("2026-09-30".into()),
            ..GuideOptions::default()
        },
        false,
    );
    assert!(prompt.contains(CLAUDE_RLM_TOOL));
    assert!(prompt.contains("destructive"));
    assert!(prompt.contains("Never commit or push unless asked"));
    assert!(prompt.ends_with("# Environment\n- Working directory: /work\n- Platform: linux\n- Date: 2026-09-30"));
    let child = claude_system_prompt(
        Audience::ClaudeChild,
        &GuideOptions {
            allowance: Some(0),
            ..GuideOptions::default()
        },
        false,
    );
    assert!(child.contains("rlm.finish"));
    assert!(child.contains("`rlm.spawn` is refused for you"));
    let one = claude_system_prompt(
        Audience::ClaudeChild,
        &GuideOptions {
            allowance: Some(1),
            ..GuideOptions::default()
        },
        false,
    );
    assert!(one.contains("up to 1 more level (`depth=` for your children at most 0)"));
}

#[test]
fn claude_root_shell_quote_matches_self_ts() {
    assert_eq!(shell_quote("/usr/bin/node"), "/usr/bin/node");
    assert_eq!(shell_quote("a b"), "'a b'");
    assert_eq!(shell_quote("it's"), "'it'\\''s'");
    assert_eq!(shell_quote(""), "''");
    assert_eq!(shell_quote("k=v,x@y%z+1:2"), "k=v,x@y%z+1:2");
}
