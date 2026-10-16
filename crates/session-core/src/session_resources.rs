//! The resources a session loads beyond the defaults, as resource-loader.ts `reload` assembles them:
//!
//! - the `-e` sources' skills and prompts (`resolveExtensionSources(additionalExtensionPaths, { temporary: true })`,
//!   resolved by the CLI, which installed them: [`crate::session_flags::SessionFlags`]), which come first and also
//!   load with `--no-skills`;
//! - the settings `packages` (`packageManager.resolve()`, origin `package`, precedence after every local and
//!   auto-discovered resource): their enabled skills and prompts, and the extension manifests in their directories.
//!
//! The defaults (`<agentDir>/skills`, `.pi/skills`, the prompt directories, the bundled skills) are loaded where they
//! were before; these lists join them in TypeScript's order (`mergePaths`, the first skill of a name kept).

use crate::RunEnv as Env;
use crate::packages::manager::{Origin, PackageManager, PackageRoot, ResolvedResource};
use crate::session_flags::SessionFlags;
use resources::prompt_templates::PromptTemplate;
use resources::skills::{LoadSkillsOptions, Skill, load_skills};
use std::path::{Path, PathBuf};

/// One resource file and where it came from (`SourceInfo`): its source (`cli`, or the package source), scope and
/// origin, and the package directory.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResourceSource {
    pub path: String,
    pub source: String,
    pub scope: String,
    pub origin: String,
    pub base_dir: Option<String>,
}

impl ResourceSource {
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
    /// The settings packages' enabled skills and prompts.
    pub package_skills: Vec<ResourceSource>,
    pub package_prompts: Vec<ResourceSource>,
    /// The settings packages' extension manifest paths (enabled by the package's `extensions` filter).
    pub package_manifests: Vec<PathBuf>,
    /// What went wrong resolving the packages (an install that failed).
    pub diagnostics: Vec<String>,
}

/// `mergePaths`: each path once, first place kept.
fn merge(paths: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for path in paths {
        if !out.contains(&path) {
            out.push(path);
        }
    }
    out
}

fn sources(list: &[ResolvedResource]) -> Vec<ResourceSource> {
    list.iter()
        .filter(|r| r.enabled && r.metadata.origin == Origin::Package)
        .map(|r| ResourceSource {
            path: r.path.clone(),
            source: r.metadata.source.clone(),
            scope: r.metadata.scope.name().into(),
            origin: "package".into(),
            base_dir: r.metadata.base_dir.clone(),
        })
        .collect()
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

/// The session's extra resources for `flags` and the settings `manager` holds (with the session's project trust).
/// A settings package that is missing is installed (unless offline), as TypeScript's `resolve()` does; npm and git
/// write to stderr.
pub fn resolve(env: &Env, settings: config::settings::SettingsManager, flags: &SessionFlags) -> SessionResources {
    let mut out = SessionResources {
        cli_skills: merge(flags.skill_paths.iter().cloned()),
        cli_prompts: merge(flags.prompt_paths.iter().cloned()),
        ..SessionResources::default()
    };
    let has_packages = |s: &serde_json::Map<String, serde_json::Value>| {
        s.get("packages")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|p| !p.is_empty())
    };
    if !has_packages(settings.global_settings())
        && !(settings.is_project_trusted() && has_packages(settings.project_settings()))
    {
        return out;
    }
    let mut manager = PackageManager::new(env, settings);
    manager.child_stdout_to_stderr = true;
    let (resolved, roots) = match manager.resolve_with_roots() {
        Ok(found) => found,
        Err(message) => {
            out.diagnostics.push(message);
            return out;
        }
    };
    out.package_skills = sources(&resolved.skills);
    out.package_prompts = sources(&resolved.prompts);
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

/// The skills of a session: the `-e` ones first, then (without `--no-skills`) the defaults and the packages', the
/// first skill of a name kept (the bundled ones are added after, by the caller).
pub fn skills(cwd: &str, agent_dir: &str, resources: &SessionResources, no_skills: bool) -> Vec<Skill> {
    let mut skills = load_skills(&LoadSkillsOptions {
        cwd,
        agent_dir,
        skill_paths: &resources.cli_skills,
        include_defaults: false,
    })
    .skills;
    if no_skills {
        return skills;
    }
    let packages: Vec<String> = resources.package_skills.iter().map(|s| s.path.clone()).collect();
    let rest = load_skills(&LoadSkillsOptions {
        cwd,
        agent_dir,
        skill_paths: &packages,
        include_defaults: true,
    })
    .skills;
    let canonical = |p: &str| resources::paths::canonicalize(p);
    for skill in rest {
        let taken = skills
            .iter()
            .any(|s| s.name == skill.name || canonical(&s.file_path) == canonical(&skill.file_path));
        if !taken {
            skills.push(skill);
        }
    }
    skills
}

/// The prompt templates of a session: the `-e` ones, the defaults, then the packages' (the first of a name kept).
pub fn prompts(cwd: &str, agent_dir: &str, resources: &SessionResources) -> Vec<PromptTemplate> {
    let cli = resources::prompt_templates::load_prompt_templates(cwd, agent_dir, &resources.cli_prompts, false);
    let packages: Vec<String> = resources.package_prompts.iter().map(|s| s.path.clone()).collect();
    let rest = resources::prompt_templates::load_prompt_templates(cwd, agent_dir, &packages, true);
    let mut out: Vec<PromptTemplate> = Vec::new();
    for template in cli.templates.into_iter().chain(rest.templates) {
        if !out.iter().any(|t| t.name == template.name) {
            out.push(template);
        }
    }
    out
}

/// Where a skill or prompt file of these resources came from (`findSourceInfoForPath`): a `-e` skill's
/// `cli`/`temporary` metadata, or its package's; `None` for anything else.
pub fn source_info(resources: &SessionResources, file_path: &str) -> Option<serde_json::Value> {
    let under = |root: &str| {
        let root = resources::paths::resolve("/", root);
        let file = resources::paths::resolve("/", file_path);
        file == root || file.starts_with(&format!("{root}/"))
    };
    if resources.cli_skills.iter().any(|p| under(p)) {
        return Some(serde_json::json!({
            "path": file_path, "source": "cli", "scope": "temporary", "origin": "top-level"
        }));
    }
    resources
        .package_skills
        .iter()
        .chain(&resources.package_prompts)
        .find(|s| under(&s.path))
        .map(|s| s.source_info(file_path))
}
