//! `/bug`'s dialogs (modes/interactive/bug-report.ts `promptForOptions` and `reportBug`), ported with their texts:
//! each answer leads to the next question, Esc anywhere cancels, an upload in offline mode is refused, a summary is
//! written before the report, and a failed upload offers the zip.

use interactive::bug_report::{Action, BugFlow, DISCLAIMER, Step, TRANSCRIPT_NOTE};
use std::rc::Rc;
use tui::keybindings::{KeybindingsManager, Platform};
use tui::qol_keys::native_app_keybindings;
use tui::theme::{ColorMode, builtin_theme_json, create_theme};

const ENTER: &str = "\r";
const DOWN: &str = "\x1b[B";
const ESC: &str = "\x1b";

fn keys() -> KeybindingsManager {
    KeybindingsManager::new(
        native_app_keybindings(Platform::new("linux", false)),
        Vec::new(),
        Default::default(),
    )
}

fn flow(hint: Option<&str>, offline: bool) -> BugFlow {
    let theme = Rc::new(create_theme(&builtin_theme_json("dark").unwrap(), ColorMode::Truecolor, None).unwrap());
    BugFlow::new(
        theme,
        keys(),
        keys(),
        hint,
        Some(("Claude Sonnet 4.5".into(), "anthropic".into())),
        ("radius.pi.dev".into(), offline),
    )
}

/// The dialog's text without styles, its lines trimmed and joined by spaces (wrapping is the renderer's).
fn shown(flow: &mut BugFlow) -> String {
    let lines = flow.render(1000, 40);
    let text: Vec<String> = lines
        .iter()
        .map(|l| {
            let mut out = String::new();
            let mut chars = l.chars();
            while let Some(c) = chars.next() {
                if c == '\x1b' {
                    for c in chars.by_ref() {
                        if c.is_ascii_alphabetic() {
                            break;
                        }
                    }
                } else {
                    out.push(c);
                }
            }
            out.trim().to_string()
        })
        .filter(|l| !l.is_empty() && !l.starts_with('─'))
        .collect();
    text.join(" | ")
}

#[test]
fn interactive_bug_report_transcript_and_zip_questions_match_typescript() {
    let mut flow = flow(Some("  the cell crashed  "), false);
    let text = shown(&mut flow);
    assert!(
        text.starts_with(&format!("Report a bug | {DISCLAIMER} | What went wrong? (optional) |")),
        "{text}"
    );
    assert!(text.contains("the cell crashed"), "prefilled: {text}");
    assert_eq!(flow.handle_input(ENTER), Action::None);
    assert_eq!(
        shown(&mut flow),
        format!(
            "Include the session transcript? | {TRANSCRIPT_NOTE} | → Yes, include the transcript | No | ↑↓ navigate  enter select  escape/ctrl+c cancel"
        )
    );
    assert_eq!(flow.handle_input(ENTER), Action::None);
    assert_eq!(
        shown(&mut flow),
        "Bug report | Description: the cell crashed | Transcript: included | Summary: none | Upload sends the report to radius.pi.dev. Export writes a zip archive to the current directory instead. | → Upload Report | Export as Zip | Cancel | ↑↓ navigate  enter select  escape/ctrl+c cancel"
    );
    assert_eq!(flow.handle_input(DOWN), Action::None);
    assert_eq!(flow.handle_input(ENTER), Action::Deliver);
    assert_eq!(flow.choices.hint.as_deref(), Some("the cell crashed"));
    assert!(flow.choices.include_session && !flow.choices.include_summary && !flow.choices.upload);
}

#[test]
fn interactive_bug_report_summary_then_upload_then_fallback() {
    let mut flow = flow(None, false);
    flow.handle_input(ENTER);
    flow.handle_input(DOWN);
    assert_eq!(flow.handle_input(ENTER), Action::None);
    assert_eq!(
        shown(&mut flow),
        "Attach a summary written by Claude Sonnet 4.5 instead? | The transcript is sent to anthropic with your credentials and tokens. Only the generated summary is attached; the transcript stays on your machine. | → Yes, generate a summary | No | ↑↓ navigate  enter select  escape/ctrl+c cancel"
    );
    flow.handle_input(ENTER);
    let text = shown(&mut flow);
    assert!(
        text.starts_with(
            "Bug report | Description: none | Transcript: not included | Summary: written by Claude Sonnet 4.5 |"
        ),
        "{text}"
    );
    assert_eq!(flow.handle_input(ENTER), Action::Summarize);
    assert!(matches!(flow.step, Step::Summarizing(_)));
    assert!(shown(&mut flow).contains("Writing summary with Claude Sonnet 4.5..."));
    assert_eq!(flow.summarized("## Report".into()), Action::Deliver);
    assert!(matches!(flow.step, Step::Uploading(_)));
    assert!(shown(&mut flow).contains("Uploading bug report..."));
    assert_eq!(flow.handle_input(ESC), Action::CancelUpload);
    flow.upload_failed("Bug report upload failed: Report too large");
    assert_eq!(
        shown(&mut flow),
        "Upload failed | Bug report upload failed: Report too large | Export the report as a zip archive instead? | → Export as Zip | Cancel | ↑↓ navigate  enter select  escape/ctrl+c cancel"
    );
    assert_eq!(flow.handle_input(ENTER), Action::ExportAfterFailure);
    flow.upload_failed("x");
    flow.handle_input(DOWN);
    assert_eq!(flow.handle_input(ENTER), Action::Cancelled);
}

#[test]
fn interactive_bug_report_cancel_and_offline() {
    // Esc on each question cancels.
    for presses in 0..3 {
        let mut flow = flow(None, false);
        for _ in 0..presses {
            if presses == 2 {
                flow.handle_input(DOWN);
            }
            flow.handle_input(ENTER);
        }
        assert_eq!(flow.handle_input(ESC), Action::Cancelled, "after {presses}");
    }
    let mut cancel = flow(None, false);
    cancel.handle_input(ENTER);
    cancel.handle_input(ENTER);
    cancel.handle_input(DOWN);
    cancel.handle_input(DOWN);
    assert_eq!(cancel.handle_input(ENTER), Action::Cancelled, "the Cancel option");
    // Offline: the upload is refused; the zip is not.
    let mut offline = flow(None, true);
    offline.handle_input(ENTER);
    offline.handle_input(ENTER);
    assert_eq!(offline.handle_input(ENTER), Action::Offline);
    let mut zip = flow(None, true);
    zip.handle_input(ENTER);
    zip.handle_input(ENTER);
    zip.handle_input(DOWN);
    assert_eq!(zip.handle_input(ENTER), Action::Deliver);
    // The summary loader's Esc stops the summary.
    let mut summary = flow(None, false);
    summary.handle_input(ENTER);
    summary.handle_input(DOWN);
    summary.handle_input(ENTER);
    summary.handle_input(ENTER);
    summary.handle_input(DOWN);
    assert_eq!(summary.handle_input(ENTER), Action::Summarize);
    assert_eq!(summary.handle_input(ESC), Action::CancelSummary);
}
