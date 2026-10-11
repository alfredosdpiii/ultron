//! Print mode (`-p`) and `--mode json` through the native architecture (native-command.ts): this process starts its
//! own server generation for the instance (crates/daemon), the session runs in a session worker process (the
//! session actor of crates/session-core: the lane, its host stack, the format-4 journal), and the client runtime
//! (crates/client) creates or attaches the session, prompts it and follows its transcript. There is no in-process
//! mode, as TypeScript has none.

use crate::Env;
use crate::args::Args;
use client::runtime::{ClientCommand, ClientResult, Route, run_client};
use daemon::server::{ServerOptions, start_foreground_server};
use daemon::workers::WorkerModel;
use serde_json::json;
use session_core::ModelFlags;
pub use session_core::core::lane_config;
pub use session_core::lane::{
    catalog_model, clock_lines, format_clock_utc, format_gap, rlm_tool, session_directory_name, system_prompt,
};
use session_journal::repo::Metadata;

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

/// What a prompted run produced: the final assistant text, its server and its session.
pub struct Prompted {
    pub text: String,
    pub server_id: String,
    pub session_id: String,
}

/// The model flags of the command line.
pub fn flags(parsed: &Args) -> ModelFlags {
    ModelFlags {
        provider: parsed.provider.clone(),
        model: parsed.model.clone(),
        api_key: parsed.api_key.clone(),
        thinking: parsed.thinking.clone(),
    }
}

/// `--provider`/`--model` against the built-in catalog, with `models.json` base URLs applied.
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
    if session_core::lane::api_key(&flags(parsed), env, &model.provider).is_none() {
        return Err(format!("No API key found for {}", model.provider));
    }
    Ok(())
}

/// The server options of a run in this instance (`extension_mode`: what extensions are told).
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
        },
        keep_alive: true,
        root: instance.root().map(std::path::Path::to_path_buf),
    })
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
    };
    match target {
        Target::New { id } => command.session_id = id.clone(),
        Target::Open(metadata) => command.session_id = Some(metadata.id.clone()),
        Target::Fork(metadata) => command.fork_from_session_id = Some(metadata.id.clone()),
        Target::Ephemeral => command.no_session = true,
    }
    command
}

/// Run one prompted turn through this process's own server generation.
pub fn run_prompt(parsed: &Args, env: &Env, prompt: String, target: Target) -> Result<Prompted, String> {
    preflight(parsed, env, &target)?;
    let json_mode = parsed.mode.as_deref() == Some("json");
    let options = server_options(parsed, env, if json_mode { "json" } else { "print" })?;
    let server = start_foreground_server(&options).map_err(|e| e.to_string())?;
    let route = Route {
        server_id: server.server_id.clone(),
        path: server.endpoint_path.clone(),
    };
    let result = run_client(&client_command(parsed, &target, Some(prompt)), &route);
    server.close();
    match result.map_err(|e| daemon::lost::describe(&e).unwrap_or(e))? {
        ClientResult::Prompted {
            server_id,
            session_id,
            text,
        } => Ok(Prompted {
            text,
            server_id,
            session_id,
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
