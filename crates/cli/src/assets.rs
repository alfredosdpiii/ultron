//! The package files the binary needs at run time, embedded at build time (`build.rs`) so a release binary runs
//! without the checkout: the Python runtime (`src/ultron/rlm/runtime.py`, its sibling modules,
//! `secret-patterns.json`), the bundled skills, the docs and `CHANGELOG.md`. Python imports the runtime's modules
//! from its directory and the model reads skills and docs by path, so they are installed as real files.
//!
//! Where the files come from (`ULTRON_DEV_ASSETS`):
//! - unset or empty: the package directory `PI_PACKAGE_DIR` names (`getPackageDir`'s override, for Nix/Guix store
//!   paths; it must hold `src/ultron/rlm/runtime.py`); else a debug build reads the live checkout it was built from
//!   when that checkout still has `runtime.py`; otherwise (every release build) the embedded files.
//! - `0`: the embedded files.
//! - `1`: the build checkout (debug builds only; a release binary records no build path).
//! - any other value: that package directory (one holding `docs/`, `skills/` and `src/ultron/rlm/runtime.py`).
//!
//! The embedded files are installed once into `<agent dir>/runtime/<version>-<hash>/` (the agent dir honours
//! `ULTRON_CODING_AGENT_DIR`), where `<hash>` is the SHA-256 of every path, mode and byte. Installation writes a
//! private temporary directory beside it and renames it into place, so a reader never sees a partial directory and
//! concurrent first runs agree on one winner. A directory that verifies (its marker names this hash and every file
//! has its embedded length) is never rewritten; a damaged one is moved aside and replaced.
//!
//! The files are installed lazily: `main` only computes the path, and [`Source::ready`] runs where the files are first
//! needed (a session, its worker, `--list-models`, autoreview, the Claude Code commands). A process using an install
//! holds a shared `flock` on its `.ultron-assets.lock` for its lifetime; `ready` prunes every other install (any
//! version) whose lock it can take exclusively, directories moved aside, and staging directories of crashed installs
//! (lock free and older than [`STAGING_GRACE`]). An installer holds its staging directory's lock exclusively until
//! the rename, then keeps it shared.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// One embedded file: its path inside the package directory, its mode and its bytes.
pub struct Asset {
    pub path: &'static str,
    pub mode: u32,
    pub bytes: &'static [u8],
}

mod generated {
    use super::Asset;
    include!(concat!(env!("OUT_DIR"), "/assets.rs"));
}

pub use generated::{CHECKOUT, FILES, HASH};

/// The environment variable choosing where the package files come from.
pub const DEV_ASSETS: &str = "ULTRON_DEV_ASSETS";
/// `PI_PACKAGE_DIR`: the package directory to read instead of the embedded files (when `ULTRON_DEV_ASSETS` is unset).
pub const PACKAGE_DIR: &str = "PI_PACKAGE_DIR";
/// The file in an installed directory that names the hash it was installed from; written last.
pub const MARKER: &str = ".ultron-assets";
/// The runtime script, relative to the package directory.
pub const RUNTIME: &str = "src/ultron/rlm/runtime.py";

/// Where this run's package files come from: a live directory, or the embedded files installed under a base.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    Live(PathBuf),
    Embedded(PathBuf),
}

/// The source `ULTRON_DEV_ASSETS` selects (see the module docs). Touches nothing on disk but the checks.
pub fn source(var: &dyn Fn(&str) -> Option<String>, home: &Path) -> Result<Source, String> {
    let checkout = || CHECKOUT.map(PathBuf::from).filter(|dir| dir.join(RUNTIME).is_file());
    let embedded = || Source::Embedded(config::paths::agent_dir(var, home).join("runtime"));
    let live = |name: &str, dir: &str| {
        let dir = config::paths::normalize(&config::paths::expand_tilde(dir, home));
        match dir.join(RUNTIME).is_file() {
            true => Ok(Source::Live(dir)),
            false => Err(format!("{name}={}: no {RUNTIME} there", dir.display())),
        }
    };
    match var(DEV_ASSETS).as_deref().unwrap_or("") {
        // `getPackageDir()`: `PI_PACKAGE_DIR` names the package directory (Nix/Guix store paths).
        "" => match var(PACKAGE_DIR).filter(|dir| !dir.is_empty()) {
            // `getRlmRuntimePath`'s error when the directory has no runtime.
            Some(dir) => live(PACKAGE_DIR, &dir).map_err(|_| "Ultron RLM runtime.py is not installed".to_string()),
            None => Ok(checkout().map_or_else(embedded, Source::Live)),
        },
        "0" => Ok(embedded()),
        "1" => checkout().map(Source::Live).ok_or_else(|| match CHECKOUT {
            Some(dir) => format!("{DEV_ASSETS}=1: the build checkout {dir} has no {RUNTIME}"),
            None => format!("{DEV_ASSETS}=1 needs a debug build; set {DEV_ASSETS} to a package directory instead"),
        }),
        dir => live(DEV_ASSETS, dir),
    }
}

impl Source {
    /// The package directory, whether or not it is installed yet.
    pub fn dir(&self) -> PathBuf {
        match self {
            Source::Live(dir) => dir.clone(),
            Source::Embedded(base) => install_dir(base),
        }
    }

    /// The package directory, ready to read: embedded files are installed if needed and held in use (a shared lock
    /// for the rest of this process) so no other process prunes them; unused older installs are pruned.
    pub fn ready(&self) -> Result<PathBuf, String> {
        match self {
            Source::Live(dir) => Ok(dir.clone()),
            Source::Embedded(base) => use_installed(base),
        }
    }
}

/// The package directory this run reads, ready: `docs/`, `skills/`, `src/ultron/rlm/` and `CHANGELOG.md` live under it.
pub fn package_dir(var: &dyn Fn(&str) -> Option<String>, home: &Path) -> Result<PathBuf, String> {
    source(var, home)?.ready()
}

/// The directory the embedded files are installed into under `base`.
pub fn install_dir(base: &Path) -> PathBuf {
    base.join(format!("{}-{}", crate::version(), &HASH[..16]))
}

/// Whether `dir` holds this binary's files: the marker names [`HASH`] and every file has its embedded length.
pub fn verified(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join(MARKER)).is_ok_and(|marker| marker.trim_end() == HASH)
        && FILES.iter().all(|asset| {
            std::fs::symlink_metadata(dir.join(asset.path))
                .is_ok_and(|meta| meta.is_file() && meta.len() == asset.bytes.len() as u64)
        })
}

/// Install the embedded files under `base` unless they already are; returns the installed directory. It is not
/// held in use; [`Source::ready`] is what a run calls.
pub fn install(base: &Path) -> Result<PathBuf, String> {
    install_inner(base).map(|(dir, _)| dir)
}

/// Files held in use by this process: their shared locks last as long as it does.
static HELD: Mutex<Vec<File>> = Mutex::new(Vec::new());

/// Install if needed, take the shared lock, prune. A pruner that took the directory between the install and the
/// lock leaves this process holding a lock file no longer at the path; it then installs again.
fn use_installed(base: &Path) -> Result<PathBuf, String> {
    for _ in 0..8 {
        let (target, installing) = install_inner(base)?;
        let lock = match installing {
            Some(lock) => lock,
            None => open_lock(&target, true)
                .map_err(|e| format!("Cannot use Ultron's runtime files in {}: {e}", target.display()))?,
        };
        // The installer's exclusive lock becomes shared here; otherwise this waits out a pruner.
        lock.lock_shared()
            .map_err(|e| format!("Cannot lock Ultron's runtime files in {}: {e}", target.display()))?;
        if same_file(&lock, &target.join(LOCK)) && verified(&target) {
            HELD.lock().unwrap_or_else(|e| e.into_inner()).push(lock);
            prune(base, &target);
            return Ok(target);
        }
    }
    Err(format!(
        "Cannot use Ultron's runtime files in {}: they keep being replaced",
        install_dir(base).display()
    ))
}

/// The installed directory and, when this call installed it, the exclusive lock its installer still holds.
fn install_inner(base: &Path) -> Result<(PathBuf, Option<File>), String> {
    let target = install_dir(base);
    if verified(&target) {
        return Ok((target, None));
    }
    let failed = |what: &str, path: &Path, error: std::io::Error| {
        format!(
            "Cannot install Ultron's runtime files ({what} {}): {error}",
            path.display()
        )
    };
    std::fs::create_dir_all(base).map_err(|e| failed("create", base, e))?;
    let name = target.file_name().unwrap_or_default().to_string_lossy().to_string();
    // A rename that loses to a damaged directory (another process is replacing it) retries; a few rounds settle it.
    for _ in 0..8 {
        let staging = unique(base, &format!("{STAGING}{name}"));
        let lock = match write_tree(&staging) {
            Ok(lock) => lock,
            Err(error) => {
                let _ = std::fs::remove_dir_all(&staging);
                return Err(failed("write", &staging, error));
            }
        };
        match std::fs::rename(&staging, &target) {
            Ok(()) => return Ok((target, Some(lock))),
            Err(_) if verified(&target) => {
                // Another process installed the same files first; keep its directory.
                let _ = std::fs::remove_dir_all(&staging);
                return Ok((target, None));
            }
            Err(_) => {
                let _ = std::fs::remove_dir_all(&staging);
                // Damaged (an interrupted copy by hand, a deleted file): move it aside, then try again. A process
                // still using it keeps it until it exits; a later prune removes it.
                let aside = unique(base, &format!("{STALE}{name}"));
                if std::fs::rename(&target, &aside).is_ok() {
                    remove_if_unused(&aside, false);
                }
            }
        }
    }
    if verified(&target) {
        return Ok((target, None));
    }
    Err(format!(
        "Cannot install Ultron's runtime files into {}: it keeps failing verification",
        target.display()
    ))
}

/// The lock file each installed directory carries; a process using the directory holds it shared.
pub const LOCK: &str = ".ultron-assets.lock";
/// Name prefixes under the base: a directory being written, and one moved aside to be removed.
pub const STAGING: &str = ".tmp-";
pub const STALE: &str = ".stale-";
/// A staging directory whose lock is free is a crashed install's once it is this old (its installer holds the lock
/// from the moment the lock file exists, so the age only covers the instant before that).
pub const STAGING_GRACE: std::time::Duration = std::time::Duration::from_secs(60);

fn open_lock(dir: &Path, create: bool) -> std::io::Result<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .truncate(false)
        .open(dir.join(LOCK))
}

fn same_file(file: &File, path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (file.metadata(), std::fs::metadata(path)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

/// Remove what nobody uses under `base` besides `keep`: other installs (any version) whose lock can be taken
/// exclusively, directories moved aside, and staging directories of crashed installs. Errors leave things as they are.
pub fn prune(base: &Path, keep: &Path) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if path == keep || !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        if name.starts_with(STAGING) {
            let old = entry
                .metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|time| time.elapsed().is_ok_and(|age| age >= STAGING_GRACE));
            if old {
                remove_if_unused(&path, false);
            }
        } else if name.starts_with(STALE) {
            remove_if_unused(&path, false);
        } else if !name.starts_with('.') && path.join(MARKER).is_file() {
            remove_if_unused(&path, true);
        }
    }
}

/// Remove `dir` when its lock (if any; `install` creates one) can be taken exclusively. An install is renamed aside
/// first, under the lock, so a process about to use it finds it gone rather than half-deleted.
fn remove_if_unused(dir: &Path, install: bool) {
    let lock = match open_lock(dir, install) {
        Ok(lock) => match lock.try_lock() {
            Ok(()) => Some(lock),
            Err(_) => return,
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !install => None,
        Err(_) => return,
    };
    let target = match install {
        true => {
            let name = dir.file_name().unwrap_or_default().to_string_lossy().to_string();
            let aside = unique(dir.parent().unwrap_or(dir), &format!("{STALE}{name}"));
            if std::fs::rename(dir, &aside).is_err() {
                return;
            }
            aside
        }
        false => dir.to_path_buf(),
    };
    let _ = std::fs::remove_dir_all(&target);
    drop(lock);
}

/// A path under `base` no other process or thread picks: `<prefix>-<pid>-<counter>-<nanos>`.
fn unique(base: &Path, prefix: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    base.join(format!(
        "{prefix}-{}-{}-{nanos}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Create `dir` with its lock file held exclusively (so no prune takes it), write every embedded file, then the
/// marker. Returns the held lock.
fn write_tree(dir: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    std::fs::DirBuilder::new().mode(0o755).create(dir)?;
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(dir.join(LOCK))?;
    lock.lock()?;
    for asset in FILES {
        let path = dir.join(asset.path);
        if let Some(parent) = path.parent() {
            std::fs::DirBuilder::new().recursive(true).mode(0o755).create(parent)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(asset.mode)
            .open(&path)?;
        file.write_all(asset.bytes)?;
        file.sync_all()?;
    }
    let mut marker = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(dir.join(MARKER))?;
    marker.write_all(format!("{HASH}\n").as_bytes())?;
    marker.sync_all()?;
    File::open(dir)?.sync_all()?;
    Ok(lock)
}
