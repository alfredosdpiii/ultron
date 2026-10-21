//! The Pi extensions TypeScript would load from a working directory and profile (`core/package-manager.ts`
//! `resolve()`, its top-level `extensions`): the settings' `extensions` entries (paths and `!`/`+`/`-` patterns,
//! project scope first), then `<cwd>/.pi/extensions/` (a trusted project only) and `<agentDir>/extensions/`. Pi
//! extensions are not supported by ultron-rs (removed 2026-10-19): none is loaded, and a session's startup names
//! the ones it ignores.

use super::{LocalResources, PathMetadata, ResolvedResource, Scope, files, rank, string_list};
use ::resources::paths::{canonicalize, join};
use indexmap::IndexMap;

impl LocalResources {
    /// The top-level extensions by precedence, each with whether its filters enable it (`resolve().extensions`
    /// without the package ones: Pi packages are not supported either).
    pub fn extensions(&self) -> Vec<ResolvedResource> {
        let mut acc: IndexMap<String, (PathMetadata, bool)> = IndexMap::new();
        let mut add = |path: &str, metadata: &PathMetadata, enabled: bool| {
            if !path.is_empty() {
                acc.entry(path.to_string())
                    .or_insert_with(|| (metadata.clone(), enabled));
            }
        };
        let global_base = self.agent_dir.clone();
        let project_base = join(&self.cwd, config::paths::CONFIG_DIR_NAME);
        let entries = |scope: Scope| {
            let s = match scope {
                Scope::User => self.settings.global_settings(),
                Scope::Project => self.settings.project_settings(),
            };
            string_list(s.get("extensions")).unwrap_or_default()
        };
        for (scope, base) in [(Scope::Project, &project_base), (Scope::User, &global_base)] {
            let list = entries(scope);
            let (plain, patterns): (Vec<String>, Vec<String>) = list.into_iter().partition(|e| !files::is_pattern(e));
            let resolved: Vec<String> = plain.iter().map(|p| self.resolve_from(p, base)).collect();
            let found = files::extension_files_from_paths(&resolved);
            let enabled = files::apply_patterns(&found, &patterns, base);
            let m = PathMetadata {
                source: "local".into(),
                scope,
                base_dir: None,
            };
            for f in &found {
                add(f, &m, enabled.contains(f));
            }
        }
        let mut auto = |scope: Scope, base: &str| {
            let m = PathMetadata {
                source: "auto".into(),
                scope,
                base_dir: Some(base.to_string()),
            };
            let overrides = entries(scope);
            for p in files::auto_extensions(&join(base, "extensions")) {
                let enabled = files::enabled_by_overrides(&p, &overrides, base);
                add(&p, &m, enabled);
            }
        };
        if self.settings.is_project_trusted() {
            auto(Scope::Project, &project_base);
        }
        auto(Scope::User, &global_base);
        let mut list: Vec<ResolvedResource> = acc
            .into_iter()
            .map(|(path, (metadata, enabled))| ResolvedResource {
                path,
                enabled,
                metadata,
            })
            .collect();
        list.sort_by_key(|r| rank(&r.metadata));
        let mut seen = std::collections::HashSet::new();
        list.retain(|r| seen.insert(canonicalize(&r.path)));
        list
    }
}

/// The startup note naming the extensions TypeScript would load here (the enabled ones of
/// [`LocalResources::extensions`]) and ultron-rs ignores; none when there are none.
pub fn ignored_extensions_note(resources: &LocalResources) -> Option<String> {
    let ignored: Vec<String> = resources
        .extensions()
        .into_iter()
        .filter(|r| r.enabled)
        .map(|r| r.path)
        .collect();
    match ignored.len() {
        0 => None,
        n => Some(format!(
            "Pi extensions are not supported by ultron-rs; ignoring {n} extension{}: {}",
            if n == 1 { "" } else { "s" },
            ignored.join(", ")
        )),
    }
}
