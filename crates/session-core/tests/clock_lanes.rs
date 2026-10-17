//! Clock lines (clock-context.ts): the root and subagent lanes (`ultron.rlm-child.*`) see the local time with its UTC
//! offset before a user message that opens their conversation; the zone is the session's `TZ`.

mod p_session_support;

use p_session_support::{Setup, actor, text, tool, tool_results};
use serde_json::Value;

/// The user texts of a request as sent, clock lines included.
fn raw_user_texts(request: &Value) -> Vec<String> {
    request["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["role"] == "user")
        .flat_map(|m| match &m["content"] {
            Value::String(text) => vec![text.clone()],
            Value::Array(blocks) => blocks
                .iter()
                .filter_map(|b| b["text"].as_str().map(str::to_string))
                .collect(),
            _ => Vec::new(),
        })
        .collect()
}

fn is_child(request: &Value) -> bool {
    raw_user_texts(request).iter().any(|t| t.starts_with("CHILD"))
}

const SPAWN: &str = "import asyncio\nh = await rlm.spawn(\"CHILD: hello\", name=\"c\")\nfor _ in range(12000):\n    try:\n        r = (await rlm.collect([h]))[0]\n        break\n    except RuntimeError:\n        await asyncio.sleep(0.01)\nprint(r[\"result\"][\"status\"])";

#[test]
fn session_core_clock_lines_on_subagent_lanes_in_the_local_zone() {
    let a = actor(
        Box::new(|request, _| {
            if is_child(request) {
                return text("child done");
            }
            if tool_results(request).is_empty() {
                tool("toolu_1", SPAWN)
            } else {
                text("root done")
            }
        }),
        Setup {
            // A POSIX zone rule: India Standard Time, UTC+05:30 all year.
            vars: vec![("TZ", "IST-5:30".into())],
            ..Setup::default()
        },
    );
    a.prompt("ROOT: go");
    let requests = a.requests();
    let clock = |request: &Value| -> Vec<String> {
        raw_user_texts(request)
            .into_iter()
            .filter(|t| t.starts_with("[clock] "))
            .collect()
    };
    let root = requests.iter().find(|r| !is_child(r)).unwrap();
    let child = requests.iter().find(|r| is_child(r)).expect("the child ran");
    for (lane, request) in [("root", root), ("child", child)] {
        let lines = clock(request);
        assert_eq!(lines.len(), 1, "{lane}: {lines:?}");
        let line = &lines[0];
        // "[clock] 2026-10-17 21:03 UTC+05:30"
        assert_eq!(line.len(), "[clock] 2026-10-17 21:03 UTC+05:30".len(), "{lane}: {line}");
        assert!(line.ends_with(" UTC+05:30"), "{lane}: {line}");
    }
    // The child's line comes right before its brief.
    let texts = raw_user_texts(child);
    let at = texts.iter().position(|t| t.starts_with("CHILD")).unwrap();
    assert!(at > 0 && texts[at - 1].starts_with("[clock] "), "{texts:?}");
}

#[test]
fn session_core_clock_format_matches_format_clock_in_its_zone() {
    use kernel_apis::localtime::Zone;
    use session_core::lane::format_clock;
    // `TZ=<zone> formatClock(ms)` of clock-context.ts.
    let cases = [
        (
            "Asia/Kolkata",
            [
                "2026-10-05 19:20 UTC+05:30",
                "1970-01-01 05:30 UTC+05:30",
                "2026-02-25 11:43 UTC+05:30",
            ],
        ),
        (
            "America/St_Johns",
            [
                "2026-10-05 11:20 UTC-02:30",
                "1969-12-31 20:30 UTC-03:30",
                "2026-02-25 02:43 UTC-03:30",
            ],
        ),
        (
            "UTC",
            [
                "2026-10-05 13:50 UTC+00:00",
                "1970-01-01 00:00 UTC+00:00",
                "2026-02-25 06:13 UTC+00:00",
            ],
        ),
    ];
    for (tz, expected) in cases {
        let zone = Zone::from_env(Some(tz));
        let got: Vec<String> = [1_791_208_239_000, 0, 1_772_000_000_000]
            .iter()
            .map(|ms| format_clock(*ms, &zone))
            .collect();
        assert_eq!(got, expected, "{tz}");
    }
}
