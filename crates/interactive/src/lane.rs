//! What the interactive TUI exchanges with the session it shows, as plain data: events from the lane (Pi message
//! JSON, queue and operation changes) and the commands it sends ([`LaneClient`]). The TUI never links the engine;
//! the composition root implements [`LaneClient`] (today an in-process lane on its own thread, later the session
//! worker over the server protocol).

use serde_json::Value;

/// A model the session can switch to (`ModelSummary`).
#[derive(Clone, Debug, PartialEq)]
pub struct ModelInfo {
    pub provider: String,
    pub id: String,
    pub name: String,
    pub reasoning: bool,
    pub context_window: u64,
}

/// A queued user message (`LaneSnapshot.queues` item).
#[derive(Clone, Debug, PartialEq)]
pub struct Queued {
    pub entry_id: String,
    /// `steer` or `followUp`.
    pub kind: QueueKind,
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueKind {
    Steer,
    FollowUp,
}

/// A stored session the resume picker offers.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionListing {
    pub path: String,
    pub id: String,
    pub name: Option<String>,
    /// Latest activity, ms since the epoch.
    pub modified: i64,
    pub message_count: usize,
    pub first_message: String,
}

/// The session the TUI shows after it opened.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionOpened {
    pub session_id: String,
    pub session_file: Option<String>,
    pub name: Option<String>,
    /// Transcript messages (Pi JSON), oldest first.
    pub messages: Vec<Value>,
}

/// Lane to TUI.
#[derive(Clone, Debug, PartialEq)]
pub enum LaneEvent {
    /// A session was opened (startup, `/new`, resume).
    Opened(SessionOpened),
    /// A turn started (operation id).
    OperationStarted(String),
    /// The streaming assistant message so far.
    Streaming(Value),
    /// A message ended and joined the transcript.
    Message(Value),
    /// A tool call started executing.
    ToolStarted { id: String, name: String, args: Value },
    /// A tool call ended (its result message follows as [`LaneEvent::Message`]).
    ToolEnded { id: String },
    /// The queued user messages now.
    Queues(Vec<Queued>),
    /// The turn ended; `error` when it failed to run at all.
    OperationEnded { error: Option<String> },
    /// The session's display name changed (`None` cleared).
    Name(Option<String>),
    /// The selected model changed.
    Model(ModelInfo),
    /// The sessions of this working directory, newest first.
    Sessions(Vec<SessionListing>),
    /// A line for the status area (`showStatus`).
    Status(String),
    /// A failure outside a turn.
    Error(String),
}

/// An image attached to a prompt (`ImageContent`: base64 data and MIME type).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptImage {
    pub data: String,
    pub mime_type: String,
}

/// TUI to lane. Every call returns at once; results arrive as [`LaneEvent`]s.
pub trait LaneClient {
    /// Start a turn. The first prompt of an unnamed session also names it (shown at once, refined in the
    /// background), unless the user names it first.
    fn prompt(&self, text: &str);
    /// Start a turn with images (the draft's `[Image #N]` attachments). Lanes without image input drop them.
    fn prompt_with_images(&self, text: &str, images: Vec<PromptImage>) {
        let _ = images;
        self.prompt(text);
    }
    /// Queue a steering message into the running turn; returns its entry id.
    fn steer(&self, text: &str) -> String;
    /// Queue a follow-up for when the running turn would stop; returns its entry id.
    fn follow_up(&self, text: &str) -> String;
    /// Drop a queued message.
    fn cancel_queued(&self, entry_id: &str);
    /// Abort the running turn.
    fn abort(&self);
    /// Start a new session.
    fn new_session(&self);
    /// Switch to a stored session.
    fn resume(&self, path: &str);
    /// List the sessions of this working directory ([`LaneEvent::Sessions`]).
    fn list_sessions(&self);
    /// Name the session (a user-given name: an automatic name never replaces it).
    fn set_name(&self, name: &str);
    /// Switch the session's model.
    fn select_model(&self, provider: &str, id: &str);
}
