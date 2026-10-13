//! `/changelog` (`utils/changelog.ts`): the package's CHANGELOG.md split into `## [x.y.z]` entries, links pinned to
//! the release tag, oldest entry first.

use tui::markdown::Token;
use tui::marked::markdown_tokens;

const GITHUB_REPO: &str = "earendil-works/pi";
const LINK_BASE: &str = "packages/coding-agent";

/// One `## [x.y.z]` section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub version: (u64, u64, u64),
    pub content: String,
}

/// `##\s+\[?(\d+)\.(\d+)\.(\d+)\]?` at the start of a `## ` line.
fn version_of(line: &str) -> Option<(u64, u64, u64)> {
    let rest = line.strip_prefix("##")?.trim_start();
    let rest = rest.strip_prefix('[').unwrap_or(rest);
    let mut parts = rest.splitn(3, '.');
    let num = |s: &str| -> Option<(u64, usize)> {
        let digits: String = s.chars().take_while(char::is_ascii_digit).collect();
        (!digits.is_empty())
            .then(|| (digits.parse().ok(), digits.len()))
            .and_then(|(n, l)| Some((n?, l)))
    };
    let (major, _) = num(parts.next()?)?;
    let (minor, l) = num(parts.next()?)?;
    let _ = l;
    let (patch, _) = num(parts.next()?)?;
    Some((major, minor, patch))
}

/// A version and the lines of its section so far.
type Section<'a> = ((u64, u64, u64), Vec<&'a str>);

/// `parseChangelog`.
pub fn parse(content: &str) -> Vec<Entry> {
    let mut entries = Vec::new();
    let mut current: Option<Section<'_>> = None;
    for line in content.split('\n') {
        if line.starts_with("## ") {
            if let Some((version, lines)) = current.take()
                && !lines.is_empty()
            {
                entries.push(Entry {
                    version,
                    content: lines.join("\n").trim().to_string(),
                });
            }
            current = version_of(line).map(|v| (v, vec![line]));
        } else if let Some((_, lines)) = current.as_mut() {
            lines.push(line);
        }
    }
    if let Some((version, lines)) = current
        && !lines.is_empty()
    {
        entries.push(Entry {
            version,
            content: lines.join("\n").trim().to_string(),
        });
    }
    entries
}

/// `encodeURI` (UTF-8 percent-encoding outside the URI's reserved and unreserved characters).
fn encode_uri(text: &str) -> String {
    const KEEP: &str = ";,/?:@&=+$-_.!~*'()#";
    let mut out = String::new();
    for c in text.chars() {
        if c.is_ascii_alphanumeric() || KEEP.contains(c) {
            out.push(c);
        } else {
            let mut buf = [0u8; 4];
            for b in c.encode_utf8(&mut buf).bytes() {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
    out
}

/// `path.posix.normalize`.
fn normalize_posix(path: &str) -> String {
    let absolute = path.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|p| *p != "..") {
                    parts.pop();
                } else if !absolute {
                    parts.push("..");
                }
            }
            p => parts.push(p),
        }
    }
    let joined = parts.join("/");
    let trailing = path.ends_with('/') && !joined.is_empty();
    match (absolute, joined.is_empty()) {
        (true, _) => format!("/{joined}{}", if trailing { "/" } else { "" }),
        (false, true) => ".".into(),
        (false, false) => format!("{joined}{}", if trailing { "/" } else { "" }),
    }
}

fn is_scheme(target: &str) -> bool {
    let Some(colon) = target.find(':') else { return false };
    let scheme = &target[..colon];
    let mut chars = scheme.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
}

/// `normalizeChangelogLinkTarget`.
fn link_target(target: &str, tag: &str) -> String {
    let repo = format!("https://github.com/{GITHUB_REPO}");
    let mut canonical = target.to_string();
    for legacy in [
        "https://github.com/badlogic/pi-mono",
        "https://github.com/earendil-works/pi-mono",
    ] {
        if let Some(rest) = canonical.strip_prefix(legacy)
            && (rest.is_empty() || rest.starts_with('/'))
        {
            canonical = format!("{repo}{rest}");
        }
    }
    for route in ["blob", "tree"] {
        for branch in ["main", "master"] {
            let prefix = format!("{repo}/{route}/{branch}/");
            if let Some(rest) = canonical.strip_prefix(&prefix) {
                canonical = format!("{repo}/{route}/{tag}/{rest}");
            }
        }
    }
    if canonical.starts_with('#') || canonical.starts_with("//") || is_scheme(&canonical) {
        return canonical;
    }
    let (before_hash, fragment) = match canonical.find('#') {
        Some(i) => (&canonical[..i], &canonical[i..]),
        None => (canonical.as_str(), ""),
    };
    let (path_part, query) = match before_hash.find('?') {
        Some(i) => (&before_hash[..i], &before_hash[i..]),
        None => (before_hash, ""),
    };
    if path_part.is_empty() {
        return canonical.clone();
    }
    let target_path = path_part.replace('\\', "/");
    let joined = if let Some(abs) = target_path.strip_prefix('/') {
        normalize_posix(abs.trim_start_matches('/'))
    } else {
        normalize_posix(&format!("{LINK_BASE}/{target_path}"))
    };
    if joined == "." || joined == ".." || joined.starts_with("../") {
        return canonical.clone();
    }
    let base = joined.rsplit('/').next().unwrap_or("");
    let route = if path_part.ends_with('/') || !base.contains('.') {
        "tree"
    } else {
        "blob"
    };
    format!("{repo}/{route}/{tag}/{}{query}{fragment}", encode_uri(&joined))
}

/// `normalizeChangelogLinks`: every inline link `[text](target)` (and image) pinned to `v<version>`.
pub fn normalize_links(markdown: &str, version: (u64, u64, u64)) -> String {
    let tag = format!("v{}.{}.{}", version.0, version.1, version.2);
    // `(!?\[[^\]\n]+\]\()([^\s)]+)((?:\s+[^)]*)?\))`
    let re = fancy_regex::Regex::new(r"(!?\[[^\]\n]+\]\()([^\s)]+)((?:\s+[^)]*)?\))").expect("link pattern");
    let mut out = String::new();
    let mut last = 0;
    for caps in re.captures_iter(markdown).flatten() {
        let whole = caps.get(0).expect("match");
        out.push_str(&markdown[last..whole.start()]);
        out.push_str(&caps[1]);
        out.push_str(&link_target(&caps[2], &tag));
        out.push_str(&caps[3]);
        last = whole.end();
    }
    out.push_str(&markdown[last..]);
    out
}

/// What `/changelog` shows: the entries oldest first, links pinned, or Pi's empty notice.
pub fn changelog_markdown(content: Option<&str>) -> String {
    let mut entries = content.map(parse).unwrap_or_default();
    if entries.is_empty() {
        return "No changelog entries found.".into();
    }
    entries.reverse();
    entries
        .iter()
        .map(|e| normalize_links(&e.content, e.version))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The tokens of [`changelog_markdown`], built entry by entry on every core.
///
/// The whole CHANGELOG.md is about 600 KB; lexing it as one document took 4 s in a release build and 46 s in a debug
/// build, so `/changelog` stalled the TUI. Every entry starts with its `## ` heading at the top level and no entry
/// defines reference links, so lexing each entry (with the blank line that joins it to the next) gives the same tokens
/// as lexing the joined document.
pub fn changelog_tokens(content: Option<&str>) -> Vec<Token> {
    let mut entries = content.map(parse).unwrap_or_default();
    if entries.is_empty() {
        return markdown_tokens("No changelog entries found.");
    }
    entries.reverse();
    let last = entries.len() - 1;
    let workers = std::thread::available_parallelism()
        .map_or(1, usize::from)
        .min(entries.len());
    let next = std::sync::atomic::AtomicUsize::new(0);
    let mut parts: Vec<(usize, Vec<Token>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(|| {
                    let mut done = Vec::new();
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(entry) = entries.get(i) else { break };
                        let mut chunk = normalize_links(&entry.content, entry.version);
                        if i < last {
                            chunk.push_str("\n\n");
                        }
                        done.push((i, markdown_tokens(&chunk)));
                    }
                    done
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().expect("changelog lexer thread"))
            .collect()
    });
    parts.sort_by_key(|(i, _)| *i);
    parts.into_iter().flat_map(|(_, tokens)| tokens).collect()
}
