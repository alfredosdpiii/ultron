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

pub mod compaction_op;
pub mod compose;
pub mod core;
pub mod journal;
pub mod lane;
pub mod naming;
pub mod pi_view;
pub mod view;

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
    /// `getDocsPath()`: packages/coding-agent/docs of the checkout this binary was built from.
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

/// The model flags a session starts with (`--provider`, `--model`, `--api-key`, `--thinking`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModelFlags {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub thinking: Option<String>,
}
