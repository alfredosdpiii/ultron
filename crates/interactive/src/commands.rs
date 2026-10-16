//! The client TUI's slash commands as the editor's autocomplete lists them (the order the TypeScript facets register
//! them in: client-tui-commands.ts, client-tui-pi-commands.ts, the TUI's own, the built-in facet's, the
//! inspection commands and the session worker's), the `/hotkeys` table and the `/session` text.

use std::rc::Rc;

use tui::components::{Component, Spacer, Text};
use tui::keybindings::KeybindingsManager;
use tui::markdown::{Markdown, MarkdownOptions};
use tui::marked::markdown_tokens;
use tui::theme::Theme;

use crate::components::{bold, fg, format_key_text, format_tokens, js_to_fixed, markdown_theme};

/// One command: name, argument hint and description.
pub struct CommandSpec {
    pub name: &'static str,
    pub hint: &'static str,
    pub description: &'static str,
}

const fn c(name: &'static str, hint: &'static str, description: &'static str) -> CommandSpec {
    CommandSpec {
        name,
        hint,
        description,
    }
}

/// Every command the TypeScript client TUI offers, in its order.
/// The Rust-only commands, offered with the overlays ([`crate::qol::QolSettings::overlays`]); a parity build has
/// exactly the client TUI's [`COMMANDS`].
pub const QOL_COMMANDS: [CommandSpec; 1] = [c(
    "mode",
    "[name]",
    "Switch mode (Default, Engineering); without a name, pick one",
)];

pub const COMMANDS: [CommandSpec; 38] = [
    c("name", "<name>", "Set session display name"),
    c("session", "", "Show session info and stats"),
    c(
        "usage",
        "",
        "What this session did: depth (frames, sub-agents), cells, tokens and cost, guardrails",
    ),
    c(
        "goal",
        "[objective | pause | resume | clear | check <command>]",
        "Set a goal the agent works on round after round (pause, resume, clear, check <command>)",
    ),
    c("copy", "", "Copy last agent message to clipboard"),
    c(
        "export",
        "[path]",
        "Export session (HTML default, or specify path: .html/.jsonl)",
    ),
    c("hotkeys", "", "Show all keyboard shortcuts"),
    c("changelog", "", "Show changelog entries"),
    c("quit", "", "Quit ultron"),
    c("settings", "", "Open settings menu"),
    c("login", "<provider>", "Configure provider authentication"),
    c("logout", "", "Remove provider authentication"),
    c("share", "", "Share session as a secret GitHub gist"),
    c("resume", "", "Resume a different session"),
    c(
        "import",
        "[path.jsonl]",
        "Import a Pi session (JSONL) as a native session",
    ),
    c(
        "engineering",
        "[true|false]",
        "Engineering mode: put the always-on skills (alwaysSkills) in the system prompt",
    ),
    c(
        "autoreview",
        "[on|off]",
        "Automatic pull-request reviews as your gh account: on (picks the account, starts the service), off, or the status",
    ),
    c("trust", "", "Save project trust decision for future sessions"),
    c("scoped-models", "", "Enable/disable models for Ctrl+P cycling"),
    c("debug", "", "Write a debug log (TUI, Session worker and server state)"),
    c("new", "", "Start a new session"),
    c(
        "rlm",
        "",
        "Toggle the live RLM graph panel; /rlm focus opens it full screen, /rlm pane the RLM pane",
    ),
    c("tree", "", "Navigate the session tree (switch branches)"),
    c("fork", "", "Create a new fork from a previous user message"),
    c("clone", "", "Duplicate the current session at the current position"),
    c("model", "<provider/model>", "Select model"),
    c("agents", "[task-id]", "Show Ultron tasks, definitions, and usage"),
    c(
        "memory",
        "why <task-id> | list",
        "Show recorded memory evidence without a new search",
    ),
    c(
        "skills",
        "why <decision-id> | list",
        "Show skill versions or why a skill was selected",
    ),
    c("experiments", "", "Show recorded experiment attempts"),
    c("goals", "[goal-id]", "Show goals and their verification state"),
    c(
        "progress",
        "<task-id>",
        "Show progress receipts and decisions for a task",
    ),
    c("extension", "<name> [args]", "Run a loaded Pi extension command"),
    c(
        "mcp",
        "",
        "MCP servers: status, connect or disconnect, sign in, enable or disable",
    ),
    c("thinking", "<level>", "Set thinking level"),
    c("compact", "<instructions>", "Manually compact the session context"),
    c(
        "review",
        "[base-ref|PR|path...] [--only bugs,security,arch,tests,ai] [--budget 300k] [--model p/m] [--post]",
        "Review code changes: specialist reviewer frames, then a verifier per finding",
    ),
    c("reload", "", "Reload server-selected plugins"),
];

/// `keyDisplayText(keybinding)`: the binding's keys, capitalized.
pub fn key_display_text(keys: &KeybindingsManager, keybinding: &str) -> String {
    format_key_text(&keys.get_keys(keybinding).join("/"), true, false)
}

/// `hotkeysMarkdown()`.
pub fn hotkeys_markdown(keys: &KeybindingsManager) -> String {
    let k = |id: &str| key_display_text(keys, id);
    format!(
        "
**Navigation**
| Key | Action |
|-----|--------|
| `{}` / `{}` | Move cursor / browse prompt history |
| `{}` | Reverse-search prompt history |
| `{}` / `{}` | Move by word |
| `{}` | Start of line |
| `{}` | End of line |
| `{}` | Jump forward to character |
| `{}` | Jump backward to character |
| `{}` / `{}` | Scroll by page |

**Editing**
| Key | Action |
|-----|--------|
| `{}` | Send message (steers while a turn runs) |
| `{}` | New line |
| `{}` | Delete word backwards |
| `{}` | Delete word forwards |
| `{}` | Delete to start of line |
| `{}` | Delete to end of line |
| `{}` | Paste the most-recently-deleted text |
| `{}` | Cycle through the deleted text after pasting |
| `{}` | Undo |

**Other**
| Key | Action |
|-----|--------|
| `{}` | Path completion / accept autocomplete |
| `{}` | Cancel autocomplete / abort the turn |
| `{}` | Clear editor (first) / exit (second) |
| `{}` | Exit (when editor is empty) |
| `{}` | Suspend to background |
| `{}` | Cycle thinking level |
| `{}` / `{}` | Cycle models |
| `{}` | Open model selector |
| `{}` | Toggle tool output expansion |
| `{}` | Toggle thinking block visibility |
| `{}` | Edit message in external editor |
| `{}` | Copy last assistant message |
| `{}` | Queue follow-up message |
| `{}` | Restore queued messages |
| `{}` | Paste image or text from clipboard |
| `{}` | Toggle the live RLM panel |
| `{}` | Open the full-screen RLM graph |
| `{}` | Open the RLM pane beside the chat, or move the focus between it and the chat |
| `/` | Slash commands |
| `/usage` | What this session did: depth (frames, sub-agents), cells, tokens and cost, guardrails |
| `!` | Run bash command |
| `!!` | Run bash command (excluded from context) |
",
        k("tui.editor.cursorUp"),
        k("tui.editor.cursorDown"),
        k("app.history.search"),
        k("tui.editor.cursorWordLeft"),
        k("tui.editor.cursorWordRight"),
        k("tui.editor.cursorLineStart"),
        k("tui.editor.cursorLineEnd"),
        k("tui.editor.jumpForward"),
        k("tui.editor.jumpBackward"),
        k("tui.editor.pageUp"),
        k("tui.editor.pageDown"),
        k("tui.input.submit"),
        k("tui.input.newLine"),
        k("tui.editor.deleteWordBackward"),
        k("tui.editor.deleteWordForward"),
        k("tui.editor.deleteToLineStart"),
        k("tui.editor.deleteToLineEnd"),
        k("tui.editor.yank"),
        k("tui.editor.yankPop"),
        k("tui.editor.undo"),
        k("tui.input.tab"),
        k("app.interrupt"),
        k("app.clear"),
        k("app.exit"),
        k("app.suspend"),
        k("app.thinking.cycle"),
        k("app.model.cycleForward"),
        k("app.model.cycleBackward"),
        k("app.model.select"),
        k("app.tools.expand"),
        k("app.thinking.toggle"),
        k("app.editor.external"),
        k("app.message.copy"),
        k("app.message.followUp"),
        k("app.message.dequeue"),
        k("app.clipboard.pasteImage"),
        k("app.rlm.toggle"),
        k("app.rlm.focus"),
        k("app.rlm.pane"),
    )
}

/// `markdownNotice(title, markdown)`: a bordered titled Markdown block.
pub struct MarkdownNotice {
    pub theme: Rc<Theme>,
    pub title: String,
    pub markdown: String,
}

impl Component for MarkdownNotice {
    fn render(&mut self, width: usize) -> Vec<String> {
        notice_lines(&self.theme, &self.title, markdown_tokens(self.markdown.trim()), width)
    }
}

/// A [`MarkdownNotice`] whose Markdown was lexed already (`/changelog`, lexed off the UI thread).
pub struct LexedNotice {
    pub theme: Rc<Theme>,
    pub title: String,
    pub tokens: Vec<tui::markdown::Token>,
}

impl Component for LexedNotice {
    fn render(&mut self, width: usize) -> Vec<String> {
        notice_lines(&self.theme, &self.title, self.tokens.clone(), width)
    }
}

fn notice_lines(t: &Rc<Theme>, title: &str, tokens: Vec<tui::markdown::Token>, width: usize) -> Vec<String> {
    let border = fg(t, "border", &"─".repeat(width.max(1)));
    let mut lines = vec![border.clone()];
    lines.extend(Text::new(&bold(&fg(t, "accent", title)), 1, 0, None).render(width));
    lines.extend(Spacer(1).render(width));
    let mut md = Markdown {
        tokens,
        padding_x: 1,
        padding_y: 1,
        theme: markdown_theme(t),
        style: None,
        options: MarkdownOptions::default(),
    };
    lines.extend(md.render(width));
    lines.push(border);
    lines
}

/// What `/session` reports (`SessionStats` over the session's entries).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionStats {
    pub session_file: Option<String>,
    pub session_id: String,
    pub name: Option<String>,
    pub user_messages: usize,
    pub assistant_messages: usize,
    pub tool_calls: usize,
    pub tool_results: usize,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost: f64,
    /// (tokens, context window, percent) when the model's window is known.
    pub context: Option<(Option<u64>, u64, Option<f64>)>,
}

/// `toLocaleString()` for an integer in the en-US locale.
fn locale(n: u64) -> String {
    crate::components::group_thousands(n)
}

/// Pi's `/session` text.
pub fn session_info(t: &Theme, s: &SessionStats) -> String {
    let dim = |x: &str| fg(t, "dim", x);
    let mut info = format!("{}\n\n", bold("Session Info"));
    if let Some(name) = &s.name {
        info += &format!("{} {name}\n", dim("Name:"));
    }
    info += &format!(
        "{} {}\n",
        dim("File:"),
        s.session_file.as_deref().unwrap_or("In-memory")
    );
    info += &format!("{} {}\n\n", dim("ID:"), s.session_id);
    info += &format!("{}\n", bold("Messages"));
    info += &format!(
        "{} {}\n",
        dim("Total:"),
        s.user_messages + s.assistant_messages + s.tool_results
    );
    info += &format!("{} {}\n", dim("User:"), s.user_messages);
    info += &format!("{} {}\n", dim("Assistant:"), s.assistant_messages);
    info += &format!(
        "{} {} calls, {} results\n\n",
        dim("Tools:"),
        s.tool_calls,
        s.tool_results
    );
    info += &format!("{}\n", bold("Tokens"));
    let prompt = s.input + s.cache_read + s.cache_write;
    info += &format!("{} {}\n", dim("Input:"), locale(prompt));
    if prompt > 0 && (s.cache_read > 0 || s.cache_write > 0) {
        let rate = dim(&format!(
            "({}%)",
            js_to_fixed(s.cache_read as f64 / prompt as f64 * 100.0, 1)
        ));
        info += &format!("  {} {} {rate}\n", dim("Cached:"), locale(s.cache_read));
        info += &format!("  {} {}\n", dim("Uncached:"), locale(s.input + s.cache_write));
    }
    info += &format!("{} {}\n", dim("Output:"), locale(s.output));
    info += &format!(
        "{} {}\n",
        dim("Total:"),
        locale(s.input + s.output + s.cache_read + s.cache_write)
    );
    if let Some((tokens, window, percent)) = s.context {
        let used = tokens.map_or("?".to_string(), format_tokens);
        let percent = percent.map_or(String::new(), |p| format!(" ({}%)", js_to_fixed(p, 1)));
        info += &format!(
            "\n{}\n{} {used}/{}{percent}\n",
            bold("Context"),
            dim("Used:"),
            format_tokens(window)
        );
    }
    if s.cost > 0.0 {
        info += &format!("\n{}\n{} ${}", bold("Cost"), dim("Total:"), js_to_fixed(s.cost, 3));
    }
    info.trim_end().to_string()
}
