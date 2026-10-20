//! Print mode (`-p`) and `--mode json` through the native architecture (native-command.ts): this process starts its
//! own server generation for the instance (crates/daemon), the session runs in a session worker process (the
//! session actor of crates/session-core: the lane, its host stack, the format-4 journal), and the client runtime
//! (crates/client) creates or attaches the session, prompts it and follows its transcript. There is no in-process
//! mode, as TypeScript has none.

use crate::Env;
use crate::args::Args;
use client::runtime::{ClientCommand, ClientResult, Route, RunOptions, run_client_with};
use daemon::server::{ServerOptions, start_foreground_server};
use daemon::workers::WorkerModel;
use serde_json::json;
use session_core::ModelFlags;
pub use session_core::core::lane_config;
pub use session_core::lane::{
    catalog_model, clock_lines, format_clock_utc, format_gap, rlm_tool, session_directory_name, system_prompt,
};
use session_journal::repo::Metadata;

/// `traceStartup(label)` with the run's `ULTRON_STARTUP_TRACE` (crate::startup_trace's format).
pub fn trace_startup(env: &Env, label: &str) {
    session_core::startup_trace::trace_startup((env.var)("ULTRON_STARTUP_TRACE").as_deref(), label);
}

/// Which session a prompted run uses.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    /// A new session (with this id when given).
    New { id: Option<String> },
    /// An existing session, continued.
    Open(Metadata),
    /// A copy of an existing session's whole tree, continued.
    Fork(Metadata),
    /// `--no-session`: the session is removed when the run ends.
    Ephemeral,
}

/// What a prompted run produced: the final assistant text, its server and its session, and the route notices of
/// its answers (shown on stderr).
pub struct Prompted {
    pub text: String,
    pub server_id: String,
    pub session_id: String,
    pub notices: Vec<String>,
}

/// The Claude subscription route notices (`details.notice` of a `claude_subscription_route` diagnostic: "Claude
/// subscription limit reached; using API credits until <time>", or the switch back) on the assistant messages a
/// session file gained after `offset` bytes.
pub fn route_notices(path: &std::path::Path, offset: u64) -> Vec<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut text = String::new();
    let read = std::fs::File::open(path).and_then(|mut file| {
        file.seek(SeekFrom::Start(offset))?;
        file.read_to_string(&mut text)
    });
    if read.is_err() {
        return Vec::new();
    }
    let mut notices = Vec::new();
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let writes = value.as_array().cloned().unwrap_or_else(|| vec![value]);
        for write in writes {
            if write["type"] != "message" {
                continue;
            }
            if let Some(notice) = interactive::lane::route_notice(&write["message"])
                && !notices.contains(&notice)
            {
                notices.push(notice);
            }
        }
    }
    notices
}

/// The session file of `id` in the sessions directory `root` or one of its per-directory folders.
fn session_file(root: &std::path::Path, id: &str) -> Option<std::path::PathBuf> {
    let entries: Vec<std::path::PathBuf> = std::fs::read_dir(root).ok()?.flatten().map(|e| e.path()).collect();
    let nested = entries
        .iter()
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flat_map(|files| files.flatten().map(|file| file.path()));
    entries.iter().cloned().chain(nested).find(|path| {
        path.extension().is_some_and(|e| e == "jsonl")
            && session_journal::repo::read_metadata(path).is_some_and(|m| m.id == id)
    })
}

/// The model flags of the command line.
pub fn flags(parsed: &Args) -> ModelFlags {
    ModelFlags {
        provider: parsed.provider.clone(),
        model: parsed.model.clone(),
        api_key: parsed.api_key.clone(),
        thinking: parsed.thinking.clone(),
        // `--models` chooses a new session's model (Pi's `buildSessionOptions`; TypeScript's native command drops
        // it, decisions row "scoped models ... accepted"); without it the settings' `enabledModels` apply.
        models: parsed.models.clone(),
    }
}

/// `--provider`/`--model` against the built-in catalog and models.json.
pub fn resolve_model(parsed: &Args, env: &Env) -> Result<ai::Model, String> {
    session_core::lane::resolve_model(&flags(parsed), env)
}

/// The checks the TypeScript worker would fail on, made before a server starts so their messages stay direct: a
/// new session needs a model, and the model named on the command line needs a key.
fn preflight(parsed: &Args, env: &Env, target: &Target) -> Result<(), String> {
    let model = match resolve_model(parsed, env) {
        Ok(model) => model,
        Err(error) if matches!(target, Target::New { .. } | Target::Ephemeral) => return Err(error),
        // A resumed session keeps its own model; the worker reports its problems.
        Err(_) => return Ok(()),
    };
    // A models.json key that does not resolve reports why (`$VAR` unset, `!command` failing).
    // A stored OAuth credential may authenticate by header alone (Kimi For Coding's bearer, `getAuth`'s headers).
    if session_core::lane::request_key(&flags(parsed), env, &model.provider)?.is_none()
        && !session_core::lane::header_auth(env, &model.provider)
    {
        return Err(format!("No API key found for {}", model.provider));
    }
    Ok(())
}

/// The server options of a run in this instance (`extension_mode`: the worker's mode, Pi's `extensionMode`).
pub fn server_options(parsed: &Args, env: &Env, extension_mode: &str) -> Result<ServerOptions, String> {
    let instance = crate::instance(env, false);
    let session_dir = crate::session::session_directory_of(parsed.session_dir.as_deref(), env, &instance);
    instance.prepare().map_err(|e| e.to_string())?;
    Ok(ServerOptions {
        directory: instance.dir.clone(),
        session_dir,
        server_id: (env.var)("ULTRON_SERVER_ID").or_else(|| (env.var)("PI_SERVER_ID")),
        model: WorkerModel {
            provider: parsed.provider.clone(),
            model: parsed.model.clone(),
            api_key: parsed.api_key.clone(),
            thinking: parsed.thinking.clone(),
            extension_mode: Some(extension_mode.into()),
            no_naming: parsed.no_session,
            models: parsed.models.clone(),
            session: session_flags(parsed),
            env: crate::startup::worker_env(env),
        },
        keep_alive: true,
        root: instance.root().map(std::path::Path::to_path_buf),
        relay: Some(crate::radius_relay::relay_config(env)),
    })
}

/// The session's resource flags (native-command.ts `startForegroundServer` options). `-e`/`--extension` and
/// `--no-extensions` are not among them: Pi extensions are not supported by ultron-rs (the CLI warns and ignores
/// them).
pub fn session_flags(parsed: &Args) -> session_core::session_flags::SessionFlags {
    session_core::session_flags::SessionFlags {
        system_prompt: parsed.system_prompt.clone(),
        // A file input is read here, in the CLI's directory; the worker gets the text.
        append_system_prompt: parsed.append_system_prompt.as_ref().map(|inputs| {
            inputs
                .iter()
                .filter_map(|input| resources::prompt_files::resolve_prompt_input(Some(input)))
                .collect()
        }),
        no_skills: parsed.no_skills,
        no_context_files: parsed.no_context_files,
    }
}

/// What one prompted run sends: the prompt text and its images.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Prompt {
    pub text: String,
    pub images: Vec<serde_json::Value>,
}

/// The client command a target selects: an existing session by id, a fork of one, a new one (with its id), or
/// one removed when the run ends.
pub fn client_command(parsed: &Args, target: &Target, prompt: Option<String>) -> ClientCommand {
    let mut command = ClientCommand {
        session_id: None,
        name: parsed.name.clone(),
        continue_: false,
        prompt,
        fork_from_session_id: None,
        no_session: false,
        images: None,
    };
    match target {
        Target::New { id } => command.session_id = id.clone(),
        Target::Open(metadata) => command.session_id = Some(metadata.id.clone()),
        Target::Fork(metadata) => command.fork_from_session_id = Some(metadata.id.clone()),
        Target::Ephemeral => command.no_session = true,
    }
    command
}

fn route_of(route: daemon::server::Route) -> Route {
    Route {
        server_id: route.server_id,
        path: route.path,
    }
}

/// The route a client run uses: a foreground generation of this process (returned, for the run to close); a client
/// of the durable instance uses its detached server (activated when not running, never stopped by this run).
pub fn open_route(
    env: &Env,
    options: &ServerOptions,
) -> Result<(Route, Option<std::sync::Arc<daemon::server::RunningServer>>), String> {
    if crate::durable_client::is_client(env) {
        let route = crate::durable_client::activate(options)?;
        trace_startup(env, "cli.server-attached");
        return Ok((route_of(route), None));
    }
    let server = start_foreground_server(options).map_err(|e| e.to_string())?;
    trace_startup(env, "cli.server-started");
    let route = Route {
        server_id: server.server_id.clone(),
        path: server.endpoint_path.clone(),
    };
    Ok((route, Some(server)))
}

/// How long a signalled print run waits for its aborted turn to end before it exits anyway (`PROMPT_ABORT_WAIT_MS`).
pub const PROMPT_ABORT_WAIT: std::time::Duration = std::time::Duration::from_millis(5_000);

/// Run one prompted turn through this process's own server generation. A signal aborts the turn and exits with the
/// signal's code once it ended; a lost worker or server is rejoined (the server restarted) and the turn's session
/// followed to its end.
pub fn run_prompt(
    parsed: &Args,
    env: &Env,
    prompt: Prompt,
    target: Target,
    on_event: Option<client::runtime::EventListener>,
) -> Result<Prompted, String> {
    use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
    use std::sync::{Arc, Mutex};
    preflight(parsed, env, &target)?;
    let json_mode = parsed.mode.as_deref() == Some("json");
    let options = server_options(parsed, env, if json_mode { "json" } else { "print" })?;
    let (route, own) = open_route(env, &options)?;
    let servers = Arc::new(Mutex::new(own.into_iter().collect::<Vec<_>>()));
    let interrupt = Arc::new(AtomicBool::new(false));
    let code = Arc::new(AtomicI32::new(0));
    let (flag, signalled) = (interrupt.clone(), code.clone());
    daemon::signals::set_handler(Arc::new(move |signal| {
        signalled.store(daemon::signals::exit_code(signal), Ordering::SeqCst);
        flag.store(true, Ordering::SeqCst);
        // Never hang the exit on the aborted turn.
        std::thread::sleep(PROMPT_ABORT_WAIT);
        std::process::exit(daemon::signals::exit_code(signal));
    }));
    let (restarted, server_options) = (servers.clone(), options.clone());
    let durable = crate::durable_client::is_client(env);
    let run_options = RunOptions {
        on_event,
        interrupt: Some(interrupt.clone()),
        reconnect: Some(Arc::new(move || {
            // The server generation went away with its coordinator: start a new one (and coordinator); a client of
            // the durable instance activates the durable server again.
            if durable {
                return crate::durable_client::activate(&server_options).map(route_of);
            }
            let server = start_foreground_server(&server_options).map_err(|e| e.to_string())?;
            let route = Route {
                server_id: server.server_id.clone(),
                path: server.endpoint_path.clone(),
            };
            restarted.lock().unwrap_or_else(|e| e.into_inner()).push(server);
            Ok(route)
        })),
    };
    // The session's file before the run: its route notices are read from what the run adds. An ephemeral session is
    // removed here, once they are read, not by the client.
    let offset = match &target {
        Target::Open(metadata) => std::fs::metadata(&metadata.path).map_or(0, |m| m.len()),
        _ => 0,
    };
    let mut command = client_command(parsed, &target, Some(prompt.text));
    command.images = (!prompt.images.is_empty()).then_some(prompt.images);
    let ephemeral = std::mem::replace(&mut command.no_session, false);
    let result = run_client_with(&command, &route, &run_options);
    let notices = match &result {
        Ok(ClientResult::Prompted { session_id, .. }) => session_file(&options.session_dir, session_id)
            .map(|path| route_notices(&path, offset))
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    if ephemeral
        && let Ok(ClientResult::Prompted { session_id, .. }) = &result
        && let Ok(connection) = client::runtime::Connection::connect(&route.path, &route.server_id)
    {
        let target = connection.server_target();
        let _ = connection.call(target, "pi.session-management", "remove", json!([session_id]));
        connection.disconnect();
    }
    for server in servers.lock().unwrap_or_else(|e| e.into_inner()).drain(..) {
        server.close();
    }
    if interrupt.load(Ordering::SeqCst) {
        std::process::exit(code.load(Ordering::SeqCst));
    }
    daemon::signals::clear_handler();
    match result.map_err(|e| daemon::lost::describe(&e).unwrap_or(e))? {
        ClientResult::Prompted {
            server_id,
            session_id,
            text,
        } => Ok(Prompted {
            text,
            server_id,
            session_id,
            notices,
        }),
        _ => Err("the client did not prompt".into()),
    }
}

/// What a prompted run prints: the text for `-p`, the "prompted" result as one JSON line for `--mode json`.
pub fn render(json_mode: bool, prompted: &Prompted) -> String {
    if json_mode {
        let result = json!({"kind": "prompted", "serverId": prompted.server_id, "sessionId": prompted.session_id, "text": prompted.text});
        format!("{result}\n")
    } else {
        format!("{}\n", prompted.text)
    }
}

/// A run without a prompt and without the TUI (`runClient` with no prompt): the session a selector names is
/// attached (`<server>\t<session>\tattached`), else the server's sessions are listed (`<server>\t<session>` each).
pub fn run_unprompted(parsed: &Args, env: &Env, target: Target) -> Result<String, String> {
    let mode = if parsed.mode.as_deref() == Some("json") {
        "json"
    } else {
        "print"
    };
    let options = server_options(parsed, env, mode)?;
    let (route, server) = open_route(env, &options)?;
    let command = client_command(parsed, &target, None);
    let result = run_client_with(&command, &route, &RunOptions::default());
    if let Some(server) = server {
        server.close();
    }
    Ok(render_unprompted(
        &result.map_err(|e| daemon::lost::describe(&e).unwrap_or(e))?,
    ))
}

/// What a run without a prompt prints for its client result.
pub fn render_unprompted(result: &ClientResult) -> String {
    match result {
        ClientResult::Attached { server_id, session_id } => format!("{server_id}\t{session_id}\tattached\n"),
        ClientResult::List(sessions) => sessions
            .iter()
            .map(|(server, session)| format!("{server}\t{session}\n"))
            .collect(),
        ClientResult::Prompted { text, .. } => format!("{text}\n"),
    }
}
