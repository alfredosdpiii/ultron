//! `ultron-s` through the product (user requirement; literal expectations): the binary run through a link named
//! `ultron-s` is a client of the durable instance's home session. The home session is created once and reused across
//! clients and a server restart, `--new` replaces it, `--session <id> --home` picks another, a deleted one is
//! replaced, `--list` lists them; `-p` / `--mode json` send to it; a client that goes away mid-run leaves the run
//! going on the durable server, and the next client sees it.

#[path = "daemon_common/mod.rs"]
mod common;

use common::{HANG_GUARD, Home, assert_none_mentioning, text_reply};
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A scripted provider whose first reply waits until `gate` exists.
fn gated_provider(replies: Vec<String>, gate: PathBuf) -> (u16, Arc<Mutex<Vec<Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();
    std::thread::spawn(move || {
        for (i, stream) in listener.incoming().enumerate() {
            let Ok(mut stream) = stream else { continue };
            let reply = replies
                .get(i)
                .cloned()
                .unwrap_or_else(|| text_reply("(no more replies)"));
            let (seen, gate) = (seen.clone(), gate.clone());
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let mut length = 0;
                loop {
                    let mut header = String::new();
                    reader.read_line(&mut header).unwrap();
                    if header.trim().is_empty() {
                        break;
                    }
                    if let Some((k, v)) = header.split_once(':')
                        && k.eq_ignore_ascii_case("content-length")
                    {
                        length = v.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                seen.lock()
                    .unwrap()
                    .push(serde_json::from_slice(&body).unwrap_or(Value::Null));
                let end = Instant::now() + HANG_GUARD;
                while i == 0 && !gate.exists() && Instant::now() < end {
                    std::thread::sleep(Duration::from_millis(10));
                }
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    reply.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(reply.as_bytes());
            });
        }
    });
    (port, requests)
}

/// `ultron-s`: a link to the binary in the test's own bin directory.
fn ultron_s(home: &Home) -> PathBuf {
    let bin = home.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let link = bin.join("ultron-s");
    if !link.exists() {
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_ultron"), &link).unwrap();
    }
    link
}

fn command(home: &Home, args: &[&str]) -> std::process::Command {
    let template = home.instance_command(&home.cwd(), &[]);
    let mut command = std::process::Command::new(ultron_s(home));
    command
        .args(args)
        .current_dir(home.cwd())
        .stdin(std::process::Stdio::null())
        .env_clear();
    for (key, value) in template.get_envs() {
        if let Some(value) = value {
            command.env(key, value);
        }
    }
    command.env_remove("ULTRON_SERVER_DIR");
    command
}

fn run(home: &Home, args: &[&str]) -> (i32, String, String) {
    let output = command(home, args).output().unwrap();
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn json_run(home: &Home, args: &[&str]) -> Value {
    let (code, out, err) = run(home, args);
    assert_eq!(code, 0, "{err}");
    serde_json::from_str(out.trim()).unwrap_or_else(|_| panic!("{out}"))
}

fn durable_dir(home: &Home) -> PathBuf {
    home.root.join(".ultron/server")
}

fn home_id(home: &Home) -> String {
    std::fs::read_to_string(durable_dir(home).join("home-session"))
        .unwrap()
        .trim()
        .to_string()
}

fn needles(home: &Home) -> Vec<PathBuf> {
    vec![home.root.clone(), daemon::instance::socket_dir(&durable_dir(home))]
}

fn stop(home: &Home) {
    let output = home
        .instance_command(&home.cwd(), &["durable", "stop"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_none_mentioning(&needles(home), HANG_GUARD);
}

fn session_file(home: &Home, id: &str) -> PathBuf {
    daemon::catalog::list(&home.sessions())
        .into_iter()
        .find(|m| m.id == id)
        .map(|m| m.path)
        .unwrap_or_else(|| panic!("no session {id}"))
}

#[test]
fn cli_ultron_s_dispatches_on_its_name() {
    for name in ["ultron-s", "/usr/local/bin/ultron-s", "ultron-s.exe", "bin/ultron-s"] {
        assert!(cli::durable_client::is_home_command(name), "{name}");
    }
    for name in ["ultron", "ultron-sx", "/opt/ultron-s/ultron", ""] {
        assert!(!cli::durable_client::is_home_command(name), "{name}");
    }
    let home = Home::new(1);
    let (code, out, err) = run(&home, &["--list"]);
    assert_eq!((code, out.as_str()), (0, "ID\tNAME\tLAST ACTIVITY\tHOME\n"), "{err}");
    // The plain binary does not: `--list` there is no such command (a usage error or a prompt, never the list).
    let plain = home.instance_command(&home.cwd(), &["--list"]).output().unwrap();
    assert!(!String::from_utf8_lossy(&plain.stdout).starts_with("ID\tNAME\tLAST ACTIVITY"));
    let (code, _, err) = run(&home, &["--home"]);
    assert_eq!(code, 1);
    assert!(err.contains("Usage: ultron-s"), "{err}");
    assert_none_mentioning(&needles(&home), HANG_GUARD);
}

#[test]
fn cli_ultron_s_home_session_is_created_once_reused_replaced_and_listed() {
    let replies = (0..8).map(|i| text_reply(&format!("answer {i}"))).collect();
    let provider = common::provider(replies, Duration::ZERO);
    let home = Home::new(provider.port);
    // Created on the first run and recorded; the run happens in it.
    let (code, out, err) = run(&home, &["-p", "one"]);
    assert_eq!((code, out.as_str()), (0, "answer 0\n"), "{err}");
    let first = home_id(&home);
    // A second client and a client after a server restart reuse it.
    assert_eq!(json_run(&home, &["--mode", "json", "two"])["sessionId"], first.as_str());
    stop(&home);
    let again = json_run(&home, &["--mode", "json", "three"]);
    assert_eq!(
        (again["sessionId"].as_str(), again["text"].as_str()),
        (Some(first.as_str()), Some("answer 2"))
    );
    let listed = daemon::catalog::list(&home.sessions());
    assert_eq!(listed.len(), 1);
    // The home session lives in the home directory, wherever `ultron-s` ran.
    assert_eq!(listed[0].cwd, home.root.display().to_string());
    // The conversation goes on in it: the third request carries the first two turns.
    let third = provider.requests.lock().unwrap()[2].clone();
    assert!(third.to_string().contains("answer 1"), "{third}");
    // `--new` makes a fresh one home.
    let fresh = json_run(&home, &["--new", "--mode", "json", "four"]);
    let second = fresh["sessionId"].as_str().unwrap().to_string();
    assert_ne!(second, first);
    assert_eq!(home_id(&home), second);
    let (code, out, _) = run(&home, &["--list"]);
    assert_eq!(code, 0);
    let rows: Vec<Vec<String>> = out
        .lines()
        .skip(1)
        .map(|l| l.split('\t').map(str::to_string).collect())
        .collect();
    assert_eq!(rows.len(), 2, "{out}");
    assert_eq!((rows[0][0].as_str(), rows[0][3].as_str()), (second.as_str(), "*"));
    assert_eq!((rows[1][0].as_str(), rows[1][3].as_str()), (first.as_str(), ""));
    assert!(rows[0][2].ends_with('Z'), "{out}");
    // `--session <id>` sends to another without moving home; with `--home` it becomes home.
    assert_eq!(
        json_run(&home, &["--session", &first, "--mode", "json", "five"])["sessionId"],
        first.as_str()
    );
    assert_eq!(home_id(&home), second);
    json_run(&home, &["--session", &first, "--home", "--mode", "json", "six"]);
    assert_eq!(home_id(&home), first);
    let (code, _, err) = run(&home, &["--session", "no-such-session", "-p", "x"]);
    assert_eq!((code, err.trim()), (1, "Error: Unknown session: no-such-session"));
    // A deleted home session is replaced by a new one.
    stop(&home);
    std::fs::remove_file(session_file(&home, &first)).unwrap();
    let replaced = json_run(&home, &["--mode", "json", "seven"]);
    let third_id = replaced["sessionId"].as_str().unwrap().to_string();
    assert!(third_id != first && third_id != second);
    assert_eq!(home_id(&home), third_id);
    stop(&home);
}

#[test]
fn cli_ultron_s_client_gone_mid_run_leaves_the_run_going_and_the_next_client_sees_it() {
    let gates = tempfile::tempdir().unwrap();
    let gate = gates.path().join("gate");
    let (port, requests) = gated_provider(
        vec![text_reply("long answer"), text_reply("after answer")],
        gate.clone(),
    );
    let home = Home::new(port);
    let mut client = command(&home, &["--mode", "json", "long task"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let end = Instant::now() + HANG_GUARD;
    while requests.lock().unwrap().is_empty() {
        assert!(Instant::now() < end, "the run's request did not arrive");
        std::thread::sleep(Duration::from_millis(10));
    }
    // The client goes away (killed: no abort reaches the server) while its run waits on the provider.
    client.kill().unwrap();
    client.wait().unwrap();
    std::fs::write(&gate, "").unwrap();
    let id = home_id(&home);
    let path = session_file(&home, &id);
    let end = Instant::now() + HANG_GUARD;
    while !std::fs::read_to_string(&path).unwrap().contains("long answer") {
        assert!(Instant::now() < end, "the run did not finish without its client");
        std::thread::sleep(Duration::from_millis(20));
    }
    // The next client attaches to the same session and its turn follows the finished one.
    let next = json_run(&home, &["--mode", "json", "next"]);
    assert_eq!(
        (next["sessionId"].as_str(), next["text"].as_str()),
        (Some(id.as_str()), Some("after answer"))
    );
    let second = requests.lock().unwrap()[1].clone();
    assert!(second.to_string().contains("long answer"), "{second}");
    stop(&home);
}

#[test]
fn cli_ultron_s_herdr_resumes_through_ultron_s() {
    use cli::durable_client::home_resume_argv;
    assert_eq!(home_resume_argv(Some("h-1"), "h-1"), ["ultron-s"]);
    assert_eq!(home_resume_argv(Some("h-1"), "s-2"), ["ultron-s", "--session", "s-2"]);
    assert_eq!(home_resume_argv(None, "s-2"), ["ultron-s", "--session", "s-2"]);
}
