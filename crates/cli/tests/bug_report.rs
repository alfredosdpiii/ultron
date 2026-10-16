//! `/bug`'s upload, replaying `fixtures/bug_upload.json` (recorded from `uploadBugReport` of
//! core/bug-report-upload.ts by `fixtures/bug_upload.mjs`): against a 127.0.0.1 gateway giving the recorded answers,
//! the Rust upload sends the same request (POST `/v1/bug-reports` whatever the gateway's path, the bearer token only
//! with one, one multipart file part per bundle file with its name and content type) and makes the same of the
//! answer (the report id, or the same error). An upload cancelled while it waits reports nothing.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;
use telemetry::bug_report::BugReportBundle;

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/bug_upload.json")).unwrap()
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        403 => "Forbidden",
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        _ => "",
    }
}

/// The parts of a `multipart/form-data` body: `{name, filename, contentType, data}`.
fn parts(content_type: &str, body: &[u8]) -> Vec<Value> {
    let boundary = content_type.split("boundary=").nth(1).unwrap().trim_matches('"');
    let text = String::from_utf8(body.to_vec()).unwrap();
    let delimiter = format!("--{boundary}");
    let mut out = Vec::new();
    for chunk in text.split(&delimiter).skip(1) {
        if chunk.starts_with("--") {
            break;
        }
        let chunk = chunk.strip_prefix("\r\n").unwrap();
        let (head, data) = chunk.split_once("\r\n\r\n").unwrap();
        let data = data.strip_suffix("\r\n").unwrap();
        let header = |name: &str| {
            head.lines()
                .find_map(|l| l.strip_prefix(name))
                .map(str::trim)
                .unwrap()
                .to_string()
        };
        let disposition = header("Content-Disposition:");
        let field = |key: &str| {
            disposition
                .split(';')
                .find_map(|p| p.trim().strip_prefix(&format!("{key}=")))
                .unwrap()
                .trim_matches('"')
                .to_string()
        };
        out.push(json!({"name": field("name"), "filename": field("filename"),
            "contentType": header("Content-Type:"), "data": data}));
    }
    out
}

/// One gateway request: answer with `reply`; with `hold`, first say the request arrived and wait until the test drops
/// its release sender.
type Hold = (mpsc::Sender<()>, mpsc::Receiver<()>);

fn gateway(reply: Value, hold: Option<Hold>) -> (String, Arc<Mutex<Vec<Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let requests = seen.clone();
    std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else { return };
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let mut words = line.split_whitespace();
        let (method, url) = (words.next().unwrap().to_string(), words.next().unwrap().to_string());
        let (mut length, mut content_type, mut authorization) = (0, String::new(), Value::Null);
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).unwrap();
            if header.trim().is_empty() {
                break;
            }
            let (key, value) = header.split_once(':').unwrap();
            match key.to_ascii_lowercase().as_str() {
                "content-length" => length = value.trim().parse().unwrap(),
                "content-type" => content_type = value.trim().to_string(),
                "authorization" => authorization = json!(value.trim()),
                _ => {}
            }
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        requests
            .lock()
            .unwrap()
            .push(json!({"method": method, "url": url, "authorization": authorization,
            "parts": parts(&content_type, &body)}));
        if let Some((arrived, release)) = hold {
            let _ = arrived.send(());
            let _ = release.recv();
        }
        let status = reply["status"].as_u64().unwrap() as u16;
        let body = reply["body"].as_str().unwrap();
        let head = format!(
            "HTTP/1.1 {status} {}\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            reason(status),
            reply["contentType"].as_str().unwrap(),
            body.len()
        );
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(body.as_bytes());
    });
    // A path on the gateway is ignored (`new URL("/v1/bug-reports", gateway)`).
    (format!("http://127.0.0.1:{port}/ignored/path"), seen)
}

fn bundle(value: &Value) -> BugReportBundle {
    BugReportBundle {
        metadata: value["metadata"].clone(),
        diagnostics: value["diagnostics"].clone(),
        session_jsonl: value["sessionJsonl"].as_str().map(str::to_string),
        summary: value["summary"].as_str().map(str::to_string),
    }
}

#[test]
fn cli_bug_report_upload_requests_and_answers_match_typescript() {
    let cases = fixture();
    let cases = cases.as_array().unwrap();
    assert_eq!(cases.len(), 8);
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let (url, seen) = gateway(case["reply"].clone(), None);
        let outcome = cli::bug_report::upload(
            &bundle(&case["bundle"]),
            &url,
            case["token"].as_str(),
            &AtomicBool::new(false),
        );
        let outcome = match outcome {
            Ok(Some(id)) => json!({"id": id}),
            Ok(None) => panic!("{name}: not cancelled"),
            Err(error) => json!({"error": error}),
        };
        assert_eq!(outcome, case["outcome"], "{name} outcome");
        assert_eq!(
            Value::Array(seen.lock().unwrap().clone()),
            case["requests"],
            "{name} request"
        );
    }
}

#[test]
fn cli_bug_report_cancelled_upload_reports_nothing() {
    let (release, hold) = mpsc::channel();
    let (arrived_tx, arrived) = mpsc::channel();
    let (url, _) = gateway(
        json!({"status": 200, "contentType": "application/json", "body": "{\"ok\":true,\"bug_report\":{\"id\":\"late\"}}"}),
        Some((arrived_tx, hold)),
    );
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = cancel.clone();
    let bundle = bundle(&fixture()[1]["bundle"]);
    let upload = std::thread::spawn(move || cli::bug_report::upload(&bundle, &url, None, &flag));
    // Cancel once the gateway holds the request.
    arrived
        .recv_timeout(Duration::from_secs(120))
        .expect("the upload arrives");
    cancel.store(true, Ordering::SeqCst);
    assert_eq!(upload.join().unwrap(), Ok(None));
    drop(release);
}
