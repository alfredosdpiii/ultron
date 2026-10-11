//! Replays `fixtures/print_mode.json` (recorded from `node packages/coding-agent/src/cli.ts` by
//! `fixtures/print_mode.mjs`): the Rust binary runs the same arguments against the same scripted Anthropic SSE server
//! and must print the same stdout, send the same request bodies (tool results from cells run in the real runtime.py
//! included) and persist the same format-4 journal (`normalize_journal`: everything but Ultron module documents and
//! progress frames, ids renamed, times zeroed). Cases with setup start from the session files a TypeScript run wrote
//! (TypeScript writes, Rust reads: continue, resume, `--session`, `--fork`, v3 upgrade).

mod common;

use common::{Naming, Sandbox, mock_server, normalize_journal, session_files, transcript};
use serde_json::Value;

fn normalize(value: &Value, tmp: &str) -> Value {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let text = serde_json::to_string(value)
        .unwrap()
        .replace(tmp, "<TMP>")
        .replace(&root.display().to_string(), "<root>");
    let clock = regex_free_clock(&text);
    serde_json::from_str(&clock).unwrap()
}

/// Replace `[clock] YYYY-MM-DD HH:MM UTC+00:00[, N unit after the previous message]` and `"serverId"/"sessionId"`
/// values in JSON-escaped stdout.
fn regex_free_clock(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(i) = rest.find("[clock] ") {
        out.push_str(&rest[..i]);
        out.push_str("[clock] <CLOCK>");
        rest = &rest[i + "[clock] ".len() + "2026-10-10 18:53 UTC+00:00".len()..];
        if let Some(gap) = rest.strip_prefix(", ")
            && let Some(end) = gap.find(" after the previous message")
        {
            rest = &gap[end + " after the previous message".len()..];
        }
    }
    out.push_str(rest);
    for key in ["serverId", "sessionId"] {
        let marker = format!("\\\"{key}\\\":\\\"");
        if let Some(i) = out.find(&marker) {
            let start = i + marker.len();
            let end = start + out[start..].find("\\\"").unwrap();
            out.replace_range(start..end, "<ID>");
        }
    }
    out
}

#[test]
fn cli_print_mode_matches_typescript() {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/print_mode.json")).unwrap();
    for case in fixture.as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let replies = case["replies"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r.as_str().unwrap().to_string())
            .collect();
        let (port, seen) = mock_server(replies, Naming::Title("Scripted title".into()));
        let sandbox = Sandbox::new(port);
        let tmp_text = sandbox.root.display().to_string();
        let sessions_dir = sandbox.sessions_dir();
        // TypeScript-written session files (made deterministic), with their modification order.
        let mut setup = Vec::new();
        for (rank, file) in case["setupFiles"].as_array().unwrap().iter().enumerate() {
            let path = sessions_dir.join(file["path"].as_str().unwrap());
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, file["content"].as_str().unwrap().replace("<TMP>", &tmp_text)).unwrap();
            let mtime = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000 + rank as u64);
            std::fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(mtime)
                .unwrap();
            setup.push((file["id"].as_str().unwrap().to_string(), path));
        }
        let args: Vec<String> = case["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| {
                let mut a = a.as_str().unwrap().to_string();
                for (n, (id, path)) in setup.iter().enumerate() {
                    a = a
                        .replace(&format!("<SESSION:{n}>"), id)
                        .replace(&format!("<SESSION_PATH:{n}>"), &path.display().to_string());
                }
                a
            })
            .collect();
        let mut command = sandbox.ultron();
        for (key, value) in case["env"].as_object().unwrap() {
            command.env(key, value.as_str().unwrap());
        }
        let output = command.args(&args).output().unwrap();
        let output_stdout = output.stdout.clone();
        let stdout = normalize(&Value::String(String::from_utf8(output.stdout).unwrap()), &tmp_text);
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert_eq!(stderr, case["stderr"].as_str().unwrap(), "{name} stderr");
        assert_eq!(stdout, case["stdout"], "{name} stdout");
        assert_eq!(
            output.status.code(),
            case["code"].as_i64().map(|c| c as i32),
            "{name} exit code"
        );
        let got = normalize(&Value::Array(seen.lock().unwrap().requests.clone()), &tmp_text);
        let want = &case["requests"];
        for (i, (g, w)) in got.as_array().unwrap().iter().zip(want.as_array().unwrap()).enumerate() {
            for key in ["method", "url"] {
                assert_eq!(g[key], w[key], "{name} request {i} {key}");
            }
            for (field, value) in w["body"].as_object().unwrap() {
                assert_eq!(&g["body"][field], value, "{name} request {i} body.{field}");
            }
            assert_eq!(g["body"], w["body"], "{name} request {i} body");
        }
        assert_eq!(
            got.as_array().unwrap().len(),
            want.as_array().unwrap().len(),
            "{name} request count"
        );
        let files = session_files(&sessions_dir);
        let want_sessions = case["sessions"].as_array().unwrap();
        assert_eq!(files.len(), want_sessions.len(), "{name} session files");
        let mut wanted: Vec<&Value> = want_sessions.iter().collect();
        for file in &files {
            let checkout = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .canonicalize()
                .unwrap();
            let text = std::fs::read_to_string(file)
                .unwrap()
                .replace(&tmp_text, "<TMP>")
                .replace(&checkout.display().to_string(), "<root>");
            let journal = normalize_journal(&text);
            // Files are matched by content: names carry times and ids.
            let position = wanted.iter().position(|w| w["journal"] == journal).unwrap_or_else(|| {
                let closest = wanted.iter().min_by_key(|w| {
                    (w["journal"]["transactions"].as_array().map_or(0, Vec::len) as i64
                        - journal["transactions"].as_array().map_or(0, Vec::len) as i64)
                        .abs()
                });
                if let Some(w) = closest {
                    let (g, w) = (&journal["transactions"], &w["journal"]["transactions"]);
                    for i in 0..g.as_array().unwrap().len().max(w.as_array().unwrap().len()) {
                        assert_eq!(g[i], w[i], "{name} {} journal transaction {i}", file.display());
                    }
                }
                panic!("{name}: no TypeScript journal equals {}:\n{journal:#}", file.display())
            });
            let want = wanted.remove(position);
            let relative = file.strip_prefix(&sessions_dir).unwrap();
            let tmp_name = tmp_text.trim_start_matches('/').replace(['/', '\\', ':'], "-");
            assert_eq!(
                Value::from(
                    relative
                        .parent()
                        .unwrap()
                        .display()
                        .to_string()
                        .replace(&tmp_name, "<TMPNAME>")
                ),
                want["dir"],
                "{name} session directory"
            );
            let (cwd, messages) = transcript(&text);
            assert_eq!(Value::from(cwd), want["cwd"], "{name} session cwd");
            assert_eq!(Value::from(messages), want["messages"], "{name} transcript");
            if name.starts_with("json")
                && file.metadata().unwrap().modified().unwrap()
                    > std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_100)
            {
                let id = text
                    .lines()
                    .next()
                    .map(|l| serde_json::from_str::<Value>(l).unwrap()["id"].clone());
                let printed: Value = serde_json::from_str(&String::from_utf8_lossy(&output_stdout)).unwrap();
                assert_eq!(Some(printed["sessionId"].clone()), id, "{name} prints its session id");
            }
        }
    }
}

#[test]
fn cli_format_clock_utc_matches_format_clock() {
    // formatClock(1791208239000) with TZ=UTC; and the epoch, and a leap day.
    assert_eq!(
        cli::print_mode::format_clock_utc(1_791_208_239_000),
        "2026-10-05 13:50 UTC+00:00"
    );
    assert_eq!(cli::print_mode::format_clock_utc(0), "1970-01-01 00:00 UTC+00:00");
    assert_eq!(
        cli::print_mode::format_clock_utc(951_782_400_000),
        "2000-02-29 00:00 UTC+00:00"
    );
}

#[test]
fn cli_session_directory_name_matches_jsonl_repo() {
    // sessionDirectoryName (packages/agent/src/harness/session/jsonl/repo.ts), values from node.
    for (cwd, want) in [
        ("/home/u/p", "--home-u-p--"),
        ("/", "----"),
        ("C:\\Users\\x", "--C--Users-x--"),
        ("/a:b/c", "--a-b-c--"),
        ("rel/x", "--rel-x--"),
        ("//double", "---double--"),
    ] {
        assert_eq!(cli::print_mode::session_directory_name(cwd), want, "{cwd}");
    }
}

#[test]
fn cli_clock_lines_match_with_clock_lines() {
    // formatGap and withClockLines (packages/coding-agent/src/ultron/clock-context.ts), values from node.
    for (ms, want) in [
        (600_000, "10 min"),
        (7_199_999, "119 min"),
        (7_200_000, "2 h"),
        (172_799_999, "47 h"),
        (172_800_000, "2 d"),
    ] {
        assert_eq!(cli::print_mode::format_gap(ms), want, "{ms}");
    }
    let user = |t: i64| serde_json::json!({"role": "user", "content": "x", "timestamp": t});
    let assistant = |t: i64| serde_json::json!({"role": "assistant", "content": [], "timestamp": t});
    // Only user messages get a line; a tool result's time still counts as the previous message (no lines here).
    let tool = serde_json::json!({"role": "toolResult", "content": [], "timestamp": 20_000_000});
    assert!(cli::print_mode::clock_lines(&[user(0), tool, user(20_000_001)]).is_empty());
    let messages = [
        user(0),
        user(1_000),
        assistant(500_000),
        user(1_099_999),
        user(1_700_000),
        user(9_000_000),
    ];
    let lines: Vec<(usize, String)> = cli::print_mode::clock_lines(&messages)
        .into_iter()
        .map(|(i, m)| match m {
            ai::Message::User(u) => match u.content {
                ai::UserMessageContent::Blocks(b) => match &b[0] {
                    ai::UserContent::Text(t) => (i, t.text.clone()),
                    _ => unreachable!(),
                },
                _ => unreachable!(),
            },
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(
        lines,
        [
            (1, "[clock] 1970-01-01 00:00 UTC+00:00".to_string()),
            (
                4,
                "[clock] 1970-01-01 00:28 UTC+00:00, 10 min after the previous message".to_string()
            ),
            (
                5,
                "[clock] 1970-01-01 02:30 UTC+00:00, 2 h after the previous message".to_string()
            ),
        ]
    );
}
