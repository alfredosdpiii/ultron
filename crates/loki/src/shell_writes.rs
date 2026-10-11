//! `shell-writes`: refuse a shell command that writes source or configuration inside the project, since such a write
//! skips the before-write check (loki.py `shell_write_targets`, `shell_writes_violation`).

use crate::config::{self, Config};
use crate::{CheckError, protect_path};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

const SINKS: [&str; 4] = ["/dev/null", "/dev/stdout", "/dev/stderr", "/dev/tty"];
/// `TEXT_REDIRECT_SUFFIXES | {".patch", ".diff", ".out", ".err"}`: prose and data, never source or config.
const DATA_SUFFIXES: [&str; 11] = [
    ".txt", ".md", ".log", ".rst", ".csv", ".tsv", ".adoc", ".patch", ".diff", ".out", ".err",
];
const COMMAND_PREFIXES: [&str; 7] = ["sudo", "env", "command", "time", "nice", "nohup", "exec"];
const PUNCTUATION: &str = "();<>|&";

/// `strip_heredocs`: drop here-document bodies, which are data.
pub fn strip_heredocs(command: &str) -> String {
    // `<<-?\s*(['"]?)NAME\1` without a backreference: a quoted name needs the same closing quote.
    static START: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r#"<<-?\s*(?:'([A-Za-z_][A-Za-z0-9_]*)'|"([A-Za-z_][A-Za-z0-9_]*)"|([A-Za-z_][A-Za-z0-9_]*))"#,
        )
        .expect("static pattern")
    });
    let mut out = Vec::new();
    let mut delimiter: Option<String> = None;
    for line in command.split('\n') {
        if let Some(end) = &delimiter {
            if crate::py::strip(line) == end {
                delimiter = None;
            }
            continue;
        }
        out.push(line);
        delimiter = START.captures(line).and_then(|caps| {
            caps.get(1)
                .or(caps.get(2))
                .or(caps.get(3))
                .map(|name| name.as_str().to_string())
        });
    }
    out.join("\n")
}

/// `shlex.shlex(text, posix=True, punctuation_chars=True)` with `whitespace_split = True` and no commenters.
pub fn shlex_tokens(text: &str) -> Result<Vec<String>, String> {
    #[derive(Clone, Copy, PartialEq)]
    enum State {
        Space,
        Word,
        Punct,
        Quote(char),
        Escape(char),
    }
    let is_space = |c: char| matches!(c, ' ' | '\t' | '\r' | '\n');
    let is_punct = |c: char| PUNCTUATION.contains(c);
    let mut tokens = Vec::new();
    let mut chars = text.chars();
    let mut pushback: Option<char> = None;
    loop {
        let mut token = String::new();
        let mut quoted = false;
        let mut state = State::Space;
        let mut end = false;
        loop {
            let next = pushback.take().or_else(|| chars.next());
            match state {
                State::Space => match next {
                    None => {
                        end = true;
                        break;
                    }
                    Some(c) if is_space(c) => {
                        if !token.is_empty() || quoted {
                            break;
                        }
                    }
                    Some('\\') => state = State::Escape('a'),
                    Some(c) if is_punct(c) => {
                        token.push(c);
                        state = State::Punct;
                    }
                    Some(c @ ('\'' | '"')) => state = State::Quote(c),
                    Some(c) => {
                        token.push(c);
                        state = State::Word;
                    }
                },
                State::Quote(q) => {
                    quoted = true;
                    match next {
                        None => return Err("No closing quotation".into()),
                        Some(c) if c == q => state = State::Word,
                        Some('\\') if q == '"' => state = State::Escape('"'),
                        Some(c) => token.push(c),
                    }
                }
                State::Escape(from) => match next {
                    None => return Err("No escaped character".into()),
                    Some(c) => {
                        if from == '"' && c != '\\' && c != '"' {
                            token.push('\\');
                        }
                        token.push(c);
                        state = if from == 'a' { State::Word } else { State::Quote(from) };
                    }
                },
                State::Word | State::Punct => match next {
                    None => {
                        end = true;
                        break;
                    }
                    Some(c) if is_space(c) => {
                        if !token.is_empty() || quoted {
                            break;
                        }
                        state = State::Space;
                    }
                    Some(c) if state == State::Punct => {
                        if is_punct(c) {
                            token.push(c);
                        } else {
                            pushback = Some(c);
                            break;
                        }
                    }
                    Some(c @ ('\'' | '"')) => state = State::Quote(c),
                    Some('\\') => state = State::Escape('a'),
                    Some(c) if !is_punct(c) => token.push(c),
                    Some(c) => {
                        pushback = Some(c);
                        if !token.is_empty() || quoted {
                            break;
                        }
                        state = State::Space;
                    }
                },
            }
        }
        // posix: an unquoted empty token is the end of input.
        if token.is_empty() && !quoted {
            if end {
                return Ok(tokens);
            }
            continue;
        }
        tokens.push(token);
        if end {
            return Ok(tokens);
        }
    }
}

/// `Path(text).name`: the last component, `.` components dropped.
fn path_name(text: &str) -> &str {
    text.split('/')
        .rfind(|part| !part.is_empty() && *part != ".")
        .unwrap_or("")
}

/// `str(Path(a) / b)` for the targets' purposes: `b` absolute replaces `a`.
fn join(directory: &str, name: &str) -> String {
    if name.starts_with('/') || directory.is_empty() {
        return name.to_string();
    }
    if name.is_empty() {
        return directory.trim_end_matches('/').to_string();
    }
    format!("{}/{name}", directory.trim_end_matches('/'))
}

/// `shell_write_targets`: the files a command writes, as `(target, how)`; None when it cannot be parsed.
/// `cwd` is the directory `Path(destination).is_dir()` is evaluated in (loki.py runs with the project root as cwd).
pub fn shell_write_targets(command: &str, cwd: &Path) -> Option<Vec<(String, String)>> {
    let tokens = shlex_tokens(&strip_heredocs(command)).ok()?;
    let mut targets: Vec<(String, String)> = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let flush = |words: &mut Vec<String>, targets: &mut Vec<(String, String)>| {
        while let Some(first) = words.first() {
            if (first.contains('=') && !first.starts_with('-')) || COMMAND_PREFIXES.contains(&first.as_str()) {
                words.remove(0);
            } else {
                break;
            }
        }
        if words.is_empty() {
            return;
        }
        let name = path_name(&words[0]).to_string();
        let args = &words[1..];
        let plain: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
        let mut add = |target: String, how: &str| targets.push((target, how.to_string()));
        match name.as_str() {
            "tee" => plain.iter().for_each(|a| add(a.to_string(), "tee")),
            "sed"
                if args
                    .iter()
                    .any(|a| a == "-i" || a.starts_with("-i") || a.starts_with("--in-place")) =>
            {
                let (mut files, mut scripted, mut skip) = (Vec::new(), false, false);
                for arg in args {
                    if skip {
                        skip = false;
                    } else if ["-e", "-f", "--expression", "--file"].contains(&arg.as_str()) {
                        scripted = true;
                        skip = true;
                    } else if !arg.starts_with('-') {
                        files.push(arg.clone());
                    }
                }
                let files = if scripted {
                    &files[..]
                } else {
                    files.get(1..).unwrap_or(&[])
                };
                files.iter().for_each(|f| add(f.clone(), "sed -i"));
            }
            "perl" if args.iter().any(|a| perl_in_place(a)) => {
                let (mut files, mut skip) = (Vec::new(), false);
                for arg in args {
                    if skip {
                        skip = false;
                    } else if arg == "-e" || arg == "-E" {
                        skip = true;
                    } else if !arg.starts_with('-') {
                        files.push(arg.clone());
                    }
                }
                files.into_iter().for_each(|f| add(f, "perl -i"));
            }
            "cp" | "mv" | "install" => {
                let t = args.iter().position(|a| a == "-t");
                if let Some(directory) = t.and_then(|i| args.get(i + 1)) {
                    for source in plain.iter().filter(|a| **a != directory) {
                        add(join(directory, path_name(source)), &name);
                    }
                } else if plain.len() >= 2 {
                    let destination = plain[plain.len() - 1];
                    if plain.len() > 2 || destination.ends_with('/') || cwd.join(destination).is_dir() {
                        for source in &plain[..plain.len() - 1] {
                            add(join(destination, path_name(source)), &name);
                        }
                    } else {
                        add(destination.clone(), &name);
                    }
                }
            }
            "dd" => args
                .iter()
                .filter_map(|a| a.strip_prefix("of="))
                .for_each(|t| add(t.to_string(), "dd")),
            _ => {}
        }
        words.clear();
    };
    let only = |token: &str, set: &str| token.chars().all(|c| set.contains(c));
    let mut index = 0;
    while index < tokens.len() {
        let token = tokens[index].as_str();
        if [">", ">>", ">|", "&>", "&>>"].contains(&token) || (token.ends_with('>') && only(token, "<>&|;()")) {
            if let Some(target) = tokens.get(index + 1) {
                targets.push((target.clone(), "redirect".into()));
            }
            index += 2;
            continue;
        }
        if only(token, "();<>|&") {
            flush(&mut words, &mut targets);
            index += 1;
            continue;
        }
        if !token.is_empty()
            && token.chars().all(|c| c.is_ascii_digit())
            && tokens.get(index + 1).is_some_and(|n| n == ">" || n == ">>")
        {
            index += 1;
            continue;
        }
        words.push(token.to_string());
        index += 1;
    }
    flush(&mut words, &mut targets);
    targets.retain(|(target, _)| {
        !SINKS.contains(&target.as_str()) && !target.starts_with("/dev/fd/") && !target.starts_with("/proc/self/fd/")
    });
    Some(targets)
}

/// `re.fullmatch(r"-[a-zA-Z]*i\S*", arg)`.
fn perl_in_place(arg: &str) -> bool {
    let Some(rest) = arg.strip_prefix('-') else {
        return false;
    };
    let letters = rest.chars().take_while(char::is_ascii_alphabetic).count();
    // Backtracking: some `i` among the leading letters, followed only by non-space characters.
    rest.char_indices()
        .take(letters)
        .any(|(i, c)| c == 'i' && !rest[i + 1..].chars().any(char::is_whitespace))
}

/// `Path.suffix` (Python 3.14): from the last dot of the name, unless the name is only dots before it.
fn suffix(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if !name[..i].chars().all(|c| c == '.') => &name[i..],
        _ => "",
    }
}

/// `os.path.abspath(cwd / target)`.
fn absolute(cwd: &Path, target: &str) -> PathBuf {
    let mut out = PathBuf::from("/");
    for component in cwd.join(target).components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(part) => out.push(part),
            _ => {}
        }
    }
    out
}

/// `git check-ignore -q -- relative` in `root`.
pub fn git_ignored(root: &Path, relative: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["check-ignore", "-q", "--", relative])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// `shell_writes_violation`: the refusal for a command that writes source or configuration inside `root`.
/// `root` is resolved; `cwd` is where the command runs.
pub fn shell_writes_violation(command: &str, root: &Path, cwd: &Path) -> Result<Option<String>, CheckError> {
    if crate::py::strip(command).is_empty() {
        return Ok(None);
    }
    let targets = shell_write_targets(command, root).unwrap_or_default();
    if targets.is_empty() {
        return Ok(None);
    }
    let config = load(root)?;
    for (target, how) in targets {
        if ['$', '`', '*', '?', '~'].iter().any(|m| target.contains(*m)) {
            continue;
        }
        let path = absolute(cwd, &target);
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let path_text = path.to_string_lossy();
        if let Some(message) = protect_path(&path_text, root, &config)? {
            return Ok(Some(message));
        }
        let relative_text = relative.to_string_lossy().into_owned();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let hidden = relative
            .components()
            .any(|c| c.as_os_str().to_string_lossy().starts_with('.'));
        if DATA_SUFFIXES.contains(&suffix(&name).to_lowercase().as_str()) && !hidden {
            continue;
        }
        if git_ignored(root, &relative_text) {
            continue;
        }
        return Ok(Some(format!(
            "loki: `{how}` into {relative_text} would skip Loki's write check; write source and configuration files with the file edit tools (Ultron: edit()/write())"
        )));
    }
    Ok(None)
}

/// `load_config`; every failure is a `ValueError` here (UnicodeDecodeError is one).
fn load(root: &Path) -> Result<Config, CheckError> {
    config::load_config(root).map_err(|e| match e {
        config::ConfigError::Invalid(m) | config::ConfigError::Guard(_, m) => CheckError::Value(m),
    })
}

/// `loki --root ROOT shell-writes` on a parsed envelope `{command, cwd?}`: exit status and stderr.
pub fn run(root: &Path, payload: &serde_json::Value) -> (u8, String) {
    let check = || -> Result<Option<String>, CheckError> {
        let map = payload
            .as_object()
            .ok_or_else(|| CheckError::Value("invalid shell envelope".into()))?;
        let root = crate::resolve(root);
        let cwd = match map.get("cwd") {
            Some(serde_json::Value::String(cwd)) if !cwd.is_empty() => crate::resolve(Path::new(cwd)),
            _ => root.clone(),
        };
        let Some(serde_json::Value::String(command)) = map.get("command") else {
            return Ok(None);
        };
        shell_writes_violation(command, &root, &cwd)
    };
    match check() {
        Ok(None) => (0, String::new()),
        Ok(Some(reason)) => (2, format!("{reason}\n")),
        Err(CheckError::Value(m) | CheckError::Guard(_, m)) => (2, format!("loki: invalid shell input: {m}\n")),
    }
}
