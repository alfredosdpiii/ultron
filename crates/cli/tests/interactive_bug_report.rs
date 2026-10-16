//! `/bug` and the easter eggs in a real terminal (tmux), as interactive-mode.ts runs them (`handleBugCommand` and
//! modes/interactive/bug-report.ts `reportBug`, `handleArminSaysHi`, `handleDementedDelves`), with the texts of
//! bug-report.ts:
//!
//! - consent, the transcript question, the delivery choice, then a zip in the working directory with `report.json`,
//!   `diagnostics.json` and the branch as `session.jsonl`, recorded in the session as a `pi.bug-report` entry;
//! - the transcript declined, a summary written by the session's model instead, uploaded to the gateway named by
//!   `PI_RADIUS_GATEWAY` (a 127.0.0.1 fake): the summary is attached and no transcript is;
//! - a failed upload offers the zip export; offline mode refuses the upload; Esc cancels;
//! - `/dementedelves` and `/arminsayshi` draw Pi's components.
//!
//! No real endpoint is contacted: the model and the gateway are 127.0.0.1 fakes, and nothing opens a browser.

mod tui_harness;

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tui_harness::{Mock, TempDir, Tui, text_reply};

const MODEL: [&str; 2] = ["--model", "anthropic/claude-sonnet-4-5"];

fn python() -> String {
    std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into())
}

/// A gateway answering every `POST` with `status` and `body`; records each request's path, Authorization and body.
fn gateway(status: u16, body: &'static str) -> (u16, Arc<Mutex<Vec<Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let requests = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let path = line.split_whitespace().nth(1).unwrap_or_default().to_string();
            let (mut length, mut authorization) = (0, Value::Null);
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                if header.trim().is_empty() {
                    break;
                }
                let (key, value) = header.split_once(':').unwrap();
                match key.to_ascii_lowercase().as_str() {
                    "content-length" => length = value.trim().parse().unwrap(),
                    "authorization" => authorization = json!(value.trim()),
                    _ => {}
                }
            }
            let mut data = vec![0; length];
            reader.read_exact(&mut data).unwrap();
            requests
                .lock()
                .unwrap()
                .push(json!({"path": path, "authorization": authorization, "body": String::from_utf8_lossy(&data)}));
            let reason = if status == 200 { "OK" } else { "Payload Too Large" };
            let _ = write!(
                stream,
                "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    (port, seen)
}

/// The archive's files, read by the tests' Python.
fn unzip(path: &Path) -> serde_json::Map<String, Value> {
    let out = std::process::Command::new(python())
        .args([
            "-I",
            "-c",
            "import json, sys, zipfile\nz = zipfile.ZipFile(sys.argv[1])\nprint(json.dumps({n: z.read(n).decode() for n in z.namelist()}))",
        ])
        .arg(path)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice::<Value>(&out.stdout)
        .unwrap()
        .as_object()
        .unwrap()
        .clone()
}

fn archives(cwd: &Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(cwd)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with("pi-bug-report-"))
        .collect()
}

/// The session journals' text (format-4 JSONL under the agent dir).
fn journals(root: &Path) -> String {
    let mut text = String::new();
    let mut stack = vec![root.join("agent")];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "jsonl") {
                text.push_str(&std::fs::read_to_string(&path).unwrap_or_default());
            }
        }
    }
    text
}

#[test]
fn cli_interactive_bug_report_exports_a_zip_with_the_transcript() {
    let mock = Mock::start(vec![text_reply("Hello back.")], None);
    let (gateway_port, _) = gateway(200, "{}");
    let tmp = TempDir::new();
    let gateway_url = format!("http://127.0.0.1:{gateway_port}");
    let env = [
        ("ULTRON_AUTO_NAME", "off"),
        ("ULTRON_SPLASH", "off"),
        ("PI_RADIUS_GATEWAY", gateway_url.as_str()),
    ];
    let tui = Tui::start_at(&tmp.0, mock.port, 120, 50, &MODEL, &env);
    tui.wait("Session:");
    tui.send_text("hi there");
    tui.send_keys(&["Enter"]);
    tui.wait("Hello back.");

    tui.send_text("/bug the cell crashed");
    tui.send_keys(&["Enter"]);
    let screen = tui.wait("What went wrong? (optional)");
    assert!(screen.contains("Report a bug"), "{screen}");
    assert!(
        screen.contains("This report goes to the Pi developers (Earendil)"),
        "{screen}"
    );
    assert!(
        screen.contains("the cell crashed"),
        "the description is prefilled: {screen}"
    );
    tui.send_keys(&["Enter"]);
    let screen = tui.wait("Include the session transcript?");
    assert!(screen.contains("→ Yes, include the transcript"), "{screen}");
    assert!(screen.contains("The transcript contains your messages"), "{screen}");
    tui.send_keys(&["Enter"]);
    let screen = tui.wait("Upload sends the report to");
    assert!(screen.contains("Description: the cell crashed"), "{screen}");
    assert!(screen.contains("Transcript: included"), "{screen}");
    assert!(screen.contains("Summary: none"), "{screen}");
    assert!(
        screen.contains(&format!("Upload sends the report to 127.0.0.1:{gateway_port}.")),
        "{screen}"
    );
    tui.send_keys(&["Down"]);
    tui.wait("→ Export as Zip");
    tui.send_keys(&["Enter"]);
    let screen = tui.wait("Report ID:");
    assert!(screen.contains("Bug report exported to:"), "{screen}");

    let cwd = tmp.0.join("cwd");
    let zips = archives(&cwd);
    assert_eq!(zips.len(), 1, "{zips:?}");
    let files = unzip(&zips[0]);
    let names: Vec<&String> = files.keys().collect();
    assert_eq!(names, ["report.json", "diagnostics.json", "session.jsonl"]);
    let report: Value = serde_json::from_str(files["report.json"].as_str().unwrap()).unwrap();
    let id = report["id"].as_str().unwrap();
    assert_eq!(
        zips[0].file_name().unwrap().to_string_lossy(),
        format!("pi-bug-report-{id}.zip")
    );
    assert_eq!(report["hint"], "the cell crashed");
    assert_eq!(report["session"]["included"], true);
    assert_eq!(report["session"]["summaryIncluded"], false);
    assert_eq!(report["session"]["cwd"], cwd.display().to_string());
    assert_eq!(report["model"]["id"], "claude-sonnet-4-5");
    let session = files["session.jsonl"].as_str().unwrap();
    let lines: Vec<Value> = session.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(lines[0]["type"], "session");
    assert!(
        session.contains("hi there") && session.contains("Hello back."),
        "{session}"
    );
    let share = lines.last().unwrap();
    assert_eq!(
        (&share["type"], &share["customType"]),
        (&json!("custom"), &json!("pi.share"))
    );
    assert!(
        share["data"]["systemPrompt"].as_str().unwrap().contains("rlm"),
        "{share}"
    );
    assert_eq!(share["data"]["tools"][0]["name"], "rlm");
    // Recorded in the session (`recordInSession`).
    let journal = journals(&tmp.0);
    assert!(journal.contains("pi.bug-report") && journal.contains(id), "{journal}");
    drop(tui);
}

#[test]
fn cli_interactive_bug_report_uploads_a_model_summary_instead_of_the_transcript() {
    let summary = "## What went wrong\nThe cell crashed.";
    let mock = Mock::start(vec![text_reply("Hello back."), text_reply(summary)], None);
    let (gateway_port, seen) = gateway(200, r#"{"ok":true,"bug_report":{"id":"br_42"}}"#);
    let tmp = TempDir::new();
    let gateway_url = format!("http://127.0.0.1:{gateway_port}/some/path");
    let env = [
        ("ULTRON_AUTO_NAME", "off"),
        ("ULTRON_SPLASH", "off"),
        ("PI_RADIUS_GATEWAY", gateway_url.as_str()),
    ];
    let tui = Tui::start_at(&tmp.0, mock.port, 120, 50, &MODEL, &env);
    tui.wait("Session:");
    tui.send_text("hi there");
    tui.send_keys(&["Enter"]);
    tui.wait("Hello back.");
    tui.send_text("/bug");
    tui.send_keys(&["Enter"]);
    tui.wait("What went wrong? (optional)");
    tui.send_keys(&["Enter"]);
    tui.wait("Include the session transcript?");
    tui.send_keys(&["Down"]);
    tui.wait("→ No");
    tui.send_keys(&["Enter"]);
    let screen = tui.wait("instead?");
    assert!(
        screen.contains("Attach a summary written by Claude Sonnet 4.5 (latest) instead?"),
        "{screen}"
    );
    assert!(
        screen.contains("The transcript is sent to anthropic with your credentials and tokens."),
        "{screen}"
    );
    tui.send_keys(&["Enter"]);
    let screen = tui.wait("Upload sends the report to");
    assert!(screen.contains("Description: none"), "{screen}");
    assert!(screen.contains("Transcript: not included"), "{screen}");
    assert!(
        screen.contains("Summary: written by Claude Sonnet 4.5 (latest)"),
        "{screen}"
    );
    tui.send_keys(&["Enter"]);
    tui.wait("Bug report uploaded. Report ID: br_42");

    let requests = seen.lock().unwrap().clone();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["path"], "/v1/bug-reports");
    assert_eq!(
        requests[0]["authorization"],
        Value::Null,
        "anonymous without a Radius login"
    );
    let body = requests[0]["body"].as_str().unwrap();
    for name in ["report.json", "diagnostics.json", "summary.md"] {
        assert!(
            body.contains(&format!("name=\"{name}\"; filename=\"{name}\"")),
            "{body}"
        );
    }
    assert!(!body.contains("session.jsonl"), "{body}");
    assert!(body.contains(&format!("{summary}\n")), "{body}");
    assert!(body.contains("\"summaryIncluded\": true"), "{body}");
    assert!(archives(&tmp.0.join("cwd")).is_empty());
    let journal = journals(&tmp.0);
    assert!(
        journal.contains("pi.bug-report") && journal.contains("\"delivery\":\"upload\""),
        "{journal}"
    );
    drop(tui);
}

#[test]
fn cli_interactive_bug_report_failed_upload_offers_the_zip_and_cancel_and_offline_stop_it() {
    let mock = Mock::start(Vec::new(), None);
    let (gateway_port, seen) = gateway(
        413,
        r#"{"ok":false,"error":"too_large","description":"Report too large"}"#,
    );
    let tmp = TempDir::new();
    let gateway_url = format!("http://127.0.0.1:{gateway_port}");
    let env = [
        ("ULTRON_AUTO_NAME", "off"),
        ("ULTRON_SPLASH", "off"),
        ("PI_RADIUS_GATEWAY", gateway_url.as_str()),
    ];
    let tui = Tui::start_at(&tmp.0, mock.port, 120, 50, &MODEL, &env);
    tui.wait("Session:");
    // Esc on the first dialog.
    tui.send_text("/bug");
    tui.send_keys(&["Enter"]);
    tui.wait("What went wrong? (optional)");
    tui.send_keys(&["Escape"]);
    tui.wait("Bug report cancelled");
    // The upload fails: the zip is offered instead.
    tui.send_text("/bug too big");
    tui.send_keys(&["Enter"]);
    tui.wait("What went wrong? (optional)");
    tui.send_keys(&["Enter"]);
    tui.wait("Include the session transcript?");
    tui.send_keys(&["Enter"]);
    tui.wait("Upload sends the report to");
    tui.send_keys(&["Enter"]);
    let screen = tui.wait("Export the report as a zip archive instead?");
    assert!(screen.contains("Upload failed"), "{screen}");
    assert!(
        screen.contains("Bug report upload failed: Report too large"),
        "{screen}"
    );
    assert_eq!(seen.lock().unwrap().len(), 1);
    tui.send_keys(&["Enter"]);
    let screen = tui.wait("Report ID:");
    assert!(screen.contains("Bug report exported to:"), "{screen}");
    assert_eq!(archives(&tmp.0.join("cwd")).len(), 1);
    drop(tui);

    // Offline mode refuses the upload before anything is built.
    let tmp = TempDir::new();
    let env = [
        ("ULTRON_AUTO_NAME", "off"),
        ("ULTRON_SPLASH", "off"),
        ("PI_RADIUS_GATEWAY", gateway_url.as_str()),
        ("PI_OFFLINE", "1"),
    ];
    let tui = Tui::start_at(&tmp.0, mock.port, 120, 50, &MODEL, &env);
    tui.wait("Session:");
    tui.send_text("/bug");
    tui.send_keys(&["Enter"]);
    tui.wait("What went wrong? (optional)");
    tui.send_keys(&["Enter"]);
    tui.wait("Include the session transcript?");
    tui.send_keys(&["Enter"]);
    tui.wait("Upload sends the report to");
    tui.send_keys(&["Enter"]);
    tui.wait("Error: Uploading bug reports requires online mode. Use Export as Zip instead.");
    assert_eq!(seen.lock().unwrap().len(), 1, "nothing more was uploaded");
    assert!(archives(&tmp.0.join("cwd")).is_empty());
    drop(tui);
}

#[test]
fn cli_interactive_easter_eggs_draw_pi_components() {
    let mock = Mock::start(Vec::new(), None);
    let tmp = TempDir::new();
    let tui = Tui::start_at(
        &tmp.0,
        mock.port,
        100,
        50,
        &MODEL,
        &[("ULTRON_AUTO_NAME", "off"), ("ULTRON_SPLASH", "off")],
    );
    tui.wait("Session:");
    tui.send_text("/dementedelves");
    tui.send_keys(&["Enter"]);
    let screen = tui.wait("pi has joined Earendil");
    assert!(screen.contains("Read the blog post:"), "{screen}");
    assert!(
        screen.contains("https://mariozechner.at/posts/2026-04-08-ive-sold-out/"),
        "{screen}"
    );
    assert!(
        screen.contains("[Image: clankolas.png [image/png] 640x537]"),
        "{screen}"
    );
    tui.send_text("/arminsayshi");
    tui.send_keys(&["Enter"]);
    tui.wait("ARMIN SAYS HI");
    // Whatever the effect, the portrait ends whole (its last rows).
    tui.wait("▀▀▀");
    tui.wait("██  ▄▄▄");
    drop(tui);
}
