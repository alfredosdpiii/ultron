//! The structured system prompt (core/system-prompt.ts) and its rendered text (`getSystemMessageText`).

use crate::context_files::ContextFile;
use crate::skills::{Skill, bundled_skills_note, format_always_skills_for_prompt, format_skills_for_prompt};

/// Paths named in the `docs` section (config.ts `getReadmePath`, `getDocsPath`, `getExamplesPath`).
#[derive(Clone, Debug, Default)]
pub struct DocPaths {
    pub readme: String,
    pub docs: String,
    pub examples: String,
}

/// `BuildSystemPromptOptions`, with TS defaults applied by `Default` (except `selected_tools`, see `new`).
#[derive(Clone, Debug, Default)]
pub struct BuildOptions {
    pub custom_prompt: Option<String>,
    pub selected_tools: Vec<String>,
    pub tool_snippets: Vec<(String, String)>,
    pub tool_guidelines: Vec<(String, Vec<String>)>,
    pub prompt_guidelines: Vec<String>,
    pub append_system_prompt: String,
    pub sections: Vec<(String, String)>,
    pub cwd: String,
    pub context_files: Vec<ContextFile>,
    pub skills: Vec<Skill>,
    pub engineering: bool,
    pub include_harness_docs: bool,
    pub doc_paths: DocPaths,
    /// `getBundledSkillsDir()`: where bundled skills live, if any.
    pub bundled_skills_dir: Option<String>,
    /// The persona's text (crate::persona), its own section after the context files and before the skills.
    pub persona: Option<String>,
    /// Engineering mode with the Rust guide: the model uses the bundled skills on its own
    /// ([`crate::skills::bundled_skills_note`]).
    pub proactive_skills: bool,
}

impl BuildOptions {
    /// Defaults of `normalizeBuildSystemPromptOptions` for `cwd`.
    pub fn new(cwd: &str) -> Self {
        Self {
            selected_tools: ["read", "bash", "edit", "write"].map(String::from).to_vec(),
            include_harness_docs: true,
            cwd: cwd.to_string(),
            ..Self::default()
        }
    }
}

/// Ordered (name, text) sections; `preamble` is untagged, every other one wrapped in `<name>` tags.
pub type Sections = Vec<(String, String)>;

fn lookup<'a, T>(pairs: &'a [(String, T)], name: &str) -> Option<&'a T> {
    pairs.iter().find(|(key, _)| key == name).map(|(_, value)| value)
}

fn valid_section_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_lowercase())
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

fn build_rules(options: &BuildOptions) -> String {
    let mut rules: Vec<String> = Vec::new();
    let mut add = |rule: &str| {
        let rule = rule.trim();
        if !rule.is_empty() && !rules.iter().any(|r| r == rule) {
            rules.push(rule.to_string());
        }
    };
    let has = |name: &str| options.selected_tools.iter().any(|t| t == name);
    let (bash, pwsh) = (has("bash"), has("powershell"));
    if (bash || pwsh) && !has("grep") && !has("find") && !has("ls") {
        add(if bash && pwsh {
            "Use bash or PowerShell for file operations like listing, searching, and finding files"
        } else if pwsh {
            "Use PowerShell for file operations like listing, searching, and finding files"
        } else {
            "Use bash for file operations like ls, rg, find"
        });
    }
    for name in &options.selected_tools {
        for rule in lookup(&options.tool_guidelines, name).into_iter().flatten() {
            add(rule);
        }
    }
    for rule in &options.prompt_guidelines {
        add(rule);
    }
    add("Be concise in your responses");
    add("Show file paths clearly when working with files");
    rules
        .iter()
        .map(|rule| format!("- {rule}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Set `name` like a JS object property: an existing key keeps its position.
fn set(sections: &mut Sections, name: &str, text: String) {
    match sections.iter_mut().find(|(key, _)| key == name) {
        Some(entry) => entry.1 = text,
        None => sections.push((name.to_string(), text)),
    }
}

/// `buildSystemPromptSections`. Errors on an invalid custom section name.
pub fn build_system_prompt_sections(options: &BuildOptions) -> Result<Sections, String> {
    for (name, _) in &options.sections {
        if !valid_section_name(name) || name == "preamble" {
            return Err(format!("Invalid system prompt section name: {name}"));
        }
    }
    let skills: Vec<Skill> = if options.engineering {
        options.skills.clone()
    } else {
        options
            .skills
            .iter()
            .map(|s| Skill {
                always_on_content: None,
                ..s.clone()
            })
            .collect()
    };
    let mut out: Sections = Vec::new();
    match options.custom_prompt.as_deref().filter(|p| !p.is_empty()) {
        Some(custom) => set(&mut out, "preamble", custom.to_string()),
        None => {
            set(&mut out, "preamble", "You are an expert coding assistant operating inside pi, a coding agent harness. You help users by reading files, executing commands, editing code, and writing new files.".to_string());
            let visible: Vec<String> = options
                .selected_tools
                .iter()
                .filter_map(|name| {
                    let snippet = lookup(&options.tool_snippets, name).filter(|s| !s.is_empty())?;
                    Some(format!("- {name}: {snippet}"))
                })
                .collect();
            let tools = if visible.is_empty() {
                "(none)".to_string()
            } else {
                visible.join("\n")
            };
            set(
                &mut out,
                "tools",
                if options.include_harness_docs {
                    format!(
                        "{tools}\n\nIn addition to the tools above, you may have access to other custom tools depending on the project."
                    )
                } else {
                    tools
                },
            );
            set(&mut out, "rules", build_rules(options));
            if options.include_harness_docs {
                let DocPaths { readme, docs, examples } = &options.doc_paths;
                set(&mut out, "docs", format!("Pi documentation (read only when the user asks about pi itself, its SDK, extensions, themes, skills, or TUI):
- Main documentation: {readme}
- Additional docs: {docs}
- Examples: {examples} (extensions, custom tools, SDK)
- When reading pi docs or examples, resolve docs/... under Additional docs and examples/... under Examples, not the current working directory
- When asked about: extensions (docs/extensions.md, examples/extensions/), themes (docs/themes.md), skills (docs/skills.md), prompt templates (docs/prompt-templates.md), TUI components (docs/tui.md), keybindings (docs/keybindings.md), SDK integrations (docs/sdk.md), custom providers (docs/custom-provider.md), adding models (docs/models.md), pi packages (docs/packages.md), environment variables (docs/environment-variables.md)
- When working on pi topics, read the docs and examples, and follow .md cross-references before implementing
- Always read pi .md files completely and follow links to related docs (e.g., tui.md for TUI API details)"));
            }
        }
    }
    if !options.append_system_prompt.is_empty() {
        set(&mut out, "addendum", options.append_system_prompt.clone());
    }
    if !options.context_files.is_empty() {
        let mut parts = vec!["Project-specific instructions and guidelines:".to_string()];
        parts.extend(options.context_files.iter().map(|ContextFile { path, content }| {
            format!("<project_instructions path=\"{path}\">\n{content}\n</project_instructions>")
        }));
        set(&mut out, "project_context", parts.join("\n\n"));
    }
    if let Some(persona) = options.persona.as_deref().filter(|p| !p.is_empty()) {
        set(&mut out, "persona", crate::persona::section_text(persona));
    }
    let read_tool = ["read", "bash", "rlm"]
        .into_iter()
        .find(|tool| options.selected_tools.iter().any(|t| t == tool));
    if let Some(tool) = read_tool.filter(|_| !skills.is_empty()) {
        let parts = [
            format_always_skills_for_prompt(&skills),
            format_skills_for_prompt(&skills, tool).trim().to_string(),
            bundled_skills_note(&skills, options.bundled_skills_dir.as_deref(), options.proactive_skills),
        ];
        let text = parts
            .into_iter()
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        if !text.is_empty() {
            set(&mut out, "skills", text);
        }
    }
    set(&mut out, "cwd", options.cwd.replace('\\', "/"));
    for (name, content) in &options.sections {
        if !content.is_empty() {
            set(&mut out, name, content.clone());
        }
    }
    Ok(out
        .into_iter()
        .map(|(name, text)| {
            if name == "preamble" {
                (name, text)
            } else {
                let wrapped = format!("<{name}>\n{text}\n</{name}>");
                (name, wrapped)
            }
        })
        .collect())
}

/// `buildSystemPrompt`: a forced prompt as is, else the sections joined as the system message replays them.
pub fn build_system_prompt(options: &BuildOptions, force: Option<&str>) -> Result<String, String> {
    if let Some(force) = force {
        return Ok(force.to_string());
    }
    let sections = build_system_prompt_sections(options)?;
    Ok(sections
        .into_iter()
        .map(|(_, text)| text)
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n"))
}
