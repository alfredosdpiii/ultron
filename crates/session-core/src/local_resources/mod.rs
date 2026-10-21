//! The local resources a session and `ultron config` resolve (the local half of `core/package-manager.ts`
//! `resolve()`): the settings' `skills`/`prompts`/`themes` entries (paths, globs and `!`/`+`/`-` patterns, project
//! scope first) and the auto-discovered ones (`.pi/skills`, `.pi/prompts`, `.pi/themes`, `.agents/skills` from the
//! cwd up to the git root, `<agentDir>/skills`, `<agentDir>/prompts`, `<agentDir>/themes`, `~/.agents/skills`). A
//! project that is not trusted contributes no settings entries, no project resources and no ancestor
//! `.agents/skills`.
//!
//! Pi packages (the settings `packages`) and Pi extensions are not supported by ultron-rs: nothing is installed and
//! no package resources are resolved; the extensions TypeScript would load are only named (see [`extensions`]).

pub mod extensions;
pub mod files;
pub mod glob;

pub use files::{RESOURCE_TYPES, ResourceType};

use ::resources::paths as resources_paths;
use config::settings::SettingsManager;
use indexmap::IndexMap;
use resources_paths::{canonicalize, dirname, join};
use serde_json::Value;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scope {
    User,
    Project,
}

impl Scope {
    pub fn name(self) -> &'static str {
        match self {
            Scope::User => "user",
            Scope::Project => "project",
        }
    }
}

/// `PathMetadata` of a top-level resource: `source` is `local` (a settings entry) or `auto` (auto-discovered).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathMetadata {
    pub source: String,
    pub scope: Scope,
    pub base_dir: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedResource {
    pub path: String,
    pub enabled: bool,
    pub metadata: PathMetadata,
}

/// `ResolvedPaths`, indexed by [`ResourceType`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResolvedPaths {
    pub skills: Vec<ResolvedResource>,
    pub prompts: Vec<ResolvedResource>,
    pub themes: Vec<ResolvedResource>,
}

impl ResolvedPaths {
    pub fn of(&self, kind: ResourceType) -> &[ResolvedResource] {
        match kind {
            ResourceType::Skills => &self.skills,
            ResourceType::Prompts => &self.prompts,
            ResourceType::Themes => &self.themes,
        }
    }
}

type Accumulator = [IndexMap<String, (PathMetadata, bool)>; 3];

fn slot(kind: ResourceType) -> usize {
    match kind {
        ResourceType::Skills => 0,
        ResourceType::Prompts => 1,
        ResourceType::Themes => 2,
    }
}

/// `addResource`: the first entry for a path wins.
fn add(acc: &mut Accumulator, kind: ResourceType, path: &str, metadata: &PathMetadata, enabled: bool) {
    if path.is_empty() {
        return;
    }
    acc[slot(kind)]
        .entry(path.to_string())
        .or_insert_with(|| (metadata.clone(), enabled));
}

/// `resourcePrecedenceRank` of a top-level resource: project before user, a settings entry before an
/// auto-discovered one.
fn rank(m: &PathMetadata) -> u8 {
    let base = if m.scope == Scope::Project { 0 } else { 2 };
    base + u8::from(m.source != "local")
}

fn string_list(value: Option<&Value>) -> Option<Vec<String>> {
    value
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
}

/// The settings `packages` entries of both scopes (a manager of an untrusted project has no project settings): Pi
/// packages are not supported, so a session reports how many it ignores.
pub fn ignored_packages(settings: &SettingsManager) -> usize {
    let count = |s: &serde_json::Map<String, Value>| s.get("packages").and_then(Value::as_array).map_or(0, Vec::len);
    count(settings.global_settings()) + count(settings.project_settings())
}

/// The startup note for settings `packages` entries ultron-rs ignores; none when there are none.
pub fn ignored_packages_note(settings: &SettingsManager) -> Option<String> {
    match ignored_packages(settings) {
        0 => None,
        n => Some(format!(
            "Pi packages are not supported by ultron-rs; ignoring {n} package{} in settings",
            if n == 1 { "" } else { "s" }
        )),
    }
}

/// Resolves the local resources of one working directory and profile.
pub struct LocalResources {
    pub cwd: String,
    pub agent_dir: String,
    home: String,
    pub settings: SettingsManager,
}

impl LocalResources {
    pub fn new(env: &crate::RunEnv, settings: SettingsManager) -> Self {
        let resolve = |p: &str| config::paths::resolve(p, &env.cwd, &env.home).display().to_string();
        Self {
            cwd: resolve(&env.cwd.display().to_string()),
            agent_dir: resolve(&env.agent_dir().display().to_string()),
            home: env.home.display().to_string(),
            settings,
        }
    }

    fn resolve_from(&self, input: &str, base: &str) -> String {
        config::paths::resolve(input.trim(), Path::new(base), Path::new(&self.home))
            .display()
            .to_string()
    }

    /// Every local resource, by precedence: the settings' entries (`resolveLocalEntries`, project scope first),
    /// then `addAutoDiscoveredResources`.
    pub fn resolve(&self) -> ResolvedPaths {
        let mut acc: Accumulator = Default::default();
        let global_base = self.agent_dir.clone();
        let project_base = join(&self.cwd, config::paths::CONFIG_DIR_NAME);
        for kind in RESOURCE_TYPES {
            for (scope, base) in [(Scope::Project, &project_base), (Scope::User, &global_base)] {
                let list = self.entries(scope, kind);
                if list.is_empty() {
                    continue;
                }
                let (plain, patterns): (Vec<String>, Vec<String>) =
                    list.into_iter().partition(|e| !files::is_pattern(e));
                let resolved: Vec<String> = plain.iter().map(|p| self.resolve_from(p, base)).collect();
                let found = files::files_from_paths(&resolved, kind);
                let enabled = files::apply_patterns(&found, &patterns, base);
                let m = PathMetadata {
                    source: "local".into(),
                    scope,
                    base_dir: None,
                };
                for f in &found {
                    add(&mut acc, kind, f, &m, enabled.contains(f));
                }
            }
        }
        self.add_auto_discovered(&mut acc, &global_base, &project_base);
        to_resolved(acc)
    }

    fn entries(&self, scope: Scope, kind: ResourceType) -> Vec<String> {
        let s = match scope {
            Scope::User => self.settings.global_settings(),
            Scope::Project => self.settings.project_settings(),
        };
        string_list(s.get(kind.key())).unwrap_or_default()
    }

    /// `addAutoDiscoveredResources`.
    fn add_auto_discovered(&self, acc: &mut Accumulator, global_base: &str, project_base: &str) {
        let meta = |scope: Scope, base: &str| PathMetadata {
            source: "auto".into(),
            scope,
            base_dir: Some(base.to_string()),
        };
        let mut put = |kind: ResourceType, paths: Vec<String>, m: &PathMetadata, scope: Scope, base: &str| {
            let overrides = self.entries(scope, kind);
            for p in paths {
                let enabled = files::enabled_by_overrides(&p, &overrides, base);
                add(acc, kind, &p, m, enabled);
            }
        };
        let user_agents = join(&join(&self.home, ".agents"), "skills");
        let trusted = self.settings.is_project_trusted();
        let project_agents: Vec<String> = if trusted {
            let user_resolved = resources_paths::resolve("/", &user_agents);
            files::ancestor_agents_skill_dirs(&self.cwd)
                .into_iter()
                .filter(|d| resources_paths::resolve("/", d) != user_resolved)
                .collect()
        } else {
            Vec::new()
        };
        let pm = meta(Scope::Project, project_base);
        let dir = |base: &str, kind: ResourceType| join(base, kind.key());
        if trusted {
            put(
                ResourceType::Skills,
                files::auto_skills(&dir(project_base, ResourceType::Skills), false),
                &pm,
                Scope::Project,
                project_base,
            );
        }
        for agents_dir in &project_agents {
            let base = dirname(agents_dir);
            put(
                ResourceType::Skills,
                files::auto_skills(agents_dir, true),
                &meta(Scope::Project, &base),
                Scope::Project,
                &base,
            );
        }
        if trusted {
            put(
                ResourceType::Prompts,
                files::auto_prompts(&dir(project_base, ResourceType::Prompts)),
                &pm,
                Scope::Project,
                project_base,
            );
            put(
                ResourceType::Themes,
                files::auto_themes(&dir(project_base, ResourceType::Themes)),
                &pm,
                Scope::Project,
                project_base,
            );
        }
        let um = meta(Scope::User, global_base);
        put(
            ResourceType::Skills,
            files::auto_skills(&dir(global_base, ResourceType::Skills), false),
            &um,
            Scope::User,
            global_base,
        );
        let user_agents_base = dirname(&user_agents);
        put(
            ResourceType::Skills,
            files::auto_skills(&user_agents, true),
            &meta(Scope::User, &user_agents_base),
            Scope::User,
            &user_agents_base,
        );
        put(
            ResourceType::Prompts,
            files::auto_prompts(&dir(global_base, ResourceType::Prompts)),
            &um,
            Scope::User,
            global_base,
        );
        put(
            ResourceType::Themes,
            files::auto_themes(&dir(global_base, ResourceType::Themes)),
            &um,
            Scope::User,
            global_base,
        );
    }
}

/// `toResolvedPaths`: by precedence (stable), the first of each canonical path kept.
fn to_resolved(acc: Accumulator) -> ResolvedPaths {
    let convert = |map: &IndexMap<String, (PathMetadata, bool)>| -> Vec<ResolvedResource> {
        let mut list: Vec<ResolvedResource> = map
            .iter()
            .map(|(path, (metadata, enabled))| ResolvedResource {
                path: path.clone(),
                enabled: *enabled,
                metadata: metadata.clone(),
            })
            .collect();
        list.sort_by_key(|r| rank(&r.metadata));
        let mut seen = std::collections::HashSet::new();
        list.retain(|r| seen.insert(canonicalize(&r.path)));
        list
    };
    ResolvedPaths {
        skills: convert(&acc[0]),
        prompts: convert(&acc[1]),
        themes: convert(&acc[2]),
    }
}
