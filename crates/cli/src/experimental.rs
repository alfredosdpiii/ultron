//! `ultron server` and `ultron client` (`PI_EXPERIMENTAL=1`, as `experimental/commands.ts` `runExperimentalCommand`
//! dispatches them): the command lines (`cli/experimental/command.ts`, `command-options.ts`, `commands/server.ts`,
//! `commands/client.ts`), an operator-held server with its Radius relay and its `Server:` / `Socket:` / `Radius: …`
//! lines (`runServerCommand`), and a client of a server reached over a Unix socket or through the Radius gateway
//! (`--connect radius://<server id>`, `client-runtime.ts` `openClientRuntime`): the interactive TUI on a terminal
//! (`runClientTui`), else a listing, an attachment or one prompted turn (`runClient`).

use crate::Env;
use crate::link::{RemoteRoute, RemoteSelection};
use client::runtime::{ClientResult, Route};
use daemon::server::{ServerOptions, activate_server, start_foreground_server};
use daemon::workers::WorkerModel;
use serde_json::{Value, json};
use server::radius_relay::AuthInput;
use server::radius_relay::{RelayHostStatus, StatusSink, relay_status_line};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};

// ------------------------------------------------------------------------------------------------- command lines

/// `TransportAddress` of `--connect`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportAddress {
    Unix(PathBuf),
    Radius(String),
}

/// `ServerCommand`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerCommand {
    pub auth: Option<AuthInput>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub plugin_packages: Vec<String>,
    pub server_id: Option<String>,
    pub session_dir: Option<String>,
}

/// `ClientCommand` (of the experimental command line).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClientCommand {
    pub auth: Option<AuthInput>,
    pub connect: Option<TransportAddress>,
    pub session_id: Option<String>,
    pub continue_: bool,
    pub resume: bool,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub plugin_packages: Vec<String>,
    pub prompt: Option<String>,
}

/// A parsed `ultron server|client` command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Experimental {
    Server(ServerCommand),
    Client(ClientCommand),
}

fn auth_json(auth: &AuthInput) -> Value {
    match auth {
        AuthInput::Token(token) => json!({"type": "token", "token": token}),
        AuthInput::File(path) => json!({"type": "file", "path": path.display().to_string()}),
    }
}

impl Experimental {
    /// The command as the TypeScript parse answers it (`{command, ...}`, keys in its order).
    pub fn to_json(&self) -> Value {
        let mut map = serde_json::Map::new();
        let mut put = |key: &str, value: Value| {
            map.insert(key.to_string(), value);
        };
        match self {
            Experimental::Server(c) => {
                put("command", json!("server"));
                if let Some(auth) = &c.auth {
                    put("auth", auth_json(auth));
                }
                if let Some(provider) = &c.provider {
                    put("provider", json!(provider));
                }
                if let Some(model) = &c.model {
                    put("model", json!(model));
                }
                if !c.plugin_packages.is_empty() {
                    put("pluginPackages", json!(c.plugin_packages));
                }
                if let Some(id) = &c.server_id {
                    put("serverId", json!(id));
                }
                if let Some(dir) = &c.session_dir {
                    put("sessionDir", json!(dir));
                }
            }
            Experimental::Client(c) => {
                put("command", json!("client"));
                if let Some(auth) = &c.auth {
                    put("auth", auth_json(auth));
                }
                match &c.connect {
                    Some(TransportAddress::Radius(id)) => {
                        put("connect", json!({"transport": "radius", "serverId": id}))
                    }
                    Some(TransportAddress::Unix(path)) => {
                        put(
                            "connect",
                            json!({"transport": "unix", "path": path.display().to_string()}),
                        );
                    }
                    None => {}
                }
                if let Some(id) = &c.session_id {
                    put("sessionId", json!(id));
                }
                if c.continue_ {
                    put("continue", json!(true));
                }
                if c.resume {
                    put("resume", json!(true));
                }
                if let Some(provider) = &c.provider {
                    put("provider", json!(provider));
                }
                if let Some(model) = &c.model {
                    put("model", json!(model));
                }
                if !c.plugin_packages.is_empty() {
                    put("pluginPackages", json!(c.plugin_packages));
                }
                if let Some(prompt) = &c.prompt {
                    put("prompt", json!(prompt));
                }
            }
        }
        Value::Object(map)
    }
}

/// An option's parsed value.
#[derive(Debug, Clone)]
enum OptionValue {
    Text(String),
    Flag,
    Address(TransportAddress),
}

/// `CommandOption`.
struct OptionSpec {
    name: &'static str,
    flag: bool,
    repeatable: bool,
    parse: fn(&str) -> Result<OptionValue, String>,
}

fn text(value: &str) -> Result<OptionValue, String> {
    Ok(OptionValue::Text(value.to_string()))
}

fn flag(_: &str) -> Result<OptionValue, String> {
    Ok(OptionValue::Flag)
}

fn server_id(value: &str) -> Result<OptionValue, String> {
    if protocol::is_server_id(value) {
        Ok(OptionValue::Text(value.to_string()))
    } else {
        Err(format!("Invalid --server-id \"{value}\"; expected a lowercase UUIDv4"))
    }
}

fn connect(value: &str) -> Result<OptionValue, String> {
    parse_transport_address(value).map(OptionValue::Address)
}

const fn value_option(name: &'static str, parse: fn(&str) -> Result<OptionValue, String>) -> OptionSpec {
    OptionSpec {
        name,
        flag: false,
        repeatable: false,
        parse,
    }
}

const fn flag_option(name: &'static str) -> OptionSpec {
    OptionSpec {
        name,
        flag: true,
        repeatable: false,
        parse: flag,
    }
}

const PLUGIN_PACKAGE: OptionSpec = OptionSpec {
    name: "-e",
    flag: false,
    repeatable: true,
    parse: text,
};

const SERVER_OPTIONS: [OptionSpec; 7] = [
    value_option("--server-id", server_id),
    value_option("--session-dir", text),
    value_option("--provider", text),
    value_option("--model", text),
    PLUGIN_PACKAGE,
    value_option("--auth-token", text),
    value_option("--auth-token-file", text),
];

const CLIENT_OPTIONS: [OptionSpec; 11] = [
    value_option("--connect", connect),
    value_option("--session-id", text),
    flag_option("--continue"),
    flag_option("-c"),
    flag_option("--resume"),
    flag_option("-r"),
    value_option("--provider", text),
    value_option("--model", text),
    PLUGIN_PACKAGE,
    value_option("--auth-token", text),
    value_option("--auth-token-file", text),
];

/// `parseOptions`: options up to the first argument that is not one (or `--`); the rest is left over.
struct Parsed {
    values: HashMap<&'static str, Vec<OptionValue>>,
    remaining: Vec<String>,
    errors: Vec<String>,
}

impl Parsed {
    fn text(&self, name: &str) -> Option<String> {
        match self.values.get(name)?.first()? {
            OptionValue::Text(value) => Some(value.clone()),
            _ => None,
        }
    }

    fn texts(&self, name: &str) -> Vec<String> {
        self.values
            .get(name)
            .into_iter()
            .flatten()
            .filter_map(|v| match v {
                OptionValue::Text(value) => Some(value.clone()),
                _ => None,
            })
            .collect()
    }

    fn flag(&self, name: &str) -> bool {
        self.values.get(name).is_some_and(|v| !v.is_empty())
    }
}

fn parse_options(argv: &[String], options: &[OptionSpec]) -> Parsed {
    let mut parsed = Parsed {
        values: HashMap::new(),
        remaining: Vec::new(),
        errors: Vec::new(),
    };
    let mut index = 0;
    while index < argv.len() {
        let argument = &argv[index];
        if argument == "--" {
            parsed.remaining.extend(argv[index..].iter().cloned());
            break;
        }
        let equals = argument.find('=');
        let name = equals.map_or(argument.as_str(), |i| &argument[..i]);
        let Some(option) = options.iter().find(|o| o.name == name) else {
            parsed.remaining.extend(argv[index..].iter().cloned());
            break;
        };
        index += 1;
        let value = if option.flag {
            if equals.is_some() {
                parsed.errors.push(format!("{name} does not take a value"));
                continue;
            }
            String::new()
        } else {
            let mut candidate = equals.map(|i| argument[i + 1..].to_string());
            if candidate.is_none()
                && let Some(next) = argv.get(index)
                && !next.starts_with('-')
            {
                candidate = Some(next.clone());
                index += 1;
            }
            match candidate {
                Some(value) if !value.is_empty() => value,
                _ => {
                    parsed.errors.push(format!("{name} requires a value"));
                    continue;
                }
            }
        };
        let values = parsed.values.entry(option.name).or_default();
        if !values.is_empty() && !option.repeatable {
            parsed.errors.push(format!("{name} may only be specified once"));
            continue;
        }
        match (option.parse)(&value) {
            Ok(value) => values.push(value),
            Err(error) => parsed.errors.push(error),
        }
    }
    parsed
}

/// `parseAuth`.
fn parse_auth(parsed: &Parsed) -> (Option<AuthInput>, Vec<String>) {
    match (parsed.text("--auth-token"), parsed.text("--auth-token-file")) {
        (Some(_), Some(_)) => (
            None,
            vec!["--auth-token and --auth-token-file are mutually exclusive".into()],
        ),
        (Some(token), None) => (Some(AuthInput::Token(token)), Vec::new()),
        (None, Some(path)) => (Some(AuthInput::File(PathBuf::from(path))), Vec::new()),
        (None, None) => (None, Vec::new()),
    }
}

fn unsupported_options(command: &str, parsed: &Parsed) -> Vec<String> {
    if parsed.remaining.is_empty() {
        return Vec::new();
    }
    vec![format!(
        "The experimental {command} command does not support existing CLI options yet"
    )]
}

/// `cli.parse(argv)` for `server …` and `client …` (`None` for anything else, which the experimental group does
/// not dispatch). `Err` holds the errors in the TypeScript order.
pub fn parse(argv: &[String]) -> Option<Result<Experimental, Vec<String>>> {
    let (command, rest) = argv.split_first()?;
    match command.as_str() {
        "server" => Some(parse_server(rest)),
        "client" => Some(parse_client(rest)),
        _ => None,
    }
}

fn parse_server(argv: &[String]) -> Result<Experimental, Vec<String>> {
    let parsed = parse_options(argv, &SERVER_OPTIONS);
    let (auth, auth_errors) = parse_auth(&parsed);
    let provider = parsed.text("--provider");
    let model = parsed.text("--model");
    let mut errors = parsed.errors.clone();
    errors.extend(auth_errors);
    if provider.is_some() && model.is_none() {
        errors.push("--provider requires --model".into());
    }
    errors.extend(unsupported_options("server", &parsed));
    if !errors.is_empty() {
        return Err(errors);
    }
    Ok(Experimental::Server(ServerCommand {
        auth,
        provider,
        model,
        plugin_packages: parsed.texts("-e"),
        server_id: parsed.text("--server-id"),
        session_dir: parsed.text("--session-dir"),
    }))
}

fn parse_client(argv: &[String]) -> Result<Experimental, Vec<String>> {
    let parsed = parse_options(argv, &CLIENT_OPTIONS);
    let (auth, auth_errors) = parse_auth(&parsed);
    let connect = parsed
        .values
        .get("--connect")
        .and_then(|v| v.first())
        .and_then(|v| match v {
            OptionValue::Address(address) => Some(address.clone()),
            _ => None,
        });
    let session_id = parsed.text("--session-id");
    let continue_ = parsed.flag("--continue") || parsed.flag("-c");
    let resume = parsed.flag("--resume") || parsed.flag("-r");
    let provider = parsed.text("--provider");
    let model = parsed.text("--model");
    let remaining = &parsed.remaining;
    let dashes = remaining.first().is_some_and(|a| a == "--");
    let prompt_args = if dashes { &remaining[1..] } else { &remaining[..] };
    let prompt = (prompt_args.len() == 1 && (dashes || !prompt_args[0].starts_with('-')) && !prompt_args[0].is_empty())
        .then(|| prompt_args[0].clone());
    let mut errors = parsed.errors.clone();
    errors.extend(auth_errors);
    if provider.is_some() && model.is_none() {
        errors.push("--provider requires --model".into());
    }
    if [session_id.is_some(), continue_, resume].iter().filter(|b| **b).count() > 1 {
        errors.push("--session-id, --continue, and --resume are mutually exclusive".into());
    }
    if !remaining.is_empty() && prompt.is_none() {
        errors.extend(unsupported_options("client", &parsed));
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    Ok(Experimental::Client(ClientCommand {
        auth,
        connect,
        session_id,
        continue_,
        resume,
        provider,
        model,
        plugin_packages: parsed.texts("-e"),
        prompt,
    }))
}

/// `decodeURIComponent`: `None` for a malformed escape or bytes that are not UTF-8.
fn decode_uri_component(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = value.get(i + 1..i + 3)?;
            if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// `parseTransportAddress`: `radius://<lowercase UUIDv4 server id>` or `unix:///<absolute path>`.
pub fn parse_transport_address(value: &str) -> Result<TransportAddress, String> {
    let invalid = || format!("Invalid --connect address \"{value}\"");
    let url = url::Url::parse(value).map_err(|_| invalid())?;
    let host = url.host_str().unwrap_or_default();
    match url.scheme() {
        "radius" => {
            let path = url.path();
            if !url.username().is_empty()
                || url.password().is_some_and(|p| !p.is_empty())
                || url.port().is_some()
                || (!path.is_empty() && path != "/")
                || url.query().is_some_and(|q| !q.is_empty())
                || url.fragment().is_some_and(|f| !f.is_empty())
                || value != format!("radius://{host}{path}")
            {
                return Err(invalid());
            }
            if !protocol::is_server_id(host) {
                return Err("Radius transport address requires a lowercase UUIDv4 server ID".into());
            }
            Ok(TransportAddress::Radius(host.to_string()))
        }
        "unix" => {
            if !host.is_empty()
                || url.port().is_some()
                || !url.username().is_empty()
                || url.password().is_some_and(|p| !p.is_empty())
            {
                return Err("Unix transport address must not include an authority".into());
            }
            if !value.starts_with("unix:///")
                || value.starts_with("unix:////")
                || value.contains('?')
                || value.contains('#')
                || url.as_str() != value
            {
                return Err(invalid());
            }
            let path = decode_uri_component(url.path()).ok_or_else(invalid)?;
            if path.contains('\0') {
                return Err(invalid());
            }
            if !path.starts_with('/') {
                return Err("Unix transport address requires an absolute path".into());
            }
            Ok(TransportAddress::Unix(PathBuf::from(path)))
        }
        scheme => Err(format!("Unsupported --connect transport \"{scheme}:\"")),
    }
}

// ------------------------------------------------------------------------------------------------------ running

/// `runExperimentalCommand`: with `PI_EXPERIMENTAL=1`, `ultron server …` and `ultron client …` run here (`None`
/// leaves every other command line to the stable CLI). Errors are printed as `Error: …` and exit 1.
pub fn run(argv: &[String], env: &Env, out: &mut dyn Write, err: &mut dyn Write) -> Option<i32> {
    if (env.var)("PI_EXPERIMENTAL").as_deref() != Some("1") {
        return None;
    }
    let parsed = parse(argv)?;
    let result = parsed.and_then(|command| {
        match command {
            Experimental::Server(command) => run_server(&command, env, out),
            Experimental::Client(command) => run_client(&command, env, out),
        }
        .map_err(|error| vec![error])
    });
    Some(match result {
        Ok(code) => code,
        Err(errors) => {
            for error in errors {
                let _ = writeln!(err, "Error: {error}");
            }
            1
        }
    })
}

fn io(error: std::io::Error) -> String {
    error.to_string()
}

/// `resolvePath` of a path option, against the working directory (`~` is the home directory).
fn resolve(env: &Env, path: &str) -> PathBuf {
    crate::file_args::resolve_to_cwd(path, &env.cwd, &env.home)
}

fn resolved_auth(env: &Env, auth: &Option<AuthInput>) -> Option<AuthInput> {
    auth.clone().map(|auth| match auth {
        AuthInput::File(path) => AuthInput::File(resolve(env, &path.display().to_string())),
        token => token,
    })
}

/// The server options of this instance for the experimental commands.
fn instance_server_options(
    env: &Env,
    session_dir: Option<&str>,
    provider: Option<String>,
    model: Option<String>,
) -> Result<ServerOptions, String> {
    let instance = crate::instance(env, false);
    instance.prepare().map_err(io)?;
    let session_dir = session_dir.map(|dir| resolve(env, dir).display().to_string());
    let session_dir = crate::session::session_directory_of(session_dir.as_deref(), env, &instance);
    Ok(ServerOptions {
        directory: instance.dir.clone(),
        session_dir,
        server_id: (env.var)("ULTRON_SERVER_ID"),
        model: WorkerModel {
            provider,
            model,
            env: crate::startup::worker_env(env),
            ..Default::default()
        },
        keep_alive: true,
        root: instance.root().map(Path::to_path_buf),
        relay: None,
    })
}

/// What the server command waits for.
enum ServerEvent {
    Status(RelayHostStatus),
    Stop,
}

/// `runServerCommand`: an operator-held server (and its Radius relay) until SIGINT/SIGTERM or until it closes. It
/// prints `Server: <id>` and `Socket: <path>`, then a `Radius: …` line per relay status change (a status reported
/// while starting is printed after those lines, the latest only).
pub fn run_server(command: &ServerCommand, env: &Env, out: &mut dyn Write) -> Result<i32, String> {
    if !command.plugin_packages.is_empty() {
        return Err("Plugin packages are not built by the Rust server".into());
    }
    let (events, received) = mpsc::channel::<ServerEvent>();
    let statuses = events.clone();
    let on_status: StatusSink = Arc::new(move |status: &RelayHostStatus| {
        let _ = statuses.send(ServerEvent::Status(status.clone()));
    });
    let mut options = instance_server_options(
        env,
        command.session_dir.as_deref(),
        command.provider.clone(),
        command.model.clone(),
    )?;
    if command.server_id.is_some() {
        options.server_id = command.server_id.clone();
    }
    options.relay = Some(crate::radius_relay::relay_config_with(
        env,
        resolved_auth(env, &command.auth),
        Some(on_status),
    ));
    let server = start_foreground_server(&options).map_err(io)?;
    let stop = std::sync::Mutex::new(events.clone());
    daemon::signals::set_handler(Arc::new(move |_| {
        let _ = stop.lock().unwrap_or_else(|e| e.into_inner()).send(ServerEvent::Stop);
    }));
    let closed = server.clone();
    std::thread::spawn(move || {
        closed.wait_closed();
        let _ = events.send(ServerEvent::Stop);
    });
    writeln!(out, "Server: {}", server.server_id).map_err(io)?;
    writeln!(out, "Socket: {}", server.socket_path.display()).map_err(io)?;
    out.flush().map_err(io)?;
    let mut previous = String::new();
    let mut report = |out: &mut dyn Write, status: &RelayHostStatus| -> Result<(), String> {
        if let Some(line) = relay_status_line(&mut previous, status) {
            writeln!(out, "{line}").map_err(io)?;
            out.flush().map_err(io)?;
        }
        Ok(())
    };
    let mut stopped = false;
    let mut started = Vec::new();
    while let Ok(event) = received.try_recv() {
        match event {
            ServerEvent::Status(status) => started.push(status),
            ServerEvent::Stop => stopped = true,
        }
    }
    if let Some(status) = pending_relay_status(started) {
        report(out, &status)?;
    }
    while !stopped {
        match received.recv() {
            Ok(ServerEvent::Status(status)) => report(out, &status)?,
            Ok(ServerEvent::Stop) | Err(_) => stopped = true,
        }
    }
    daemon::signals::clear_handler();
    server.close();
    Ok(0)
}

/// `pendingRelayStatus`: of the relay statuses reported while the server started (before its `Server:` and
/// `Socket:` lines), only the latest is reported.
pub fn pending_relay_status(statuses: Vec<RelayHostStatus>) -> Option<RelayHostStatus> {
    statuses.into_iter().last()
}

/// `routeFromExplicitPath`.
fn route_from_explicit_path(path: &Path) -> Result<Route, String> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let server_id = name.strip_suffix(".sock").unwrap_or_default();
    if !protocol::is_server_id(server_id) {
        return Err("--connect path must end with <uuidv4-server-id>.sock".into());
    }
    Ok(Route {
        server_id: server_id.to_string(),
        path: path.to_path_buf(),
    })
}

/// Where `ultron client` goes (`openClientRuntime`'s routes): one server, or every server discovered (in server ID
/// order) when several are.
pub enum ClientRoutes {
    One(RemoteRoute),
    Several(Vec<Route>),
}

/// `openClientRuntime`'s routes: the `--connect` address, else the instance's reachable servers, else a server
/// activated for this client (with the command's model).
pub fn client_routes(command: &ClientCommand, env: &Env) -> Result<ClientRoutes, String> {
    let radius = matches!(command.connect, Some(TransportAddress::Radius(_)));
    if command.auth.is_some() && !radius {
        return Err("Authentication is only supported for experimental Radius connections".into());
    }
    if command.provider.is_some() && command.model.is_none() {
        return Err("Server model provider requires a model".into());
    }
    if command.connect.is_some() && command.model.is_some() {
        return Err("Model selection is only valid when automatically activating a new server".into());
    }
    if radius && !command.plugin_packages.is_empty() {
        return Err("Plugin package paths can only be configured on a local Unix server".into());
    }
    let routes = match &command.connect {
        Some(TransportAddress::Radius(server_id)) => ClientRoutes::One(RemoteRoute::Radius {
            server_id: server_id.clone(),
            auth: Arc::new(crate::radius_relay::auth_resolver(
                env,
                resolved_auth(env, &command.auth),
            )),
        }),
        Some(TransportAddress::Unix(path)) => ClientRoutes::One(RemoteRoute::Unix(route_from_explicit_path(path)?)),
        None => {
            let options = instance_server_options(env, None, command.provider.clone(), command.model.clone())?;
            let sockets = daemon::instance::socket_dir(&options.directory);
            let discovered = client::discover_unix_servers(&sockets, client::DEFAULT_DISCOVERY_TIMEOUT).map_err(io)?;
            if !discovered.is_empty() && command.model.is_some() {
                return Err("Model selection is only valid when automatically activating a new server".into());
            }
            let mut routes: Vec<Route> = discovered
                .into_iter()
                .map(|found| Route {
                    server_id: found.server_id,
                    path: found.path,
                })
                .collect();
            match routes.len() {
                0 => {
                    let activated = activate_server(&options, false).map_err(io)?;
                    ClientRoutes::One(RemoteRoute::Unix(Route {
                        server_id: activated.server_id,
                        path: activated.path,
                    }))
                }
                1 => ClientRoutes::One(RemoteRoute::Unix(routes.remove(0))),
                _ => ClientRoutes::Several(routes),
            }
        }
    };
    if !command.plugin_packages.is_empty() {
        if matches!(routes, ClientRoutes::Several(_)) {
            return Err("Plugin selection requires exactly one local server".into());
        }
        return Err("Plugin packages are not built by the Rust server".into());
    }
    Ok(routes)
}

/// What a client of several discovered servers does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerChoice {
    /// Every server's sessions, `(server id, session id)` in that order.
    List(Vec<(String, String)>),
    /// The session `session_id` of `servers[server]`.
    Session { server: usize, session_id: String },
}

/// The session selection over several servers, each `(server id, its session directory's summaries)`: `runClient`'s
/// (`runClientOperation`), or with `tui` the client TUI's (`prepareClientSession`). Creating a Session needs exactly
/// one server, so with several only an existing one is chosen.
pub fn choose_server(
    command: &ClientCommand,
    servers: &[(String, Vec<Value>)],
    tui: bool,
) -> Result<ServerChoice, String> {
    let id_of = |summary: &Value| summary["sessionId"].as_str().unwrap_or_default().to_string();
    let mut session_id = command.session_id.clone();
    if session_id.is_none() && command.prompt.is_none() && !tui {
        let mut sessions: Vec<(String, String)> = servers
            .iter()
            .flat_map(|(server, sessions)| sessions.iter().map(move |s| (server.clone(), id_of(s))))
            .collect();
        sessions.sort();
        return Ok(ServerChoice::List(sessions));
    }
    let single = || {
        if tui {
            "Starting a Session requires exactly one server".to_string()
        } else {
            "Client prompt requires exactly one discovered server to create a Session".to_string()
        }
    };
    if session_id.is_none() && (command.continue_ || command.resume) {
        let modified = |s: &Value| s["modifiedAt"].as_f64().unwrap_or(0.0);
        let newest = servers
            .iter()
            .enumerate()
            .flat_map(|(index, (server, sessions))| sessions.iter().map(move |s| (index, server, s)))
            .min_by(|(_, a_server, a), (_, b_server, b)| {
                modified(b)
                    .partial_cmp(&modified(a))
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a_server.cmp(b_server))
                    .then_with(|| id_of(a).cmp(&id_of(b)))
            });
        if let Some((index, _, summary)) = newest {
            if tui {
                return Ok(ServerChoice::Session {
                    server: index,
                    session_id: id_of(summary),
                });
            }
            session_id = Some(id_of(summary));
        }
    }
    let Some(id) = session_id else {
        return Err(single());
    };
    let matches: Vec<usize> = servers
        .iter()
        .enumerate()
        .filter(|(_, (_, sessions))| sessions.iter().any(|s| id_of(s) == id))
        .map(|(index, _)| index)
        .collect();
    match matches.as_slice() {
        [server] => Ok(ServerChoice::Session {
            server: *server,
            session_id: id,
        }),
        [] if tui => Err(single()),
        [] => Err(format!("No discovered server contains session {id}")),
        _ => Err(format!("Session {id} is available from more than one server")),
    }
}

/// A server's session summaries (its `pi.session-directory` state).
fn server_sessions(route: &Route) -> Result<Vec<Value>, String> {
    let connection = client::runtime::Connection::connect(&route.path, &route.server_id).map_err(crate::link::fail)?;
    let sessions = connection
        .subscribe(connection.server_target(), "pi.session-directory")
        .map(|subscription| {
            connection
                .state(&subscription, "state")
                .and_then(|state| state["sessions"].as_array().cloned())
                .unwrap_or_default()
        })
        .map_err(crate::link::fail);
    connection.disconnect();
    sessions
}

/// `runClientCommand`: the TUI on a terminal without a prompt (`runClientTui`), else `runClient`'s listing
/// (`<server id>\t<session id>` lines), attachment (`… attached`) or prompted answer (streamed as it arrives). Of
/// several discovered servers, the one holding the selected session is used (`choose_server`). A Radius client's
/// prompt survives its relay connection dropping: the reconnector brings it back and the client rejoins the session,
/// printing the rest of the answer.
pub fn run_client(command: &ClientCommand, env: &Env, out: &mut dyn Write) -> Result<i32, String> {
    let tui = command.prompt.is_none() && crate::interactive::is_terminal();
    let (route, chosen) = match client_routes(command, env)? {
        ClientRoutes::One(route) => (route, None),
        ClientRoutes::Several(routes) => {
            let servers = routes
                .iter()
                .map(|route| Ok((route.server_id.clone(), server_sessions(route)?)))
                .collect::<Result<Vec<_>, String>>()?;
            match choose_server(command, &servers, tui)? {
                ServerChoice::List(sessions) => {
                    for (server_id, session_id) in sessions {
                        writeln!(out, "{server_id}\t{session_id}").map_err(io)?;
                    }
                    return Ok(0);
                }
                ServerChoice::Session { server, session_id } => {
                    (RemoteRoute::Unix(routes[server].clone()), Some(session_id))
                }
            }
        }
    };
    let continue_ = command.continue_ || command.resume;
    let session_id = chosen.or_else(|| command.session_id.clone());
    if tui {
        let selection = RemoteSelection {
            session_id,
            continue_,
            name: None,
            fork: None,
        };
        return crate::interactive::run_remote_interactive(&crate::args::Args::default(), env, route, selection);
    }
    let connection = route.connect().map_err(crate::link::fail)?;
    // The text of the assistant message streaming now: a rejoined prompt prints the rest of its answer.
    let streamed = Arc::new(std::sync::Mutex::new(None::<String>));
    let streaming = streamed.clone();
    let on_event: client::runtime::EventListener = Arc::new(move |event: &Value| {
        let mut current = streaming.lock().unwrap_or_else(|e| e.into_inner());
        if event["type"] == "message_start" && current.is_some() {
            *current = Some(String::new());
        }
        if event["type"] == "message_update"
            && event["frame"]["type"] == "text_delta"
            && let Some(delta) = event["frame"]["delta"].as_str()
        {
            current.get_or_insert_with(String::new).push_str(delta);
            let mut stdout = std::io::stdout();
            let _ = stdout.write_all(delta.as_bytes());
            let _ = stdout.flush();
        }
    });
    let request = client::runtime::ClientCommand {
        session_id,
        continue_,
        prompt: command.prompt.clone(),
        ..Default::default()
    };
    let options = client::runtime::RunOptions {
        on_event: Some(on_event),
        ..Default::default()
    };
    let result = match &route {
        RemoteRoute::Radius { server_id, auth } => {
            let reopen = client::radius::RadiusReopen::new(server_id, auth.clone());
            let reopen = move |session: &str| reopen.reopen(session);
            client::runtime::run_client_reopening(&request, connection, &options, Some(&reopen))?
        }
        RemoteRoute::Unix(_) => client::runtime::run_client_on(&request, connection, &options)?,
    };
    match result {
        ClientResult::Attached { server_id, session_id } => {
            writeln!(out, "{server_id}\t{session_id}\tattached").map_err(io)?;
        }
        ClientResult::Prompted { text, .. } => {
            let partial = streamed.lock().unwrap_or_else(|e| e.into_inner()).clone();
            match partial {
                None => writeln!(out, "{text}").map_err(io)?,
                Some(partial) => match text.strip_prefix(partial.as_str()) {
                    Some(rest) => writeln!(out, "{rest}").map_err(io)?,
                    None => writeln!(out, "\n{text}").map_err(io)?,
                },
            }
        }
        ClientResult::List(sessions) => {
            for (server_id, session_id) in sessions {
                writeln!(out, "{server_id}\t{session_id}").map_err(io)?;
            }
        }
    }
    Ok(0)
}
