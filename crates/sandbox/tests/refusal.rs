//! `sandbox::refusal`: which failed-command output is the sandbox's refusal, the path it names, and the note, with
//! literal expectations over the error lines real tools print (coreutils, bash, dash, Python, Node, git, curl, ssh).
//! The host side is a fixed layout (`Fake`), so each rule is checked on its own: the policy keeps the path
//! read-only, the host could write it, the path is hidden and exists, the network is off.

use sandbox::launcher::Policy;
use sandbox::refusal::{HostPaths, Refusal, detect, note};
use std::path::{Path, PathBuf};

/// The host outside the sandbox: `/home/u` and below writable, `/usr` not; these files exist.
struct Fake;

impl HostPaths for Fake {
    fn writable(&self, path: &Path) -> bool {
        path.starts_with("/home/u") || path.starts_with("/srv/shared") || path.starts_with("/tmp")
    }
    fn exists(&self, path: &Path) -> bool {
        [
            "/home/u/.ssh/id_ed25519",
            "/home/u/.config/gh/hosts.yml",
            "/home/u/.ultron/session/notes",
        ]
        .iter()
        .any(|known| path == Path::new(known))
    }
}

fn policy(network: bool) -> Policy {
    Policy {
        cwd: "/home/u/project".into(),
        home: "/home/u".into(),
        writable: vec!["/home/u/project".into(), "/home/u/.ultron/session".into()],
        readable: Vec::new(),
        hidden: vec![
            "/home/u/.ssh".into(),
            "/home/u/.config/gh".into(),
            "/home/u/.ultron".into(),
        ],
        tmp: None,
        network,
        env: Vec::new(),
        image: None,
        uid: 1000,
        gid: 1000,
    }
}

fn read_only(path: &str) -> Option<Refusal> {
    Some(Refusal::ReadOnly(PathBuf::from(path)))
}

#[test]
fn sandbox_refusal_names_the_read_only_path_of_each_tools_message() {
    let cases = [
        ("bash: /home/u/notes.txt: Read-only file system", "/home/u/notes.txt"),
        (
            "/bin/bash: line 1: /home/u/notes.txt: Read-only file system",
            "/home/u/notes.txt",
        ),
        (
            "sh: 1: cannot create /home/u/out.txt: Read-only file system",
            "/home/u/out.txt",
        ),
        (
            "touch: cannot touch '/home/u/new.txt': Read-only file system",
            "/home/u/new.txt",
        ),
        (
            "mkdir: cannot create directory ‘/home/u/build’: Read-only file system",
            "/home/u/build",
        ),
        (
            "cp: cannot create regular file \"/home/u/copy\": Permission denied",
            "/home/u/copy",
        ),
        (
            "rm: cannot remove '/home/u/old.log': Read-only file system",
            "/home/u/old.log",
        ),
        (
            "OSError: [Errno 30] Read-only file system: '/home/u/data.json'",
            "/home/u/data.json",
        ),
        (
            "PermissionError: [Errno 13] Permission denied: '/home/u/x.py'",
            "/home/u/x.py",
        ),
        (
            "npm ERR! EROFS: read-only file system, open '/home/u/.npm-global/lib/x'",
            "/home/u/.npm-global/lib/x",
        ),
        (
            "error: could not lock config file /home/u/.gitconfig: Read-only file system",
            "/home/u/.gitconfig",
        ),
        (
            "chmod: changing permissions of '/home/u/run.sh': Operation not permitted",
            "/home/u/run.sh",
        ),
        // Relative to the project: `..` leaves it.
        ("bash: ../sibling/file: Read-only file system", "/home/u/sibling/file"),
        ("bash: ~/notes.txt: Read-only file system", "/home/u/notes.txt"),
    ];
    for (line, path) in cases {
        let output = format!("some output\n{line}\nmore");
        assert_eq!(detect(&policy(true), &output, &Fake), read_only(path), "{line}");
    }
}

#[test]
fn sandbox_refusal_is_claimed_only_when_the_sandbox_is_the_reason() {
    let none = [
        // Writable in the policy: the error came from elsewhere.
        "bash: /home/u/project/locked: Permission denied",
        "touch: cannot touch '/home/u/.ultron/session/x': Permission denied",
        // Read-only on the host too: the command fails the same way unsandboxed.
        "bash: /usr/local/bin/tool: Permission denied",
        "touch: cannot touch '/etc/hosts': Read-only file system",
        // The sandbox's private /tmp.
        "bash: /tmp/x: Permission denied",
        // A missing file that is not hidden; a hidden name that does not exist on the host.
        "cat: /home/u/missing.txt: No such file or directory",
        "cat: /home/u/.ssh/nothing: No such file or directory",
        // No path, or no refusal at all.
        "sudo: a password is required",
        "error: Permission denied",
        "grep: pattern not found",
    ];
    for line in none {
        assert_eq!(detect(&policy(true), line, &Fake), None, "{line}");
    }
    // Network errors only count when the network is off.
    assert_eq!(
        detect(&policy(true), "curl: (6) Could not resolve host: example.com", &Fake),
        None
    );
}

#[test]
fn sandbox_refusal_hidden_paths_and_the_most_specific_rule() {
    let hidden = |path: &str| Some(Refusal::Hidden(PathBuf::from(path)));
    assert_eq!(
        detect(
            &policy(true),
            "cat: /home/u/.ssh/id_ed25519: No such file or directory",
            &Fake
        ),
        hidden("/home/u/.ssh/id_ed25519")
    );
    assert_eq!(
        detect(
            &policy(true),
            "bash: ~/.config/gh/hosts.yml: Read-only file system",
            &Fake
        ),
        hidden("/home/u/.config/gh/hosts.yml")
    );
    // A writable dir nested in a hidden one wins there (the session dir under ~/.ultron).
    assert_eq!(
        detect(
            &policy(true),
            "cat: /home/u/.ultron/session/notes: No such file or directory",
            &Fake
        ),
        None
    );
}

#[test]
fn sandbox_refusal_network_messages_when_the_network_is_off() {
    let lines = [
        "curl: (6) Could not resolve host: example.com",
        "curl: (7) Failed to connect to 1.2.3.4 port 80 after 0 ms: Network is unreachable",
        "bash: connect: Network is unreachable",
        "fatal: unable to access 'https://github.com/a/b/': Could not resolve host: github.com",
        "ssh: Could not resolve hostname github.com: Temporary failure in name resolution",
        "wget: unable to resolve host address 'example.com'",
        "socket.gaierror: [Errno -2] Name or service not known",
        "npm ERR! request to https://registry.npmjs.org/x failed, reason: getaddrinfo EAI_AGAIN registry.npmjs.org",
        "OSError: [Errno 101] Network is unreachable",
    ];
    for line in lines {
        assert_eq!(detect(&policy(false), line, &Fake), Some(Refusal::Network), "{line}");
    }
    // The first refusal in output order is the one reported.
    let both = "bash: /home/u/a: Read-only file system\ncurl: (6) Could not resolve host: x";
    assert_eq!(detect(&policy(false), both, &Fake), read_only("/home/u/a"));
}

#[test]
fn sandbox_refusal_notes_say_what_is_allowed() {
    let p = policy(false);
    assert_eq!(
        note(&p, &Refusal::ReadOnly("/home/u/notes.txt".into())),
        "[sandbox: writing /home/u/notes.txt was blocked: writes outside the project are blocked by the sandbox. Writable: /home/u/project, /home/u/.ultron/session, /tmp; the sandbox.writable setting adds paths]"
    );
    assert_eq!(
        note(&p, &Refusal::Hidden("/home/u/.ssh/id_ed25519".into())),
        "[sandbox: /home/u/.ssh/id_ed25519 is hidden by the sandbox (credentials and sandbox.hidden paths are invisible to commands); it cannot be read or written from here]"
    );
    assert_eq!(
        note(&p, &Refusal::Network),
        "[sandbox: commands have no network access (sandbox.network is off); retrying will not help]"
    );
}

#[test]
fn sandbox_refusal_real_host_paths() {
    use sandbox::refusal::RealHostPaths;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::write(root.join("file"), "x").unwrap();
    assert!(RealHostPaths.writable(&root.join("file")));
    // A missing path is as writable as its nearest existing directory.
    assert!(RealHostPaths.writable(&root.join("missing/deeper")));
    assert!(RealHostPaths.exists(&root.join("file")));
    assert!(!RealHostPaths.exists(&root.join("missing")));
    // `/` is not writable by an ordinary user (tests never run as root): nor is a missing path right under it.
    assert!(!RealHostPaths.writable(Path::new("/ultron-refusal-test-missing/x")));
}
