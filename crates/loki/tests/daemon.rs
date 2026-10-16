//! `loki daemon` and its client (loki.py `daemon_client`, `daemon_serve`, `daemon_command`), between processes
//! this test starts, in a private HOME: a forwarded `hook` / `protect` answers exactly as the local run does, the
//! client speaks the request protocol (engine digest, argv, stdin, cwd, environment) and runs locally on a stale or
//! missing daemon, `status` sees a serving daemon, `stop` and a request for another engine end it.

use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const HANG_GUARD: Duration = Duration::from_secs(120);
/// A fake AWS access key id, built from parts so the source holds no credential-shaped literal.
const KEY: &str = concat!("AKIA", "Q7XRJ2M4", "PL3ZWNVB");

struct Repo {
    _dir: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
}

fn git(cwd: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", cwd)
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

fn repo() -> Repo {
    let dir = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let (root, home) = (base.join("r"), base.join("h"));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(root.join("a.txt"), "x\n").unwrap();
    git(&root, &["init", "-q"]);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "base"]);
    Repo { _dir: dir, root, home }
}

fn socket(repo: &Repo) -> PathBuf {
    let digest: String = Sha256::digest(repo.root.as_os_str().as_encoded_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    repo.home.join(".cache/loki/d").join(&digest[..16])
}

fn loki(repo: &Repo, args: &[&str], daemon: Option<&str>, stdin: &str) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_loki"));
    command
        .args(["--root", repo.root.to_str().unwrap()])
        .args(args)
        .current_dir(&repo.root)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", &repo.home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(value) = daemon {
        command.env("LOKI_DAEMON", value);
    }
    let mut child = command.spawn().unwrap();
    child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
    child.wait_with_output().unwrap()
}

fn wait_for(what: &str, ready: impl Fn() -> bool) {
    let deadline = Instant::now() + HANG_GUARD;
    while !ready() {
        assert!(Instant::now() < deadline, "{what}: not reached");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Waits for a daemon this test started to end on its own; never signals it.
fn ended(child: &mut Child) -> bool {
    let deadline = Instant::now() + HANG_GUARD;
    while Instant::now() < deadline {
        if child.try_wait().unwrap().is_some() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

fn serve(repo: &Repo) -> Child {
    let child = Command::new(env!("CARGO_BIN_EXE_loki"))
        .args(["--root", repo.root.to_str().unwrap(), "daemon", "serve"])
        .current_dir(&repo.root)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", &repo.home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_for("the daemon's socket", || socket(repo).exists());
    child
}

fn parts(output: &Output) -> (Option<i32>, String, String) {
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn loki_daemon_runs_forwarded_hooks_exactly_as_the_local_engine() {
    let repo = repo();
    let mut daemon = serve(&repo);
    let status = loki(&repo, &["daemon", "status"], None, "");
    assert_eq!(
        String::from_utf8_lossy(&status.stdout),
        format!("loki daemon: running ({})\n", socket(&repo).display())
    );
    std::fs::write(repo.root.join("a.txt"), format!("x\nkey = '{KEY}'\n")).unwrap();
    let forwarded = loki(&repo, &["hook", "--file", "a.txt"], None, "");
    let local = loki(&repo, &["hook", "--file", "a.txt"], Some("0"), "");
    assert_eq!(parts(&forwarded), parts(&local));
    assert_eq!(forwarded.status.code(), Some(2));
    // A command that reads stdin takes it along.
    let envelope = format!(r#"{{"tool_name": "write", "tool_input": {{"path": "x", "content": "key = '{KEY}'\n"}}}}"#);
    let args = ["protect", "--file", "b.py", "--preview", "ultron"];
    assert_eq!(
        parts(&loki(&repo, &args, None, &envelope)),
        parts(&loki(&repo, &args, Some("0"), &envelope))
    );
    // `stop` ends it; its socket goes with it.
    let stopped = loki(&repo, &["daemon", "stop"], None, "");
    assert_eq!(
        String::from_utf8_lossy(&stopped.stdout),
        format!("loki daemon: running ({})\n", socket(&repo).display())
    );
    assert!(ended(&mut daemon), "the daemon did not stop");
    assert!(!socket(&repo).exists());
    let status = loki(&repo, &["daemon", "status"], None, "");
    assert_eq!(
        String::from_utf8_lossy(&status.stdout),
        format!("loki daemon: not running ({})\n", socket(&repo).display())
    );
}

#[test]
fn loki_daemon_answers_another_engine_stale_and_exits() {
    let repo = repo();
    let mut daemon = serve(&repo);
    let mut connection = std::os::unix::net::UnixStream::connect(socket(&repo)).unwrap();
    connection
        .write_all(br#"{"engine": "another", "argv": ["hook", "--file", "a.txt"], "stdin": "", "cwd": "/", "env": {}}"#)
        .unwrap();
    connection.shutdown(std::net::Shutdown::Write).unwrap();
    let mut reply = String::new();
    connection.read_to_string(&mut reply).unwrap();
    assert_eq!(reply, r#"{"stale": true}"#);
    assert!(ended(&mut daemon));
}

#[test]
fn loki_daemon_client_speaks_the_protocol_and_falls_back_locally() {
    let repo = repo();
    let path = socket(&repo);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let listener = UnixListener::bind(&path).unwrap();
    // A stand-in daemon: records the request and answers it.
    let respond = |answer: &'static str| {
        let listener = listener.try_clone().unwrap();
        std::thread::spawn(move || {
            let (mut connection, _) = listener.accept().unwrap();
            let mut request = String::new();
            connection.read_to_string(&mut request).unwrap();
            connection.write_all(answer.as_bytes()).unwrap();
            serde_json::from_str::<serde_json::Value>(&request).unwrap()
        })
    };
    let served = respond(r#"{"exit": 3, "stdout": "from the daemon\n", "stderr": "note\n"}"#);
    let output = loki(
        &repo,
        &["protect", "--file", "a.txt", "--preview", "ultron"],
        None,
        "{}",
    );
    assert_eq!(parts(&output), (Some(3), "from the daemon\n".into(), "note\n".into()));
    let request = served.join().unwrap();
    assert_eq!(
        request["argv"],
        serde_json::json!([
            "--root",
            repo.root.to_str().unwrap(),
            "protect",
            "--file",
            "a.txt",
            "--preview",
            "ultron"
        ])
    );
    assert_eq!(request["stdin"], "{}");
    assert_eq!(request["cwd"], repo.root.to_str().unwrap());
    assert_eq!(request["env"]["HOME"], repo.home.to_str().unwrap());
    let engine: String = Sha256::digest(std::fs::read(env!("CARGO_BIN_EXE_loki")).unwrap())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(request["engine"], engine);
    // A stale daemon: the command runs here, on the stdin forwarding already read.
    let stale = respond(r#"{"stale": true}"#);
    let envelope = format!(r#"{{"tool_name": "write", "tool_input": {{"path": "x", "content": "key = '{KEY}'\n"}}}}"#);
    let args = ["protect", "--file", "b.py", "--preview", "ultron"];
    let fallback = loki(&repo, &args, None, &envelope);
    stale.join().unwrap();
    assert_eq!(parts(&fallback), parts(&loki(&repo, &args, Some("0"), &envelope)));
    assert_eq!(fallback.status.code(), Some(2));
    // LOKI_DAEMON=0 never forwards; commands other than protect, hook and shell never do.
    let _ = std::fs::remove_file(&path);
    drop(listener);
    let listener = UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    assert!(
        loki(&repo, &["hook", "--file", "a.txt"], Some("0"), "")
            .status
            .code()
            .is_some()
    );
    assert!(loki(&repo, &["scan"], None, "").status.code().is_some());
    assert!(listener.accept().is_err(), "nothing connected");
}

#[test]
fn loki_daemon_is_never_started_by_a_hook_unasked() {
    let repo = repo();
    let output = loki(&repo, &["hook", "--file", "a.txt"], None, "");
    assert_eq!(output.status.code(), Some(0));
    assert!(!repo.home.join(".cache/loki/d").exists(), "a hook started a daemon");
}
