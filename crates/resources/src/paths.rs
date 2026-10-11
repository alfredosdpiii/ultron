//! Path helpers with Node `path` semantics on POSIX (string in, string out, lexical).

use std::path::Path;

/// Node `path.resolve(base, input)`: absolute, `.`/`..` removed, no trailing slash.
pub fn resolve(base: &str, input: &str) -> String {
    let joined = if input.starts_with('/') {
        input.to_string()
    } else {
        format!("{base}/{input}")
    };
    let mut parts: Vec<&str> = Vec::new();
    for part in joined.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            _ => parts.push(part),
        }
    }
    format!("/{}", parts.join("/"))
}

/// `resolvePath` from utils/paths.ts: `~` expansion against `$HOME`, then `resolve`.
pub fn resolve_path(input: &str, base: &str, trim: bool) -> String {
    let input = if trim { input.trim() } else { input };
    let home = std::env::var("HOME").unwrap_or_default();
    if input == "~" {
        return resolve("/", &home);
    }
    if let Some(rest) = input.strip_prefix("~/") {
        return resolve(&home, rest);
    }
    resolve(base, input)
}

/// Node `path.dirname` for an absolute, normalized path.
pub fn dirname(path: &str) -> String {
    match path.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(i) => path[..i].to_string(),
    }
}

/// Node `path.basename`.
pub fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Node `path.join(dir, name)` for a normalized dir and a plain name.
pub fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// `canonicalizePath`: realpath, or the input when it cannot be resolved.
pub fn canonicalize(path: &str) -> String {
    std::fs::canonicalize(Path::new(path))
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string())
}

/// Node `path.relative(from, to)` for absolute, normalized paths.
pub fn relative(from: &str, to: &str) -> String {
    let a: Vec<&str> = from.split('/').filter(|s| !s.is_empty()).collect();
    let b: Vec<&str> = to.split('/').filter(|s| !s.is_empty()).collect();
    let common = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let mut out: Vec<&str> = vec![".."; a.len() - common];
    out.extend(&b[common..]);
    out.join("/")
}

/// Strip a leading UTF-8 byte order mark.
pub fn strip_bom(text: &str) -> &str {
    text.strip_prefix('\u{feff}').unwrap_or(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resources_paths_follow_node_semantics() {
        assert_eq!(resolve("/a/b", "../c/./d/"), "/a/c/d");
        assert_eq!(resolve("/a", "/x/../y"), "/y");
        assert_eq!(dirname("/a/b/c"), "/a/b");
        assert_eq!(dirname("/a"), "/");
        assert_eq!(relative("/a/b", "/a/c/d"), "../c/d");
        assert_eq!(relative("/a/b", "/a/b"), "");
        assert_eq!(basename("/a/b/SKILL.md"), "SKILL.md");
    }
}
