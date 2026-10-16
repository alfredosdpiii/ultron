//! The runtime guide as text (guide.ts), for Ultron's own model (`ultron guide`) and for Claude Code driving the
//! REPL over MCP (`ultron guide --for claude`), plus the context files and skills native Ultron would put in its
//! system prompt (resources.ts). `ultron claude` passes the Claude variant as Claude Code's whole system prompt.

use resources::context_files::ContextFile;
use resources::runtime_guide::{EventDelivery, RuntimeGuideOptions, rlm_runtime_prompt};
use resources::skills::Skill;
use resources::system_prompt::{BuildOptions, build_system_prompt_sections};
use std::path::Path;

/// The tool as Claude Code names it: server `ultron`, tool `rlm`.
pub const CLAUDE_RLM_TOOL: &str = "mcp__ultron__rlm";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Audience {
    Ultron,
    Claude,
    ClaudeChild,
}

/// The context files (AGENTS.md / CLAUDE.md) and skills native Ultron loads for a directory.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PromptResources {
    pub context_files: Vec<ContextFile>,
    pub skills: Vec<Skill>,
    /// Engineering mode when loaded: the always-on skills' text goes in the prompt.
    pub engineering: bool,
}

/// `--no-context-files` / `--no-skills`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResourceFlags {
    pub no_context_files: bool,
    pub no_skills: bool,
}

/// Where resources are read from.
pub struct ResourceDirs<'a> {
    pub cwd: &'a Path,
    pub agent_dir: &'a Path,
    pub home: &'a Path,
    /// The bundled skills directory (`<package>/skills`), unless `ULTRON_BUNDLED_SKILLS` turns it off.
    pub bundled_skills: Option<&'a Path>,
}

/// Whether `ULTRON_BUNDLED_SKILLS` leaves the bundled skills on.
pub fn bundled_skills_enabled(value: Option<&str>) -> bool {
    !matches!(
        value.map(|v| v.trim().to_lowercase()).as_deref(),
        Some("off" | "0" | "false")
    )
}

/// `workerProjectTrusted`: nothing to trust, else the stored decision, else `defaultProjectTrust` is not "never".
pub fn project_trusted(cwd: &Path, agent_dir: &Path, home: &Path) -> bool {
    if !config::trust::has_trust_requiring_project_resources(cwd, home) {
        return true;
    }
    if let Ok(Some(decision)) = config::trust::ProjectTrustStore::new(agent_dir).get(cwd) {
        return decision;
    }
    config::settings::SettingsManager::create(cwd, agent_dir, false).default_project_trust() != "never"
}

/// `loadClaudePromptResources`: what native Ultron would load in `cwd` (a distrusted project loads no project
/// skills).
pub fn load_prompt_resources(dirs: &ResourceDirs, flags: ResourceFlags) -> PromptResources {
    if flags.no_context_files && flags.no_skills {
        return PromptResources::default();
    }
    let cwd = dirs.cwd.display().to_string();
    let agent_dir = dirs.agent_dir.display().to_string();
    let trusted = project_trusted(dirs.cwd, dirs.agent_dir, dirs.home);
    let settings = config::settings::SettingsManager::create(dirs.cwd, dirs.agent_dir, trusted);
    let mut skills = Vec::new();
    if !flags.no_skills {
        skills = resources::skills::load_skills(&resources::skills::LoadSkillsOptions {
            cwd: &cwd,
            agent_dir: &agent_dir,
            skill_paths: &[],
            include_defaults: true,
        })
        .skills;
        if !trusted {
            skills.retain(|s| s.source != "project");
        }
        // The resource loader resolves project resources before the user's (package-manager order).
        skills.sort_by_key(|s| s.source != "project");
        if let Some(bundled) = dirs.bundled_skills {
            skills.extend(resources::skills::load_skills_from_dir(&bundled.display().to_string(), "bundled").skills);
        }
    }
    let context_files = if flags.no_context_files {
        Vec::new()
    } else {
        resources::context_files::load_project_context_files(&cwd, &agent_dir)
    };
    PromptResources {
        context_files,
        skills,
        engineering: settings.settings().get("engineering") == Some(&serde_json::Value::Bool(true)),
    }
}

/// The `<project_context>` and `<skills>` sections as native Ultron renders them for the REPL-only tool set.
pub fn render_prompt_resources(resources: &PromptResources, bundled_skills: Option<&Path>) -> Vec<String> {
    if resources.context_files.is_empty() && resources.skills.is_empty() {
        return Vec::new();
    }
    let options = BuildOptions {
        custom_prompt: Some("-".into()),
        selected_tools: vec!["rlm".into()],
        context_files: resources.context_files.clone(),
        skills: resources.skills.clone(),
        engineering: resources.engineering,
        bundled_skills_dir: bundled_skills.map(|p| p.display().to_string()),
        ..BuildOptions::new("")
    };
    let Ok(sections) = build_system_prompt_sections(&options) else {
        return Vec::new();
    };
    ["project_context", "skills"]
        .into_iter()
        .filter_map(|name| {
            sections
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, text)| text.clone())
        })
        .collect()
}

/// What `--print-config` shows: paths and names only, never file contents.
pub fn describe_prompt_resources(resources: &PromptResources) -> serde_json::Value {
    serde_json::json!({
        "contextFiles": resources.context_files.iter().map(|f| f.path.clone()).collect::<Vec<_>>(),
        "skills": resources.skills.iter().filter(|s| !s.disable_model_invocation).map(|s| s.name.clone()).collect::<Vec<_>>(),
    })
}

/// Options of the Claude system prompt.
#[derive(Clone, Debug, Default)]
pub struct GuideOptions<'a> {
    pub cwd: Option<String>,
    pub platform: Option<String>,
    pub date: Option<String>,
    /// For a subagent: how many more levels it may delegate.
    pub allowance: Option<u64>,
    pub resources: Option<&'a PromptResources>,
    pub bundled_skills: Option<&'a Path>,
}

/// The runtime guide for Claude Code: next-call event delivery, no `ctx` (Claude Code owns its context).
pub fn claude_runtime_guide(hints: bool) -> String {
    let options = RuntimeGuideOptions {
        delivery: EventDelivery::NextCall,
        external_root: true,
        hints,
        ..RuntimeGuideOptions::default()
    };
    rlm_runtime_prompt(&["rlm"], &options).unwrap_or_default()
}

/// `hintsEnabled`: `ULTRON_HINTS` is on, 1, true or yes.
pub fn hints_enabled(value: Option<&str>) -> bool {
    matches!(
        value.map(|v| v.trim().to_lowercase()).as_deref(),
        Some("on" | "1" | "true" | "yes")
    )
}

fn root_identity() -> String {
    format!(
        "You are Claude, the root agent of Ultron, a recursive language-model runtime, running inside Claude Code. You help the user with software engineering and data work in their working directory.

# Your tool
You have exactly one tool, `rlm` (listed as `{CLAUDE_RLM_TOOL}`). It runs Python in a persistent kernel on the user's machine, in the working directory. Everything goes through it: reading and searching files, shell commands (`await bash('''...''')`), edits (`await edit(...)`), tests, data processing, bounded sub-model calls and subagents. There are no other tools; do not ask for any. A call runs until its cell ends. If Claude Code moves an `rlm` call to the background, it keeps running and its result arrives as a notification: never call `rlm` again just to wait for it."
    )
}

const WORKING_WITH_THE_USER: &str = "# Working with the user
- Be concise and direct. Answer the question first; skip preamble and summaries of what you are about to do. Use GitHub-flavored Markdown; cite code as `path:line`.
- Do what was asked, nothing more. Prefer editing existing files to creating new ones; do not write documentation files unless asked. Match the surrounding code's style, and check that a library is already used before relying on it.
- Before anything destructive or hard to reverse (deleting files or data, `rm -rf`, `git reset --hard`, `git push --force`, rewriting history, killing processes you did not start, changing global or system configuration, touching credentials), ask first unless the user asked for exactly that. Never commit or push unless asked.
- Never print, log or commit secrets. Output that looks like a secret is masked for you; do not try to recover it.
- Do not help with malicious code (malware, credential theft, attacks on systems you are not authorized to test).
- Verify your work: run the relevant tests, build or a direct check through `bash`, and say plainly what you verified and what you could not.
- If a request is ambiguous in a way that changes the result, ask one short question; otherwise make a reasonable choice and state it.";

fn child_identity() -> String {
    format!(
        "You are a subagent of Ultron, a recursive language-model runtime, running as Claude Code. A parent agent gave you the brief in the user message; no person is watching and nobody can answer questions, so do the brief yourself with your best judgment.

# Your tool
You have exactly one tool, `rlm` (listed as `{CLAUDE_RLM_TOOL}`). It runs Python in your own persistent kernel, in the working directory you share with your parent (files yes, conversation no). Everything goes through it: files, shell (`await bash('''...''')`), edits (`await edit(...)`), tests, data, bounded sub-model calls.

# Finishing
When the brief is done (or cannot be done), record your verdict in a cell: `await rlm.finish(status, summary, evidence=[...], changed_files=[...])` with status \"passed\", \"failed\" or \"blocked\", concrete evidence (commands with their outcome, files with lines) and every file you created, edited or deleted. The host checks changed_files against the files that changed while you ran. Then reply briefly: what you did, paths, and any uncertainty. Stay inside the brief: never do destructive things it did not ask for, and never commit or push unless it says so. In a worktree of your own (a `[Worktree]` note says so) never commit, even if the brief asks: the host commits your work when you finish."
    )
}

fn delegation_allowance(allowance: u64) -> String {
    if allowance > 0 {
        format!(
            "You may delegate: `rlm.spawn` works for you, up to {allowance} more level{} (`depth=` for your children at most {}).",
            if allowance == 1 { "" } else { "s" },
            allowance - 1
        )
    } else {
        "You do not delegate: `rlm.spawn` is refused for you. `rlm.map` and `rlm.infer` (bounded sub-model calls) work."
            .into()
    }
}

/// `Infinity` as JS prints it in the allowance sentence (a root server's default allowance).
pub const UNLIMITED: u64 = u64::MAX;

fn environment(options: &GuideOptions) -> Option<String> {
    let lines: Vec<String> = [
        options.cwd.as_ref().map(|v| format!("- Working directory: {v}")),
        options.platform.as_ref().map(|v| format!("- Platform: {v}")),
        options.date.as_ref().map(|v| format!("- Date: {v}")),
    ]
    .into_iter()
    .flatten()
    .collect();
    (!lines.is_empty()).then(|| format!("# Environment\n{}", lines.join("\n")))
}

/// Claude Code's whole system prompt for an Ultron root (`Claude`) or subagent (`ClaudeChild`).
pub fn claude_system_prompt(audience: Audience, options: &GuideOptions, hints: bool) -> String {
    let mut parts = if audience == Audience::Claude {
        vec![
            root_identity(),
            WORKING_WITH_THE_USER.to_string(),
            claude_runtime_guide(hints),
        ]
    } else {
        let allowance = options.allowance.unwrap_or(0);
        let text = if allowance == UNLIMITED {
            "You may delegate: `rlm.spawn` works for you, up to Infinity more levels (`depth=` for your children at most Infinity).".to_string()
        } else {
            delegation_allowance(allowance)
        };
        vec![child_identity(), text, claude_runtime_guide(hints)]
    };
    if let Some(resources) = options.resources {
        parts.extend(render_prompt_resources(resources, options.bundled_skills));
    }
    parts.extend(environment(options));
    parts.join("\n\n")
}

/// `RLM_TOOL_DESCRIPTION`.
pub fn rlm_tool_description() -> String {
    session_core::lane::rlm_tool().description
}

/// The guide for an audience: Ultron's own runtime guide, or Claude Code's system prompt.
pub fn runtime_guide(audience: Audience, options: &GuideOptions, hints: bool) -> String {
    match audience {
        Audience::Ultron => {
            let guide = rlm_runtime_prompt(
                &["rlm"],
                &RuntimeGuideOptions {
                    hints,
                    ..RuntimeGuideOptions::default()
                },
            )
            .unwrap_or_default();
            format!("{}\n\n{guide}", rlm_tool_description())
        }
        other => claude_system_prompt(other, options, hints),
    }
}

/// `process.platform` of this build.
pub fn platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

/// Today's date in UTC (`new Date().toISOString().slice(0, 10)`).
pub fn utc_date() -> String {
    session_file::js::iso_string(session_core::lane::now_ms())[..10].to_string()
}
