//! `ultron mcp add|remove|list|help` (`extensions/mcp/cli.ts`): add, remove and check the MCP servers Ultron connects
//! to, outside a session, and sign in to OAuth servers (`login`/`logout`, crate::mcp_login).
//!
//! `ultron mcp` with none of these subcommands is Ultron's own stdio MCP server for Claude Code, a different program.

use crate::Env;
use mcp::config::{LoadedConfig, Scope, ServerEntry, load_config, tool_exposure, validate_server_config};
use mcp::runtime::{TransportSpec, transport_spec};
use mcp::{Client, ClientOptions, ConnState, RequestOptions, Transport};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

const MANAGEMENT_COMMANDS: [&str; 6] = ["add", "remove", "list", "login", "logout", "help"];
const HELP_HINT: &str = "Use \"ultron mcp --help\" for usage.";
const STDERR_TAIL_CHARS: usize = 2_000;
const DEFAULT_TIMEOUT_SECONDS: f64 = 60.0;

const HELP: &str = r#"Usage:
  ultron mcp add <server> [options] -- <command> [args...]
  ultron mcp add <server> [options] --url <url>
  ultron mcp remove <server> [-l]
  ultron mcp list [--json]
  ultron mcp login <server> [--timeout <seconds>]
  ultron mcp logout <server>

Configure and check the MCP servers Ultron connects to, and sign in to OAuth servers, without starting a
session. Reads ~/.ultron/agent/mcp.json and, in trusted projects, .pi/mcp.json. In a session the
servers are reached from Python (await mcp.servers(), mcp.tools(server), mcp.call(tool, ...)) and managed with /mcp.

"ultron mcp" with none of these commands is something else: it serves Ultron's own tools as a stdio MCP
server, which "ultron claude" hands to Claude Code. It is not meant to be run by hand.

Commands:
  add <server>            Add or replace a server in mcp.json
  remove <server>         Remove a server from mcp.json
  list                    Show state, tools, and errors (exits 1 on failure)
  login <server>          Sign in through the browser
  logout <server>         Delete the stored OAuth credentials

Options for add and remove:
  -l, --local             Use .pi/mcp.json in the current project instead of the global file

Options for add:
  --url <url>             Streamable HTTP server URL (instead of a command)
  --env <KEY=VALUE>       Environment variable for a stdio server (repeatable)
  --cwd <dir>             Working directory for a stdio server
  --header <KEY=VALUE>    HTTP header (repeatable)
  --bearer-token-env-var <NAME>
                          Send "Authorization: Bearer ${NAME}"
  --oauth-client-id <id>  Pre-registered OAuth client id
  --oauth-client-secret <secret>
                          OAuth client secret (may be ${NAME} or !command)
  --oauth-callback-port <port>
                          Fixed OAuth callback port
  --oauth-client-name <name>
                          Client name sent when registering with the OAuth server
  --exposure <mode>       hidden makes the server's tools unreachable; Pi's other modes are accepted and ignored
  --description <text>    What the server offers, shown by mcp.servers()

Other options:
  --json                  Print the list as JSON
  --timeout <seconds>     How long login waits for the browser (default: 300)"#;

/// `isMcpManagementCommand`.
pub fn is_management_command(first: Option<&str>) -> bool {
    first.is_some_and(|f| MANAGEMENT_COMMANDS.contains(&f) || f == "--help" || f == "-h")
}

#[derive(Clone, Copy, PartialEq)]
enum OptionKind {
    Flag,
    Value,
    List,
}

#[derive(Default)]
struct Parsed {
    positional: Vec<String>,
    values: HashMap<String, Option<String>>,
    lists: HashMap<String, Vec<String>>,
}

struct Io<'a> {
    out: &'a mut dyn std::io::Write,
    err: &'a mut dyn std::io::Write,
}

impl Io<'_> {
    fn log(&mut self, line: &str) {
        let _ = writeln!(self.out, "{line}");
    }
    fn error(&mut self, line: &str) {
        let _ = writeln!(self.err, "{line}");
    }
}

fn parse_options(args: &[String], known: &[(&str, OptionKind)], io: &mut Io, max_positionals: usize) -> Option<Parsed> {
    let mut parsed = Parsed::default();
    let mut index = 0;
    while index < args.len() {
        let arg = if args[index] == "-l" {
            "--local"
        } else {
            args[index].as_str()
        };
        if arg == "--" || parsed.positional.len() >= max_positionals {
            let from = if arg == "--" { index + 1 } else { index };
            parsed.positional.extend(args[from..].iter().cloned());
            break;
        }
        let Some(name) = arg.strip_prefix("--") else {
            parsed.positional.push(arg.to_string());
            index += 1;
            continue;
        };
        let Some((_, kind)) = known.iter().find(|(k, _)| *k == name) else {
            io.error(&format!("Unknown option {arg}.\n{HELP_HINT}"));
            return None;
        };
        if *kind == OptionKind::Flag {
            parsed.values.insert(name.to_string(), None);
            index += 1;
            continue;
        }
        index += 1;
        let Some(value) = args.get(index) else {
            io.error(&format!("{arg} needs a value."));
            return None;
        };
        if *kind == OptionKind::List {
            parsed.lists.entry(name.to_string()).or_default().push(value.clone());
        } else {
            parsed.values.insert(name.to_string(), Some(value.clone()));
        }
        index += 1;
    }
    Some(parsed)
}

/// Handle `mcp <management command>`; None when `argv` is not one.
pub fn run(argv: &[String], env: &Env, out: &mut dyn std::io::Write, err: &mut dyn std::io::Write) -> Option<i32> {
    if argv.first().map(String::as_str) != Some("mcp") || !is_management_command(argv.get(1).map(String::as_str)) {
        return None;
    }
    let mut io = Io { out, err };
    let args = &argv[1..];
    let command = args[0].as_str();
    if command == "help" || args.iter().any(|a| a == "--help" || a == "-h") {
        io.log(HELP);
        return Some(0);
    }
    let agent_dir = env.agent_dir();
    let project_config = env.cwd.join(mcp::config::CONFIG_DIR_NAME).join("mcp.json");
    let rest = &args[1..];
    match command {
        "add" => return Some(add(rest, &project_config, env, &agent_dir, &mut io)),
        "remove" => return Some(remove(rest, &project_config, env, &agent_dir, &mut io)),
        _ => {}
    }
    let trusted = config::trust::ProjectTrustStore::new(&agent_dir)
        .get(&env.cwd)
        .ok()
        .flatten()
        == Some(true);
    let loaded = load_config(&agent_dir, &env.cwd, trusted);
    let untrusted_note = (!trusted && project_config.exists()).then(|| {
        format!(
            "{} is ignored because the project is not trusted. Start ultron in the project to trust it.",
            project_config.display()
        )
    });
    match command {
        "list" => {
            let Some(parsed) = parse_options(rest, &[("json", OptionKind::Flag)], &mut io, usize::MAX) else {
                return Some(1);
            };
            if !parsed.positional.is_empty() {
                io.error(&format!("Usage: ultron mcp list [--json]\n{HELP_HINT}"));
                return Some(1);
            }
            Some(list(
                &loaded,
                parsed.values.contains_key("json"),
                untrusted_note,
                env,
                &agent_dir,
                &mut io,
            ))
        }
        _ => {
            let known: &[(&str, OptionKind)] = if command == "login" {
                &[("timeout", OptionKind::Value)]
            } else {
                &[]
            };
            let Some(parsed) = parse_options(rest, known, &mut io, usize::MAX) else {
                return Some(1);
            };
            let (Some(name), 1) = (parsed.positional.first(), parsed.positional.len()) else {
                io.error(&format!("Usage: ultron mcp {command} <server>\n{HELP_HINT}"));
                return Some(1);
            };
            let Some(entry) = loaded.servers.iter().find(|s| &s.name == name) else {
                let configured: Vec<&str> = loaded.servers.iter().map(|s| s.name.as_str()).collect();
                io.error(&format!(
                    "No MCP server named \"{name}\".{} Configured: {}.",
                    untrusted_note.map_or(String::new(), |n| format!(" {n}")),
                    if configured.is_empty() {
                        "none".to_string()
                    } else {
                        configured.join(", ")
                    }
                ));
                return Some(1);
            };
            let connection =
                mcp::connection::Connection::new(entry.clone(), crate::mcp_login::connection_env(env, &agent_dir));
            let Some(url) = connection.oauth_url() else {
                io.error(&format!(
                    "MCP server \"{name}\" does not use OAuth. Only HTTP servers without an Authorization header do."
                ));
                return Some(1);
            };
            let outcome = if command == "logout" {
                crate::mcp_login::logout(entry, &url, &agent_dir)
            } else {
                let timeout = parsed.values.get("timeout").cloned().flatten();
                match crate::mcp_login::login_timeout(timeout.as_deref()) {
                    Ok(seconds) => crate::mcp_login::login(&connection, &url, seconds, env),
                    Err(message) => crate::mcp_login::Outcome {
                        err: vec![message],
                        code: 1,
                        ..Default::default()
                    },
                }
            };
            connection.close();
            for line in &outcome.out {
                io.log(line);
            }
            for line in &outcome.err {
                io.error(line);
            }
            Some(outcome.code)
        }
    }
}

fn parse_pairs(option: &str, pairs: Option<&Vec<String>>, io: &mut Io) -> Option<Map<String, Value>> {
    let mut record = Map::new();
    for pair in pairs.into_iter().flatten() {
        match pair.find('=') {
            Some(at) if at > 0 => {
                record.insert(pair[..at].to_string(), Value::String(pair[at + 1..].to_string()));
            }
            _ => {
                io.error(&format!("--{option} expects KEY=VALUE, got \"{pair}\"."));
                return None;
            }
        }
    }
    Some(record)
}

/// `Number(text)` as JSON.
fn js_number_value(text: &str) -> Value {
    let t = crate::js::trim(text);
    let parsed = if t.is_empty() {
        Some(0.0)
    } else {
        t.parse::<f64>().ok().filter(|n| n.is_finite())
    };
    match parsed {
        Some(n) => crate::js::num(n),
        // NaN serializes as null.
        None => Value::Null,
    }
}

fn add(args: &[String], project_config: &Path, env: &Env, agent_dir: &Path, io: &mut Io) -> i32 {
    use OptionKind::{Flag, List, Value as V};
    let usage = format!("Usage: ultron mcp add <server> [options] (--url <url> | -- <command> [args...])\n{HELP_HINT}");
    let known = [
        ("local", Flag),
        ("url", V),
        ("env", List),
        ("cwd", V),
        ("header", List),
        ("bearer-token-env-var", V),
        ("oauth-client-id", V),
        ("oauth-client-secret", V),
        ("oauth-callback-port", V),
        ("oauth-client-name", V),
        ("exposure", V),
        ("description", V),
    ];
    let Some(parsed) = parse_options(args, &known, io, 2) else {
        return 1;
    };
    let value = |k: &str| parsed.values.get(k).cloned().flatten();
    let has = |k: &str| parsed.values.contains_key(k) || parsed.lists.contains_key(k);
    let (name, command) = match parsed.positional.split_first() {
        Some((name, command)) => (name.clone(), command.to_vec()),
        None => (String::new(), Vec::new()),
    };
    let url = value("url");
    if name.is_empty() || url.is_none() == command.is_empty() {
        io.error(&usage);
        return 1;
    }
    let http_only = [
        "header",
        "bearer-token-env-var",
        "oauth-client-id",
        "oauth-client-secret",
        "oauth-callback-port",
        "oauth-client-name",
    ];
    let stdio_only = ["env", "cwd"];
    let misplaced = if url.is_none() { &http_only[..] } else { &stdio_only[..] }
        .iter()
        .find(|o| has(o));
    if let Some(option) = misplaced {
        io.error(&format!(
            "--{option} only applies to {}.",
            if url.is_none() {
                "HTTP servers (--url)"
            } else {
                "stdio servers"
            }
        ));
        return 1;
    }
    let mut config = Map::new();
    if let Some(url) = &url {
        let Some(mut headers) = parse_pairs("header", parsed.lists.get("header"), io) else {
            return 1;
        };
        if let Some(bearer) = value("bearer-token-env-var") {
            headers.insert("Authorization".into(), Value::String(format!("Bearer ${{{bearer}}}")));
        }
        let mut oauth = Map::new();
        if let Some(v) = value("oauth-client-id") {
            oauth.insert("clientId".into(), v.into());
        }
        if let Some(v) = value("oauth-client-secret") {
            oauth.insert("clientSecret".into(), v.into());
        }
        if let Some(v) = value("oauth-callback-port") {
            oauth.insert("callbackPort".into(), js_number_value(&v));
        }
        if let Some(v) = value("oauth-client-name") {
            oauth.insert("clientName".into(), v.into());
        }
        config.insert("url".into(), Value::String(url.clone()));
        if !headers.is_empty() {
            config.insert("headers".into(), Value::Object(headers));
        }
        if !oauth.is_empty() {
            config.insert("oauth".into(), Value::Object(oauth));
        }
    } else {
        let Some(env_pairs) = parse_pairs("env", parsed.lists.get("env"), io) else {
            return 1;
        };
        config.insert("command".into(), Value::String(command[0].clone()));
        if command.len() > 1 {
            config.insert("args".into(), json!(command[1..]));
        }
        if !env_pairs.is_empty() {
            config.insert("env".into(), Value::Object(env_pairs));
        }
        if let Some(cwd) = value("cwd") {
            config.insert("cwd".into(), cwd.into());
        }
    }
    if let Some(exposure) = value("exposure") {
        config.insert("exposure".into(), exposure.into());
    }
    if let Some(description) = value("description") {
        config.insert("description".into(), description.into());
    }
    let validated = match validate_server_config(&name, &Value::Object(config)) {
        Ok(v) => v,
        Err(message) => {
            io.error(&message);
            return 1;
        }
    };
    let project = parsed.values.contains_key("local");
    let path = if project {
        project_config.to_path_buf()
    } else {
        agent_dir.join("mcp.json")
    };
    let scope = if project { "project" } else { "global" };
    let may_need_sign_in = validated.contains_key("url")
        && !validated
            .get("headers")
            .and_then(Value::as_object)
            .is_some_and(|h| h.keys().any(|k| k.eq_ignore_ascii_case("authorization")));
    let replaced = match mcp::edit::add_server_config(&path, &name, validated) {
        Ok(r) => r,
        Err(message) => {
            io.error(&format!("Could not update {}: {message}", path.display()));
            return 1;
        }
    };
    io.log(&format!(
        "{} {scope} MCP server \"{name}\" in {}.",
        if replaced { "Replaced" } else { "Added" },
        path.display()
    ));
    if project
        && config::trust::ProjectTrustStore::new(agent_dir)
            .get(&env.cwd)
            .ok()
            .flatten()
            != Some(true)
    {
        io.log(&format!(
            "The project is not trusted, so {} is ignored until you start ultron in the project and trust it.",
            path.display()
        ));
    }
    io.log(&format!(
        "Check it with: ultron mcp list{}",
        if may_need_sign_in {
            format!(". If it requires sign-in: ultron mcp login {name}")
        } else {
            String::new()
        }
    ));
    0
}

fn remove(args: &[String], project_config: &Path, env: &Env, agent_dir: &Path, io: &mut Io) -> i32 {
    let Some(parsed) = parse_options(args, &[("local", OptionKind::Flag)], io, usize::MAX) else {
        return 1;
    };
    let (Some(name), 1) = (parsed.positional.first(), parsed.positional.len()) else {
        io.error(&format!("Usage: ultron mcp remove <server> [-l]\n{HELP_HINT}"));
        return 1;
    };
    let project = parsed.values.contains_key("local");
    let path = if project {
        project_config.to_path_buf()
    } else {
        agent_dir.join("mcp.json")
    };
    let scope = if project { Scope::Project } else { Scope::Global };
    match mcp::edit::remove_server_config(&path, name) {
        Ok(true) => {
            io.log(&format!(
                "Removed {} MCP server \"{name}\" from {}.",
                scope.as_str(),
                path.display()
            ));
            0
        }
        Ok(false) => {
            let loaded = load_config(agent_dir, &env.cwd, true);
            let other = loaded.servers.iter().find(|s| &s.name == name && s.scope != scope);
            let hint = other.map_or(String::new(), |o| {
                format!(
                    " It is defined in {}{}.",
                    o.source,
                    if o.scope == Scope::Project {
                        "; use --local"
                    } else {
                        "; omit --local"
                    }
                )
            });
            io.error(&format!(
                "No {} MCP server named \"{name}\" in {}.{hint}",
                scope.as_str(),
                path.display()
            ));
            1
        }
        Err(message) => {
            io.error(&format!("Could not update {}: {message}", path.display()));
            1
        }
    }
}

/// What one connection attempt found (`McpServerConnection` after `getClient`).
struct Connection {
    state: &'static str,
    tools: Vec<String>,
    resources: Option<(usize, usize)>,
    error: Option<String>,
}

fn is_app_resource(item: &Value) -> bool {
    let uri = item
        .get("uri")
        .or_else(|| item.get("uriTemplate"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let mime = item
        .get("mimeType")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let profile = mime.split(';').skip(1).any(|part| {
        let part = part.trim();
        part.strip_prefix("profile")
            .map(str::trim_start)
            .and_then(|p| p.strip_prefix('='))
            .map(|p| p.trim().trim_matches('"'))
            == Some("mcp-app")
    });
    uri.starts_with("ui://") || profile
}

fn file_url(path: &Path) -> String {
    let mut out = String::from("file://");
    for byte in path.to_string_lossy().bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._~!$&'()*+,;=:@".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// An OAuth server, connected with its stored credentials (`createConnection(entry, options, credentials)`): signed
/// in, it lists its tools; otherwise it needs sign-in.
fn connect_oauth(entry: &ServerEntry, env: &Env) -> Connection {
    let connection =
        mcp::connection::Connection::new(entry.clone(), crate::mcp_login::connection_env(env, &env.agent_dir()));
    let report = match connection.get_client() {
        Ok(_) => Connection {
            state: "connected",
            tools: connection
                .tools()
                .iter()
                .filter_map(|t| t.get("name").and_then(Value::as_str).map(str::to_string))
                .collect(),
            // `connection.hasResources`: the counts listed at connect, zero included.
            resources: connection.listed_resource_counts(),
            error: None,
        },
        Err(message) => Connection {
            state: connection.state().as_str(),
            tools: Vec::new(),
            resources: None,
            // A server that needs sign-in reports no error (the list says how to sign in).
            error: match connection.state() {
                mcp::connection::ServerState::NeedsAuth => None,
                _ => connection.error().or(Some(message)),
            },
        },
    };
    connection.close();
    report
}

fn connect(entry: &ServerEntry, env: &Env) -> Connection {
    if mcp::connection::Connection::new(entry.clone(), Default::default())
        .oauth_url()
        .is_some()
    {
        return connect_oauth(entry, env);
    }
    let failed = |error: String| Connection {
        state: "failed",
        tools: Vec::new(),
        resources: None,
        error: Some(error),
    };
    let timeout = entry
        .config
        .get("timeout")
        .and_then(Value::as_f64)
        .unwrap_or(DEFAULT_TIMEOUT_SECONDS);
    let client = Client::new(ClientOptions {
        name: "ultron".into(),
        version: crate::version(),
        request_timeout: Some(Duration::from_secs_f64((timeout * 1000.0).max(0.0) / 1000.0)),
        roots: Some(vec![json!({
            "uri": file_url(&env.cwd),
            "name": env.cwd.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        })]),
        ..ClientOptions::default()
    });
    let spec = match transport_spec(&entry.name, &entry.config, &env.cwd, &env.home, &HashMap::new()) {
        Ok((spec, _)) => spec,
        Err(message) => return failed(message),
    };
    let (transport, stdio): (Arc<dyn Transport>, Option<Arc<mcp::StdioTransport>>) = match spec {
        TransportSpec::Stdio(options) => {
            let t = Arc::new(mcp::StdioTransport::new(options));
            (t.clone(), Some(t))
        }
        TransportSpec::Http(options) => (Arc::new(mcp::http::HttpTransport::new(options)), None),
    };
    let outcome = (|| -> Result<Connection, mcp::Error> {
        client.connect(transport)?;
        let capabilities = client.server_capabilities().unwrap_or(Value::Null);
        let tools = if capabilities.get("tools").is_some_and(|t| !t.is_null()) {
            client.list_tools(RequestOptions::default())?
        } else {
            Vec::new()
        };
        let has_resources = capabilities.get("resources").is_some();
        let resources = has_resources.then(|| {
            let resources = client.list_resources(RequestOptions::default()).unwrap_or_default();
            let templates = client
                .list_resource_templates(RequestOptions::default())
                .unwrap_or_default();
            (
                resources.iter().filter(|r| !is_app_resource(r)).count(),
                templates.iter().filter(|r| !is_app_resource(r)).count(),
            )
        });
        if client.connection_state() != ConnState::Connected {
            return Err(mcp::Error::Other("connection closed during setup".into()));
        }
        Ok(Connection {
            state: "connected",
            tools: tools
                .iter()
                .filter_map(|t| t.get("name").and_then(Value::as_str).map(str::to_string))
                .collect(),
            resources,
            error: None,
        })
    })();
    client.close();
    match outcome {
        Ok(connection) => connection,
        Err(error) => {
            let tail = stdio.map(|s| s.stderr()).map(|s| {
                let trimmed = s.trim().to_string();
                let len = crate::js::len16(&trimmed);
                crate::js::tail16(&trimmed, len.saturating_sub(STDERR_TAIL_CHARS)).to_string()
            });
            failed(match tail.filter(|t| !t.is_empty()) {
                Some(tail) => format!("{error}\n{tail}"),
                None => error.to_string(),
            })
        }
    }
}

fn describe_transport(config: &Map<String, Value>) -> String {
    if let Some(url) = config.get("url").and_then(Value::as_str) {
        return url.to_string();
    }
    let mut parts = vec![config.get("command").map(crate::js::string).unwrap_or_default()];
    parts.extend(
        config
            .get("args")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(crate::js::string),
    );
    parts.join(" ")
}

fn list(loaded: &LoadedConfig, as_json: bool, note: Option<String>, env: &Env, agent_dir: &Path, io: &mut Io) -> i32 {
    let mut reports: Vec<Map<String, Value>> = Vec::new();
    let mut failed = !loaded.errors.is_empty();
    for entry in &loaded.servers {
        let exposure = if entry.config.get("exposure").and_then(Value::as_str) == Some("hidden") {
            "hidden"
        } else {
            "repl"
        };
        let mut report = Map::new();
        report.insert("name".into(), entry.name.clone().into());
        report.insert("scope".into(), entry.scope.as_str().into());
        report.insert("source".into(), entry.source.clone().into());
        report.insert("enabled".into(), entry.enabled().into());
        report.insert("exposure".into(), exposure.into());
        report.insert("transport".into(), describe_transport(&entry.config).into());
        report.insert("state".into(), "disabled".into());
        report.insert("tools".into(), json!([]));
        if entry.enabled() {
            let connection = connect(entry, env);
            report.insert("state".into(), connection.state.into());
            report.insert("tools".into(), json!(connection.tools));
            let overrides: Map<String, Value> = connection
                .tools
                .iter()
                .filter_map(|tool| {
                    let e = if tool_exposure(&entry.config, tool) == "hidden" {
                        "hidden"
                    } else {
                        "repl"
                    };
                    (e != exposure).then(|| (tool.clone(), Value::String(e.into())))
                })
                .collect();
            if !overrides.is_empty() {
                report.insert("toolExposure".into(), Value::Object(overrides));
            }
            if let Some((resources, templates)) = connection.resources {
                report.insert("resources".into(), json!(resources));
                report.insert("resourceTemplates".into(), json!(templates));
            }
            if let Some(error) = connection.error.filter(|_| connection.state != "connected") {
                report.insert("error".into(), error.into());
            }
            failed |= connection.state != "connected";
        }
        reports.push(report);
    }
    if as_json {
        let mut value = json!({"servers": reports, "errors": loaded.errors});
        if let Some(note) = &note {
            value["note"] = Value::String(note.clone());
        }
        io.log(&config::json::stringify_pretty(&value));
        return i32::from(failed);
    }
    if reports.is_empty() && loaded.errors.is_empty() {
        io.log(&format!(
            "No MCP servers configured. Add them to {} or .pi/mcp.json.",
            agent_dir.join("mcp.json").display()
        ));
    }
    for report in &reports {
        let str_of = |k: &str| report.get(k).and_then(Value::as_str).unwrap_or_default();
        let tools: Vec<&str> = report["tools"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let state = match str_of("state") {
            "connected" => format!(
                "connected, {} tool{}",
                tools.len(),
                if tools.len() == 1 { "" } else { "s" }
            ),
            "needs-auth" => "needs sign-in".into(),
            other => other.to_string(),
        };
        io.log(&format!(
            "{}: {state} ({}, {})",
            str_of("name"),
            str_of("exposure"),
            str_of("scope")
        ));
        io.log(&format!("  {}", str_of("transport")));
        if str_of("state") == "needs-auth" {
            io.log(&format!("  sign in with: ultron mcp login {}", str_of("name")));
        }
        if !tools.is_empty() {
            let shown: Vec<String> = tools
                .iter()
                .map(|t| {
                    match report
                        .get("toolExposure")
                        .and_then(|e| e.get(*t))
                        .and_then(Value::as_str)
                    {
                        Some(e) => format!("{t} [{e}]"),
                        None => t.to_string(),
                    }
                })
                .collect();
            io.log(&format!("  tools: {}", shown.join(", ")));
        }
        if let Some(resources) = report.get("resources") {
            io.log(&format!(
                "  resources: {}, URI templates: {}",
                resources,
                report.get("resourceTemplates").cloned().unwrap_or(json!(0))
            ));
        }
        if let Some(error) = report.get("error").and_then(Value::as_str).filter(|e| !e.is_empty()) {
            io.log(&format!("  {}", error.split('\n').collect::<Vec<_>>().join("\n  ")));
        }
    }
    for error in &loaded.errors {
        io.log(&format!("config error: {error}"));
    }
    if let Some(note) = note {
        io.log(&note);
    }
    i32::from(failed)
}
