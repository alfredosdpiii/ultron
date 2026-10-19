//! Process terminal: raw mode and size through crossterm, escape-sequence writes, and an input pump that feeds stdin
//! bytes through [`StdinBuffer`] (port of `ProcessTerminal` in packages/tui/src/terminal.ts; the Kitty keyboard
//! protocol negotiation is [`crate::keyboard_protocol`], whose replies the run loop reads from the input; on Windows
//! the console input mode gains `ENABLE_VIRTUAL_TERMINAL_INPUT`; no progress keepalive).

use std::io::{self, Read, Write};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use crate::keyboard_protocol::KeyboardProtocol;
use crate::stdin_buffer::{InputEvent, StdinBuffer};

/// Escape output of the terminal, over any writer.
pub struct Terminal<W: Write> {
    pub out: W,
    /// The keyboard protocol negotiation ([`Terminal::start_keyboard_protocol`]); [`Terminal::stop`] pops it.
    pub keyboard: KeyboardProtocol,
    was_raw: bool,
    started: bool,
}

impl<W: Write> Terminal<W> {
    pub fn new(out: W) -> Self {
        Self {
            out,
            keyboard: KeyboardProtocol::new(),
            was_raw: false,
            started: false,
        }
    }

    pub fn write(&mut self, data: &str) -> io::Result<()> {
        self.out.write_all(data.as_bytes())?;
        self.out.flush()
    }

    /// Enter raw mode (when `raw`) and enable bracketed paste.
    pub fn start(&mut self, raw: bool) -> io::Result<()> {
        if raw {
            self.was_raw = crossterm::terminal::is_raw_mode_enabled()?;
            crossterm::terminal::enable_raw_mode()?;
            // After raw mode, which resets the console mode flags (terminal.ts:194-198).
            enable_windows_vt_input();
        }
        self.started = raw;
        self.write("\x1b[?2004h")
    }

    /// `queryAndEnableKittyProtocol` (the end of `start`): push and query the Kitty flags, with the DA sentinel.
    pub fn start_keyboard_protocol(&mut self) -> io::Result<()> {
        let query = self.keyboard.query();
        self.write(query)
    }

    /// Disable bracketed paste, pop the Kitty flags and reset modifyOtherKeys when they were set, and restore the
    /// previous raw-mode state.
    pub fn stop(&mut self) -> io::Result<()> {
        self.write("\x1b[?2004l")?;
        let keyboard = self.keyboard.stop();
        if !keyboard.is_empty() {
            self.write(&keyboard)?;
        }
        if std::mem::take(&mut self.started) && !self.was_raw {
            crossterm::terminal::disable_raw_mode()?;
        }
        Ok(())
    }

    pub fn hide_cursor(&mut self) -> io::Result<()> {
        self.write("\x1b[?25l")
    }

    pub fn show_cursor(&mut self) -> io::Result<()> {
        self.write("\x1b[?25h")
    }
}

/// `ENABLE_VIRTUAL_TERMINAL_INPUT`: the console sends VT sequences for modified keys (`\x1b[Z` for Shift+Tab).
pub const ENABLE_VIRTUAL_TERMINAL_INPUT: u32 = 0x0200;

/// The console input mode `mode` with VT input on.
pub fn vt_input_mode(mode: u32) -> u32 {
    mode | ENABLE_VIRTUAL_TERMINAL_INPUT
}

/// `enableWindowsVTInput` (terminal.ts:369-382): add VT input to the stdin console handle; without it Shift+Tab
/// arrives as plain `\t`. A failure is ignored (Shift+Tab is then not distinguishable from Tab).
#[cfg(windows)]
fn enable_windows_vt_input() {
    use crossterm_winapi::{ConsoleMode, Handle};
    if let Ok(handle) = Handle::input_handle() {
        let console = ConsoleMode::from(handle);
        if let Ok(mode) = console.mode() {
            let _ = console.set_mode(vt_input_mode(mode));
        }
    }
}

#[cfg(not(windows))]
fn enable_windows_vt_input() {}

/// Terminal (columns, rows): the tty size, else $COLUMNS/$LINES, else 80x24.
pub fn size() -> (u16, u16) {
    let env = |k: &str, d: u16| {
        std::env::var(k)
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|v| *v > 0)
            .unwrap_or(d)
    };
    match crossterm::terminal::size() {
        Ok((c, r)) if c > 0 && r > 0 => (c, r),
        _ => (env("COLUMNS", 80), env("LINES", 24)),
    }
}

/// Input as the TS terminal delivers it: one sequence per event, pastes re-wrapped in bracketed-paste markers.
fn deliver(events: Vec<InputEvent>, tx: &mpsc::Sender<String>) -> bool {
    events.into_iter().all(|e| {
        let data = match e {
            InputEvent::Data(s) => s,
            InputEvent::Paste(p) => format!("\x1b[200~{p}\x1b[201~"),
        };
        tx.send(data).is_ok()
    })
}

/// `resolveEscapeTimeoutMs(env)`: how long a lone ESC waits for the rest of an escape sequence before it is the
/// Escape key. `PI_TUI_ESC_TIMEOUT` (milliseconds, a positive number), else 100 ms over SSH (`SSH_CONNECTION` or
/// `SSH_TTY` set: legacy Alt+key is ESC plus a byte, and high-latency links split them), else 10 ms.
pub fn resolve_escape_timeout(env: &dyn Fn(&str) -> Option<String>) -> Duration {
    let configured = js_number(env("PI_TUI_ESC_TIMEOUT").as_deref());
    if configured.is_finite() && configured > 0.0 {
        return Duration::from_secs_f64(configured / 1000.0);
    }
    let set = |name: &str| env(name).is_some_and(|v| !v.is_empty());
    if set("SSH_CONNECTION") || set("SSH_TTY") {
        return Duration::from_millis(100);
    }
    Duration::from_millis(10)
}

/// JavaScript's `Number(value)` for an environment value: `undefined` is NaN, blank is 0, `0x`/`0o`/`0b` integers
/// and decimals (with exponent) parse, anything else is NaN.
fn js_number(value: Option<&str>) -> f64 {
    let Some(value) = value else { return f64::NAN };
    let text = value.trim();
    if text.is_empty() {
        return 0.0;
    }
    for (prefix, radix) in [("0x", 16), ("0o", 8), ("0b", 2)] {
        if let Some(digits) = text
            .get(..2)
            .filter(|p| p.eq_ignore_ascii_case(prefix))
            .map(|_| &text[2..])
        {
            return u64::from_str_radix(digits, radix).map_or(f64::NAN, |n| n as f64);
        }
    }
    match text.trim_start_matches(['+', '-']) {
        "Infinity" => {
            if text.starts_with('-') {
                f64::NEG_INFINITY
            } else {
                f64::INFINITY
            }
        }
        rest if rest
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-')) =>
        {
            text.parse().unwrap_or(f64::NAN)
        }
        _ => f64::NAN,
    }
}

/// Reads `input` on a thread, splits it with [`StdinBuffer`] (flushing partial sequences after its timeout, a lone
/// ESC after [`resolve_escape_timeout`] of this process's environment, as `ProcessTerminal` reads `process.env`) and
/// sends each input sequence. Ends at EOF or when the receiver is dropped.
pub fn spawn_input_pump<R: Read + Send + 'static>(mut input: R) -> Receiver<String> {
    let (bytes_tx, bytes_rx) = mpsc::channel::<Vec<u8>>();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = input.read(&mut buf) {
            if n == 0 || bytes_tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    thread::spawn(move || {
        let mut sb = StdinBuffer::new().with_escape_timeout(resolve_escape_timeout(&|k| std::env::var(k).ok()));
        loop {
            let chunk = match sb.flush_after() {
                Some(t) => bytes_rx.recv_timeout(t),
                None => bytes_rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            };
            let events = match chunk {
                Ok(bytes) => sb.process_bytes(&bytes),
                Err(RecvTimeoutError::Timeout) => sb.flush().into_iter().map(InputEvent::Data).collect(),
                Err(RecvTimeoutError::Disconnected) => {
                    deliver(sb.flush().into_iter().map(InputEvent::Data).collect(), &tx);
                    break;
                }
            };
            if !deliver(events, &tx) {
                break;
            }
        }
    });
    rx
}

/// [`spawn_input_pump`] over the process stdin.
pub fn spawn_stdin_pump() -> Receiver<String> {
    spawn_input_pump(io::stdin())
}
