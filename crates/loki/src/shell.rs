//! `shell`, `shell-writes` and `explain`: loki.py's shell guards and event log.

use crate::config::{self, Config};
use crate::{CheckError, lexical_path, protect_path, py, resolve};
use regex::Regex;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::LazyLock;

const PUNCTUATION: &str = "();<>|&";
const TEXT_REDIRECT_SUFFIXES: [&str; 7] = [".txt", ".md", ".log", ".rst", ".csv", ".tsv", ".adoc"];
const WRITE_DATA_EXTRA: [&str; 4] = [".patch", ".diff", ".out", ".err"];
const WRITE_SINKS: [&str; 4] = ["/dev/null", "/dev/stdout", "/dev/stderr", "/dev/tty"];
const COMMAND_PREFIXES: [&str; 7] = ["sudo", "env", "command", "time", "nice", "nohup", "exec"];

fn invalid(message: impl Into<String>) -> CheckError {
    CheckError::Value(message.into())
}

/// `shlex.shlex(s, posix=True, punctuation_chars=True)` with `whitespace_split`;
/// `comments` keeps the default `#` commenter.
pub fn shlex_punctuated(s: &str, comments: bool) -> Result<Vec<String>, String> {
    let chars: Vec<char> = s.chars().collect();
    let (mut i, mut tokens) = (0usize, Vec::new());
    let is_space = |c: char| matches!(c, ' ' | '\t' | '\r' | '\n');
    let is_punct = |c: char| PUNCTUATION.contains(c);
    loop {
        let (mut token, mut quoted, mut state) = (String::new(), false, ' ');
        let mut escaped = 'a';
        loop {
            let next = chars.get(i).copied();
            i += 1;
            match state {
                ' ' => match next {
                    None => break,
                    Some(c) if is_space(c) => {
                        if !token.is_empty() || quoted {
                            break;
                        }
                    }
                    Some('#') if comments => {
                        while i <= chars.len() && chars.get(i - 1) != Some(&'\n') {
                            i += 1;
                        }
                    }
                    Some('\\') => {
                        escaped = 'a';
                        state = '\\';
                    }
                    Some(c) if is_punct(c) => {
                        token.push(c);
                        state = 'c';
                    }
                    Some(c @ ('\'' | '"')) => state = c,
                    Some(c) => {
                        token.push(c);
                        state = 'a';
                    }
                },
                'a' | 'c' => match next {
                    None => break,
                    Some(c) if is_space(c) => {
                        state = ' ';
                        if !token.is_empty() || quoted {
                            break;
                        }
                    }
                    Some('#') if comments => {
                        while i <= chars.len() && chars.get(i - 1) != Some(&'\n') {
                            i += 1;
                        }
                        state = ' ';
                        if !token.is_empty() || quoted {
                            break;
                        }
                    }
                    Some(c) if state == 'c' => {
                        if is_punct(c) {
                            token.push(c);
                        } else {
                            i -= 1;
                            break;
                        }
                    }
                    Some(c @ ('\'' | '"')) => state = c,
                    Some('\\') => {
                        escaped = 'a';
                        state = '\\';
                    }
                    Some(c) if !is_punct(c) => token.push(c),
                    Some(_) => {
                        i -= 1;
                        state = ' ';
                        if !token.is_empty() || quoted {
                            break;
                        }
                    }
                },
                '\\' => {
                    let Some(c) = next else {
                        return Err("No escaped character".into());
                    };
                    if escaped == '"' && c != '\\' && c != '"' {
                        token.push('\\');
                    }
                    token.push(c);
                    state = escaped;
                }
                quote => {
                    quoted = true;
                    match next {
                        None => return Err("No closing quotation".into()),
                        Some(c) if c == quote => state = 'a',
                        Some('\\') if quote == '"' => {
                            escaped = '"';
                            state = '\\';
                        }
                        Some(c) => token.push(c),
                    }
                }
            }
        }
        if token.is_empty() && !quoted {
            return Ok(tokens);
        }
        tokens.push(token);
    }
}

fn punctuation_only(token: &str) -> bool {
    token.chars().all(|c| PUNCTUATION.contains(c))
}

/// `shlex.quote`.
pub fn shlex_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".into();
    }
    if s.chars().all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c)) {
        return s.into();
    }
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

/// `command > target` or `>>` when that is the only operator.
pub fn shell_redirect(command: &str) -> Option<(String, String)> {
    let tokens = shlex_punctuated(command, true).ok()?;
    let operators: Vec<&String> = tokens.iter().filter(|t| punctuation_only(t)).collect();
    let n = tokens.len();
    if n < 3 || operators != [&tokens[n - 2]] || !matches!(tokens[n - 2].as_str(), ">" | ">>") {
        return None;
    }
    let printer = tokens[..n - 2]
        .iter()
        .map(|t| shlex_quote(t))
        .collect::<Vec<_>>()
        .join(" ");
    Some((printer, tokens[n - 1].clone()))
}

fn suffix(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    match name.rfind('.') {
        Some(i) if i > 0 && i + 1 < name.len() => name[i..].to_lowercase(),
        _ => String::new(),
    }
}

/// `os.path.abspath(cwd / target)`: lexical, no `~` expansion.
fn abspath(cwd: &Path, target: &str) -> PathBuf {
    let mut out = PathBuf::from("/");
    for component in cwd.join(target).components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::Normal(part) => out.push(part),
            _ => {}
        }
    }
    out
}

fn relative_of(path: &Path, root: &Path) -> Option<String> {
    path.strip_prefix(root).ok().map(|p| p.to_string_lossy().into_owned())
}

fn hidden(relative: &str) -> bool {
    relative.split('/').any(|part| part.starts_with('.'))
}

fn load(root: &Path) -> Result<Config, CheckError> {
    config::load_config(root).map_err(|e| match e {
        config::ConfigError::Invalid(m) => CheckError::Value(m),
        config::ConfigError::Guard(kind, m) => CheckError::Guard(kind, m),
    })
}

/// Printing into project notes is allowed; source and policy writes are not.
fn redirect_violation(target: &str, root: &Path, cwd: &Path) -> Result<Option<String>, CheckError> {
    let path = abspath(cwd, target);
    if !path.starts_with(root) || resolve(&path) != path {
        return Ok(Some("loki: shell redirect target escapes the repository root".into()));
    }
    if let Some(message) = protect_path(&path.to_string_lossy(), root, &load(root)?)? {
        return Ok(Some(message));
    }
    let relative = relative_of(&path, root).unwrap_or_default();
    let shown = if relative.is_empty() {
        ".".to_string()
    } else {
        relative.clone()
    };
    if !TEXT_REDIRECT_SUFFIXES.contains(&suffix(&path).as_str()) || hidden(&relative) || path.is_dir() {
        return Ok(Some(format!(
            "loki: shell redirect into {shown} bypasses write checks; use file edit tools for source and configuration files"
        )));
    }
    Ok(None)
}

/// `destructive_git`: Git commands that discard work or rewrite history.
pub fn destructive_git(words: &[String]) -> bool {
    let command = words.get(1).map_or("", String::as_str);
    let options: Vec<&str> = words.iter().skip(2).map(String::as_str).collect();
    let any = |set: &[&str]| options.iter().any(|o| set.contains(o));
    match command {
        "reset" => any(&["--hard", "--merge", "--keep"]),
        "clean" => {
            options
                .iter()
                .any(|o| o.starts_with('-') && !o.starts_with("--") && o.contains('f'))
                || options.contains(&"--force")
        }
        "push" => {
            any(&["-f", "--force", "--force-with-lease", "--delete", "--mirror"])
                || options.iter().any(|o| o.starts_with([':', '+']))
        }
        "branch" => options.contains(&"-D") || (options.contains(&"--delete") && options.contains(&"--force")),
        "stash" => any(&["drop", "clear"]),
        "checkout" => any(&["--", ".", "-f", "--force"]),
        _ => matches!(command, "restore" | "rebase" | "filter-branch" | "update-ref"),
    }
}

/// `shell_violation`: the default-deny shell guard.
pub fn shell_violation(command: &Value, root: &Path, cwd: &Path) -> Result<Option<String>, CheckError> {
    if let Some(text) = command.as_str()
        && let Some((printer, target)) = shell_redirect(text)
    {
        // loki.py's `try` also covers the redirect check: its `ValueError`s fall through.
        if config::simple_shell_words(&Value::String(printer))
            .is_ok_and(|words| matches!(words[0].as_str(), "echo" | "printf"))
        {
            match redirect_violation(&target, root, cwd) {
                Err(CheckError::Value(_)) => {}
                other => return other,
            }
        }
        return Ok(Some(
            "loki: shell redirect from a command other than echo/printf requires review; use file edit tools for writes"
                .into(),
        ));
    }
    let words = match config::simple_shell_words(command) {
        Ok(words) => words,
        Err(message) => return Ok(Some(message)),
    };
    let first = words[0].as_str();
    if first == "git"
        && words.len() >= 2
        && matches!(words[1].as_str(), "status" | "diff" | "log" | "show" | "rev-parse")
    {
        if words[2..].iter().any(|w| {
            ["--output", "--ext-diff", "--textconv", "--exec"]
                .iter()
                .any(|p| w.starts_with(p))
        }) {
            return Ok(Some("loki: shell Git option requires review".into()));
        }
        return Ok(None);
    }
    if matches!(first, "pwd" | "whoami") && words.len() == 1 {
        return Ok(None);
    }
    if first == "git" && destructive_git(&words) {
        let shown = words.iter().take(4).cloned().collect::<Vec<_>>().join(" ");
        return Ok(Some(format!(
            "loki: blocked destructive Git command `{shown}`: it can discard uncommitted work or rewrite history; ask the user to run it"
        )));
    }
    if matches!(first, "echo" | "printf") {
        return Ok(None);
    }
    if first != "git" {
        return reviewed_shell_violation(command.as_str().unwrap_or(""), root, cwd);
    }
    Ok(Some(
        "loki: shell command requires review; use verified file tools for writes".into(),
    ))
}

fn git(root: &Path, args: &[&str]) -> Result<Vec<u8>, CheckError> {
    let output = Command::new("git")
        .arg("--literal-pathspecs")
        .args(args)
        .current_dir(root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| invalid(format!("git: {e}")))?;
    if !output.status.success() {
        let reason = py::strip(&String::from_utf8_lossy(&output.stderr)).to_string();
        let reason = if reason.is_empty() {
            format!("exited {}", output.status.code().unwrap_or(-1))
        } else {
            reason
        };
        return Err(invalid(format!("git: {reason}")));
    }
    Ok(output.stdout)
}

/// `ls-tree -z` rows: path to (mode, kind, oid).
fn tree_entries(root: &Path, base: &str, paths: &[String]) -> Result<Vec<(String, [String; 3])>, CheckError> {
    let mut args = vec!["ls-tree", "-z", base, "--"];
    args.extend(paths.iter().map(String::as_str));
    let raw = git(root, &args)?;
    let mut entries = Vec::new();
    for row in raw.split(|&b| b == 0).filter(|r| !r.is_empty()) {
        let text = String::from_utf8_lossy(row);
        let (metadata, path) = text
            .split_once('\t')
            .ok_or_else(|| invalid("git: invalid tree entry"))?;
        let fields: Vec<String> = metadata.split_whitespace().map(str::to_string).collect();
        let [mode, kind, oid] = <[String; 3]>::try_from(fields).map_err(|_| invalid("git: invalid tree entry"))?;
        entries.push((path.to_string(), [mode, kind, oid]));
    }
    Ok(entries)
}

/// `base_policy`: the committed `.loki/loki.json` at `base`.
fn base_policy(root: &Path, base: &str) -> Result<Option<Config>, CheckError> {
    let entries = tree_entries(root, base, &[".loki/loki.json".into()])?;
    let Some((_, [mode, kind, oid])) = entries.into_iter().next() else {
        return Ok(None);
    };
    if !matches!(mode.as_str(), "100644" | "100755") || kind != "blob" {
        return Err(invalid("loki: base policy must be a regular file"));
    }
    let raw = git(root, &["cat-file", "blob", &oid])?;
    let value: Value = serde_json::from_slice(&raw).map_err(|e| invalid(e.to_string()))?;
    config::validate_config(&value).map(Some).map_err(CheckError::Value)
}

/// Working-tree content and Git mode of a candidate, without following a final symlink.
fn worktree_entry(path: &Path, name: &str) -> Result<Option<(Vec<u8>, &'static str)>, CheckError> {
    use std::os::unix::fs::PermissionsExt;
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(invalid(e.to_string())),
    };
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(path).map_err(|e| invalid(e.to_string()))?;
        return Ok(Some((target.into_os_string().into_encoded_bytes(), "120000")));
    }
    if !meta.is_file() {
        return Err(invalid(format!("loki: unsupported changed file type: {name}")));
    }
    let content = std::fs::read(path).map_err(|e| invalid(e.to_string()))?;
    let mode = if meta.permissions().mode() & 0o100 != 0 {
        "100755"
    } else {
        "100644"
    };
    Ok(Some((content, mode)))
}

/// `reviewed_shell_violation`: an exact committed grant whose inputs are unchanged.
fn reviewed_shell_violation(command: &str, root: &Path, cwd: &Path) -> Result<Option<String>, CheckError> {
    let top = String::from_utf8_lossy(&git(root, &["rev-parse", "--show-toplevel"])?)
        .trim_end_matches('\n')
        .to_string();
    if resolve(Path::new(&top)) != root {
        return Err(invalid("shell permissions require the repository root"));
    }
    let base = String::from_utf8_lossy(&git(
        root,
        &["rev-parse", "--verify", "--end-of-options", "HEAD^{commit}"],
    )?)
    .trim()
    .to_string();
    let policy = base_policy(root, &base)?.unwrap_or_default();
    config::shell_permissions(&policy).map_err(CheckError::Value)?;
    let grants = policy
        .get("shell_commands")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let grant = grants.iter().find(|g| {
        g["command"].as_str() == Some(command) && g["cwd"].as_str().is_some_and(|d| lexical_path(root, d) == cwd)
    });
    let Some(grant) = grant else {
        return Ok(Some(
            "loki: no committed permission for this exact shell command and cwd".into(),
        ));
    };
    let mut paths = vec![".loki/loki.json".to_string()];
    for input in grant["inputs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if !paths.iter().any(|p| p == input) {
            paths.push(input.to_string());
        }
    }
    let entries = tree_entries(root, &base, &paths)?;
    let entry = |name: &str| entries.iter().find(|(p, _)| p == name).map(|(_, e)| e);
    if paths.iter().any(|name| {
        entry(name).is_none_or(|[mode, kind, _]| !matches!(mode.as_str(), "100644" | "100755") || kind != "blob")
    }) {
        return Err(invalid("shell permission inputs must be committed regular files"));
    }
    for name in &paths {
        let path = root.join(name);
        if resolve(&path) != path {
            return Err(invalid(format!("shell permission input is symlinked: {name}")));
        }
        let [mode, _, oid] = entry(name).expect("checked above");
        let blob = git(root, &["cat-file", "blob", oid])?;
        let current = worktree_entry(&path, name)?;
        if current
            .as_ref()
            .is_none_or(|(content, m)| *content != blob || m != mode)
        {
            return Ok(Some(format!("loki: shell permission input differs from base: {name}")));
        }
    }
    Ok(None)
}

/// `shell_cwd`: the hook's working directory, which must be a real directory inside the root.
pub fn shell_cwd(payload: &Value, harness: &str, root: &Path) -> Result<PathBuf, CheckError> {
    let names: &[&str] = match harness {
        "claude" => &["Bash"],
        "factory" => &["Execute"],
        _ => &["bash", "Bash"],
    };
    if !payload["tool_name"].as_str().is_some_and(|n| names.contains(&n)) {
        return Err(invalid("unexpected shell tool name"));
    }
    let fields = [
        "command",
        "description",
        "timeout",
        "run_in_background",
        "summary",
        "riskLevel",
        "riskLevelReason",
        "fireAndForget",
    ];
    let input = payload["tool_input"]
        .as_object()
        .ok_or_else(|| invalid("invalid shell envelope"))?;
    if input.keys().any(|k| !fields.contains(&k.as_str())) {
        return Err(invalid("unsupported shell input fields"));
    }
    let directory = match payload.get("cwd").and_then(Value::as_str) {
        Some(d) if !d.is_empty() && !d.contains('\0') => d,
        _ => return Err(invalid("shell cwd must be an absolute directory")),
    };
    // pathlib equality ignores `.` components and trailing slashes, not `..`.
    let cwd: PathBuf = Path::new(directory).components().collect();
    if !cwd.is_absolute() || resolve(&cwd) != cwd || !cwd.starts_with(root) || !cwd.is_dir() {
        return Err(invalid("shell cwd must be a non-symlinked directory inside the root"));
    }
    Ok(cwd)
}

static HEREDOC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<<-?\s*(?:'([A-Za-z_][A-Za-z0-9_]*)'|"([A-Za-z_][A-Za-z0-9_]*)"|([A-Za-z_][A-Za-z0-9_]*))"#)
        .expect("static pattern")
});

/// Drop here-document bodies, which are data, so the rest parses as shell words.
pub fn strip_heredocs(command: &str) -> String {
    let (mut out, mut delimiter) = (Vec::new(), None::<String>);
    for line in command.split('\n') {
        if let Some(d) = &delimiter {
            if py::strip(line) == d {
                delimiter = None;
            }
            continue;
        }
        out.push(line);
        if let Some(caps) = HEREDOC.captures(line) {
            delimiter = (1..=3).find_map(|g| caps.get(g)).map(|m| m.as_str().to_string());
        }
    }
    out.join("\n")
}

fn file_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// `str(Path(directory) / Path(src).name)`.
fn join_name(directory: &str, source: &str) -> String {
    let joined: PathBuf = Path::new(directory).join(file_name(source)).components().collect();
    joined.to_string_lossy().into_owned()
}

static PERL_INPLACE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^-[a-zA-Z]*i\S*$").expect("static pattern"));

fn command_targets(words: &mut Vec<String>, targets: &mut Vec<(String, String)>) {
    while words
        .first()
        .is_some_and(|w| (w.contains('=') && !w.starts_with('-')) || COMMAND_PREFIXES.contains(&w.as_str()))
    {
        words.remove(0);
    }
    if words.is_empty() {
        return;
    }
    let (name, args) = (file_name(&words[0]), &words[1..]);
    let plain: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
    let mut add = |target: String, how: &str| targets.push((target, how.to_string()));
    match name.as_str() {
        "tee" => plain.iter().for_each(|a| add(a.to_string(), "tee")),
        "sed" if args.iter().any(|a| a.starts_with("-i") || a.starts_with("--in-place")) => {
            let (mut files, mut scripted, mut skip) = (Vec::new(), false, false);
            for arg in args {
                if skip {
                    skip = false;
                } else if matches!(arg.as_str(), "-e" | "-f" | "--expression" | "--file") {
                    scripted = true;
                    skip = true;
                } else if !arg.starts_with('-') {
                    files.push(arg.clone());
                }
            }
            let skip_script = usize::from(!scripted);
            files.into_iter().skip(skip_script).for_each(|f| add(f, "sed -i"));
        }
        "perl" if args.iter().any(|a| PERL_INPLACE.is_match(a)) => {
            let mut skip = false;
            for arg in args {
                if skip {
                    skip = false;
                } else if matches!(arg.as_str(), "-e" | "-E") {
                    skip = true;
                } else if !arg.starts_with('-') {
                    add(arg.clone(), "perl -i");
                }
            }
        }
        "cp" | "mv" | "install" => {
            let flag = args.iter().position(|a| a == "-t");
            if let Some(directory) = flag.and_then(|i| args.get(i + 1)) {
                for source in plain.iter().filter(|a| *a != &directory) {
                    add(join_name(directory, source), &name);
                }
            } else if plain.len() >= 2 {
                let destination = plain[plain.len() - 1];
                if plain.len() > 2 || destination.ends_with('/') || Path::new(destination.as_str()).is_dir() {
                    for source in &plain[..plain.len() - 1] {
                        add(join_name(destination, source), &name);
                    }
                } else {
                    add(destination.clone(), &name);
                }
            }
        }
        "dd" => args
            .iter()
            .filter_map(|a| a.strip_prefix("of="))
            .for_each(|t| add(t.into(), "dd")),
        _ => {}
    }
    words.clear();
}

/// The files a shell command writes, as `(target, how)`; `None` when it cannot be parsed.
pub fn shell_write_targets(command: &str) -> Option<Vec<(String, String)>> {
    let tokens = shlex_punctuated(&strip_heredocs(command), false).ok()?;
    let (mut targets, mut words) = (Vec::new(), Vec::new());
    let mut index = 0;
    while index < tokens.len() {
        let token = tokens[index].as_str();
        let redirect = matches!(token, ">" | ">>" | ">|" | "&>" | "&>>")
            || ((token.ends_with('>') || token.ends_with(">>")) && punctuation_only(token));
        if redirect {
            if let Some(next) = tokens.get(index + 1) {
                targets.push((next.clone(), "redirect".to_string()));
            }
            index += 2;
            continue;
        }
        if punctuation_only(token) {
            command_targets(&mut words, &mut targets);
            index += 1;
            continue;
        }
        if !token.is_empty()
            && token.chars().all(char::is_numeric)
            && tokens.get(index + 1).is_some_and(|n| n == ">" || n == ">>")
        {
            index += 1;
            continue;
        }
        words.push(token.to_string());
        index += 1;
    }
    command_targets(&mut words, &mut targets);
    targets.retain(|(t, _)| {
        !WRITE_SINKS.contains(&t.as_str()) && !t.starts_with("/dev/fd/") && !t.starts_with("/proc/self/fd/")
    });
    Some(targets)
}

fn git_ignored(root: &Path, relative: &str) -> bool {
    Command::new("git")
        .args(["-C", &root.to_string_lossy(), "check-ignore", "-q", "--", relative])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// `shell_writes_violation`: deny shell writes of source or configuration inside the project.
pub fn shell_writes_violation(command: &Value, root: &Path, cwd: &Path) -> Result<Option<String>, CheckError> {
    let Some(command) = command.as_str().filter(|c| !py::strip(c).is_empty()) else {
        return Ok(None);
    };
    let Some(targets) = shell_write_targets(command).filter(|t| !t.is_empty()) else {
        return Ok(None);
    };
    let root = resolve(root);
    let config = load(&root)?;
    for (target, how) in targets {
        if target.contains(['$', '`', '*', '?', '~']) {
            continue;
        }
        let path = abspath(cwd, &target);
        let Some(relative) = relative_of(&path, &root) else {
            continue;
        };
        if let Some(message) = protect_path(&path.to_string_lossy(), &root, &config)? {
            return Ok(Some(message));
        }
        let data = TEXT_REDIRECT_SUFFIXES.contains(&suffix(&path).as_str())
            || WRITE_DATA_EXTRA.contains(&suffix(&path).as_str());
        if data && !hidden(&relative) {
            continue;
        }
        let shown = if relative.is_empty() { ".".to_string() } else { relative };
        if git_ignored(&root, &shown) {
            continue;
        }
        return Ok(Some(format!(
            "loki: `{how}` into {shown} would skip Loki's write check; write source and configuration files with the file edit tools (Ultron: edit()/write())"
        )));
    }
    Ok(None)
}

/// `event_path`: `~/.local/state/loki/<sha256(root)>/events.jsonl`.
pub fn event_path(root: &Path) -> PathBuf {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(resolve(root).as_os_str().as_encoded_bytes());
    let identity: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join(".local/state/loki").join(identity).join("events.jsonl")
}

fn sort_keys(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> = map.into_iter().collect();
            entries.sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()));
            Value::Object(entries.into_iter().map(|(k, v)| (k, sort_keys(v))).collect())
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sort_keys).collect()),
        other => other,
    }
}

/// `explain`: the last `limit` recorded events, one sorted-key JSON object per line.
pub fn explain(root: &Path, limit: i64) -> Result<Vec<String>, String> {
    if !(1..=1000).contains(&limit) {
        return Err("limit must be between 1 and 1000".into());
    }
    let path = event_path(root);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e.to_string()),
    };
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let start = lines.len().saturating_sub(limit as usize);
    lines[start..]
        .iter()
        .map(|line| {
            let value: Value = serde_json::from_str(line).map_err(|e| e.to_string())?;
            Ok(py::dumps(&sort_keys(value)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn loki_shell_shlex_punctuated_matches_python() {
        // python3: l = shlex.shlex(s, posix=True, punctuation_chars=True); l.whitespace_split = True; list(l)
        assert_eq!(
            shlex_punctuated("a>b;c&&d|e", true).unwrap(),
            words(&["a", ">", "b", ";", "c", "&&", "d", "|", "e"])
        );
        assert_eq!(
            shlex_punctuated(r#"echo "x\y\$" 2>&1 # c"#, true).unwrap(),
            words(&["echo", r"x\y\$", "2", ">&", "1"])
        );
        assert_eq!(shlex_punctuated("a#b c", false).unwrap(), words(&["a#b", "c"]));
        assert_eq!(shlex_punctuated("x '' y", true).unwrap(), words(&["x", "", "y"]));
        assert_eq!(shlex_punctuated("echo 'a", true).unwrap_err(), "No closing quotation");
    }

    #[test]
    fn loki_shell_quote_matches_python() {
        // python3 -c 'import shlex; print([shlex.quote(s) for s in ["", "a-b/c.d", "it'"'"'s", "a b"]])'
        assert_eq!(shlex_quote(""), "''");
        assert_eq!(shlex_quote("a-b/c.d"), "a-b/c.d");
        assert_eq!(shlex_quote("it's"), r#"'it'"'"'s'"#);
        assert_eq!(shlex_quote("a b"), "'a b'");
    }

    #[test]
    fn loki_shell_strip_heredocs_drops_bodies() {
        assert_eq!(
            strip_heredocs("cat > a <<'EOF'\nx > y\n  EOF  \necho z"),
            "cat > a <<'EOF'\necho z"
        );
        assert_eq!(strip_heredocs("cat <<-\"END\"\nbody"), "cat <<-\"END\"");
    }

    #[test]
    fn loki_shell_write_targets_cover_writers() {
        let found =
            shell_write_targets("sed -i -e s/a/b/ x.py y.py; dd of=z.img; make 2>/dev/null | tee -a log.txt").unwrap();
        let expected = [
            ("x.py", "sed -i"),
            ("y.py", "sed -i"),
            ("z.img", "dd"),
            ("log.txt", "tee"),
        ];
        assert_eq!(found, expected.map(|(t, h)| (t.to_string(), h.to_string())));
        assert_eq!(shell_write_targets("echo 'x > a.py"), None);
    }

    #[test]
    fn loki_shell_destructive_git_cases() {
        assert!(destructive_git(&words(&["git", "clean", "-xdf"])));
        assert!(!destructive_git(&words(&["git", "clean", "--dry-run"])));
        assert!(destructive_git(&words(&["git", "branch", "--force", "--delete", "x"])));
        assert!(!destructive_git(&words(&["git", "branch", "--force", "x"])));
        assert!(destructive_git(&words(&["git", "update-ref", "-d", "x"])));
    }
}
