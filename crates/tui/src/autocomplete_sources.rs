//! Extra autocomplete sources (ports of omp packages/tui/src/prompt/emoji-autocomplete.ts,
//! github-ref-autocomplete.ts and model-mention-autocomplete.ts).
//!
//! - Emoji: `:name` suggestions.
//!   The shortcode table `data/emojis.json` is omp's (built from gemoji), loaded once on first use.
//! - GitHub `#123` references: local PR/issue candidates; titles come from a [`RefTitleSource`] whose network
//!   lookups run on its own thread, so suggestions never wait on `gh`.
//! - Model mentions: `@model` (trigger configurable; omp uses `^`) ranked by fuzzy match over ids and names.
//!
//! [`ChainProvider`] stacks providers: the first one with suggestions wins, so `@cla` offers models while `@src/`
//! falls through to file completion.

use std::collections::{HashMap, HashSet};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

use crate::autocomplete::{
    AbortSignal, AutocompleteItem, AutocompleteProvider, AutocompleteSuggestions, Completion, SuggestionsJob,
};
use crate::fuzzy::fuzzy_filter;

// ---- emoji ----

type Buckets = HashMap<String, Vec<(String, String)>>;

fn buckets() -> &'static Buckets {
    static B: OnceLock<Buckets> = OnceLock::new();
    B.get_or_init(|| serde_json::from_str(include_str!("data/emojis.json")).unwrap_or_default())
}

/// Longest first, so `:-)` wins over `:)`.
const EMOTICONS: &[(&str, &str)] = &[
    (":'-(", "😢"),
    (">:-(", "😠"),
    (":-)", "🙂"),
    (":-(", "🙁"),
    (":-D", "😃"),
    (":-P", "😛"),
    (":-p", "😛"),
    (":-O", "😮"),
    (":-o", "😮"),
    (":-|", "😐"),
    (":-/", "😕"),
    (":-\\", "😕"),
    (":-*", "😘"),
    (";-)", "😉"),
    (";-P", "😜"),
    (":')", "🥲"),
    (":'D", "😂"),
    (":'(", "😢"),
    ("</3", "💔"),
    (">:(", "😠"),
    ("B-)", "😎"),
    ("8-)", "😎"),
    ("o.O", "😳"),
    ("O.o", "😳"),
    (":)", "🙂"),
    (":(", "🙁"),
    (":D", "😃"),
    (":P", "😛"),
    (":p", "😛"),
    (":O", "😮"),
    (":o", "😮"),
    (":|", "😐"),
    (":/", "😕"),
    (":\\", "😕"),
    (":*", "😘"),
    (";)", "😉"),
    (":3", "😺"),
    ("<3", "❤️"),
    ("xD", "😆"),
    ("XD", "😆"),
    ("B)", "😎"),
    ("8)", "😎"),
];

const MAX_EMOJI_SUGGESTIONS: usize = 12;

fn is_name_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'_' | b'+' | b'-')
}

fn has_left_boundary(text: &str, idx: usize) -> bool {
    idx == 0
        || matches!(
            text.as_bytes()[idx - 1],
            b' ' | b'\t' | b'\n' | b'\r' | b'(' | b'[' | b'{' | b'>'
        )
}

/// `:name` suggestions for the text before the cursor (emoticons first, then shortcodes), at most 12.
pub fn emoji_suggestions(text_before: &str) -> Option<AutocompleteSuggestions> {
    let b = text_before.as_bytes();
    let mut i = b.len();
    while i > 0 && is_name_byte(b[i - 1]) {
        i -= 1;
    }
    if i == 0 || b[i - 1] != b':' || !has_left_boundary(text_before, i - 1) {
        return None;
    }
    let name = &text_before[i..];
    let query = name.to_lowercase();
    if query.is_empty() {
        return None;
    }
    let wanted = format!(":{query}");
    let mut items = Vec::new();
    for (pattern, ch) in EMOTICONS {
        if items.len() >= MAX_EMOJI_SUGGESTIONS {
            break;
        }
        if pattern.len() >= wanted.len() && pattern.to_lowercase()[..wanted.len()] == wanted {
            items.push(AutocompleteItem {
                value: (*ch).to_string(),
                label: format!("{ch}  {pattern}"),
                description: None,
            });
        }
    }
    if let Some(bucket) = buckets().get(&query[..1]) {
        let start = bucket.partition_point(|(n, _)| n.as_str() < query.as_str());
        for (n, ch) in &bucket[start..] {
            if items.len() >= MAX_EMOJI_SUGGESTIONS || !n.starts_with(&query) {
                break;
            }
            items.push(AutocompleteItem {
                value: ch.clone(),
                label: format!("{ch}  :{n}:"),
                description: None,
            });
        }
    }
    (!items.is_empty()).then(|| AutocompleteSuggestions {
        items,
        prefix: format!(":{name}"),
    })
}

fn line_before(lines: &[String], line: usize, col: usize) -> &str {
    lines.get(line).map_or("", |l| &l[..col.min(l.len())])
}

fn replace_prefix(lines: &[String], line: usize, col: usize, prefix_start: usize, insert: &str) -> Completion {
    let current = lines.get(line).map_or("", String::as_str);
    let mut new_lines = lines.to_vec();
    if let Some(l) = new_lines.get_mut(line) {
        *l = format!("{}{insert}{}", &current[..prefix_start], &current[col..]);
    }
    Completion {
        lines: new_lines,
        cursor_line: line,
        cursor_col: prefix_start + insert.len(),
    }
}

/// `:` emoji completion.
pub struct EmojiProvider;

impl AutocompleteProvider for EmojiProvider {
    fn trigger_characters(&self) -> Vec<String> {
        vec![":".into()]
    }
    fn get_suggestions(
        &self,
        lines: &[String],
        line: usize,
        col: usize,
        _force: bool,
    ) -> Option<AutocompleteSuggestions> {
        emoji_suggestions(line_before(lines, line, col))
    }
    fn apply_completion(
        &self,
        lines: &[String],
        line: usize,
        col: usize,
        item: &AutocompleteItem,
        prefix: &str,
    ) -> Completion {
        replace_prefix(lines, line, col, col.saturating_sub(prefix.len()), &item.value)
    }
}

// ---- GitHub references ----

/// Which kind a `#123` reference names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RefKind {
    Pr,
    Issue,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GithubRefContext {
    /// Text replaced on accept: `#3164`, or `pr #3164`.
    pub prefix: String,
    pub qualifier: Option<RefKind>,
    pub number: String,
}

/// A standalone `#N` token (optionally `pr`/`pull`/`issue` before it) ending at the cursor.
pub fn github_ref_context(text_before: &str) -> Option<GithubRefContext> {
    let digits = text_before.len() - text_before.trim_end_matches(|c: char| c.is_ascii_digit()).len();
    let num_start = text_before.len() - digits;
    let number = &text_before[num_start..];
    if digits == 0 || number.starts_with('0') || !text_before[..num_start].ends_with('#') {
        return None;
    }
    let hash = num_start - 1;
    let boundary = |i: usize| {
        i == 0
            || text_before[..i]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_whitespace() || "\"'`(<=".contains(c))
    };
    let before = &text_before[..hash];
    let ws = before.len() - before.trim_end().len();
    if ws > 0 {
        let word_end = hash - ws;
        let word_start = before[..word_end]
            .char_indices()
            .rev()
            .take_while(|(_, c)| c.is_ascii_alphabetic())
            .last()
            .map_or(word_end, |(i, _)| i);
        let word = &before[word_start..word_end];
        let qualifier = match word.to_ascii_lowercase().as_str() {
            "pr" | "pull" => Some(RefKind::Pr),
            "issue" => Some(RefKind::Issue),
            _ => None,
        };
        if let Some(q) = qualifier.filter(|_| boundary(word_start)) {
            return Some(GithubRefContext {
                prefix: text_before[word_start..].to_string(),
                qualifier: Some(q),
                number: number.to_string(),
            });
        }
    }
    boundary(hash).then(|| GithubRefContext {
        prefix: format!("#{number}"),
        qualifier: None,
        number: number.to_string(),
    })
}

/// Titles for references, fetched off the paint path.
pub trait RefTitleSource {
    /// The cached title, if any; never blocks.
    fn title(&self, kind: RefKind, number: u64) -> Option<String>;
    /// Ask for a title in the background (deduplicated by the implementation).
    fn request(&self, kind: RefKind, number: u64);
}

type TitleCache = Arc<Mutex<HashMap<(RefKind, u64), Option<String>>>>;

/// Titles from `gh {pr|issue} view N --json title` on a worker thread.
pub struct GhRefTitles {
    cache: TitleCache,
    asked: Mutex<HashSet<(RefKind, u64)>>,
    tx: Mutex<Sender<(RefKind, u64)>>,
}

impl GhRefTitles {
    /// `lookup` does the network call (the default runs `gh` in `cwd`); `on_update` runs after each answer.
    pub fn spawn(
        lookup: impl Fn(RefKind, u64) -> Option<String> + Send + 'static,
        on_update: impl Fn() + Send + 'static,
    ) -> Self {
        let cache: TitleCache = Arc::default();
        let shared = Arc::clone(&cache);
        let (tx, rx) = mpsc::channel::<(RefKind, u64)>();
        thread::Builder::new()
            .name("gh-ref-titles".into())
            .spawn(move || {
                for (kind, n) in rx {
                    let title = lookup(kind, n);
                    shared
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert((kind, n), title);
                    on_update();
                }
            })
            .ok();
        Self {
            cache,
            asked: Mutex::default(),
            tx: Mutex::new(tx),
        }
    }

    /// `gh` in `cwd`.
    pub fn gh(cwd: std::path::PathBuf, on_update: impl Fn() + Send + 'static) -> Self {
        Self::spawn(
            move |kind, n| {
                let sub = if kind == RefKind::Pr { "pr" } else { "issue" };
                let out = Command::new("gh")
                    .args([sub, "view", &n.to_string(), "--json", "title", "--jq", ".title"])
                    .current_dir(&cwd)
                    .stdin(Stdio::null())
                    .stderr(Stdio::null())
                    .output()
                    .ok()
                    .filter(|o| o.status.success())?;
                let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
                (!t.is_empty()).then_some(t)
            },
            on_update,
        )
    }
}

impl RefTitleSource for GhRefTitles {
    fn title(&self, kind: RefKind, number: u64) -> Option<String> {
        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(kind, number))
            .cloned()
            .flatten()
    }
    fn request(&self, kind: RefKind, number: u64) {
        if self
            .asked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((kind, number))
        {
            let _ = self.tx.lock().unwrap_or_else(|e| e.into_inner()).send((kind, number));
        }
    }
}

/// Suggestions for a `#N` token: PR and Issue candidates (or the named kind), titled when known.
pub fn github_ref_suggestions(
    text_before: &str,
    titles: Option<&dyn RefTitleSource>,
) -> Option<AutocompleteSuggestions> {
    let ctx = github_ref_context(text_before)?;
    let kinds: Vec<RefKind> = match ctx.qualifier {
        Some(k) => vec![k],
        None => vec![RefKind::Pr, RefKind::Issue],
    };
    let n: u64 = ctx.number.parse().ok()?;
    let items = kinds
        .into_iter()
        .map(|kind| {
            let (scheme, label, desc) = match kind {
                RefKind::Pr => ("pr", "PR", "GitHub pull request"),
                RefKind::Issue => ("issue", "Issue", "GitHub issue"),
            };
            let title = titles.and_then(|t| {
                t.request(kind, n);
                t.title(kind, n)
            });
            AutocompleteItem {
                value: format!("{scheme}://{}", ctx.number),
                label: format!("{label} #{}", ctx.number),
                description: Some(title.unwrap_or_else(|| desc.to_string())),
            }
        })
        .collect();
    Some(AutocompleteSuggestions {
        items,
        prefix: ctx.prefix,
    })
}

/// `#N` completion; accepting inserts the reference (`pr://N`) and a space.
pub struct GithubRefProvider {
    pub titles: Option<Box<dyn RefTitleSource>>,
}

impl AutocompleteProvider for GithubRefProvider {
    fn trigger_characters(&self) -> Vec<String> {
        vec!["#".into()]
    }
    fn get_suggestions(
        &self,
        lines: &[String],
        line: usize,
        col: usize,
        _force: bool,
    ) -> Option<AutocompleteSuggestions> {
        github_ref_suggestions(line_before(lines, line, col), self.titles.as_deref())
    }
    fn apply_completion(
        &self,
        lines: &[String],
        line: usize,
        col: usize,
        item: &AutocompleteItem,
        prefix: &str,
    ) -> Completion {
        replace_prefix(
            lines,
            line,
            col,
            col.saturating_sub(prefix.len()),
            &format!("{} ", item.value),
        )
    }
}

// ---- model mentions ----

/// A model offered for mention.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MentionModel {
    /// Canonical selector, e.g. `anthropic/claude-sonnet-4-5`.
    pub selector: String,
    pub name: String,
}

/// The whitespace-delimited mention token ending at the cursor (`@cla`), if any.
pub fn mention_prefix(text_before: &str, trigger: char) -> Option<&str> {
    let start = text_before
        .rfind(char::is_whitespace)
        .map_or(0, |i| i + text_before[i..].chars().next().map_or(1, char::len_utf8));
    let token = &text_before[start..];
    token.starts_with(trigger).then_some(token)
}

const MAX_MODEL_SUGGESTIONS: usize = 20;

/// `@model` completion over a fixed model list. Only answers when a model matches, so other `@` uses fall through.
pub struct ModelMentionProvider {
    pub models: Vec<MentionModel>,
    pub trigger: char,
}

impl AutocompleteProvider for ModelMentionProvider {
    fn trigger_characters(&self) -> Vec<String> {
        vec![self.trigger.to_string()]
    }
    fn get_suggestions(
        &self,
        lines: &[String],
        line: usize,
        col: usize,
        _force: bool,
    ) -> Option<AutocompleteSuggestions> {
        let prefix = mention_prefix(line_before(lines, line, col), self.trigger)?;
        let query = &prefix[self.trigger.len_utf8()..];
        if query.is_empty() {
            return None;
        }
        // Substring hits (selector, then name) rank ahead of fuzzy subsequence hits.
        let q = query.to_lowercase();
        let text = |m: &MentionModel| format!("{} {}", m.selector, m.name);
        let tier = |m: &MentionModel| {
            if m.selector.to_lowercase().contains(&q) {
                0
            } else if m.name.to_lowercase().contains(&q) {
                1
            } else {
                2
            }
        };
        let mut ranked = fuzzy_filter(&self.models, query, text);
        ranked.sort_by_key(tier);
        let items: Vec<AutocompleteItem> = ranked
            .into_iter()
            .take(MAX_MODEL_SUGGESTIONS)
            .map(|m| AutocompleteItem {
                value: m.selector.clone(),
                label: m.selector,
                description: Some(m.name.replace('"', "")),
            })
            .collect();
        (!items.is_empty()).then(|| AutocompleteSuggestions {
            items,
            prefix: prefix.to_string(),
        })
    }
    fn apply_completion(
        &self,
        lines: &[String],
        line: usize,
        col: usize,
        item: &AutocompleteItem,
        prefix: &str,
    ) -> Completion {
        let live = mention_prefix(line_before(lines, line, col), self.trigger).unwrap_or(prefix);
        let start = col.saturating_sub(live.len());
        replace_prefix(lines, line, col, start, &format!("{}{} ", self.trigger, item.value))
    }
}

// ---- chaining ----

/// Providers tried in order; the first with suggestions answers and applies its own completions.
pub struct ChainProvider {
    pub providers: Vec<Box<dyn AutocompleteProvider>>,
    /// Index of the provider that produced the last suggestions.
    last: std::cell::Cell<usize>,
}

impl ChainProvider {
    pub fn new(providers: Vec<Box<dyn AutocompleteProvider>>) -> Self {
        Self {
            providers,
            last: std::cell::Cell::new(0),
        }
    }
}

impl AutocompleteProvider for ChainProvider {
    fn trigger_characters(&self) -> Vec<String> {
        let mut all: Vec<String> = Vec::new();
        for c in self.providers.iter().flat_map(|p| p.trigger_characters()) {
            if !all.contains(&c) {
                all.push(c);
            }
        }
        all
    }
    fn get_suggestions(
        &self,
        lines: &[String],
        line: usize,
        col: usize,
        force: bool,
    ) -> Option<AutocompleteSuggestions> {
        self.providers.iter().enumerate().find_map(|(i, p)| {
            let s = p.get_suggestions(lines, line, col, force)?;
            self.last.set(i);
            Some(s)
        })
    }
    /// The first provider with an answer, as in `get_suggestions`. A background answer from the last provider goes
    /// off the UI thread; one from an earlier provider is waited for here, since a miss falls through to the rest.
    fn suggestions_job(&self, lines: &[String], line: usize, col: usize, force: bool) -> SuggestionsJob {
        let last = self.providers.len().saturating_sub(1);
        for (i, p) in self.providers.iter().enumerate() {
            let answer = match p.suggestions_job(lines, line, col, force) {
                SuggestionsJob::Background(job) if i == last => {
                    self.last.set(i);
                    return SuggestionsJob::Background(job);
                }
                SuggestionsJob::Background(job) => job(&AbortSignal::default()),
                SuggestionsJob::Ready(answer) => answer,
            };
            if let Some(s) = answer {
                self.last.set(i);
                return SuggestionsJob::Ready(Some(s));
            }
        }
        SuggestionsJob::Ready(None)
    }
    fn apply_completion(
        &self,
        lines: &[String],
        line: usize,
        col: usize,
        item: &AutocompleteItem,
        prefix: &str,
    ) -> Completion {
        let i = self.last.get().min(self.providers.len().saturating_sub(1));
        match self.providers.get(i) {
            Some(p) => p.apply_completion(lines, line, col, item, prefix),
            None => crate::autocomplete::apply_completion(lines, line, col, item, prefix),
        }
    }
    fn should_trigger_file_completion(&self, lines: &[String], line: usize, col: usize) -> bool {
        self.providers
            .iter()
            .any(|p| p.should_trigger_file_completion(lines, line, col))
    }
}
