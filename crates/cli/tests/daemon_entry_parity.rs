//! Entry-point parity through the `ultron` binary (Rust-only fixes, literal expectations): every client gets the
//! same session, whatever command started it. A scripted Anthropic server on 127.0.0.1, private HOME / agent /
//! server dirs; any durable server a test starts is stopped by `ultron durable stop` (it is the test's own).
//!
//! - `ultron durable` attaches under `--offline` / `PI_OFFLINE` and the `httpProxy` setting (the server runs with
//!   them), and the options that do conflict are named;
//! - `-p` and `--mode json` run `/goal`, `/persona`, `/keepgoing` as the session's commands, never a model turn;
//! - `--thinking` applies to a resumed session;
//! - `ultron-s` resolves `@file`, `--append-system-prompt` and `--skill` against the caller's directory and refuses
//!   `-c` / `-r` / `--fork`;
//! - `--no-session --name x` keeps the name; `--skill` loads the skill; `-na` ignores the project's resources.

#[path = "daemon_common/mod.rs"]
mod common;

use common::{HANG_GUARD, Home, assert_none_mentioning, provider, text_reply};
use serde_json::{Value, json};
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

const MODEL: &str = "anthropic/claude-sonnet-4-5";

fn output(command: &mut Command) -> (i32, String, String) {
    let out = command.output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn durable_dir(home: &Home) -> PathBuf {
    home.root.join(".ultron/server")
}

fn stop_durable(home: &Home) {
    let (code, _, err) = output(&mut home.instance_command(&home.cwd(), &["durable", "stop"]));
    assert_eq!(code, 0, "{err}");
    assert_none_mentioning(
        &[home.root.clone(), daemon::instance::socket_dir(&durable_dir(home))],
        HANG_GUARD,
    );
}

/// The environment of a live process as `KEY=value` lines.
fn environ(pid: u32) -> Vec<String> {
    std::fs::read(format!("/proc/{pid}/environ"))
        .map(|bytes| {
            bytes
                .split(|b| *b == 0)
                .map(|v| String::from_utf8_lossy(v).into_owned())
                .collect()
        })
        .unwrap_or_default()
}

fn system_text(request: &Value) -> String {
    match &request["system"] {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn user_text(request: &Value) -> String {
    request["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["role"] == "user")
        .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
        .filter_map(|c| c["text"].as_str().map(str::to_string))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn cli_entry_durable_attaches_under_offline_and_the_proxy_setting_and_takes_session_options() {
    let provider = provider(
        vec![
            text_reply("offline answer"),
            text_reply("env answer"),
            text_reply("third answer"),
            text_reply("fourth answer"),
        ],
        Duration::ZERO,
    );
    let home = Home::new(provider.port);
    std::fs::write(
        home.agent().join("settings.json"),
        r#"{"sessionNaming":{"enabled":false},"durable":{"model":"anthropic/claude-opus-4-8"},"httpProxy":"http://127.0.0.1:9"}"#,
    )
    .unwrap();
    // The provider is on 127.0.0.1, which the proxy does not cover.
    let mut first = home.instance_command(&home.cwd(), &["durable", "--offline", "-p", "hello"]);
    first.env("NO_PROXY", "127.0.0.1");
    let (code, out, err) = output(&mut first);
    assert_eq!((code, out.as_str()), (0, "offline answer\n"), "{err}");
    // The server this client activated runs with the client's run environment (and so do its workers).
    let generations = daemon::instance::generations(&durable_dir(&home));
    assert_eq!(generations.len(), 1);
    let env = environ(generations[0].pid);
    for wanted in [
        "PI_OFFLINE=1",
        "HTTP_PROXY=http://127.0.0.1:9",
        "HTTPS_PROXY=http://127.0.0.1:9",
    ] {
        assert!(env.iter().any(|v| v == wanted), "{wanted} not in {env:?}");
    }
    // PI_OFFLINE in the client's own environment attaches too, to the same server.
    let mut second = home.instance_command(&home.cwd(), &["durable", "-p", "again"]);
    second.env("NO_PROXY", "127.0.0.1").env("PI_OFFLINE", "1");
    let (code, out, err) = output(&mut second);
    assert_eq!((code, out.as_str()), (0, "env answer\n"), "{err}");
    assert_eq!(daemon::instance::generations(&durable_dir(&home)).len(), 1);
    // The client's own session options reach its session's worker (no refusal): `--thinking`, `--no-session`
    // and `--model` run.
    let (code, out, err) = output(&mut home.instance_command(
        &home.cwd(),
        &["durable", "-p", "--thinking", "high", "--no-session", "x"],
    ));
    assert_eq!((code, out.as_str()), (0, "third answer\n"), "{err}");
    let (code, out, err) = output(&mut home.instance_command(&home.cwd(), &["durable", "-p", "--model", MODEL, "y"]));
    assert_eq!((code, out.as_str()), (0, "fourth answer\n"), "{err}");
    assert_eq!(provider.requests.lock().unwrap().len(), 4);
    stop_durable(&home);
}

#[test]
fn cli_entry_print_and_json_modes_run_session_commands_never_a_turn() {
    let provider = provider(Vec::new(), Duration::ZERO);
    let home = Home::new(provider.port);
    let (code, out, err) = output(&mut home.command(&["--model", MODEL, "-p", "/persona on"]));
    assert_eq!(
        (code, out.as_str()),
        (0, "Persona is on for this session; keep-going off\n"),
        "{err}"
    );
    let (code, out, err) = output(&mut home.command(&["--model", MODEL, "-p", "/goal clear"]));
    assert_eq!((code, out.as_str()), (0, "No goal to clear.\n"), "{err}");
    let (code, out, err) = output(&mut home.command(&["--model", MODEL, "--mode", "json", "/keepgoing"]));
    assert_eq!(code, 0, "{err}");
    let result: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(
        result["text"],
        "Persona for this session follows the settings (off); keep-going off"
    );
    // The same session's toggle is kept: `/keepgoing on` then a report in that session.
    let id = "5d0c1a2b-3c4d-4e5f-8a6b-7c8d9e0f1a2b";
    let (code, out, err) = output(&mut home.command(&["--model", MODEL, "--session-id", id, "-p", "/keepgoing on"]));
    assert_eq!(code, 0, "{err}");
    assert!(
        out.starts_with("Persona is on for this session; keep-going on (0/"),
        "{out}"
    );
    let (code, out, err) = output(&mut home.command(&["--session", id, "-p", "/persona"]));
    assert_eq!(code, 0, "{err}");
    assert!(
        out.starts_with("Persona is on for this session; keep-going on (0/"),
        "{out}"
    );
    assert!(
        provider.requests.lock().unwrap().is_empty(),
        "a command reached the model"
    );
}

#[test]
fn cli_entry_thinking_flag_applies_to_a_resumed_session() {
    let provider = provider(vec![text_reply("one"), text_reply("two")], Duration::ZERO);
    let home = Home::new(provider.port);
    let id = "6e1d2b3c-4d5e-4f60-9b7c-8d9e0f1a2b3c";
    let (code, _, err) =
        output(&mut home.command(&["--model", MODEL, "--thinking", "off", "--session-id", id, "-p", "first"]));
    assert_eq!(code, 0, "{err}");
    // Resumed without --model: the session keeps its model, and --thinking holds.
    let (code, _, err) = output(&mut home.command(&["--session", id, "--thinking", "high", "-p", "second"]));
    assert_eq!(code, 0, "{err}");
    let requests = provider.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["model"], "claude-sonnet-4-5");
    assert_eq!(requests[1]["model"], "claude-sonnet-4-5");
    assert!(
        requests[0].get("thinking").is_none_or(|t| t["type"] == "disabled"),
        "{}",
        requests[0]
    );
    assert_eq!(requests[1]["thinking"]["type"], "enabled", "{}", requests[1]);
}

/// `ultron-s` (the binary under that name) with `home`'s environment, in `cwd`.
fn home_session(home: &Home, cwd: &Path, args: &[&str]) -> Command {
    let mut command = home.instance_command(cwd, args);
    command.arg0("ultron-s");
    command
}

#[test]
fn cli_entry_home_session_resolves_files_in_the_callers_directory_for_its_flags() {
    let provider = provider(vec![text_reply("summarized"), text_reply("with flags")], Duration::ZERO);
    let home = Home::new(provider.port);
    let work = home.cwd().join("project");
    std::fs::create_dir_all(work.join("skills/deploy")).unwrap();
    std::fs::write(work.join("notes.md"), "NOTE-TEXT-1").unwrap();
    std::fs::write(work.join("extra.md"), "APPEND-TEXT-2").unwrap();
    std::fs::write(
        work.join("skills/deploy/SKILL.md"),
        "---\nname: deploy-here\ndescription: Deploys the caller's project.\n---\nSteps.\n",
    )
    .unwrap();
    let (code, out, err) = output(&mut home_session(&home, &work, &["-p", "@notes.md", "summarize"]));
    assert_eq!((code, out.as_str()), (0, "summarized\n"), "{err}");
    let requests = provider.requests.lock().unwrap().clone();
    let user = user_text(&requests[0]);
    assert!(
        user.contains(&format!("<file name=\"{}\">", work.join("notes.md").display())),
        "{user}"
    );
    assert!(user.contains("NOTE-TEXT-1"), "{user}");
    // Per-client prompt and skill flags reach the session's worker: the caller's files, named in its directory.
    let (code, out, err) = output(&mut home_session(
        &home,
        &work,
        &[
            "-p",
            "--append-system-prompt",
            "extra.md",
            "--skill",
            "skills/deploy",
            "x",
        ],
    ));
    assert_eq!((code, out.as_str()), (0, "with flags\n"), "{err}");
    let system = system_text(&provider.requests.lock().unwrap()[1]);
    assert!(
        system.contains("APPEND-TEXT-2") && system.contains("deploy-here"),
        "{system}"
    );
    // What ultron-s hands the client names the caller's files (it runs in the home directory).
    let rewritten = cli::durable_client::caller_args(
        &[
            "--append-system-prompt",
            "extra.md",
            "--append-system-prompt",
            "plain text",
            "--skill",
            "skills/deploy",
            "--name",
            "@kept",
            "@notes.md",
            "--",
            "@b.md",
        ]
        .map(String::from),
        &work,
        &home.root,
    );
    let at = |name: &str| work.join(name).display().to_string();
    assert_eq!(
        rewritten,
        [
            "--append-system-prompt".to_string(),
            at("extra.md"),
            "--append-system-prompt".into(),
            "plain text".into(),
            "--skill".into(),
            at("skills/deploy"),
            "--name".into(),
            "@kept".into(),
            format!("@{}", at("notes.md")),
            "--".into(),
            format!("@{}", at("b.md")),
        ]
    );
    // Another selector than the home session is no longer refused (see daemon_durable_home); one at a time.
    let (code, _, err) = output(&mut home_session(&home, &work, &["-c", "-r", "-p", "x"]));
    assert_eq!(code, 1);
    assert!(err.contains("Usage: ultron-s"), "{err}");
    assert_eq!(provider.requests.lock().unwrap().len(), 2);
    stop_durable(&home);
}

#[test]
fn cli_entry_no_session_keeps_its_name_over_rpc() {
    let provider = provider(Vec::new(), Duration::ZERO);
    let home = Home::new(provider.port);
    let mut child = home
        .command(&[
            "--model",
            MODEL,
            "--no-session",
            "--name",
            "scratch work",
            "--mode",
            "rpc",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, "{}", json!({"id": "s", "type": "get_state"})).unwrap();
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    let state = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|l| l["id"] == "s")
        .unwrap_or_else(|| panic!("no state: {}", String::from_utf8_lossy(&out.stderr)));
    assert_eq!(state["data"]["sessionName"], "scratch work", "{state}");
}

#[test]
fn cli_entry_no_approve_ignores_the_projects_resources_for_the_run() {
    let provider = provider(vec![text_reply("trusted"), text_reply("ignored")], Duration::ZERO);
    let home = Home::new(provider.port);
    // A project's own APPEND_SYSTEM.md needs trust: trusted (defaultProjectTrust "always"), it is in the prompt.
    std::fs::create_dir_all(home.cwd().join(".pi")).unwrap();
    std::fs::write(home.cwd().join(".pi/APPEND_SYSTEM.md"), "PROJECT-RULE-3").unwrap();
    std::fs::write(
        home.agent().join("settings.json"),
        r#"{"sessionNaming":{"enabled":false},"defaultProjectTrust":"always"}"#,
    )
    .unwrap();
    let (code, _, err) = output(&mut home.command(&["--model", MODEL, "-p", "trusted-run-prompt"]));
    assert_eq!(code, 0, "{err}");
    let (code, _, err) = output(&mut home.command(&["--model", MODEL, "-na", "-p", "ignored-run-prompt"]));
    assert_eq!(code, 0, "{err}");
    // Each run's request by its prompt (a retried request would shift the order).
    let requests = provider.requests.lock().unwrap().clone();
    let of = |prompt: &str| {
        requests
            .iter()
            .filter(|r| user_text(r).contains(prompt))
            .map(system_text)
            .collect::<Vec<_>>()
    };
    let (trusted, ignored) = (of("trusted-run-prompt"), of("ignored-run-prompt"));
    assert!(!trusted.is_empty() && trusted.iter().all(|s| s.contains("PROJECT-RULE-3")));
    assert!(!ignored.is_empty(), "{requests:?}");
    for system in &ignored {
        assert!(!system.contains("PROJECT-RULE-3"), "{system}");
    }
}

#[test]
fn cli_entry_unapplied_flags_warn() {
    let home = Home::new(1);
    let (code, _, err) = output(&mut home.command(&[
        "--prompt-template",
        "p.md",
        "--no-prompt-templates",
        "--theme",
        "t.json",
        "--no-themes",
        "--no-skills",
        "--skill",
        "s",
        "--version",
    ]));
    assert_eq!(code, 0);
    assert_eq!(
        err,
        "Warning: ultron-rs does not take --prompt-template p.md from the command line; ignoring it\n\
         Warning: ultron-rs does not take --no-prompt-templates from the command line; ignoring it\n\
         Warning: ultron-rs does not take --theme t.json from the command line; ignoring it\n\
         Warning: ultron-rs does not take --no-themes from the command line; ignoring it\n\
         Warning: --no-skills leaves every skill out; ignoring --skill s\n"
    );
}

#[test]
fn cli_entry_skill_flag_loads_the_skill_from_the_callers_directory() {
    let provider = provider(vec![text_reply("one"), text_reply("two")], Duration::ZERO);
    let home = Home::new(provider.port);
    std::fs::create_dir_all(home.cwd().join("skills/deploy")).unwrap();
    std::fs::write(
        home.cwd().join("skills/deploy/SKILL.md"),
        "---\nname: deploy-here\ndescription: Deploys the caller's project.\n---\nSteps.\n",
    )
    .unwrap();
    let (code, _, err) = output(&mut home.command(&["--model", MODEL, "-p", "without"]));
    assert_eq!(code, 0, "{err}");
    let (code, _, err) = output(&mut home.command(&["--model", MODEL, "--skill", "skills/deploy", "-p", "with"]));
    assert_eq!(code, 0, "{err}");
    let requests = provider.requests.lock().unwrap().clone();
    assert!(!system_text(&requests[0]).contains("deploy-here"));
    let system = system_text(&requests[1]);
    let skills_dir = home.cwd().join("skills/deploy/SKILL.md");
    let entry = format!(
        "<skill>\n    <name>deploy-here</name>\n    <description>Deploys the caller&apos;s project.</description>\n    <location>{}</location>\n  </skill>",
        skills_dir.display()
    );
    assert!(system.contains(&entry), "{system}");
}
