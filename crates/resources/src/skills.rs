//! Skills: discovery (with the `.gitignore`/`.ignore`/`.fdignore` rules of the directories walked), frontmatter
//! validation, always-on skills and their system prompt listing (core/skills.ts).

use crate::Diagnostic;
use crate::frontmatter;
use crate::ignore::{Ignore, add_ignore_rules};
use crate::paths::{basename, canonicalize, dirname, join, relative, resolve, resolve_path};
use std::collections::HashSet;
use std::fs;
use yaml_rust2::Yaml;

const MAX_NAME_LENGTH: usize = 64;
const MAX_DESCRIPTION_LENGTH: usize = 1024;
/// Most characters of always-on skill text in one system prompt.
pub const ALWAYS_SKILLS_MAX_CHARS: usize = 16_000;
/// Project config directory name (`CONFIG_DIR_NAME`).
pub const CONFIG_DIR_NAME: &str = ".pi";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub file_path: String,
    pub base_dir: String,
    /// "user", "project", "path" or another source id.
    pub source: String,
    pub disable_model_invocation: bool,
    /// The skill body when `alwaysSkills` puts it in every system prompt.
    pub always_on_content: Option<String>,
}

#[derive(Debug, Default)]
pub struct LoadSkillsResult {
    pub skills: Vec<Skill>,
    pub diagnostics: Vec<Diagnostic>,
}

/// JS string length (UTF-16 code units).
fn js_len(text: &str) -> usize {
    text.encode_utf16().count()
}

fn validate_name(name: &str) -> Vec<String> {
    let mut errors = Vec::new();
    if js_len(name) > MAX_NAME_LENGTH {
        errors.push(format!("name exceeds {MAX_NAME_LENGTH} characters ({})", js_len(name)));
    }
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        errors.push("name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)".to_string());
    }
    if name.starts_with('-') || name.ends_with('-') {
        errors.push("name must not start or end with a hyphen".to_string());
    }
    if name.contains("--") {
        errors.push("name must not contain consecutive hyphens".to_string());
    }
    errors
}

fn validate_description(description: Option<&str>) -> Vec<String> {
    match description {
        Some(text) if !text.trim().is_empty() => {
            if js_len(text) > MAX_DESCRIPTION_LENGTH {
                vec![format!(
                    "description exceeds {MAX_DESCRIPTION_LENGTH} characters ({})",
                    js_len(text)
                )]
            } else {
                vec![]
            }
        }
        _ => vec!["description is required".to_string()],
    }
}

fn load_skill_from_file(file_path: &str, source: &str) -> (Option<Skill>, Vec<Diagnostic>) {
    let mut diagnostics = Vec::new();
    let declared = basename(file_path) == "SKILL.md";
    let raw = match fs::read_to_string(file_path) {
        Ok(raw) => raw,
        Err(error) => {
            diagnostics.push(Diagnostic::warning(error.to_string(), Some(file_path)));
            return (None, diagnostics);
        }
    };
    let fm = match frontmatter::parse(&raw) {
        Ok(parsed) => parsed.frontmatter,
        Err(message) => {
            if declared {
                diagnostics.push(Diagnostic::warning(message, Some(file_path)));
            }
            return (None, diagnostics);
        }
    };
    let description = frontmatter::string_field(&fm, "description");
    let has_description = description.is_some_and(|d| !d.trim().is_empty());
    if !declared && !has_description {
        return (None, diagnostics);
    }
    let skill_dir = dirname(file_path);
    for error in validate_description(description) {
        diagnostics.push(Diagnostic::warning(error, Some(file_path)));
    }
    let name = match frontmatter::string_field(&fm, "name") {
        Some(name) if !name.is_empty() => name.to_string(),
        _ => basename(&skill_dir).to_string(),
    };
    for error in validate_name(&name) {
        diagnostics.push(Diagnostic::warning(error, Some(file_path)));
    }
    let Some(description) = description.filter(|_| has_description) else {
        return (None, diagnostics);
    };
    let skill = Skill {
        name,
        description: description.to_string(),
        file_path: file_path.to_string(),
        base_dir: skill_dir,
        source: source.to_string(),
        disable_model_invocation: fm["disable-model-invocation"] == Yaml::Boolean(true),
        always_on_content: None,
    };
    (Some(skill), diagnostics)
}

/// Directory entries sorted by name, as Node's `readdirSync` (libuv scandir) returns them.
fn sorted_entries(dir: &str) -> Vec<(String, fs::FileType)> {
    let Ok(read) = fs::read_dir(dir) else { return Vec::new() };
    let mut entries: Vec<(String, fs::FileType)> = read
        .filter_map(Result::ok)
        .filter_map(|e| Some((e.file_name().into_string().ok()?, e.file_type().ok()?)))
        .collect();
    entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    entries
}

/// (is_dir, is_file), following symlinks; `None` for a broken link.
fn kind(path: &str, file_type: &fs::FileType) -> Option<(bool, bool)> {
    if file_type.is_symlink() {
        let meta = fs::metadata(path).ok()?;
        return Some((meta.is_dir(), meta.is_file()));
    }
    Some((file_type.is_dir(), file_type.is_file()))
}

/// Load skills from a directory: a dir with SKILL.md is a skill root (no recursion); otherwise direct `.md`
/// children of the top directory are skills, and subdirectories are searched for SKILL.md.
pub fn load_skills_from_dir(dir: &str, source: &str) -> LoadSkillsResult {
    load_dir(dir, source, true)
}

fn load_dir(dir: &str, source: &str, include_root_files: bool) -> LoadSkillsResult {
    load_dir_with(dir, source, include_root_files, &mut Ignore::new(), dir)
}

/// `loadSkillsFromDirInternal`: the `.gitignore`, `.ignore` and `.fdignore` rules of each directory apply to it and
/// below, relative to the walk's `root`.
fn load_dir_with(dir: &str, source: &str, include_root_files: bool, ig: &mut Ignore, root: &str) -> LoadSkillsResult {
    let mut result = LoadSkillsResult::default();
    if !std::path::Path::new(dir).exists() {
        return result;
    }
    add_ignore_rules(ig, dir, root);
    let entries = sorted_entries(dir);
    if let Some((name, file_type)) = entries.iter().find(|(name, _)| name == "SKILL.md") {
        let full = join(dir, name);
        if kind(&full, file_type).is_some_and(|(_, is_file)| is_file) && !ig.ignores(&relative(root, &full)) {
            let (skill, diagnostics) = load_skill_from_file(&full, source);
            result.skills.extend(skill);
            result.diagnostics.extend(diagnostics);
            return result;
        }
    }
    for (name, file_type) in &entries {
        if name.starts_with('.') || name == "node_modules" {
            continue;
        }
        let full = join(dir, name);
        let Some((is_dir, is_file)) = kind(&full, file_type) else {
            continue;
        };
        let rel = relative(root, &full);
        if ig.ignores(&if is_dir { format!("{rel}/") } else { rel }) {
            continue;
        }
        if is_dir {
            let sub = load_dir_with(&full, source, false, ig, root);
            result.skills.extend(sub.skills);
            result.diagnostics.extend(sub.diagnostics);
            continue;
        }
        if !is_file || !include_root_files || !name.ends_with(".md") {
            continue;
        }
        let (skill, diagnostics) = load_skill_from_file(&full, source);
        result.skills.extend(skill);
        result.diagnostics.extend(diagnostics);
    }
    result
}

/// Options of `loadSkills`.
pub struct LoadSkillsOptions<'a> {
    pub cwd: &'a str,
    pub agent_dir: &'a str,
    pub skill_paths: &'a [String],
    pub include_defaults: bool,
}

/// Load skills from the agent dir, the project's `.pi/skills` and explicit paths; first name wins.
pub fn load_skills(options: &LoadSkillsOptions) -> LoadSkillsResult {
    let cwd = resolve_path(options.cwd, "/", false);
    let agent_dir = resolve_path(options.agent_dir, &cwd, false);
    let user_dir = join(&agent_dir, "skills");
    let project_dir = resolve(&cwd, &format!("{CONFIG_DIR_NAME}/skills"));

    let mut skills: Vec<Skill> = Vec::new();
    let mut real_paths: HashSet<String> = HashSet::new();
    let mut diagnostics: Vec<Diagnostic> = Vec::new();
    let mut collisions: Vec<Diagnostic> = Vec::new();
    let mut add = |result: LoadSkillsResult, diagnostics: &mut Vec<Diagnostic>| {
        diagnostics.extend(result.diagnostics);
        for skill in result.skills {
            let real = canonicalize(&skill.file_path);
            if real_paths.contains(&real) {
                continue;
            }
            if let Some(existing) = skills.iter().find(|s| s.name == skill.name) {
                collisions.push(Diagnostic {
                    kind: "collision",
                    message: format!("name \"{}\" collision", skill.name),
                    path: Some(skill.file_path.clone()),
                    collision: Some((existing.file_path.clone(), skill.file_path.clone())),
                });
            } else {
                real_paths.insert(real);
                skills.push(skill);
            }
        }
    };

    if options.include_defaults {
        add(load_dir(&user_dir, "user", true), &mut diagnostics);
        add(load_dir(&project_dir, "project", true), &mut diagnostics);
    }
    let under = |target: &str, root: &str| target == root || target.starts_with(&format!("{root}/"));
    for raw in options.skill_paths {
        let path = resolve_path(raw, &cwd, true);
        let Ok(meta) = fs::metadata(&path) else {
            diagnostics.push(Diagnostic::warning("skill path does not exist", Some(&path)));
            continue;
        };
        let source = if !options.include_defaults && under(&path, &user_dir) {
            "user"
        } else if !options.include_defaults && under(&path, &project_dir) {
            "project"
        } else {
            "path"
        };
        if meta.is_dir() {
            add(load_dir(&path, source, true), &mut diagnostics);
        } else if meta.is_file() && path.ends_with(".md") {
            let (skill, found) = load_skill_from_file(&path, source);
            match skill {
                Some(skill) => add(
                    LoadSkillsResult {
                        skills: vec![skill],
                        diagnostics: found,
                    },
                    &mut diagnostics,
                ),
                None => diagnostics.extend(found),
            }
        } else {
            diagnostics.push(Diagnostic::warning("skill path is not a markdown file", Some(&path)));
        }
    }
    diagnostics.extend(collisions);
    LoadSkillsResult { skills, diagnostics }
}

/// Mark the named skills always-on (body read once), in order, up to `ALWAYS_SKILLS_MAX_CHARS`.
pub fn apply_always_skills(skills: &mut [Skill], names: &[String]) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let mut used = 0;
    let mut seen = HashSet::new();
    for name in names {
        if !seen.insert(name) {
            continue;
        }
        let Some(skill) = skills.iter_mut().find(|s| &s.name == name) else {
            diagnostics.push(Diagnostic::warning(
                format!("alwaysSkills: no skill named \"{name}\" is loaded"),
                None,
            ));
            continue;
        };
        let body = fs::read_to_string(&skill.file_path)
            .map_err(|e| e.to_string())
            .and_then(|raw| frontmatter::strip(&raw));
        let body = match body {
            Ok(body) => body.trim().to_string(),
            Err(reason) => {
                diagnostics.push(Diagnostic::warning(
                    format!("alwaysSkills: cannot read {name}: {reason}"),
                    Some(&skill.file_path),
                ));
                continue;
            }
        };
        let len = js_len(&body);
        if used + len > ALWAYS_SKILLS_MAX_CHARS {
            diagnostics.push(Diagnostic::warning(
                format!(
                    "alwaysSkills: {name} ({len} chars) would pass the {ALWAYS_SKILLS_MAX_CHARS}-char limit for always-on skills; it stays a listed skill"
                ),
                Some(&skill.file_path),
            ));
            continue;
        }
        used += len;
        skill.always_on_content = Some(body);
    }
    diagnostics
}

fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// The always-on skills' full text for the system prompt, or "" when none is set.
pub fn format_always_skills_for_prompt(skills: &[Skill]) -> String {
    let always: Vec<String> = skills
        .iter()
        .filter_map(|skill| {
            let content = skill.always_on_content.as_ref()?;
            Some(format!(
                "<skill name=\"{}\" location=\"{}\">\nReferences are relative to {}.\n\n{content}\n</skill>",
                escape_xml(&skill.name),
                escape_xml(&skill.file_path),
                escape_xml(&skill.base_dir)
            ))
        })
        .collect();
    if always.is_empty() {
        return String::new();
    }
    let mut parts = vec![
        "Always-on skills: the user set these to apply to every task. Follow them without being asked; their text is below, so do not read their files.".to_string(),
    ];
    parts.extend(always);
    parts.join("\n\n")
}

/// The `<available_skills>` listing; `file_read_tool` is "read", "bash" or "rlm".
pub fn format_skills_for_prompt(skills: &[Skill], file_read_tool: &str) -> String {
    let visible: Vec<&Skill> = skills
        .iter()
        .filter(|s| !s.disable_model_invocation && s.always_on_content.is_none())
        .collect();
    if visible.is_empty() {
        return String::new();
    }
    let how = match file_read_tool {
        "read" => "Use the read tool to load a skill's file when the task matches its description.",
        "rlm" => {
            "Load a skill's file in the rlm tool (`print(Path(location).read_text())`) when the task matches its description."
        }
        _ => "Use bash to load a skill's file when the task matches its description.",
    };
    let mut lines = vec![
        "\n\nThe following skills provide specialized instructions for specific tasks.".to_string(),
        how.to_string(),
        "When a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.".to_string(),
        String::new(),
        "<available_skills>".to_string(),
    ];
    for skill in visible {
        lines.push("  <skill>".to_string());
        lines.push(format!("    <name>{}</name>", escape_xml(&skill.name)));
        lines.push(format!(
            "    <description>{}</description>",
            escape_xml(&skill.description)
        ));
        lines.push(format!("    <location>{}</location>", escape_xml(&skill.file_path)));
        lines.push("  </skill>".to_string());
    }
    lines.push("</available_skills>".to_string());
    lines.join("\n")
}

/// The first sentence of a description, at most `max` characters.
fn short_purpose(description: &str, max: usize) -> String {
    let flat = crate::runtime_guide::collapse_whitespace(description);
    let sentence = match flat.find(". ") {
        Some(i) => &flat[..=i],
        None => flat.as_str(),
    };
    if sentence.chars().count() > max {
        format!("{}…", sentence.chars().take(max - 1).collect::<String>().trim_end())
    } else {
        sentence.to_string()
    }
}

/// Index of the bundled skills that are loaded but hidden from the listing; "" when none (or no bundled dir).
pub fn bundled_skills_note(skills: &[Skill], bundled: Option<&str>) -> String {
    let Some(bundled) = bundled else { return String::new() };
    let prefix = format!("{bundled}/");
    let listed: Vec<String> = skills
        .iter()
        .filter(|s| {
            s.disable_model_invocation
                && s.always_on_content.is_none()
                && s.file_path.starts_with(&prefix)
                && !s.name.starts_with("principle-")
        })
        .map(|s| {
            format!(
                "- {}: {}",
                relative(bundled, &dirname(&s.file_path)),
                short_purpose(&s.description, 80)
            )
        })
        .collect();
    if listed.is_empty() {
        return String::new();
    }
    let mut lines = vec![format!(
        "Bundled skills in {bundled} (<source>/<name>/SKILL.md). Read one only when the user asks for it or the task plainly calls for that workflow; ordinary coding needs none. The principles pstack's skills cite are pstack/principle-<name>/SKILL.md."
    )];
    lines.extend(listed);
    lines.join("\n")
}
