//! Project-isolated instances beside the global durable one (user requirement; TypeScript shares one server
//! directory): private HOME, no ULTRON_SERVER_DIR unless a test sets it.

#[path = "daemon_common/mod.rs"]
mod common;

use common::{Home, assert_none_mentioning, provider, text_reply};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const MODEL: &str = "anthropic/claude-sonnet-4-5";

fn json_line(output: &std::process::Output) -> Value {
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn prompt(home: &Home, cwd: &Path, extra: &[&str], text: &str) -> Value {
    let mut args = vec!["--mode", "json", "--model", MODEL];
    args.extend_from_slice(extra);
    args.push(text);
    json_line(&home.instance_command(cwd, &args).output().unwrap())
}

fn project_dirs(home: &Home) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(home.root.join(".ultron/projects"))
        .map(|e| e.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    dirs.sort();
    dirs
}

fn session_ids(dir: &Path) -> Vec<String> {
    let mut ids: Vec<String> = daemon::catalog::list(dir).into_iter().map(|m| m.id).collect();
    ids.sort();
    ids
}

/// Every directory a test's processes can mention: the temp root and each instance's socket directory.
fn needles(home: &Home) -> Vec<PathBuf> {
    let mut needles = vec![home.root.clone()];
    for dir in project_dirs(home).into_iter().chain([home.root.join(".ultron/server")]) {
        needles.push(daemon::instance::socket_dir(&dir));
    }
    needles
}

#[test]
fn cli_instances_projects_are_isolated_and_subdirectories_share_theirs() {
    let provider = provider((0..6).map(|i| text_reply(&format!("r{i}"))).collect(), Duration::ZERO);
    let home = Home::new(provider.port);
    let (a, b) = (home.root.join("alpha"), home.root.join("beta"));
    std::fs::create_dir_all(a.join("sub")).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    // alpha is a git project: its subdirectory resolves to the same root.
    let git = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&a)
        .status();
    assert!(git.is_ok_and(|s| s.success()));
    let in_a = prompt(&home, &a, &[], "one");
    let in_b = prompt(&home, &b, &[], "two");
    let dirs = project_dirs(&home);
    assert_eq!(dirs.len(), 2, "one instance per project: {dirs:?}");
    let instance_of = |root: &Path| {
        dirs.iter()
            .find(|d| {
                std::fs::read_to_string(d.join("project.json"))
                    .is_ok_and(|t| serde_json::from_str::<Value>(&t).unwrap()["root"] == root.display().to_string())
            })
            .cloned()
            .unwrap()
    };
    let (dir_a, dir_b) = (instance_of(&a), instance_of(&b));
    assert!(dir_a.file_name().unwrap().to_string_lossy().starts_with("alpha-"));
    assert_eq!(dir_a, daemon::instance::project_dir(&home.root, &a));
    // Each instance has its own server identity and session store.
    let id = |d: &Path| std::fs::read_to_string(d.join("default-server-id")).unwrap();
    assert_ne!(id(&dir_a), id(&dir_b));
    assert_eq!(
        session_ids(&dir_a.join("sessions")),
        [in_a["sessionId"].as_str().unwrap()]
    );
    assert_eq!(
        session_ids(&dir_b.join("sessions")),
        [in_b["sessionId"].as_str().unwrap()]
    );
    assert_ne!(in_a["serverId"], in_b["serverId"]);
    // -c in beta continues beta's session, never alpha's.
    let again_b = prompt(&home, &b, &["-c"], "three");
    assert_eq!(again_b["sessionId"], in_b["sessionId"]);
    // A subdirectory of alpha uses alpha's instance.
    let in_sub = prompt(&home, &a.join("sub"), &[], "four");
    assert_eq!(in_sub["serverId"], in_a["serverId"]);
    assert_eq!(project_dirs(&home).len(), 2);
    assert_eq!(session_ids(&dir_a.join("sessions")).len(), 2);
    // Nothing went to the durable instance's or the explicit default store.
    assert!(!home.root.join(".ultron/server").exists());
    assert!(session_ids(&home.sessions()).is_empty());
    assert_none_mentioning(&needles(&home), Duration::from_secs(10));
}

#[test]
fn cli_instances_server_dir_override_selects_the_instance() {
    let provider = provider(vec![text_reply("x")], Duration::ZERO);
    let home = Home::new(provider.port);
    // Home::command sets ULTRON_SERVER_DIR: no project instance is created.
    let output = home.run(&["--mode", "json", "--model", MODEL, "hello"]);
    let result = json_line(&output);
    assert!(project_dirs(&home).is_empty());
    let id = std::fs::read_to_string(home.server_dir().join("default-server-id")).unwrap();
    assert_eq!(result["serverId"], id);
    assert_eq!(session_ids(&home.sessions()), [result["sessionId"].as_str().unwrap()]);
    assert_none_mentioning(&needles(&home), Duration::from_secs(10));
}

fn instances(home: &Home) -> Vec<Vec<String>> {
    let output = home.instance_command(&home.cwd(), &["instances"]).output().unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|l| l.split('\t').map(str::to_string).collect())
        .collect()
}

#[test]
fn cli_instances_durable_is_separate_and_survives_a_killed_project_server() {
    // The project run must stay in progress until the test kills it, however slowly the machine gets there: its
    // reply is held far past the test's end.
    let provider = provider(vec![text_reply("slow"), text_reply("p")], Duration::from_secs(3600));
    let home = Home::new(provider.port);
    let project = home.root.join("proj");
    std::fs::create_dir_all(&project).unwrap();
    let started = home.instance_command(&home.cwd(), &["durable"]).output().unwrap();
    assert!(started.status.success(), "{}", String::from_utf8_lossy(&started.stderr));
    let line = String::from_utf8(started.stdout).unwrap();
    let (durable_id, durable_socket) = line.trim().split_once('\t').unwrap();
    assert!(client::runtime::Connection::connect(Path::new(durable_socket), durable_id).is_ok());
    // A second `ultron durable` finds the running one.
    let again = home.instance_command(&home.cwd(), &["durable"]).output().unwrap();
    assert_eq!(String::from_utf8(again.stdout).unwrap(), line);
    // A project run in progress is a second instance; the durable one lists none of its sessions.
    let mut client = home
        .instance_command(&project, &["--mode", "json", "--model", MODEL, "busy"])
        .spawn()
        .unwrap();
    let deadline = Instant::now() + common::HANG_GUARD;
    let running = loop {
        let running = instances(&home);
        if running.len() == 2 {
            break running;
        }
        // A client that exited cannot become the second instance: fail now (its stderr is the test's).
        if let Some(status) = client.try_wait().unwrap() {
            panic!("the project client exited ({status}); instances: {running:?}");
        }
        assert!(Instant::now() < deadline, "instances: {running:?}");
        std::thread::sleep(Duration::from_millis(50));
    };
    let kinds: Vec<&str> = running.iter().map(|r| r[0].as_str()).collect();
    assert_eq!(kinds, ["durable", "project"]);
    assert_eq!(running[1][1], project.display().to_string());
    let durable_sessions = client::runtime::run_client(
        &client::runtime::ClientCommand::default(),
        &client::runtime::Route {
            server_id: durable_id.to_string(),
            path: PathBuf::from(durable_socket),
        },
    )
    .unwrap();
    assert_eq!(durable_sessions, client::runtime::ClientResult::List(vec![]));
    // Killing the project's server (its `ultron` process) leaves the durable one running.
    client.kill().unwrap();
    client.wait().unwrap();
    let deadline = Instant::now() + common::HANG_GUARD;
    let left = loop {
        let left = instances(&home);
        if left.len() == 1 {
            break left;
        }
        assert!(Instant::now() < deadline, "instances: {left:?}");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(left[0][0], "durable");
    assert!(client::runtime::Connection::connect(Path::new(durable_socket), durable_id).is_ok());
    let status = home
        .instance_command(&home.cwd(), &["durable", "status"])
        .output()
        .unwrap();
    assert!(status.status.success());
    let stopped = home
        .instance_command(&home.cwd(), &["durable", "stop"])
        .output()
        .unwrap();
    assert!(stopped.status.success());
    assert_none_mentioning(&needles(&home), Duration::from_secs(15));
    assert!(instances(&home).is_empty());
}
