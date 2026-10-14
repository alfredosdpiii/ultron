//! The composition root end to end: `ultron -p` (and the library's prompted run) over the real host stack
//! (KernelApis -> inference -> delegation and extensions -> sandboxed host with Loki's file hooks) and the real
//! Python kernel, against a scripted provider. Every run uses a private HOME, ULTRON_CODING_AGENT_DIR and
//! ULTRON_SERVER_DIR under the system temp dir (asserted), never the user's.

mod common;

use common::{
    Naming, Sandbox, anthropic_text, anthropic_tool, assert_private_home, mock_server, routed_server, session_files,
    tool_result_text,
};
use kernel::Signal;
use kernel_apis::MemoryBackend;
use serde_json::{Map, Value, json};
use std::path::Path;
use std::process::Output;
use std::sync::{Arc, Mutex};

const MODEL: &str = "anthropic/claude-sonnet-4-5";

fn sandbox(port: u16) -> Sandbox {
    let sandbox = Sandbox::new(port);
    assert_private_home(&sandbox.root);
    std::fs::write(
        sandbox.agent_dir.join("settings.json"),
        r#"{"sessionNaming":{"enabled":false}}"#,
    )
    .unwrap();
    sandbox
}

/// [`sandbox`] whose cells run in the command sandbox (`sandbox.mode`, off by default).
fn sandboxed(port: u16) -> Sandbox {
    let sandbox = sandbox(port);
    std::fs::write(
        sandbox.agent_dir.join("settings.json"),
        r#"{"sessionNaming":{"enabled":false},"sandbox":{"mode":"auto"}}"#,
    )
    .unwrap();
    sandbox
}

fn run(sandbox: &Sandbox, args: &[&str]) -> Output {
    sandbox.ultron().args(args).output().unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The tool list of every model request: exactly the `rlm` tool.
fn assert_only_rlm(requests: &[Value]) {
    assert!(!requests.is_empty());
    for request in requests {
        let names: Vec<&str> = request["body"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["rlm"], "the model's tool list");
        assert!(cli::compose::only_rlm(request["body"]["tools"].as_array().unwrap()));
    }
}

/// Whether the sandbox isolates shell commands here (bubblewrap, unshare or Docker works).
fn isolation_available(sandbox: &Sandbox) -> bool {
    let output = run(sandbox, &["sandbox", "doctor"]);
    let report: Value = serde_json::from_str(&stdout(&output)).unwrap();
    !report["mechanism"].is_null()
}

#[test]
fn cli_composition_sandboxed_cell_cannot_write_home() {
    let write_home = "import os\ntry:\n    await write(os.path.expanduser('~/escape.txt'), 'x')\nexcept Exception as e:\n    print('write refused:', e)\nprint('exists:', os.path.exists(os.path.expanduser('~/escape.txt')))";
    let shell_home = "r = await bash('echo x > ~/escape-shell.txt; echo rc=$?')\nprint(r)";
    let write_project = "await write('inside.txt', 'ok')\nprint(open('inside.txt').read())";
    // The kernel itself runs inside the sandbox: Python's own open() cannot write the home either.
    let open_home = "import os\ntry:\n    open(os.path.expanduser('~/direct.txt'), 'w').write('x')\n    print('opened')\nexcept OSError as e:\n    print('open refused', e.errno)";
    let (port, seen) = mock_server(
        vec![
            anthropic_tool("toolu_a", write_home),
            anthropic_tool("toolu_b", shell_home),
            anthropic_tool("toolu_c", write_project),
            anthropic_tool("toolu_d", open_home),
            anthropic_text("done"),
        ],
        Naming::Fail,
    );
    let sandbox = sandboxed(port);
    let output = run(&sandbox, &["-p", "--model", MODEL, "try to escape"]);
    assert_eq!(stdout(&output), "done\n", "{}", stderr(&output));
    let requests = seen.lock().unwrap().requests.clone();
    assert_only_rlm(&requests);
    let write = tool_result_text(&requests[1]["body"], "toolu_a").unwrap();
    assert!(write.contains("write refused:"), "{write}");
    assert!(write.contains("outside the sandbox's writable paths"), "{write}");
    assert!(write.contains("exists: False"), "{write}");
    assert!(!sandbox.root.join("escape.txt").exists());
    if isolation_available(&sandbox) {
        let shell = tool_result_text(&requests[2]["body"], "toolu_b").unwrap();
        assert!(!shell.contains("rc=0"), "the shell wrote into HOME: {shell}");
        assert!(!sandbox.root.join("escape-shell.txt").exists());
        let opened = tool_result_text(&requests[4]["body"], "toolu_d").unwrap();
        assert!(
            opened.starts_with("open refused"),
            "the kernel wrote into HOME: {opened}"
        );
        assert!(!sandbox.root.join("direct.txt").exists());
    }
    // The project stays writable.
    let inside = tool_result_text(&requests[3]["body"], "toolu_c").unwrap();
    assert!(inside.contains("ok"), "{inside}");
    assert_eq!(std::fs::read_to_string(sandbox.cwd.join("inside.txt")).unwrap(), "ok");
}

#[test]
fn cli_composition_kernel_sigkill_recovers_state_and_reports_restored_names() {
    let (port, seen) = mock_server(
        vec![
            anthropic_tool("toolu_1", "answer = 41\nnotes = ['kept']\nanswer"),
            anthropic_tool("toolu_2", "import os, signal\nos.kill(os.getpid(), signal.SIGKILL)"),
            anthropic_tool("toolu_3", "answer + 1, notes, (await rlm.recovery())['restored']"),
            anthropic_text("recovered"),
        ],
        Naming::Fail,
    );
    let sandbox = sandbox(port);
    let output = run(&sandbox, &["-p", "--model", MODEL, "crash the kernel"]);
    assert_eq!(stdout(&output), "recovered\n", "{}", stderr(&output));
    let requests = seen.lock().unwrap().requests.clone();
    assert_only_rlm(&requests);
    let killed = tool_result_text(&requests[2]["body"], "toolu_2").unwrap();
    assert!(!killed.contains("Restored"), "{killed}");
    let after = tool_result_text(&requests[3]["body"], "toolu_3").unwrap();
    assert!(after.contains("[RLM kernel restarted after:"), "{after}");
    assert!(after.contains("[Restored: _, answer, notes, state]"), "{after}");
    // rlm.recovery() (the lanes' API) reports the same restart to the cell.
    assert!(
        after.contains("(42, ['kept'], ['_', 'answer', 'notes', 'state'])"),
        "{after}"
    );
    // The checkpoint lives under the profile's per-session snapshot directory, signed with the profile key.
    let snapshots = sandbox.agent_dir.join("rlm-snapshots");
    let session = std::fs::read_dir(&snapshots).unwrap().next().unwrap().unwrap().path();
    assert!(session.join("main.snapshot").exists());
    assert!(sandbox.agent_dir.join("rlm-snapshot.key").exists());
}

#[test]
fn cli_composition_cell_calls_a_code_skill() {
    let propose = "r = await skills.propose_code('double', 'def double(x):\\n    return 2 * x\\n', 'from code_skills import double\\ndef test_double():\\n    assert double.double(2) == 4\\n', {'worked': 'doubling'})\nprint(r['status'], r['test']['passed'])";
    let call = "from code_skills import double\ndouble.double(21)";
    let (port, seen) = mock_server(
        vec![
            anthropic_tool("toolu_p", propose),
            anthropic_tool("toolu_q", call),
            anthropic_tool("toolu_l", "[s['name'] for s in await skills.code_list()]"),
            anthropic_text("done"),
        ],
        Naming::Fail,
    );
    let sandbox = sandbox(port);
    let output = run(&sandbox, &["-p", "--model", MODEL, "learn a skill"]);
    assert_eq!(stdout(&output), "done\n", "{}", stderr(&output));
    let requests = seen.lock().unwrap().requests.clone();
    assert_only_rlm(&requests);
    let proposed = tool_result_text(&requests[1]["body"], "toolu_p").unwrap();
    assert_eq!(proposed, "active True");
    assert_eq!(tool_result_text(&requests[2]["body"], "toolu_q").unwrap(), "42");
    assert_eq!(tool_result_text(&requests[3]["body"], "toolu_l").unwrap(), "['double']");
    // Code skills live in the profile (ULTRON_CODE_SKILLS_DIR defaults to <agent>/skills).
    assert!(sandbox.agent_dir.join("skills").join("double").exists());
}

#[test]
fn cli_composition_cell_calls_an_extension_function_never_a_tool() {
    let (port, seen) = mock_server(
        vec![
            anthropic_tool("toolu_e", "print(await ext.hello.greet(name='Rust'))\nprint(ext.hello)"),
            anthropic_text("greeted"),
        ],
        Naming::Fail,
    );
    let sandbox = sandbox(port);
    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("../extensions/examples/hello");
    let installed = sandbox.agent_dir.join("extensions").join("hello");
    std::fs::create_dir_all(&installed).unwrap();
    for file in ["extension.json", "hello.py"] {
        std::fs::copy(example.join(file), installed.join(file)).unwrap();
    }
    let output = run(&sandbox, &["-p", "--model", MODEL, "greet"]);
    assert_eq!(stdout(&output), "greeted\n", "{}", stderr(&output));
    let requests = seen.lock().unwrap().requests.clone();
    // The function is a kernel API listed in the system prompt; the tool list stays [rlm].
    assert_only_rlm(&requests);
    let system = requests[0]["body"]["system"].to_string();
    assert!(system.contains("ext.hello.greet(name: string)"), "{system}");
    let greeted = tool_result_text(&requests[1]["body"], "toolu_e").unwrap();
    // runtime.py's `ext` (ext_api.py) shows an extension with its functions.
    assert_eq!(
        greeted,
        "Hello, Rust!\n<ext.hello: cancelled, crash, greet, info, shout, slow>"
    );
}

#[test]
fn cli_composition_delegation_child_done_continues_the_run() {
    // Host collect mode: the cell that spawns never waits; the parent ends its turn, the host waits for the child,
    // and the child's end continues the run as a child_done runtime event; collecting then returns its result.
    let (port, seen) = routed_server(
        vec![
            anthropic_tool(
                "toolu_s",
                "h = await rlm.spawn('say hi', name='greeter')\ntry:\n    await rlm.collect([h])\nexcept RuntimeError as e:\n    print('not waiting:', 'The host waits for them, not this cell' in str(e))\nprint('spawned')",
            ),
            anthropic_text("waiting for the child"),
            anthropic_tool(
                "toolu_c",
                "r = await rlm.collect([h])\nprint(r[0]['result']['status'], r[0]['result'].get('value'))",
            ),
            anthropic_text("parent done"),
        ],
        "say hi",
        vec![anthropic_text("hi from the child")],
    );
    let sandbox = sandbox(port);
    let output = run(&sandbox, &["-p", "--model", MODEL, "delegate"]);
    assert_eq!(stdout(&output), "parent done\n", "{}", stderr(&output));
    let requests = seen.lock().unwrap().clone();
    assert_only_rlm(&requests);
    let parent: Vec<&Value> = requests.iter().filter(|r| r["child"] == false).collect();
    let child: Vec<&Value> = requests.iter().filter(|r| r["child"] == true).collect();
    assert_eq!((parent.len(), child.len()), (4, 1));
    // The cell's collect does not wait for the running child (host collect mode).
    assert_eq!(
        tool_result_text(&parent[1]["body"], "toolu_s").unwrap(),
        "not waiting: True\nspawned"
    );
    // The third parent request carries the event that continued the run.
    let event = parent[2]["body"]["messages"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .to_string();
    assert!(event.contains("<runtime_event kind=\\\"child_done\\\""), "{event}");
    assert!(
        event.contains("status=\\\"succeeded\\\" summary=\\\"rlm-child@1: [unverified] hi from the child\\\""),
        "{event}"
    );
    assert_eq!(
        tool_result_text(&parent[3]["body"], "toolu_c").unwrap(),
        "succeeded hi from the child"
    );
}

#[test]
fn cli_composition_session_files_stay_private() {
    // Every file a composed session writes is under the private profile or the project.
    let (port, _seen) = mock_server(
        vec![anthropic_tool("toolu_x", "x = 1"), anthropic_text("ok")],
        Naming::Fail,
    );
    let sandbox = sandbox(port);
    let output = run(&sandbox, &["-p", "--model", MODEL, "hello"]);
    assert_eq!(stdout(&output), "ok\n", "{}", stderr(&output));
    let files = session_files(&sandbox.sessions_dir());
    assert_eq!(files.len(), 1);
    let session = files[0]
        .file_stem()
        .unwrap()
        .to_string_lossy()
        .rsplit('_')
        .next()
        .unwrap()
        .to_string();
    for dir in ["rlm-snapshots", "rlm-output", "rlm-tmp"] {
        assert!(sandbox.agent_dir.join(dir).join(&session).is_dir(), "{dir}");
    }
}

/// A Hindsight-shaped memory bank in memory: retained items are recalled by substring.
#[derive(Default)]
struct FakeBank {
    calls: Mutex<Vec<Value>>,
    items: Mutex<Vec<Value>>,
}

impl MemoryBackend for FakeBank {
    fn namespace(&self) -> String {
        "fake://bank".into()
    }
    fn scope_tags(&self) -> Map<String, Value> {
        json!({"session": ["ultron:session:s1"], "project": ["ultron:project:p1"]})
            .as_object()
            .unwrap()
            .clone()
    }
    fn recall(&self, request: &Value, _: &Signal) -> Result<Value, String> {
        self.calls.lock().unwrap().push(json!(["recall", request]));
        let query = request["query"].as_str().unwrap_or_default().to_lowercase();
        let results: Vec<Value> = self
            .items
            .lock()
            .unwrap()
            .iter()
            .filter(|item| {
                item["content"]
                    .as_str()
                    .unwrap_or_default()
                    .to_lowercase()
                    .contains(&query)
            })
            .map(|item| {
                json!({"id": item["document_id"], "text": item["content"], "tags": item["tags"],
                    "document_id": item["document_id"], "type": "world", "metadata": item["metadata"]})
            })
            .collect();
        Ok(json!({"results": results}))
    }
    fn retain(&self, request: &Value, _: &Signal) -> Result<Value, String> {
        self.calls.lock().unwrap().push(json!(["retain", request]));
        self.items.lock().unwrap().push(request["items"][0].clone());
        Ok(json!({"success": true, "async": false}))
    }
    fn get(&self, id: &str, _: &Signal) -> Result<Value, String> {
        let items = self.items.lock().unwrap();
        let item = items.iter().find(|item| item["document_id"] == id);
        Ok(
            json!({"id": id, "original_text": item.map(|i| i["content"].clone()), "tags": item.map(|i| i["tags"].clone())}),
        )
    }
    fn delete(&self, id: &str, _: &Signal) -> Result<Value, String> {
        Ok(json!({"success": true, "document_id": id}))
    }
    fn operation(&self, id: &str, _: &Signal) -> Result<Value, String> {
        Ok(json!({"operation_id": id, "status": "completed"}))
    }
}

#[test]
fn cli_composition_cell_calls_the_memory_api_on_the_injected_backend() {
    let (port, seen) = mock_server(
        vec![
            anthropic_tool(
                "toolu_m",
                "m = await memory.propose('The build uses cargo workspaces', [{'ref': 'Cargo.toml'}])\nprint(m['state'], m['scope'], m['evidence'])\nprint([(x['state'], x['memoryId'] == m['memoryId']) for x in await memory.list()])",
            ),
            anthropic_text("remembered"),
        ],
        Naming::Fail,
    );
    let sandbox = sandbox(port);
    let bank = Arc::new(FakeBank::default());
    let vars: Vec<(String, String)> = vec![
        ("HOME".into(), sandbox.root.display().to_string()),
        (
            "ULTRON_CODING_AGENT_DIR".into(),
            sandbox.agent_dir.display().to_string(),
        ),
        (
            "ULTRON_SERVER_DIR".into(),
            sandbox.root.join("server").display().to_string(),
        ),
        ("ANTHROPIC_API_KEY".into(), "test-key".into()),
    ];
    let env = cli::Env {
        cwd: sandbox.cwd.clone(),
        home: sandbox.root.clone(),
        var: Arc::new(move |key| vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())),
        docs: config::paths::normalize(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/docs")),
    };
    // The session actor (what the session worker runs) with the backend injected, in this process.
    let root = sandbox.sessions_dir();
    let repo = session_journal::SessionRepo::new(&root, session_journal::SystemClock::shared());
    let created = repo.create(&sandbox.cwd.display().to_string(), None, None).unwrap();
    let open = session_core::core::OpenOptions {
        env: env.clone(),
        flags: session_core::ModelFlags {
            model: Some(MODEL.into()),
            ..Default::default()
        },
        session_root: root,
        metadata: created.metadata().clone(),
        mode: "print".into(),
        services: cli::compose::Services {
            memory: Some(bank.clone()),
            approver: None,
            ..Default::default()
        },
        auto_name: true,
    };
    drop(created);
    let (core, actor, _) = session_core::core::spawn(open, Box::new(|_| {})).unwrap();
    let ran = session_core::core::call(
        &core,
        session_core::core::Op::PromptAndWait {
            message: "remember".into(),
            images: Vec::new(),
        },
    )
    .unwrap()
    .unwrap();
    assert_eq!(ran["error"], Value::Null);
    let last = session_core::core::call(&core, session_core::core::Op::LastAssistantText).unwrap();
    let (done, closed) = std::sync::mpsc::channel();
    core.send(session_core::core::Input::Close(done)).unwrap();
    closed.recv().unwrap();
    actor.join().unwrap();
    let prompted_text = last.unwrap();
    assert_eq!(prompted_text, "remembered");
    let requests = seen.lock().unwrap().requests.clone();
    assert_only_rlm(&requests);
    let result = tool_result_text(&requests[1]["body"], "toolu_m").unwrap();
    assert_eq!(result, "stored session [{'ref': 'Cargo.toml'}]\n[('stored', True)]");
    let calls = bank.calls.lock().unwrap().clone();
    let retained = calls
        .iter()
        .find(|c| c[0] == "retain")
        .expect("the backend retained the memory");
    assert_eq!(retained[1]["items"][0]["content"], "The build uses cargo workspaces");
}

#[test]
fn cli_composition_sandbox_doctor_reports_isolation() {
    let (port, _seen) = mock_server(vec![], Naming::Fail);
    let sandbox = sandboxed(port);
    let output = run(&sandbox, &["sandbox", "doctor"]);
    let report: Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(report["mode"], "auto");
    assert_eq!(output.status.code(), Some(if report["ok"] == true { 0 } else { 1 }));
    if !report["mechanism"].is_null() {
        // The self-check tried the escapes: the project is writable, the home and the system are not.
        assert_eq!(report["selfCheck"]["project"], "writable", "{report:#}");
        assert_eq!(report["selfCheck"]["home"], "read-only", "{report:#}");
        assert_eq!(report["selfCheck"]["system"], "read-only", "{report:#}");
    }
    std::fs::write(sandbox.agent_dir.join("settings.json"), r#"{"sandbox":{"mode":"off"}}"#).unwrap();
    let off = run(&sandbox, &["sandbox", "doctor"]);
    let report: Value = serde_json::from_str(&stdout(&off)).unwrap();
    assert_eq!(
        (report["mode"].as_str(), report["ok"].as_bool()),
        (Some("off"), Some(true))
    );
    assert_eq!(off.status.code(), Some(0));
    let unknown = run(&sandbox, &["sandbox", "frobnicate"]);
    assert_eq!(unknown.status.code(), Some(1));
    assert!(stderr(&unknown).starts_with("Error: unknown sandbox command: frobnicate\nUsage: ultron sandbox doctor"));
}

#[test]
fn cli_composition_lane_declares_exactly_the_rlm_tool() {
    // With an extension that registers functions loaded, the lane's tool list and the tools its system message
    // declares are both exactly [rlm]: extension functions are kernel APIs.
    let sandbox = sandbox(1);
    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("../extensions/examples/hello");
    let installed = sandbox.agent_dir.join("extensions").join("hello");
    std::fs::create_dir_all(&installed).unwrap();
    for file in ["extension.json", "hello.py"] {
        std::fs::copy(example.join(file), installed.join(file)).unwrap();
    }
    let vars: Vec<(String, String)> = vec![
        ("HOME".into(), sandbox.root.display().to_string()),
        (
            "ULTRON_CODING_AGENT_DIR".into(),
            sandbox.agent_dir.display().to_string(),
        ),
        ("PATH".into(), std::env::var("PATH").unwrap_or_default()),
    ];
    let env = cli::Env {
        cwd: sandbox.cwd.clone(),
        home: sandbox.root.clone(),
        var: Arc::new(move |key| vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())),
        docs: config::paths::normalize(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/docs")),
    };
    let runtime = cli::compose::Runtime::new(
        &env,
        "print",
        sandbox.sessions_dir().join("lane"),
        cli::compose::Services::default(),
    )
    .unwrap();
    assert!(
        runtime
            .extensions
            .as_ref()
            .is_some_and(|host| !host.functions().is_empty())
    );
    let model = ai::models::builtin_model("anthropic", "claude-sonnet-4-5")
        .unwrap()
        .clone();
    let lane = cli::lane::Lane::open(&runtime, &model, Some("test-key".into()), "lane-test").unwrap();
    let tools: Vec<String> = lane.tools().iter().map(|t| t.tool().name.clone()).collect();
    assert_eq!(tools, ["rlm"]);
    let Some(ai::Message::System(system)) = lane.system_message(&runtime.system.current()) else {
        panic!("no system message");
    };
    let declared: Vec<String> = system.tools_added.iter().flatten().map(|t| t.name.clone()).collect();
    assert_eq!(declared, ["rlm"]);
    lane.shutdown();
    if let Some(host) = &runtime.extensions {
        host.shutdown();
    }
}

#[test]
fn cli_composition_cell_reaches_the_lane_api() {
    let cell = "await rlm.lane.open('side')\nprint([(l['lane'], l['cell'] is not None) for l in await rlm.lanes()])\nprint([c['lane'] for c in await rlm.cells()])";
    let (port, seen) = mock_server(
        vec![anthropic_tool("toolu_l", cell), anthropic_text("lanes")],
        Naming::Fail,
    );
    let sandbox = sandbox(port);
    let output = run(&sandbox, &["-p", "--model", MODEL, "lanes"]);
    assert_eq!(stdout(&output), "lanes\n", "{}", stderr(&output));
    let requests = seen.lock().unwrap().requests.clone();
    // The lane running this cell is busy; the opened one is idle; the running cell is listed.
    assert_eq!(
        tool_result_text(&requests[1]["body"], "toolu_l").unwrap(),
        "[('main', True), ('side', False)]\n['main']"
    );
}

#[test]
fn cli_composition_worktree_subagent_has_its_own_directory_and_prompt() {
    let (port, seen) = routed_server(
        vec![
            anthropic_tool(
                "toolu_w",
                "h = await rlm.spawn('look around', name='scout', worktree=True)\nprint('spawned')",
            ),
            anthropic_text("waiting"),
            anthropic_text("parent done"),
        ],
        "look around",
        vec![
            anthropic_tool("toolu_cwd", "import os\nprint(os.getcwd())"),
            anthropic_text("child done"),
        ],
    );
    let sandbox = sandbox(port);
    let git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(&sandbox.cwd)
            .env("HOME", &sandbox.root)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            status.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&status.stderr)
        );
    };
    git(&["init", "-q"]);
    std::fs::write(sandbox.cwd.join("README.md"), "repo\n").unwrap();
    git(&["add", "README.md"]);
    git(&[
        "-c",
        "user.email=t@t",
        "-c",
        "user.name=t",
        "commit",
        "-q",
        "-m",
        "init",
    ]);
    let output = run(&sandbox, &["-p", "--model", MODEL, "delegate"]);
    assert_eq!(stdout(&output), "parent done\n", "{}", stderr(&output));
    let requests = seen.lock().unwrap().clone();
    let child: Vec<&Value> = requests.iter().filter(|r| r["child"] == true).collect();
    assert_eq!(child.len(), 2);
    let cwd = sandbox.cwd.display().to_string();
    let worktree = tool_result_text(&child[1]["body"], "toolu_cwd").unwrap();
    assert_ne!(worktree, cwd, "the child runs in its own worktree");
    assert!(
        worktree.starts_with(&format!("{cwd}/.git/ultron-worktrees/")) && worktree.contains("-scout-"),
        "{worktree}"
    );
    // Its system prompt names the worktree, not the parent's checkout.
    let system = child[0]["body"]["system"].to_string();
    assert!(system.contains(&worktree), "{system}");
    assert!(
        !system.contains(&format!("{cwd}\\n")) && !system.contains(&format!("{cwd}<")),
        "{system}"
    );
}
