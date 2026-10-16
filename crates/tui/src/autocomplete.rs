//! Autocomplete provider interface and `CombinedAutocompleteProvider` (port of packages/tui/src/autocomplete.ts):
//! slash commands and their arguments, `@` fuzzy file search through `fd`, and plain path completion from a
//! directory listing. Columns are byte offsets. `get_suggestions` answers synchronously; `suggestions_job` lets a
//! provider hand the editor work that may block (the `fd` walk), which the editor runs off the UI thread with an
//! [`AbortSignal`] (TS `getSuggestions(..., { signal })`), dropping answers for text that changed meanwhile.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::collate::locale_compare;
use crate::fuzzy::fuzzy_filter;
use crate::utils::{is_cjk_punctuation, is_whitespace_char};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AutocompleteItem {
    pub value: String,
    pub label: String,
    pub description: Option<String>,
}

impl AutocompleteItem {
    pub fn new(value: &str) -> Self {
        AutocompleteItem {
            value: value.into(),
            label: value.into(),
            description: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AutocompleteSuggestions {
    pub items: Vec<AutocompleteItem>,
    /// What the items replace, e.g. `/mo` or `src/`.
    pub prefix: String,
}

/// Lines and cursor after applying a completion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Completion {
    pub lines: Vec<String>,
    pub cursor_line: usize,
    pub cursor_col: usize,
}

/// TS `AbortSignal` for one autocomplete request: aborting it SIGKILLs the `fd` child the request is waiting on
/// (only that child, which this process spawned and has not reaped yet).
#[derive(Debug, Default)]
pub struct AbortSignal {
    aborted: AtomicBool,
    child: Mutex<Option<Child>>,
}

impl AbortSignal {
    pub fn abort(&self) {
        self.aborted.store(true, Ordering::SeqCst);
        let mut child = self.child.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(child) = child.as_mut() {
            // Unreaped, so the pid is still this child's (TS `child.kill("SIGKILL")`).
            let _ = child.kill();
        }
    }

    pub fn is_aborted(&self) -> bool {
        self.aborted.load(Ordering::SeqCst)
    }
}

/// Work that answers a request off the UI thread.
pub type BackgroundSuggestions = Box<dyn FnOnce(&AbortSignal) -> Option<AutocompleteSuggestions> + Send>;

/// How a provider answers a request: at once, or by work the editor runs off the UI thread.
pub enum SuggestionsJob {
    Ready(Option<AutocompleteSuggestions>),
    Background(BackgroundSuggestions),
}

pub trait AutocompleteProvider {
    /// Characters that trigger this provider at token boundaries.
    fn trigger_characters(&self) -> Vec<String> {
        Vec::new()
    }
    /// The answer, computed on the calling thread (blocking for `@` file search).
    fn get_suggestions(
        &self,
        lines: &[String],
        line: usize,
        col: usize,
        force: bool,
    ) -> Option<AutocompleteSuggestions>;
    /// What the editor calls: providers whose answer may block return [`SuggestionsJob::Background`].
    fn suggestions_job(&self, lines: &[String], line: usize, col: usize, force: bool) -> SuggestionsJob {
        SuggestionsJob::Ready(self.get_suggestions(lines, line, col, force))
    }
    fn apply_completion(
        &self,
        lines: &[String],
        line: usize,
        col: usize,
        item: &AutocompleteItem,
        prefix: &str,
    ) -> Completion {
        apply_completion(lines, line, col, item, prefix)
    }
    /// Whether an explicit Tab should start file completion.
    fn should_trigger_file_completion(&self, _lines: &[String], _line: usize, _col: usize) -> bool {
        true
    }
}

/// Completions for a command's argument text; `None` when the command has none.
pub type ArgumentCompletions = Box<dyn Fn(&str) -> Option<Vec<AutocompleteItem>>>;

/// A slash command (TS `SlashCommand`; a plain `AutocompleteItem` command is one with only a name and description).
pub struct SlashCommand {
    pub name: String,
    pub description: Option<String>,
    pub argument_hint: Option<String>,
    pub argument_completions: Option<ArgumentCompletions>,
}

/// `\s` or CJK punctuation (TS `autocompleteSeparatorRegex`).
pub fn is_autocomplete_separator(c: char) -> bool {
    is_whitespace_char(c) || is_cjk_punctuation(c)
}

fn is_path_delimiter(c: char) -> bool {
    matches!(c, ' ' | '\t' | '"' | '\'' | '=')
}

/// Byte index of the last path delimiter or separator.
fn find_last_delimiter(text: &str) -> Option<usize> {
    text.char_indices()
        .rev()
        .filter(|&(_, c)| is_path_delimiter(c) || is_autocomplete_separator(c))
        .map(|(i, _)| i)
        .next()
}

fn find_unclosed_quote_start(text: &str) -> Option<usize> {
    let mut start = None;
    let mut open = false;
    for (i, b) in text.bytes().enumerate() {
        if b == b'"' {
            open = !open;
            if open {
                start = Some(i);
            }
        }
    }
    if open { start } else { None }
}

fn is_token_start(text: &str, index: usize) -> bool {
    text[..index]
        .chars()
        .next_back()
        .is_none_or(|c| is_path_delimiter(c) || is_autocomplete_separator(c))
}

fn extract_quoted_prefix(text: &str) -> Option<&str> {
    let q = find_unclosed_quote_start(text)?;
    if q > 0 && text.as_bytes()[q - 1] == b'@' {
        return is_token_start(text, q - 1).then(|| &text[q - 1..]);
    }
    is_token_start(text, q).then(|| &text[q..])
}

/// (raw prefix, is `@`, is quoted).
fn parse_path_prefix(prefix: &str) -> (&str, bool, bool) {
    if let Some(rest) = prefix.strip_prefix("@\"") {
        (rest, true, true)
    } else if let Some(rest) = prefix.strip_prefix('"') {
        (rest, false, true)
    } else if let Some(rest) = prefix.strip_prefix('@') {
        (rest, true, false)
    } else {
        (prefix, false, false)
    }
}

fn build_completion_value(path: &str, is_at: bool, is_quoted: bool) -> String {
    let at = if is_at { "@" } else { "" };
    if is_quoted || path.chars().any(is_autocomplete_separator) {
        format!("{at}\"{path}\"")
    } else {
        format!("{at}{path}")
    }
}

/// Node `path.normalize` (POSIX).
fn normalize(path: &str) -> String {
    let absolute = path.starts_with('/');
    let trailing = path.ends_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|p| *p != "..") {
                    parts.pop();
                } else if !absolute {
                    parts.push("..");
                }
            }
            s => parts.push(s),
        }
    }
    let mut out = parts.join("/");
    if out.is_empty() && !absolute {
        out = ".".into();
    }
    if trailing && !out.is_empty() {
        out.push('/');
    }
    if absolute { format!("/{out}") } else { out }
}

/// Node `path.join` (POSIX) of two parts.
fn join(a: &str, b: &str) -> String {
    let joined = [a, b]
        .iter()
        .filter(|s| !s.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join("/");
    if joined.is_empty() {
        return ".".into();
    }
    normalize(&joined)
}

/// Node `path.dirname` (POSIX).
fn dirname(path: &str) -> String {
    let b = path.as_bytes();
    if b.is_empty() {
        return ".".into();
    }
    let has_root = b[0] == b'/';
    let mut end = None;
    let mut matched_slash = true;
    for i in (1..b.len()).rev() {
        if b[i] == b'/' {
            if !matched_slash {
                end = Some(i);
                break;
            }
        } else {
            matched_slash = false;
        }
    }
    match end {
        None => (if has_root { "/" } else { "." }).into(),
        Some(1) if has_root => "//".into(),
        Some(e) => path[..e].into(),
    }
}

/// Node `path.basename` (POSIX).
fn basename(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "";
    }
    trimmed.rsplit('/').next().unwrap_or(trimmed)
}

fn home_dir() -> String {
    std::env::var("HOME").unwrap_or_default()
}

fn expand_home(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        let expanded = join(&home_dir(), rest);
        if path.ends_with('/') && !expanded.ends_with('/') {
            return format!("{expanded}/");
        }
        return expanded;
    }
    if path == "~" {
        return home_dir();
    }
    path.into()
}

fn regex_escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if ".*+?^${}()|[]\\".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn build_fd_path_query(query: &str) -> String {
    if !query.contains('/') {
        return query.into();
    }
    let trimmed = query.trim_matches('/');
    if trimmed.is_empty() {
        return query.into();
    }
    let sep = "[\\\\/]";
    let mut pattern = trimmed
        .split('/')
        .filter(|s| !s.is_empty())
        .map(regex_escape)
        .collect::<Vec<_>>()
        .join(sep);
    if query.ends_with('/') {
        pattern.push_str(sep);
    }
    pattern
}

/// One `fd` result: the path as printed (directories end in `/`).
#[derive(Clone, Debug)]
struct FdEntry {
    path: String,
    is_dir: bool,
}

/// `walkDirectoryWithFd`: `fd` over `base_dir` (hidden files, symlinks followed, `.git` excluded).
fn walk_with_fd(
    base_dir: &Path,
    fd: &Path,
    query: &str,
    max_results: usize,
    max_depth: Option<usize>,
    signal: &AbortSignal,
) -> Vec<FdEntry> {
    if signal.is_aborted() {
        return Vec::new();
    }
    let mut cmd = Command::new(fd);
    cmd.arg("--base-directory")
        .arg(base_dir)
        .args(["--max-results", &max_results.to_string()])
        .args(["--type", "f", "--type", "d", "--follow", "--hidden"])
        .args(["--exclude", ".git", "--exclude", ".git/*", "--exclude", ".git/**"]);
    if let Some(depth) = max_depth {
        cmd.args(["--max-depth", &depth.to_string()]);
    }
    if query.contains('/') {
        cmd.arg("--full-path");
    }
    if !query.is_empty() {
        cmd.arg(build_fd_path_query(query));
    }
    let Ok(mut child) = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return Vec::new();
    };
    let Some(mut pipe) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Vec::new();
    };
    {
        let mut slot = signal.child.lock().unwrap_or_else(|e| e.into_inner());
        if signal.is_aborted() {
            let _ = child.kill();
            let _ = child.wait();
            return Vec::new();
        }
        *slot = Some(child);
    }
    let mut bytes = Vec::new();
    let _ = pipe.read_to_end(&mut bytes);
    let child = signal.child.lock().unwrap_or_else(|e| e.into_inner()).take();
    let success = child.and_then(|mut c| c.wait().ok()).is_some_and(|s| s.success());
    if signal.is_aborted() || !success || bytes.is_empty() {
        return Vec::new();
    }
    let stdout = String::from_utf8_lossy(&bytes);
    stdout
        .trim_matches(is_whitespace_char)
        .split('\n')
        .filter(|l| !l.is_empty())
        .filter_map(|line| {
            let is_dir = line.ends_with('/');
            let p = line.strip_suffix('/').unwrap_or(line);
            if p == ".git" || p.starts_with(".git/") || p.contains("/.git/") {
                return None;
            }
            Some(FdEntry {
                path: line.into(),
                is_dir,
            })
        })
        .collect()
}

fn score_entry(path: &str, query: &str, is_dir: bool) -> u32 {
    let name = basename(path).to_lowercase();
    let q = query.to_lowercase();
    let mut score = if name == q {
        100
    } else if name.starts_with(&q) {
        80
    } else if name.contains(&q) {
        50
    } else if path.to_lowercase().contains(&q) {
        30
    } else {
        0
    };
    if is_dir && score > 0 {
        score += 10;
    }
    score
}

/// Slash commands, `@` fuzzy file search and path completion.
pub struct CombinedAutocompleteProvider {
    pub commands: Vec<SlashCommand>,
    pub base_path: PathBuf,
    /// The `fd` binary; without it `@` completion finds nothing.
    pub fd_path: Option<PathBuf>,
}

impl CombinedAutocompleteProvider {
    pub fn new(commands: Vec<SlashCommand>, base_path: impl Into<PathBuf>, fd_path: Option<PathBuf>) -> Self {
        CombinedAutocompleteProvider {
            commands,
            base_path: base_path.into(),
            fd_path,
        }
    }

    fn slash_suggestions(&self, text: &str) -> Option<AutocompleteSuggestions> {
        let Some(space) = text.find(' ') else {
            let prefix = &text[1..];
            let items: Vec<AutocompleteItem> = self
                .commands
                .iter()
                .map(|c| {
                    let desc = c.description.clone().unwrap_or_default();
                    let full = match &c.argument_hint {
                        Some(h) if !h.is_empty() && !desc.is_empty() => format!("{h} — {desc}"),
                        Some(h) if !h.is_empty() => h.clone(),
                        _ => desc,
                    };
                    AutocompleteItem {
                        value: c.name.clone(),
                        label: c.name.clone(),
                        description: (!full.is_empty()).then_some(full),
                    }
                })
                .collect();
            let filtered = fuzzy_filter(&items, prefix, |i| match i.value.strip_prefix("skill:") {
                Some(rest) if !prefix.starts_with("skill:") => rest.to_string(),
                _ => i.value.clone(),
            });
            return (!filtered.is_empty()).then(|| AutocompleteSuggestions {
                items: filtered,
                prefix: text.to_string(),
            });
        };
        let (name, argument) = (&text[1..space], &text[space + 1..]);
        let complete = self
            .commands
            .iter()
            .find(|c| c.name == name)?
            .argument_completions
            .as_ref()?;
        let items = complete(argument).filter(|items| !items.is_empty())?;
        Some(AutocompleteSuggestions {
            items,
            prefix: argument.to_string(),
        })
    }

    fn extract_at_prefix<'a>(&self, text: &'a str) -> Option<&'a str> {
        if let Some(q) = extract_quoted_prefix(text)
            && q.starts_with("@\"")
        {
            return Some(q);
        }
        let start = find_last_delimiter(text).map_or(0, |i| i + text[i..].chars().next().map_or(1, char::len_utf8));
        text[start..].starts_with('@').then(|| &text[start..])
    }

    fn extract_path_prefix<'a>(&self, text: &'a str, force: bool) -> Option<&'a str> {
        if let Some(q) = extract_quoted_prefix(text) {
            return Some(q);
        }
        let start = find_last_delimiter(text).map_or(0, |i| i + text[i..].chars().next().map_or(1, char::len_utf8));
        let prefix = &text[start..];
        if force || prefix.contains('/') || prefix.starts_with('.') || prefix.starts_with("~/") {
            return Some(prefix);
        }
        let ends_at_separator = text.chars().next_back().is_some_and(is_autocomplete_separator);
        (prefix.is_empty() && ends_at_separator).then_some(prefix)
    }

    fn resolve_dir(&self, raw: &str, expanded: &str) -> PathBuf {
        if raw.starts_with('~') || expanded.starts_with('/') {
            PathBuf::from(expanded)
        } else {
            PathBuf::from(join(&self.base_path.display().to_string(), expanded))
        }
    }

    /// `getFileSuggestions`: entries of the prefix's directory that start with its last segment.
    fn file_suggestions(&self, prefix: &str) -> Vec<AutocompleteItem> {
        let (raw, is_at, is_quoted) = parse_path_prefix(prefix);
        let expanded = if raw.starts_with('~') {
            expand_home(raw)
        } else {
            raw.to_string()
        };
        let is_root = matches!(raw, "" | "./" | "../" | "~" | "~/" | "/");
        let (search_dir, search_prefix) = if is_root || raw.ends_with('/') {
            (self.resolve_dir(raw, &expanded), String::new())
        } else {
            let dir = dirname(&expanded);
            let file = basename(&expanded).to_string();
            let search_dir = if raw.starts_with('~') || expanded.starts_with('/') {
                PathBuf::from(dir)
            } else {
                PathBuf::from(join(&self.base_path.display().to_string(), &dir))
            };
            (search_dir, file)
        };
        let Ok(entries) = std::fs::read_dir(&search_dir) else {
            return Vec::new();
        };
        let lower_prefix = search_prefix.to_lowercase();
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if !name.to_lowercase().starts_with(&lower_prefix) {
                continue;
            }
            let mut is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            if !is_dir && entry.file_type().is_ok_and(|t| t.is_symlink()) {
                is_dir = std::fs::metadata(search_dir.join(&name)).is_ok_and(|m| m.is_dir());
            }
            let relative = if raw.ends_with('/') {
                format!("{raw}{name}")
            } else if raw.contains('/') || raw.contains('\\') {
                if let Some(home_rel) = raw.strip_prefix("~/") {
                    let dir = dirname(home_rel);
                    if dir == "." {
                        format!("~/{name}")
                    } else {
                        format!("~/{}", join(&dir, &name))
                    }
                } else if raw.starts_with('/') {
                    let dir = dirname(raw);
                    if dir == "/" {
                        format!("/{name}")
                    } else {
                        format!("{dir}/{name}")
                    }
                } else {
                    let joined = join(&dirname(raw), &name);
                    if raw.starts_with("./") && !joined.starts_with("./") {
                        format!("./{joined}")
                    } else {
                        joined
                    }
                }
            } else if raw.starts_with('~') {
                format!("~/{name}")
            } else {
                name.clone()
            };
            let relative = relative.replace('\\', "/");
            let path_value = if is_dir { format!("{relative}/") } else { relative };
            out.push(AutocompleteItem {
                value: build_completion_value(&path_value, is_at, is_quoted),
                label: if is_dir { format!("{name}/") } else { name },
                description: None,
            });
        }
        out.sort_by(|a, b| {
            let (ad, bd) = (a.label.ends_with('/'), b.label.ends_with('/'));
            bd.cmp(&ad).then_with(|| locale_compare(&a.label, &b.label))
        });
        out
    }
}

/// `resolveScopedFuzzyQuery`: `dir/que` searches `que` inside `dir` when it is a directory.
fn resolve_scoped_query(base_path: &Path, raw: &str) -> Option<(PathBuf, String, String)> {
    let normalized = raw.replace('\\', "/");
    let slash = normalized.rfind('/')?;
    let display_base = normalized[..=slash].to_string();
    let query = normalized[slash + 1..].to_string();
    let base_dir = if display_base.starts_with("~/") {
        PathBuf::from(expand_home(&display_base))
    } else if display_base.starts_with('/') {
        PathBuf::from(&display_base)
    } else {
        PathBuf::from(join(&base_path.display().to_string(), &display_base))
    };
    std::fs::metadata(&base_dir)
        .ok()?
        .is_dir()
        .then_some((base_dir, query, display_base))
}

/// `getFuzzyFileSuggestions`: depth-1 then recursive `fd` results, scored, best 20.
fn fuzzy_file_suggestions(
    base_path: &Path,
    fd: &Path,
    query: &str,
    is_quoted: bool,
    signal: &AbortSignal,
) -> Vec<AutocompleteItem> {
    if signal.is_aborted() {
        return Vec::new();
    }
    let scoped = resolve_scoped_query(base_path, query);
    let (base, fd_query) = match &scoped {
        Some((dir, q, _)) => (dir.clone(), q.clone()),
        None => (base_path.to_path_buf(), query.to_string()),
    };
    let mut entries = walk_with_fd(&base, fd, &fd_query, 100, Some(1), signal);
    let recursive = walk_with_fd(&base, fd, &fd_query, 100, None, signal);
    if signal.is_aborted() {
        return Vec::new();
    }
    for e in recursive {
        if !entries.iter().any(|s| s.path == e.path) {
            entries.push(e);
        }
    }
    let mut scored: Vec<(u32, FdEntry)> = entries
        .into_iter()
        .map(|e| {
            let score = if fd_query.is_empty() {
                1
            } else {
                score_entry(&e.path, &fd_query, e.is_dir)
            };
            (score, e)
        })
        .filter(|(s, _)| *s > 0)
        .collect();
    let depth = |p: &str| p.split('/').filter(|s| !s.is_empty()).count();
    let len16 = |p: &str| p.encode_utf16().count();
    scored.sort_by(|(sa, a), (sb, b)| {
        sb.cmp(sa)
            .then_with(|| depth(&a.path).cmp(&depth(&b.path)))
            .then_with(|| len16(&a.path).cmp(&len16(&b.path)))
            .then_with(|| locale_compare(&a.path, &b.path))
    });
    scored
        .into_iter()
        .take(20)
        .map(|(_, e)| {
            let without_slash = if e.is_dir {
                &e.path[..e.path.len() - 1]
            } else {
                &e.path[..]
            };
            let display = match &scoped {
                Some((_, _, base)) if base == "/" => format!("/{without_slash}"),
                Some((_, _, base)) => format!("{base}{without_slash}"),
                None => without_slash.to_string(),
            };
            let name = basename(without_slash);
            let completion = if e.is_dir {
                format!("{display}/")
            } else {
                display.clone()
            };
            AutocompleteItem {
                value: build_completion_value(&completion, true, is_quoted),
                label: if e.is_dir { format!("{name}/") } else { name.to_string() },
                description: Some(display),
            }
        })
        .collect()
}

impl AutocompleteProvider for CombinedAutocompleteProvider {
    fn get_suggestions(
        &self,
        lines: &[String],
        line: usize,
        col: usize,
        force: bool,
    ) -> Option<AutocompleteSuggestions> {
        match self.suggestions_job(lines, line, col, force) {
            SuggestionsJob::Ready(answer) => answer,
            SuggestionsJob::Background(job) => job(&AbortSignal::default()),
        }
    }

    /// `@` file search runs `fd`, so it goes off the UI thread; slash commands and directory listings answer at once.
    fn suggestions_job(&self, lines: &[String], line: usize, col: usize, force: bool) -> SuggestionsJob {
        let text = lines.get(line).map_or("", |l| &l[..col.min(l.len())]);
        if let Some(at) = self.extract_at_prefix(text) {
            let Some(fd) = self.fd_path.clone() else {
                return SuggestionsJob::Ready(None);
            };
            let (raw, _, is_quoted) = parse_path_prefix(at);
            let (raw, prefix, base) = (raw.to_string(), at.to_string(), self.base_path.clone());
            return SuggestionsJob::Background(Box::new(move |signal: &AbortSignal| {
                let items = fuzzy_file_suggestions(&base, &fd, &raw, is_quoted, signal);
                (!items.is_empty()).then_some(AutocompleteSuggestions { items, prefix })
            }));
        }
        if !force && text.starts_with('/') {
            return SuggestionsJob::Ready(self.slash_suggestions(text));
        }
        let Some(prefix) = self.extract_path_prefix(text, force) else {
            return SuggestionsJob::Ready(None);
        };
        let items = self.file_suggestions(prefix);
        SuggestionsJob::Ready((!items.is_empty()).then(|| AutocompleteSuggestions {
            items,
            prefix: prefix.to_string(),
        }))
    }

    fn should_trigger_file_completion(&self, lines: &[String], line: usize, col: usize) -> bool {
        let text = lines.get(line).map_or("", |l| &l[..col.min(l.len())]);
        let trimmed = text.trim_matches(is_whitespace_char);
        !(trimmed.starts_with('/') && !trimmed.contains(' '))
    }
}

/// `CombinedAutocompleteProvider.applyCompletion`: replace `prefix` before the cursor with the item.
pub fn apply_completion(
    lines: &[String],
    line: usize,
    col: usize,
    item: &AutocompleteItem,
    prefix: &str,
) -> Completion {
    let current = lines.get(line).map_or("", String::as_str);
    let before = &current[..col.saturating_sub(prefix.len())];
    let after = &current[col..];
    let quoted = prefix.starts_with('"') || prefix.starts_with("@\"");
    let value = item.value.as_str();
    let after = match after.strip_prefix('"') {
        Some(rest) if quoted && value.ends_with('"') => rest,
        _ => after,
    };
    let is_dir = item.label.ends_with('/');
    // Directories with a closing quote keep the cursor inside the quotes.
    let offset = if is_dir && value.ends_with('"') {
        value.len() - 1
    } else {
        value.len()
    };
    let is_slash_command =
        prefix.starts_with('/') && before.trim_matches(is_whitespace_char).is_empty() && !prefix[1..].contains('/');
    let (new_line, cursor_col) = if is_slash_command {
        (format!("{before}/{value} {after}"), before.len() + value.len() + 2)
    } else if prefix.starts_with('@') {
        let suffix = if is_dir { "" } else { " " };
        (
            format!("{before}{value}{suffix}{after}"),
            before.len() + offset + suffix.len(),
        )
    } else {
        (format!("{before}{value}{after}"), before.len() + offset)
    };
    let mut new_lines = lines.to_vec();
    if let Some(l) = new_lines.get_mut(line) {
        *l = new_line;
    }
    Completion {
        lines: new_lines,
        cursor_line: line,
        cursor_col,
    }
}
