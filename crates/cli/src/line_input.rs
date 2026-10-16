//! A one-line answer read from the terminal that can be given up: Esc or Ctrl+C cancels it, and the caller's `stop`
//! (the flow finished another way, or its time ran out) ends it within one poll. The read happens on the caller's
//! thread, byte by byte as they arrive, so nothing is left reading stdin afterwards to swallow later input (the
//! TypeScript prompt is a `readline.question` with an abort signal, closed when the sign-in ends).
//!
//! The terminal is in non-canonical mode without echo or signals while the line is read; the typed text is echoed
//! here and the mode restored after.

use std::io::Write;
use std::time::Duration;

/// How long one wait for input lasts before `stop` is checked again.
pub const POLL: Duration = Duration::from_millis(50);

/// Bytes as they arrive.
pub trait Input {
    /// The bytes available within `timeout`: `None` when none came, an empty vector at the end of the input.
    fn wait(&mut self, timeout: Duration) -> std::io::Result<Option<Vec<u8>>>;
}

#[derive(Debug, PartialEq, Eq)]
pub enum LineRead {
    /// Enter: the text typed.
    Line(String),
    /// Esc or Ctrl+C.
    Cancelled,
    /// The input ended (Ctrl+D on an empty line, a closed stdin, a read error).
    Closed,
    /// `stop` said so.
    Stopped,
}

/// Read one line from `input`, echoing it to `echo`, until Enter, Esc/Ctrl+C, the end of the input or `stop()`.
pub fn read_line(input: &mut dyn Input, echo: &mut dyn Write, stop: &dyn Fn() -> bool) -> LineRead {
    let mut line: Vec<u8> = Vec::new();
    loop {
        if stop() {
            return LineRead::Stopped;
        }
        let bytes = match input.wait(POLL) {
            Ok(None) => continue,
            Ok(Some(bytes)) if bytes.is_empty() => return LineRead::Closed,
            Ok(Some(bytes)) => bytes,
            Err(_) => return LineRead::Closed,
        };
        let mut i = 0;
        while i < bytes.len() {
            let byte = bytes[i];
            i += 1;
            match byte {
                b'\r' | b'\n' => {
                    let _ = echo.write_all(b"\r\n");
                    let _ = echo.flush();
                    return LineRead::Line(String::from_utf8_lossy(&line).into_owned());
                }
                0x03 => return LineRead::Cancelled,
                0x1b => match bytes.get(i) {
                    // An escape sequence (an arrow or function key): skipped. CSI ends at a final byte, SS3 after one.
                    Some(b'[') => {
                        i += 1;
                        while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                            i += 1;
                        }
                        i += 1;
                    }
                    Some(b'O') => i += 2,
                    _ => return LineRead::Cancelled,
                },
                0x7f | 0x08 => {
                    // One character back: a UTF-8 sequence's continuation bytes go with it.
                    while line.last().is_some_and(|b| b & 0xc0 == 0x80) {
                        line.pop();
                    }
                    if line.pop().is_some() {
                        let _ = echo.write_all(b"\x08 \x08");
                    }
                }
                0x04 if line.is_empty() => return LineRead::Closed,
                byte if byte < 0x20 => {}
                byte => {
                    line.push(byte);
                    let _ = echo.write_all(&[byte]);
                }
            }
        }
        let _ = echo.flush();
    }
}

/// The process's terminal stdin in non-canonical mode, without echo or signals, until dropped.
pub struct TerminalInput {
    saved: rustix::termios::Termios,
}

impl TerminalInput {
    pub fn open() -> std::io::Result<TerminalInput> {
        use rustix::termios::{LocalModes, OptionalActions, SpecialCodeIndex, tcgetattr, tcsetattr};
        let stdin = std::io::stdin();
        let saved = tcgetattr(&stdin)?;
        let mut raw = saved.clone();
        raw.local_modes
            .remove(LocalModes::ICANON | LocalModes::ECHO | LocalModes::ISIG | LocalModes::IEXTEN);
        raw.special_codes[SpecialCodeIndex::VMIN] = 1;
        raw.special_codes[SpecialCodeIndex::VTIME] = 0;
        tcsetattr(&stdin, OptionalActions::Now, &raw)?;
        Ok(TerminalInput { saved })
    }
}

impl Drop for TerminalInput {
    fn drop(&mut self) {
        let _ = rustix::termios::tcsetattr(std::io::stdin(), rustix::termios::OptionalActions::Now, &self.saved);
    }
}

impl Input for TerminalInput {
    fn wait(&mut self, timeout: Duration) -> std::io::Result<Option<Vec<u8>>> {
        wait_fd(&std::io::stdin(), timeout)
    }
}

/// Poll `fd` for up to `timeout`; read only what is there (straight from the descriptor, so no buffer keeps bytes a
/// later reader should get).
fn wait_fd(fd: &impl std::os::fd::AsFd, timeout: Duration) -> std::io::Result<Option<Vec<u8>>> {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    let timeout = Timespec {
        tv_sec: i64::try_from(timeout.as_secs()).unwrap_or(i64::MAX),
        tv_nsec: i64::from(timeout.subsec_nanos()),
    };
    let mut fds = [PollFd::new(fd, PollFlags::IN)];
    match poll(&mut fds, Some(&timeout)) {
        Ok(0) | Err(rustix::io::Errno::INTR) => return Ok(None),
        Ok(_) => {}
        Err(e) => return Err(e.into()),
    }
    let mut buf = [0u8; 1024];
    match rustix::io::read(fd, &mut buf) {
        Ok(n) => Ok(Some(buf[..n].to_vec())),
        Err(rustix::io::Errno::INTR | rustix::io::Errno::AGAIN) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Any readable descriptor (a pipe in tests) as [`Input`], without changing its mode.
pub struct FdInput<F: std::os::fd::AsFd>(pub F);

impl<F: std::os::fd::AsFd> Input for FdInput<F> {
    fn wait(&mut self, timeout: Duration) -> std::io::Result<Option<Vec<u8>>> {
        wait_fd(&self.0, timeout)
    }
}
