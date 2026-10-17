//! The `claude-code` provider (`packages/ai/src/providers/claude-code.ts`): model calls run through the user's
//! installed, logged-in `claude` CLI in headless mode (the `claude-code-cli` API, [`crate::claude_code_cli`]). It is
//! always registered, after every catalog provider, with five models: Opus 5.5 and Haiku 5.5 by their exact ids,
//! then the `opus`/`sonnet`/`haiku` aliases the CLI resolves itself. Its models answer only to their full
//! `claude-code/<id>` reference ([`crate::model_resolver::is_claude_code_model`]) and never start a session on
//! their own (`findInitialModel` skips them).
//!
//! Auth is the CLI's own login: Ultron never reads, copies or mints Claude credentials. The provider counts as
//! configured (its models are available) when a `claude` executable is present ([`find_cli`]); a request always
//! resolves (with no key), so an explicit `claude-code/...` choice reaches the stream, which explains a missing CLI
//! or login. It has no remote catalog (`withRemoteCatalog` skips it).
//!
//! Evidence: `fixtures/generate-claude-code-provider.mjs` records the TypeScript runtime with and without a fake
//! `claude`; `tests/claude_code_provider.rs` replays it.

use crate::types::Model;
use indexmap::IndexMap;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

pub const PROVIDER: &str = "claude-code";
pub const API: &str = "claude-code-cli";
/// The provider's display name (`createProvider({ name })`).
pub const NAME: &str = "Claude Code (CLI)";
/// The name of its key-less auth (`claudeCodeCliAuth().name`, the `/login` method).
pub const AUTH_NAME: &str = "Claude Code CLI";
/// Where its configured status comes from (`checkAuth`'s `source`).
pub const AUTH_SOURCE: &str = "Claude Code CLI (its own login)";
/// What a request of this provider carries as its key: there is none (the CLI authenticates itself), but Rust
/// request paths require one where `getAuth` would resolve, so this marker stands in for TypeScript's `{ auth: {} }`.
/// The `claude-code-cli` API never reads it.
pub const REQUEST_KEY: &str = "<claude-code-cli>";

/// Pi thinking levels as `claude --effort` levels (`EFFORT_LEVELS`); "off" disables extended thinking.
const EFFORT_LEVELS: [(&str, &str); 6] = [
    ("minimal", "low"),
    ("low", "low"),
    ("medium", "medium"),
    ("high", "high"),
    ("xhigh", "xhigh"),
    ("max", "max"),
];

fn alias_model(id: &str, name: &str) -> Model {
    let effort: serde_json::Map<String, serde_json::Value> =
        EFFORT_LEVELS.iter().map(|(k, v)| (k.to_string(), json!(v))).collect();
    serde_json::from_value(json!({
        "id": id,
        "name": name,
        "api": API,
        "provider": PROVIDER,
        "baseUrl": "claude-code://cli",
        "reasoning": true,
        "thinkingLevelMap": effort,
        "input": ["text", "image"],
        // Subscription usage has no per-token price here; each response carries the CLI's own reported cost.
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
        "contextWindow": 200_000,
        "maxTokens": 32_000,
    }))
    .expect("claude-code models deserialize")
}

/// `CLAUDE_CODE_MODELS`, by id in catalog order.
pub static MODELS: LazyLock<IndexMap<String, Model>> = LazyLock::new(|| {
    [
        ("claude-opus-5-5", "Claude Opus 5.5 (Claude Code CLI)"),
        ("claude-haiku-5-5", "Claude Haiku 5.5 (Claude Code CLI)"),
        ("opus", "Claude Opus (Claude Code CLI)"),
        ("sonnet", "Claude Sonnet (Claude Code CLI)"),
        ("haiku", "Claude Haiku (Claude Code CLI)"),
    ]
    .into_iter()
    .map(|(id, name)| (id.to_string(), alias_model(id, name)))
    .collect()
});

/// `pathEntries`: `;`-separated when the value has a `;` and no `:/` (a Windows PATH), else `:`-separated.
fn path_entries(value: &str) -> impl Iterator<Item = &str> {
    let separator = if value.contains(';') && !value.contains(":/") {
        ';'
    } else {
        ':'
    };
    value.split(separator).filter(|entry| !entry.is_empty())
}

/// The auth context's `env`: a value that is empty or only whitespace counts as unset.
fn env_value(lookup: &dyn Fn(&str) -> Option<String>, name: &str) -> Option<String> {
    lookup(name).filter(|value| !value.trim().is_empty())
}

/// The auth context's `fileExists` (`fs.access` after expanding a leading `~` to the home directory).
fn file_exists(path: &str, exists: &dyn Fn(&Path) -> bool, home: Option<&Path>) -> bool {
    match (path.strip_prefix('~'), home) {
        (Some(rest), Some(home)) => exists(&PathBuf::from(format!("{}{rest}", home.display()))),
        _ => exists(Path::new(path)),
    }
}

/// `findCli`: the CLI the provider would use, without running it: `ULTRON_CLAUDE_CODE_BIN` when set (and only if
/// that file exists), else the first `claude`, `claude.exe` or `claude.cmd` in a PATH directory. Existence only:
/// the stream itself checks that it is executable and explains when it is not.
pub fn find_cli(source: &crate::env::EnvSource<'_>) -> Option<String> {
    let home = source.home.as_deref();
    if let Some(configured) = env_value(source.lookup, crate::claude_code_cli::CLAUDE_CODE_BIN_ENV) {
        let configured = configured.trim().to_string();
        return file_exists(&configured, source.exists, home).then_some(configured);
    }
    let path = env_value(source.lookup, "PATH").unwrap_or_default();
    for dir in path_entries(&path) {
        let dir = dir.trim_end_matches(['/', '\\']);
        for name in ["claude", "claude.exe", "claude.cmd"] {
            let candidate = format!("{dir}/{name}");
            if file_exists(&candidate, source.exists, home) {
                return Some(candidate);
            }
        }
    }
    None
}

/// `claudeCodeCliAuth().check`: whether the provider is configured (its models are available).
pub fn configured(source: &crate::env::EnvSource<'_>) -> bool {
    find_cli(source).is_some()
}
