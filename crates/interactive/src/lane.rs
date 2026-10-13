//! What the interactive TUI exchanges with the session it shows, as plain data: events from the lane (Pi message
//! JSON, queue and operation changes) and the commands it sends ([`LaneClient`]). The TUI never links the engine;
//! the composition root implements [`LaneClient`] (today an in-process lane on its own thread, later the session
//! worker over the server protocol).

use serde_json::Value;

/// A model the session can switch to (`ModelSummary`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModelInfo {
    pub provider: String,
    pub id: String,
    pub name: String,
    pub reasoning: bool,
    pub context_window: u64,
    /// The thinking levels the model supports (`getThinkingLevels`), lowest first; empty: Pi's default set.
    pub thinking_levels: Vec<String>,
}

impl ModelInfo {
    /// The model's thinking levels: its own, else `off` for a model without reasoning, else Pi's five.
    pub fn levels(&self) -> Vec<String> {
        if !self.thinking_levels.is_empty() {
            return self.thinking_levels.clone();
        }
        let levels: &[&str] = if self.reasoning {
            &["off", "minimal", "low", "medium", "high"]
        } else {
            &["off"]
        };
        levels.iter().map(|l| (*l).to_string()).collect()
    }
}

/// What a `!` command left (`BashResult`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BashResult {
    pub output: String,
    pub exit_code: Option<i64>,
    pub cancelled: bool,
    pub truncated: bool,
    pub full_output_path: Option<String>,
}

/// A user message a fork can start before (`/fork`).
#[derive(Clone, Debug, PartialEq)]
pub struct ForkMessage {
    pub entry_id: String,
    pub text: String,
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
    /// The session's working directory.
    pub cwd: String,
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
    /// The opened session's lane entries on its branch (format-4 entries: messages, compactions with their retained
    /// tails, branch summaries); the transcript is drawn from them as the client TUI draws its lane snapshot.
    Entries(Vec<Value>),
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
    /// Command output for the transcript (`host.notice(new Text(text, 1, 0))`).
    Notice(String),
    /// A `!` command finished (its message follows as [`LaneEvent::Message`] with role `bashExecution`), or failed
    /// to run.
    BashDone(Result<BashResult, String>),
    /// A manual compaction started (`compaction_start`).
    CompactionStarted,
    /// A compaction ended: `Ok` with the compaction entry (`entry_added` of a `compaction` entry), else why it was
    /// rejected.
    Compacted(Result<Value, String>),
    /// The session's thinking level changed.
    Thinking(String),
    /// The user messages `/fork` offers (every branch), oldest first.
    ForkMessages(Vec<ForkMessage>),
    /// The session tree (Pi's `getTree()` nodes, labels resolved) and its current leaf, for `/tree`.
    Tree { tree: Value, leaf: Option<String> },
    /// A tree navigation ended (after the session reopened at its new position).
    Navigated(Result<(), String>),
    /// A session was deleted (`Err`: why not); a fresh [`LaneEvent::Sessions`] follows.
    SessionDeleted(Result<(), String>),
    /// The settings `/settings` shows: the merged settings document, the model settings environment variables
    /// override (key -> (variable, value)), the available themes and the terminal's appearance (`light`/`dark`).
    Settings {
        settings: Value,
        env_overrides: std::collections::BTreeMap<String, (String, String)>,
        themes: Vec<String>,
        terminal_theme: String,
    },
    /// The stored credentials (`auth.json`): provider id and type (`api_key` or `oauth`), for `/logout`.
    Credentials(Vec<(String, String)>),
    /// What `/login` offers: every sign-in option with what is configured, for the query `/login` was given.
    LoginProviders {
        providers: Vec<crate::selectors::AuthProvider>,
        query: String,
    },
    /// What `/changelog` shows, lexed off the UI thread ([`crate::changelog::changelog_tokens`]).
    Changelog(Vec<tui::markdown::Token>),
    /// The models the session can use now (after a login or logout).
    Catalog(Vec<ModelInfo>),
    /// The session's subagents for the RLM views (`rlm.list_subagents`: `{subagents: [...]}`).
    RlmStatus(Value),
    /// The session's subagents, for the agent hub.
    Agents(Vec<crate::agent_hub::AgentView>),
    /// The TUI moved to a fork or clone of the session (after [`LaneEvent::Opened`]): the editor text it starts with
    /// and the status to show.
    Forked { editor_text: String, status: String },
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
    /// [`LaneClient::steer`] with the draft's images.
    fn steer_with_images(&self, text: &str, images: Vec<PromptImage>) -> String {
        let _ = images;
        self.steer(text)
    }
    /// [`LaneClient::follow_up`] with the draft's images.
    fn follow_up_with_images(&self, text: &str, images: Vec<PromptImage>) -> String {
        let _ = images;
        self.follow_up(text)
    }
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
    /// Run a `!` command in the session's shell (`exclude`: `!!`, kept out of the model's context);
    /// [`LaneEvent::BashDone`] answers.
    fn bash(&self, command: &str, exclude: bool) {
        let _ = (command, exclude);
    }
    /// Cancel the running `!` command.
    fn abort_bash(&self) {}
    /// Compact the session context now ([`LaneEvent::Compacted`] answers).
    fn compact(&self, instructions: Option<&str>) {
        let _ = instructions;
    }
    /// Set the thinking level ([`LaneEvent::Thinking`] answers).
    fn set_thinking(&self, level: &str) {
        let _ = level;
    }
    /// List the user messages a fork can start before ([`LaneEvent::ForkMessages`]).
    fn fork_messages(&self) {}
    /// Fork the session before user message `entry_id` and switch to the fork.
    fn fork(&self, entry_id: &str) {
        let _ = entry_id;
    }
    /// Copy the session at its current position and switch to the copy.
    fn clone_session(&self) {}
    /// Export the session (`/export [path]`: HTML, or the Pi JSONL for a `.jsonl` path).
    fn export(&self, path: Option<&str>) {
        let _ = path;
    }
    /// The session report (`/usage`), as a [`LaneEvent::Notice`].
    fn usage_report(&self) {}
    /// Read the session tree ([`LaneEvent::Tree`]).
    fn tree(&self) {}
    /// Move the session to `target` (`None`: the root), optionally summarizing the branch it leaves
    /// ([`LaneEvent::Navigated`]).
    fn navigate(&self, target: Option<&str>, summarize: bool, instructions: Option<&str>) {
        let _ = (target, summarize, instructions);
    }
    /// Label a tree entry (`None` removes it).
    fn set_label(&self, entry_id: &str, label: Option<&str>) {
        let _ = (entry_id, label);
    }
    /// List the session's subagents ([`LaneEvent::Agents`]).
    fn agents(&self) {}
    /// Poll the session's subagents for the RLM views ([`LaneEvent::RlmStatus`]).
    fn rlm_status(&self) {}
    /// Read the settings for `/settings` ([`LaneEvent::Settings`]).
    fn read_settings(&self) {}
    /// The sign-in options for `/login` ([`LaneEvent::LoginProviders`]).
    fn login_providers(&self, query: &str) {
        let _ = query;
    }
    /// Save an API key for a provider; statuses and a fresh [`LaneEvent::Catalog`] answer.
    fn save_api_key(&self, provider: &str, key: &str) {
        let _ = (provider, key);
    }
    /// Read the changelog ([`LaneEvent::Changelog`]).
    fn changelog(&self) {}
    /// `/share`: export the session and upload it as a secret gist with `gh`; statuses answer.
    fn share(&self) {}
    /// List the stored credentials ([`LaneEvent::Credentials`]).
    fn credentials(&self) {}
    /// Remove a stored credential; a [`LaneEvent::Status`] and a fresh [`LaneEvent::Catalog`] answer.
    fn logout(&self, provider: &str, done: &str) {
        let _ = (provider, done);
    }
    /// Delete a stored session (not the attached one); [`LaneEvent::SessionDeleted`] answers.
    fn delete_session(&self, path: &str) {
        let _ = path;
    }
    /// Rename a stored session (the attached one through [`LaneClient::set_name`]); [`LaneEvent::Sessions`] answers
    /// with the fresh listing.
    fn rename_session(&self, path: &str, name: &str) {
        let _ = (path, name);
    }
    /// Abort and release a subagent (and its unfinished descendants).
    fn kill_agent(&self, id: &str) {
        let _ = id;
    }
    /// Save a global setting (Pi's `SettingsManager` writes).
    fn save_setting(&self, key: &str, value: Value) {
        let _ = (key, value);
    }
}
