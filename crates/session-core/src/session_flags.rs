//! The resource flags a session worker is started with (`SessionWorkerOptions` of experimental/session-worker.ts,
//! from the command line through native-command.ts and the server's model options):
//!
//! - `--system-prompt <text>`: the system prompt is exactly this text (`currentSystemPrompt` returns it unrendered);
//! - `-e`/`--extension <path>`: extensions loaded before the discovered ones, also with `--no-extensions`; the
//!   sources' skills and prompts (`skillPaths`, `promptPaths`, which the CLI resolved when it installed them) load
//!   before the discovered ones, also with `--no-skills`;
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
    /// The `-e` sources' skill files and directories (`resolveExtensionSources(...).skills`), absolute.
    pub skill_paths: Vec<String>,
    /// The `-e` sources' prompt template files and directories, absolute.
    pub prompt_paths: Vec<String>,
    pub no_skills: bool,
    pub no_context_files: bool,
}

impl SessionFlags {
    /// The flags of a worker's options (`systemPrompt`, `extensionPaths`, `skillPaths`, `promptPaths`, `noSkills`,
    /// `noContextFiles`).
    pub fn from_worker_options(options: &serde_json::Value) -> SessionFlags {
        let strings = |key: &str| -> Vec<String> {
            options[key]
                .as_array()
                .map(|paths| paths.iter().filter_map(|p| p.as_str()).map(str::to_string).collect())
                .unwrap_or_default()
        };
        SessionFlags {
            system_prompt: options["systemPrompt"].as_str().map(str::to_string),
            extension_paths: strings("extensionPaths"),
            skill_paths: strings("skillPaths"),
            prompt_paths: strings("promptPaths"),
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
        if !self.skill_paths.is_empty() {
            options["skillPaths"] = self.skill_paths.clone().into();
        }
        if !self.prompt_paths.is_empty() {
            options["promptPaths"] = self.prompt_paths.clone().into();
        }
        if self.no_skills {
            options["noSkills"] = true.into();
        }
        if self.no_context_files {
            options["noContextFiles"] = true.into();
        }
    }
}

/// The extensions a session loads: the `-e` manifests, then (without `--no-extensions`) the discovered ones and the
/// settings packages' (`resources`, last: a package ranks after every local and auto-discovered resource).
pub fn extension_discovery(
    env: &Env,
    agent_dir: &Path,
    settings: &config::settings::SettingsManager,
    flags: &SessionFlags,
    no_extensions: bool,
) -> extensions::Discovery {
    extension_discovery_with(env, agent_dir, settings, flags, no_extensions, &Default::default())
}

/// [`extension_discovery`] with the session's resolved package resources.
pub fn extension_discovery_with(
    env: &Env,
    agent_dir: &Path,
    settings: &config::settings::SettingsManager,
    flags: &SessionFlags,
    no_extensions: bool,
    resources: &crate::session_resources::SessionResources,
) -> extensions::Discovery {
    let cli = extensions::manifest::discover_cli_paths(&flags.extension_paths, &env.cwd, &env.home);
    let discovered = (!no_extensions).then(|| {
        let mut discovered = extensions::discover_with_settings(&env.cwd, agent_dir, &env.home, settings);
        let manifests: Vec<String> = resources
            .package_manifests
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        let packages = extensions::manifest::discover_cli_paths(&manifests, &env.cwd, &env.home);
        for manifest in packages.manifests {
            if !discovered.manifests.iter().any(|m| m.path == manifest.path)
                && !discovered.disabled.contains(&manifest.path)
            {
                discovered.manifests.push(manifest);
            }
        }
        discovered.diagnostics.extend(packages.diagnostics);
        discovered
    });
    extensions::manifest::merge_discoveries(cli, discovered)
}

/// The `/` commands without the skill commands (`skill:<name>`) when `--no-skills` left the skills out; the `-e`
/// sources' skills (`sourceInfo.source` `cli`) stay, as their skills still load.
pub fn filter_commands(commands: Vec<serde_json::Value>, flags: &SessionFlags) -> Vec<serde_json::Value> {
    if !flags.no_skills {
        return commands;
    }
    commands
        .into_iter()
        .filter(|c| c["source"] != "skill" || c["sourceInfo"]["source"] == "cli")
        .collect()
}
