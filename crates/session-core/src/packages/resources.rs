//! Resource discovery of `core/package-manager.ts`: which extension, skill, prompt and theme files a package, a
//! settings entry or an auto-discovered directory provides, and which of them are enabled by `pi` manifests and
//! filter patterns (`!x` excludes, `+x` force-includes, `-x` force-excludes).
//!
//! Directory entries are visited in name order (TypeScript takes the operating system's `readdir` order).

use super::glob::minimatch;
use resources::ignore::{Ignore, add_ignore_rules};
use resources::paths::{basename, dirname, join, relative, resolve};
use serde_json::Value;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ResourceType {
    Extensions,
    Skills,
    Prompts,
    Themes,
}

pub const RESOURCE_TYPES: [ResourceType; 4] = [
    ResourceType::Extensions,
    ResourceType::Skills,
    ResourceType::Prompts,
    ResourceType::Themes,
];

impl ResourceType {
    pub fn key(self) -> &'static str {
        match self {
            ResourceType::Extensions => "extensions",
            ResourceType::Skills => "skills",
            ResourceType::Prompts => "prompts",
            ResourceType::Themes => "themes",
        }
    }

    /// `FILE_PATTERNS`.
    fn matches_file(self, name: &str) -> bool {
        match self {
            ResourceType::Extensions => name.ends_with(".ts") || name.ends_with(".js"),
            ResourceType::Skills | ResourceType::Prompts => name.ends_with(".md"),
            ResourceType::Themes => name.ends_with(".json"),
        }
    }
}

/// One directory entry: name, and whether it is a directory / a file (a symlink's target decides).
struct Entry {
    name: String,
    is_dir: bool,
    is_file: bool,
}

/// `readdirSync(dir, { withFileTypes: true })` with symlinks resolved by `statSync`; a dangling link is skipped.
fn entries(dir: &str) -> Vec<Entry> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<Entry> = read
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let file_type = e.file_type().ok()?;
            let (is_dir, is_file) = if file_type.is_symlink() {
                let meta = std::fs::metadata(e.path()).ok()?;
                (meta.is_dir(), meta.is_file())
            } else {
                (file_type.is_dir(), file_type.is_file())
            };
            Some(Entry { name, is_dir, is_file })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn exists(path: &str) -> bool {
    Path::new(path).exists()
}

/// `collectFiles`.
fn collect_files(dir: &str, kind: ResourceType, ig: &mut Ignore, root: &str, out: &mut Vec<String>) {
    if !exists(dir) {
        return;
    }
    add_ignore_rules(ig, dir, root);
    for entry in entries(dir) {
        if entry.name.starts_with('.') || entry.name == "node_modules" {
            continue;
        }
        let full = join(dir, &entry.name);
        let rel = relative(root, &full);
        let ignore_path = if entry.is_dir { format!("{rel}/") } else { rel };
        if ig.ignores(&ignore_path) {
            continue;
        }
        if entry.is_dir {
            collect_files(&full, kind, ig, root, out);
        } else if entry.is_file && kind.matches_file(&entry.name) {
            out.push(full);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SkillMode {
    Pi,
    Agents,
}

/// `collectSkillEntries`: a directory with `SKILL.md` is one skill; otherwise its subdirectories are searched, and
/// loose `.md` files count at the root (`pi` mode) or below it (`agents` mode).
fn collect_skill_entries(dir: &str, mode: SkillMode, ig: &mut Ignore, root: &str, out: &mut Vec<String>) {
    if !exists(dir) {
        return;
    }
    add_ignore_rules(ig, dir, root);
    let list = entries(dir);
    for entry in &list {
        if entry.name != "SKILL.md" {
            continue;
        }
        let full = join(dir, &entry.name);
        if entry.is_file && !ig.ignores(&relative(root, &full)) {
            out.push(full);
            return;
        }
    }
    for entry in &list {
        if entry.name.starts_with('.') || entry.name == "node_modules" {
            continue;
        }
        let full = join(dir, &entry.name);
        let rel = relative(root, &full);
        let at_root = dir == root;
        if entry.is_file
            && entry.name.ends_with(".md")
            && !ig.ignores(&rel)
            && ((mode == SkillMode::Pi && at_root) || (mode == SkillMode::Agents && !at_root))
        {
            out.push(full);
            continue;
        }
        if !entry.is_dir || ig.ignores(&format!("{rel}/")) {
            continue;
        }
        collect_skill_entries(&full, mode, ig, root, out);
    }
}

fn skill_entries(dir: &str, mode: SkillMode) -> Vec<String> {
    let mut out = Vec::new();
    collect_skill_entries(dir, mode, &mut Ignore::new(), dir, &mut out);
    out
}

/// `collectAutoPromptEntries` / `collectAutoThemeEntries`: top-level files of one extension.
fn top_level_files(dir: &str, extension: &str) -> Vec<String> {
    if !exists(dir) {
        return Vec::new();
    }
    let mut ig = Ignore::new();
    add_ignore_rules(&mut ig, dir, dir);
    entries(dir)
        .into_iter()
        .filter(|e| !e.name.starts_with('.') && e.name != "node_modules")
        .filter(|e| e.is_file && e.name.ends_with(extension) && !ig.ignores(&e.name))
        .map(|e| join(dir, &e.name))
        .collect()
}

/// `readPiManifest`: the `pi` object's string arrays of a package.json; None when there is no `pi` object.
pub fn read_pi_manifest(package_json: &str) -> Option<Vec<(ResourceType, Vec<String>)>> {
    let text = std::fs::read_to_string(package_json).ok()?;
    let value: Value = serde_json::from_str(resources::paths::strip_bom(&text)).ok()?;
    let pi = value.as_object()?.get("pi")?.as_object()?;
    Some(
        RESOURCE_TYPES
            .iter()
            .filter_map(|kind| {
                let list = pi.get(kind.key())?.as_array()?;
                let strings: Option<Vec<String>> = list.iter().map(|v| v.as_str().map(str::to_string)).collect();
                strings.map(|s| (*kind, s))
            })
            .collect(),
    )
}

pub fn manifest_entries(manifest: &[(ResourceType, Vec<String>)], kind: ResourceType) -> Option<&Vec<String>> {
    manifest.iter().find(|(k, _)| *k == kind).map(|(_, v)| v)
}

/// `resolveExtensionEntries`: a directory's manifest extensions that exist, else its `index.ts` / `index.js`.
fn resolve_extension_entries(dir: &str) -> Option<Vec<String>> {
    let package_json = join(dir, "package.json");
    if exists(&package_json)
        && let Some(manifest) = read_pi_manifest(&package_json)
        && let Some(list) = manifest_entries(&manifest, ResourceType::Extensions).filter(|l| !l.is_empty())
    {
        let found: Vec<String> = list.iter().map(|p| resolve(dir, p)).filter(|p| exists(p)).collect();
        if !found.is_empty() {
            return Some(found);
        }
    }
    ["index.ts", "index.js"]
        .iter()
        .map(|n| join(dir, n))
        .find(|p| exists(p))
        .map(|p| vec![p])
}

/// `collectAutoExtensionEntries`.
fn auto_extension_entries(dir: &str) -> Vec<String> {
    if !exists(dir) {
        return Vec::new();
    }
    if let Some(found) = resolve_extension_entries(dir) {
        return found;
    }
    let mut ig = Ignore::new();
    add_ignore_rules(&mut ig, dir, dir);
    let mut out = Vec::new();
    for entry in entries(dir) {
        if entry.name.starts_with('.') || entry.name == "node_modules" {
            continue;
        }
        let full = join(dir, &entry.name);
        let ignore_path = if entry.is_dir {
            format!("{}/", entry.name)
        } else {
            entry.name.clone()
        };
        if ig.ignores(&ignore_path) {
            continue;
        }
        if entry.is_file && (entry.name.ends_with(".ts") || entry.name.ends_with(".js")) {
            out.push(full);
        } else if entry.is_dir
            && let Some(found) = resolve_extension_entries(&full)
        {
            out.extend(found);
        }
    }
    out
}

/// `collectResourceFiles`.
fn resource_files(dir: &str, kind: ResourceType) -> Vec<String> {
    match kind {
        ResourceType::Skills => skill_entries(dir, SkillMode::Pi),
        ResourceType::Extensions => auto_extension_entries(dir),
        _ => {
            let mut out = Vec::new();
            collect_files(dir, kind, &mut Ignore::new(), dir, &mut out);
            out
        }
    }
}

/// `collectFilesFromPaths`.
pub fn files_from_paths(paths: &[String], kind: ResourceType) -> Vec<String> {
    let mut out = Vec::new();
    for p in paths {
        match std::fs::metadata(p) {
            Ok(meta) if meta.is_file() => out.push(p.clone()),
            Ok(meta) if meta.is_dir() => out.extend(resource_files(p, kind)),
            _ => {}
        }
    }
    out
}

/// The files of a convention directory (`<root>/<type>`), all enabled.
pub fn convention_files(package_root: &str, kind: ResourceType) -> Option<Vec<String>> {
    let dir = join(package_root, kind.key());
    exists(&dir).then(|| resource_files(&dir, kind))
}

// ---------------------------------------------------------------------------------------------------- patterns

fn is_override(s: &str) -> bool {
    s.starts_with('!') || s.starts_with('+') || s.starts_with('-')
}

pub fn is_pattern(s: &str) -> bool {
    is_override(s) || s.contains('*') || s.contains('?')
}

fn has_glob(s: &str) -> bool {
    s.contains('*') || s.contains('?')
}

/// `matchesAnyPattern`: the path relative to `base`, its name, or the whole path; a `SKILL.md` also by its folder.
fn matches_any(file: &str, patterns: &[&str], base: &str) -> bool {
    let rel = relative(base, file);
    let name = basename(file);
    let skill = name == "SKILL.md";
    let parent = dirname(file);
    patterns.iter().any(|pattern| {
        if minimatch(&rel, pattern) || minimatch(name, pattern) || minimatch(file, pattern) {
            return true;
        }
        skill
            && (minimatch(&relative(base, &parent), pattern)
                || minimatch(basename(&parent), pattern)
                || minimatch(&parent, pattern))
    })
}

/// `matchesAnyExactPattern`.
fn matches_any_exact(file: &str, patterns: &[&str], base: &str) -> bool {
    let rel = relative(base, file);
    let skill = basename(file) == "SKILL.md";
    let parent = dirname(file);
    patterns.iter().any(|pattern| {
        let normalized = pattern.strip_prefix("./").unwrap_or(pattern);
        if normalized == rel || normalized == file {
            return true;
        }
        skill && (normalized == relative(base, &parent) || normalized == parent)
    })
}

/// `isEnabledByOverrides`.
pub fn enabled_by_overrides(file: &str, patterns: &[String], base: &str) -> bool {
    let with = |c: char| -> Vec<&str> { patterns.iter().filter_map(|p| p.strip_prefix(c)).collect() };
    let (excludes, force_in, force_out) = (with('!'), with('+'), with('-'));
    let mut enabled = true;
    if !excludes.is_empty() && matches_any(file, &excludes, base) {
        enabled = false;
    }
    if !force_in.is_empty() && matches_any_exact(file, &force_in, base) {
        enabled = true;
    }
    if !force_out.is_empty() && matches_any_exact(file, &force_out, base) {
        enabled = false;
    }
    enabled
}

/// `applyPatterns`: the enabled subset of `all`, in `applyPatterns`' order (includes, then force-includes).
pub fn apply_patterns(all: &[String], patterns: &[String], base: &str) -> Vec<String> {
    let (mut includes, mut excludes, mut force_in, mut force_out) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for p in patterns {
        if let Some(rest) = p.strip_prefix('+') {
            force_in.push(rest);
        } else if let Some(rest) = p.strip_prefix('-') {
            force_out.push(rest);
        } else if let Some(rest) = p.strip_prefix('!') {
            excludes.push(rest);
        } else {
            includes.push(p.as_str());
        }
    }
    let mut result: Vec<&String> = if includes.is_empty() {
        all.iter().collect()
    } else {
        all.iter().filter(|f| matches_any(f, &includes, base)).collect()
    };
    if !excludes.is_empty() {
        result.retain(|f| !matches_any(f, &excludes, base));
    }
    if !force_in.is_empty() {
        for f in all {
            if !result.contains(&f) && matches_any_exact(f, &force_in, base) {
                result.push(f);
            }
        }
    }
    if !force_out.is_empty() {
        result.retain(|f| !matches_any_exact(f, &force_out, base));
    }
    result.into_iter().cloned().collect()
}

/// `applyAutoloadDisabledPatterns`: only the files a pattern names, in the order patterns name them (later ones
/// win).
pub fn apply_autoload_disabled_patterns(all: &[String], patterns: &[String], base: &str) -> Vec<(String, bool)> {
    let mut result: Vec<(String, bool)> = Vec::new();
    for pattern in patterns {
        let target = if is_override(pattern) {
            &pattern[1..]
        } else {
            pattern.as_str()
        };
        let enabled = !pattern.starts_with('-') && !pattern.starts_with('!');
        let exact = pattern.starts_with('+') || pattern.starts_with('-');
        for file in all {
            let hit = if exact {
                matches_any_exact(file, &[target], base)
            } else {
                matches_any(file, &[target], base)
            };
            if hit {
                match result.iter_mut().find(|(f, _)| f == file) {
                    Some(slot) => slot.1 = enabled,
                    None => result.push((file.clone(), enabled)),
                }
            }
        }
    }
    result
}

/// `expandPackageGlob`: matches under `root` (no segment starting with `.`), sorted.
fn expand_glob(pattern: &str, root: &str) -> Vec<String> {
    let pattern = pattern.strip_prefix("./").unwrap_or(pattern);
    let mut found = Vec::new();
    let mut stack = vec![String::new()];
    while let Some(rel) = stack.pop() {
        let dir = if rel.is_empty() {
            root.to_string()
        } else {
            join(root, &rel)
        };
        for entry in entries(&dir) {
            if entry.name.starts_with('.') {
                continue;
            }
            let child = if rel.is_empty() {
                entry.name.clone()
            } else {
                format!("{rel}/{}", entry.name)
            };
            if minimatch(&child, pattern) {
                found.push(resolve(root, &child));
            }
            if entry.is_dir {
                stack.push(child);
            }
        }
    }
    found.sort();
    found.dedup();
    found
}

/// `collectFilesFromManifestEntries`: plain entries resolve under `root`, globs expand; override patterns skipped.
pub fn files_from_manifest_entries(entries: &[String], root: &str, kind: ResourceType) -> Vec<String> {
    let mut resolved = Vec::new();
    for entry in entries.iter().filter(|e| !is_override(e)) {
        if has_glob(entry) {
            resolved.extend(expand_glob(entry, root));
        } else {
            resolved.push(resolve(root, entry));
        }
    }
    files_from_paths(&resolved, kind)
}

/// The override patterns of a list.
pub fn override_patterns(entries: &[String]) -> Vec<String> {
    entries.iter().filter(|e| is_override(e)).cloned().collect()
}

// ------------------------------------------------------------------------------------------------ auto-discovery

pub fn auto_extensions(dir: &str) -> Vec<String> {
    auto_extension_entries(dir)
}

pub fn auto_skills(dir: &str, agents: bool) -> Vec<String> {
    skill_entries(dir, if agents { SkillMode::Agents } else { SkillMode::Pi })
}

pub fn auto_prompts(dir: &str) -> Vec<String> {
    top_level_files(dir, ".md")
}

pub fn auto_themes(dir: &str) -> Vec<String> {
    top_level_files(dir, ".json")
}

/// `collectAncestorAgentsSkillDirs`: `<dir>/.agents/skills` from `start` up to the git repository root (or `/`).
pub fn ancestor_agents_skill_dirs(start: &str) -> Vec<String> {
    let start = resolve("/", start);
    let git_root = {
        let mut dir = start.clone();
        loop {
            if exists(&join(&dir, ".git")) {
                break Some(dir);
            }
            let parent = dirname(&dir);
            if parent == dir {
                break None;
            }
            dir = parent;
        }
    };
    let mut out = Vec::new();
    let mut dir = start;
    loop {
        out.push(join(&join(&dir, ".agents"), "skills"));
        if git_root.as_deref() == Some(dir.as_str()) {
            break;
        }
        let parent = dirname(&dir);
        if parent == dir {
            break;
        }
        dir = parent;
    }
    out
}
