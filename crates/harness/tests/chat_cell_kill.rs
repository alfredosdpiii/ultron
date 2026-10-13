//! Kill/reopen process test of the durable REPL cell (spec §8.4, rewriteplan "Example recovery contract"): a child
//! process (this binary re-run in child mode) opens a ChatHarness over a real SQLite or JSONL file with cells running
//! in the real Python kernel (`runtime.py`). The model calls the REPL; the cell commits partial output through the
//! `cell.output` host request, records an external effect, and blocks. The parent SIGKILLs the child and its kernel,
//! reopens the same file with a fresh kernel, and checks the contract: the cell's committed intent is answered with
//! one `interrupted` result carrying the committed partial output, the cell's code never runs again, and the run
//! finishes.

mod chat_support;
mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chat_support::*;
use durable::{JsonlOptions, JsonlStorage, SqliteOptions, SqliteStorage, SubmissionStatus};
use harness::chat::faux::{rlm_call, text_message};
use harness::chat::kernel_cells::{KernelCells, rlm_tool};
use harness::chat::{ChatHarness, ChatOptions, Conversation, SubmissionDraft};
use harness::scheduler::Registry;
use kernel::{HostHandler, Kernel, KernelOptions, Signal};
use serde_json::{Map, Value, json};
use support::{bg, eventually};

const CHILD_ENV: &str = "ULTRON_CHAT_KILL_CHILD";
const DIR_ENV: &str = "ULTRON_CHAT_KILL_DIR";

fn runtime() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/src/ultron/rlm/runtime.py")
}

fn open_storage(backend: &str, dir: &Path, options: ChatOptions) -> ChatHarness {
    match backend {
        "sqlite" => ChatHarness::open(
            SqliteStorage::open(dir.join("session.sqlite"), SqliteOptions::default()).unwrap(),
            options,
        ),
        _ => ChatHarness::open(
            JsonlStorage::open(dir.join("session"), JsonlOptions::default()).unwrap(),
            options,
        ),
    }
    .unwrap()
}

/// Cells in per-lane kernels rooted at `dir`, with private agent and server directories.
fn kernel_cells(dir: &Path) -> Arc<KernelCells> {
    let cwd = dir.to_path_buf();
    let open = Arc::new(move |_conversation: durable::ConversationId, _lane: &str| {
        let mut options = KernelOptions::new(&cwd, runtime());
        options.env = vec![
            (
                "ULTRON_CODING_AGENT_DIR".into(),
                cwd.join("agent").display().to_string(),
            ),
            ("ULTRON_SERVER_DIR".into(), cwd.join("server").display().to_string()),
        ];
        let handler: Arc<dyn HostHandler> = Arc::new(|kind: &str, _: Map<String, Value>, _: &Signal| {
            Err::<Value, String>(format!("Unknown host request {kind}"))
        });
        let kernel = Kernel::new(options, handler).map_err(|error| error.to_string())?;
        kernel.start().map_err(|error| error.to_string())?;
        Ok(Arc::new(kernel))
    });
    Arc::new(KernelCells::new(rlm_tool("Python REPL"), open, None))
}

fn open(backend: &str, dir: &Path, setup: &Setup, cells: Arc<KernelCells>) -> (ChatHarness, Conversation) {
    let mut options = ChatOptions::new(Registry::new(), Arc::new(setup.faux.clone()), cells);
    let reports = setup.reports.clone();
    options.on_report = Some(Arc::new(move |error| reports.lock().unwrap().push(error.clone())));
    let chat = open_storage(backend, dir, options);
    let root = chat
        .root(Some(&harness::chat::AgentChange::model(
            harness::chat::faux::Faux::model_ref(),
        )))
        .unwrap();
    (chat, root)
}

fn cell_code(dir: &Path) -> String {
    let marker = dir.join("effects.log");
    format!(
        "await rlm.host_request(\"cell.output\", {{\"text\": \"partial line\\n\"}})\n\
         with open({marker:?}, \"a\") as f:\n    f.write(\"ran\\n\")\n\
         import time\n\
         time.sleep(600)\n",
        marker = marker.display().to_string()
    )
}

fn child_main(backend: &str, dir: &Path) {
    let setup = Setup::new();
    setup.faux.set_responses(vec![rlm_call(&cell_code(dir), "c1").into()]);
    let cells = kernel_cells(dir);
    let (chat, root) = open(backend, dir, &setup, cells.clone());
    root.submit(SubmissionDraft::input("go")).unwrap();
    // The partial output is durable and the effect happened: hand the kernel's PID to the parent.
    eventually(|| {
        chat.live(root.id).unwrap()["tools"][0]["output"] == json!("partial line\n")
            && std::fs::read_to_string(dir.join("effects.log")).is_ok_and(|log| log == "ran\n")
    });
    let pid = cells.lane(root.id, "main").unwrap().pid().unwrap();
    // Written whole, then renamed: the parent never reads a half-written file.
    std::fs::write(dir.join("ready.tmp"), pid.to_string()).unwrap();
    std::fs::rename(dir.join("ready.tmp"), dir.join("ready")).unwrap();
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}

/// Child entry point: does nothing unless the parent test re-ran this binary in child mode.
#[test]
fn harness_chat_cell_kill_child_process() {
    let (Ok(backend), Ok(dir)) = (std::env::var(CHILD_ENV), std::env::var(DIR_ENV)) else {
        return;
    };
    child_main(&backend, Path::new(&dir));
}

fn kill(pid: &str) {
    let _ = std::process::Command::new("kill").args(["-9", pid]).status();
}

fn kill_and_recover(backend: &str) {
    let dir = tempfile::tempdir().unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "harness_chat_cell_kill_child_process",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, backend)
        .env(DIR_ENV, dir.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let ready = dir.path().join("ready");
    let deadline = Instant::now() + Duration::from_secs(120);
    while !ready.exists() {
        assert!(Instant::now() < deadline, "child never reached the kill point");
        assert!(child.try_wait().unwrap().is_none(), "child exited early");
        std::thread::sleep(Duration::from_millis(10));
    }
    let kernel = std::fs::read_to_string(&ready).unwrap();
    // SIGKILL the process and its kernel mid-cell.
    child.kill().unwrap();
    child.wait().unwrap();
    kill(kernel.trim());

    let setup = Setup::new();
    setup.faux.set_responses(vec![text_message("done").into()]);
    let cells = kernel_cells(dir.path());
    let (chat, root) = open(backend, dir.path(), &setup, cells.clone());
    let submission = chat
        .inspect()
        .unwrap()
        .submissions
        .first()
        .map(|record| record.id)
        .expect("the run's input is still placed");
    let settled = chat.submission(submission).unwrap().unwrap().wait(&bg()).unwrap();
    assert_eq!(settled.status, SubmissionStatus::Done);

    let entries = root.entries().unwrap();
    assert_eq!(
        transcript(&entries),
        ["pi.user:go", "pi.assistant", "pi.tool-result", "pi.assistant:done"]
    );
    let results: Vec<Value> = entries
        .iter()
        .filter(|entry| entry.kind == "pi.tool-result")
        .map(|entry| entry.model.as_ref().unwrap()[0].clone())
        .collect();
    assert_eq!(results.len(), 1, "exactly one result for the interrupted cell");
    let text: Vec<&str> = results[0]["content"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["text"].as_str().unwrap())
        .collect();
    assert_eq!(
        text,
        [
            "partial line\n",
            "<harness>\n[error] Tool rlm was interrupted and may have partially run\n</harness>"
        ]
    );
    assert_eq!(results[0]["isError"], true);
    assert_eq!(results[0]["toolCallId"], "c1");
    // Nothing was executed again: the effect happened once, and no kernel was started for the cell.
    assert!(cells.open_lanes().is_empty());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("effects.log")).unwrap(),
        "ran\n"
    );
    assert_eq!(live(&chat, root.id), json!({}));
    chat.close().unwrap();
    cells.shutdown();
    assert!(setup.reports().is_empty(), "{:?}", setup.reports());
}

#[test]
fn harness_chat_cell_kill_answers_a_cell_interrupted_by_sigkill_exactly_once_without_rerunning_it_sqlite() {
    kill_and_recover("sqlite");
}

#[test]
fn harness_chat_cell_kill_answers_a_cell_interrupted_by_sigkill_exactly_once_without_rerunning_it_jsonl() {
    kill_and_recover("jsonl");
}

/// The same cell runs to completion through the kernel when nothing interrupts it: the executor's own output.
#[test]
fn harness_chat_cell_kill_runs_a_cell_in_the_kernel_and_answers_with_its_output() {
    let dir = tempfile::tempdir().unwrap();
    let setup = Setup::new();
    setup.faux.set_responses(vec![
        rlm_call(
            "await rlm.host_request(\"cell.output\", {\"text\": \"note\\n\"})\nprint(6 * 7)",
            "c1",
        )
        .into(),
        text_message("done").into(),
    ]);
    let cells = kernel_cells(dir.path());
    let (chat, root) = open("sqlite", dir.path(), &setup, cells.clone());
    let settled = root.submit(SubmissionDraft::input("go")).unwrap().wait(&bg()).unwrap();
    assert_eq!(settled.status, SubmissionStatus::Done);
    let result = root
        .entries()
        .unwrap()
        .into_iter()
        .find(|entry| entry.kind == "pi.tool-result")
        .unwrap()
        .model
        .unwrap()[0]
        .clone();
    assert_eq!(result["content"], json!([{ "type": "text", "text": "42" }]));
    assert_eq!(result["isError"], false);
    chat.close().unwrap();
    cells.shutdown();
}
