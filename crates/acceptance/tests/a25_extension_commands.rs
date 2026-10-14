//! A25: extension commands are listed where TypeScript lists them. Replays crates/session-core/fixtures/
//! extension-commands.json (generate-extension-commands.mjs: what TypeScript's `get_commands` lists for extensions
//! in the profile's and the project's extensions directories and in both settings files, two registering the same
//! name). The Rust build runs the same extensions as processes (crates/extensions/PROTOCOL.md: one directory each
//! with an `extension.json`, the scope decision that removed JavaScript extension loading), so a command's
//! `sourceInfo.path` is its manifest where TypeScript's is the `index.ts` beside it; everything else (load order,
//! `name:N` invocation names, descriptions, `source`, `scope`, `baseDir`, `/mcp` after them) is compared exactly.
//! The session actor answers the same list to the client TUI (`pi.legacy-extension-commands.list`) and runs a
//! command (`LegacyExtensionCommands.run`: `{notifications: []}` once its handler returned, "Unknown extension
//! command: /<name>" otherwise).

mod common;

use common::{HANG, Install, Provider, Reply};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// An extension process registering the commands given as JSON in argv[1]; each `commands/run` is a line of
/// argv[2], with the extension's directory.
const EXTENSION: &str = r#"import json, os, sys
COMMANDS = json.loads(sys.argv[1])
RUNS = sys.argv[2]
def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()
for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    if "id" not in message or method is None:
        continue
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": message["id"], "result": {"protocolVersion": "2026-10-10",
            "extensionInfo": {"name": "commands"}, "capabilities": {"commands": COMMANDS}}})
    elif method == "commands/run":
        with open(RUNS, "a") as out:
            out.write(json.dumps({"ext": os.path.basename(os.getcwd()), **message["params"]}) + "\n")
        send({"jsonrpc": "2.0", "id": message["id"], "result": None})
    elif method == "commands/complete":
        params = message["params"]
        items = None
        if params["name"] == "greet":
            here = os.path.basename(os.getcwd())
            items = [{"value": params["prefix"] + "-" + here, "label": here, "description": "from " + here}]
        send({"jsonrpc": "2.0", "id": message["id"], "result": items})
    elif method == "shutdown":
        send({"jsonrpc": "2.0", "id": message["id"], "result": {}})
        break
    else:
        send({"jsonrpc": "2.0", "id": message["id"], "error": {"code": -32601, "message": "Method not found"}})
"#;

fn fixture() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../session-core/fixtures/extension-commands.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// A fixture path (`<TMP>/agent/...`, `<TMP>/cwd/...`) in this install: the project is `<root>/project`.
fn local(install: &Install, recorded: &str) -> String {
    let root = install.root.path().display().to_string();
    recorded
        .replace("<TMP>/cwd", &install.project().display().to_string())
        .replace("<TMP>", &root)
}

/// The extensions and settings of the fixture, as extension processes in `install`; returns the runs log.
fn lay_out(install: &Install) -> PathBuf {
    let fixture = fixture();
    let runs = install.root.path().join("runs.jsonl");
    let python = std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into());
    for extension in fixture["extensions"].as_array().unwrap() {
        let dir = PathBuf::from(local(install, &format!("<TMP>/{}", extension["dir"].as_str().unwrap())));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ext.py"), EXTENSION).unwrap();
        let name = dir.file_name().unwrap().to_str().unwrap().to_string();
        std::fs::write(
            dir.join("extension.json"),
            json!({"name": name, "command": python,
                "args": ["ext.py", extension["commands"].to_string(), runs.display().to_string()]})
            .to_string(),
        )
        .unwrap();
    }
    let listing = |scope: &str| -> Vec<String> {
        fixture["settings"][scope]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| local(install, e.as_str().unwrap()))
            .collect()
    };
    std::fs::write(
        install.agent().join("settings.json"),
        json!({"extensions": listing("user")}).to_string(),
    )
    .unwrap();
    std::fs::create_dir_all(install.project().join(".pi")).unwrap();
    std::fs::write(
        install.project().join(".pi/settings.json"),
        json!({"extensions": listing("project")}).to_string(),
    )
    .unwrap();
    runs
}

/// The fixture's extension commands as this install lists them: paths here, the manifest beside each `index.ts`.
fn expected(install: &Install) -> Vec<Value> {
    let text = fixture()["commands"]
        .to_string()
        .replace("/index.ts\"", "/extension.json\"");
    let commands: Value = serde_json::from_str(&local(install, &text)).unwrap();
    commands.as_array().unwrap().clone()
}

fn extension_commands(commands: &Value) -> Vec<Value> {
    commands
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["source"] == "extension")
        .cloned()
        .collect()
}

#[test]
fn acceptance_a25_get_commands_lists_extension_process_commands_in_typescript_order_with_their_source() {
    let provider = Provider::start(|_| Reply::Text("unused".into()));
    let install = Install::new(&provider);
    lay_out(&install);
    let mut client = install.rpc(&["--no-session"], &[]);
    let commands = client.get_commands().unwrap()["commands"].clone();
    assert_eq!(extension_commands(&commands), expected(&install));
    // Then `/review`, as before.
    let names: Vec<&str> = commands
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    let mcp = names.iter().position(|n| *n == "mcp").unwrap();
    assert_eq!(names[mcp + 1], "review");
    let (_, code) = client.finish(HANG);
    assert_eq!(code, Some(0));
}

#[test]
fn acceptance_a25_the_session_lists_and_runs_extension_commands_for_the_client_tui() {
    let provider = Provider::start(|_| Reply::Text("unused".into()));
    let install = Install::new(&provider);
    let runs = lay_out(&install);
    let root = install.root.path().to_path_buf();
    let vars: Vec<(String, String)> = vec![
        ("HOME".into(), root.display().to_string()),
        ("ULTRON_CODING_AGENT_DIR".into(), install.agent().display().to_string()),
        ("ULTRON_SERVER_DIR".into(), root.join("server").display().to_string()),
        ("GROQ_API_KEY".into(), "test-groq".into()),
        ("ULTRON_HINDSIGHT_URL".into(), "off".into()),
    ];
    let env = cli::Env {
        cwd: install.project(),
        home: root.clone(),
        var: Arc::new(move |key| vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())),
        docs: config::paths::normalize(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/docs")),
    };
    let sessions = root.join("sessions");
    let repo = session_journal::SessionRepo::new(&sessions, session_journal::SystemClock::shared());
    let created = repo
        .create(&install.project().display().to_string(), None, None)
        .unwrap();
    let open = session_core::core::OpenOptions {
        env,
        flags: session_core::ModelFlags {
            model: Some(common::MODEL.into()),
            ..Default::default()
        },
        session_root: sessions,
        metadata: created.metadata().clone(),
        mode: "print".into(),
        services: cli::compose::Services::default(),
        auto_name: false,
    };
    drop(created);
    let (core, actor, _) = session_core::core::spawn(open, Box::new(|_| {})).unwrap();
    let call = |op| session_core::core::call(&core, op);

    // `pi.legacy-extension-commands.list`: `{name, description?, sourceInfo}` in load order, `/mcp` last.
    let commands = call(session_core::core::Op::Commands).unwrap().unwrap();
    let listed = session_core::extension_commands::from_listing(commands.as_array().unwrap());
    let infos: Vec<Value> = listed
        .iter()
        .map(session_core::extension_commands::command_info)
        .collect();
    let want: Vec<Value> = expected(&install)
        .into_iter()
        .map(|mut c| {
            c.as_object_mut().unwrap().remove("source");
            c
        })
        .collect();
    assert_eq!(infos, want);
    // `listCommands` of the session control: `description` and `sourceInfo` null when absent.
    let control = session_core::extension_commands::session_command_infos(commands.as_array().unwrap());
    let bare = control.iter().find(|c| c["name"] == "bare").unwrap();
    assert_eq!(bare["description"], Value::Null);
    let review = control.iter().find(|c| c["name"] == "review").unwrap();
    assert_eq!(review["sourceInfo"], Value::Null);

    // `run`: the invocation name reaches the extension that registered it, under its own name.
    let ran = call(session_core::core::Op::ExtensionCommand {
        name: "greet:2".into(),
        args: "a b".into(),
    })
    .unwrap();
    assert_eq!(ran, Some(json!({"notifications": []})));
    let ran = call(session_core::core::Op::ExtensionCommand {
        name: "bare".into(),
        args: String::new(),
    })
    .unwrap();
    assert_eq!(ran, Some(json!({"notifications": []})));
    // `commands/complete` of the extension that registered the invocation name, under its own name.
    let completed = call(session_core::core::Op::ExtensionCompletions {
        name: "greet:1".into(),
        prefix: "x".into(),
    })
    .unwrap();
    assert_eq!(
        completed,
        Some(json!([{"value": "x-zeta", "label": "zeta", "description": "from zeta"}]))
    );
    let none = call(session_core::core::Op::ExtensionCompletions {
        name: "bare".into(),
        prefix: String::new(),
    })
    .unwrap();
    assert_eq!(none, Some(Value::Null));
    let unknown = call(session_core::core::Op::ExtensionCommand {
        name: "nope".into(),
        args: String::new(),
    })
    .unwrap_err();
    assert_eq!(unknown, "Unknown extension command: /nope");
    let (done, closed) = std::sync::mpsc::channel();
    core.send(session_core::core::Input::Close(done)).unwrap();
    closed.recv_timeout(HANG).unwrap();
    actor.join().unwrap();
    let runs: Vec<Value> = std::fs::read_to_string(&runs)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    // greet:2 is the profile directory's `greet` (the settings-listed `zeta` loads first and is greet:1).
    assert_eq!(
        runs,
        [
            json!({"ext": "greet", "name": "greet", "args": "a b"}),
            json!({"ext": "greet", "name": "bare", "args": ""})
        ]
    );
}
