//! Internal control lines (experimental/process.ts `encodeControlLine` and the coordinator's line readers): one
//! `JSON.stringify` value per LF-terminated line over a Unix socket, at most 128 MiB.

use serde_json::Value;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};

pub const MAX_CONTROL_LINE_BYTES: usize = 128 * 1024 * 1024;

pub fn encode_control_line(message: &Value) -> io::Result<String> {
    let line = format!("{}\n", session_file::js::stringify(message));
    if line.len() > MAX_CONTROL_LINE_BYTES {
        return Err(io::Error::other("Internal control message is too large"));
    }
    Ok(line)
}

/// A shared writer: whole lines are written under one lock, so concurrent senders never interleave.
#[derive(Clone)]
pub struct LineWriter(Arc<Mutex<UnixStream>>);

impl LineWriter {
    pub fn new(stream: UnixStream) -> Self {
        LineWriter(Arc::new(Mutex::new(stream)))
    }

    pub fn send(&self, message: &Value) -> io::Result<()> {
        let line = encode_control_line(message)?;
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .write_all(line.as_bytes())
    }

    pub fn shutdown(&self) {
        let _ = self
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .shutdown(std::net::Shutdown::Both);
    }

    pub fn same(&self, other: &LineWriter) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// Reads JSON lines until end of stream; an oversized or invalid line ends the stream with an error.
pub struct LineReader<R> {
    reader: BufReader<R>,
}

impl<R: Read> LineReader<R> {
    pub fn new(stream: R) -> Self {
        LineReader {
            reader: BufReader::new(stream),
        }
    }

    pub fn read_value(&mut self) -> io::Result<Option<Value>> {
        let mut line = Vec::new();
        let read = (&mut self.reader)
            .take(MAX_CONTROL_LINE_BYTES as u64 + 1)
            .read_until(b'\n', &mut line)?;
        if read == 0 {
            return Ok(None);
        }
        if line.last() != Some(&b'\n') {
            // An unterminated final line is dropped, as the TypeScript readers never see its newline.
            if line.len() > MAX_CONTROL_LINE_BYTES {
                return Err(io::Error::other("Control message is too large"));
            }
            return Ok(None);
        }
        line.pop();
        serde_json::from_slice(&line)
            .map(Some)
            .map_err(|_| io::Error::other("Invalid control JSON"))
    }
}
