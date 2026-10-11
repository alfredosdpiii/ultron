//! Process terminal: raw mode and size through crossterm, escape-sequence writes, and an input pump that feeds stdin
//! bytes through [`StdinBuffer`] (port of `ProcessTerminal` in packages/tui/src/terminal.ts, without Kitty keyboard
//! protocol negotiation, modifyOtherKeys, progress keepalive or the Windows VT input switch).

use std::io::{self, Read, Write};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;

use crate::stdin_buffer::{InputEvent, StdinBuffer};

/// Escape output of the terminal, over any writer.
pub struct Terminal<W: Write> {
    pub out: W,
    was_raw: bool,
    started: bool,
}

impl<W: Write> Terminal<W> {
    pub fn new(out: W) -> Self {
        Self {
            out,
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
        }
        self.started = raw;
        self.write("\x1b[?2004h")
    }

    /// Disable bracketed paste and restore the previous raw-mode state.
    pub fn stop(&mut self) -> io::Result<()> {
        self.write("\x1b[?2004l")?;
        if std::mem::take(&mut self.started) && !self.was_raw {
            crossterm::terminal::disable_raw_mode()?;
        }
        Ok(())
    }

    pub fn move_by(&mut self, lines: i64) -> io::Result<()> {
        match lines {
            0 => Ok(()),
            l if l > 0 => self.write(&format!("\x1b[{l}B")),
            l => self.write(&format!("\x1b[{}A", -l)),
        }
    }

    pub fn hide_cursor(&mut self) -> io::Result<()> {
        self.write("\x1b[?25l")
    }

    pub fn show_cursor(&mut self) -> io::Result<()> {
        self.write("\x1b[?25h")
    }

    pub fn clear_line(&mut self) -> io::Result<()> {
        self.write("\x1b[K")
    }

    pub fn clear_from_cursor(&mut self) -> io::Result<()> {
        self.write("\x1b[J")
    }

    pub fn clear_screen(&mut self) -> io::Result<()> {
        self.write("\x1b[2J\x1b[H")
    }

    pub fn set_title(&mut self, title: &str) -> io::Result<()> {
        self.write(&format!("\x1b]0;{title}\x07"))
    }
}

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

/// Reads `input` on a thread, splits it with [`StdinBuffer`] (flushing partial sequences after its timeout) and
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
        let mut sb = StdinBuffer::new();
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
