//! The resources a session loads, as resource-loader.ts `reload` assembles them for the session worker:
//!
//! - every local resource (crate::local_resources) in its precedence order: the settings' local `skills`/`prompts`
//!   entries (paths, globs and `!`/`+`/`-` patterns, project scope first), and the auto-discovered ones
//!   (`.pi/skills`, `.pi/prompts`, `.agents/skills` from the cwd up to the git root, `<agentDir>/skills`,
//!   `<agentDir>/prompts`, `~/.agents/skills`). A project that is not trusted contributes no settings, no project
//!   resources and no ancestor `.agents/skills`;
//! - then the bundled skills (`bundledSkillPaths`), the first skill of a name kept;
//! - the base system prompt and its addendum: `.pi/SYSTEM.md` / `.pi/APPEND_SYSTEM.md` in a trusted project, else
//!   `<agentDir>/SYSTEM.md` / `<agentDir>/APPEND_SYSTEM.md` (`discoverSystemPromptFile`).
//!
//! Pi packages (the settings `packages`) and the `-e` sources are not supported by ultron-rs: they contribute
//! nothing (the CLI notes the ignored packages once at startup).
//!
//! Every skill and prompt carries its `SourceInfo` as `findSourceInfoForPath` gives it (the resolved resource's
//! metadata, exact path first, then the first resource directory holding it), else the loader's own.

use crate::RunEnv as Env;
use crate::local_resources::{LocalResources, PathMetadata};
use resources::prompt_templates::PromptTemplate;
use resources::skills::{LoadSkillsOptions, Skill, load_skills};
use std::path::Path;

/// Where one resource came from (`PathMetadata` with its path): its source (`local` or `auto`), scope and origin
/// (always `top-level`), and its base directory.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResourceSource {
    pub path: String,
    pub source: String,
    pub scope: String,
    pub origin: String,
    pub base_dir: Option<String>,
}

impl ResourceSource {
    fn of(path: &str, metadata: &PathMetadata) -> ResourceSource {
        ResourceSource {
            path: path.to_string(),
            source: metadata.source.clone(),
            scope: metadata.scope.name().into(),
            origin: "top-level".into(),
            base_dir: metadata.base_dir.clone(),
        }
    }

    /// `createSourceInfo(path, metadata)`.
    pub fn source_info(&self, path: &str) -> serde_json::Value {
        let mut info = serde_json::json!({
            "path": path,
            "source": self.source,
            "scope": self.scope,
            "origin": self.origin,
        });
        if let Some(base) = &self.base_dir {
            info["baseDir"] = base.as_str().into();
        }
        info
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionResources {
    /// `resolve()`'s enabled skills in precedence order. They are files already (`mapSkillPath` has nothing to map:
    /// the resolver lists a skill directory's `SKILL.md`).
    pub skill_paths: Vec<String>,
    /// `resolve()`'s enabled prompt templates in precedence order.
    pub prompt_paths: Vec<String>,
    /// `metadataByPath` in insertion order: every resolved resource (enabled or not); the first entry of a path
    /// kept.
    pub metadata: Vec<ResourceSource>,
    /// The discovered SYSTEM.md's text (`getSystemPrompt`), if any.
    pub system_prompt: Option<String>,
    /// The discovered APPEND_SYSTEM.md's text (`getAppendSystemPrompt`).
    pub append_system_prompt: Vec<String>,
}

/// `mergePaths`: each path (resolved against the cwd) once by its canonical path, first place kept.
fn merge(cwd: &str, paths: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for path in paths {
        let resolved = resources::paths::resolve_path(&path, cwd, true);
        let canonical = resources::paths::canonicalize(&resolved);
        if !seen.contains(&canonical) {
            seen.push(canonical);
            out.push(resolved);
        }
    }
    out
}

/// The session's resources for the settings `settings` holds (with the session's project trust).
pub fn resolve(env: &Env, settings: config::settings::SettingsManager) -> SessionResources {
    let cwd = resources::paths::resolve_path(&env.cwd.display().to_string(), "/", false);
    let agent_dir = env.agent_dir().display().to_string();
    let trusted = settings.is_project_trusted();
    let prompt_files = resources::prompt_files::load_prompt_files(&cwd, &agent_dir, trusted, None, None);
    let resolved = LocalResources::new(env, settings).resolve();
    let mut metadata: Vec<ResourceSource> = Vec::new();
    for list in [&resolved.skills, &resolved.prompts, &resolved.themes] {
        for r in list {
            if !metadata.iter().any(|m| m.path == r.path) {
                metadata.push(ResourceSource::of(&r.path, &r.metadata));
            }
        }
    }
    let enabled = |list: &[crate::local_resources::ResolvedResource]| -> Vec<String> {
        list.iter().filter(|r| r.enabled).map(|r| r.path.clone()).collect()
    };
    SessionResources {
        skill_paths: enabled(&resolved.skills),
        prompt_paths: enabled(&resolved.prompts),
        metadata,
        system_prompt: prompt_files.system_prompt,
        append_system_prompt: prompt_files.append_system_prompt,
    }
}

/// The resources a session in `env`'s directory loads with no flags, its project trust decided as the worker
/// decides it (`workerProjectTrusted`).
pub fn resolve_default(env: &Env) -> SessionResources {
    let agent_dir = env.agent_dir();
    let trusted = config::trust::worker_project_trusted(&env.cwd, &agent_dir, &env.home);
    let settings = config::settings::SettingsManager::create(&env.cwd, &agent_dir, trusted);
    resolve(env, settings)
}

/// The skills of a session (`updateSkillsFromPaths`): without `--no-skills`, the resolved ones and the bundled ones
/// in `bundled` (a directory), the first skill of a name kept.
pub fn skills(
    cwd: &str,
    agent_dir: &str,
    resources: &SessionResources,
    no_skills: bool,
    bundled: Option<&str>,
) -> Vec<Skill> {
    if no_skills {
        return Vec::new();
    }
    let paths = merge(
        cwd,
        resources.skill_paths.iter().cloned().chain(bundled.map(str::to_string)),
    );
    if paths.is_empty() {
        return Vec::new();
    }
    load_skills(&LoadSkillsOptions {
        cwd,
        agent_dir,
        skill_paths: &paths,
        include_defaults: false,
    })
    .skills
}

/// The prompt templates of a session (`updatePromptsFromPaths`): the resolved ones, the first of a name kept.
pub fn prompts(cwd: &str, agent_dir: &str, resources: &SessionResources) -> Vec<PromptTemplate> {
    let paths = merge(cwd, resources.prompt_paths.iter().cloned());
    let loaded = resources::prompt_templates::load_prompt_templates(cwd, agent_dir, &paths, false);
    let mut out: Vec<PromptTemplate> = Vec::new();
    for template in loaded.templates {
        if !out.iter().any(|t| t.name == template.name) {
            out.push(template);
        }
    }
    out
}

/// `findSourceInfoForPath` over the resolved resources: the metadata of the first resource that is the file or a
/// directory holding it; `None` for anything else. (TypeScript looks for the exact path first; here every resolved
/// skill and prompt is a file of its own, listed before any directory that could hold it, so one pass is the same.)
pub fn source_info(resources: &SessionResources, file_path: &str) -> Option<serde_json::Value> {
    let file = resources::paths::resolve("/", file_path);
    resources
        .metadata
        .iter()
        .find(|m| {
            let root = resources::paths::resolve("/", &m.path);
            file == root || file.starts_with(&format!("{root}/"))
        })
        .map(|m| m.source_info(file_path))
}

/// A skill's `SourceInfo`: the resolved resource's, else the loader's own (`createSkillSourceInfo` for a path
/// outside the profile's and project's skill directories, whose skills always have metadata: `local`, `temporary`,
/// the skill's directory). In practice the bundled skills.
pub fn skill_source_info(resources: &SessionResources, skill: &Skill) -> serde_json::Value {
    source_info(resources, &skill.file_path).unwrap_or_else(|| {
        serde_json::json!({
            "path": skill.file_path, "source": "local", "scope": "temporary", "origin": "top-level",
            "baseDir": skill.base_dir,
        })
    })
}

/// A prompt template's `SourceInfo`: the resolved resource's, else the loader's own (`local`; scope `user` or
/// `project` with that prompts directory, else `temporary` with the file's directory).
pub fn prompt_source_info(
    resources: &SessionResources,
    template: &PromptTemplate,
    cwd: &str,
    agent_dir: &str,
) -> serde_json::Value {
    source_info(resources, &template.file_path).unwrap_or_else(|| {
        let (scope, base) = match template.scope {
            "user" => ("user", resources::paths::join(agent_dir, "prompts")),
            "project" => (
                "project",
                resources::paths::join(
                    &resources::paths::join(cwd, resources::skills::CONFIG_DIR_NAME),
                    "prompts",
                ),
            ),
            _ => ("temporary", resources::paths::dirname(&template.file_path)),
        };
        serde_json::json!({
            "path": template.file_path, "source": "local", "scope": scope, "origin": "top-level", "baseDir": base,
        })
    })
}

/// `defaultBundledSkillPaths`: the bundled skills directory `dir`, unless `ULTRON_BUNDLED_SKILLS` is `off`, `0` or
/// `false`.
pub fn bundled_skills(dir: &Path, var: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let raw = var("ULTRON_BUNDLED_SKILLS").map(|v| v.trim().to_lowercase());
    if matches!(raw.as_deref(), Some("off" | "0" | "false")) {
        return None;
    }
    Some(config::paths::normalize(dir).display().to_string())
}
