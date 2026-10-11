//! Node `path` semantics (POSIX) and the coding agent's `normalizePath`/`resolvePath`.

/// `os.homedir()`.
pub fn home_dir() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/".to_string())
}

fn normalize_string(path: &str, allow_above_root: bool) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|p| *p != "..") {
                    parts.pop();
                } else if allow_above_root {
                    parts.push("..");
                }
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

/// `path.normalize`.
pub fn normalize(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let absolute = path.starts_with('/');
    let trailing = path.ends_with('/');
    let mut s = normalize_string(path, !absolute);
    if s.is_empty() {
        if absolute {
            return "/".to_string();
        }
        return if trailing { "./" } else { "." }.to_string();
    }
    if trailing {
        s.push('/');
    }
    if absolute { format!("/{s}") } else { s }
}

/// `path.join`.
pub fn join(parts: &[&str]) -> String {
    let joined: Vec<&str> = parts.iter().copied().filter(|p| !p.is_empty()).collect();
    if joined.is_empty() {
        return ".".to_string();
    }
    normalize(&joined.join("/"))
}

/// `path.resolve(base, path)` with `base` absolute.
fn resolve_from(base: &str, path: &str) -> String {
    let full = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("{base}/{path}")
    };
    format!("/{}", normalize_string(&full, false))
}

fn process_cwd() -> String {
    std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "/".to_string())
}

/// `path.resolve(path, "..")`: the parent directory of a resolved path.
pub fn parent(path: &str) -> String {
    resolve_from(&resolve_from(&process_cwd(), path), "..")
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%'
            && let (Some(h), Some(l)) = (
                bytes.get(i + 1).and_then(|b| hex(*b)),
                bytes.get(i + 2).and_then(|b| hex(*b)),
            )
        {
            out.push((h * 16 + l) as u8);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `normalizePath`: expands a leading `~` and converts `file://` URLs to paths.
pub fn normalize_path(input: &str) -> String {
    if input == "~" {
        return home_dir();
    }
    if let Some(rest) = input.strip_prefix("~/") {
        return join(&[&home_dir(), rest]);
    }
    if let Some(rest) = input.strip_prefix("file://") {
        // fileURLToPath: only an empty or "localhost" host is a local path.
        let path = rest.strip_prefix("localhost").unwrap_or(rest);
        return percent_decode(path.split(['?', '#']).next().unwrap_or(path));
    }
    input.to_string()
}

/// `resolvePath`: `normalizePath`, then `path.resolve` against the process cwd.
pub fn resolve_path(input: &str) -> String {
    resolve_from(&process_cwd(), &normalize_path(input))
}
