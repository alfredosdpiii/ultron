//! `/bug [description]` (modes/interactive/bug-report.ts `reportBug`): the consent and options dialogs in the
//! editor's place, the loaders while the summary is written or the report uploads, and the fallback offer after a
//! failed upload. The report itself is the client's ([`crate::lane::LaneClient::bug_report`]).

use std::rc::Rc;

use tui::components::{Component, Text};
use tui::keybindings::KeybindingsManager;
use tui::theme::Theme;

use crate::components::{bold, fg, format_key_text, key_hint, key_text};
use crate::extension_dialogs::{BorderedLoader, DialogOutcome, ExtensionEditorDialog};

pub const DISCLAIMER: &str = "This report goes to the Pi developers (Earendil) and is not shared publicly. It includes your pi version, operating system, the current model and provider configuration (without API keys), loaded extensions, settings, and provider error diagnostics from this session.";
pub const TRANSCRIPT_NOTE: &str = "The transcript contains your messages, model output, tool calls and their results, including file contents and command output read during this session.";

/// `ExtensionSelectorComponent` with a description: a bold accent title, the description, the options (`→ ` marks
/// the selected one), the key hints, between borders.
pub struct SelectorDialog {
    theme: Rc<Theme>,
    keys: KeybindingsManager,
    title: String,
    description: Option<String>,
    options: Vec<String>,
    selected: usize,
}

impl SelectorDialog {
    pub fn new(
        theme: Rc<Theme>,
        keys: KeybindingsManager,
        title: &str,
        options: &[&str],
        description: Option<&str>,
    ) -> Self {
        SelectorDialog {
            theme,
            keys,
            title: title.to_string(),
            description: description.filter(|d| !d.is_empty()).map(str::to_string),
            options: options.iter().map(|o| o.to_string()).collect(),
            selected: 0,
        }
    }

    pub fn handle_input(&mut self, data: &str) -> DialogOutcome {
        if self.keys.matches(data, "tui.select.up") || data == "k" {
            self.selected = self.selected.saturating_sub(1);
        } else if self.keys.matches(data, "tui.select.down") || data == "j" {
            self.selected = (self.selected + 1).min(self.options.len().saturating_sub(1));
        } else if self.keys.matches(data, "tui.select.confirm") || data == "\n" {
            if let Some(option) = self.options.get(self.selected) {
                return DialogOutcome::Submit(option.clone());
            }
        } else if self.keys.matches(data, "tui.select.cancel") {
            return DialogOutcome::Cancel;
        }
        DialogOutcome::None
    }

    pub fn render(&self, width: usize) -> Vec<String> {
        let t = &self.theme;
        let border = fg(t, "border", &"─".repeat(width.max(1)));
        let text = |s: &str| Text::new(s, 1, 0, None).render(width);
        let mut lines = vec![border.clone(), String::new()];
        lines.extend(text(&fg(t, "accent", &bold(&self.title))));
        if let Some(description) = &self.description {
            lines.push(String::new());
            lines.extend(text(&fg(t, "text", description)));
        }
        lines.push(String::new());
        for (i, option) in self.options.iter().enumerate() {
            let line = if i == self.selected {
                format!("{}{}", fg(t, "accent", "→ "), fg(t, "accent", option))
            } else {
                format!("  {}", fg(t, "text", option))
            };
            lines.extend(text(&line));
        }
        lines.push(String::new());
        let raw = format!(
            "{}{}",
            fg(t, "dim", &format_key_text("↑↓", false, false)),
            fg(t, "muted", " navigate")
        );
        let hints = format!(
            "{raw}  {}  {}",
            key_hint(t, &key_text(&self.keys, "tui.select.confirm", false), "select"),
            key_hint(t, &key_text(&self.keys, "tui.select.cancel", false), "cancel")
        );
        lines.extend(text(&hints));
        lines.push(String::new());
        lines.push(border);
        lines
    }
}

/// Where the flow is.
pub enum Step {
    /// "Report a bug": what went wrong (optional).
    Hint(Box<ExtensionEditorDialog>),
    Transcript(SelectorDialog),
    Summary(SelectorDialog),
    Delivery(SelectorDialog),
    /// "Writing summary with ...": the model writes the summary.
    Summarizing(BorderedLoader),
    /// "Uploading bug report...".
    Uploading(BorderedLoader),
    /// "Upload failed": export as a zip instead?
    UploadFailed(SelectorDialog),
}

/// What the user decided so far.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Choices {
    /// The trimmed description (`None`: none given).
    pub hint: Option<String>,
    pub include_session: bool,
    pub include_summary: bool,
    pub upload: bool,
}

/// What the app does after a key in the flow.
#[derive(Debug, PartialEq)]
pub enum Action {
    None,
    /// The flow ended without a report (`Bug report cancelled`).
    Cancelled,
    /// Upload chosen in offline mode (`showError`).
    Offline,
    /// Ask the session's model for the summary.
    Summarize,
    /// Esc on the summary loader: stop it.
    CancelSummary,
    /// Build and deliver the report (`Choices::upload`: upload it, else export a zip).
    Deliver,
    /// Esc on the upload loader: stop it.
    CancelUpload,
    /// Export the report the failed upload built.
    ExportAfterFailure,
    /// Ctrl+G in the description: edit this text in the external editor.
    External(String),
}

/// The `/bug` flow.
pub struct BugFlow {
    theme: Rc<Theme>,
    keys: KeybindingsManager,
    pub step: Step,
    pub choices: Choices,
    /// The current model's name and provider, as the dialogs name them.
    model: Option<(String, String)>,
    /// The gateway host an upload goes to, and whether this session is offline (`PI_OFFLINE`).
    gateway: (String, bool),
    /// The summary, once written.
    pub summary: Option<String>,
}

impl BugFlow {
    /// The first dialog, prefilled with `/bug <description>`.
    pub fn new(
        theme: Rc<Theme>,
        keys: KeybindingsManager,
        editor_keys: KeybindingsManager,
        hint: Option<&str>,
        model: Option<(String, String)>,
        gateway: (String, bool),
    ) -> Self {
        let dialog = ExtensionEditorDialog::new(Rc::clone(&theme), keys.clone(), editor_keys, "Report a bug", hint)
            .with_description(&format!("{DISCLAIMER}\n\nWhat went wrong? (optional)"));
        BugFlow {
            theme,
            keys,
            step: Step::Hint(Box::new(dialog)),
            choices: Choices::default(),
            model,
            gateway,
            summary: None,
        }
    }

    fn model_name(&self) -> String {
        self.model
            .as_ref()
            .map_or_else(|| "the current model".into(), |(name, _)| name.clone())
    }

    fn selector(&self, title: &str, options: &[&str], description: &str) -> SelectorDialog {
        SelectorDialog::new(
            Rc::clone(&self.theme),
            self.keys.clone(),
            title,
            options,
            Some(description),
        )
    }

    fn loader(&self, message: &str) -> BorderedLoader {
        BorderedLoader::new(Rc::clone(&self.theme), self.keys.clone(), message)
    }

    /// `promptForOptions`' last question.
    fn delivery(&self) -> SelectorDialog {
        let choices = &self.choices;
        let summary = if choices.include_summary {
            format!("written by {}", self.model_name())
        } else {
            "none".into()
        };
        let description = format!(
            "Description: {}\nTranscript: {}\nSummary: {summary}\n\nUpload sends the report to {}. Export writes a zip archive to the current directory instead.",
            choices.hint.as_deref().unwrap_or("none"),
            if choices.include_session {
                "included"
            } else {
                "not included"
            },
            self.gateway.0
        );
        self.selector(
            "Bug report",
            &["Upload Report", "Export as Zip", "Cancel"],
            &description,
        )
    }

    /// The options are chosen: offline refusal, the summary, or the report.
    fn chosen(&mut self) -> Action {
        if self.choices.upload && self.gateway.1 {
            return Action::Offline;
        }
        if self.choices.include_summary {
            self.step = Step::Summarizing(self.loader(&format!("Writing summary with {}...", self.model_name())));
            return Action::Summarize;
        }
        self.deliver()
    }

    fn deliver(&mut self) -> Action {
        if self.choices.upload {
            self.step = Step::Uploading(self.loader("Uploading bug report..."));
        }
        Action::Deliver
    }

    /// The summary arrived.
    pub fn summarized(&mut self, summary: String) -> Action {
        self.summary = Some(summary);
        self.deliver()
    }

    /// The upload failed with `failure`: offer the zip export.
    pub fn upload_failed(&mut self, failure: &str) {
        self.step = Step::UploadFailed(self.selector(
            "Upload failed",
            &["Export as Zip", "Cancel"],
            &format!("{failure}\n\nExport the report as a zip archive instead?"),
        ));
    }

    pub fn tick(&mut self) {
        if let Step::Summarizing(loader) | Step::Uploading(loader) = &mut self.step {
            loader.tick();
        }
    }

    pub fn animating(&self) -> bool {
        matches!(self.step, Step::Summarizing(_) | Step::Uploading(_))
    }

    pub fn handle_input(&mut self, data: &str) -> Action {
        match &mut self.step {
            Step::Hint(dialog) => match dialog.handle_input(data) {
                DialogOutcome::Submit(text) => {
                    let hint = session_file::js::trim(&text).to_string();
                    self.choices.hint = (!hint.is_empty()).then_some(hint);
                    self.step = Step::Transcript(self.selector(
                        "Include the session transcript?",
                        &["Yes, include the transcript", "No"],
                        TRANSCRIPT_NOTE,
                    ));
                    Action::None
                }
                DialogOutcome::Cancel => Action::Cancelled,
                DialogOutcome::External(text) => Action::External(text),
                DialogOutcome::None => Action::None,
            },
            Step::Transcript(dialog) => match dialog.handle_input(data) {
                DialogOutcome::Submit(choice) => {
                    self.choices.include_session = choice != "No";
                    if self.choices.include_session {
                        self.step = Step::Delivery(self.delivery());
                    } else {
                        let (name, provider) = self
                            .model
                            .clone()
                            .unwrap_or_else(|| ("the current model".to_string(), "your provider".to_string()));
                        self.step = Step::Summary(self.selector(
                            &format!("Attach a summary written by {name} instead?"),
                            &["Yes, generate a summary", "No"],
                            &format!("The transcript is sent to {provider} with your credentials and tokens. Only the generated summary is attached; the transcript stays on your machine."),
                        ));
                    }
                    Action::None
                }
                DialogOutcome::Cancel => Action::Cancelled,
                _ => Action::None,
            },
            Step::Summary(dialog) => match dialog.handle_input(data) {
                DialogOutcome::Submit(choice) => {
                    self.choices.include_summary = choice != "No";
                    self.step = Step::Delivery(self.delivery());
                    Action::None
                }
                DialogOutcome::Cancel => Action::Cancelled,
                _ => Action::None,
            },
            Step::Delivery(dialog) => match dialog.handle_input(data) {
                DialogOutcome::Submit(choice) if choice == "Cancel" => Action::Cancelled,
                DialogOutcome::Submit(choice) => {
                    self.choices.upload = choice == "Upload Report";
                    self.chosen()
                }
                DialogOutcome::Cancel => Action::Cancelled,
                _ => Action::None,
            },
            Step::Summarizing(loader) => {
                if loader.handle_input(data) {
                    Action::CancelSummary
                } else {
                    Action::None
                }
            }
            Step::Uploading(loader) => {
                if loader.handle_input(data) {
                    Action::CancelUpload
                } else {
                    Action::None
                }
            }
            Step::UploadFailed(dialog) => match dialog.handle_input(data) {
                DialogOutcome::Submit(choice) if choice == "Export as Zip" => Action::ExportAfterFailure,
                DialogOutcome::Submit(_) | DialogOutcome::Cancel => Action::Cancelled,
                _ => Action::None,
            },
        }
    }

    pub fn render(&mut self, width: usize, rows: usize) -> Vec<String> {
        match &mut self.step {
            Step::Hint(dialog) => dialog.render(width, rows),
            Step::Transcript(dialog) | Step::Summary(dialog) | Step::Delivery(dialog) | Step::UploadFailed(dialog) => {
                dialog.render(width)
            }
            Step::Summarizing(loader) | Step::Uploading(loader) => loader.render(width),
        }
    }

    /// The `/bug` hint step is an `ExtensionEditorComponent`: its editor takes the mouse; the other steps are text.
    pub fn handle_mouse(&mut self, event: &tui::mouse::MouseEvent, rows: usize) -> Option<tui::mouse::MouseResult> {
        match &mut self.step {
            Step::Hint(dialog) => dialog.handle_mouse(event, rows),
            _ => None,
        }
    }

    /// The editor dialog, for Ctrl+G's external editor.
    pub fn editor(&mut self) -> Option<&mut ExtensionEditorDialog> {
        match &mut self.step {
            Step::Hint(dialog) => Some(dialog),
            _ => None,
        }
    }
}
