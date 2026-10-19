//! `ultron memory import --from claude-code|codex [--dry-run]` through `cli::run`, over a fake HOME in a temp dir
//! (never the user's ~/.claude or ~/.codex) and the fake Hindsight server of crates/hindsight/fixtures on 127.0.0.1:
//! the dry run prints the plan and writes nothing; the import stores typed records tagged with their source file,
//! refuses a note holding a secret, and a rerun leaves unchanged notes alone; a session composed in the project then
//! reads an imported record with `memory.get`.

use kernel::Signal;
use serde_json::{Map, Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;

struct Fake {
    child: Child,
    port: u16,
}

impl Fake {
    fn start() -> Fake {
        let python = std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into());
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../hindsight/fixtures/fake_hindsight.py");
        let mut child = Command::new(python)
            .arg("-I")
            .arg(script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        Fake {
            child,
            port: line.trim().parse().unwrap(),
        }
    }

    fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Request lines and bodies of every retain the server received.
    fn retains(&self) -> Vec<Value> {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        write!(stream, "GET /__log HTTP/1.1\r\nhost: fake\r\nconnection: close\r\n\r\n").unwrap();
        let mut text = String::new();
        stream.read_to_string(&mut text).unwrap();
        let log: Value = serde_json::from_str(text.split_once("\r\n\r\n").unwrap().1).unwrap();
        log.as_array()
            .unwrap()
            .iter()
            .filter(|entry| {
                entry["line"].as_str().unwrap().starts_with("POST")
                    && entry["line"].as_str().unwrap().contains("/memories ")
            })
            .map(|entry| serde_json::from_str(entry["body"].as_str().unwrap()).unwrap())
            .collect()
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        drop(self.child.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Machine {
    _dir: tempfile::TempDir,
    home: PathBuf,
}

impl Machine {
    fn new() -> Machine {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap().join("home");
        std::fs::create_dir_all(home.join(".ultron/agent")).unwrap();
        std::fs::write(
            home.join(".ultron/agent/settings.json"),
            r#"{"sandbox": {"mode": "off"}}"#,
        )
        .unwrap();
        Machine { _dir: dir, home }
    }

    fn env(&self, cwd: &Path, vars: &[(&str, String)]) -> cli::Env {
        let mut all: Vec<(String, String)> = vec![
            ("HOME".into(), self.home.display().to_string()),
            (
                "ULTRON_CODING_AGENT_DIR".into(),
                self.home.join(".ultron/agent").display().to_string(),
            ),
            (
                "ULTRON_SERVER_DIR".into(),
                self.home.join("server").display().to_string(),
            ),
        ];
        all.extend(vars.iter().map(|(k, v)| (k.to_string(), v.clone())));
        cli::Env {
            cwd: cwd.to_path_buf(),
            home: self.home.clone(),
            var: Arc::new(move |key| all.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.clone())),
            docs: config::paths::normalize(
                &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/docs"),
            ),
        }
    }
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn run(args: &[&str], env: &cli::Env) -> (i32, String, String) {
    let argv: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = cli::run(&argv, env, &mut out, &mut err);
    (code, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
}

#[test]
fn cli_memory_import_codex_dry_run_import_and_rerun() {
    let fake = Fake::start();
    let machine = Machine::new();
    let memories = machine.home.join(".codex/memories");
    let token = format!("ghp_{}", "b".repeat(36));
    write(
        &memories.join("memory_summary.md"),
        "## User preferences\n- Short answers\n\n## General Tips\n- Run the linter first\n",
    );
    write(
        &memories.join("MEMORY.md"),
        &format!("## Reusable knowledge\n- The token is {token}\n"),
    );
    let cwd = machine.home.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let env = machine.env(&cwd, &[("ULTRON_HINDSIGHT_URL", fake.base())]);
    let index = machine.home.join(".ultron/agent/memory/records.json");

    let (code, out, err) = run(&["memory", "import", "--from", "codex", "--dry-run"], &env);
    assert_eq!((code, err.as_str()), (0, ""), "{out}");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        lines[0],
        "Dry run: 3 notes from Codex (~/.codex/memories); nothing was written."
    );
    assert!(
        lines[1].starts_with("  import: preference codex-pref-short-answers-"),
        "{out}"
    );
    assert!(
        lines[1].ends_with(" (global) from Codex ~/.codex/memories/memory_summary.md"),
        "{out}"
    );
    assert!(
        lines[2].starts_with("  import: procedure codex-tip-run-the-linter-first-"),
        "{out}"
    );
    assert!(
        lines[3].starts_with("  refuse (holds a secret or personal identifier): fact codex-fact-the-token-is-"),
        "{out}"
    );
    assert!(!out.contains(&token), "the secret is never printed");
    assert_eq!(lines[4], "2 to import, 0 to replace, 0 unchanged, 1 refused.");
    assert!(fake.retains().is_empty(), "a dry run sends nothing");
    assert!(!index.exists(), "a dry run writes nothing");

    let (code, out, err) = run(&["memory", "import", "--from", "codex"], &env);
    assert_eq!((code, err.as_str()), (0, ""), "{out}");
    assert!(
        out.ends_with("Imported 2, replaced 0, unchanged 0, refused 1, failed 0 (3 notes from Codex).\n"),
        "{out}"
    );
    let retains = fake.retains();
    assert_eq!(retains.len(), 2);
    let item = &retains[0]["items"][0];
    assert_eq!(item["content"], "Short answers");
    assert_eq!(item["metadata"]["ultron_type"], "preference");
    assert_eq!(item["metadata"]["ultron_evidence_class"], "user_statement");
    let home_tag = format!(
        "ultron:global:{}",
        hindsight::resolve::directory_identity(&machine.home.display().to_string())
    );
    assert_eq!(item["tags"], json!([home_tag]));
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&index).unwrap()).unwrap();
    let records = saved["records"].as_array().unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["source"], "Codex ~/.codex/memories/memory_summary.md");
    assert_eq!(records[0]["scope"], "global");

    // A rerun leaves both alone; an edited note replaces its record (same document).
    let (code, out, _) = run(&["memory", "import", "--from", "codex"], &env);
    assert_eq!(code, 0);
    assert!(
        out.ends_with("Imported 0, replaced 0, unchanged 2, refused 1, failed 0 (3 notes from Codex).\n"),
        "{out}"
    );
    assert_eq!(fake.retains().len(), 2);

    // A session of any project on the machine reads the global record by id.
    let id = records[0]["id"].as_str().unwrap().to_string();
    let other = machine.home.join("elsewhere");
    std::fs::create_dir_all(&other).unwrap();
    let session_env = machine.env(&other, &[("ULTRON_HINDSIGHT_URL", fake.base())]);
    let runtime = session_core::compose::Runtime::new(
        &session_env,
        "print",
        machine.home.join("sessions"),
        session_core::compose::Services::default(),
    )
    .unwrap();
    let model = ai::models::builtin_models("anthropic").next().unwrap().clone();
    let stack = runtime.stack(&model, None, "s-reader").unwrap();
    let payload: Map<String, Value> = json!({"id": id}).as_object().unwrap().clone();
    let read = stack
        .cell_handler("main", None)
        .handle("memory.get", payload, &Signal::new())
        .unwrap();
    assert_eq!(read["content"], "Short answers");
    assert_eq!(read["record"]["source"], "Codex ~/.codex/memories/memory_summary.md");
}

#[test]
fn cli_memory_import_claude_code_project_notes_and_errors() {
    let fake = Fake::start();
    let machine = Machine::new();
    let checkout = machine.home.join("src/app");
    std::fs::create_dir_all(&checkout).unwrap();
    let encoded = checkout.display().to_string().replace('/', "-");
    let memory = machine.home.join(".claude/projects").join(&encoded).join("memory");
    write(&memory.join("MEMORY.md"), "- [Build](build.md)\n");
    write(
        &memory.join("build.md"),
        "---\nname: build\ndescription: when building the app\nmetadata:\n  type: project\n---\nRun make build.\n",
    );
    let env = machine.env(&checkout, &[("ULTRON_HINDSIGHT_URL", fake.base())]);
    let (code, out, err) = run(&["memory", "import", "--from=claude-code", "--dry-run"], &env);
    assert_eq!((code, err.as_str()), (0, ""));
    assert!(
        out.contains(&format!(
            " (project ~/src/app) from Claude Code ~/.claude/projects/{encoded}/memory/build.md\n"
        )),
        "{out}"
    );
    assert!(out.contains("  import: fact build-"), "{out}");
    let (code, out, err) = run(&["memory", "import", "--from", "claude-code"], &env);
    assert_eq!((code, err.as_str()), (0, ""), "{out}");
    let retains = fake.retains();
    let project_tag = format!(
        "ultron:project:{}",
        hindsight::resolve::directory_identity(&checkout.display().to_string())
    );
    assert_eq!(retains[0]["items"][0]["tags"], json!([project_tag]));
    assert_eq!(
        retains[0]["items"][0]["metadata"]["ultron_situation"],
        "when building the app"
    );

    // Errors: no notes, a bad --from, memory off.
    let empty = Machine::new();
    let env = empty.env(&checkout, &[("ULTRON_HINDSIGHT_URL", fake.base())]);
    assert_eq!(
        run(&["memory", "import", "--from", "codex"], &env),
        (
            1,
            String::new(),
            "Error: no Codex memory notes found in ~/.codex/memories\n".into()
        )
    );
    let (code, _, err) = run(&["memory", "import", "--from", "cursor"], &env);
    assert_eq!(code, 1);
    assert!(
        err.starts_with("Error: --from must be claude-code or codex\nUsage: ultron memory import"),
        "{err}"
    );
    write(
        &empty.home.join(".codex/memories/MEMORY.md"),
        "## Reusable knowledge\n- A fact\n",
    );
    let off = empty.env(&checkout, &[("ULTRON_HINDSIGHT_URL", "off".into())]);
    let (code, _, err) = run(&["memory", "import", "--from", "codex"], &off);
    assert_eq!(
        (code, err.as_str()),
        (1, "Error: Hindsight is not configured. Set ULTRON_HINDSIGHT_URL.\n")
    );
    // A dry run needs no server.
    assert_eq!(run(&["memory", "import", "--from", "codex", "--dry-run"], &off).0, 0);
}
