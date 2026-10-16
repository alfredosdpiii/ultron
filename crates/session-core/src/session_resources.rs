//! The resources a session loads, as resource-loader.ts `reload` assembles them for the session worker:
//!
//! - the `-e` sources' skills and prompts (`resolveExtensionSources(additionalExtensionPaths, { temporary: true })`,
//!   resolved by the CLI, which installed them: [`crate::session_flags::SessionFlags`]), which come first and also
//!   load with `--no-skills`;
//! - everything `packageManager.resolve()` finds, in its precedence order: the settings `packages` (installed when
//!   missing), the settings' local `skills`/`prompts` entries (paths, globs and `!`/`+`/`-` patterns, project scope
//!   first), and the auto-discovered ones (`.pi/skills`, `.pi/prompts`, `.agents/skills` from the cwd up to the git
//!   root, `<agentDir>/skills`, `<agentDir>/prompts`, `~/.agents/skills`). A project that is not trusted contributes
//!   no settings, no project resources and no ancestor `.agents/skills`;
//! - then the bundled skills (`bundledSkillPaths`), the first skill of a name kept;
//! - the base system prompt and its addendum: `.pi/SYSTEM.md` / `.pi/APPEND_SYSTEM.md` in a trusted project, else
//!   `<agentDir>/SYSTEM.md` / `<agentDir>/APPEND_SYSTEM.md` (`discoverSystemPromptFile`).
//!
//! Every skill and prompt carries its `SourceInfo` as `findSourceInfoForPath` gives it (the resolved resource's
//! metadata, exact path first, then the first resource directory holding it), else the loader's own.

use crate::RunEnv as Env;
use crate::packages::manager::{Origin, PackageManager, PackageRoot, PathMetadata, ResolvedPaths};
use crate::session_flags::SessionFlags;
use resources::prompt_templates::PromptTemplate;
use resources::skills::{LoadSkillsOptions, Skill, load_skills};
use std::path::{Path, PathBuf};

/// Where one resource came from (`PathMetadata` with its path): its source (`cli`, `local`, `auto` or the package
/// source), scope and origin, and its base directory.
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
            origin: match metadata.origin {
                Origin::Package => "package",
                Origin::TopLevel => "top-level",
            }
            .into(),
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
    /// The `-e` sources' skill files and directories (metadata `cli`/`temporary`).
    pub cli_skills: Vec<String>,
    pub cli_prompts: Vec<String>,
    /// `resolve()`'s enabled skills in precedence order. They are files already (`mapSkillPath` has nothing to map:
    /// the package manager lists a skill directory's `SKILL.md`).
    pub skill_paths: Vec<String>,
    /// `resolve()`'s enabled prompt templates in precedence order.
    pub prompt_paths: Vec<String>,
    /// `metadataByPath` in insertion order: every resolved resource (enabled or not), the `-e` skills and prompts;
    /// the first entry of a path kept.
    pub metadata: Vec<ResourceSource>,
    /// The discovered SYSTEM.md's text (`getSystemPrompt`), if any.
    pub system_prompt: Option<String>,
    /// The discovered APPEND_SYSTEM.md's text (`getAppendSystemPrompt`).
    pub append_system_prompt: Vec<String>,
    /// The settings packages' extension manifest paths (enabled by the package's `extensions` filter).
    pub package_manifests: Vec<PathBuf>,
    /// What went wrong resolving the packages (an install that failed).
    pub diagnostics: Vec<String>,
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

/// The extension manifests a package directory holds, as a `-e` path's are found, then its settings filter's
/// `extensions` patterns (relative to the package): `[]` turns them all off, `autoload: false` without patterns too.
pub fn package_manifests(root: &PackageRoot, cwd: &Path, home: &Path) -> (Vec<PathBuf>, Vec<String>) {
    let discovery = extensions::manifest::discover_cli_paths(std::slice::from_ref(&root.root), cwd, home);
    let diagnostics = discovery
        .diagnostics
        .iter()
        .map(|d| format!("{}: {}", d.path.display(), d.message))
        .collect();
    let found: Vec<PathBuf> = discovery.manifests.into_iter().map(|m| m.path).collect();
    let Some(filter) = &root.filter else {
        return (found, diagnostics);
    };
    let patterns: Option<Vec<&str>> = filter
        .get("extensions")
        .and_then(serde_json::Value::as_array)
        .map(|items| items.iter().filter_map(serde_json::Value::as_str).collect());
    let autoload_off = filter.get("autoload") == Some(&serde_json::Value::Bool(false));
    let enabled = match patterns {
        Some(patterns) if patterns.is_empty() => Vec::new(),
        Some(patterns) => extensions::manifest::apply_patterns(&found, &patterns, Path::new(&root.root)),
        None if autoload_off => Vec::new(),
        None => found,
    };
    (enabled, diagnostics)
}

/// The session's resources for `flags` and the settings `manager` holds (with the session's project trust). A
/// settings package that is missing is installed (unless offline), as TypeScript's `resolve()` does; npm and git
/// write to stderr. A package that cannot be resolved is reported, and the local and auto-discovered resources
/// still load.
pub fn resolve(env: &Env, settings: config::settings::SettingsManager, flags: &SessionFlags) -> SessionResources {
    let cwd = resources::paths::resolve_path(&env.cwd.display().to_string(), "/", false);
    let agent_dir = env.agent_dir().display().to_string();
    let trusted = settings.is_project_trusted();
    let prompt_files = resources::prompt_files::load_prompt_files(&cwd, &agent_dir, trusted, None, None);
    let mut out = SessionResources {
        cli_skills: merge(&cwd, flags.skill_paths.iter().cloned()),
        cli_prompts: merge(&cwd, flags.prompt_paths.iter().cloned()),
        system_prompt: prompt_files.system_prompt,
        append_system_prompt: prompt_files.append_system_prompt,
        ..SessionResources::default()
    };
    let mut manager = PackageManager::new(env, settings);
    manager.child_stdout_to_stderr = true;
    let (resolved, roots): (ResolvedPaths, Vec<PackageRoot>) = match manager.resolve_with_roots() {
        Ok(found) => found,
        Err(message) => {
            out.diagnostics.push(message);
            (manager.resolve_local(), Vec::new())
        }
    };
    let mut metadata: Vec<ResourceSource> = Vec::new();
    for list in [
        &resolved.extensions,
        &resolved.skills,
        &resolved.prompts,
        &resolved.themes,
    ] {
        for r in list {
            if !metadata.iter().any(|m| m.path == r.path) {
                metadata.push(ResourceSource::of(&r.path, &r.metadata));
            }
        }
    }
    out.skill_paths = resolved
        .skills
        .iter()
        .filter(|r| r.enabled)
        .map(|r| r.path.clone())
        .collect();
    out.prompt_paths = resolved
        .prompts
        .iter()
        .filter(|r| r.enabled)
        .map(|r| r.path.clone())
        .collect();
    for path in &flags.skill_paths {
        if !metadata.iter().any(|m| &m.path == path) {
            metadata.push(ResourceSource {
                path: path.clone(),
                source: "cli".into(),
                scope: "temporary".into(),
                origin: "top-level".into(),
                base_dir: None,
            });
        }
    }
    // `getEnabledPaths(cliExtensionPaths.prompts)`: an `-e` prompt has its package's metadata (the source as the
    // worker got it, `temporary`, `package`, the package directory).
    for path in &flags.prompt_paths {
        let root = flags.extension_paths.iter().find(|root| {
            let root = resources::paths::resolve("/", root);
            path == &root || path.starts_with(&format!("{root}/"))
        });
        if let Some(root) = root
            && !metadata.iter().any(|m| &m.path == path)
        {
            metadata.push(ResourceSource {
                path: path.clone(),
                source: root.clone(),
                scope: "temporary".into(),
                origin: "package".into(),
                base_dir: Some(root.clone()),
            });
        }
    }
    out.metadata = metadata;
    for root in &roots {
        let (manifests, diagnostics) = package_manifests(root, &env.cwd, &env.home);
        for manifest in manifests {
            if !out.package_manifests.contains(&manifest) {
                out.package_manifests.push(manifest);
            }
        }
        out.diagnostics.extend(diagnostics);
    }
    out
}

/// The resources a session in `env`'s directory loads with no flags, its project trust decided as the worker
/// decides it (`workerProjectTrusted`).
pub fn resolve_default(env: &Env) -> SessionResources {
    let agent_dir = env.agent_dir();
    let trusted = crate::extension_commands::worker_project_trusted(&env.cwd, &agent_dir, &env.home);
    let settings = config::settings::SettingsManager::create(&env.cwd, &agent_dir, trusted);
    resolve(env, settings, &SessionFlags::default())
}

/// The skills of a session (`updateSkillsFromPaths`): the `-e` ones first, then (without `--no-skills`) the
/// resolved ones and the bundled ones in `bundled` (a directory), the first skill of a name kept.
pub fn skills(
    cwd: &str,
    agent_dir: &str,
    resources: &SessionResources,
    no_skills: bool,
    bundled: Option<&str>,
) -> Vec<Skill> {
    let paths: Vec<String> = if no_skills {
        merge(cwd, resources.cli_skills.iter().cloned())
    } else {
        merge(
            cwd,
            resources
                .cli_skills
                .iter()
                .chain(&resources.skill_paths)
                .cloned()
                .chain(bundled.map(str::to_string)),
        )
    };
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

/// The prompt templates of a session (`updatePromptsFromPaths`): the `-e` ones, then the resolved ones, the first
/// of a name kept.
pub fn prompts(cwd: &str, agent_dir: &str, resources: &SessionResources) -> Vec<PromptTemplate> {
    let paths = merge(
        cwd,
        resources.cli_prompts.iter().chain(&resources.prompt_paths).cloned(),
    );
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
