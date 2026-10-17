//! Secrets in completion summaries are masked as cell output is (async-events.ts `runtimeEventText`:
//! `maskCellOutput(summary)`), with the session's masking: `ULTRON_MASK_SECRETS=off` leaves them, and values the
//! session registered as secret (MCP env values, headers, tokens) are masked whatever they look like.

mod p_session_support;

use p_session_support::{HANG, Setup, actor, text, tool, tool_results};
use serde_json::Value;
use std::time::{Duration, Instant};

const SECRET: &str = "ghp_aB3dE5fG7hI9jK1lM3nO5pQ7rS9tU1vW3xY5";

fn event_text(request: &Value) -> Option<String> {
    request["messages"]
        .as_array()?
        .iter()
        .filter(|m| m["role"] == "user")
        .map(|m| m.to_string())
        .find(|m| m.contains("<runtime_event kind=\\\"job_done\\\""))
}

/// Run a root turn that leaves a job printing the secret, and return the event message the job's end re-invokes the
/// root with.
fn job_event(vars: Vec<(&'static str, String)>) -> String {
    // The job ends once the test lets it (a file in the working directory; bounded at about 120 s), after the turn.
    let cell = format!(
        "j = await bash(\"i=0; while [ ! -e go ] && [ $i -lt 2400 ]; do sleep 0.05; i=$((i+1)); done; echo token {SECRET}\", yield_after=0)\nprint('started', j.id)"
    );
    let a = actor(
        Box::new(move |request, _| {
            if event_text(request).is_some() {
                return text("seen");
            }
            if tool_results(request).is_empty() {
                tool("toolu_1", &cell)
            } else {
                text("waiting")
            }
        }),
        Setup {
            vars,
            ..Setup::default()
        },
    );
    a.prompt("go");
    std::fs::write(a.cwd.join("go"), "").unwrap();
    let end = Instant::now() + HANG;
    loop {
        if let Some(text) = a.requests().iter().find_map(event_text) {
            return text;
        }
        assert!(Instant::now() < end, "the job's end never re-invoked the root");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn session_core_event_summaries_are_masked() {
    let event = job_event(Vec::new());
    assert!(!event.contains(SECRET), "{event}");
    assert!(event.contains("[REDACTED:"), "{event}");
}

#[test]
fn session_core_event_summaries_honour_ultron_mask_secrets_off() {
    let event = job_event(vec![("ULTRON_MASK_SECRETS", "off".into())]);
    assert!(event.contains(SECRET), "{event}");
}

#[test]
fn session_core_event_lines_mask_registered_values() {
    let masker = std::sync::Arc::new(host::secrets::OutputMasker::default());
    masker.register_secret_values(&[Value::String("opaque-session-value".into())]);
    let mask: session_core::async_events::MaskText = {
        let masker = masker.clone();
        std::sync::Arc::new(move |text: &str| masker.mask_counted(text, None).0)
    };
    let line = session_core::compose::runtime_event_line_with(
        "job_done",
        "job-1",
        "completed",
        "printed opaque-session-value",
        "await rlm.job(\"job-1\")",
        &mask,
    );
    assert_eq!(
        line,
        "<runtime_event kind=\"job_done\" id=\"job-1\" status=\"completed\" summary=\"printed [REDACTED:known_secret]\" fetch=\"await rlm.job(&quot;job-1&quot;)\" />"
    );
    // The shipped rules alone do not know the value.
    let plain = session_core::compose::runtime_event_line(
        "job_done",
        "job-1",
        "completed",
        "printed opaque-session-value",
        "x",
    );
    assert!(plain.contains("printed opaque-session-value"), "{plain}");
}
