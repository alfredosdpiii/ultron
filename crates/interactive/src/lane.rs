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

impl model_scope::ScopeModel for ModelInfo {
    fn provider(&self) -> &str {
        &self.provider
    }
    fn id(&self) -> &str {
        &self.id
    }
    fn name(&self) -> &str {
        &self.name
    }
    /// The Claude Code CLI provider's models (`claude-code`, all on its `claude-code-cli` API).
    fn is_claude_code(&self) -> bool {
        self.provider == "claude-code"
    }
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

/// `/bug`'s choices (bug-report.ts `BugReportOptions`, with the summary written).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BugReportRequest {
    pub hint: Option<String>,
    pub include_session: bool,
    /// The model's summary, when the user chose one instead of the transcript.
    pub summary: Option<String>,
    /// Upload it (else export a zip).
    pub upload: bool,
}

/// How a `/bug` report's delivery ended.
#[derive(Clone, Debug, PartialEq)]
pub enum BugReportOutcome {
    /// Uploaded: the report id.
    Uploaded(String),
    /// Exported: the archive and the report id.
    Exported { path: String, id: String },
    /// The upload failed (the fallback is offered).
    UploadFailed(String),
    /// Building or writing the report failed (`showError`).
    Failed(String),
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
    /// `steer`, `followUp` or `nextRun`.
    pub kind: QueueKind,
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueKind {
    Steer,
    FollowUp,
    /// Held for the session's next run.
    NextRun,
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
    /// Every user and assistant message's text, joined by spaces (`SessionInfo.allMessagesText`); the selector
    /// searches it.
    pub all_messages_text: String,
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

/// What a login flow reported (`AuthEvent`), as the dialog shows it.
/// A notice interactive-mode.ts shows around startup.
#[derive(Clone, Debug, PartialEq)]
pub enum StartupNotice {
    /// `showStartupNoticesIfNeeded`: the new changelog entries; `collapse` is the `collapseChangelog` setting.
    WhatsNew {
        markdown: String,
        collapse: bool,
        version: String,
    },
    /// `showNewVersionNotification`: the newer release's version.
    NewVersion(String),
    /// `showWarning` with the Anthropic subscription-auth warning (once per run).
    SubscriptionAuth,
}

/// How a `/scoped-models` catalog refresh ended.
#[derive(Clone, Debug, PartialEq)]
pub enum CatalogRefresh {
    /// Finished; the providers that could not be refreshed (`result.errors`).
    Done(Vec<String>),
    /// The 15 s budget ran out (`result.aborted && timedOut`).
    TimedOut,
    /// The refresh itself failed (the `.catch` branch).
    Failed(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum LoginEvent {
    AuthUrl {
        url: String,
        instructions: Option<String>,
    },
    DeviceCode {
        user_code: String,
        verification_uri: String,
    },
    /// A message and its links (`label`, `url`).
    Info {
        message: String,
        links: Vec<(Option<String>, String)>,
    },
    Progress(String),
}

/// What a login flow asks (`AuthPrompt`).
#[derive(Clone, Debug, PartialEq)]
pub enum LoginPrompt {
    /// Choose one option (`id`, `label`).
    Select {
        message: String,
        options: Vec<(String, String)>,
    },
    /// Paste the authorization code or redirect URL.
    ManualCode { message: String },
    /// A text or secret answer.
    Text {
        message: String,
        placeholder: Option<String>,
        secret: bool,
    },
}

/// One step of a subscription login.
#[derive(Clone, Debug, PartialEq)]
pub enum LoginStep {
    /// The dialog opens (`reach`: `browserReach()`, `local`, `container` or `ssh`).
    Started {
        name: String,
        reach: String,
    },
    Event(LoginEvent),
    Prompt(LoginPrompt),
    /// The flow ended (success, failure or cancel); its outcome arrives as statuses.
    Finished,
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
    /// Pi's own sessions of this working directory (`/import` without a path), newest first.
    PiSessions(Vec<SessionListing>),
    /// A line for the status area (`showStatus`).
    Status(String),
    /// The recovery of a Radius-hosted server's connection (client-tui.ts `#handleConnectionState` /
    /// `#handleAttachmentState`): while `busy`, input waits; `status` replaces the status line (empty clears it).
    Recovery { busy: bool, status: String },
    /// A failure outside a turn.
    Error(String),
    /// Command output for the transcript (`host.notice(new Text(text, 1, 0))`).
    Notice(String),
    /// The session goal's reply to `/goal` (shown dim, as client-tui-commands.ts does).
    GoalReply(String),
    /// A `!` command finished (its message follows as [`LaneEvent::Message`] with role `bashExecution`), or failed
    /// to run.
    BashDone(Result<BashResult, String>),
    /// An extension's `user_bash` failed a `!` command closed (it did not run; the extension's error is shown as
    /// [`LaneEvent::ExtensionError`]).
    BashFailedClosed,
    /// A handler of the session's extensions failed (`extension_error`).
    ExtensionError { path: String, error: String },
    /// `/bug`'s summary: the text, or why there is none.
    BugSummary(Result<String, String>),
    /// `/bug`'s report was delivered, or not.
    BugReport(BugReportOutcome),
    /// A compaction started (`compaction_start`): `reason` is `manual`, or `threshold` / `overflow` for one the
    /// session runs on its own during a run.
    CompactionStarted { reason: String },
    /// An automatic compaction ended without a compaction entry (`compaction_end`): `declined` (a hook trimmed the
    /// context instead), `failed` with its error, or `aborted`.
    CompactionEnded { status: String, error: Option<String> },
    /// A compaction ended: `Ok` with the compaction entry (`entry_added` of a `compaction` entry), else why it was
    /// rejected.
    Compacted(Result<Value, String>),
    /// A compaction's or branch summary's request failed and is retried (`summarization_retry_scheduled`): the
    /// retry's number, the retries allowed, the delay and the provider's error.
    SummarizationRetryScheduled {
        attempt: u64,
        max_attempts: u64,
        delay_ms: u64,
        error: String,
    },
    /// The retry starts (`summarization_retry_attempt_start`): of a branch summary, or of a compaction for `reason`.
    SummarizationRetryStarted { branch_summary: bool, reason: String },
    /// The summary's retries ended (`summarization_retry_finished`).
    SummarizationRetryFinished,
    /// The run's request failed and is retried after `delay_ms` (`auto_retry_start`).
    AutoRetryStart {
        attempt: u64,
        max_attempts: u64,
        delay_ms: u64,
    },
    /// The run's retries ended (`auto_retry_end`): `Some(error)` when the last attempt failed too.
    AutoRetryEnd { attempt: u64, failed: Option<String> },
    /// The session's thinking level changed.
    Thinking(String),
    /// The user messages `/fork` offers (every branch), oldest first.
    ForkMessages(Vec<ForkMessage>),
    /// The session tree (Pi's `getTree()` nodes, labels resolved) and its current leaf, for `/tree`.
    Tree { tree: Value, leaf: Option<String> },
    /// A tree navigation ended (after the session reopened at its new position), with the selected user or custom
    /// message's text for the editor (AgentSession `navigateTree`'s `editorText`).
    Navigated(Result<Option<String>, String>),
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
    /// The settings' mode for `/mode` and `/engineering`: engineering mode and the always-on skills it names.
    Mode {
        engineering: bool,
        always_skills: Vec<String>,
    },
    /// The accounts `gh` is logged in to, as `(login, host)`, for `/autoreview on`.
    AutoreviewAccounts(Vec<(String, String)>),
    /// The stored credentials (`auth.json`): provider id and type (`api_key` or `oauth`), for `/logout`.
    Credentials(Vec<(String, String)>),
    /// What `/login` offers: every sign-in option with what is configured, for the query `/login` was given.
    LoginProviders {
        providers: Vec<crate::selectors::AuthProvider>,
        query: String,
    },
    /// The session's extension commands (`LegacyExtensionCommands.list`): invocation name and description, in
    /// load order; each becomes a slash command of its own.
    ExtensionCommands(Vec<(String, Option<String>)>),
    /// The extensions' keyboard shortcuts in force and the conflict warnings ([`LaneClient::extension_shortcuts`]).
    ExtensionShortcuts {
        shortcuts: Vec<crate::extension_shortcuts::ExtensionShortcut>,
        diagnostics: Vec<crate::extension_shortcuts::ShortcutDiagnostic>,
    },
    /// An extension command's argument completions for `prefix`: `(value, label, description)` items, or `None`
    /// when it offers none.
    ExtensionCompletions {
        name: String,
        prefix: String,
        items: Option<Vec<(String, String, Option<String>)>>,
    },
    /// A subscription login's progress (the flow runs in the client process).
    Login(LoginStep),
    /// What `/changelog` shows, lexed off the UI thread ([`crate::changelog::changelog_tokens`]).
    Changelog(Vec<tui::markdown::Token>),
    /// The models the session can use now (after a login or logout).
    Catalog(Vec<ModelInfo>),
    /// The model catalogs refreshed ([`LaneClient::refresh_catalog`]), or why not.
    CatalogRefreshed(CatalogRefresh),
    /// Configured packages behind their source, found by the startup update check (display names); the transcript
    /// shows [`crate::package_updates::PackageUpdateNotice`] when there are any.
    PackageUpdates(Vec<String>),
    /// A startup notice from the client ([`crate::startup_notices`]); shown once the session is open.
    StartupNotice(StartupNotice),
    /// The session's subagents for the RLM views (`rlm.list_subagents`: `{subagents: [...]}`).
    RlmStatus(Value),
    /// The session's subagents, for the agent hub.
    Agents(Vec<crate::agent_hub::AgentView>),
    /// A subagent's transcript (`agents.transcript`): its messages, for the agent hub's transcript view.
    AgentTranscript { id: String, messages: Vec<Value> },
    /// What the agent hub's steer or revive did (shown as the hub's notice).
    AgentNotice(String),
    /// The TUI moved to a fork or clone of the session (after [`LaneEvent::Opened`]): the editor text it starts with
    /// and the status to show.
    Forked { editor_text: String, status: String },
    /// `/import` imported a Pi session (`imported`: what to say about it, e.g. "Imported 16 entries from ... as session
    /// <id>"): `session_file` is the native session to switch to, `current` when that is the attached session.
    PiImported {
        imported: String,
        source: String,
        session_file: Option<String>,
        current: bool,
    },
    /// `/trust`: the session's trust choices, saved decision and current trust.
    Trust(crate::trust_selector::TrustView),
    /// `/debug` wrote its log to `path`; the notice shows it with `summary`.
    DebugLogWritten { path: String, summary: Vec<String> },
    /// `/share`'s gist upload started (`true`: the "Creating gist..." loader shows) or ended (`false`).
    ShareLoader(bool),
    /// An RLM inspection's answer ([`LaneClient::rlm_inspect`]).
    RlmInspected { tag: String, result: Result<Value, String> },
    /// Every project's sessions, newest first (the resume selector's All scope).
    AllSessions(Vec<SessionListing>),
    /// Pi's sessions of every project, newest first (`/import`'s All scope).
    PiAllSessions(Vec<SessionListing>),
    /// A dialog or notification of the session's extensions (`/mcp`), in Pi's RPC wire format
    /// (`extension_ui_request`: `select`, `confirm`, `input`, `editor`, `notify`, `set_editor_text`, ...).
    ExtensionUi(Value),
}

/// An image attached to a prompt (`ImageContent`: base64 data and MIME type).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptImage {
    pub data: String,
    pub mime_type: String,
}

/// TUI to lane. Every call returns at once; results arrive as [`LaneEvent`]s.
pub trait LaneClient {
    /// Answer an extension dialog (`ultron.extension-ui.respond`): `{value}`, `{confirmed}` or `{cancelled: true}`.
    fn extension_ui_respond(&self, _id: &str, _answer: Value) {}
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
    /// Stop the run's retry wait (`abortRetry`); the run then ends with its error.
    fn abort_retry(&self) {
        self.abort();
    }
    /// Start a new session.
    fn new_session(&self);
    /// `/import <path>`: import the Pi session file at `path` through the server ([`LaneEvent::PiImported`] or a
    /// [`LaneEvent::Status`] error answers).
    fn import_pi(&self, path: &str) {
        let _ = path;
    }
    /// `/import` without a path: list Pi's sessions of this working directory for Pi's picker
    /// ([`LaneEvent::PiSessions`]). Pi's files are only read.
    fn list_pi_sessions(&self) {}
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
    /// `/goal [objective | pause | resume | clear | check <command>]`: the session goal's command
    /// ([`LaneEvent::GoalReply`] answers).
    fn goal(&self, args: &str) {
        let _ = args;
    }
    /// Read the session tree ([`LaneEvent::Tree`]).
    fn tree(&self) {}
    /// Navigate to the selected tree entry `target` (AgentSession `navigateTree`: before a user or custom message,
    /// else onto the entry; `None`: the root), optionally summarizing the branch it leaves
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
    /// A read-only inspection request (`/memory list`, `/agents <id>`, ...); its answer, rendered under `title`
    /// ([`crate::inspection::format_inspection`]), arrives as a [`LaneEvent::Status`].
    fn inspect(&self, title: &str, request: &str, payload: Value) {
        let _ = (title, request, payload);
    }
    /// Poll the session's subagents for the RLM views ([`LaneEvent::RlmStatus`]).
    fn rlm_status(&self) {}
    /// Read the settings for `/settings` ([`LaneEvent::Settings`]).
    fn read_settings(&self) {}
    /// Read the settings' mode for `/mode` and `/engineering` ([`LaneEvent::Mode`]).
    fn read_mode(&self) {}
    /// `/autoreview`: answered by [`LaneEvent::AutoreviewAccounts`] for [`AutoreviewRequest::Accounts`], else by a
    /// [`LaneEvent::Status`].
    fn autoreview(&self, request: AutoreviewRequest) {
        let _ = request;
    }
    /// The sign-in options for `/login` ([`LaneEvent::LoginProviders`]).
    fn login_providers(&self, query: &str) {
        let _ = query;
    }
    /// Start a subscription (OAuth) login for `provider` (`name` is what the dialog says): [`LaneEvent::Login`]
    /// steps, then statuses and a fresh [`LaneEvent::Catalog`] answer. `false` when this client cannot sign in to
    /// that provider with an account.
    fn oauth_login(&self, provider: &str, name: &str) -> bool {
        let _ = (provider, name);
        false
    }
    /// Refresh the model catalogs (`models.refresh`): a fresh [`LaneEvent::Catalog`], then
    /// [`LaneEvent::CatalogRefreshed`].
    fn refresh_catalog(&self) {}
    /// The answer to the login's pending prompt; `None` cancels the login.
    fn login_reply(&self, answer: Option<String>) {
        let _ = answer;
    }
    /// Save an API key for a provider; statuses and a fresh [`LaneEvent::Catalog`] answer.
    fn save_api_key(&self, provider: &str, key: &str) {
        let _ = (provider, key);
    }
    /// Read the changelog ([`LaneEvent::Changelog`]).
    fn changelog(&self) {}
    /// `getChangelogForDisplay` for a session that opened without messages: the entries newer than the
    /// `lastChangelogVersion` setting (which it records), as [`LaneEvent::StartupNotice`] when there are any.
    fn startup_changelog(&self) {}
    /// `maybeWarnAboutAnthropicSubscriptionAuth` for the session's model's `provider`: a
    /// [`StartupNotice::Warning`] when its credential is a subscription login and the warning is on.
    fn subscription_auth_warning(&self, provider: &str) {
        let _ = provider;
    }
    /// Run an extension's command `/name args` (`LegacyExtensionCommands.run`); statuses answer. `/mcp`'s dialogs
    /// and notices arrive as [`LaneEvent::ExtensionUi`].
    fn extension_command(&self, name: &str, args: &str) {
        let _ = (name, args);
    }
    /// Ask for the extensions' shortcuts under the TUI's effective keybindings (action, keys);
    /// [`LaneEvent::ExtensionShortcuts`] answers.
    fn extension_shortcuts(&self, keybindings: Vec<(String, Vec<String>)>) {
        let _ = keybindings;
    }
    /// Run the extension shortcut bound to `key` (`shortcuts/run`) without waiting for it; a failure answers as
    /// [`LaneEvent::Error`] `Shortcut handler error: <message>`.
    fn extension_shortcut(&self, key: &str, keybindings: Vec<(String, Vec<String>)>) {
        let _ = (key, keybindings);
    }
    /// Ask for an extension command's argument completions for `prefix` (`commands/complete`);
    /// [`LaneEvent::ExtensionCompletions`] answers.
    fn extension_completions(&self, name: &str, prefix: &str) {
        let _ = (name, prefix);
    }
    /// `/share`: export the session and upload it as a secret gist with `gh`; statuses answer, and
    /// [`LaneEvent::ShareLoader`] while the gist is created.
    fn share(&self) {}
    /// Esc on the share loader: stop the gist upload; nothing more is reported about it.
    fn cancel_share(&self) {}
    /// `/bug`: the host an upload goes to (`new URL(getRadiusGatewayUrl()).host`) and whether the session is offline
    /// (`PI_OFFLINE`).
    fn bug_report_target(&self) -> (String, bool) {
        ("radius.pi.dev".into(), false)
    }
    /// `/bug`'s summary written by the session's model (`summarizeForBugReport`); [`LaneEvent::BugSummary`] answers.
    fn bug_report_summary(&self, _hint: Option<&str>) {}
    /// Stop the summary in progress; its answer is then ignored.
    fn abort_bug_report_summary(&self) {}
    /// Build the report and upload or export it; [`LaneEvent::BugReport`] answers (an upload stopped with
    /// [`LaneClient::cancel_bug_report`] answers nothing).
    fn bug_report(&self, _request: BugReportRequest) {}
    /// Export the report the failed upload built as a zip; [`LaneEvent::BugReport`] answers.
    fn bug_report_export(&self) {}
    /// Esc on the upload loader: stop the upload.
    fn cancel_bug_report(&self) {}
    /// Save a credential `ModelRuntime.login` produced (`{type: "api_key", key?, env?}`) for `provider`; statuses and
    /// a fresh [`LaneEvent::Catalog`] answer as for [`LaneClient::save_api_key`].
    fn save_credential(&self, provider: &str, credential: Value) {
        let _ = (provider, credential);
    }
    /// A read-only inspection for the RLM views (`rlm.frames`, `rlm.pool`, `ctx.state`, `instances.list`), answered
    /// by [`LaneEvent::RlmInspected`] with `tag`.
    fn rlm_inspect(&self, tag: &str, request: &str, payload: Value) {
        let _ = (tag, request, payload);
    }
    /// Every project's sessions for the resume selector's All scope ([`LaneEvent::AllSessions`]).
    fn list_all_sessions(&self) {}
    /// Pi's sessions of every project for `/import`'s All scope ([`LaneEvent::PiAllSessions`]).
    fn list_all_pi_sessions(&self) {}
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
    /// Steer a subagent with `text` (`agent_message.send` with `steer: true`); the outcome arrives as a
    /// [`LaneEvent::AgentNotice`].
    fn steer_agent(&self, id: &str, text: &str) {
        let _ = (id, text);
    }
    /// Start an interrupted subagent again from its brief; the outcome arrives as a [`LaneEvent::AgentNotice`].
    fn revive_agent(&self, id: &str) {
        let _ = id;
    }
    /// Read a subagent's transcript ([`LaneEvent::AgentTranscript`]).
    fn agent_transcript(&self, id: &str) {
        let _ = id;
    }
    /// Save a global setting (Pi's `SettingsManager` writes).
    fn save_setting(&self, key: &str, value: Value) {
        let _ = (key, value);
    }
    /// `/trust`: read the worker's trust state ([`LaneEvent::Trust`], or a [`LaneEvent::Status`] error).
    fn read_trust(&self) {}
    /// Save a `/trust` choice through the worker (`setProjectTrust`); a [`LaneEvent::Status`] answers.
    fn set_project_trust(&self, trusted: bool, updates: Vec<(String, Option<bool>)>) {
        let _ = (trusted, updates);
    }
    /// `/debug`: add the worker's and server's state to what the TUI shows and write the log
    /// ([`LaneEvent::DebugLogWritten`], or a [`LaneEvent::Status`] error).
    fn write_debug_log(&self, capture: crate::debug_log::DebugCapture) {
        let _ = capture;
    }
    /// `/reload`: reload the server-selected presentation plugins and the session's plugins; statuses answer.
    fn reload_plugins(&self) {}
}

/// The diagnostic the Claude subscription router puts on the message where it switched routes
/// (`ai::claude_subscription::fallback::ROUTE_DIAGNOSTIC`).
pub const ROUTE_DIAGNOSTIC: &str = "claude_subscription_route";

/// The route notice an assistant message carries (`details.notice` of its `claude_subscription_route` diagnostic):
/// "Claude subscription limit reached; using API credits until <time>", or the switch back.
pub fn route_notice(message: &Value) -> Option<String> {
    if message["role"] != "assistant" {
        return None;
    }
    message["diagnostics"]
        .as_array()?
        .iter()
        .find(|d| d["type"] == ROUTE_DIAGNOSTIC || d["kind"] == ROUTE_DIAGNOSTIC)
        .and_then(|d| d["details"]["notice"].as_str())
        .map(str::to_string)
}

/// What `/autoreview` asks of the client (client-tui-pi-commands.ts `setAutoreview`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AutoreviewRequest {
    /// The setting, the accounts it reviews as and whether the user service is installed.
    Status,
    /// The `gh` accounts to choose from.
    Accounts,
    /// Save `autoreview.enabled: true` and `autoreview.accounts` (`None`: every logged-in account), then write and
    /// start the user service. `reviewing_as` is what the status names.
    On {
        accounts: Option<Vec<String>>,
        reviewing_as: String,
    },
    /// Save `autoreview.enabled: false`, stop the service and remove its file.
    Off,
}
