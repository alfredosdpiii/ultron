//! Session operations the task scheduler (`crates/harness`) needs beyond the public API: failing the Session for a
//! throw in one of the scheduler's own commits (upstream `SessionImpl.fail`).

use super::{Session, SessionError};

impl Session {
    /// Fail the Session with `error`, the first failure wins: admission ends, close listeners run, every watch ends,
    /// and Storage closes. Must not be called on the mutation line (from a commit callback or listener).
    pub fn fail(&self, error: SessionError) {
        let mut line = self.inner.line.enter();
        self.inner.fail(&mut line, error);
    }
}
