//! Ultron's format-4 session journal: the JSONL file the TypeScript product writes for every session
//! (`packages/agent/src/harness/session/{commit,in-memory-storage-state,session,context,fork-policy}.ts` and
//! `jsonl/*`). A file is a header line (`{"v":4,"kind":"header",...}`) followed by one committed transaction per
//! line; replaying the transactions gives the session's entries (a tree through `parentId`), scalar values (branch
//! tips, lane configuration and state, operation state, names, labels, Ultron documents), lists and usage rows.
//!
//! Legacy Pi v3 files (`crates/session-file`) are read as format-4 writes and upgraded in place by their first
//! commit. [`run`] writes the harness operation journal of a prompted run; [`names`] lists sessions with their
//! names and resolves `--resume <name|id>`.

pub mod checkpoints;
pub mod codec;
pub mod context;
pub mod fork;
pub mod io;
pub mod legacy_v3;
pub mod names;
pub mod repo;
pub mod run;
pub mod session;
pub mod state;
pub mod storage;
pub mod usage;

pub use fork::ForkOptions;
pub use repo::{Metadata, SessionRepo};
pub use session::Session;
pub use state::{CommitResult, Write};
pub use storage::JsonlStorage;

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// The clock and id source of a session (`Date.now()` and `uuidv7()` of `@ultron/ai`).
pub trait Clock: Send {
    fn now(&mut self) -> i64;
    /// `uuidv7(at)`: an id whose timestamp is exactly `at` (a follower id; the ordinary clock is unaffected).
    fn uuidv7(&mut self, at: i64) -> String;
    /// `uuidv7()`: an id at the current time, never earlier than the previous ordinary id.
    fn next_id(&mut self) -> String;
}

pub type SharedClock = Arc<Mutex<dyn Clock>>;

pub(crate) fn lock(clock: &SharedClock) -> MutexGuard<'_, dyn Clock + 'static> {
    clock.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Lock a shared clock (a poisoned lock is still usable: the clock has no invariants to break).
pub fn lock_clock(clock: &SharedClock) -> MutexGuard<'_, dyn Clock + 'static> {
    lock(clock)
}

const MAX_UUID_V7_TIMESTAMP: i64 = 0xffff_ffff_ffff;
const MAX_SEQUENCE: u64 = (1 << 41) - 1;

/// The `uuidv7` generator: a 41-bit sequence seeded from the first id's random bytes, then incremented.
#[derive(Debug)]
pub struct Uuidv7 {
    last_ordinary: i64,
    sequence: Option<u64>,
}

impl Default for Uuidv7 {
    fn default() -> Self {
        Uuidv7 {
            last_ordinary: -1,
            sequence: None,
        }
    }
}

impl Uuidv7 {
    /// An id at `explicit` when given, else at `now` clamped to the last ordinary id's time.
    pub fn generate(&mut self, explicit: Option<i64>, now: i64, mut bytes: [u8; 16]) -> String {
        let timestamp = match explicit {
            Some(at) => at,
            None => {
                self.last_ordinary = now.max(self.last_ordinary);
                self.last_ordinary
            }
        }
        .clamp(0, MAX_UUID_V7_TIMESTAMP);
        let sequence = match self.sequence {
            None => bytes[1..6].iter().fold(0u64, |acc, b| (acc << 8) | u64::from(*b)),
            // ponytail: upstream throws once 2^41 ids share one generator; wrap instead of failing a session.
            Some(s) if s == MAX_SEQUENCE => 0,
            Some(s) => s + 1,
        };
        self.sequence = Some(sequence);
        let ts = timestamp as u64;
        for (index, byte) in bytes.iter_mut().take(6).enumerate() {
            *byte = (ts >> ((5 - index) * 8)) as u8;
        }
        bytes[6] = 0x70 | ((sequence >> 37) & 0x0f) as u8;
        bytes[7] = (sequence >> 29) as u8;
        bytes[8] = 0x80 | ((sequence >> 23) & 0x3f) as u8;
        bytes[9] = (sequence >> 15) as u8;
        bytes[10] = (sequence >> 7) as u8;
        bytes[11] = (((sequence & 0x7f) << 1) as u8) | (bytes[11] & 0x01);
        let hex: Vec<String> = bytes.iter().map(|x| format!("{x:02x}")).collect();
        format!(
            "{}-{}-{}-{}-{}",
            hex[0..4].concat(),
            hex[4..6].concat(),
            hex[6..8].concat(),
            hex[8..10].concat(),
            hex[10..].concat()
        )
    }
}

/// The system clock with the process-wide UUIDv7 generator and OS randomness.
#[derive(Debug, Default)]
pub struct SystemClock;

static GENERATOR: Mutex<Uuidv7> = Mutex::new(Uuidv7 {
    last_ordinary: -1,
    sequence: None,
});

fn system_now() -> i64 {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
    now.map_or(0, |d| d.as_millis() as i64)
}

fn random_bytes() -> [u8; 16] {
    let mut bytes = [0; 16];
    let _ = getrandom::fill(&mut bytes);
    bytes
}

impl Clock for SystemClock {
    fn now(&mut self) -> i64 {
        system_now()
    }

    fn uuidv7(&mut self, at: i64) -> String {
        let mut generator = GENERATOR.lock().unwrap_or_else(PoisonError::into_inner);
        generator.generate(Some(at), 0, random_bytes())
    }

    fn next_id(&mut self) -> String {
        let mut generator = GENERATOR.lock().unwrap_or_else(PoisonError::into_inner);
        generator.generate(None, system_now(), random_bytes())
    }
}

impl SystemClock {
    pub fn shared() -> SharedClock {
        Arc::new(Mutex::new(SystemClock))
    }
}

/// A deterministic clock for tests and fixtures: `now()` reads `time` and advances it by `step`; ids use zero
/// random bytes (the fixture generators pin `crypto.getRandomValues` to zeros the same way).
#[derive(Debug)]
pub struct TestClock {
    pub time: i64,
    pub step: i64,
    generator: Uuidv7,
}

impl TestClock {
    pub fn new(start: i64, step: i64) -> Self {
        TestClock {
            time: start,
            step,
            generator: Uuidv7::default(),
        }
    }

    pub fn shared(start: i64, step: i64) -> SharedClock {
        Arc::new(Mutex::new(TestClock::new(start, step)))
    }
}

impl Clock for TestClock {
    fn now(&mut self) -> i64 {
        let now = self.time;
        self.time += self.step;
        now
    }

    fn uuidv7(&mut self, at: i64) -> String {
        self.generator.generate(Some(at), 0, [0; 16])
    }

    fn next_id(&mut self) -> String {
        let now = self.now();
        self.generator.generate(None, now, [0; 16])
    }
}
