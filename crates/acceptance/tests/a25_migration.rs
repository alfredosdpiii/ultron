//! A25 (partial): existing Pi capabilities and data survive migration and rollback.
//! Ported from packages/coding-agent/test/ultron-migration.test.ts ("profile backup and restore" and the argument
//! cases of "runMigrationCommand") through the `ultron migrate` subcommand of the Rust binary, and from
//! packages/coding-agent/test/ultron-native-rpc.test.ts (the event translator, and the parts of the native RPC run
//! the Rust build serves) through `ultron --mode rpc`.
//! Not ported: `import-pi` / `export-pi` (12 cases plus the end-to-end CLI case) are not implemented in the Rust
//! build; native RPC `inspect` and `export_html` answer "not implemented", and `get_commands` cannot list a
//! TypeScript extension's command (TS extensions are not loaded by the Rust host).

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
    let (_, code) = client.finish(common::HANG);
    assert_eq!(code, Some(0));
}
