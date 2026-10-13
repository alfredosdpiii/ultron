//! Signal handling without `unsafe`: SIGINT, SIGTERM and SIGHUP are blocked on the main thread before any other
//! thread starts (threads inherit the mask) and read from a `signalfd` on a thread of their own. std's process
//! spawning resets the mask in children, so kernels, shells and re-launched internal processes see default
//! signal handling.
//!
//! A role or mode installs its handler ([`set_handler`]); it runs on a fresh thread so it may wait (abort a turn,
//! release workers). A second signal while a handler runs exits at once with the signal's exit code. Without a
//! handler the process dies of the signal as it would have unblocked.

use nix::sys::signal::{SigSet, Signal, raise};
use nix::sys::signalfd::{SfdFlags, SignalFd};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

pub use nix::sys::signal::Signal as Sig;

/// The signals handled here.
pub const HANDLED: [Signal; 3] = [Signal::SIGINT, Signal::SIGTERM, Signal::SIGHUP];

pub type Handler = Arc<dyn Fn(Signal) + Send + Sync>;

static HANDLER: Mutex<Option<Handler>> = Mutex::new(None);
static STARTED: OnceLock<()> = OnceLock::new();
static HANDLING: AtomicBool = AtomicBool::new(false);
/// The last handled signal read (its number), 0 before any.
static RECEIVED: AtomicI32 = AtomicI32::new(0);

fn handled() -> SigSet {
    HANDLED.iter().copied().collect()
}

/// The exit code a signal ends a process with (`128 + n`: SIGINT 130, SIGTERM 143, SIGHUP 129).
pub fn exit_code(signal: Signal) -> i32 {
    128 + signal as i32
}

/// Block the handled signals on this thread and start reading them. Call once, first thing in `main`.
pub fn install() -> Result<(), String> {
    if STARTED.get().is_some() {
        return Ok(());
    }
    let mask = handled();
    mask.thread_block().map_err(|e| e.to_string())?;
    let fd = SignalFd::with_flags(&mask, SfdFlags::SFD_CLOEXEC).map_err(|e| e.to_string())?;
    let _ = STARTED.set(());
    std::thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            loop {
                let info = match fd.read_signal() {
                    Ok(Some(info)) => {
                        RECEIVED.store(info.ssi_signo as i32, Ordering::SeqCst);
                        info
                    }
                    Ok(None) => continue,
                    Err(_) => return,
                };
                let Ok(signal) = Signal::try_from(info.ssi_signo as i32) else {
                    continue;
                };
                dispatch(signal);
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn dispatch(signal: Signal) {
    let handler = HANDLER.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let Some(handler) = handler else {
        die(signal);
    };
    if HANDLING.swap(true, Ordering::SeqCst) {
        std::process::exit(exit_code(signal));
    }
    std::thread::spawn(move || handler(signal));
}

/// Die of `signal` with its default action, as an unblocked process would have.
fn die(signal: Signal) -> ! {
    let one: SigSet = [signal].into_iter().collect();
    let _ = one.thread_unblock();
    let _ = raise(signal);
    std::process::exit(exit_code(signal));
}

/// A handled signal this process received: one already read, or one still pending (`ShdPnd`/`SigPnd` of
/// /proc/self/status). Lets a mode tell input that ended because of a signal (the terminal went away) from input
/// that ended on its own, whichever its handler sees first.
pub fn received() -> Option<Signal> {
    let read = || Signal::try_from(RECEIVED.load(Ordering::SeqCst)).ok();
    if let Some(signal) = read() {
        return Some(signal);
    }
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let pending: u64 = status
        .lines()
        .filter(|l| l.starts_with("ShdPnd:") || l.starts_with("SigPnd:"))
        .filter_map(|l| u64::from_str_radix(l.split(':').nth(1).unwrap_or("").trim(), 16).ok())
        .fold(0, |all, mask| all | mask);
    if let Some(signal) = HANDLED.into_iter().find(|s| pending & (1 << (*s as i32 - 1)) != 0) {
        return Some(signal);
    }
    // The reader may be between taking the signal off the queue and recording it.
    std::thread::sleep(std::time::Duration::from_millis(5));
    read()
}

/// Handle the signals with `handler` from now on (replacing any previous one).
pub fn set_handler(handler: Handler) {
    *HANDLER.lock().unwrap_or_else(|e| e.into_inner()) = Some(handler);
}

/// Back to the default action.
pub fn clear_handler() {
    *HANDLER.lock().unwrap_or_else(|e| e.into_inner()) = None;
}
