//! The `shellPath` and `shellCommandPrefix` settings: replays crates/host/fixtures/shell.json (recorded from
//! utils/shell.ts `getShellConfig`, host-bash.ts `runHostBash` and bash-executor.ts `executeBashWithOperations`
//! over `createLocalBashOperations({ shellPath })` by fixtures/generate-shell.mjs) with the same fake shell, and
//! checks the resolution order and the shell jobs' shell.

use host::shell::{ShellConfig, prefixed_command, shell_config, system_shell_config};
use host::{AllowAll, Host};
use kernel::{HostHandler, Signal};
use serde_json::{Map, Value, json};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/shell.json")).unwrap()
}

struct Dir {
    _tmp: tempfile::TempDir,
    path: PathBuf,
}

impl Dir {
    /// A temp dir with the fixture's fake shell: logs each argument as `[arg]`, then runs /bin/bash with them.
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let path = std::fs::canonicalize(tmp.path()).unwrap();
        let fake = path.join("fake-shell");
        let log = path.join("argv.log");
        std::fs::write(
            &fake,
            format!(
                "#!/bin/bash\nfor a in \"$@\"; do printf '[%s]\\n' \"$a\"; done >> \"{}\"\nexec /bin/bash \"$@\"\n",
                log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        Dir { _tmp: tmp, path }
    }
    fn abs(&self, recorded: &str) -> String {
        recorded.replace("<dir>", &self.path.to_string_lossy())
    }
    fn rel(&self, text: &str) -> String {
        text.replace(&*self.path.to_string_lossy(), "<dir>")
    }
    fn take_log(&self) -> String {
        let log = self.path.join("argv.log");
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        let _ = std::fs::write(&log, "");
        self.rel(&text)
    }
    fn host(&self, shell_path: Option<String>) -> Host {
        Host::new(&self.path, self.path.join(".jobs"), &self.path, Box::new(AllowAll)).with_shell_path(shell_path)
    }
}

fn bash(host: &Host, command: &str) -> Result<Value, String> {
    let mut payload = Map::new();
    payload.insert("command".into(), command.into());
    host.handle("bash", payload, &Signal::new())
}

#[test]
fn host_shell_config_matches_ts_get_shell_config() {
    let dir = Dir::new();
    for case in fixture()["configs"].as_array().unwrap() {
        let custom = case["custom"].as_str().map(|c| dir.abs(c));
        let got = match system_shell_config(custom.as_deref()) {
            Ok(ShellConfig { shell, args }) => json!({ "value": { "shell": dir.rel(&shell), "args": args } }),
            Err(error) => json!({ "error": dir.rel(&error) }),
        };
        assert_eq!(got, case["result"], "{custom:?}");
    }
}

/// `getShellConfig`'s order on Unix: the custom path, `/bin/bash`, `bash` on PATH, then `sh`.
#[test]
fn host_shell_config_falls_back_from_bin_bash_to_path_to_sh() {
    let nothing = |_: &Path| false;
    let only_bin_bash = |p: &Path| p == Path::new("/bin/bash");
    let on_path = |name: &str| (name == "bash").then(|| "/opt/tools/bash".to_string());
    let nowhere = |_: &str| None;
    let shell = |c: Result<ShellConfig, String>| c.unwrap().shell;
    assert_eq!(shell(shell_config(None, &only_bin_bash, &on_path)), "/bin/bash");
    assert_eq!(shell(shell_config(None, &nothing, &on_path)), "/opt/tools/bash");
    assert_eq!(
        shell_config(None, &nothing, &nowhere).unwrap(),
        ShellConfig {
            shell: "sh".into(),
            args: vec!["-c".into()]
        }
    );
    assert_eq!(shell(shell_config(Some(""), &only_bin_bash, &on_path)), "/bin/bash");
    assert_eq!(
        shell_config(Some("/x/zsh"), &only_bin_bash, &on_path),
        Err("Custom shell path not found: /x/zsh".into())
    );
    let any = |_: &Path| true;
    assert_eq!(shell(shell_config(Some("/x/zsh"), &any, &on_path)), "/x/zsh");
}

#[test]
fn host_shell_path_runs_the_repl_bash_like_ts() {
    let dir = Dir::new();
    for case in fixture()["host"].as_array().unwrap() {
        let host = dir.host(Some(dir.abs(case["shellPath"].as_str().unwrap())));
        let got = match bash(&host, case["command"].as_str().unwrap()) {
            Ok(r) => {
                json!({ "value": { "output": dir.rel(r["output"].as_str().unwrap()), "exit_code": r["exit_code"] } })
            }
            Err(error) => json!({ "error": dir.rel(&error) }),
        };
        assert_eq!(got, case["result"], "{}", case["command"]);
        assert_eq!(dir.take_log(), case["argv"].as_str().unwrap(), "{}", case["command"]);
    }
}

#[test]
fn host_shell_command_prefix_composes_the_user_command_like_ts() {
    let dir = Dir::new();
    let host = dir.host(Some(dir.path.join("fake-shell").to_string_lossy().into_owned()));
    for case in fixture()["user"].as_array().unwrap() {
        let command = prefixed_command(case["prefix"].as_str(), case["command"].as_str().unwrap());
        let r = bash(&host, &command).unwrap();
        assert_eq!(
            json!({ "value": { "output": r["output"], "exitCode": r["exit_code"] } }),
            case["result"],
            "{}",
            case["command"]
        );
        assert_eq!(dir.take_log(), case["argv"].as_str().unwrap(), "{}", case["command"]);
    }
}

/// Shell jobs (`bash(cmd, yield_after=...)`) run under the same shell (`ShellJobs` takes the session's
/// `createLocalBashOperations({ shellPath })`); a missing shell fails the job with getShellConfig's error.
#[test]
fn host_shell_path_runs_shell_jobs_too() {
    let dir = Dir::new();
    let call = |host: &Host, kind: &str, payload: Value| {
        let Value::Object(payload) = payload else {
            unreachable!()
        };
        host.handle(kind, payload, &Signal::new())
    };
    let host = dir.host(Some(dir.path.join("fake-shell").to_string_lossy().into_owned()));
    let started = call(
        &host,
        "shell.run",
        json!({ "command": "echo job-out", "yield_after": 0 }),
    )
    .unwrap();
    let id = started["id"].as_str().unwrap().to_string();
    let deadline = Instant::now() + Duration::from_secs(120);
    let result = loop {
        let r = call(&host, "shell.result", json!({ "id": id })).unwrap();
        if r["status"] != "running" {
            break r;
        }
        assert!(Instant::now() < deadline, "hang guard");
        std::thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(result["status"], "completed", "{result}");
    assert_eq!(result["exit_code"], 0, "{result}");
    assert_eq!(dir.take_log(), "[-c]\n[echo job-out]\n");

    let missing = dir.host(Some(dir.path.join("missing-shell").to_string_lossy().into_owned()));
    let failed = call(
        &missing,
        "shell.run",
        json!({ "command": "echo never", "yield_after": 0 }),
    )
    .unwrap();
    let id = failed["id"].as_str().unwrap().to_string();
    let r = call(&missing, "shell.result", json!({ "id": id }));
    let text = format!("{r:?}");
    assert!(
        text.contains("Custom shell path not found: ") && text.contains("missing-shell"),
        "{text}"
    );
}
