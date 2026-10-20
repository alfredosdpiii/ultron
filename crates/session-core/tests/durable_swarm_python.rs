//! Python-level process tests of the durable many-agent APIs through the real `runtime.py` (`swarm_api.py`): the
//! model's REPL cells call `rlm.spawn(keep=True)`, `rlm.collect`, `rlm.ask`, `rlm.partial`, `swarm`, `board` and
//! `notes`; every request reaches the Rust host operations through the kernel protocol, and every durable record
//! goes to the product's durable store (a format-4 session file through `JournalStorage`). One test SIGKILLs the
//! process (and its kernel) after a persistent child answered, reopens the session file, and asks the child again
//! from a new cell: the child survived the restart with its context.

#[path = "../../harness/tests/chat_support/mod.rs"]
mod chat_support;
mod journal_kill_support;
#[path = "../../harness/tests/support/mod.rs"]
mod support;

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use ai::Message;
use chat_support::*;
use durable::{ConversationId, SubmissionStatus};
use harness::chat::faux::{FauxStep, rlm_call, text_message};
use harness::chat::host_ops::HostOps;
use harness::chat::kernel_cells::{KernelCells, rlm_tool};
use harness::chat::{AgentChange, ChatHarness, ChatOptions, Conversation, SubmissionDraft};
use harness::scheduler::Registry;
use journal_kill_support::{create_session, kill_reported_pid, open_journal};
use kernel::{HostHandler, Kernel, KernelOptions, Signal};
use serde_json::{Map, Value, json};
use support::{bg, js};

const CHILD_ENV: &str = "ULTRON_SWARM_PY_CHILD";
const DIR_ENV: &str = "ULTRON_SWARM_PY_DIR";

fn runtime() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/src/ultron/rlm/runtime.py")
}

/// Root turns: the code its REPL call runs for each user message.
fn code_for(user: &str) -> String {
    match user {
        "go" => r#"
import json
h = await rlm.spawn("You own the parser.", name="parser", keep=True)
first = await rlm.collect([h])
follow = await rlm.ask(h, "And the lexer?", timeout=60)
try:
    await rlm.ask(h, "?", timeout=0)
    bad_timeout = "accepted"
except ValueError as error:
    bad_timeout = str(error)
await swarm.register(topics=["planning"], work="split the parser")
task = await board.add("fix lexer", detail="escapes")
claimed = await board.claim(task["id"])
await notes.append("lexer owned by parser")
routed = await swarm.route("who owns the parser")
await rlm.partial("checkpoint\n")
print(json.dumps({
    "child": h.rlm_child_id,
    "first": first[0]["result"]["value"],
    "follow": follow["value"],
    "badTimeout": bad_timeout,
    "claimed": [claimed["status"], claimed["owner"]],
    "notes": (await notes.read())["text"],
    "routed": [entry["name"] for entry in routed],
    "agents": sorted((await swarm.agents()).keys()),
}))
"#
        .into(),
        "again" => r#"
import json
child = (await swarm.agents())["parser"]["child"]
answer = await rlm.ask(child, "Still there?")
print(json.dumps({"answer": answer["value"], "board": [t["status"] for t in await board.list()]}))
"#
        .into(),
        other => format!("print({other:?})"),
    }
}

/// Faux model: a root turn calls the REPL once with [`code_for`] its input, then answers `done`; the persistent
/// child answers its brief and each follow-up.
fn route(setup: &Setup) {
    let steps = (0..60)
        .map(|_| {
            FauxStep::factory(|context, _, _| {
                let users: Vec<(usize, String)> = context
                    .messages
                    .iter()
                    .enumerate()
                    .filter(|(_, message)| matches!(message, Message::User(_)))
                    .filter_map(|(index, message)| text_of(message).map(|text| (index, text)))
                    .collect();
                let (last_index, last) = users.last().cloned().unwrap_or_default();
                if users.first().is_some_and(|(_, first)| first == "go") {
                    let answered = context
                        .messages
                        .iter()
                        .skip(last_index)
                        .any(|message| matches!(message, Message::ToolResult(_)));
                    return Ok(if answered {
                        text_message("done")
                    } else {
                        rlm_call(&code_for(&last), &format!("c-{last}"))
                    });
                }
                Ok(text_message(&match last.as_str() {
                    "You own the parser." => "I know the parser.".to_owned(),
                    other => format!("re: {other}"),
                }))
            })
        })
        .collect();
    setup.faux.set_responses(steps);
}

struct Opened {
    chat: ChatHarness,
    root: Conversation,
    cells: Arc<KernelCells>,
}

fn open(dir: &Path, journal: &Path, setup: &Setup) -> Opened {
    let cwd = dir.to_path_buf();
    let open_lane = Arc::new(move |_: ConversationId, _: &str| {
        let mut options = KernelOptions::new(&cwd, runtime());
        options.env = vec![
            (
                "ULTRON_CODING_AGENT_DIR".into(),
                cwd.join("agent").display().to_string(),
            ),
            ("ULTRON_SERVER_DIR".into(), cwd.join("server").display().to_string()),
        ];
        let fallback: Arc<dyn HostHandler> = Arc::new(|kind: &str, _: Map<String, Value>, _: &Signal| {
            Err::<Value, String>(format!("Unknown host request {kind}"))
        });
        let kernel = Kernel::new(options, fallback).map_err(|error| error.to_string())?;
        kernel.start().map_err(|error| error.to_string())?;
        Ok(Arc::new(kernel))
    });
    let ops: Arc<OnceLock<HostOps>> = Arc::default();
    let handlers = {
        let ops = ops.clone();
        Arc::new(move |call: &harness::chat::CellCall| {
            ops.get()
                .expect("host operations are set after open")
                .handler(call.clone(), None)
        })
    };
    let cells = Arc::new(KernelCells::new(rlm_tool("Python REPL"), open_lane, Some(handlers)));
    let mut options = ChatOptions::new(Registry::new(), Arc::new(setup.faux.clone()), cells.clone());
    let reports = setup.reports.clone();
    options.on_report = Some(Arc::new(move |error| reports.lock().unwrap().push(error.clone())));
    let chat = ChatHarness::open(open_journal(journal), options).unwrap();
    let _ = ops.set(HostOps::new(chat.clone()));
    let root = chat
        .root(Some(&AgentChange::model(harness::chat::faux::Faux::model_ref())))
        .unwrap();
    Opened { chat, root, cells }
}

/// The printed JSON of the result of the root's REPL call for `user`.
fn printed(root: &Conversation, call: &str) -> Value {
    let result = root
        .entries()
        .unwrap()
        .into_iter()
        .filter(|entry| entry.kind == "pi.tool-result")
        .map(|entry| entry.model.unwrap()[0].clone())
        .find(|result| result["toolCallId"] == call)
        .unwrap_or_else(|| panic!("no result for {call}"));
    let text = result["content"][0]["text"].as_str().unwrap().to_owned();
    assert_eq!(result["isError"], false, "{text}");
    serde_json::from_str(text.lines().last().unwrap()).unwrap_or_else(|_| panic!("{text}"))
}

fn turn(opened: &Opened, user: &str) {
    let settled = opened
        .root
        .submit(SubmissionDraft::input(user))
        .unwrap()
        .wait(&bg())
        .unwrap();
    assert_eq!(settled.status, SubmissionStatus::Done);
}

#[test]
fn session_core_durable_swarm_python_runs_the_durable_agent_apis_from_repl_cells() {
    let dir = tempfile::tempdir().unwrap();
    let journal = create_session(dir.path());
    let setup = Setup::new();
    route(&setup);
    let opened = open(dir.path(), &journal, &setup);
    turn(&opened, "go");
    let out = printed(&opened.root, "c-go");
    assert_eq!(out["first"], "I know the parser.");
    assert_eq!(out["follow"], "re: And the lexer?");
    assert_eq!(
        out["badTimeout"],
        "rlm.ask timeout must be a positive number of seconds"
    );
    assert_eq!(out["claimed"], json!(["claimed", "root"]));
    assert_eq!(out["notes"], "[root] lexer owned by parser\n");
    // Both hold "parser" in their recent work; the child that owns it ranks first (registered first on a tie).
    assert_eq!(out["routed"], json!(["parser", "root"]));
    assert_eq!(out["agents"], json!(["parser", "root"]));
    assert!(out["child"].as_str().is_some_and(|id| id.parse::<u64>().is_ok()));
    // The kept child is a live background task; the root's run has ended.
    let child = durable::TaskId(out["child"].as_str().unwrap().parse().unwrap());
    assert_eq!(
        js(&opened.chat.get_task(child).unwrap().unwrap().state)["checkpoint"]["phase"],
        "idle"
    );
    assert!(setup.reports().is_empty(), "{:?}", setup.reports());
    opened.chat.close().unwrap();
    opened.cells.shutdown();
}

fn child_main(dir: &Path, journal: &Path) {
    let setup = Setup::new();
    route(&setup);
    let opened = open(dir, journal, &setup);
    turn(&opened, "go");
    let pids: Vec<String> = opened
        .cells
        .open_lanes()
        .into_iter()
        .filter_map(|(conversation, lane)| opened.cells.lane(conversation, &lane).ok()?.pid())
        .map(|pid| pid.to_string())
        .collect();
    // Written whole, then renamed: the parent never reads a half-written file.
    std::fs::write(dir.join("ready.tmp"), pids.join(" ")).unwrap();
    std::fs::rename(dir.join("ready.tmp"), dir.join("ready")).unwrap();
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}

/// Child entry point: does nothing unless the parent test re-ran this binary in child mode.
#[test]
fn session_core_durable_swarm_python_child_process() {
    let (Ok(journal), Ok(dir)) = (std::env::var(CHILD_ENV), std::env::var(DIR_ENV)) else {
        return;
    };
    child_main(Path::new(&dir), Path::new(&journal));
}

#[test]
fn session_core_durable_swarm_python_asks_a_persistent_child_again_after_the_process_was_killed() {
    let dir = tempfile::tempdir().unwrap();
    let journal = create_session(dir.path());
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "session_core_durable_swarm_python_child_process",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, &journal)
        .env(DIR_ENV, dir.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let ready = dir.path().join("ready");
    let deadline = Instant::now() + Duration::from_secs(120);
    while !ready.exists() {
        assert!(Instant::now() < deadline, "child never finished its first turn");
        assert!(child.try_wait().unwrap().is_none(), "child exited early");
        std::thread::sleep(Duration::from_millis(10));
    }
    child.kill().unwrap();
    child.wait().unwrap();
    for pid in std::fs::read_to_string(&ready).unwrap().split_whitespace() {
        kill_reported_pid(pid);
    }

    let setup = Setup::new();
    route(&setup);
    let opened = open(dir.path(), &journal, &setup);
    turn(&opened, "again");
    let out = printed(&opened.root, "c-again");
    assert_eq!(out, json!({ "answer": "re: Still there?", "board": ["claimed"] }));
    // The child kept its context across the restart: the follow-up is its third question.
    let agents = opened.chat.commit(harness::chat::swarm::agents).unwrap();
    let conversation = ConversationId(agents["parser"]["conversation"].as_u64().unwrap());
    let child_conversation = opened.chat.conversation(conversation).unwrap().unwrap();
    assert_eq!(
        transcript(&child_conversation.entries().unwrap()),
        [
            "pi.user:You own the parser.",
            "pi.assistant:I know the parser.",
            "pi.user:And the lexer?",
            "pi.assistant:re: And the lexer?",
            "pi.user:Still there?",
            "pi.assistant:re: Still there?",
        ]
    );
    opened.chat.close().unwrap();
    opened.cells.shutdown();
}
