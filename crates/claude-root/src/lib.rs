//! Claude Code as the root agent of Ultron (packages/coding-agent/src/ultron/claude): `ultron claude` (the
//! launcher), `ultron mcp` (Ultron's REPL as the one MCP server Claude Code talks to, exposing exactly the `rlm`
//! tool), `ultron mcp --bridge` (a `--claude` lane's tools), `ultron hook` (Claude Code hook commands),
//! `ultron watch` and `ultron guide`, plus the `--claude` root-model selection of the session worker.
//!
//! RLM-first: Claude Code sees one tool, `mcp__ultron__rlm`; every capability is a kernel API reached from a cell.

pub mod bridge;
pub mod checkpoints;
pub mod child;
pub mod child_usage;
pub mod claude_cli;
pub mod cli;
pub mod control;
pub mod durable_apis;
pub mod external_lane;
pub mod external_root;
pub mod guide;
pub mod herdr_hook;
pub mod hooks;
pub mod launcher;
pub mod mcp_protocol;
pub mod mcp_server;
pub mod persona_hook;
pub mod root_sink;
pub mod schedules;
pub mod self_cmd;
pub mod settings;
pub mod unavailable;
pub mod watch;
pub mod worker_root;

use session_core::RunEnv;
use std::path::{Path, PathBuf};

/// A usage error: printed without a stack, exit code 2.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsageError(pub String);

/// What a command sees: the process environment and locations.
pub struct Ctx {
    pub env: RunEnv,
}

impl Ctx {
    /// The bundled skills directory (`<package>/skills`), unless `ULTRON_BUNDLED_SKILLS` turns it off.
    pub fn bundled_skills(&self) -> Option<PathBuf> {
        guide::bundled_skills_enabled((self.env.var)("ULTRON_BUNDLED_SKILLS").as_deref())
            .then(|| config::paths::normalize(&self.env.docs.join("../skills")))
    }
}

/// `n` random bytes as lowercase hex.
pub fn random_hex(n: usize) -> String {
    let mut bytes = vec![0u8; n];
    let _ = getrandom::fill(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `os.tmpdir()`: `TMPDIR` without a trailing slash, else `/tmp`.
pub fn tmpdir(var: &dyn Fn(&str) -> Option<String>) -> PathBuf {
    match var("TMPDIR").filter(|v| !v.is_empty()) {
        Some(dir) if dir.len() > 1 => PathBuf::from(dir.trim_end_matches('/')),
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from("/tmp"),
    }
}

/// `mkdtempSync(join(tmpdir(), prefix))`: a new private directory.
pub fn temp_dir(prefix: &str, var: &dyn Fn(&str) -> Option<String>) -> Result<PathBuf, String> {
    use std::os::unix::fs::DirBuilderExt;
    let base = tmpdir(var);
    for _ in 0..16 {
        let path = base.join(format!("{prefix}{}", random_hex(3)));
        match std::fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("{}: {e}", path.display())),
        }
    }
    Err(format!("could not create a temp directory in {}", base.display()))
}

/// Write `content` to a new owner-only file.
pub fn write_private(path: &Path, content: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(content.as_bytes())
}
