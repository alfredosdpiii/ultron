//! Whole-tree memory cap for a kernel: the kernel plus every process its cells spawn (tree-memory.ts).
//!
//! - `Cgroup`: the kernel starts inside a transient systemd user scope (`systemd-run --user --scope -p MemoryMax=…
//!   -p MemorySwapMax=0`), so the kernel's memory controller enforces the cap; the host reads the scope's
//!   `memory.events` to report an OOM kill clearly. Needs no root, only a user manager with the memory controller
//!   delegated (probed once per launcher program by starting a tiny scope and reading back its `memory.max`).
//! - `Watchdog`: the host sums the proportional anonymous and shared memory of the kernel's descendant tree from
//!   /proc (Pss_Anon + Pss_Shmem + SwapPss; RssAnon + RssShmem + VmSwap where PSS is unavailable) and kills the tree
//!   when it is over the cap, polling every 500 ms, 250 ms above 40% of the cap and 100 ms above 70%.
//! - `Off`: no tree cap (the cap is 0, or not Linux).
//!
//! RLIMIT_DATA per process stays in force in every mode.

use crate::KernelError;
use crate::config::is_js_space;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;
/// The memory cap of the probe scope (64 MiB), read back from its `memory.max`.
const PROBE_BYTES: u64 = 64 * 1024 * 1024;
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeMemoryBackend {
    Cgroup,
    Watchdog,
    Off,
}

impl TreeMemoryBackend {
    pub fn name(self) -> &'static str {
        match self {
            TreeMemoryBackend::Cgroup => "cgroup",
            TreeMemoryBackend::Watchdog => "watchdog",
            TreeMemoryBackend::Off => "off",
        }
    }
}

/// `kernelTreeMemoryLimit`: the override, else `ULTRON_RLM_MAX_TREE_MEMORY_MB` when it is all digits, else twice the
/// per-process limit; 0 disables. A value past 2^53 - 1 is an error, as in TypeScript.
pub fn kernel_tree_memory_limit(
    env_value: Option<&str>,
    per_process_mb: u64,
    override_mb: Option<u64>,
) -> Result<u64, KernelError> {
    let error = || KernelError::new("RLM maxTreeMemoryMb must be a non-negative integer");
    let fallback = per_process_mb.checked_mul(2).ok_or_else(error)?;
    let value = match override_mb {
        Some(value) => value,
        None => match env_value.map(|text| text.trim_matches(is_js_space)) {
            Some(text) if !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()) => {
                text.parse::<u64>().unwrap_or(u64::MAX)
            }
            _ => fallback,
        },
    };
    if value > MAX_SAFE_INTEGER {
        return Err(error());
    }
    Ok(value)
}

/// `treeMemoryBackend`: `requested` (ULTRON_RLM_TREE_MEMORY_BACKEND: auto|cgroup|watchdog|off) for a cap; "cgroup",
/// "auto" and anything unrecognized take a capped scope when `cgroup_available` says this system allows one, else
/// the watchdog.
pub fn tree_memory_backend(
    cap_mb: u64,
    requested: Option<&str>,
    cgroup_available: impl FnOnce() -> bool,
) -> TreeMemoryBackend {
    if cap_mb == 0 || !cfg!(target_os = "linux") {
        return TreeMemoryBackend::Off;
    }
    let choice = requested
        .map(|text| text.trim_matches(is_js_space).to_lowercase())
        .unwrap_or_default();
    match choice.as_str() {
        "off" => TreeMemoryBackend::Off,
        "watchdog" => TreeMemoryBackend::Watchdog,
        _ if cgroup_available() => TreeMemoryBackend::Cgroup,
        _ => TreeMemoryBackend::Watchdog,
    }
}

static PROBES: LazyLock<Mutex<HashMap<OsString, bool>>> = LazyLock::new(Mutex::default);

/// `cgroupScopeAvailable`: whether `program` (systemd-run) can start a transient user scope with a memory cap here.
/// Probed once per program by starting a tiny scope and reading back its `memory.max`, which checks the launcher,
/// the user manager and memory-controller delegation together.
pub fn cgroup_scope_available(program: &OsStr) -> bool {
    if !cfg!(target_os = "linux") {
        return false;
    }
    let mut probes = PROBES.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(known) = probes.get(program) {
        return *known;
    }
    let available = probe(program);
    probes.insert(program.to_os_string(), available);
    available
}

fn probe(program: &OsStr) -> bool {
    let mut args: Vec<String> = cgroup_scope_command(PROBE_BYTES / (1024 * 1024))[1..].to_vec();
    args.extend([
        "/bin/sh".into(),
        "-c".into(),
        r#"cat "/sys/fs/cgroup$(cut -d: -f3 /proc/self/cgroup)/memory.max""#.into(),
    ]);
    let Ok(mut child) = Command::new(program)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
    match child.wait_with_output() {
        Ok(output) => {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).trim_matches(is_js_space) == PROBE_BYTES.to_string()
        }
        Err(_) => false,
    }
}

/// `cgroupScopeCommand`: the command prefix that starts the kernel inside a capped transient scope; the first item
/// is the launcher (`systemd-run`), which execs the command after `--` itself.
pub fn cgroup_scope_command(cap_mb: u64) -> Vec<String> {
    [
        "systemd-run",
        "--user",
        "--scope",
        "--quiet",
        "--collect",
        "-p",
        &format!("MemoryMax={cap_mb}M"),
        "-p",
        "MemorySwapMax=0",
        // The host sees the OOM kill and stops the tree itself with a clear error, instead of systemd stopping the
        // scope with SIGTERM.
        "-p",
        "OOMPolicy=continue",
        "--",
    ]
    .map(String::from)
    .to_vec()
}

/// `treeMemoryLimitMessage`: the error a kernel stopped by the tree cap reports.
pub fn tree_memory_limit_message(cap_mb: u64, backend: TreeMemoryBackend) -> String {
    let how = if backend == TreeMemoryBackend::Cgroup {
        "cgroup MemoryMax"
    } else {
        "host watchdog"
    };
    format!(
        "RLM kernel process tree exceeded its memory limit ({cap_mb} MiB total across the kernel and every process its cells started, ULTRON_RLM_MAX_TREE_MEMORY_MB, enforced by {how}) and was stopped; the next cell starts a fresh kernel without earlier Python state"
    )
}

/// `processCgroupDir`: the cgroup v2 directory of `pid` (from its `0::` line), or none.
pub fn process_cgroup_dir(pid: i32) -> Option<PathBuf> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    cgroup_dir_of(&text)
}

/// Whether `dir` may be treated as the kernel's own scope: it is not this host process's cgroup nor one of its
/// ancestors. A kernel that was never moved into a scope of its own (a launcher that only execs, or the moment before
/// systemd-run moves it) still sits in the host's cgroup, and killing "every process in it" would kill the host and
/// the terminal it runs in (on 2026-10-15 a test under a fake systemd-run killed the user's Ghostty surface this way).
pub fn is_foreign_scope(dir: &Path) -> bool {
    let Some(own) = process_cgroup_dir(std::process::id() as i32) else {
        return false;
    };
    dir.to_string_lossy().ends_with(".scope") && !own.starts_with(dir)
}

/// The cgroup v2 directory named by a `/proc/<pid>/cgroup` text.
pub fn cgroup_dir_of(text: &str) -> Option<PathBuf> {
    let line = text.split('\n').find(|line| line.starts_with("0::"))?;
    Some(PathBuf::from(format!("/sys/fs/cgroup{}", &line[3..])))
}

/// `cgroupOomKills`: OOM kills recorded in a cgroup's `memory.events` (0 without the line), or none when the cgroup
/// is gone.
pub fn cgroup_oom_kills(dir: &Path) -> Option<u64> {
    let text = std::fs::read_to_string(dir.join("memory.events")).ok()?;
    Some(
        text.lines()
            .find_map(|line| line.strip_prefix("oom_kill "))
            .and_then(|count| {
                count
                    .bytes()
                    .all(|byte| byte.is_ascii_digit())
                    .then(|| count.parse().ok())
                    .flatten()
            })
            .unwrap_or(0),
    )
}

/// `cgroupPids`: pids listed in a cgroup's `cgroup.procs` and its child cgroups, or none when it is gone.
pub fn cgroup_pids(dir: &Path) -> Vec<i32> {
    let mut pids = Vec::new();
    collect_cgroup_pids(dir, &mut pids);
    pids
}

fn collect_cgroup_pids(dir: &Path, pids: &mut Vec<i32>) {
    let Ok(text) = std::fs::read_to_string(dir.join("cgroup.procs")) else {
        return;
    };
    pids.extend(
        text.split('\n')
            .filter(|line| !line.is_empty() && line.bytes().all(|byte| byte.is_ascii_digit()))
            .filter_map(|line| line.parse::<i32>().ok()),
    );
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            collect_cgroup_pids(&entry.path(), pids);
        }
    }
}

/// Sum of the `kB` values of `fields` in a /proc status or smaps_rollup text, in bytes.
fn sum_kib(text: &str, fields: &[&str]) -> u64 {
    let kib: u64 = text
        .lines()
        .filter_map(|line| {
            let (name, rest) = line.split_once(':')?;
            if !fields.contains(&name) {
                return None;
            }
            let value = rest
                .strip_prefix([' ', '\t'])?
                .trim_start_matches([' ', '\t'])
                .strip_suffix(" kB")?;
            (!value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
                .then(|| value.parse::<u64>().ok())
                .flatten()
        })
        .sum();
    kib * 1024
}

/// `processPrivateRssBytes` of a `/proc/<pid>/status` text: RssAnon + RssShmem + VmSwap.
pub fn private_rss_bytes(status: &str) -> u64 {
    sum_kib(status, &["RssAnon", "RssShmem", "VmSwap"])
}

/// `processMemoryBytes` of a `/proc/<pid>/smaps_rollup` text: Pss_Anon + Pss_Shmem + SwapPss, where a page shared by
/// N processes counts 1/N in each; none when the rollup has no Pss_Anon (before Linux 5.9: fall back to the status).
pub fn proportional_bytes(rollup: &str) -> Option<u64> {
    if !rollup.lines().any(|line| line.starts_with("Pss_Anon:")) {
        return None;
    }
    Some(sum_kib(rollup, &["Pss_Anon", "Pss_Shmem", "SwapPss"]))
}

/// `processMemoryBytes`: one process's proportional memory in bytes; 0 when it is gone or a kernel thread.
pub fn process_memory_bytes(pid: i32) -> u64 {
    let status = || {
        std::fs::read_to_string(format!("/proc/{pid}/status"))
            .map(|text| private_rss_bytes(&text))
            .unwrap_or(0)
    };
    match std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup")) {
        Err(_) => status(),
        Ok(rollup) => match proportional_bytes(&rollup) {
            Some(bytes) => bytes,
            None if rollup.trim().is_empty() => 0,
            None => status(),
        },
    }
}

/// `treeMemoryUsage`: memory of `root` and its live descendant tree, with the descendants seen.
pub fn tree_memory_usage(root: i32) -> (u64, Vec<i32>) {
    let descendants = crate::process::descendant_pids(root);
    let bytes = process_memory_bytes(root) + descendants.iter().map(|pid| process_memory_bytes(*pid)).sum::<u64>();
    (bytes, descendants)
}

/// `watchdogPollMs`: the watchdog's next poll for a tree at `bytes` of a `cap_bytes` cap, faster near the cap.
pub fn watchdog_poll_ms(bytes: u64, cap_bytes: u64) -> u64 {
    let (bytes, cap) = (bytes as f64, cap_bytes as f64);
    if bytes >= cap * 0.7 {
        100
    } else if bytes >= cap * 0.4 {
        250
    } else {
        500
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The host's own cgroup (where a kernel that was never moved into its own scope sits) and its ancestors are
    /// never treated as the kernel's scope, so a tree kill skips them; a sibling scope is. Regression for 2026-10-15,
    /// when a kernel under a fake systemd-run killed the user's Ghostty surface. Deliberately signals nothing: a
    /// regressed guard must fail this assertion, not kill the desktop.
    #[test]
    fn kernel_tree_memory_never_treats_the_hosts_cgroup_as_the_kernels() {
        let Some(own) = process_cgroup_dir(std::process::id() as i32) else {
            return;
        };
        assert!(!is_foreign_scope(&own));
        for ancestor in own.ancestors() {
            assert!(!is_foreign_scope(ancestor), "{}", ancestor.display());
        }
        assert!(is_foreign_scope(&own.with_file_name("run-u-kernel.scope")));
        assert!(!is_foreign_scope(&own.with_file_name("run-u-kernel.slice")));
    }

    #[test]
    fn kernel_tree_memory_text_parsers() {
        let rollup = "Rss:  9000 kB\nPss_Anon:   100 kB\nPss_File: 7 kB\nPss_Shmem: 20 kB\nSwapPss:  3 kB\n";
        assert_eq!(proportional_bytes(rollup), Some(123 * 1024));
        assert_eq!(proportional_bytes("Rss: 10 kB\n"), None);
        let status = "Name:\tpython3\nRssAnon:\t   40 kB\nRssFile:\t 99 kB\nRssShmem:\t 2 kB\nVmSwap:\t 1 kB\n";
        assert_eq!(private_rss_bytes(status), 43 * 1024);
        assert_eq!(
            cgroup_dir_of("1:name=x:/a\n0::/user.slice/run-u1.scope\n"),
            Some(PathBuf::from("/sys/fs/cgroup/user.slice/run-u1.scope"))
        );
        assert_eq!(cgroup_dir_of("1:cpu:/a\n"), None);
    }
}
