//! Linux process inspection and the kernel's process-tree kill (tree-memory.ts `killProcessTree`, `descendantPids`).

use rustix::process::{Pid, Signal, kill_process, kill_process_group};
use std::collections::HashMap;
use std::time::Duration;

pub(crate) struct ProcStat {
    pub state: String,
    pub ppid: i32,
    /// Start time in clock ticks since boot: tells a live process apart from a later one reusing its pid.
    pub start_time: u64,
}

/// Fields after the parenthesized command name of /proc/<pid>/stat: state(3), ppid(4), ...
fn stat_fields(pid: i32) -> Option<Vec<String>> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = stat.get(stat.rfind(')')? + 2..)?;
    Some(rest.split(' ').map(str::to_string).collect())
}

/// Parent, state and start time of one process (`processStat`), or none when it is gone.
pub(crate) fn process_stat(pid: i32) -> Option<ProcStat> {
    let fields = stat_fields(pid)?;
    Some(ProcStat {
        state: fields.first()?.clone(),
        ppid: fields.get(1)?.parse().ok()?,
        start_time: fields.get(19)?.parse().ok()?,
    })
}

/// CPU seconds (user + system) of one Linux process, or None when unavailable.
pub(crate) fn process_cpu_seconds(pid: i32) -> Option<f64> {
    let fields = stat_fields(pid)?;
    let user: f64 = fields.get(11)?.parse().ok()?;
    let system: f64 = fields.get(12)?.parse().ok()?;
    Some((user + system) / 100.0)
}

/// Live descendants of `root`, breadth first (zombies are skipped: they cannot be signalled).
pub(crate) fn descendant_pids(root: i32) -> Vec<i32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
    for entry in entries.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|name| name.parse::<i32>().ok()) else {
            continue;
        };
        let Some(stat) = process_stat(pid) else {
            continue;
        };
        if stat.state == "Z" || stat.state == "X" {
            continue;
        }
        children.entry(stat.ppid).or_default().push(pid);
    }
    let mut result = Vec::new();
    let mut queue = std::collections::VecDeque::from([root]);
    while let Some(parent) = queue.pop_front() {
        for &child in children.get(&parent).map(Vec::as_slice).unwrap_or_default() {
            result.push(child);
            queue.push_back(child);
        }
    }
    result
}

fn signal(pid: i32, signal: Signal) {
    if let Some(pid) = Pid::from_raw(pid) {
        let _ = kill_process(pid, signal);
    }
}

/// What else a tree kill reaches (`KillTreeOptions` of tree-memory.ts).
#[derive(Default)]
pub(crate) struct KillExtras<'a> {
    /// Processes seen in the tree earlier (pid to start time), killed when still the same process.
    pub known: Option<&'a HashMap<i32, u64>>,
    /// The kernel's cgroup scope: every process still in it is killed.
    pub cgroup_dir: Option<&'a std::path::Path>,
}

/// SIGKILLs `root` and every process in its tree. The root is stopped first so it can neither start new processes
/// nor exit while its descendants are killed; as a child subreaper it adopts their orphans, which later sweeps kill.
/// `root_alive`: the root is still the host's unreaped child, so its pid cannot belong to another process. Processes
/// seen in earlier watchdog scans and the members of the kernel's scope are killed too.
pub(crate) fn kill_process_tree(root: i32, owns_process_group: bool, root_alive: bool, extras: KillExtras<'_>) {
    if root_alive {
        signal(root, Signal::STOP);
        for sweep in 0..20 {
            let pids = descendant_pids(root);
            if pids.is_empty() {
                break;
            }
            for pid in pids {
                signal(pid, Signal::KILL);
            }
            std::thread::sleep(Duration::from_millis(if sweep < 5 { 2 } else { 10 }));
        }
    }
    if owns_process_group && let Some(group) = Pid::from_raw(root) {
        let _ = kill_process_group(group, Signal::KILL);
    }
    if root_alive {
        signal(root, Signal::KILL);
    }
    for (&pid, &start_time) in extras.known.into_iter().flatten() {
        if process_stat(pid).is_some_and(|stat| stat.start_time == start_time && stat.state != "Z") {
            signal(pid, Signal::KILL);
        }
    }
    // Only a scope of the kernel's own: never the host's cgroup or an ancestor of it (see `is_foreign_scope`).
    if let Some(dir) = extras
        .cgroup_dir
        .filter(|dir| crate::tree_memory::is_foreign_scope(dir))
    {
        let own = std::process::id() as i32;
        for pid in crate::tree_memory::cgroup_pids(dir) {
            if pid != own {
                signal(pid, Signal::KILL);
            }
        }
    }
}

/// Node's name for a signal number, as kernel.ts reports it.
pub(crate) fn signal_name(number: i32) -> String {
    const NAMES: &[&str] = &[
        "SIGHUP",
        "SIGINT",
        "SIGQUIT",
        "SIGILL",
        "SIGTRAP",
        "SIGABRT",
        "SIGBUS",
        "SIGFPE",
        "SIGKILL",
        "SIGUSR1",
        "SIGSEGV",
        "SIGUSR2",
        "SIGPIPE",
        "SIGALRM",
        "SIGTERM",
        "SIGSTKFLT",
        "SIGCHLD",
        "SIGCONT",
        "SIGSTOP",
        "SIGTSTP",
        "SIGTTIN",
        "SIGTTOU",
        "SIGURG",
        "SIGXCPU",
        "SIGXFSZ",
        "SIGVTALRM",
        "SIGPROF",
        "SIGWINCH",
        "SIGIO",
        "SIGPWR",
        "SIGSYS",
    ];
    usize::try_from(number - 1)
        .ok()
        .and_then(|index| NAMES.get(index))
        .map_or_else(|| number.to_string(), |name| (*name).to_string())
}
