//! `ultron migrate ...` (`ultron/migration-cli.ts`, `ultron/migration.ts`): profile backup and restore, and the
//! command line of Pi <-> Ultron session migration. Output names paths and counts only; credential file contents are
//! never printed.
//! `import-pi` and `export-pi` run `session_core::migration`.

use crate::Env;
use serde_json::{Value, json};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

pub const PROFILE_BACKUP_FORMAT: &str = "ultron-profile-backup";
const MANIFEST_FILE: &str = "manifest.json";
const FILES_DIR: &str = "files";
const EXCLUDED_TOP_LEVEL: [&str; 6] = ["sessions", "traces", "bin", "npm", "node_modules", "backups"];

fn usage() -> String {
    [
        "Usage: ultron migrate <command>",
        "",
        "Commands:",
        "  import-pi <pi-session.jsonl> [--sessions-root <dir>]   Import a whole Pi session tree as a native session",
        "  export-pi <native-session.jsonl> <out.jsonl>          Write a native session tree as a Pi session",
        "  backup [dir] [--agent-dir <dir>]                      Back up the config profile (not sessions)",
        "  restore <backup-dir> [--agent-dir <dir>]              Verify and restore a profile backup",
    ]
    .join("\n")
}

enum Failure {
    Usage(String),
    Error(String),
}

fn io_error(e: &std::io::Error, call: &str, path: &Path) -> Failure {
    Failure::Error(crate::usage::log::io_message(e, call, path))
}

struct Options {
    positional: Vec<String>,
    options: Vec<(String, String)>,
}

impl Options {
    fn get(&self, name: &str) -> Option<&str> {
        self.options
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

fn parse_options(args: &[String], flags: &[&str]) -> Result<Options, Failure> {
    let mut parsed = Options {
        positional: Vec::new(),
        options: Vec::new(),
    };
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if let Some(body) = arg.strip_prefix("--") {
            // `split("=", 2)`: the inline value stops at a second `=`.
            let mut parts = body.split('=');
            let name = parts.next().unwrap_or_default().to_string();
            let inline = parts.next().map(str::to_string);
            if !flags.contains(&name.as_str()) {
                return Err(Failure::Usage(format!("Unknown option --{name}")));
            }
            let value = match inline {
                Some(v) => Some(v),
                None => {
                    index += 1;
                    args.get(index).cloned()
                }
            };
            match value {
                Some(v) if !v.is_empty() => parsed.options.push((name, v)),
                _ => return Err(Failure::Usage(format!("Option --{name} requires a value"))),
            }
        } else {
            parsed.positional.push(arg.clone());
        }
        index += 1;
    }
    Ok(parsed)
}

fn expect_positional(positional: &[String], min: usize, max: usize) -> Result<(), Failure> {
    if positional.len() < min {
        return Err(Failure::Usage("Missing argument".into()));
    }
    if positional.len() > max {
        return Err(Failure::Usage(format!("Unexpected argument: {}", positional[max])));
    }
    Ok(())
}

/// Handle `migrate ...`; None when `argv` is not a migrate command.
pub fn run(argv: &[String], env: &Env, out: &mut dyn std::io::Write, err: &mut dyn std::io::Write) -> Option<i32> {
    if argv.first().map(String::as_str) != Some("migrate") {
        return None;
    }
    let sub = argv.get(1).map(String::as_str);
    if matches!(sub, None | Some("--help" | "-h" | "help")) {
        let _ = writeln!(out, "{}", usage());
        return Some(i32::from(sub.is_none()));
    }
    let rest = &argv[2..];
    match command(sub.unwrap_or_default(), rest, env) {
        Ok(lines) => {
            for line in lines {
                let _ = writeln!(out, "{line}");
            }
            Some(0)
        }
        Err(Failure::Usage(message)) => {
            let _ = writeln!(err, "Error: {message}\n{}", usage());
            Some(1)
        }
        Err(Failure::Error(message)) => {
            let _ = writeln!(err, "Error: {message}");
            Some(1)
        }
    }
}

fn resolve(env: &Env, path: &str) -> PathBuf {
    config::paths::normalize(&env.cwd.join(path))
}

fn command(sub: &str, rest: &[String], env: &Env) -> Result<Vec<String>, Failure> {
    let agent_dir = |o: &Options| o.get("agent-dir").map_or_else(|| env.agent_dir(), PathBuf::from);
    match sub {
        "import-pi" => {
            let o = parse_options(rest, &["sessions-root"])?;
            expect_positional(&o.positional, 1, 1)?;
            let root_input = o
                .get("sessions-root")
                .map(str::to_string)
                .or_else(|| (env.var)(config::paths::ENV_SESSION_DIR))
                .unwrap_or_else(|| {
                    env.agent_dir()
                        .join("experimental")
                        .join("sessions")
                        .display()
                        .to_string()
                });
            let sessions_root = config::paths::resolve(&root_input, &env.cwd, &env.home);
            let result = session_core::migration::import_pi_session(&resolve(env, &o.positional[0]), &sessions_root)
                .map_err(|e| Failure::Error(e.to_string()))?;
            Ok(import_lines(&result))
        }
        "export-pi" => {
            let o = parse_options(rest, &[])?;
            expect_positional(&o.positional, 2, 2)?;
            let result = session_core::migration::export_native_session_to_pi(
                &resolve(env, &o.positional[0]),
                &resolve(env, &o.positional[1]),
            )
            .map_err(Failure::Error)?;
            Ok(vec![format!(
                "Exported native session {} ({} entries) to {}",
                result.session_id,
                result.entries,
                result.path.display()
            )])
        }
        "backup" => {
            let o = parse_options(rest, &["agent-dir"])?;
            expect_positional(&o.positional, 0, 1)?;
            let agent = agent_dir(&o);
            let backup_dir = match o.positional.first() {
                Some(dir) => PathBuf::from(dir),
                None => agent.parent().unwrap_or(Path::new("/")).join("backups"),
            };
            let (path, count) = backup_profile(
                &resolve(env, &agent.to_string_lossy()),
                &resolve(env, &backup_dir.to_string_lossy()),
            )?;
            Ok(vec![format!("Backed up {count} profile entries to {}", path.display())])
        }
        "restore" => {
            let o = parse_options(rest, &["agent-dir"])?;
            expect_positional(&o.positional, 1, 1)?;
            let agent = agent_dir(&o);
            let restored = restore_profile(&resolve(env, &o.positional[0]), &resolve(env, &agent.to_string_lossy()))?;
            Ok(vec![format!(
                "Restored {restored} profile entries to {}",
                agent.display()
            )])
        }
        other => Err(Failure::Usage(format!("Unknown migrate command: {other}"))),
    }
}

/// What `migrate import-pi` prints for an import: paths and counts only.
fn import_lines(result: &session_core::migration::ImportResult) -> Vec<String> {
    let entries = |count: usize| if count == 1 { "entry" } else { "entries" };
    let mut lines = vec![
        format!(
            "Imported {} entries as native session {}",
            result.imported, result.session_id
        ),
        format!("  {}", result.path.display()),
    ];
    for (kind, count) in &result.skipped {
        lines.push(format!("  skipped {count} {kind} {}", entries(*count)));
    }
    for (kind, count) in &result.preserved {
        lines.push(format!(
            "  preserved {count} unknown {kind} {} as custom entries",
            entries(*count)
        ));
    }
    if result.branch_copies > 0 {
        lines.push(format!(
            "  applied branch-local context edits per branch with {} copied {}",
            result.branch_copies,
            entries(result.branch_copies)
        ));
    }
    for id in &result.unapplied_context_edits {
        lines.push(format!(
            "  context edit {id} does not target a message on its own path; kept as an entry, not applied"
        ));
    }
    if result.usage_entries > 0 {
        lines.push(format!(
            "  imported usage of {} Pi {} (${}) into usage root {}",
            result.usage_entries,
            entries(result.usage_entries),
            session_file::js::format_f64(result.usage_cost),
            result.usage_root
        ));
    }
    lines
}

fn is_excluded(relative: &str) -> bool {
    let segments: Vec<&str> = relative.split('/').collect();
    if segments.len() == 1 && EXCLUDED_TOP_LEVEL.contains(&segments[0]) {
        return true;
    }
    let name = segments.last().copied().unwrap_or_default();
    name == "sessions" || name.ends_with("-sessions") || name.ends_with(".lock")
}

fn is_inside(parent: &Path, child: &Path) -> bool {
    child.starts_with(parent)
}

#[derive(PartialEq)]
enum Kind {
    File,
    Symlink,
}

fn sorted_names(dir: &Path) -> Result<Vec<String>, Failure> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map_err(|e| io_error(&e, "scandir", dir))?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
    Ok(names)
}

fn collect_profile(agent_dir: &Path, skip: Option<&Path>) -> Result<Vec<(String, Kind)>, Failure> {
    fn walk(root: &Path, rel_dir: &str, skip: Option<&Path>, out: &mut Vec<(String, Kind)>) -> Result<(), Failure> {
        for name in sorted_names(&root.join(rel_dir))? {
            let rel = if rel_dir.is_empty() {
                name
            } else {
                format!("{rel_dir}/{name}")
            };
            let absolute = root.join(&rel);
            if is_excluded(&rel) || skip.is_some_and(|s| is_inside(s, &absolute)) {
                continue;
            }
            let meta = std::fs::symlink_metadata(&absolute).map_err(|e| io_error(&e, "lstat", &absolute))?;
            if meta.file_type().is_symlink() {
                out.push((rel, Kind::Symlink));
            } else if meta.is_dir() {
                walk(root, &rel, skip, out)?;
            } else if meta.is_file() {
                out.push((rel, Kind::File));
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(agent_dir, "", skip, &mut out)?;
    Ok(out)
}

fn sha256(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

fn chmod(path: &Path, mode: u32) -> Result<(), Failure> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(|e| io_error(&e, "chmod", path))
}

fn mkdir_private(path: &Path) -> Result<(), Failure> {
    std::fs::create_dir_all(path).map_err(|e| io_error(&e, "mkdir", path))?;
    chmod(path, 0o700)
}

/// `mkdir(path, { recursive: true, mode: 0o700 })`: missing levels are created 0700 (less the umask).
fn mkdir_recursive(path: &Path) -> Result<(), Failure> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|e| io_error(&e, "mkdir", path))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn write_new(path: &Path, data: &[u8], mode: u32) -> Result<(), Failure> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
        .map_err(|e| io_error(&e, "open", path))?;
    file.write_all(data).map_err(|e| io_error(&e, "write", path))
}

/// Copy the config profile into `<backup_dir>/ultron-profile-<timestamp>/`; returns the path and the entry count.
fn backup_profile(agent_dir: &Path, backup_dir: &Path) -> Result<(PathBuf, usize), Failure> {
    if !std::fs::symlink_metadata(agent_dir).is_ok_and(|m| m.is_dir()) {
        return Err(Failure::Error(format!(
            "Profile directory does not exist: {}",
            agent_dir.display()
        )));
    }
    mkdir_recursive(backup_dir)?;
    let created = session_file::js::iso_string(now_ms());
    let base = format!("ultron-profile-{}", created.replace([':', '.'], "-"));
    let mut target = None;
    for attempt in 0..100 {
        let candidate = backup_dir.join(if attempt == 0 {
            base.clone()
        } else {
            format!("{base}-{attempt}")
        });
        match std::fs::create_dir(&candidate) {
            Ok(()) => {
                chmod(&candidate, 0o700)?;
                target = Some(candidate);
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(io_error(&e, "mkdir", &candidate)),
        }
    }
    let target = target.ok_or_else(|| {
        Failure::Error(format!(
            "Could not create a unique backup directory under {}",
            backup_dir.display()
        ))
    })?;
    let result = (|| -> Result<usize, Failure> {
        let files_root = target.join(FILES_DIR);
        mkdir_private(&files_root)?;
        let skip = is_inside(agent_dir, backup_dir).then_some(backup_dir);
        let mut entries = Vec::new();
        for (rel, kind) in collect_profile(agent_dir, skip)? {
            let source = agent_dir.join(&rel);
            let destination = files_root.join(&rel);
            // `mkdirPrivateTree`: each missing level 0700.
            let mut current = files_root.clone();
            for segment in Path::new(&rel).parent().into_iter().flat_map(Path::components) {
                current.push(segment);
                if !current.exists() {
                    std::fs::create_dir(&current).map_err(|e| io_error(&e, "mkdir", &current))?;
                }
                chmod(&current, 0o700)?;
            }
            if kind == Kind::Symlink {
                let link = std::fs::read_link(&source).map_err(|e| io_error(&e, "readlink", &source))?;
                symlink(&link, &destination).map_err(|e| io_error(&e, "symlink", &destination))?;
                entries.push(json!({"path": rel, "type": "symlink", "target": link.to_string_lossy()}));
                continue;
            }
            let data = std::fs::read(&source).map_err(|e| io_error(&e, "open", &source))?;
            let mode = std::fs::symlink_metadata(&source)
                .map_err(|e| io_error(&e, "lstat", &source))?
                .permissions()
                .mode()
                & 0o777;
            write_new(&destination, &data, 0o600)?;
            chmod(&destination, 0o600)?;
            entries
                .push(json!({"path": rel, "type": "file", "size": data.len(), "sha256": sha256(&data), "mode": mode}));
        }
        let count = entries.len();
        let manifest = json!({
            "format": PROFILE_BACKUP_FORMAT,
            "version": 1,
            "createdAt": created,
            "sourceAgentDir": agent_dir.display().to_string(),
            "entries": entries,
        });
        write_new(
            &target.join(MANIFEST_FILE),
            format!("{}\n", config::json::stringify_pretty(&manifest)).as_bytes(),
            0o600,
        )?;
        Ok(count)
    })();
    match result {
        Ok(count) => Ok((target, count)),
        Err(e) => {
            let _ = std::fs::remove_dir_all(&target);
            Err(e)
        }
    }
}

fn verification_error(problems: &[String]) -> Failure {
    Failure::Error(format!(
        "Profile backup failed verification:\n{}",
        problems
            .iter()
            .map(|p| format!("  - {p}"))
            .collect::<Vec<_>>()
            .join("\n")
    ))
}

fn is_safe_relative_path(path: Option<&Value>) -> bool {
    let Some(path) = path.and_then(Value::as_str) else {
        return false;
    };
    !path.is_empty()
        && !path.contains('\\')
        && !path.contains('\0')
        && !path.starts_with('/')
        && path.split('/').all(|s| !s.is_empty() && s != "." && s != "..")
}

fn list_backup_files(root: &Path, rel_dir: &str, out: &mut Vec<String>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(root.join(rel_dir))? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = if rel_dir.is_empty() {
            name
        } else {
            format!("{rel_dir}/{name}")
        };
        if entry.file_type()?.is_dir() {
            list_backup_files(root, &rel, out)?;
        } else {
            out.push(rel);
        }
    }
    Ok(())
}

/// Verify a backup against its manifest; returns the manifest's entries.
fn verify_profile_backup(root: &Path) -> Result<Vec<Value>, Failure> {
    let manifest_path = root.join(MANIFEST_FILE);
    let manifest: Value = match std::fs::read_to_string(&manifest_path) {
        Ok(text) => {
            serde_json::from_str(&text).map_err(|e| verification_error(&[format!("unreadable manifest: {e}")]))?
        }
        Err(e) => {
            return Err(verification_error(&[format!(
                "unreadable manifest: {}",
                crate::usage::log::io_message(&e, "open", &manifest_path)
            )]));
        }
    };
    if manifest.get("format").and_then(Value::as_str) != Some(PROFILE_BACKUP_FORMAT)
        || manifest.get("version").and_then(Value::as_f64) != Some(1.0)
    {
        return Err(verification_error(&["unsupported manifest format or version".into()]));
    }
    let Some(entries) = manifest.get("entries").and_then(Value::as_array) else {
        return Err(verification_error(&["manifest has no entries list".into()]));
    };
    let files_root = root.join(FILES_DIR);
    let mut problems = Vec::new();
    let mut expected = std::collections::HashSet::new();
    for entry in entries {
        if !is_safe_relative_path(entry.get("path")) {
            let shown = entry
                .get("path")
                .map_or("undefined".to_string(), session_file::js::stringify);
            problems.push(format!("unsafe path in manifest: {shown}"));
            continue;
        }
        let path = entry["path"].as_str().unwrap_or_default();
        expected.insert(path.to_string());
        let absolute = files_root.join(path);
        let Ok(meta) = std::fs::symlink_metadata(&absolute) else {
            problems.push(format!("missing: {path}"));
            continue;
        };
        match entry.get("type").and_then(Value::as_str) {
            Some("symlink") => {
                let target = std::fs::read_link(&absolute).ok();
                if !meta.file_type().is_symlink()
                    || target.map(|t| t.to_string_lossy().into_owned()).as_deref()
                        != entry.get("target").and_then(Value::as_str)
                {
                    problems.push(format!("symlink changed: {path}"));
                }
            }
            Some("file") => {
                if !meta.is_file() {
                    problems.push(format!("not a regular file: {path}"));
                } else {
                    let data = std::fs::read(&absolute).unwrap_or_default();
                    if Some(sha256(&data).as_str()) != entry.get("sha256").and_then(Value::as_str)
                        || Some(meta.len() as f64) != entry.get("size").and_then(Value::as_f64)
                    {
                        problems.push(format!("hash mismatch: {path}"));
                    }
                }
            }
            _ => problems.push(format!("unknown entry type for {path}")),
        }
    }
    let mut present = Vec::new();
    if list_backup_files(&files_root, "", &mut present).is_err() {
        present.clear();
    }
    for path in present {
        if !expected.contains(&path) {
            problems.push(format!("not in manifest: {path}"));
        }
    }
    if !problems.is_empty() {
        return Err(verification_error(&problems));
    }
    Ok(entries.clone())
}

/// Restore a verified backup into `agent_dir`; returns how many entries were written.
fn restore_profile(root: &Path, agent_dir: &Path) -> Result<usize, Failure> {
    let entries = verify_profile_backup(root)?;
    let files_root = root.join(FILES_DIR);
    mkdir_recursive(agent_dir)?;
    let mut restored = 0;
    for entry in &entries {
        let rel = entry["path"].as_str().unwrap_or_default();
        let destination = agent_dir.join(rel);
        if let Some(parent) = destination.parent() {
            mkdir_recursive(parent)?;
        }
        let temporary = PathBuf::from(format!(
            "{}.ultron-restore-{}",
            destination.display(),
            std::process::id()
        ));
        let _ = std::fs::remove_file(&temporary);
        if entry["type"].as_str() == Some("symlink") {
            let target = entry["target"].as_str().unwrap_or_default();
            symlink(target, &temporary).map_err(|e| io_error(&e, "symlink", &temporary))?;
        } else {
            let data = std::fs::read(files_root.join(rel)).map_err(|e| io_error(&e, "open", &files_root.join(rel)))?;
            if Some(sha256(&data).as_str()) != entry["sha256"].as_str() {
                return Err(verification_error(&[format!("hash mismatch: {rel}")]));
            }
            write_new(&temporary, &data, 0o600)?;
            chmod(&temporary, (entry["mode"].as_f64().unwrap_or(0.0) as u32) & 0o777)?;
        }
        if std::fs::symlink_metadata(&destination).is_ok_and(|m| m.is_dir()) {
            let _ = std::fs::remove_file(&temporary);
            return Err(Failure::Error(format!(
                "Cannot restore {rel}: a directory is in the way"
            )));
        }
        std::fs::rename(&temporary, &destination).map_err(|e| io_error(&e, "rename", &temporary))?;
        restored += 1;
    }
    Ok(restored)
}
