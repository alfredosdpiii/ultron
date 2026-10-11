//! Transaction operations the task scheduler (`crates/harness`) needs beyond the public `Tx` API: committed reads
//! that ignore the read-after-write rule (upstream's scheduler reads its Storage directly inside commit callbacks),
//! the records this transaction has staged (upstream `Transaction.stagedTasks/stagedConversations`), and the root
//! conversation bootstrap at its reserved ID (upstream `Transaction.createRootConversation`).

use super::Tx;
use crate::session::Result;
use crate::{
    ConversationId, ConversationQuery, ConversationRecord, Cursor, Page, ROOT_CONVERSATION_ID, StorageWrite,
    SubmissionQuery, SubmissionRecord, TaskId, TaskQuery, TaskRecord,
};

impl Tx<'_> {
    /// The committed task record, also after this transaction's first table write.
    pub fn committed_task_record(&mut self, id: TaskId) -> Result<Option<TaskRecord>> {
        self.read(|storage| storage.task(id))
    }

    /// The committed conversation record, also after this transaction's first table write.
    pub fn committed_conversation(&mut self, id: ConversationId) -> Result<Option<ConversationRecord>> {
        self.read(|storage| storage.conversation(id))
    }

    pub fn committed_scan_tasks(
        &mut self,
        query: &TaskQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> Result<Page<TaskRecord>> {
        self.read(|storage| storage.scan_tasks(query, limit, cursor))
    }

    pub fn committed_scan_conversations(
        &mut self,
        query: &ConversationQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> Result<Page<ConversationRecord>> {
        self.read(|storage| storage.scan_conversations(query, limit, cursor))
    }

    pub fn committed_scan_submissions(
        &mut self,
        query: &SubmissionQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> Result<Page<SubmissionRecord>> {
        self.read(|storage| storage.scan_submissions(query, limit, cursor))
    }

    /// Latest candidate of every task this transaction created or replaced, in first-write order.
    pub fn staged_tasks(&self) -> Vec<TaskRecord> {
        self.tasks
            .values()
            .filter_map(|task| task.write.as_ref().map(|(_, record)| record.clone()))
            .collect()
    }

    /// Every conversation this transaction created or forked, in order.
    pub fn staged_conversations(&self) -> Vec<ConversationRecord> {
        self.writes
            .iter()
            .filter_map(|write| match write {
                StorageWrite::Conversation { value } => Some(value.clone()),
                _ => None,
            })
            .collect()
    }

    /// Create the ownerless root conversation at its reserved ID.
    pub fn create_root_conversation(&mut self) -> Result<ConversationRecord> {
        self.has_table_write = true;
        let record = ConversationRecord {
            id: ROOT_CONVERSATION_ID,
            parent: None,
            owner: None,
        };
        self.created_conversations.insert(record.id);
        self.writes.push(StorageWrite::Conversation { value: record.clone() });
        Ok(record)
    }
}
