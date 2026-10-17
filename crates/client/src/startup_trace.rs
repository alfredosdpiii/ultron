//! `traceStartup` (experimental/startup-trace.ts): append one startup milestone to the file named by
//! `ULTRON_STARTUP_TRACE` (every process of a start shares it), as a JSON line `{at, origin, pid, label}`: `at` the
//! wall-clock time in milliseconds, `origin` the process's start (`performance.timeOrigin`). Off unless the variable
//! is set; a failed write is ignored (tracing never affects startup).

use std::io::Write;
use std::path::Path;
use std::sync::OnceLock;

pub const TRACE_ENV: &str = "ULTRON_STARTUP_TRACE";

/// The trace line of `label` at `at` for process `pid` started at `origin` (`JSON.stringify` key order).
pub fn trace_line(at: f64, origin: f64, pid: u32, label: &str) -> String {
    let line = serde_json::json!({"at": at, "origin": origin, "pid": pid, "label": label});
    format!("{line}\n")
}

/// Append the trace line to `path`.
pub fn append(path: &Path, at: f64, origin: f64, pid: u32, label: &str) {
    let file = std::fs::OpenOptions::new().create(true).append(true).open(path);
    if let Ok(mut file) = file {
        let _ = file.write_all(trace_line(at, origin, pid, label).as_bytes());
    }
}

fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64() * 1000.0)
}

/// This process's start in milliseconds since the epoch: `/proc/self/stat` start time (clock ticks after boot,
/// USER_HZ 100) plus `/proc/stat` `btime`; else the first trace's time.
fn origin() -> f64 {
    static ORIGIN: OnceLock<f64> = OnceLock::new();
    *ORIGIN.get_or_init(|| {
        let start = || -> Option<f64> {
            let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
            // Fields after the parenthesized command name; starttime is field 22 overall.
            let rest = &stat[stat.rfind(')')? + 2..];
            let ticks: f64 = rest.split(' ').nth(19)?.parse().ok()?;
            let boot = std::fs::read_to_string("/proc/stat").ok()?;
            let btime: f64 = boot
                .lines()
                .find_map(|l| l.strip_prefix("btime "))?
                .trim()
                .parse()
                .ok()?;
            Some(btime * 1000.0 + ticks * 10.0)
        };
        start().unwrap_or_else(now_ms)
    })
}

/// `traceStartup(label)` with the trace file `path` (`ULTRON_STARTUP_TRACE`; nothing when unset or empty).
pub fn trace_startup(path: Option<&str>, label: &str) {
    let Some(path) = path.filter(|p| !p.is_empty()) else {
        return;
    };
    let origin = origin();
    append(Path::new(path), now_ms().max(origin), origin, std::process::id(), label);
}
