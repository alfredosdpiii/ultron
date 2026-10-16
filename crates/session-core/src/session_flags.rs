//! The resource flags a session worker is started with (`SessionWorkerOptions` of experimental/session-worker.ts,
//! from the command line through native-command.ts and the server's model options):
//!
//! - `--system-prompt <text>`: the system prompt is exactly this text (`currentSystemPrompt` returns it unrendered);
//! - `-e`/`--extension <path>`: extensions loaded before the discovered ones, also with `--no-extensions`;
//! - `--no-skills`: no skills in the prompt, the `skills.*` catalog or the command list (bundled ones included);
//! - `--no-context-files`: no AGENTS.md / CLAUDE.md in the prompt.
//!
//! The tool flags (`--tools`, `--no-tools`, ...) are not here: the model's tool list is exactly `[rlm]`.

use crate::RunEnv as Env;
use std::path::Path;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionFlags {
    pub system_prompt: Option<String>,
    /// Absolute paths (the client resolved them against its cwd), or package sources.
    pub extension_paths: Vec<String>,
    pub no_skills: bool,
    pub no_context_files: bool,
}

impl SessionFlags {
    /// The flags of a worker's options (`systemPrompt`, `extensionPaths`, `noSkills`, `noContextFiles`).
    pub fn from_worker_options(options: &serde_json::Value) -> SessionFlags {
        SessionFlags {
            system_prompt: options["systemPrompt"].as_str().map(str::to_string),
            extension_paths: options["extensionPaths"]
                .as_array()
                .map(|paths| paths.iter().filter_map(|p| p.as_str()).map(str::to_string).collect())
                .unwrap_or_default(),
            no_skills: options["noSkills"] == true,
            no_context_files: options["noContextFiles"] == true,
        }
    }

    /// Sets the worker options keys of the flags that are set (the inverse of [`SessionFlags::from_worker_options`]).
    pub fn write_worker_options(&self, options: &mut serde_json::Value) {
        if let Some(prompt) = &self.system_prompt {
            options["systemPrompt"] = prompt.as_str().into();
        }
        if !self.extension_paths.is_empty() {
            options["extensionPaths"] = self.extension_paths.clone().into();
        }
        if self.no_skills {
            options["noSkills"] = true.into();
        }
        if self.no_context_files {
            options["noContextFiles"] = true.into();
        }
    }
}

/// The extensions a session loads: the `-e` manifests, then (without `--no-extensions`) the discovered ones.
pub fn extension_discovery(
    env: &Env,
    agent_dir: &Path,
    settings: &config::settings::SettingsManager,
    flags: &SessionFlags,
    no_extensions: bool,
) -> extensions::Discovery {
    let cli = extensions::manifest::discover_cli_paths(&flags.extension_paths, &env.cwd, &env.home);
    let discovered =
        (!no_extensions).then(|| extensions::discover_with_settings(&env.cwd, agent_dir, &env.home, settings));
    extensions::manifest::merge_discoveries(cli, discovered)
}

/// The `/` commands without the skill commands (`skill:<name>`) when `--no-skills` left the skills out.
pub fn filter_commands(commands: Vec<serde_json::Value>, flags: &SessionFlags) -> Vec<serde_json::Value> {
    if !flags.no_skills {
        return commands;
    }
    commands.into_iter().filter(|c| c["source"] != "skill").collect()
}
