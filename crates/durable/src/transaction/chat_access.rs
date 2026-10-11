//! Committed entry reads the conversation runtime (`crates/harness` `chat`) needs inside commit callbacks: context
//! derivation reads the transcript in the same transaction that appends to it (upstream derives it from Storage on
//! the Session line), so these reads ignore the read-after-write rule.

use super::Tx;
use crate::session::Result;
use crate::{ConversationId, Cursor, EntryId, EntryQuery, EntryRecord, Page};

impl Tx<'_> {
    pub fn committed_scan_entries(
        &mut self,
        query: &EntryQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> Result<Page<EntryRecord>> {
        self.read(|storage| storage.scan_entries(query, limit, cursor))
    }

    /// One entry only when it is visible through the conversation's fork ancestry.
    pub fn committed_visible_entry(
        &mut self,
        conversation_id: ConversationId,
        id: EntryId,
    ) -> Result<Option<EntryRecord>> {
        Ok(self
            .read(|storage| storage.visible_entry(conversation_id, id))?
            .map(|stored| stored.entry))
    }

    /// The newest visible entry with a `head` at or below the optional inclusive cutoff.
    pub fn committed_head_marker(
        &mut self,
        conversation_id: ConversationId,
        at_or_before: Option<EntryId>,
    ) -> Result<Option<EntryRecord>> {
        self.read(|storage| storage.find_latest_head_marker(conversation_id, at_or_before))
    }
}
