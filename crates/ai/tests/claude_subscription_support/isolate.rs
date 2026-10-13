//! A sealed process environment for every test binary that touches claude-subscription, so no test (and no
//! mutation of the code under test) can reach a real `claude` CLI, a real login or a real API key.
//!
//! Edition 2024 makes `std::env::set_var` unsafe and the workspace forbids `unsafe`, so the binary cannot clean its
//! own environment. Instead the first [`seal`] call re-executes this test binary, with the same arguments, under a
//! cleared environment and exits with its status; inside it [`seal`] only verifies the seal. Every test calls
//! [`seal`] first, so nothing runs in the unsealed parent.
//!
//! The sealed environment:
//! - PATH is a private `bin` holding a sentinel `claude` (exits 97 at once), then `/usr/bin:/bin` (python, sh).
//!   The sentinel comes first, so resolving `claude` never falls through to the HOME-relative or system install
//!   prefixes `process::resolve_claude` probes after PATH (`~/.local/bin`, `~/.npm-global/bin`, `~/.bun/bin`, ...).
//! - HOME, CLAUDE_CONFIG_DIR, the XDG dirs, TMPDIR, ULTRON_CODING_AGENT_DIR and ULTRON_SERVER_DIR are private
//!   directories under one temp root (removed afterwards).
//! - Nothing else survives except a few harmless runtime variables ([`KEEP`]): no CLAUDE_CODE_OAUTH_TOKEN, no
//!   ANTHROPIC_*, no proxies, no ULTRON_CLAUDE_COMMAND.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Once;

/// Set in the sealed child: its private root.
pub const SEALED: &str = "ULTRON_TEST_SEALED_ROOT";
/// Exit status of the sentinel `claude`.
pub const SENTINEL_EXIT: i32 = 97;
const KEEP: &[&str] = &[
    "ULTRON_PYTHON",
    "RUST_BACKTRACE",
    "RUST_LIB_BACKTRACE",
    "RUST_TEST_THREADS",
    "RUST_MIN_STACK",
    "LANG",
    "LC_ALL",
    "TERM",
    "NO_COLOR",
];
const XDG: &[&str] = &[
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_STATE_HOME",
    "XDG_RUNTIME_DIR",
];

fn private_dir(path: &Path) -> PathBuf {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .unwrap();
    path.to_path_buf()
}

/// The sealed environment rooted at `root`.
fn sealed_env(root: &Path) -> Vec<(String, String)> {
    let bin = private_dir(&root.join("bin"));
    let sentinel = bin.join("claude");
    std::fs::write(
        &sentinel,
        format!("#!/bin/sh\necho 'sealed test environment: no real claude here' >&2\nexit {SENTINEL_EXIT}\n"),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&sentinel, std::fs::Permissions::from_mode(0o755)).unwrap();
    let dir = |name: &str| private_dir(&root.join(name)).display().to_string();
    let mut env = vec![
        (SEALED.to_string(), root.display().to_string()),
        ("PATH".to_string(), format!("{}:/usr/bin:/bin", bin.display())),
        ("HOME".to_string(), dir("home")),
        ("CLAUDE_CONFIG_DIR".to_string(), dir("claude-config")),
        ("TMPDIR".to_string(), dir("tmp")),
        ("ULTRON_CODING_AGENT_DIR".to_string(), dir("agent")),
        ("ULTRON_SERVER_DIR".to_string(), dir("server")),
    ];
    for name in XDG {
        env.push((name.to_string(), dir(&name.to_lowercase())));
    }
    for name in KEEP {
        if let Ok(value) = std::env::var(name) {
            env.push((name.to_string(), value));
        }
    }
    env
}

/// Run this test binary sealed (once per process); inside the sealed run, check the seal.
pub fn seal() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        if std::env::var_os(SEALED).is_some() {
            assert_sealed();
            return;
        }
        let root = std::env::temp_dir().join(format!("ultron-sealed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let env = sealed_env(&private_dir(&root));
        let status = Command::new(std::env::current_exe().unwrap())
            .args(std::env::args_os().skip(1))
            .env_clear()
            .envs(env)
            .status()
            .expect("the sealed test binary starts");
        let _ = std::fs::remove_dir_all(&root);
        std::process::exit(status.code().unwrap_or(101));
    });
}

/// The process environment is the sealed one, and `claude` resolves to the sentinel.
fn assert_sealed() {
    let root = PathBuf::from(std::env::var(SEALED).unwrap());
    let path = std::env::var("PATH").unwrap();
    assert_eq!(path, format!("{}:/usr/bin:/bin", root.join("bin").display()));
    for name in [
        "HOME",
        "CLAUDE_CONFIG_DIR",
        "TMPDIR",
        "ULTRON_CODING_AGENT_DIR",
        "ULTRON_SERVER_DIR",
    ]
    .iter()
    .chain(XDG)
    {
        assert!(Path::new(&std::env::var(name).unwrap()).starts_with(&root), "{name}");
    }
    for (name, _) in std::env::vars_os() {
        let name = name.to_string_lossy();
        assert!(
            !name.starts_with("ANTHROPIC_") && !name.starts_with("CLAUDE_CODE_") && name != "ULTRON_CLAUDE_COMMAND",
            "{name} leaked into the sealed environment"
        );
    }
    let env: ai::claude_subscription::Env = std::env::vars().collect();
    let resolved = ai::claude_subscription::process::resolve_claude(None, &env).expect("the sentinel resolves");
    assert_eq!(resolved, [root.join("bin/claude").display().to_string()]);
}
