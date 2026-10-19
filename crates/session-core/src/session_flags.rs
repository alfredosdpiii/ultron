//! The resource flags a session worker is started with (`SessionWorkerOptions` of experimental/session-worker.ts,
//! from the command line through native-command.ts and the server's model options):
//!
//! - `--system-prompt <text>`: the system prompt is exactly this text (`currentSystemPrompt` returns it unrendered);
//! - `--no-skills`: no skills in the prompt, the `skills.*` catalog or the command list (bundled ones included);
//! - `--no-context-files`: no AGENTS.md / CLAUDE.md in the prompt.
//!
//! The tool flags (`--tools`, `--no-tools`, ...) are not here: the model's tool list is exactly `[rlm]`. Neither are
//! `-e`/`--extension` and `--no-extensions`: Pi extensions are not supported by ultron-rs (the CLI warns and ignores
//! them), and a worker ignores `extensionPaths`, `skillPaths` and `promptPaths` options.

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionFlags {
    pub system_prompt: Option<String>,
    pub no_skills: bool,
    pub no_context_files: bool,
}

impl SessionFlags {
    /// The flags of a worker's options (`systemPrompt`, `noSkills`, `noContextFiles`).
    pub fn from_worker_options(options: &serde_json::Value) -> SessionFlags {
        SessionFlags {
            system_prompt: options["systemPrompt"].as_str().map(str::to_string),
            no_skills: options["noSkills"] == true,
            no_context_files: options["noContextFiles"] == true,
        }
    }

    /// Sets the worker options keys of the flags that are set (the inverse of [`SessionFlags::from_worker_options`]).
    pub fn write_worker_options(&self, options: &mut serde_json::Value) {
        if let Some(prompt) = &self.system_prompt {
            options["systemPrompt"] = prompt.as_str().into();
        }
        if self.no_skills {
            options["noSkills"] = true.into();
        }
        if self.no_context_files {
            options["noContextFiles"] = true.into();
        }
    }
}

/// The `/` commands without the skill commands (`skill:<name>`) when `--no-skills` left the skills out.
pub fn filter_commands(commands: Vec<serde_json::Value>, flags: &SessionFlags) -> Vec<serde_json::Value> {
    if !flags.no_skills {
        return commands;
    }
    commands.into_iter().filter(|c| c["source"] != "skill").collect()
}
