//! The TUI's debug environment variables (packages/tui): `PI_TUI_WRITE_LOG` (every byte written to the terminal,
//! appended to a file; a directory gets `tui-<local time>-<pid>.log`, terminal.ts:149-163), `PI_TUI_DEBUG_REDRAW=1`
//! (one line per full redraw of the main-screen renderer in `<log dir>/pi-tui-debug.log`, tui-main-screen.ts:321-328)
//! and `PI_TUI_DEBUG=1` (a dump of each differential render in `/tmp/tui/render-<ms>-<random>.log`,
//! tui-main-screen.ts:569-596). Files are written by a background thread: nothing on the paint path waits on disk.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::mpsc::{Sender, channel};
use std::thread::JoinHandle;

/// Where `PI_TUI_DEBUG` dumps go (TS hardcodes it).
pub const RENDER_DUMP_DIR: &str = "/tmp/tui";
/// The redraw log's name in the log directory.
pub const REDRAW_LOG_NAME: &str = "pi-tui-debug.log";

/// Local calendar time (`Date#getFullYear()` .. `getSeconds()`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalStamp {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

/// `writeLogPath`: the file `PI_TUI_WRITE_LOG` names, or in an existing directory `tui-YYYY-MM-DD_HH-MM-SS-<pid>.log`.
pub fn write_log_path(value: &str, is_dir: bool, at: LocalStamp, pid: u32) -> Option<PathBuf> {
    if value.is_empty() {
        return None;
    }
    if !is_dir {
        return Some(PathBuf::from(value));
    }
    let LocalStamp {
        year,
        month,
        day,
        hour,
        minute,
        second,
    } = at;
    Some(Path::new(value).join(format!(
        "tui-{year}-{month:02}-{day:02}_{hour:02}-{minute:02}-{second:02}-{pid}.log"
    )))
}

/// `Date#toISOString()` of `ms` since the epoch.
pub fn iso_time(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // civil_from_days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// `Number#toString(36)` of a non-negative integer.
fn base36(mut n: u64) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    loop {
        out.push(DIGITS[(n % 36) as usize]);
        n /= 36;
        if n == 0 {
            break;
        }
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

/// `render-${Date.now()}-${Math.random().toString(36).slice(2)}.log`: `nonce` stands in for the random digits.
pub fn render_dump_name(ms: i64, nonce: u64) -> String {
    format!("render-{ms}-{}.log", base36(nonce))
}

/// One line of the redraw log: `[<ISO time>] fullRender: <reason> (prev=N, new=M, height=H)`.
pub fn redraw_line(iso: &str, entry: &str) -> String {
    format!("[{iso}] {entry}\n")
}

enum FileOp {
    Append(PathBuf, String),
    Create(PathBuf, String),
}

/// Appends and writes files on its own thread, in order; dropping it waits for what was sent.
pub struct FileWriter {
    tx: Mutex<Option<Sender<FileOp>>>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Default for FileWriter {
    fn default() -> Self {
        let (tx, rx) = channel::<FileOp>();
        let thread = std::thread::spawn(move || {
            for write in rx {
                // Debug logging never affects the TUI: errors are ignored, as in TS.
                let (path, data, append) = match write {
                    FileOp::Append(path, data) => (path, data, true),
                    FileOp::Create(path, data) => (path, data, false),
                };
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(append)
                    .write(true)
                    .truncate(!append)
                    .open(&path);
                if let Ok(mut file) = file {
                    let _ = file.write_all(data.as_bytes());
                }
            }
        });
        FileWriter {
            tx: Mutex::new(Some(tx)),
            thread: Mutex::new(Some(thread)),
        }
    }
}

impl FileWriter {
    fn send(&self, write: FileOp) {
        if let Some(tx) = self.tx.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            let _ = tx.send(write);
        }
    }

    /// Wait until everything sent so far is on disk; later sends are dropped.
    pub fn finish(&self) {
        self.tx.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(thread) = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = thread.join();
        }
    }
}

impl Drop for FileWriter {
    fn drop(&mut self) {
        self.finish();
    }
}

/// The debug outputs the environment turned on, with the clock they stamp entries with.
pub struct RenderDebug {
    /// `PI_TUI_WRITE_LOG`.
    pub write_log: Option<PathBuf>,
    /// `PI_TUI_DEBUG_REDRAW=1`: the TUI's log directory (the agent directory).
    pub redraw_dir: Option<PathBuf>,
    /// `PI_TUI_DEBUG=1`: the dump directory ([`RENDER_DUMP_DIR`]).
    pub dump_dir: Option<PathBuf>,
    /// Milliseconds since the epoch.
    pub clock: Box<dyn Fn() -> i64 + Send + Sync>,
    nonce: std::sync::atomic::AtomicU64,
    files: FileWriter,
}

impl RenderDebug {
    /// What `var` turns on: `PI_TUI_WRITE_LOG` (resolved by the caller to [`write_log_path`]),
    /// `PI_TUI_DEBUG_REDRAW` under `log_dir`, `PI_TUI_DEBUG` under `dump_dir`. `None` when nothing is on.
    pub fn from_env(
        var: &dyn Fn(&str) -> Option<String>,
        write_log: Option<PathBuf>,
        log_dir: &Path,
        dump_dir: &Path,
        clock: Box<dyn Fn() -> i64 + Send + Sync>,
    ) -> Option<RenderDebug> {
        let on = |name: &str| var(name).as_deref() == Some("1");
        let debug = RenderDebug {
            write_log,
            redraw_dir: on("PI_TUI_DEBUG_REDRAW").then(|| log_dir.to_path_buf()),
            dump_dir: on("PI_TUI_DEBUG").then(|| dump_dir.to_path_buf()),
            clock,
            nonce: std::sync::atomic::AtomicU64::new(0),
            files: FileWriter::default(),
        };
        (debug.write_log.is_some() || debug.redraw_dir.is_some() || debug.dump_dir.is_some()).then_some(debug)
    }

    /// The renderer collects redraw reasons and dumps only when they are wanted.
    pub fn traces_renders(&self) -> bool {
        self.redraw_dir.is_some() || self.dump_dir.is_some()
    }

    /// Bytes written to the terminal.
    pub fn wrote(&self, data: &str) {
        if let Some(path) = &self.write_log {
            self.files.send(FileOp::Append(path.clone(), data.to_string()));
        }
    }

    /// One full redraw (`fullRender: ...`).
    pub fn redraw(&self, entry: &str) {
        if let Some(dir) = &self.redraw_dir {
            let line = redraw_line(&iso_time((self.clock)()), entry);
            self.files.send(FileOp::Append(dir.join(REDRAW_LOG_NAME), line));
        }
    }

    /// One differential render's dump.
    pub fn dump(&self, text: &str) {
        if let Some(dir) = &self.dump_dir {
            let ms = (self.clock)();
            let seed = (ms as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
            let n = self.nonce.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let name = render_dump_name(ms, seed.wrapping_add(n.wrapping_mul(0xbf58_476d_1ce4_e5b9)));
            self.files.send(FileOp::Create(dir.join(name), text.to_string()));
        }
    }

    /// Wait until every entry is written.
    pub fn finish(&self) {
        self.files.finish();
    }
}

/// The values a differential render's dump reports (tui-main-screen.ts:569-596), in viewport rows.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderDump {
    pub first_changed: usize,
    pub viewport_top: usize,
    pub cursor_row: usize,
    pub height: usize,
    pub line_diff: i64,
    pub hardware_cursor_row: usize,
    pub render_end: usize,
    pub final_cursor_row: usize,
    pub cursor_pos: Option<(usize, usize)>,
    pub new_lines: Vec<String>,
    pub previous_lines: Vec<String>,
    /// UTF-16 length of the bytes written.
    pub written: usize,
}

impl RenderDump {
    /// The dump file's text.
    pub fn text(&self) -> String {
        let pretty = |lines: &[String]| serde_json::to_string_pretty(lines).unwrap_or_default();
        let cursor = match self.cursor_pos {
            Some((row, col)) => format!("{{\"row\":{row},\"col\":{col}}}"),
            None => "null".into(),
        };
        [
            format!("firstChanged: {}", self.first_changed),
            format!("viewportTop: {}", self.viewport_top),
            format!("cursorRow: {}", self.cursor_row),
            format!("height: {}", self.height),
            format!("lineDiff: {}", self.line_diff),
            format!("hardwareCursorRow: {}", self.hardware_cursor_row),
            format!("renderEnd: {}", self.render_end),
            format!("finalCursorRow: {}", self.final_cursor_row),
            format!("cursorPos: {cursor}"),
            format!("newLines.length: {}", self.new_lines.len()),
            format!("previousLines.length: {}", self.previous_lines.len()),
            String::new(),
            "=== newLines ===".into(),
            pretty(&self.new_lines),
            String::new(),
            "=== previousLines ===".into(),
            pretty(&self.previous_lines),
            String::new(),
            "=== buffer ===".into(),
            format!("[{} chars written in bounded chunks]", self.written),
        ]
        .join("\n")
    }
}
