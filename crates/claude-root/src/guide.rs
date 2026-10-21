//! The runtime guide as text (guide.ts), for Ultron's own model (`ultron guide`) and for Claude Code driving the
//! REPL over MCP (`ultron guide --for claude`), plus the context files and skills native Ultron would put in its
//! system prompt (resources.ts). `ultron claude` passes the Claude variant as Claude Code's whole system prompt.

use resources::context_files::ContextFile;
use resources::runtime_guide::{EventDelivery, RuntimeGuideOptions, rlm_runtime_prompt};
use resources::skills::Skill;
use resources::system_prompt::{BuildOptions, build_system_prompt_sections};
use session_core::RunEnv;
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
    /// Engineering mode outside the TypeScript guide (`ULTRON_GUIDE=ts`): the bundled skills are used without being
    /// asked (resources::skills::bundled_skills_note).
    pub proactive_skills: bool,
    /// `"persona": "ultron"`: the text of `<agent dir>/personality.md` (resources::persona), for the root's prompt.
    pub persona: Option<String>,
    /// The discovered SYSTEM.md (`.pi/SYSTEM.md` in a trusted project, else `<agentDir>/SYSTEM.md`): in place of the
    /// working rules of the root's prompt, as native Ultron puts it in place of Pi's preamble and rules.
    pub system_prompt: Option<String>,
    /// The discovered APPEND_SYSTEM.md texts: the addendum of every prompt (root and subagents), as in native Ultron.
    pub append_system_prompt: Vec<String>,
    /// Loki's policy note (`readContext`), when Loki is on and its policy is readable: in every prompt, as native
    /// Ultron puts it in its system prompt (subagents and a root without Ultron's hooks get no SessionStart context).
    pub loki: Option<String>,
}

/// Loki's policy note for a session in `env`'s directory (none when Loki is off or its policy is unreadable).
pub fn loki_note(env: &RunEnv) -> Option<String> {
    loki_note_at(session_core::compose::Config::from_env(env).loki_root.as_deref())
}

/// Loki's policy note for the policy under `root`.
pub fn loki_note_at(root: Option<&Path>) -> Option<String> {
    let config = loki::config::load_config(root?).ok()?;
    loki::injected_context(&config, Some("ultron"))
        .ok()
        .filter(|note| !note.is_empty())
}

/// The kernel APIs the runtime guide names for a session in `env`'s directory, as native Ultron's guide names them
/// (session_core::compose::mcp_runtime_guide: the `mcp.json` servers through `mcp`, the Rust host's APIs), minus what
/// an external root cannot serve (see [`external_root_apis`]).
/// `ULTRON_GUIDE=ts` keeps the TypeScript guide's set (it names neither `mcp.json` servers nor the Rust APIs).
pub fn runtime_apis(env: &RunEnv) -> RuntimeGuideOptions {
    if (env.var)("ULTRON_GUIDE").as_deref() == Some("ts") {
        return RuntimeGuideOptions::default();
    }
    external_root_apis(session_core::compose::mcp_runtime_guide(
        &session_core::compose::Config::from_env(env),
    ))
}

/// What `ultron claude` serves of native Ultron's APIs: all of them, persistent children with `rlm.ask`, the swarm
/// (`swarm`, `board`, `notes`) and `schedule` included (crate::durable_apis, crate::schedules). It is an external
/// root's guide: Claude Code owns its context, so there is no `ctx`.
pub fn external_root_apis(native: RuntimeGuideOptions) -> RuntimeGuideOptions {
    RuntimeGuideOptions {
        external_root: true,
        ..native
    }
}

/// `--no-context-files` / `--no-skills`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResourceFlags {
    pub no_context_files: bool,
    pub no_skills: bool,
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
    config::trust::worker_project_trusted(cwd, agent_dir, home)
}

/// `loadClaudePromptResources`: what native Ultron would load in `env`'s directory, with the session worker's
/// resource loader (crate session-core `session_resources`: the settings' `skills` entries, the
/// auto-discovered `.pi/skills`, `.agents/skills` up to the git root, `<agentDir>/skills` and `~/.agents/skills`,
/// then the bundled skills in `bundled`, the first skill of a name kept), settings and project trust (a distrusted
/// project contributes no settings and no project skills). `bundled` is the bundled skills directory, unless
/// `ULTRON_BUNDLED_SKILLS` turned it off.
pub fn load_prompt_resources(env: &RunEnv, bundled: Option<&Path>, flags: ResourceFlags) -> PromptResources {
    let agent_dir_path = env.agent_dir();
    let trusted = project_trusted(&env.cwd, &agent_dir_path, &env.home);
    let settings = config::settings::SettingsManager::create(&env.cwd, &agent_dir_path, trusted);
    // The persona does not depend on the context-file and skill flags; an unreadable file gives no section.
    let persona = if resources::persona::enabled(settings.settings()) {
        resources::persona::read_personality(&agent_dir_path).ok().flatten()
    } else {
        None
    };
    // SYSTEM.md and APPEND_SYSTEM.md shape the prompt whatever the context-file and skill flags say (native Ultron).
    let files = resources::prompt_files::load_prompt_files(
        &env.cwd.display().to_string(),
        &agent_dir_path.display().to_string(),
        trusted,
        None,
        None,
    );
    let shaped = PromptResources {
        persona,
        system_prompt: files.system_prompt.filter(|text| !text.trim().is_empty()),
        append_system_prompt: files.append_system_prompt,
        loki: loki_note(env),
        ..PromptResources::default()
    };
    if flags.no_context_files && flags.no_skills {
        return shaped;
    }
    let engineering = settings.settings().get("engineering") == Some(&serde_json::Value::Bool(true));
    let cwd = env.cwd.display().to_string();
    let agent_dir = agent_dir_path.display().to_string();
    let skills = if flags.no_skills {
        Vec::new()
    } else {
        let resources = session_core::session_resources::resolve_default(env);
        let bundled = bundled.map(|dir| dir.display().to_string());
        let mut skills =
            session_core::session_resources::skills(&cwd, &agent_dir, &resources, false, bundled.as_deref());
        // `alwaysSkills` carry their text into the prompt as in native Ultron (session_core::lane); outside
        // engineering mode the prompt lists them only (resources::system_prompt).
        let always = session_core::lane::PromptMode::from_settings(&settings).always_skills;
        for diagnostic in resources::skills::apply_always_skills(&mut skills, &always) {
            eprintln!("Warning: {}", diagnostic.message);
        }
        skills
    };
    let context_files = if flags.no_context_files {
        Vec::new()
    } else {
        resources::context_files::load_project_context_files(&cwd, &agent_dir)
    };
    PromptResources {
        context_files,
        skills,
        engineering,
        proactive_skills: engineering && (env.var)("ULTRON_GUIDE").as_deref() != Some("ts"),
        ..shaped
    }
}

/// The `<project_context>` and `<skills>` sections as native Ultron renders them for the REPL-only tool set; for the
/// root (`root`), the `<persona>` section between them when the persona is on.
pub fn render_prompt_resources(resources: &PromptResources, bundled_skills: Option<&Path>, root: bool) -> Vec<String> {
    let persona = resources
        .persona
        .as_deref()
        .filter(|_| root)
        .map(|text| resources::persona::claude_persona_text(text, bundled_skills));
    if resources.context_files.is_empty() && resources.skills.is_empty() && persona.is_none() {
        return Vec::new();
    }
    let options = BuildOptions {
        custom_prompt: Some("-".into()),
        selected_tools: vec!["rlm".into()],
        context_files: resources.context_files.clone(),
        skills: resources.skills.clone(),
        engineering: resources.engineering,
        bundled_skills_dir: bundled_skills.map(|p| p.display().to_string()),
        persona,
        proactive_skills: resources.proactive_skills,
        ..BuildOptions::new("")
    };
    let Ok(sections) = build_system_prompt_sections(&options) else {
        return Vec::new();
    };
    ["project_context", "persona", "skills"]
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
    /// The root is woken when work it started ends while it is idle (Ultron's hooks are on): the guide says to end
    /// the turn instead of awaiting everything first.
    pub wake: bool,
    /// The kernel APIs the guide names ([`runtime_apis`]); none: the TypeScript guide's set.
    pub apis: Option<&'a RuntimeGuideOptions>,
}

/// The runtime guide for Claude Code: next-call event delivery, no `ctx` (Claude Code owns its context).
pub fn claude_runtime_guide(hints: bool) -> String {
    claude_runtime_guide_with(hints, false)
}

/// Whether the root is woken by its events: Ultron's hooks are on (`hooks`) and the guide is not TypeScript's
/// (`ULTRON_GUIDE=ts` keeps its next-call wording).
pub fn wakes(var: &dyn Fn(&str) -> Option<String>, hooks: bool) -> bool {
    hooks && var("ULTRON_GUIDE").as_deref() != Some("ts")
}

/// [`claude_runtime_guide`]; `wake`: events wake the idle root (crate::hooks::WAIT_EVENTS), as in native Ultron.
pub fn claude_runtime_guide_with(hints: bool, wake: bool) -> String {
    claude_runtime_guide_for(None, hints, wake)
}

/// The runtime guide for Claude Code naming the kernel APIs `apis` lists ([`runtime_apis`]; none: the TypeScript
/// guide's set), its events delivered with the next result unless `wake`.
pub fn claude_runtime_guide_for(apis: Option<&RuntimeGuideOptions>, hints: bool, wake: bool) -> String {
    let options = RuntimeGuideOptions {
        delivery: if wake {
            EventDelivery::Wake
        } else {
            EventDelivery::NextCall
        },
        external_root: true,
        hints,
        // A woken root's rlm.collect never waits (the server's `--wake`, session_core::wake).
        host_waits: wake,
        ..apis.cloned().unwrap_or_default()
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
    let resources = options.resources;
    // SYSTEM.md: the user's own base prompt, in place of the root's working rules (a subagent has none: it gets the
    // text after its identity, as a native subagent gets the root's prompt). APPEND_SYSTEM.md follows the base.
    let custom = resources.and_then(|r| r.system_prompt.clone());
    let addendum = resources
        .map(|r| r.append_system_prompt.join("\n\n"))
        .filter(|text| !text.is_empty());
    let mut parts = if audience == Audience::Claude {
        vec![
            root_identity(),
            custom.unwrap_or_else(|| WORKING_WITH_THE_USER.to_string()),
        ]
    } else {
        vec![child_identity()].into_iter().chain(custom).collect()
    };
    parts.extend(addendum);
    if audience == Audience::Claude {
        parts.push(claude_runtime_guide_for(options.apis, hints, options.wake));
    } else {
        let allowance = options.allowance.unwrap_or(0);
        let text = if allowance == UNLIMITED {
            "You may delegate: `rlm.spawn` works for you, up to Infinity more levels (`depth=` for your children at most Infinity).".to_string()
        } else {
            delegation_allowance(allowance)
        };
        // A Claude Code subagent's server serves neither the session's shared world (persistent children, swarm,
        // board, notes) nor `schedule` (crate::unavailable::child_refusal): an external root's subagent reader
        // leaves them out (its contracts stay). The TypeScript guide's set (`ULTRON_GUIDE=ts`) names none of them
        // and keeps TypeScript's wording.
        let apis = options.apis.map(|apis| RuntimeGuideOptions {
            reader: if apis.durable_apis {
                resources::runtime_guide::GuideReader::Subagent { swarm: true }
            } else {
                apis.reader
            },
            ..apis.clone()
        });
        parts.extend([text, claude_runtime_guide_for(apis.as_ref(), hints, false)]);
    }
    if let Some(resources) = resources {
        parts.extend(render_prompt_resources(
            resources,
            options.bundled_skills,
            audience == Audience::Claude,
        ));
        parts.extend(resources.loki.clone());
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
