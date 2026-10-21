//! The resource flags a session worker is started with (`SessionWorkerOptions` of experimental/session-worker.ts,
//! from the command line through native-command.ts and the server's model options):
//!
//! - `--system-prompt <text>`: the system prompt is exactly this text (`currentSystemPrompt` returns it unrendered);
//! - `--append-system-prompt <text or file>` (repeatable): appended to the rendered prompt in place of a discovered
//!   APPEND_SYSTEM.md. The CLI reads a file input in its own directory and sends the text (the worker's cwd may
//!   differ). The TS server never passed it to a worker; ultron-rs does, as `appendSystemPrompt`;
//! - `--no-skills`: no skills in the prompt, the `skills.*` catalog or the command list (bundled ones included);
//! - `--no-context-files`: no AGENTS.md / CLAUDE.md in the prompt;
//! - `--skill <path>` (repeatable, `skillPaths`; absolute, the CLI resolves them in its own directory): skills loaded
//!   after the discovered ones (resource-loader.ts `additionalSkillPaths`), the first skill of a name kept.
//!
//! The tool flags (`--tools`, `--no-tools`, ...) are not here: the model's tool list is exactly `[rlm]`. Neither are
//! `-e`/`--extension` and `--no-extensions`: Pi extensions are not supported by ultron-rs (the CLI warns and ignores
//! them), and a worker ignores `extensionPaths` and `promptPaths` options.

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionFlags {
    pub system_prompt: Option<String>,
    pub append_system_prompt: Option<Vec<String>>,
    pub no_skills: bool,
    pub no_context_files: bool,
    pub skill_paths: Vec<String>,
}

impl SessionFlags {
    /// The flags of a worker's options (`systemPrompt`, `appendSystemPrompt`, `noSkills`, `noContextFiles`).
    pub fn from_worker_options(options: &serde_json::Value) -> SessionFlags {
        SessionFlags {
            system_prompt: options["systemPrompt"].as_str().map(str::to_string),
            append_system_prompt: options["appendSystemPrompt"].as_array().map(|list| {
                list.iter()
                    .filter_map(|text| text.as_str().map(str::to_string))
                    .collect()
            }),
            no_skills: options["noSkills"] == true,
            no_context_files: options["noContextFiles"] == true,
            skill_paths: options["skillPaths"]
                .as_array()
                .map(|list| list.iter().filter_map(|p| p.as_str().map(str::to_string)).collect())
                .unwrap_or_default(),
        }
    }

    /// Sets the worker options keys of the flags that are set (the inverse of [`SessionFlags::from_worker_options`]).
    pub fn write_worker_options(&self, options: &mut serde_json::Value) {
        if let Some(prompt) = &self.system_prompt {
            options["systemPrompt"] = prompt.as_str().into();
        }
        if let Some(appends) = &self.append_system_prompt {
            options["appendSystemPrompt"] = appends.clone().into();
        }
        if self.no_skills {
            options["noSkills"] = true.into();
        }
        if self.no_context_files {
            options["noContextFiles"] = true.into();
        }
        if !self.skill_paths.is_empty() {
            options["skillPaths"] = self.skill_paths.clone().into();
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
