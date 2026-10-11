//! A session's runtime is crates/session-core (the lane, its host stack, the format-4 journal and the session
//! actor the worker runs); these are the parts the daemon and its clients name.

pub use session_core::lane::{format_clock_utc, now_ms, rlm_tool, session_directory_name, system_prompt};
pub use session_core::{ModelFlags, RunEnv};

/// `--provider`/`--model` against the built-in catalog plus `models.json` (the checks a server makes before it
/// starts a worker).
pub fn resolve_model(provider: Option<&str>, model: Option<&str>, env: &RunEnv) -> Result<ai::Model, String> {
    let flags = ModelFlags {
        provider: provider.map(str::to_string),
        model: model.map(str::to_string),
        ..Default::default()
    };
    session_core::lane::resolve_model(&flags, env)
}
