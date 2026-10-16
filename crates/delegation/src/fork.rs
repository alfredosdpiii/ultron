//! `rlm.spawn(fork=True)` (native-host.ts `forkPoint`, `FORK_BRIEF_NOTE`): a forked child's conversation starts as
//! its parent's conversation so far, cut before the tool call still running (the cell that spawns it), and its brief
//! says its kernel is new.

use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;

/// Leads a forked child's brief: the conversation above is its parent's, and its kernel does not have its state.
pub const FORK_BRIEF_NOTE: &str = "You are a subagent forked from the conversation above: it is your parent's context. Your REPL kernel is new: variables, imports and functions from earlier cells do not exist (files do). Do only the brief below.";

/// Where a forked child of `lane` starts: an opaque anchor naming the parent's conversation entry (the spawn reply's
/// `parent_branch_anchor`), none when the lane has no finished conversation yet, or an error.
pub type ForkPointHook = Arc<dyn Fn(&str) -> Result<Option<String>, String> + Send + Sync>;

/// `forkPoint`: over a branch's entries (oldest first, each with its id and the messages it holds), the newest entry
/// after which no tool call waits for its result; none for an empty branch.
pub fn fork_point<'a, I>(entries: I) -> Option<String>
where
    I: IntoIterator<Item = (String, &'a [Value])>,
{
    let mut pending: HashSet<String> = HashSet::new();
    let mut at = None;
    for (id, messages) in entries {
        for message in messages {
            match message["role"].as_str() {
                Some("assistant") => {
                    for part in message["content"].as_array().into_iter().flatten() {
                        if part["type"] == "toolCall"
                            && let Some(call) = part["id"].as_str()
                        {
                            pending.insert(call.to_string());
                        }
                    }
                }
                Some("toolResult") => {
                    if let Some(call) = message["toolCallId"].as_str() {
                        pending.remove(call);
                    }
                }
                _ => {}
            }
        }
        if pending.is_empty() {
            at = Some(id);
        }
    }
    at
}
