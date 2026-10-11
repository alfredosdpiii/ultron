//! Clock, randomness, and agent directory: the process inputs the session manager reads.

use std::sync::{Mutex, PoisonError};

/// Inputs the TypeScript implementation takes from the process (`Date`, `crypto`, the agent dir).
pub trait Env {
    /// Milliseconds since the Unix epoch (`Date.now()`).
    fn now_ms(&mut self) -> i64;
    /// A random UUID string (`crypto.randomUUID()`); entry ids use its first 8 characters.
    fn random_uuid(&mut self) -> String;
    /// A new session id (`uuidv7()`, whose generator state is process-wide upstream).
    fn session_id(&mut self) -> String;
    /// The agent directory (`getAgentDir()`): default session dirs live under `<agent>/sessions`.
    fn agent_dir(&self) -> String;
}

/// The real process environment: system clock, OS randomness, `ULTRON_CODING_AGENT_DIR` or `~/.ultron/agent`.
#[derive(Debug, Default)]
pub struct SystemEnv;

impl Env for SystemEnv {
    fn now_ms(&mut self) -> i64 {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
        now.map_or(0, |d| d.as_millis() as i64)
    }

    fn random_uuid(&mut self) -> String {
        let mut b = random_bytes();
        b[6] = (b[6] & 0x0f) | 0x40;
        b[8] = (b[8] & 0x3f) | 0x80;
        format_uuid(&b)
    }

    fn session_id(&mut self) -> String {
        static GENERATOR: Mutex<Uuidv7> = Mutex::new(Uuidv7 {
            last_timestamp: -1,
            sequence: None,
        });
        let now = self.now_ms();
        GENERATOR
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .next(now, random_bytes())
    }

    fn agent_dir(&self) -> String {
        match std::env::var("ULTRON_CODING_AGENT_DIR") {
            Ok(dir) if !dir.is_empty() => crate::paths::normalize_path(&dir),
            _ => crate::paths::join(&[&crate::paths::home_dir(), ".ultron", "agent"]),
        }
    }
}

fn random_bytes() -> [u8; 16] {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).expect("OS randomness is unavailable");
    bytes
}

fn format_uuid(b: &[u8; 16]) -> String {
    let hex: Vec<String> = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        hex[0..4].concat(),
        hex[4..6].concat(),
        hex[6..8].concat(),
        hex[8..10].concat(),
        hex[10..].concat()
    )
}

const MAX_UUID_V7_TIMESTAMP: i64 = 0xffff_ffff_ffff;
const MAX_SEQUENCE: u64 = (1 << 41) - 1;

/// The `uuidv7()` generator of `@ultron/ai`: a monotonic timestamp and a 41-bit sequence seeded
/// from random bytes once, then incremented, so ids from one generator sort in creation order.
#[derive(Debug, Default)]
pub struct Uuidv7 {
    last_timestamp: i64,
    sequence: Option<u64>,
}

impl Uuidv7 {
    /// The next id at `now_ms` (clamped to never go backwards) with fresh random bytes.
    pub fn next(&mut self, now_ms: i64, mut bytes: [u8; 16]) -> String {
        let timestamp = now_ms.max(self.last_timestamp).clamp(0, MAX_UUID_V7_TIMESTAMP);
        self.last_timestamp = timestamp;
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
        format_uuid(&bytes)
    }
}
