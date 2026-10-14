//! `/import <path>` in the real TUI (tmux) over the real server: the client reads a Pi session file, the server's
//! `pi.session-management.importPi` imports it (session_core::migration), the TUI offers to switch and opens the
//! imported session; a second import of the same Pi session is answered "Already imported", nothing written
//! (client-tui-sessions.ts `importPiSessionFile`, server.ts `importPi`). The Pi file is the linear fixture of
//! crates/session-core/fixtures/migration (TypeScript test ultron-migration.test.ts).

mod tui_harness;

use serde_json::Value;
use tui_harness::{Mock, Tui};

fn linear_pi() -> String {
    let fixture: Value =
        serde_json::from_str(include_str!("../../session-core/fixtures/migration/migration.json")).unwrap();
    fixture["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "linear")
        .unwrap()["pi"]
        .as_str()
        .unwrap()
        .to_string()
}

/// The native session files the server stores for `cwd` that hold the imported Pi session (the native id is a digest of
/// the Pi session id, recorded with the fixture).
fn native_session_files(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let fixture: Value =
        serde_json::from_str(include_str!("../../session-core/fixtures/migration/migration.json")).unwrap();
    let id = fixture["cases"][0]["result"]["sessionId"].as_str().unwrap().to_string();
    let cwd = root.join("cwd").display().to_string();
    let dir = root
        .join("agent/experimental/sessions")
        .join(format!("--{}--", cwd.trim_start_matches('/').replace('/', "-")));
    std::fs::read_dir(&dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.to_string_lossy().ends_with(&format!("_{id}.jsonl")))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn cli_interactive_import_imports_a_pi_session_through_the_server_and_switches_to_it() {
    let mock = Mock::start(Vec::new(), None);
    let tui = Tui::start(mock.port, 200, 40, &["--model", "anthropic/claude-sonnet-4-5"], &[]);
    tui.wait("ultron v");
    let pi = tui.root().join("pi.jsonl");
    // The Pi session belongs to the TUI's working directory (the worker that opens it runs there) and to a model
    // this build knows (the worker refuses a session whose lane model it cannot find).
    let cwd = tui.root().join("cwd").display().to_string();
    let pi_text = linear_pi()
        .replace("\"cwd\":\"/project\"", &format!("\"cwd\":\"{cwd}\""))
        .replace("claude-test", "claude-sonnet-4-5");
    std::fs::write(&pi, pi_text).unwrap();

    // A missing file is reported, nothing imported.
    tui.send_text("/import missing.jsonl");
    tui.send_keys(&["Enter"]);
    tui.wait("Error: Failed to import session: ENOENT");

    tui.send_text("/import ../pi.jsonl");
    tui.send_keys(&["Enter"]);
    let screen = tui.wait("Switch to it?");
    assert!(
        screen.contains("Imported 16 entries from <TMP>/cwd/../pi.jsonl as session <ID>. Switch to it?"),
        "{screen}"
    );
    assert_eq!(native_session_files(tui.root()).len(), 1);
    tui.send_keys(&["Enter"]);
    let screen = tui.wait("Session imported from: <TMP>/cwd/../pi.jsonl");
    // The imported transcript is open from its compaction: the kept turns and the last answer show.
    let screen = if screen.contains("Continuing with Y.") {
        screen
    } else {
        tui.wait("Continuing with Y.")
    };
    assert!(screen.contains("Using approach Y."), "{screen}");

    // The same Pi session again: the existing native copy, nothing written, it is the current session.
    tui.send_text("/import ../pi.jsonl");
    tui.send_keys(&["Enter"]);
    tui.wait("Already imported as session <ID> (the current session)");
    assert_eq!(native_session_files(tui.root()).len(), 1);
    tui.send_text("/quit");
    tui.send_keys(&["Enter"]);
    tui.wait("<exited>");
}

/// `/import` without a path in the real TUI (client-tui-sessions.ts `showPiSessionPicker`): Pi's selector lists the
/// sessions of Pi's profile (`~/.pi/agent`) for this working directory only, and the chosen one is imported through
/// the server like `/import <path>`.
#[test]
fn cli_interactive_import_without_a_path_picks_a_pi_session_of_this_project_and_imports_it() {
    let mock = Mock::start(Vec::new(), None);
    let tui = Tui::start(mock.port, 200, 40, &["--model", "anthropic/claude-sonnet-4-5"], &[]);
    tui.wait("ultron v");
    let cwd = tui.root().join("cwd").display().to_string();
    let pi_sessions = tui.root().join(".pi/agent/sessions");
    let here = pi_sessions.join(format!("--{}--", cwd.trim_start_matches('/').replace('/', "-")));
    let elsewhere = pi_sessions.join("--elsewhere--");
    std::fs::create_dir_all(&here).unwrap();
    std::fs::create_dir_all(&elsewhere).unwrap();
    let pi_text = linear_pi()
        .replace("\"cwd\":\"/project\"", &format!("\"cwd\":\"{cwd}\""))
        .replace("claude-test", "claude-sonnet-4-5");
    let pi = here.join("pi.jsonl");
    std::fs::write(&pi, &pi_text).unwrap();
    std::fs::write(
        elsewhere.join("other.jsonl"),
        pi_text
            .replace(&format!("\"cwd\":\"{cwd}\""), "\"cwd\":\"/elsewhere\"")
            .replace("Imported work", "Other project work"),
    )
    .unwrap();

    tui.send_text("/import");
    tui.send_keys(&["Enter"]);
    // The Pi session is listed by its name.
    let screen = tui.wait("Imported work");
    assert!(screen.contains("Resume Session (Current Folder)"), "{screen}");
    assert!(!screen.contains("Other project work"), "{screen}");
    tui.send_keys(&["Enter"]);
    let screen = tui.wait("Switch to it?");
    // The file of Pi's per-project directory (`--<cwd with dashes>--`; the screen masks the temp name).
    assert!(
        screen.contains(
            "Imported 16 entries from <TMP>/.pi/agent/sessions/--<TMPNAME>-cwd--/pi.jsonl as session <ID>. Switch to it?"
        ),
        "{screen}"
    );
    assert_eq!(native_session_files(tui.root()).len(), 1);
    // Pi's file is only read.
    assert_eq!(std::fs::read_to_string(&pi).unwrap(), pi_text);
    tui.send_keys(&["Escape"]);
    tui.wait("Use /resume to open it.");
    tui.send_text("/quit");
    tui.send_keys(&["Enter"]);
    tui.wait("<exited>");
}
