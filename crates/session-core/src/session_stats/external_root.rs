//! The counters of a session whose root agent lives outside Ultron: Claude Code calling `rlm` over MCP
//! (session-worker.ts `new SessionStatsRecorder({ root: external === undefined ? "lane" : "external" })`).

use super::{SessionStats, lock};
use serde_json::json;

impl SessionStats {
    /// Count this session as one whose root is `root` (`external` for Claude Code). Called before
    /// [`SessionStats::attach`], it is the root the stored document takes; nothing is saved until a counter changes.
    pub fn set_root(&self, root: &str) {
        lock(&self.shared.state).document["root"] = json!(root);
    }
}
