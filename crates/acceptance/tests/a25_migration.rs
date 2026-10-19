//! A25 (partial): existing Pi capabilities and data survive migration and rollback.
//! Ported from packages/coding-agent/test/ultron-migration.test.ts ("profile backup and restore" and the argument
//! cases of "runMigrationCommand") through the `ultron migrate` subcommand of the Rust binary, and from
//! packages/coding-agent/test/ultron-native-rpc.test.ts (the event translator, and the parts of the native RPC run
//! the Rust build serves) through `ultron --mode rpc`.
//! The Pi <-> native session migration (`importPiSession` / `exportNativeSessionToPi`: every import/export case and
//! the end-to-end CLI case) runs through `session_core::migration` and `ultron migrate import-pi|export-pi`, and is
//! checked byte for byte against the TypeScript output (`mod migration`).
//! The native RPC run also covers `inspect` (session inspector), `get_commands` (with sources; the full list is
//! replayed against TS in crates/cli/tests/rpc_mode.rs) and `export_html`. Pi extensions are not supported by
//! ultron-rs, so the TS test's `hello.ts` extension command is not listed.

mod common;

use cli::rpc::events::{LaneEvent, RpcEventTranslator};
use common::{Install, Provider, Reply};
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Output;

const SECRET: &str = "sk-test-super-secret-credential";

fn profile_files() -> Vec<(&'static str, String)> {
    vec![
        ("settings.json", json!({"defaultModel": "claude-test"}).to_string()),
        ("models.json", json!({"providers": {}}).to_string()),
        (
            "auth.json",
            json!({"anthropic": {"type": "api_key", "key": SECRET}}).to_string(),
        ),
        ("extensions/demo/index.ts", "export default () => {};\n".into()),
        ("skills/review/SKILL.md", "# Review\n".into()),
        ("prompts/fix.md", "Fix it.\n".into()),
        ("themes/night.json", "{}\n".into()),
        ("mcp-oauth/server.json", json!({"token": SECRET}).to_string()),
    ]
}

const NON_PROFILE_FILES: [&str; 4] = [
    "sessions/--project--/2026-01-01_x.jsonl",
    "experimental/sessions/--project--/y.jsonl",
    "traces/trace.log",
    "npm/node_modules/pkg/index.js",
];

fn write_profile(agent: &Path) {
    for (path, content) in profile_files() {
        let file = agent.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, content).unwrap();
    }
    std::fs::set_permissions(agent.join("auth.json"), std::fs::Permissions::from_mode(0o600)).unwrap();
    for path in NON_PROFILE_FILES {
        let file = agent.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, "session data").unwrap();
    }
}

fn list(dir: &Path, prefix: &str, dirs: bool, out: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir.join(prefix)).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        if entry.file_type().unwrap().is_dir() {
            if dirs {
                out.push(rel.clone());
            }
            list(dir, &rel, dirs, out);
        } else if !dirs {
            out.push(rel);
        }
    }
}

fn list_files(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    list(dir, "", false, &mut out);
    out.sort();
    out
}

fn list_dirs(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    list(dir, "", true, &mut out);
    out
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn sha256(data: &[u8]) -> String {
    // Through the binary's own manifest check is circular; compute with `sha256sum` instead.
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("data");
    std::fs::write(&file, data).unwrap();
    let out = std::process::Command::new("sha256sum").arg(&file).output().unwrap();
    String::from_utf8(out.stdout)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_string()
}

/// `ultron <args>` with a private home and agent dir.
fn ultron(root: &Path, args: &[&str]) -> Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_ultron-acceptance"))
        .env_clear()
        .args(args)
        .current_dir(root)
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", root)
        .env("ULTRON_CODING_AGENT_DIR", root.join("default-agent"))
        .env("ULTRON_SERVER_DIR", root.join("server"))
        .env("NO_COLOR", "1")
        .output()
        .unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// `backupProfile` through `ultron migrate backup`: the backup path and the printed lines.
fn backup(root: &Path, agent: &Path) -> (PathBuf, String) {
    let backups = root.join("backups");
    let output = ultron(
        root,
        &[
            "migrate",
            "backup",
            backups.to_str().unwrap(),
            "--agent-dir",
            agent.to_str().unwrap(),
        ],
    );
    assert!(output.status.success(), "{}", text(&output.stderr));
    let stdout = text(&output.stdout);
    let path = stdout.trim_end().split(" to ").nth(1).unwrap().to_string();
    (PathBuf::from(path), stdout)
}

/// `restoreProfile` through `ultron migrate restore`.
fn restore(root: &Path, backup: &Path, agent: &Path) -> Output {
    ultron(
        root,
        &[
            "migrate",
            "restore",
            backup.to_str().unwrap(),
            "--agent-dir",
            agent.to_str().unwrap(),
        ],
    )
}

/// The problems of a failed verification (`ProfileBackupVerificationError.problems`).
fn problems(output: &Output) -> Vec<String> {
    assert_eq!(output.status.code(), Some(1));
    let stderr = text(&output.stderr);
    assert!(
        stderr.starts_with("Error: Profile backup failed verification:\n"),
        "{stderr}"
    );
    stderr
        .lines()
        .filter_map(|line| line.strip_prefix("  - "))
        .map(str::to_string)
        .collect()
}

#[test]
fn acceptance_a25_backup_is_private_with_a_manifest_and_excludes_sessions() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let agent = root.join("agent");
    write_profile(&agent);
    let (path, stdout) = backup(&root, &agent);

    assert!(
        path.starts_with(root.join("backups"))
            && path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("ultron-profile-")
    );
    assert_eq!(stdout, format!("Backed up 8 profile entries to {}\n", path.display()));
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(path.join("manifest.json")).unwrap()).unwrap();
    let entries = manifest["entries"].as_array().unwrap();
    let mut paths: Vec<&str> = entries.iter().map(|e| e["path"].as_str().unwrap()).collect();
    paths.sort_unstable();
    let mut expected: Vec<&str> = profile_files().iter().map(|(p, _)| *p).collect();
    expected.sort_unstable();
    assert_eq!(paths, expected);
    let files = profile_files();
    for entry in entries {
        assert_eq!(entry["type"], "file");
        let content = &files.iter().find(|(p, _)| *p == entry["path"]).unwrap().1;
        assert_eq!(entry["sha256"].as_str().unwrap(), sha256(content.as_bytes()));
    }
    let auth = entries.iter().find(|e| e["path"] == "auth.json").unwrap();
    assert_eq!(auth["mode"], 0o600);

    assert_eq!(mode(&path), 0o700);
    for dir in list_dirs(&path) {
        assert_eq!(mode(&path.join(&dir)), 0o700, "{dir}");
    }
    let files = list_files(&path);
    assert!(files.contains(&"manifest.json".to_string()));
    for file in &files {
        assert_eq!(mode(&path.join(file)), 0o600, "{file}");
    }
    assert!(!files.iter().any(|f| f.contains("sessions") || f.contains("traces")));
    assert!(!stdout.contains(SECRET));
}

#[test]
fn acceptance_a25_restore_rehearsal_is_byte_identical_and_leaves_sessions_untouched() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let agent = root.join("agent");
    write_profile(&agent);
    let (path, _) = backup(&root, &agent);
    let snapshot: Vec<(String, Vec<u8>)> = list_files(&agent)
        .into_iter()
        .map(|f| {
            let data = std::fs::read(agent.join(&f)).unwrap();
            (f, data)
        })
        .collect();
    let original = |file: &str| &snapshot.iter().find(|(f, _)| f == file).unwrap().1;

    // Damage the profile.
    std::fs::write(agent.join("settings.json"), "{broken").unwrap();
    std::fs::remove_file(agent.join("auth.json")).unwrap();
    std::fs::remove_dir_all(agent.join("extensions")).unwrap();
    std::fs::write(
        agent.join("sessions/--project--/2026-01-01_x.jsonl"),
        "newer session data",
    )
    .unwrap();

    let output = restore(&root, &path, &agent);
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert_eq!(
        text(&output.stdout),
        format!("Restored 8 profile entries to {}\n", agent.display())
    );
    for (file, _) in profile_files() {
        assert_eq!(&std::fs::read(agent.join(file)).unwrap(), original(file), "{file}");
    }
    assert_eq!(mode(&agent.join("auth.json")), 0o600);
    assert_eq!(
        std::fs::read_to_string(agent.join("sessions/--project--/2026-01-01_x.jsonl")).unwrap(),
        "newer session data"
    );

    // Restoring into a fresh directory also reproduces the full profile.
    let fresh = root.join("fresh-agent");
    assert!(restore(&root, &path, &fresh).status.success());
    let mut expected: Vec<String> = profile_files().iter().map(|(p, _)| p.to_string()).collect();
    expected.sort();
    assert_eq!(list_files(&fresh), expected);
    for (file, _) in profile_files() {
        assert_eq!(&std::fs::read(fresh.join(file)).unwrap(), original(file), "{file}");
    }
}

#[test]
fn acceptance_a25_tampered_backup_is_refused_without_touching_the_profile() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let agent = root.join("agent");
    write_profile(&agent);
    let (path, _) = backup(&root, &agent);
    std::fs::write(agent.join("settings.json"), "current").unwrap();

    std::fs::write(
        path.join("files/models.json"),
        json!({"providers": {"evil": {}}}).to_string(),
    )
    .unwrap();
    let output = restore(&root, &path, &agent);
    assert_eq!(problems(&output), ["hash mismatch: models.json"]);
    assert!(!text(&output.stderr).contains(SECRET));
    assert_eq!(std::fs::read_to_string(agent.join("settings.json")).unwrap(), "current");
    assert_eq!(
        std::fs::read_to_string(agent.join("models.json")).unwrap(),
        json!({"providers": {}}).to_string()
    );
}

#[test]
fn acceptance_a25_backups_with_unlisted_files_or_unsafe_paths_are_refused() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let agent = root.join("agent");
    write_profile(&agent);
    let (first, _) = backup(&root, &agent);
    std::fs::write(first.join("files/extensions/injected.ts"), "evil").unwrap();
    assert_eq!(
        problems(&restore(&root, &first, &agent)),
        ["not in manifest: extensions/injected.ts"]
    );

    let (second, _) = backup(&root, &agent);
    assert_ne!(second, first);
    let manifest_path = second.join("manifest.json");
    let mut manifest: Value = serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
    manifest["entries"][0]["path"] = "../escape.json".into();
    std::fs::write(&manifest_path, manifest.to_string()).unwrap();
    let refused = problems(&restore(&root, &second, &agent));
    assert!(
        refused.contains(&"unsafe path in manifest: \"../escape.json\"".to_string()),
        "{refused:?}"
    );
    assert!(!root.join("escape.json").exists() && !agent.join("../escape.json").exists());
}

#[test]
fn acceptance_a25_migrate_ignores_other_commands_prints_usage_and_rejects_bad_arguments() {
    let root = tempfile::tempdir().unwrap();
    let env = cli::Env {
        cwd: root.path().to_path_buf(),
        home: root.path().to_path_buf(),
        var: std::sync::Arc::new(|_| None),
        docs: root.path().to_path_buf(),
    };
    let run = |args: &[&str]| {
        let argv: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = cli::migrate::run(&argv, &env, &mut out, &mut err);
        (code, text(&out), text(&err))
    };
    // Not a migrate command: not handled, nothing printed.
    assert_eq!(run(&[]), (None, String::new(), String::new()));
    assert_eq!(run(&["install", "migrate"]), (None, String::new(), String::new()));

    let (code, out, _) = run(&["migrate"]);
    assert_eq!(code, Some(1));
    assert!(out.contains("import-pi"));
    let (code, out, _) = run(&["migrate", "--help"]);
    assert_eq!(code, Some(0));
    assert!(out.contains("import-pi"));

    for args in [
        &["migrate", "bogus"][..],
        &["migrate", "import-pi"],
        &["migrate", "export-pi", "only-one.jsonl"],
        &["migrate", "restore"],
        &["migrate", "backup", "a", "b"],
        &["migrate", "import-pi", "x.jsonl", "--nope", "1"],
        &["migrate", "import-pi", "x.jsonl", "--sessions-root"],
    ] {
        let (code, _, err) = run(args);
        assert_eq!(code, Some(1), "{args:?}");
        assert!(err.starts_with("Error: "), "{args:?}: {err}");
    }
    // Through the binary too: the exit status is the command's.
    let output = ultron(root.path(), &["migrate", "bogus"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(text(&output.stderr).starts_with("Error: Unknown migrate command: bogus"));
}

/// The TS test's events, as lane-watch JSON.
#[test]
fn acceptance_a25_rpc_translator_reconstructs_turn_and_agent_boundaries() {
    let user = json!({"role": "user", "content": "hi", "timestamp": 1});
    let assistant = json!({
        "role": "assistant", "content": [{"type": "text", "text": "ok"}], "api": "openai-completions",
        "provider": "mock", "model": "mock",
        "usage": {"input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 2,
            "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}},
        "stopReason": "stop", "timestamp": 2,
    });
    let events = [
        json!({"type": "run_start", "runId": "r", "startedAt": 0}),
        json!({"type": "message_start", "runId": "r", "message": user}),
        json!({"type": "message_end", "runId": "r", "message": user}),
        json!({"type": "message_start", "runId": "r", "message": assistant}),
        json!({"type": "message_update", "runId": "r", "message": assistant,
            "frame": {"type": "text_delta", "contentIndex": 0, "delta": "ok"}}),
        json!({"type": "message_end", "runId": "r", "message": assistant}),
        json!({"type": "run_end", "runId": "r", "fromTipId": null, "tipId": "t", "endedAt": 1, "status": "completed"}),
    ];
    let mut translator = RpcEventTranslator::default();
    let types: Vec<String> = events
        .iter()
        .filter_map(LaneEvent::from_watch)
        .flat_map(|event| translator.translate(event))
        .map(|line| line["type"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        types,
        [
            "agent_start",
            "message_start",
            "message_end",
            "turn_start",
            "message_start",
            "message_update",
            "message_end",
            "turn_end",
            "agent_end",
            "agent_settled",
        ]
    );
}

#[test]
fn acceptance_a25_native_rpc_runs_prompts_bash_and_session_commands_ported_part() {
    let provider = Provider::start(|_| Reply::Text("NATIVE_RPC_OK".into()));
    let install = Install::new(&provider);
    // A prompt template and a skill in the profile, which `get_commands` lists with their sources.
    std::fs::create_dir_all(install.agent().join("prompts")).unwrap();
    std::fs::write(
        install.agent().join("prompts/fix.md"),
        "---\ndescription: Fix it\n---\nFix it.\n",
    )
    .unwrap();
    std::fs::create_dir_all(install.agent().join("skills/demo")).unwrap();
    std::fs::write(
        install.agent().join("skills/demo/SKILL.md"),
        "---\nname: demo\ndescription: A demo skill\n---\nDo the demo.\n",
    )
    .unwrap();
    let mut client = install.rpc(&["--no-session"], &[]);

    let state = client.get_state().unwrap();
    assert_eq!(state["model"]["provider"], "groq");
    assert_eq!(state["model"]["id"], "llama-3.3-70b-versatile");
    assert_eq!(state["isStreaming"], false);

    let events = client.prompt_and_wait("say the marker", common::HANG).unwrap();
    let types: Vec<&str> = events.iter().filter_map(|e| e["type"].as_str()).collect();
    for want in [
        "agent_start",
        "turn_start",
        "message_end",
        "turn_end",
        "agent_end",
        "agent_settled",
    ] {
        assert!(types.contains(&want), "{want} in {types:?}");
    }
    assert_eq!(
        client.get_last_assistant_text().unwrap().as_deref(),
        Some("NATIVE_RPC_OK")
    );

    // The read-only inspector: optional controls are reported, all off by default; a mutating request is refused.
    let status = client
        .send(json!({"type": "inspect", "request": "agents.status"}))
        .unwrap();
    assert_eq!(status["success"], true, "{status}");
    let controls = status["data"]["controls"].as_object().unwrap();
    assert!(
        !controls.is_empty() && controls.values().all(|enabled| enabled == false),
        "{status}"
    );
    let refused = client
        .send(json!({"type": "inspect", "request": "agents.spawn"}))
        .unwrap();
    assert_eq!(refused["success"], false);
    assert_eq!(refused["error"], "Not an inspection request: agents.spawn");

    // Commands: `/mcp`, `/review`, the profile's prompt template and skill, each with its source.
    let commands = client.get_commands().unwrap()["commands"].clone();
    let commands = commands.as_array().unwrap();
    let agent = install.agent().display().to_string();
    assert_eq!(
        commands[..4],
        [
            json!({"name": "mcp", "description": "MCP servers: status, connect or disconnect, sign in, enable or disable",
                "source": "extension", "sourceInfo": {"path": "<inline:mcp>", "source": "inline", "scope": "temporary", "origin": "top-level"}}),
            json!({"name": "review", "description": "Review code changes: specialist reviewer frames, then a verifier per finding", "source": "prompt"}),
            json!({"name": "fix", "description": "Fix it", "source": "prompt", "sourceInfo": {"path": format!("{agent}/prompts/fix.md"),
                "source": "auto", "scope": "user", "origin": "top-level", "baseDir": agent}}),
            json!({"name": "skill:demo", "description": "A demo skill", "source": "skill", "sourceInfo": {"path": format!("{agent}/skills/demo/SKILL.md"),
                "source": "auto", "scope": "user", "origin": "top-level", "baseDir": agent}}),
        ]
    );
    // Then the bundled skills, which the worker loads by path (cli/tests/rpc_mode.rs replays them against TS).
    assert!(commands.len() > 4);
    assert!(
        commands[4..]
            .iter()
            .all(|c| c["source"] == "skill" && c["sourceInfo"]["scope"] == "temporary")
    );

    let bash = client.bash("echo native-bash").unwrap();
    assert_eq!(bash["output"], "native-bash\n");
    assert_eq!(bash["exitCode"], 0);
    assert_eq!(bash["cancelled"], false);

    client.set_session_name("Native RPC").unwrap();
    assert_eq!(client.get_state().unwrap()["sessionName"], "Native RPC");

    let roles: Vec<String> = client
        .get_messages()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(roles, ["user", "assistant", "bashExecution"]);

    // The HTML export of the running session: its active branch, as Pi's exporter embeds it.
    let target = install.project().join("session.html");
    let exported = client
        .send(json!({"type": "export_html", "outputPath": target.display().to_string()}))
        .unwrap();
    assert_eq!(exported["success"], true, "{exported}");
    assert_eq!(exported["data"]["path"], target.display().to_string());
    let html = std::fs::read_to_string(&target).unwrap();
    let at = html.find("id=\"session-data\"").unwrap();
    let encoded = &html[at..];
    let encoded = &encoded[encoded.find('>').unwrap() + 1..];
    let encoded = &encoded[..encoded.find('<').unwrap()];
    let data: Value = serde_json::from_slice(&base64_decode(encoded.trim())).unwrap();
    let roles: Vec<&str> = data["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["message"]["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["user", "assistant", "bashExecution"]);
    let (_, code) = client.finish(common::HANG);
    assert_eq!(code, Some(0));
}

// ---------------------------------------------------------------------------------------------------------------
// Pi <-> native session migration (`importPiSession` / `exportNativeSessionToPi`), through
// `session_core::migration` and `ultron migrate import-pi|export-pi`. The Pi inputs are the TS test's fixtures,
// recorded with the native file and the exported Pi file the TypeScript implementation writes for each
// (crates/session-core/fixtures/migration/generate.mjs, cwd "/project").

mod migration {
    use super::{text, ultron};
    use serde_json::{Value, json};
    use session_core::migration::{
        ImportError, PI_ENTRY_CUSTOM_TYPE_PREFIX, PI_LEAF_MARKER_CUSTOM_TYPE, PI_OMITTED_MESSAGE_CUSTOM_TYPE,
        export_native_session_to_pi, import_pi_session, import_pi_session_with, native_session_id_for_pi_session,
    };
    use session_file::{Leaf, SessionManager, SystemEnv};
    use session_journal::session::{BRANCH_TIP, ENTRY_LABEL, LANE_CONFIG};
    use session_journal::{Session, SessionRepo, SystemClock, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    const PI_SESSION_ID: &str = "0192a0b0-0000-7000-8000-000000000001";

    fn fixture() -> Value {
        serde_json::from_str(include_str!("../../session-core/fixtures/migration/migration.json")).unwrap()
    }

    fn case(name: &str) -> Value {
        fixture()["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == name)
            .unwrap()
            .clone()
    }

    /// The fixture's Pi file `name` written under `root`.
    fn write_pi(root: &Path, name: &str) -> PathBuf {
        let path = root.join("pi").join(format!("{name}.jsonl"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, case(name)["pi"].as_str().unwrap()).unwrap();
        path
    }

    fn open_pi(path: &Path) -> SessionManager {
        let entries = session_file::load_entries_from_file(&path.display().to_string()).unwrap();
        SessionManager::in_memory(Box::new(SystemEnv), "/project", None, Some(entries)).unwrap()
    }

    fn pi_entries(pi: &SessionManager) -> Vec<Value> {
        pi.entries().into_iter().cloned().collect()
    }

    /// Pi's `buildSessionContext(entries, leafId)` as JSON.
    fn pi_context_at(pi: &SessionManager, leaf: &str) -> Value {
        let context = session_file::build_session_context(&pi_entries(pi), Leaf::Id(&json!(leaf)));
        json!({"messages": context.messages, "model": context.model, "thinkingLevel": context.thinking_level})
    }

    fn pi_context(pi: &SessionManager) -> Value {
        let context = pi.build_session_context();
        json!({"messages": context.messages, "model": context.model, "thinkingLevel": context.thinking_level})
    }

    /// Pi's `getTree()` as nested JSON.
    fn pi_tree(pi: &SessionManager) -> Value {
        let tree = pi.tree();
        fn node(tree: &session_file::SessionTree<'_>, at: usize) -> Value {
            let node = &tree.nodes[at];
            let children: Vec<Value> = node.children.iter().map(|c| node_at(tree, *c)).collect();
            let mut out = json!({"entry": node.entry, "children": children});
            if let Some(label) = node.label {
                out["label"] = label.clone();
            }
            if let Some(stamp) = node.label_timestamp {
                out["labelTimestamp"] = stamp.clone();
            }
            out
        }
        fn node_at(tree: &session_file::SessionTree<'_>, at: usize) -> Value {
            node(tree, at)
        }
        Value::Array(tree.roots.iter().map(|r| node_at(&tree, *r)).collect())
    }

    fn labels_of(pi: &SessionManager) -> Vec<(String, Option<Value>)> {
        pi_entries(pi)
            .iter()
            .map(|e| {
                let id = e["id"].as_str().unwrap().to_string();
                let label = pi.label(&id).cloned();
                (id, label)
            })
            .collect()
    }

    fn messages_of(entries: &[Value]) -> Vec<Value> {
        entries
            .iter()
            .filter(|e| e["type"] == "message")
            .map(|e| e["message"].clone())
            .collect()
    }

    fn open_native(sessions_root: &Path, id: &str) -> (Session, usize) {
        let repo = SessionRepo::new(sessions_root, SystemClock::shared());
        let all = repo.list(None);
        let matching: Vec<_> = all.iter().filter(|m| m.id == id).collect();
        assert_eq!(matching.len(), 1);
        (repo.open(matching[0]).unwrap(), all.len())
    }

    fn native_context_at(session: &Session, leaf: &str) -> Value {
        Value::Array(session_journal::context::build_session_context(
            &session.state().branch_path(Some(leaf)).unwrap(),
        ))
    }

    fn by_id<'a>(session: &'a Session, id: &str) -> &'a Value {
        session
            .state()
            .entry(id)
            .unwrap_or_else(|| panic!("no native entry {id}"))
    }

    fn native_labels(session: &Session) -> Vec<(String, Value)> {
        let mut labels: Vec<(String, Value)> = session
            .state()
            .scan_values(ENTRY_LABEL, "")
            .into_iter()
            .map(|v| (v.key.clone(), v.value.clone()))
            .collect();
        labels.sort_by(|a, b| a.0.cmp(&b.0));
        labels
    }

    fn user(text: &str, at: i64) -> Value {
        json!({"role": "user", "content": [{"type": "text", "text": text}], "timestamp": 1_760_000_000_000i64 + at})
    }

    #[test]
    fn acceptance_a25_import_and_export_write_the_same_bytes_as_typescript() {
        for case in fixture()["cases"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let root = tempfile::tempdir().unwrap();
            let pi = root.path().join("pi.jsonl");
            std::fs::write(&pi, case["pi"].as_str().unwrap()).unwrap();
            let sessions = root.path().join("native");
            let result = import_pi_session(&pi, &sessions).unwrap();
            let mut recorded = result.to_json();
            recorded["path"] = json!(result.path.strip_prefix(&sessions).unwrap().display().to_string());
            assert_eq!(recorded, case["result"], "{name}: result");
            assert_eq!(
                std::fs::read_to_string(&result.path).unwrap(),
                case["native"].as_str().unwrap(),
                "{name}: native file"
            );
            let out = root.path().join("out.jsonl");
            let exported = export_native_session_to_pi(&result.path, &out).unwrap();
            assert_eq!(exported.session_id, case["exported"]["sessionId"].as_str().unwrap());
            assert_eq!(exported.entries as u64, case["exported"]["entries"].as_u64().unwrap());
            assert_eq!(
                std::fs::read_to_string(&out).unwrap(),
                case["exported"]["content"].as_str().unwrap(),
                "{name}: exported Pi file"
            );
        }
    }

    #[test]
    fn acceptance_a25_imports_every_entry_of_the_linear_fixture_with_nothing_skipped_and_resumes_at_the_pi_leaf() {
        let root = tempfile::tempdir().unwrap();
        let pi_path = write_pi(root.path(), "linear");
        let sessions = root.path().join("native");
        let before = std::fs::read(&pi_path).unwrap();
        let result = import_pi_session(&pi_path, &sessions).unwrap();
        assert_eq!(std::fs::read(&pi_path).unwrap(), before);
        assert_eq!(result.session_id, native_session_id_for_pi_session(PI_SESSION_ID));
        let id: Vec<char> = result.session_id.chars().collect();
        assert_eq!(
            (id.len(), id[14], id[8], id[13], id[18], id[23]),
            (36, '8', '-', '-', '-', '-')
        );
        assert!("89ab".contains(id[19]));
        assert_eq!(result.imported, 16);
        assert!(result.skipped.is_empty() && result.preserved.is_empty() && result.unapplied_context_edits.is_empty());

        let (native, _) = open_native(&sessions, &result.session_id);
        assert_eq!(native.metadata().cwd, "/project");
        assert_eq!(native.metadata().path, result.path);
        assert_eq!(native.metadata().created_at, 1_760_000_000_000);
        let first = std::fs::read_to_string(&result.path).unwrap();
        assert!(first.lines().next().unwrap().contains("\"kind\":\"header\""));

        let pi = open_pi(&pi_path);
        let branch: Vec<Value> = pi.branch_entries(None).into_iter().cloned().collect();
        let entries = native.branch_entries("main").unwrap();
        assert_eq!(messages_of(&entries), messages_of(&branch));
        let field = |list: &[Value], key: &str| list.iter().map(|e| e[key].clone()).collect::<Vec<_>>();
        assert_eq!(field(&entries, "id"), field(&branch, "id"));
        assert_eq!(field(&entries, "parentId"), field(&branch, "parentId"));
        let stamps: Vec<Value> = branch
            .iter()
            .map(|e| json!(session_file::js::parse_date(e["timestamp"].as_str().unwrap()).unwrap()))
            .collect();
        assert_eq!(field(&entries, "timestamp"), stamps);
        // The abandoned branch is imported too, with its parents.
        assert_eq!(native.state().entries().len(), 16);
        assert_eq!(by_id(&native, "x2")["parentId"], "x1");

        // Resumes at the Pi leaf with Pi's resolved labels, model and thinking level.
        assert_eq!(pi.leaf_id(), Some(&json!("m8")));
        assert_eq!(native.branch_tip("main"), Some(Some("m8".into())));
        assert_eq!(native_labels(&native), [("m4".to_string(), json!("checkpoint"))]);
        assert_eq!(
            native.value(LANE_CONFIG, "main"),
            Some(
                &json!({"model": {"provider": "anthropic", "modelId": "claude-test"}, "thinkingLevel": "high", "activeToolNames": []})
            )
        );
        let e1 = by_id(&native, "e1");
        assert_eq!(
            (&e1["type"], &e1["customType"], &e1["data"]),
            (
                &json!("custom"),
                &json!(format!("{PI_ENTRY_CUSTOM_TYPE_PREFIX}model_change")),
                &json!({"provider": "anthropic", "modelId": "claude-test"})
            )
        );
        let summary = entries.iter().find(|e| e["type"] == "branch_summary").unwrap();
        assert_eq!(
            (
                &summary["fromId"],
                &summary["summary"],
                &summary["fromHook"],
                &summary["parentId"]
            ),
            (&json!("x2"), &json!("Approach X failed."), &json!(false), &json!("m5"))
        );
        // The native compaction keeps exactly the messages Pi would have kept in context.
        let compaction = entries.iter().find(|e| e["type"] == "compaction").unwrap();
        assert_eq!(compaction["summary"], "Listed files, X failed, Y chosen.");
        assert_eq!(compaction["tokensBefore"], 1234);
        let pi_messages = pi.build_session_context().messages;
        assert_eq!(compaction["retainedTail"], json!(pi_messages[1..4]));
        let roles: Vec<&Value> = compaction["retainedTail"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| &m["role"])
            .collect();
        assert_eq!(roles, [&json!("user"), &json!("branchSummary"), &json!("assistant")]);
        assert_eq!(native.name().as_deref(), Some("Imported work"));
    }

    #[test]
    fn acceptance_a25_imports_the_whole_tree_with_labels_model_history_usage_context_edits_and_unknown_types() {
        let root = tempfile::tempdir().unwrap();
        let pi_path = write_pi(root.path(), "tree");
        let sessions = root.path().join("native");
        let before = std::fs::read(&pi_path).unwrap();
        let result = import_pi_session(&pi_path, &sessions).unwrap();
        assert_eq!(std::fs::read(&pi_path).unwrap(), before);
        let pi = open_pi(&pi_path);
        let pi_all = pi_entries(&pi);
        assert_eq!(result.imported, pi_all.len());
        assert!(result.skipped.is_empty() && result.unapplied_context_edits.is_empty());
        assert_eq!(result.preserved, [("future_thing".to_string(), 1)]);

        let (native, _) = open_native(&sessions, &result.session_id);
        let triple = |e: &Value| (e["id"].clone(), e["parentId"].clone());
        let natives: Vec<_> = native.state().entries().iter().map(triple).collect();
        let pis: Vec<_> = pi_all.iter().map(triple).collect();
        assert_eq!(natives, pis);
        let stamps: Vec<Value> = native
            .state()
            .entries()
            .iter()
            .map(|e| e["timestamp"].clone())
            .collect();
        let pi_stamps: Vec<Value> = pi_all
            .iter()
            .map(|e| json!(session_file::js::parse_date(e["timestamp"].as_str().unwrap()).unwrap()))
            .collect();
        assert_eq!(stamps, pi_stamps);
        assert_eq!(pi.leaf_id(), Some(&json!("mc")));
        assert_eq!(native.branch_tip("main"), Some(Some("mc".into())));
        // Labels: Pi's resolved labels on both branches (a cleared label stays cleared).
        assert_eq!(
            native_labels(&native),
            [
                ("m4".to_string(), json!("checkpoint")),
                ("x1".to_string(), json!("abandoned-x"))
            ]
        );
        assert_eq!(native.label("m2"), None);
        let at_leaf = pi_context_at(&pi, "mc");
        assert_eq!(
            at_leaf["model"],
            json!({"provider": "anthropic", "modelId": "claude-next"})
        );
        assert_eq!(at_leaf["thinkingLevel"], "low");
        assert_eq!(
            native.value(LANE_CONFIG, "main"),
            Some(
                &json!({"model": {"provider": "anthropic", "modelId": "claude-next"}, "thinkingLevel": "low", "activeToolNames": []})
            )
        );
        let custom = |id: &str, kind: &str, data: Value| {
            let entry = by_id(&native, id);
            assert_eq!(entry["type"], "custom", "{id}");
            assert_eq!(entry["customType"], kind, "{id}");
            for (key, value) in data.as_object().unwrap() {
                assert_eq!(&entry["data"][key], value, "{id}.{key}");
            }
        };
        custom(
            "xm",
            "pi-session:model_change",
            json!({"provider": "openai", "modelId": "gpt-test"}),
        );
        custom(
            "t1",
            "pi-session:thinking_level_change",
            json!({"thinkingLevel": "low"}),
        );
        custom(
            "la",
            "pi-session:label",
            json!({"targetId": "m4", "label": "checkpoint"}),
        );
        custom("lc", "pi-session:label", json!({"targetId": "m2"}));
        custom("s1", "pi-session:session_info", json!({"name": "Imported work"}));
        custom(
            "u1",
            "pi-session:usage",
            json!({"kind": "cache_warm", "provider": "anthropic", "model": "claude-test", "note": "warm"}),
        );
        custom("ce1", "pi-session:context_edit", json!({"targetId": "m6"}));
        custom("f1", "pi-session:future_thing", json!({"payload": {"answer": 42}}));
        custom("cx", "ext-state", json!({}));
        assert_eq!(by_id(&native, "cx")["data"], json!({"n": 1}));
        let cm = by_id(&native, "cm");
        assert_eq!(
            (&cm["type"], &cm["message"]["role"], &cm["message"]["customType"]),
            (&json!("message"), &json!("custom"), &json!("note"))
        );
        // Context edits are baked into their targets: m6 replaced, m7 omitted, x2 replaced.
        assert_eq!(
            by_id(&native, "m6")["message"]["content"],
            json!([{"type": "text", "text": "Using approach Y (edited)."}])
        );
        let m7 = by_id(&native, "m7");
        assert_eq!(
            (&m7["type"], &m7["customType"], &m7["data"]["message"]["role"]),
            (&json!("custom"), &json!(PI_OMITTED_MESSAGE_CUSTOM_TYPE), &json!("user"))
        );
        assert!(!by_id(&native, "x2").to_string().contains("draft"));
        // The effective model context is Pi's at the main leaf and at the side branch's leaf.
        for leaf in ["mc", "x4", "m8", "m5"] {
            assert_eq!(
                native_context_at(&native, leaf),
                pi_context_at(&pi, leaf)["messages"],
                "{leaf}"
            );
        }
        assert!(
            pi_context_at(&pi, "mc")["messages"]
                .to_string()
                .contains("Using approach Y (edited).")
        );
        assert!(!pi_context_at(&pi, "mc")["messages"].to_string().contains("noise"));
        assert!(
            pi_context_at(&pi, "x4")["messages"]
                .to_string()
                .contains("X step two (redacted)")
        );
        // The abandoned branch is navigable: its path ends at the side leaf.
        let side: Vec<Value> = native
            .state()
            .branch_path(Some("x4"))
            .unwrap()
            .iter()
            .map(|e| e["id"].clone())
            .collect();
        let pi_side: Vec<Value> = pi.branch_entries(Some("x4")).iter().map(|e| e["id"].clone()).collect();
        assert_eq!(side, pi_side);
    }

    #[test]
    fn acceptance_a25_applies_a_context_edit_that_only_some_branches_carry_on_those_branches_only() {
        let root = tempfile::tempdir().unwrap();
        let pi_path = write_pi(root.path(), "edit");
        let sessions = root.path().join("native");
        let result = import_pi_session(&pi_path, &sessions).unwrap();
        assert!(result.unapplied_context_edits.is_empty());
        assert_eq!((result.imported, result.branch_copies), (4, 1));
        let pi = open_pi(&pi_path);
        {
            let (native, _) = open_native(&sessions, &result.session_id);
            for leaf in ["b", "e"] {
                assert_eq!(
                    native_context_at(&native, leaf),
                    pi_context_at(&pi, leaf)["messages"],
                    "{leaf}"
                );
            }
            assert_eq!(by_id(&native, "a")["message"], user("shared", 1));
            let copy = by_id(&native, "a~e");
            assert_eq!(
                (&copy["parentId"], &copy["message"]["content"]),
                (&Value::Null, &json!("edited"))
            );
            assert_eq!(by_id(&native, "c")["parentId"], "a~e");
            assert_eq!(native.branch_tip("main"), Some(Some("e".into())));
            assert!(native_context_at(&native, "e").to_string().contains("edited"));
        }
        // Pi still honors the edit after a round trip.
        let out = root.path().join("edit-out.jsonl");
        export_native_session_to_pi(&result.path, &out).unwrap();
        let restored = open_pi(&out);
        assert_eq!(pi_context(&restored), pi_context(&pi));
        assert!(pi_context(&restored)["messages"].to_string().contains("edited"));
    }

    #[test]
    fn acceptance_a25_matches_pi_context_at_every_leaf_with_nested_competing_compacted_and_omitting_edits() {
        let root = tempfile::tempdir().unwrap();
        let pi_path = write_pi(root.path(), "edits");
        let sessions = root.path().join("native");
        let result = import_pi_session(&pi_path, &sessions).unwrap();
        assert_eq!(result.unapplied_context_edits, ["ex"]);
        assert!(result.branch_copies > 0);
        let pi = open_pi(&pi_path);
        let ids: Vec<String> = pi_entries(&pi)
            .iter()
            .map(|e| e["id"].as_str().unwrap().to_string())
            .collect();
        let parents: Vec<Value> = pi_entries(&pi).iter().map(|e| e["parentId"].clone()).collect();
        let leaves: Vec<String> = ids.iter().filter(|id| !parents.contains(&json!(id))).cloned().collect();
        assert_eq!(leaves, ["a4", "a5", "a6", "b2", "ex"]);
        assert_eq!(result.imported, ids.len());
        {
            let (native, _) = open_native(&sessions, &result.session_id);
            for leaf in &leaves {
                assert_eq!(
                    native_context_at(&native, leaf),
                    pi_context_at(&pi, leaf)["messages"],
                    "{leaf}"
                );
            }
            let native_parents: Vec<Value> = native.state().entries().iter().map(|e| e["parentId"].clone()).collect();
            let mut native_leaves: Vec<String> = native
                .state()
                .entries()
                .iter()
                .map(|e| e["id"].as_str().unwrap().to_string())
                .filter(|id| !native_parents.contains(&json!(id)))
                .collect();
            native_leaves.sort();
            assert_eq!(native_leaves, leaves);
            let shown = |id: &str| pi_context_at(&pi, id)["messages"].to_string();
            assert!(shown("c1").contains("t text"));
            assert!(shown("b2").contains("t edited on B"));
            assert!(shown("a2").contains("t edited on A"));
            assert!(shown("a4").contains("t edited again on A1"));
            assert!(!shown("a4").contains("n text"));
            assert!(shown("a5").contains("a1 edited after compaction"));
            assert!(!shown("a6").contains("a1 edited after compaction"));
        }
        // Export drops the copies: the Pi file has exactly the original entries and the same context everywhere.
        let out = root.path().join("edits-out.jsonl");
        export_native_session_to_pi(&result.path, &out).unwrap();
        let restored = open_pi(&out);
        let pairs = |m: &SessionManager| -> Vec<(Value, Value)> {
            pi_entries(m)
                .iter()
                .map(|e| (e["id"].clone(), e["parentId"].clone()))
                .collect()
        };
        assert_eq!(pairs(&restored), pairs(&pi));
        for id in &ids {
            assert_eq!(pi_context_at(&restored, id), pi_context_at(&pi, id), "{id}");
        }
    }

    #[test]
    fn acceptance_a25_imports_pi_reported_usage_into_the_usage_ledger_as_a_marked_historical_root() {
        let root = tempfile::tempdir().unwrap();
        let pi_path = write_pi(root.path(), "usage");
        let sessions = root.path().join("native");
        let result = import_pi_session(&pi_path, &sessions).unwrap();
        let usage_root = format!("import:pi:{PI_SESSION_ID}");
        assert_eq!(
            (result.usage_root.as_str(), result.usage_entries, result.usage_cost),
            (usage_root.as_str(), 3, 0.875)
        );
        // Pi's own session totals, as its stats compute them.
        let (mut input, mut output, mut cost) = (0.0, 0.0, 0.0);
        for entry in pi_entries(&open_pi(&pi_path)) {
            let reported = match entry["type"].as_str() {
                Some("usage" | "compaction") => &entry["usage"],
                Some("message") if entry["message"]["role"] == "assistant" => &entry["message"]["usage"],
                _ => continue,
            };
            input += reported["input"].as_f64().unwrap();
            output += reported["output"].as_f64().unwrap();
            cost += reported["cost"]["total"].as_f64().unwrap();
        }
        let (native, _) = open_native(&sessions, &result.session_id);
        let stored = native.value("ultron.usage", "root").unwrap().clone();
        let ledger = delegation::usage::UsageLedger::new(
            Box::new(delegation::task_store::MemoryStore {
                value: Some(stored.clone()),
            }),
            Default::default(),
        )
        .unwrap();
        let status = ledger.status(None).unwrap();
        let session = &status["session"];
        for (key, value) in [
            ("calls", json!(3)),
            ("modelCalls", json!(3)),
            ("inputTokens", json!(input)),
            ("outputTokens", json!(output)),
            ("totalTokens", json!(100 + 20 + 7 + 50 + 5 + 9)),
            ("cost", json!(cost)),
            ("unknownCalls", json!(0)),
        ] {
            assert_eq!(session["usage"][key].as_f64(), value.as_f64(), "{key}: {status}");
        }
        assert_eq!(session["spentUsd"].as_f64(), Some(cost));
        assert_eq!(
            stored["roots"][&usage_root]["imported"],
            json!({"source": "pi", "entries": 3})
        );
        // The imported root never admits new work, and reconciliation accounts it without discrepancies.
        let refused = ledger
            .reserve(&delegation::usage::ReserveRequest {
                kind: "task".into(),
                root_id: Some(usage_root.clone()),
                ..Default::default()
            })
            .unwrap_err();
        assert!(refused.contains("imported history root"), "{refused}");
        let reconciliation = delegation::reconcile::reconcile_records(&delegation::reconcile::ReconcileInput {
            tasks: None,
            usage: Some(&stored),
            progress: None,
            schedules: None,
            root_id: None,
        })
        .unwrap();
        assert_eq!(reconciliation["discrepancies"], json!([]));
        assert_eq!(reconciliation["imported"]["modelCalls"], 3);
        assert_eq!(reconciliation["imported"]["cost"].as_f64(), Some(cost));
    }

    #[test]
    fn acceptance_a25_refuses_to_re_import_the_same_pi_session() {
        let root = tempfile::tempdir().unwrap();
        let pi_path = write_pi(root.path(), "linear");
        let sessions = root.path().join("native");
        let first = import_pi_session(&pi_path, &sessions).unwrap();
        for _ in 0..2 {
            match import_pi_session(&pi_path, &sessions) {
                Err(ImportError::AlreadyImported { session_id, path, .. }) => {
                    assert_eq!((session_id, path), (first.session_id.clone(), first.path.clone()));
                }
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(open_native(&sessions, &first.session_id).1, 1);
    }

    #[test]
    fn acceptance_a25_removes_a_partial_import_when_an_entry_cannot_be_written_so_a_retry_is_possible() {
        let root = tempfile::tempdir().unwrap();
        let pi_path = write_pi(root.path(), "tree");
        let before = std::fs::read(&pi_path).unwrap();
        let sessions = root.path().join("native");
        let mut calls = 0;
        let error = import_pi_session_with(&pi_path, &sessions, &mut |n| {
            calls = n;
            if n == 10 { Err("disk full".into()) } else { Ok(()) }
        })
        .unwrap_err();
        assert_eq!(error, ImportError::Failed("disk full".into()));
        assert_eq!(calls, 10);
        assert_eq!(std::fs::read(&pi_path).unwrap(), before);
        assert!(SessionRepo::new(&sessions, SystemClock::shared()).list(None).is_empty());
        let retried = import_pi_session(&pi_path, &sessions).unwrap();
        assert_eq!(retried.imported, pi_entries(&open_pi(&pi_path)).len());
    }

    #[test]
    fn acceptance_a25_rejects_files_that_are_not_pi_sessions() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("native.jsonl");
        std::fs::write(&path, format!("{}\n", json!({"v": 4, "kind": "header", "id": "x"}))).unwrap();
        let error = import_pi_session(&path, &root.path().join("native"))
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("Not a Pi session"), "{error}");
    }

    #[test]
    fn acceptance_a25_round_trips_pi_to_native_to_pi_with_the_same_active_branch_messages_and_context() {
        let root = tempfile::tempdir().unwrap();
        let pi_path = write_pi(root.path(), "linear");
        let sessions = root.path().join("native");
        let imported = import_pi_session(&pi_path, &sessions).unwrap();
        let out = root.path().join("rollback").join("restored.jsonl");
        let exported = export_native_session_to_pi(&imported.path, &out).unwrap();
        assert_eq!(exported.path, out);
        assert_eq!(std::fs::metadata(&out).unwrap().permissions().mode() & 0o777, 0o600);
        let original = open_pi(&pi_path);
        let restored = open_pi(&out);
        let header = restored.header().unwrap();
        assert_eq!(
            (&header["type"], &header["version"], &header["cwd"]),
            (&json!("session"), &json!(3), &json!("/project"))
        );
        let branch = |m: &SessionManager| -> Vec<Value> { m.branch_entries(None).into_iter().cloned().collect() };
        assert_eq!(messages_of(&branch(&restored)), messages_of(&branch(&original)));
        assert_eq!(
            restored.build_session_context().messages,
            original.build_session_context().messages
        );
        assert_eq!(restored.session_name().as_deref(), Some("Imported work"));
        let compaction = branch(&restored)
            .into_iter()
            .find(|e| e["type"] == "compaction")
            .unwrap();
        assert_eq!(
            (&compaction["firstKeptEntryId"], &compaction["tokensBefore"]),
            (&json!("m5"), &json!(1234))
        );
        // Refuses to overwrite an existing file.
        assert!(export_native_session_to_pi(&imported.path, &out).is_err());
    }

    #[test]
    fn acceptance_a25_round_trips_the_whole_tree_identical_tree_contexts_at_every_leaf_labels_and_name() {
        for fixture in ["linear", "tree"] {
            let root = tempfile::tempdir().unwrap();
            let pi_path = write_pi(root.path(), fixture);
            let sessions = root.path().join("native");
            let imported = import_pi_session(&pi_path, &sessions).unwrap();
            let out = root.path().join("rollback").join(format!("{fixture}.jsonl"));
            let exported = export_native_session_to_pi(&imported.path, &out).unwrap();
            let original = open_pi(&pi_path);
            let restored = open_pi(&out);
            assert_eq!(exported.entries, pi_entries(&original).len());
            assert_eq!(pi_tree(&restored), pi_tree(&original), "{fixture}");
            assert_eq!(restored.leaf_id(), original.leaf_id());
            assert_eq!(pi_context(&restored), pi_context(&original));
            let leaves: Vec<String> = pi_entries(&original)
                .iter()
                .map(|e| e["id"].as_str().unwrap().to_string())
                .filter(|id| original.children(id).is_empty())
                .collect();
            assert_eq!(leaves.len(), 2);
            for leaf in &leaves {
                assert_eq!(pi_context_at(&restored, leaf), pi_context_at(&original, leaf));
            }
            assert_eq!(labels_of(&restored), labels_of(&original));
            assert_eq!(restored.session_name(), original.session_name());
        }
    }

    #[test]
    fn acceptance_a25_exports_native_changes_made_after_import_entries_labels_name_model_and_thinking() {
        let root = tempfile::tempdir().unwrap();
        let pi_path = write_pi(root.path(), "tree");
        let sessions = root.path().join("native");
        let imported = import_pi_session(&pi_path, &sessions).unwrap();
        {
            let (mut session, _) = open_native(&sessions, &imported.session_id);
            session.append_message("main", user("native follow-up", 40)).unwrap();
            session.set_label("m1", Some("start")).unwrap();
            session.set_label("m4", None).unwrap();
            session.set_name(Some("Renamed")).unwrap();
            session
                .commit(&[Write::set(
                    LANE_CONFIG,
                    "main",
                    json!({"model": {"provider": "openai", "modelId": "gpt-next"}, "thinkingLevel": "medium", "activeToolNames": []}),
                )])
                .unwrap();
        }
        let out = root.path().join("changed.jsonl");
        export_native_session_to_pi(&imported.path, &out).unwrap();
        let restored = open_pi(&out);
        let context = pi_context(&restored);
        assert_eq!(context["model"], json!({"provider": "openai", "modelId": "gpt-next"}));
        assert_eq!(context["thinkingLevel"], "medium");
        assert!(
            context["messages"]
                .as_array()
                .unwrap()
                .last()
                .unwrap()
                .to_string()
                .contains("native follow-up")
        );
        assert_eq!(restored.label("m1"), Some(&json!("start")));
        assert_eq!(restored.label("m4"), None);
        assert_eq!(restored.label("x1"), Some(&json!("abandoned-x")));
        assert_eq!(restored.session_name().as_deref(), Some("Renamed"));
        assert_eq!(pi_context_at(&restored, "x4"), pi_context_at(&open_pi(&pi_path), "x4"));
    }

    #[test]
    fn acceptance_a25_resumes_pi_at_the_native_tip_when_the_tip_is_not_the_newest_entry() {
        let root = tempfile::tempdir().unwrap();
        let pi_path = write_pi(root.path(), "tree");
        let sessions = root.path().join("native");
        let imported = import_pi_session(&pi_path, &sessions).unwrap();
        let original = open_pi(&pi_path);
        let set_tip = |tip: &str| {
            let (mut session, _) = open_native(&sessions, &imported.session_id);
            session.commit(&[Write::set(BRANCH_TIP, "main", json!(tip))]).unwrap();
        };
        // Tip moved to a leaf of the abandoned branch.
        set_tip("x4");
        let leaf_out = root.path().join("leaf.jsonl");
        export_native_session_to_pi(&imported.path, &leaf_out).unwrap();
        let restored = open_pi(&leaf_out);
        let ids =
            |m: &SessionManager| -> Vec<Value> { m.branch_entries(None).iter().map(|e| e["id"].clone()).collect() };
        let mut expected: Vec<Value> = original
            .branch_entries(Some("x4"))
            .iter()
            .map(|e| e["id"].clone())
            .collect();
        expected.extend([json!("ultron-model"), json!("ultron-thinking")]);
        assert_eq!(ids(&restored), expected);
        assert_eq!(
            pi_context(&restored),
            json!({"messages": pi_context_at(&original, "x4")["messages"],
                "model": {"provider": "anthropic", "modelId": "claude-next"}, "thinkingLevel": "low"})
        );
        // Tip moved to an interior entry: a marker child makes Pi resume there.
        set_tip("m6");
        let interior_out = root.path().join("interior.jsonl");
        export_native_session_to_pi(&imported.path, &interior_out).unwrap();
        let restored = open_pi(&interior_out);
        let branch: Vec<Value> = restored.branch_entries(None).into_iter().cloned().collect();
        let tail = &branch[branch.len() - 3..];
        assert_eq!(
            (&tail[0]["type"], &tail[0]["customType"], &tail[0]["parentId"]),
            (&json!("custom"), &json!(PI_LEAF_MARKER_CUSTOM_TYPE), &json!("m6"))
        );
        assert_eq!(
            (&tail[1]["type"], &tail[2]["type"]),
            (&json!("model_change"), &json!("thinking_level_change"))
        );
        assert_eq!(
            pi_context(&restored)["messages"],
            pi_context_at(&original, "m6")["messages"]
        );
    }

    #[test]
    fn acceptance_a25_runs_import_export_backup_and_restore_end_to_end_without_printing_secrets() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let pi_path = write_pi(&root, "linear");
        let sessions = root.join("native");
        let pi = pi_path.to_str().unwrap();
        let output = ultron(
            &root,
            &[
                "migrate",
                "import-pi",
                pi,
                "--sessions-root",
                sessions.to_str().unwrap(),
            ],
        );
        assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
        assert_eq!(text(&output.stderr), "");
        let session_id = native_session_id_for_pi_session(PI_SESSION_ID);
        let stdout = text(&output.stdout);
        let lines: Vec<&str> = stdout.lines().collect();
        assert_eq!(lines[0], format!("Imported 16 entries as native session {session_id}"));
        let native_path = open_native(&sessions, &session_id).0.metadata().path.clone();
        assert_eq!(lines[1], format!("  {}", native_path.display()));
        assert!(lines.iter().all(|l| !l.contains("skipped")));
        assert_eq!(
            lines[2],
            format!("  imported usage of 5 Pi entries ($0) into usage root import:pi:{PI_SESSION_ID}")
        );
        assert_eq!(lines.len(), 3);

        let again = ultron(
            &root,
            &[
                "migrate",
                "import-pi",
                pi,
                &format!("--sessions-root={}", sessions.display()),
            ],
        );
        assert_eq!(again.status.code(), Some(1));
        assert!(
            text(&again.stderr).starts_with("Error: Pi session "),
            "{}",
            text(&again.stderr)
        );
        assert!(text(&again.stderr).contains("already imported"));

        let out = root.join("out.jsonl");
        let exported = ultron(
            &root,
            &[
                "migrate",
                "export-pi",
                native_path.to_str().unwrap(),
                out.to_str().unwrap(),
            ],
        );
        assert_eq!(exported.status.code(), Some(0), "{}", text(&exported.stderr));
        assert_eq!(
            text(&exported.stdout),
            format!(
                "Exported native session {session_id} (16 entries) to {}\n",
                out.display()
            )
        );
        let branch = |m: &SessionManager| -> Vec<Value> { m.branch_entries(None).into_iter().cloned().collect() };
        assert_eq!(
            messages_of(&branch(&open_pi(&out))),
            messages_of(&branch(&open_pi(&pi_path)))
        );

        // Backup, restore and a tampered restore through the same binary print no secret.
        let agent = root.join("agent");
        super::write_profile(&agent);
        let backup = ultron(
            &root,
            &[
                "migrate",
                "backup",
                root.join("backups").to_str().unwrap(),
                "--agent-dir",
                agent.to_str().unwrap(),
            ],
        );
        assert_eq!(backup.status.code(), Some(0));
        let backup_path = text(&backup.stdout)
            .trim_end()
            .split(" to ")
            .nth(1)
            .unwrap()
            .to_string();
        std::fs::write(agent.join("auth.json"), "{}").unwrap();
        let restore = ultron(
            &root,
            &[
                "migrate",
                "restore",
                &backup_path,
                "--agent-dir",
                agent.to_str().unwrap(),
            ],
        );
        assert_eq!(restore.status.code(), Some(0));
        assert!(
            std::fs::read_to_string(agent.join("auth.json"))
                .unwrap()
                .contains(super::SECRET)
        );
        std::fs::write(Path::new(&backup_path).join("files/auth.json"), "tampered").unwrap();
        let tamper = ultron(
            &root,
            &[
                "migrate",
                "restore",
                &backup_path,
                "--agent-dir",
                agent.to_str().unwrap(),
            ],
        );
        assert_eq!(tamper.status.code(), Some(1));
        assert!(text(&tamper.stderr).contains("hash mismatch: auth.json"));
        for printed in [&output, &again, &exported, &backup, &restore, &tamper] {
            assert!(!text(&printed.stdout).contains(super::SECRET) && !text(&printed.stderr).contains(super::SECRET));
        }
    }
}

/// Standard base64 (the HTML export's embedded session data).
fn base64_decode(text: &str) -> Vec<u8> {
    let value = |c: u8| match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        _ => panic!("not base64: {c}"),
    };
    let bytes: Vec<u8> = text.bytes().filter(|b| *b != b'=').map(value).collect();
    bytes
        .chunks(4)
        .flat_map(|chunk| {
            let n = chunk.iter().fold(0u32, |n, b| (n << 6) | u32::from(*b)) << (6 * (4 - chunk.len()));
            let all = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
            all[..chunk.len().saturating_sub(1)].to_vec()
        })
        .collect()
}
