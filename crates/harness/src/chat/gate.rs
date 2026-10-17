//! Gates on a run's model requests and REPL cells (the worker's `before_request` and `before_tool` hooks of
//! session-worker.ts): the application decides, per lane run, whether a request may go out, goes out as the run's one
//! final request (tool choice `none`, with a note appended to its context), or is refused; and whether a cell may run.
//! Ultron uses them for the per-root turn, token and cost limits on every lane.

use std::sync::Arc;

use ai::{Message, TextContent, UserContent, UserMessage, UserMessageContent};
use durable::{ConversationId, Tx};

use super::cell::CellCall;
use super::live::live;
use super::util::get;
use crate::scheduler::Result;

/// One run of one conversation: the conversation and a key that stays the same for the whole run (its first input
/// submission, else the task that owns the run). A new run of the same conversation has a new key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct GateRun {
    pub conversation_id: ConversationId,
    pub run: Option<u64>,
}

/// What the request gate decided for a model request of a run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RequestGate {
    Allow,
    /// The run's last request: no tools (`toolChoice: "none"`), and `note` appended to the context it sends.
    Final {
        note: String,
    },
    /// The request is not sent: the run fails with `request_blocked` and `reason`.
    Block {
        reason: String,
    },
}

/// A cell refused before it runs: its result is `reason` (an error); `terminate` also ends the run after the round.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolBlock {
    pub reason: String,
    pub terminate: bool,
}

/// `before_request` (step `assistant`) of one run.
pub type GateRequest = Arc<dyn Fn(&GateRun) -> RequestGate + Send + Sync>;
/// `before_tool` of one cell of a run.
pub type GateTool = Arc<dyn Fn(&GateRun, &CellCall) -> Option<ToolBlock> + Send + Sync>;

/// The run `conversation_id` is in, as the gates see it.
pub(crate) fn gate_run(tx: &mut Tx<'_>, conversation_id: ConversationId) -> Result<GateRun> {
    let live = live(tx, conversation_id)?;
    let first_input = get(&live, &["run", "inputs"])?
        .and_then(|inputs| inputs.as_array().and_then(|inputs| inputs.first().cloned()))
        .and_then(|input| input.as_u64());
    let task = get(&live, &["run", "taskId"])?.and_then(|id| id.as_u64());
    Ok(GateRun {
        conversation_id,
        run: first_input.or(task),
    })
}

/// The tool choice a run's final request is sent with.
pub const FINAL_TOOL_CHOICE: &str = "none";

/// Apply a final request's gate to what a provider is sent: tool choice `none`, and the note as a user message
/// after the context (`createCustomMessage(...)` as the model sees it: a user message with one text block).
pub fn apply_final(note: &str, messages: &mut Vec<Message>, options: &mut ai::SimpleStreamOptions, timestamp: i64) {
    options.tool_choice = Some(FINAL_TOOL_CHOICE.into());
    messages.push(Message::User(UserMessage {
        content: UserMessageContent::Blocks(vec![UserContent::Text(TextContent {
            text: note.to_string(),
            text_signature: None,
        })]),
        timestamp,
    }));
}
