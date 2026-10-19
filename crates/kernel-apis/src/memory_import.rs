//! `ultron memory import --from claude-code|codex`: the notes other harnesses keep on disk become typed memory
//! records (crate::memory_records), stored through `memory.propose` with the user's evidence, so the same secret
//! check refuses what holds a secret. Which files are read follows Mirror (`memory/import/*.rs`):
//!
//! - Claude Code: every `<config>/projects/<encoded project>/memory/` directory (`<config>` is `CLAUDE_CONFIG_DIR`,
//!   else `~/.claude`): one note per Markdown file, `MEMORY.md` (the index), dotfiles, symlinks and `old/` left
//!   out. YAML frontmatter `name` gives the key, `description` the situation and `metadata.type` (or `type`) the
//!   record type: `feedback` → preference, `user`/`project` → fact, else reference. A project directory whose
//!   encoded name maps back to exactly one existing directory makes its notes `project` records of that
//!   directory; otherwise they are `global`.
//! - Codex: `<codex home>/memories/` (`CODEX_HOME`, else `~/.codex`): `memory_summary.md` (User Profile → fact,
//!   User preferences → preference, General Tips → procedure) and `MEMORY.md` (per task group: User preferences,
//!   Reusable knowledge → fact, Failures and how to do differently → pitfall; `[Task n]` tags name the situation).
//!   Bullets are the unit. All `global`.
//!
//! Every record is tagged with its source file (`source`: harness and home-relative path). Keys are the note's
//! slug plus a hash of what identifies it in its source (the file for Claude Code, file and text for Codex), so a
//! rerun finds the same keys: a record whose key is already indexed with the same text and fields is unchanged,
//! one with other text is replaced. Notes over their type's cap, or holding a secret, are refused.

use crate::memory::{MemoryError, MemoryService};
use crate::memory_records::{MAX_SITUATION_BYTES, RecordIndex, Typed, content_cap};
use crate::util::sha256_hex;
use kernel::Signal;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use yaml_rust2::{Yaml, YamlLoader};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Harness {
    ClaudeCode,
    Codex,
}

impl Harness {
    /// `--from` names: `claude-code` (or `claude`) and `codex`.
    pub fn parse(name: &str) -> Option<Harness> {
        match name {
            "claude-code" | "claude" => Some(Harness::ClaudeCode),
            "codex" => Some(Harness::Codex),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Harness::ClaudeCode => "Claude Code",
            Harness::Codex => "Codex",
        }
    }
}

/// One note as a reader produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub key: String,
    pub kind: &'static str,
    pub situation: String,
    pub content: String,
    /// The file the text came from.
    pub file: PathBuf,
}

/// One directory of notes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub harness: Harness,
    pub directory: PathBuf,
    /// The project the notes belong to (`project` scope), `None`: `global`.
    pub project: Option<PathBuf>,
}

/// Where a harness keeps its notes, given the injected environment.
pub fn notes_root(harness: Harness, home: &Path, var: &dyn Fn(&str) -> Option<String>) -> PathBuf {
    let configured = |name: &str| var(name).filter(|value| !value.trim().is_empty()).map(PathBuf::from);
    match harness {
        Harness::ClaudeCode => configured("CLAUDE_CONFIG_DIR")
            .unwrap_or_else(|| home.join(".claude"))
            .join("projects"),
        Harness::Codex => configured("CODEX_HOME")
            .unwrap_or_else(|| home.join(".codex"))
            .join("memories"),
    }
}

/// The note directories of `harness` under `root` ([`notes_root`]), in path order.
pub fn discover(harness: Harness, root: &Path) -> Vec<Source> {
    match harness {
        Harness::ClaudeCode => {
            let mut projects: Vec<PathBuf> = std::fs::read_dir(root)
                .into_iter()
                .flatten()
                .flatten()
                .map(|entry| entry.path())
                .filter(|project| project.join("memory").is_dir())
                .collect();
            projects.sort();
            projects
                .into_iter()
                .map(|project| Source {
                    harness,
                    directory: project.join("memory"),
                    project: project_path(&project),
                })
                .collect()
        }
        Harness::Codex => {
            if root.join("memory_summary.md").is_file() || root.join("MEMORY.md").is_file() {
                vec![Source {
                    harness,
                    directory: root.to_path_buf(),
                    project: None,
                }]
            } else {
                Vec::new()
            }
        }
    }
}

/// The checkout a Claude Code project directory stands for, when its encoded name (`-home-me-src-repo`) maps back
/// to exactly one existing directory (a `-` is a separator or a literal hyphen).
pub fn project_path(project_dir: &Path) -> Option<PathBuf> {
    let name = project_dir.file_name()?.to_string_lossy().into_owned();
    let segments: Vec<&str> = name.trim_start_matches('-').split('-').collect();
    let mut matches = Vec::new();
    resolve_segments(Path::new("/"), &segments, &mut matches);
    (matches.len() == 1).then(|| matches.remove(0))
}

fn resolve_segments(base: &Path, segments: &[&str], matches: &mut Vec<PathBuf>) {
    if matches.len() > 1 {
        return;
    }
    if segments.is_empty() {
        matches.push(base.to_path_buf());
        return;
    }
    for take in 1..=segments.len() {
        let candidate = base.join(segments[..take].join("-"));
        if candidate.is_dir() {
            resolve_segments(&candidate, &segments[take..], matches);
        }
    }
}

impl Source {
    pub fn read(&self) -> Result<Vec<Note>, String> {
        match self.harness {
            Harness::ClaudeCode => claude_code_notes(&self.directory),
            Harness::Codex => codex_notes(&self.directory),
        }
    }
}

// --- Claude Code ---------------------------------------------------------------------------------------------------

fn collect_markdown(directory: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = std::fs::read_dir(directory).map_err(|e| format!("{}: {e}", directory.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let file_type = entry.file_type().map_err(|e| e.to_string())?;
        if name.starts_with('.') || file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            if name != "old" {
                collect_markdown(&path, out)?;
            }
        } else if file_type.is_file() && name.ends_with(".md") && name != "MEMORY.md" {
            out.push(path);
        }
    }
    Ok(())
}

/// Leading `---` fenced YAML and the body after it.
fn split_frontmatter(text: &str) -> (Option<&str>, &str) {
    let Some(rest) = text.strip_prefix("---\n").or_else(|| text.strip_prefix("---\r\n")) else {
        return (None, text);
    };
    let mut cursor = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            return (Some(&rest[..cursor]), &rest[cursor + line.len()..]);
        }
        cursor += line.len();
    }
    (None, text)
}

fn yaml_text<'a>(yaml: &'a Yaml, key: &str) -> Option<&'a str> {
    yaml[key].as_str().map(str::trim).filter(|text| !text.is_empty())
}

/// Every note under a Claude Code memory directory, in path order.
pub fn claude_code_notes(directory: &Path) -> Result<Vec<Note>, String> {
    let mut files = Vec::new();
    collect_markdown(directory, &mut files)?;
    files.sort();
    let mut notes = Vec::new();
    for path in files {
        let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let (front, body) = split_frontmatter(&text);
        let front = front
            .and_then(|yaml| YamlLoader::load_from_str(yaml).ok())
            .and_then(|mut docs| (!docs.is_empty()).then(|| docs.remove(0)))
            .unwrap_or(Yaml::Null);
        let body = body.trim();
        if body.is_empty() {
            continue;
        }
        let stem = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
        let name = yaml_text(&front, "name").map(str::to_string).unwrap_or(stem);
        let kind = match yaml_text(&front["metadata"], "type").or_else(|| yaml_text(&front, "type")) {
            Some("feedback") => "preference",
            Some("user" | "project") => "fact",
            _ => "reference",
        };
        notes.push(Note {
            key: slug_key(&name),
            kind,
            situation: yaml_text(&front, "description").unwrap_or_default().to_string(),
            content: body.to_string(),
            file: path,
        });
    }
    Ok(notes)
}

// --- Codex -----------------------------------------------------------------------------------------------------------

struct Section {
    heading: String,
    lines: Vec<String>,
}

impl Section {
    /// Bullets, indented continuation text joined with a space; an indented list marker starts its own bullet.
    fn bullets(&self) -> Vec<String> {
        let mut bullets: Vec<String> = Vec::new();
        for line in &self.lines {
            let trimmed = line.trim_start_matches([' ', '\t']);
            if let Some(item) = trimmed.strip_prefix("- ") {
                bullets.push(item.trim().to_string());
            } else if (line.starts_with(' ') || line.starts_with('\t'))
                && let Some(last) = bullets.last_mut()
            {
                last.push(' ');
                last.push_str(line.trim());
            }
        }
        bullets
    }

    fn paragraphs(&self) -> Vec<String> {
        self.lines
            .iter()
            .map(|line| line.trim())
            .filter(|line| !line.is_empty() && !line.starts_with("- "))
            .map(str::to_string)
            .collect()
    }
}

/// `##` sections; any other heading ends the current one.
fn sections(text: &str) -> Vec<Section> {
    let mut sections: Vec<Section> = Vec::new();
    for line in text.lines() {
        if let Some(heading) = line.strip_prefix("## ") {
            sections.push(Section {
                heading: heading.trim().to_string(),
                lines: Vec::new(),
            });
        } else if line.starts_with('#') {
            sections.push(Section {
                heading: String::new(),
                lines: Vec::new(),
            });
        } else if let Some(section) = sections.last_mut() {
            section.lines.push(line.to_string());
        }
    }
    sections.retain(|section| !section.heading.is_empty());
    sections
}

fn note(key: &str, kind: &'static str, situation: &str, content: &str, file: &Path) -> Note {
    Note {
        key: slug_key(key),
        kind,
        situation: situation.to_string(),
        content: content.to_string(),
        file: file.to_path_buf(),
    }
}

fn summary_notes(text: &str, file: &Path) -> Vec<Note> {
    let mut notes = Vec::new();
    for section in sections(text) {
        match section.heading.as_str() {
            "User Profile" => {
                let profile = section.paragraphs().join(" ");
                if !profile.is_empty() {
                    notes.push(note(
                        "codex-user-profile",
                        "fact",
                        "who the user is, as Codex summarized it",
                        &profile,
                        file,
                    ));
                }
            }
            "User preferences" => {
                for bullet in section.bullets() {
                    notes.push(note(
                        &format!("codex-pref-{}", slug_key(&bullet)),
                        "preference",
                        "how the user wants work done, as Codex summarized it",
                        &bullet,
                        file,
                    ));
                }
            }
            "General Tips" => {
                for bullet in section.bullets() {
                    notes.push(note(
                        &format!("codex-tip-{}", slug_key(&bullet)),
                        "procedure",
                        "working practice Codex learned across tasks",
                        &bullet,
                        file,
                    ));
                }
            }
            _ => {}
        }
    }
    notes
}

/// `Task <n>: <title>` of a heading.
fn task_heading(heading: &str) -> Option<(String, String)> {
    let rest = heading.strip_prefix("Task ")?;
    let (number, title) = rest.split_once(": ")?;
    (!number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()) && !title.trim().is_empty())
        .then(|| (number.to_string(), title.trim().to_string()))
}

/// A trailing `[Task <n>]` tag: its number and where it starts (with the whitespace before it).
fn trailing_task_tag(content: &str) -> Option<(String, usize)> {
    let trimmed = content.trim_end();
    let inner = trimmed.strip_suffix(']')?;
    let open = inner.rfind("[Task ")?;
    let number = &inner[open + "[Task ".len()..];
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let start = content[..open].trim_end().len();
    Some((number.to_string(), start))
}

fn memory_notes(text: &str, file: &Path) -> Vec<Note> {
    let mut tasks: BTreeMap<String, String> = BTreeMap::new();
    let mut notes = Vec::new();
    for section in sections(text) {
        if let Some((number, title)) = task_heading(&section.heading) {
            tasks.insert(number, title);
            continue;
        }
        let (kind, prefix) = match section.heading.as_str() {
            "User preferences" => ("preference", "codex-pref"),
            "Reusable knowledge" => ("fact", "codex-fact"),
            "Failures and how to do differently" => ("pitfall", "codex-pitfall"),
            _ => continue,
        };
        for bullet in section.bullets() {
            let mut situation = String::from("from the user's Codex task memory");
            let mut content = bullet.clone();
            while let Some((number, start)) = trailing_task_tag(&content) {
                if let Some(title) = tasks.get(&number) {
                    situation = format!("Codex task: {title}");
                }
                content.truncate(start);
            }
            if content.trim().is_empty() {
                continue;
            }
            notes.push(note(
                &format!("{prefix}-{}", slug_key(&content)),
                kind,
                &situation,
                content.trim(),
                file,
            ));
        }
    }
    notes
}

/// Notes of a Codex memories directory: `memory_summary.md`, then `MEMORY.md`.
pub fn codex_notes(directory: &Path) -> Result<Vec<Note>, String> {
    let mut notes = Vec::new();
    let mut read_any = false;
    for (name, parse) in [
        ("memory_summary.md", summary_notes as fn(&str, &Path) -> Vec<Note>),
        ("MEMORY.md", memory_notes),
    ] {
        let path = directory.join(name);
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                read_any = true;
                notes.extend(parse(&text, &path));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("{}: {error}", path.display())),
        }
    }
    if !read_any {
        return Err(format!("no Codex memory files found in {}", directory.display()));
    }
    Ok(notes)
}

// --- planning and import -------------------------------------------------------------------------------------------

/// A kebab-case key of at most 60 bytes from free text (Mirror `slug_key`).
pub fn slug_key(text: &str) -> String {
    let mut key = String::new();
    let mut last_dash = true;
    for character in text.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            key.push(character);
            last_dash = false;
        } else if !last_dash {
            key.push('-');
            last_dash = true;
        }
        if key.len() >= 60 {
            break;
        }
    }
    let key = key.trim_matches('-').to_string();
    if key.is_empty() { "note".to_string() } else { key }
}

/// Keep newlines and tabs, drop other control characters and carriage returns, trim.
fn clean(text: &str) -> String {
    text.replace("\r\n", "\n")
        .chars()
        .filter(|ch| !ch.is_control() || *ch == '\n' || *ch == '\t')
        .collect::<String>()
        .trim()
        .to_string()
}

fn truncate_bytes(text: &str, max: usize) -> &str {
    let mut end = 0;
    for (offset, ch) in text.char_indices() {
        if offset + ch.len_utf8() > max {
            break;
        }
        end = offset + ch.len_utf8();
    }
    &text[..end]
}

/// `path` with the home directory written as `~`.
pub fn home_relative(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// The note's key: its slug and 12 hex digits of what identifies it within its source (the `occurrence`-th note
/// with that identity, past the first, hashes its number too).
fn note_key(harness: Harness, note: &Note, occurrence: usize) -> String {
    let file = note.file.display().to_string();
    let mut identity = match harness {
        Harness::ClaudeCode => file,
        Harness::Codex => format!("{file}\0{}", clean(&note.content)),
    };
    if occurrence > 1 {
        identity.push_str(&format!("\0{occurrence}"));
    }
    let suffix = &sha256_hex(identity.as_bytes())[..12];
    let base = note.key.trim_end_matches('-');
    let end = base.len().min(64 - 1 - suffix.len());
    format!("{}-{suffix}", base[..end].trim_end_matches('-'))
}

/// What an import does with one note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// A new record.
    Add,
    /// The key is indexed with other text or fields: that record (id) is replaced.
    Replace(String),
    /// The key is indexed with this exact record (id).
    Unchanged(String),
    /// Not imported, and why.
    Refuse(String),
}

/// One planned record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planned {
    pub typed: Typed,
    pub content: String,
    pub scope: &'static str,
    /// The directory a `project` record belongs to.
    pub project: Option<PathBuf>,
    /// sha256 of the source file.
    pub file_sha: String,
    pub action: Action,
}

/// The identity tags a record of `scope` gets (as a session in `project` would tag it; hindsight
/// `worker_scope_tags`): computed by the caller, which knows the server's tagging.
pub type ScopeTags<'a> = &'a dyn Fn(Option<&Path>) -> Map<String, Value>;

/// The plan for `sources`: every note with its key, scope and action against `index`.
pub fn plan(sources: &[Source], home: &Path, index: &RecordIndex, tags: ScopeTags<'_>) -> Result<Vec<Planned>, String> {
    let mut planned = Vec::new();
    for source in sources {
        let notes = source.read()?;
        let scope = if source.project.is_some() { "project" } else { "global" };
        let filters = tags(source.project.as_deref());
        let mut seen: BTreeMap<String, usize> = BTreeMap::new();
        for note in notes {
            let count = seen.entry(note_key(source.harness, &note, 1)).or_default();
            *count += 1;
            let key = note_key(source.harness, &note, *count);
            let situation = clean(&note.situation).replace(['\n', '\t'], " ");
            let situation = match truncate_bytes(&situation, MAX_SITUATION_BYTES).trim() {
                "" => "from the user's imported notes".to_string(),
                text => text.to_string(),
            };
            let content = clean(&note.content);
            let typed = Typed {
                kind: note.kind.to_string(),
                key: Some(key.clone()),
                situation,
                path_globs: Vec::new(),
                source: Some(format!(
                    "{} {}",
                    source.harness.label(),
                    home_relative(&note.file, home)
                )),
            };
            let file_sha = std::fs::read(&note.file)
                .map(|bytes| sha256_hex(&bytes))
                .map_err(|e| format!("{}: {e}", note.file.display()))?;
            let action = if content.len() > content_cap(note.kind) {
                Action::Refuse(format!(
                    "over the {}-byte cap of a {} record",
                    content_cap(note.kind),
                    note.kind
                ))
            } else if host::secrets::contains_sensitive_memory(&content)
                || host::secrets::contains_sensitive_memory(&typed.screened())
            {
                Action::Refuse("holds a secret or personal identifier".into())
            } else {
                match index.by_key(&key, scope, &filters)? {
                    Some(entry) if entry.text_hash == sha256_hex(content.as_bytes()) && entry.typed == typed => {
                        Action::Unchanged(entry.id)
                    }
                    Some(entry) => Action::Replace(entry.id),
                    None => Action::Add,
                }
            };
            planned.push(Planned {
                typed,
                content,
                scope,
                project: source.project.clone(),
                file_sha,
                action,
            });
        }
    }
    Ok(planned)
}

/// The `memory.propose` payload of a planned record: the user's own notes, with the source file as evidence.
pub fn propose_payload(planned: &Planned) -> Map<String, Value> {
    let typed = &planned.typed;
    let source = typed.source.clone().unwrap_or_default();
    let mut payload = json!({
        "text": planned.content,
        "evidence": [{"ref": source, "sha256": planned.file_sha}],
        "scope": planned.scope,
        "evidenceClass": "user_statement",
        "type": typed.kind,
        "situation": typed.situation,
        "source": source,
    });
    if let Some(key) = &typed.key {
        payload["key"] = key.clone().into();
    }
    payload.as_object().cloned().unwrap_or_default()
}

/// Store one planned record (an `Add` or `Replace`) through `service`: the written operation, or why not.
pub fn import_one(service: &MemoryService, planned: &Planned) -> Result<Value, MemoryError> {
    service.propose(&propose_payload(planned), &Signal::new())
}
