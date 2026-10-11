//! Off-thread terminal output pump (port of omp crates/pi-natives/src/tty_writer.rs, in safe Rust over any
//! `Write`).
//!
//! A blocking `write(2)` to a slow or occluded terminal would stall whoever paints. Frames are appended to a back
//! buffer instead ([`TtyWriter::write`] never blocks); a pump thread swaps the buffer out and writes it in bounded
//! chunks, in FIFO order. [`TtyWriter::pending`] reports bytes accepted but not yet written, decremented per chunk,
//! so the render loop can skip frames while the terminal owes more than [`MAX_PENDING_OUTPUT_BYTES`] (see
//! [`crate::viewport::RenderGate`]). A write error marks the writer dead and drops the queue.

use std::io::{self, ErrorKind, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Largest single write handed to the terminal; big replays arrive as several bounded writes.
pub const MAX_TTY_WRITE_CHUNK_BYTES: usize = 16 * 1024;
/// Healthy backlog: above this many pending bytes the next frame is deferred (omp `STDOUT_BACKLOG_CLEAR_BYTES`).
pub const MAX_PENDING_OUTPUT_BYTES: usize = 256 * 1024;

struct Inner {
    back: Mutex<Vec<u8>>,
    pending: AtomicUsize,
    cv: Condvar,
    stop: AtomicBool,
    dead: AtomicBool,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, Vec<u8>> {
        self.back.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Write `buf` in chunks of at most [`MAX_TTY_WRITE_CHUNK_BYTES`], reporting each completed chunk. Retries
/// `Interrupted`; waits out `WouldBlock` (a child may have set O_NONBLOCK on the shared file description).
pub fn write_all_chunked(
    mut write: impl FnMut(&[u8]) -> io::Result<usize>,
    buf: &[u8],
    mut on_progress: impl FnMut(usize),
) -> io::Result<()> {
    let mut off = 0;
    while off < buf.len() {
        let end = (off + MAX_TTY_WRITE_CHUNK_BYTES).min(buf.len());
        match write(&buf[off..end]) {
            Ok(0) => return Err(ErrorKind::WriteZero.into()),
            Ok(n) => {
                off += n;
                on_progress(n);
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) if e.kind() == ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(1)),
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn pump<W: Write>(mut out: W, inner: &Inner) {
    let mut front = Vec::new();
    loop {
        {
            let mut back = inner.lock();
            while back.is_empty() {
                if inner.stop.load(Ordering::Acquire) {
                    return;
                }
                back = inner.cv.wait(back).unwrap_or_else(|e| e.into_inner());
            }
            std::mem::swap(&mut *back, &mut front);
        }
        if inner.dead.load(Ordering::Acquire) {
            inner.pending.fetch_sub(front.len(), Ordering::AcqRel);
        } else {
            let mut written = 0;
            let result = write_all_chunked(
                |chunk| {
                    let n = out.write(chunk)?;
                    out.flush()?;
                    Ok(n)
                },
                &front,
                |n| {
                    written += n;
                    inner.pending.fetch_sub(n, Ordering::AcqRel);
                },
            );
            if result.is_err() {
                inner.dead.store(true, Ordering::Release);
                let mut back = inner.lock();
                let dropped = back.len() + (front.len() - written);
                back.clear();
                inner.pending.fetch_sub(dropped, Ordering::AcqRel);
            }
        }
        front.clear();
        inner.cv.notify_all();
    }
}

/// Dedicated writer thread for one terminal output.
pub struct TtyWriter {
    inner: Arc<Inner>,
    thread: Option<JoinHandle<()>>,
}

impl TtyWriter {
    pub fn new<W: Write + Send + 'static>(out: W) -> io::Result<Self> {
        let inner = Arc::new(Inner {
            back: Mutex::new(Vec::new()),
            pending: AtomicUsize::new(0),
            cv: Condvar::new(),
            stop: AtomicBool::new(false),
            dead: AtomicBool::new(false),
        });
        let pump_inner = Arc::clone(&inner);
        let thread = thread::Builder::new()
            .name("tty-writer".into())
            .spawn(move || pump(out, &pump_inner))?;
        Ok(Self {
            inner,
            thread: Some(thread),
        })
    }

    /// Writer over the process stdout.
    pub fn stdout() -> io::Result<Self> {
        Self::new(io::stdout())
    }

    /// Enqueue output; never blocks on the terminal. Returns the bytes now pending.
    pub fn write(&self, data: &str) -> usize {
        self.write_bytes(data.as_bytes())
    }

    pub fn write_bytes(&self, data: &[u8]) -> usize {
        if data.is_empty() || self.dead() {
            return self.pending();
        }
        {
            let mut back = self.inner.lock();
            back.extend_from_slice(data);
            // Account while still holding the lock, so the pump cannot drain and subtract first.
            self.inner.pending.fetch_add(data.len(), Ordering::AcqRel);
        }
        self.inner.cv.notify_all();
        self.pending()
    }

    /// Bytes accepted but not yet written.
    pub fn pending(&self) -> usize {
        self.inner.pending.load(Ordering::Acquire)
    }

    /// True once a write failed; queued output was dropped.
    pub fn dead(&self) -> bool {
        self.inner.dead.load(Ordering::Acquire)
    }

    /// Block until drained, dead, or `timeout`. True when fully drained. Exit paths only.
    pub fn flush_sync(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut back = self.inner.lock();
        while self.pending() > 0 && !self.dead() {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            back = self
                .inner
                .cv
                .wait_timeout(back, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        drop(back);
        self.pending() == 0
    }

    /// Flush (bounded), stop and join the pump. A pump stuck in a blocked write is detached instead of joined.
    pub fn stop(&mut self, flush_timeout: Duration) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        let drained = self.flush_sync(flush_timeout);
        self.inner.stop.store(true, Ordering::Release);
        self.inner.cv.notify_all();
        if drained || self.dead() {
            let _ = thread.join();
        }
    }
}

impl Drop for TtyWriter {
    fn drop(&mut self) {
        self.stop(Duration::ZERO);
    }
}

/// A cloneable handle that enqueues into a shared [`TtyWriter`] (for notifications and other side writers).
#[derive(Clone)]
pub struct TtyHandle(Arc<TtyWriter>);

impl TtyHandle {
    pub fn new(writer: TtyWriter) -> Self {
        Self(Arc::new(writer))
    }

    pub fn write(&self, data: &str) -> usize {
        self.0.write(data)
    }

    pub fn pending(&self) -> usize {
        self.0.pending()
    }

    pub fn dead(&self) -> bool {
        self.0.dead()
    }

    pub fn flush_sync(&self, timeout: Duration) -> bool {
        self.0.flush_sync(timeout)
    }
}
