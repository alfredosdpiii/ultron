//! Ported from omp crates/pi-natives/src/tty_writer.rs tests (pipes replaced by in-process writers).

use std::io::{self, ErrorKind, Write};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use tui::tty_writer::*;

#[test]
fn tui_tty_writer_splits_large_frames_into_bounded_writes() {
    let frame = vec![b'x'; MAX_TTY_WRITE_CHUNK_BYTES * 2 + 7];
    let (mut requested, mut output, mut progress) = (Vec::new(), Vec::new(), Vec::new());
    write_all_chunked(
        |chunk| {
            requested.push(chunk.len());
            output.extend_from_slice(chunk);
            Ok(chunk.len())
        },
        &frame,
        |n| progress.push(n),
    )
    .unwrap();
    assert_eq!(requested, [16384, 16384, 7]);
    assert_eq!(progress, [16384, 16384, 7]);
    assert_eq!(output, frame);
}

#[test]
fn tui_tty_writer_retries_interrupted_and_would_block_and_fails_on_zero() {
    let mut calls = 0;
    let mut out = Vec::new();
    write_all_chunked(
        |chunk| {
            calls += 1;
            match calls {
                1 => Err(ErrorKind::Interrupted.into()),
                2 => Err(ErrorKind::WouldBlock.into()),
                3 => {
                    out.extend_from_slice(&chunk[..2]);
                    Ok(2)
                }
                _ => {
                    out.extend_from_slice(chunk);
                    Ok(chunk.len())
                }
            }
        },
        b"hello",
        |_| {},
    )
    .unwrap();
    assert_eq!(out, b"hello");
    assert_eq!(calls, 4);
    let err = write_all_chunked(|_| Ok(0), b"x", |_| {}).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::WriteZero);
}

/// A writer whose writes block until the test opens the gate; each write accepts at most `per_write` bytes.
#[derive(Clone)]
struct Gated {
    open: Arc<(Mutex<bool>, Condvar)>,
    out: Arc<Mutex<Vec<u8>>>,
    per_write: usize,
}

impl Gated {
    fn new(open: bool, per_write: usize) -> Self {
        Self {
            open: Arc::new((Mutex::new(open), Condvar::new())),
            out: Arc::new(Mutex::new(Vec::new())),
            per_write,
        }
    }
    fn set(&self, open: bool) {
        *self.open.0.lock().unwrap() = open;
        self.open.1.notify_all();
    }
    fn bytes(&self) -> Vec<u8> {
        self.out.lock().unwrap().clone()
    }
}

impl Write for Gated {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut open = self.open.0.lock().unwrap();
        while !*open {
            open = self.open.1.wait(open).unwrap();
        }
        let n = buf.len().min(self.per_write);
        self.out.lock().unwrap().extend_from_slice(&buf[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn tui_tty_writer_writes_in_order_and_drains() {
    let sink = Gated::new(true, usize::MAX);
    let mut w = TtyWriter::new(sink.clone()).unwrap();
    w.write("hello ");
    w.write("world");
    assert!(w.flush_sync(Duration::from_secs(2)));
    assert_eq!(w.pending(), 0);
    assert_eq!(sink.bytes(), b"hello world");
    w.stop(Duration::from_secs(1));
}

#[test]
fn tui_tty_writer_enqueue_never_blocks_and_reports_pending() {
    let sink = Gated::new(false, usize::MAX);
    let mut w = TtyWriter::new(sink.clone()).unwrap();
    let chunk = "x".repeat(256 * 1024);
    let start = Instant::now();
    w.write(&chunk);
    let pending = w.write(&chunk);
    assert!(start.elapsed() < Duration::from_millis(200), "enqueue blocked");
    assert_eq!(pending, 512 * 1024);
    assert!(
        !w.flush_sync(Duration::from_millis(20)),
        "cannot drain while the terminal is stalled"
    );
    sink.set(true);
    assert!(w.flush_sync(Duration::from_secs(5)));
    assert_eq!(sink.bytes().len(), 512 * 1024);
    w.stop(Duration::from_secs(1));
}

#[test]
fn tui_tty_writer_pending_tracks_partial_drain_progress() {
    // The terminal accepts 1000 bytes per write call; pending must shrink as chunks land, not only per batch.
    #[derive(Clone)]
    struct Slow(Arc<Mutex<usize>>);
    impl Write for Slow {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            std::thread::sleep(Duration::from_millis(2));
            let n = buf.len().min(1000);
            *self.0.lock().unwrap() += n;
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let total = 200_000;
    let counter = Arc::new(Mutex::new(0));
    let mut w = TtyWriter::new(Slow(counter.clone())).unwrap();
    w.write_bytes(&vec![b'x'; total]);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let p = w.pending();
        if p > 0 && p < total {
            break;
        }
        assert!(Instant::now() < deadline, "no observable partial progress: {p}");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(w.flush_sync(Duration::from_secs(10)));
    assert_eq!(*counter.lock().unwrap(), total);
    w.stop(Duration::from_secs(1));
}

#[test]
fn tui_tty_writer_rapid_small_writes_never_underflow() {
    let sink = Gated::new(true, usize::MAX);
    let mut w = TtyWriter::new(sink.clone()).unwrap();
    for _ in 0..100_000 {
        w.write("x");
    }
    assert!(w.flush_sync(Duration::from_secs(5)));
    assert_eq!(w.pending(), 0);
    assert_eq!(sink.bytes().len(), 100_000);
    w.stop(Duration::from_secs(1));
}

#[test]
fn tui_tty_writer_dead_output_marks_writer_and_drops_queue() {
    struct Broken;
    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut w = TtyWriter::new(Broken).unwrap();
    w.write("doomed");
    let deadline = Instant::now() + Duration::from_secs(2);
    while !w.dead() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(w.dead());
    assert_eq!(w.pending(), 0);
    assert_eq!(w.write("more"), 0, "a dead writer accepts nothing");
    assert!(w.flush_sync(Duration::from_millis(10)));
    w.stop(Duration::from_millis(100));
}

#[test]
fn tui_tty_writer_stop_detaches_a_blocked_pump() {
    let sink = Gated::new(false, usize::MAX);
    let mut w = TtyWriter::new(sink.clone()).unwrap();
    w.write("stuck");
    let start = Instant::now();
    w.stop(Duration::from_millis(30));
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "stop must not join a blocked pump"
    );
    sink.set(true); // let the detached thread finish
}

#[test]
fn tui_tty_writer_handle_shares_one_queue() {
    let sink = Gated::new(true, usize::MAX);
    let h = TtyHandle::new(TtyWriter::new(sink.clone()).unwrap());
    let h2 = h.clone();
    h.write("a");
    h2.write("b");
    assert!(h.flush_sync(Duration::from_secs(2)));
    assert_eq!(sink.bytes(), b"ab");
    assert!(!h.dead());
    assert_eq!(h2.pending(), 0);
}
