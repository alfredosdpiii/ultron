//! `ultron memory import --from claude-code|codex [--dry-run]`: Claude Code's and Codex's memory notes become typed
//! memory records in the shared Hindsight memory (kernel_apis::memory_import), each tagged with its source file and
//! checked for secrets as `memory.propose` checks a write. `--dry-run` prints the plan and writes nothing.

use crate::Env;
use kernel_apis::memory_import::{self, Action, Harness, Planned};
use kernel_apis::memory_records::RecordIndex;
use kernel_apis::{FileDocumentStore, MemoryBackend, MemoryService, worker_gate};
use serde_json::{Map, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const USAGE: &str = "Usage: ultron memory import --from claude-code|codex [--dry-run]\n\nImports the memory notes another harness keeps as typed Ultron memory records:\n  claude-code  every ~/.claude/projects/<project>/memory/*.md note (CLAUDE_CONFIG_DIR moves ~/.claude)\n  codex        ~/.codex/memories/memory_summary.md and MEMORY.md (CODEX_HOME moves ~/.codex)\nEach record keeps its source file; a note holding a secret is refused. Running it again replaces changed notes\nand leaves unchanged ones. --dry-run prints what would be imported and writes nothing.\n";

/// Builds the memory backend of one scope group (`None`: global notes; `Some`: a project's).
pub type BackendFor<'a> = &'a dyn Fn(Option<&Path>) -> Result<Arc<dyn MemoryBackend>, String>;

fn scope_tags(env: &Env, project: Option<&Path>) -> Map<String, Value> {
    let cwd = project.unwrap_or(&env.cwd);
    hindsight::worker_scope_tags("import", &cwd.display().to_string(), &env.home.display().to_string())
}

pub fn run(argv: &[String], env: &Env, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32, String> {
    let agent_dir = env.agent_dir();
    let endpoint = || hindsight::Endpoint::from_env(&|key| (env.var)(key), &env.cwd, &agent_dir);
    let backend = |project: Option<&Path>| -> Result<Arc<dyn MemoryBackend>, String> {
        let endpoint = endpoint().ok_or("Hindsight is not configured. Set ULTRON_HINDSIGHT_URL.")?;
        let mut options = hindsight::HindsightOptions::new(&endpoint.url, &endpoint.bank, scope_tags(env, project));
        options.ensure_bank = true;
        Ok(Arc::new(
            hindsight::HindsightBackend::new(options).map_err(|e| e.to_string())?,
        ))
    };
    run_with(argv, env, &backend, out, err)
}

/// [`run`] over `backend` (tests inject one).
pub fn run_with(
    argv: &[String],
    env: &Env,
    backend: BackendFor<'_>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, String> {
    let io = |e: std::io::Error| e.to_string();
    let usage = |err: &mut dyn Write, message: &str| -> Result<i32, String> {
        writeln!(err, "Error: {message}").map_err(io)?;
        err.write_all(USAGE.as_bytes()).map_err(io)?;
        Ok(1)
    };
    match argv.first().map(String::as_str) {
        Some("import") => {}
        Some("--help" | "-h") | None => {
            out.write_all(USAGE.as_bytes()).map_err(io)?;
            return Ok(if argv.is_empty() { 1 } else { 0 });
        }
        Some(other) => return usage(err, &format!("unknown memory command: {other}")),
    }
    let mut from = None;
    let mut dry_run = false;
    let mut rest = argv[1..].iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--dry-run" => dry_run = true,
            "--from" => from = rest.next().cloned(),
            other if other.starts_with("--from=") => from = Some(other["--from=".len()..].to_string()),
            "--help" | "-h" => {
                out.write_all(USAGE.as_bytes()).map_err(io)?;
                return Ok(0);
            }
            other => return usage(err, &format!("unknown option: {other}")),
        }
    }
    let Some(harness) = from.as_deref().and_then(Harness::parse) else {
        return usage(err, "--from must be claude-code or codex");
    };
    let root = memory_import::notes_root(harness, &env.home, &|key| (env.var)(key));
    let sources = memory_import::discover(harness, &root);
    let shown = memory_import::home_relative(&root, &env.home);
    if sources.is_empty() {
        writeln!(err, "Error: no {} memory notes found in {shown}", harness.label()).map_err(io)?;
        return Ok(1);
    }
    let agent_dir = env.agent_dir();
    let index = Arc::new(RecordIndex::for_agent_dir(&agent_dir));
    let planned = memory_import::plan(&sources, &env.home, &index, &|project| scope_tags(env, project))?;
    let count = |wanted: fn(&Action) -> bool| planned.iter().filter(|p| wanted(&p.action)).count();
    if dry_run {
        writeln!(
            out,
            "Dry run: {} notes from {} ({shown}); nothing was written.",
            planned.len(),
            harness.label()
        )
        .map_err(io)?;
        for item in &planned {
            let verb = match &item.action {
                Action::Add => "import".to_string(),
                Action::Replace(id) => format!("replace {id}"),
                Action::Unchanged(id) => format!("unchanged {id}"),
                Action::Refuse(reason) => format!("refuse ({reason})"),
            };
            writeln!(out, "  {verb}: {}", describe(item, &env.home)).map_err(io)?;
        }
        writeln!(
            out,
            "{} to import, {} to replace, {} unchanged, {} refused.",
            count(|a| matches!(a, Action::Add)),
            count(|a| matches!(a, Action::Replace(_))),
            count(|a| matches!(a, Action::Unchanged(_))),
            count(|a| matches!(a, Action::Refuse(_)))
        )
        .map_err(io)?;
        return Ok(0);
    }
    let journal = agent_dir.join("memory").join("import-journal.json");
    let mut service: Option<(Option<PathBuf>, MemoryService)> = None;
    let (mut imported, mut replaced, mut unchanged, mut refused, mut failed) = (0, 0, 0, 0, 0);
    for item in &planned {
        match &item.action {
            Action::Unchanged(_) => {
                unchanged += 1;
                continue;
            }
            Action::Refuse(reason) => {
                refused += 1;
                writeln!(out, "  refused ({reason}): {}", describe(item, &env.home)).map_err(io)?;
                continue;
            }
            Action::Add | Action::Replace(_) => {}
        }
        // One service per scope group, over the one import journal (each loads what the previous one wrote).
        if service.as_ref().is_none_or(|(project, _)| *project != item.project) {
            let memory = MemoryService::new(
                Arc::new(FileDocumentStore::new(&journal)),
                backend(item.project.as_deref())?,
                worker_gate(),
            )
            .map_err(|e| e.to_string())?
            .with_records(index.clone());
            service = Some((item.project.clone(), memory));
        }
        let Some((_, memory)) = &service else {
            continue;
        };
        match memory_import::import_one(memory, item) {
            Ok(op) if op["state"] == "sensitive" || op["state"] == "skipped" => {
                refused += 1;
                writeln!(
                    out,
                    "  refused (holds a secret or personal identifier): {}",
                    describe(item, &env.home)
                )
                .map_err(io)?;
            }
            Ok(op) if op["phase"] == "backend" => {
                if matches!(item.action, Action::Replace(_)) {
                    replaced += 1;
                } else {
                    imported += 1;
                }
                let id = op["memoryId"].as_str().unwrap_or_default();
                writeln!(out, "  stored {id}: {}", describe(item, &env.home)).map_err(io)?;
            }
            Ok(op) => {
                failed += 1;
                writeln!(
                    err,
                    "  not stored ({}): {}",
                    op["state"].as_str().unwrap_or("?"),
                    describe(item, &env.home)
                )
                .map_err(io)?;
            }
            Err(error) => {
                failed += 1;
                writeln!(err, "  failed ({error}): {}", describe(item, &env.home)).map_err(io)?;
            }
        }
    }
    writeln!(
        out,
        "Imported {imported}, replaced {replaced}, unchanged {unchanged}, refused {refused}, failed {failed} ({} notes from {}).",
        planned.len(),
        harness.label()
    )
    .map_err(io)?;
    Ok(if failed > 0 { 1 } else { 0 })
}

/// `<type> <key> (<scope>[ <project>]) from <source>`.
fn describe(item: &Planned, home: &Path) -> String {
    let scope = match &item.project {
        Some(project) => format!("project {}", memory_import::home_relative(project, home)),
        None => item.scope.to_string(),
    };
    format!(
        "{} {} ({scope}) from {}",
        item.typed.kind,
        item.typed.key.as_deref().unwrap_or(""),
        item.typed.source.as_deref().unwrap_or("")
    )
}
