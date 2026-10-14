//! A56: extension and MCP tool calls from the REPL do not block the turn. Ported from
//! packages/coding-agent/test/ultron-extension-tools.test.ts (the three cases the manifest names) with their literal
//! expectations: the real runtime.py under the Rust kernel, `tools.*` served by `kernel_apis::ToolCalls` over the
//! session's runner (`session_core::extension_tools::ExtensionTools`), which calls a real extension process
//! (`common/fake_tools_extension.py`, the port of fake-mcp-extension.ts: `mcp` gateway-shaped, `probe`, `slow`,
//! `boom`). Where the TypeScript `slow` sleeps for `ms`, these calls wait for a file the test (or the cell) writes.

mod common;

use common::{null_handler, runtime, wait_until};
use extensions::{ExtensionHost, NoHost, SessionInfo, parse_manifest};
use kernel::{Kernel, KernelOptions, Signal, Status};
use kernel_apis::tool_calls::{tool_call_summary, tool_event};
use kernel_apis::{KernelApis, MemoryDocumentStore, ToolCallEnd, ToolCalls, ToolRunner};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

struct Fixture {
    dir: tempfile::TempDir,
    calls: ToolCalls,
    ends: Arc<Mutex<Vec<ToolCallEnd>>>,
    _host: Arc<ExtensionHost>,
}

fn python() -> String {
    std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into())
}

impl Fixture {
    fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/common/fake_tools_extension.py");
        let text = json!({
            "name": "fake-tools",
            "command": python(),
            "args": [script.to_string_lossy()],
            "startupTimeoutMs": 120_000,
            "env": {"FAKE_CALLS_LOG": dir.path().join("calls.jsonl").to_string_lossy()},
        })
        .to_string();
        let manifest = dir.path().join("extension.json");
        std::fs::write(&manifest, &text).unwrap();
        let session = SessionInfo {
            cwd: dir.path().display().to_string(),
            mode: "print".into(),
            has_ui: false,
            flags: Map::new(),
        };
        let host = Arc::new(ExtensionHost::load(
            vec![parse_manifest(&manifest, &text).unwrap()],
            session,
            Arc::new(NoHost),
        ));
        let runner: Arc<dyn ToolRunner> = Arc::new(session_core::extension_tools::ExtensionTools::new(host.clone()));
        let calls = ToolCalls::new(Some(runner), Arc::new(MemoryDocumentStore::default()));
        calls.set_roots(Arc::new(|_: &str| Some("turn:run-1".to_string())));
        let ends = Arc::new(Mutex::new(Vec::new()));
        let sink = ends.clone();
        calls.on_end(Arc::new(move |end| sink.lock().unwrap().push(end)));
        Fixture {
            dir,
            calls,
            ends,
            _host: host,
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn kernel(&self, env: &[(&str, &str)]) -> Kernel {
        let mut options = KernelOptions::new(self.dir.path(), runtime());
        options.env = env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let apis = KernelApis::new(null_handler()).with(Arc::new(self.calls.clone()));
        Kernel::new(options, Arc::new(apis)).expect("kernel")
    }

    /// The fake's call log: one JSON object per call (and per cancellation).
    fn logged(&self) -> Vec<Value> {
        std::fs::read_to_string(self.path("calls.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn ends(&self) -> Vec<ToolCallEnd> {
        self.ends.lock().unwrap().clone()
    }
}

fn value(kernel: &Kernel, code: &str) -> String {
    let result = kernel.execute(code, None, None).unwrap();
    assert_eq!(result.status, Status::Ok, "{code}: {result:?}");
    result.result.unwrap_or_default()
}

#[test]
fn acceptance_a56_aborting_the_cell_cancels_a_call_it_is_waiting_on_and_nothing_is_announced() {
    let fixture = Fixture::new();
    let kernel = Arc::new(fixture.kernel(&[]));
    let signal = Signal::new();
    let never = fixture.path("never").display().to_string();
    let (running, cell) = (kernel.clone(), signal.clone());
    let code = format!("await tools.slow(ms=20000, gate='{never}', yield_after=None)");
    let waiting = std::thread::spawn(move || running.execute(&code, Some(&cell), None));
    wait_until("the slow call started", || {
        fixture.logged().iter().any(|c| c["tool"] == "slow")
    });
    signal.abort("Esc");
    let _ = waiting.join().unwrap();
    wait_until("the call is cancelled", || {
        fixture.calls.list().first().is_some_and(|c| c["status"] == "cancelled")
    });
    wait_until("the extension saw the cancellation", || {
        fixture
            .logged()
            .iter()
            .any(|c| c["tool"] == "slow" && c["aborted"] == true)
    });
    wait_until("the call's end", || fixture.ends().len() == 1);
    let end = &fixture.ends()[0];
    assert!(end.awaited);
    // Nothing is announced: the cell that waited already has the outcome.
    assert_eq!(tool_event(end), None);
    kernel.shutdown();
}

#[test]
fn acceptance_a56_a_slow_call_detaches_after_the_yield_time_and_its_end_is_reported_for_an_event() {
    let fixture = Fixture::new();
    let kernel = fixture.kernel(&[("ULTRON_TOOL_YIELD_AFTER", "0.3")]);
    let gate = fixture.path("go").display().to_string();
    let detached = value(
        &kernel,
        &format!(
            "r = await tools.slow(ms=1200, gate='{gate}')\n(r.running, r.ok, r.call.status, 'still running as call' in r, r.call.id)"
        ),
    );
    let id = detached
        .strip_prefix("(True, False, 'running', True, '")
        .and_then(|rest| rest.strip_suffix("')"))
        .unwrap_or_else(|| panic!("{detached}"))
        .to_string();
    assert!(
        id.starts_with("call-") && id[5..].chars().all(|c| c.is_ascii_hexdigit()),
        "{id}"
    );
    // The cell ended; the call keeps running in the host, and its end is not awaited by anyone.
    assert!(fixture.ends().is_empty());
    std::fs::write(&gate, "").unwrap();
    wait_until("the call's end", || fixture.ends().len() == 1);
    let end = &fixture.ends()[0];
    assert!(!end.awaited);
    assert!(!end.root_aborted);
    assert_eq!(
        (
            &end.call["id"],
            &end.call["status"],
            &end.call["rootId"],
            &end.call["source"]
        ),
        (&json!(id), &json!("completed"), &json!("turn:run-1"), &json!("repl"))
    );
    assert_eq!(tool_call_summary(&end.call, 240), "slow: ok; slept 1200");
    assert_eq!(
        tool_event(end),
        Some((
            id.clone(),
            "completed".into(),
            "slow: ok; slept 1200".into(),
            format!("await tools.result(\"{id}\")")
        ))
    );
    assert_eq!(
        value(
            &kernel,
            &format!("r = await tools.result(\"{id}\")\n(str(r), r.details)")
        ),
        "('slept 1200', {'ms': 1200})"
    );
    kernel.shutdown();
}

#[test]
fn acceptance_a56_yield_after_returns_a_handle_result_waits_cancel_stops_a_root_abort_cancels() {
    let fixture = Fixture::new();
    let kernel = fixture.kernel(&[]);
    let (first, never) = (
        fixture.path("first").display().to_string(),
        fixture.path("never").display().to_string(),
    );
    let handle = value(
        &kernel,
        &[
            format!("h = await tools.slow(ms=300, gate='{first}', yield_after=0)"),
            "first = (type(h).__name__, h.running)".into(),
            format!("open('{first}', 'w').close()"),
            "r = await h.result()".into(),
            "h2 = await mcp.call('exa-agent_exa_agent_wait_run', run_id='run-9', yield_after=0)".into(),
            format!("h3 = await tools.slow(ms=20000, gate='{never}', yield_after=0)"),
            "await h3.cancel()".into(),
            "(first, str(r), type(h2).__name__, h3.status)".into(),
        ]
        .join("\n"),
    );
    assert_eq!(handle, "(('ToolCall', True), 'slept 300', 'ToolCall', 'cancelled')");
    let root = value(
        &kernel,
        &format!("h4 = await tools.slow(ms=20000, gate='{never}', yield_after=0)\nh4.id"),
    );
    let id = root.replace('\'', "");
    fixture.calls.cancel_root("turn:run-1");
    // The cancellation settles the call and reports its end asynchronously (on the call's own thread): wait for both
    // as events (hang guard 120 s) instead of reading them right after cancel_root returns.
    wait_until("the root's call is cancelled", || {
        fixture
            .calls
            .list()
            .iter()
            .any(|c| c["id"] == id.as_str() && c["status"] == "cancelled")
    });
    wait_until("the cancelled call's end", || {
        fixture.ends().iter().any(|e| e.call["id"] == id.as_str())
    });
    let end = fixture
        .ends()
        .into_iter()
        .find(|e| e.call["id"] == id.as_str())
        .unwrap();
    assert!(end.root_aborted);
    assert_eq!(tool_event(&end), None);
    kernel.shutdown();
}

/// `<runtime_event kind="tool_done" id="call-…" status="…" summary="…" fetch="…" />` in a text: (id, status,
/// summary, fetch).
fn tool_done(text: &str) -> Option<(String, String, String, String)> {
    let at = text.find("<runtime_event kind=\"tool_done\"")?;
    let line = text[at..].lines().next()?;
    let field = |name: &str| -> Option<String> {
        let start = line.find(&format!("{name}=\""))? + name.len() + 2;
        let end = line[start..].find('"')? + start;
        Some(line[start..end].to_string())
    };
    Some((field("id")?, field("status")?, field("summary")?, field("fetch")?))
}

fn last_user(request: &common::Request) -> String {
    request.users().last().cloned().unwrap_or_default()
}

fn last_tool_result(request: &common::Request) -> Option<String> {
    let last = request.body["messages"].as_array()?.last()?.clone();
    (last["role"] == "tool").then(|| request.tool_results().last().cloned().unwrap_or_default())
}

fn first_user(request: &common::Request) -> String {
    request
        .users()
        .into_iter()
        .find(|u| !u.starts_with("[clock]"))
        .unwrap_or_default()
}

/// Ported from ultron-extension-tools-cli.test.ts: the fake extension is installed in the profile (its `mcp`
/// gateway's wait waits for a file the test writes), `ULTRON_TOOL_YIELD_AFTER=0.5` detaches the wait, the turn ends,
/// and the call's `tool_done` event re-invokes the idle root, which fetches the result.
#[test]
fn acceptance_a56_a_wait_past_the_tool_yield_detaches_and_its_tool_done_event_reinvokes_the_model() {
    use common::{HANG, Install, Provider, Reply, Request};
    const DETACH: &str = "DETACH: start a run and tell me when it is done";
    let provider = Provider::start(|request: &Request| {
        if first_user(request) != DETACH {
            return Reply::Text("ok".into());
        }
        if let Some((id, ..)) = tool_done(&last_user(request))
            && last_tool_result(request).is_none()
        {
            return Reply::Rlm(format!(
                "r = await tools.result(\"{id}\")\nprint(\"RESULT\", r.json()[\"output\"])"
            ));
        }
        if let Some(result) = last_tool_result(request).filter(|r| r.starts_with("RESULT")) {
            return Reply::Text(format!("DONE: {}", result.trim()));
        }
        if request.messages("assistant").is_empty() {
            return Reply::Rlm(
                [
                    "run = await mcp.call('exa-agent_exa_agent_create_run', query='solana tps')",
                    "w = await mcp.call('exa-agent_exa_agent_wait_run', run_id=run.json()['id'])",
                    "print('WAITING', w.running)",
                ]
                .join("\n"),
            );
        }
        Reply::Text("THE RUN IS GOING; I will report when it finishes.".into())
    });
    let install = Install::new(&provider);
    let gate = install.project().join("gate");
    let extension = install.agent().join("extensions/fake-tools");
    std::fs::create_dir_all(&extension).unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/common/fake_tools_extension.py");
    std::fs::write(
        extension.join("extension.json"),
        json!({
            "name": "fake-tools",
            "command": python(),
            "args": [script.to_string_lossy()],
            "startupTimeoutMs": 120_000,
            "env": {"FAKE_MCP_GATE": gate.to_string_lossy()},
        })
        .to_string(),
    )
    .unwrap();
    let mut client = install.rpc(&["--no-session"], &[("ULTRON_TOOL_YIELD_AFTER", "0.5")]);
    client.prompt_and_wait(DETACH, HANG).unwrap();
    assert!(
        client
            .get_last_assistant_text()
            .unwrap()
            .unwrap_or_default()
            .contains("THE RUN IS GOING")
    );
    let requests = provider.requests();
    // The model's tool list stays exactly [rlm].
    let tools: Vec<&str> = requests[0].body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(tools, ["rlm"]);
    let cell = last_tool_result(&requests[1]).unwrap();
    assert!(cell.contains("WAITING True"), "{cell}");
    std::fs::write(&gate, "").unwrap();
    // The call's end starts exactly one more run, which fetches the result.
    while client.transcript.iter().filter(|l| l["type"] == "agent_end").count() < 2 {
        client.mark();
        client.wait_for(&|l| l["type"] == "agent_end", HANG).unwrap();
    }
    assert_eq!(
        client.get_last_assistant_text().unwrap().as_deref(),
        Some("DONE: RESULT answer for solana tps")
    );
    let (id, status, summary, fetch) = provider
        .requests()
        .iter()
        .find_map(|r| tool_done(&last_user(r)))
        .expect("a request carries the call's event");
    assert_eq!(status, "completed");
    assert!(summary.contains("mcp exa-agent_exa_agent_wait_run: ok;"), "{summary}");
    assert_eq!(fetch, format!("await tools.result(&quot;{id}&quot;)"));
    let (_, code) = client.finish(HANG);
    assert_eq!(code, Some(0));
}
