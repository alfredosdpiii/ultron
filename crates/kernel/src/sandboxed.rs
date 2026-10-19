//! Launching runtime.py inside the session's sandbox (opt-in through [`crate::KernelOptions::sandbox`]).
//!
//! Without it the kernel process can `open()` and write anywhere the user can, bypassing the host's write checks.
//! With it the kernel starts through the same [`sandbox::Launcher`] policy as the session's bash: the project, the
//! session's extra writable dirs, the snapshot directory and the spill directory writable; the home read-only with
//! credentials hidden; a private /tmp; the environment cleared to the policy's allowlist plus the kernel's own
//! variables. The interpreter's prefix and runtime.py's directory are added read-only so Python can start.
//!
//! The protocol descriptors (fd 3 and fd 4) are inherited: bubblewrap and `unshare` pass inherited descriptors through
//! to the command, so the wire protocol (and host requests over it) works unchanged. Docker cannot pass them into a
//! container and is not used for the kernel: `required` then refuses, `auto` runs the kernel unsandboxed.
//!
//! Both mechanisms tie the sandbox to its parent with `PR_SET_PDEATHSIG`, which fires when the *thread* that spawned
//! it exits. Sandboxed kernels are therefore spawned from one process-lifetime thread, so a kernel started from a
//! short-lived thread (a continuity restart, a blocking-pool worker) lives until the host process dies.

use crate::KernelError;
use crate::config::kernel_environment;
use sandbox::launcher::{Launcher, Mechanism, Policy};
use sandbox::settings::Mode;
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::OnceLock;
use std::sync::mpsc;

/// The mechanism a sandboxed kernel would run under: None runs it unsandboxed (mode off, or `auto` with no usable
/// mechanism); an error when `required` and none can host the kernel.
pub(crate) fn mechanism(launcher: &Launcher) -> Result<Option<Mechanism>, KernelError> {
    match launcher.mechanism() {
        Some(Mechanism::Docker) | None if launcher.mode == Mode::Required => Err(KernelError::new(
            "sandbox.mode is required but no sandbox can host the RLM kernel here (install bubblewrap or allow unprivileged user namespaces; Docker cannot pass the kernel's protocol descriptors)",
        )),
        Some(Mechanism::Docker) | None => Ok(None),
        Some(mechanism) => Ok(Some(mechanism)),
    }
}

/// The kernel's environment inside the sandbox: the policy's allowlist and the base environment's `ULTRON_*` runtime
/// settings, both through the kernel's credential filter, then `fixed` and `extra` (later entries win).
fn environment(
    policy: &Policy,
    base: Vec<(OsString, OsString)>,
    fixed: &[(String, String)],
    extra: &[(String, String)],
) -> Vec<(String, String)> {
    let runtime_settings = base
        .into_iter()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .filter(|(name, _)| name.starts_with("ULTRON_"));
    let filtered = kernel_environment(policy.env.iter().cloned().chain(runtime_settings));
    let mut out: Vec<(String, String)> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for (name, value) in filtered
        .into_iter()
        .chain(fixed.iter().cloned())
        .chain(extra.iter().cloned())
    {
        match index.get(&name) {
            Some(&at) => out[at].1 = value,
            None => {
                index.insert(name.clone(), out.len());
                out.push((name, value));
            }
        }
    }
    out
}

fn push_unique(list: &mut Vec<PathBuf>, path: PathBuf) {
    if !list.contains(&path) {
        list.push(path);
    }
}

/// Read-only paths Python needs: the interpreter's prefix (as named and with symlinks resolved, so a virtualenv and
/// its base interpreter both resolve) and runtime.py's directory (its sibling modules). Never `/`.
fn python_paths(python: &Path, runtime: &Path) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if python.is_absolute() {
        candidates.extend(python.parent().and_then(Path::parent).map(Path::to_path_buf));
        if let Ok(real) = std::fs::canonicalize(python) {
            candidates.extend(real.parent().and_then(Path::parent).map(Path::to_path_buf));
        }
    }
    candidates.extend(
        std::fs::canonicalize(runtime)
            .ok()
            .and_then(|real| real.parent().map(Path::to_path_buf)),
    );
    let mut out = Vec::new();
    for path in candidates
        .into_iter()
        .filter_map(|path| std::fs::canonicalize(path).ok())
    {
        if path != Path::new("/") {
            push_unique(&mut out, path);
        }
    }
    out
}

/// A directory the kernel writes (snapshot or spill dir), created now so it can be bound.
fn writable_dir(dir: &Path) -> Option<PathBuf> {
    if !dir.is_dir() {
        let mut builder = std::fs::DirBuilder::new();
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.recursive(true).create(dir).ok()?;
    }
    std::fs::canonicalize(dir).ok()
}

pub(crate) struct Launch<'a> {
    pub launcher: &'a Launcher,
    pub mechanism: Mechanism,
    pub python: &'a Path,
    pub argv: Vec<OsString>,
    pub cwd: &'a Path,
    pub runtime: &'a Path,
    pub snapshot_path: Option<&'a Path>,
    pub fixed_env: Vec<(String, String)>,
    pub extra_env: &'a [(String, String)],
    /// The kernel's base environment ([`crate::KernelOptions::base_env`]): its `ULTRON_*` runtime settings pass in.
    pub base_env: Vec<(OsString, OsString)>,
}

/// The command starting `argv` (python, runtime.py, flags) under `mechanism` with the session's policy, extended
/// for the kernel. Its own environment is only the launcher's (PATH, DOCKER_HOST); the kernel's goes through the
/// policy.
pub(crate) fn command(launch: Launch<'_>) -> Command {
    let mut policy = launch.launcher.policy.clone();
    policy.env = environment(&policy, launch.base_env, &launch.fixed_env, launch.extra_env);
    let spill = policy
        .env
        .iter()
        .find(|(name, _)| name == "ULTRON_RLM_SPILL_DIR")
        .map(|(_, value)| PathBuf::from(value.trim()))
        .filter(|dir| dir.is_absolute());
    let snapshot_dir = launch.snapshot_path.and_then(Path::parent).map(Path::to_path_buf);
    for dir in snapshot_dir.into_iter().chain(spill) {
        if let Some(dir) = writable_dir(&dir) {
            push_unique(&mut policy.writable, dir);
        }
    }
    // Code skills are imported from their directory directly (writes to it go through the host).
    let skills = policy
        .env
        .iter()
        .find(|(name, _)| name == "ULTRON_CODE_SKILLS_DIR")
        .and_then(|(_, value)| std::fs::canonicalize(value.trim()).ok());
    for path in python_paths(launch.python, launch.runtime).into_iter().chain(skills) {
        if !policy.writable.contains(&path) {
            push_unique(&mut policy.readable, path);
        }
    }
    let argv: Vec<String> = launch
        .argv
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    // The launcher's own environment and PATH are the session's (`Policy::launch`), never this process's.
    policy.command(launch.mechanism, &argv, launch.cwd)
}

type SpawnJob = (Command, mpsc::Sender<std::io::Result<Child>>);

/// Spawns `command` from the process-lifetime spawner thread (see the module docs: the parent-death signal follows
/// the spawning thread). The command, and with it the kernel's ends of the protocol pipes, is dropped there.
pub(crate) fn spawn(command: Command) -> std::io::Result<Child> {
    static SPAWNER: OnceLock<mpsc::Sender<SpawnJob>> = OnceLock::new();
    let spawner = SPAWNER.get_or_init(|| {
        let (jobs, receiver) = mpsc::channel::<SpawnJob>();
        let _ = std::thread::Builder::new()
            .name("rlm-kernel-sandbox-spawner".into())
            .spawn(move || {
                for (mut command, reply) in receiver {
                    let child = command.spawn();
                    drop(command);
                    let _ = reply.send(child);
                }
            });
        jobs
    });
    let unavailable = || std::io::Error::other("the sandboxed kernel spawner thread is not running");
    let (reply, result) = mpsc::channel();
    spawner.send((command, reply)).map_err(|_| unavailable())?;
    result.recv().map_err(|_| unavailable())?
}

/// The host pid of the Python process under the sandbox's `root` (bwrap or the unshare chain): the descendant
/// whose command line starts with `python runtime`. The CPU backstop measures it, not the launcher.
pub(crate) fn python_pid(root: i32, python: &Path, runtime: &Path) -> Option<i32> {
    let python = python.as_os_str().as_encoded_bytes();
    let runtime = runtime.as_os_str().as_encoded_bytes();
    crate::process::descendant_pids(root).into_iter().find(|pid| {
        std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|cmdline| {
            let mut args = cmdline.split(|byte| *byte == 0);
            args.next() == Some(python) && args.next() == Some(runtime)
        })
    })
}
