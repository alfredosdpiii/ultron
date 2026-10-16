//! One session's runtime, shared by every mode. TypeScript runs every session in a session worker process and so
//! does the Rust build (crates/daemon `worker`); this crate is what that worker runs:
//!
//! - [`lane`]: the model, its key, the native worker's system prompt, the provider call, the clock lines and the
//!   single `rlm` tool (the model's tool list is exactly `[rlm]`);
//! - [`compose`]: the composition root of the host stack behind a lane's kernels;
//! - [`journal`]: the format-4 harness operation journal of a run; [`compaction_op`] that of a compaction;
//! - [`naming`]: automatic and model-made session names;
//! - [`pi_view`] / [`view`]: the session as Pi entries and Pi's read views (tree, messages, stats);
//! - [`core`]: the session actor that owns all of it and reports lane-watch events.

pub mod agent_hub;
pub mod async_events;
pub mod auto_compaction;
pub mod budget_note;
pub mod commands;
pub mod compaction_op;
pub mod compose;
pub mod core;
pub mod durable_store;
pub mod extension_apis;
pub mod extension_commands;
pub mod extension_file_hooks;
pub mod extension_host;
pub mod extension_providers;
pub mod extension_tools;
pub mod frame_models;
pub mod frames;
pub mod guard_log;
pub mod inspect;
pub mod journal;
pub mod lane;
pub mod lane_reuse;
pub mod migration;
pub mod model_refresh;
pub mod naming;
pub mod nudge;
pub mod op_journal;
pub mod packages;
pub mod pi_view;
pub mod recovery;
pub mod review_command;
pub mod session_flags;
pub mod session_resources;
pub mod session_tools;
pub mod stored_auth;
pub mod subagents;
pub mod swarm_ops;
pub mod turns;
pub mod view;
pub mod view_images;

use std::path::PathBuf;
use std::sync::Arc;

/// An environment variable lookup usable from any thread.
pub type VarLookup = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// The process environment and locations a session sees.
#[derive(Clone)]
pub struct RunEnv {
    pub cwd: PathBuf,
    pub home: PathBuf,
    pub var: VarLookup,
    /// `getDocsPath()`: the `docs/` directory of the package files (`cli::assets`): the embedded files installed under
    /// `<agent dir>/runtime/<version>-<hash>/`, or a live checkout in dev builds (`ULTRON_DEV_ASSETS`). Its parent holds
    /// `skills/`, `src/ultron/rlm/runtime.py` and `CHANGELOG.md`; the CLI makes it ready before a session reads it.
    pub docs: PathBuf,
}

impl RunEnv {
    pub fn agent_dir(&self) -> PathBuf {
        config::paths::agent_dir(|k| (self.var)(k), &self.home)
    }

    /// This process's environment in `cwd`.
    pub fn process(cwd: PathBuf, docs: PathBuf) -> RunEnv {
        RunEnv {
            cwd,
            home: std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default(),
            var: Arc::new(|k| std::env::var(k).ok()),
            docs,
        }
    }
}

/// The model flags a session starts with (`--provider`, `--model`, `--api-key`, `--thinking`, `--models`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModelFlags {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub thinking: Option<String>,
    /// `--models`: the scoped model patterns (None: the settings' `enabledModels`).
    pub models: Option<Vec<String>>,
}
