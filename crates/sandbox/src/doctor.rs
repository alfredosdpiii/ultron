//! `doctor`: check that isolation actually holds, like `ultron autoreview doctor` (autoreview_tests.py `self_check`).
//!
//! A probe runs under the session's policy with a scratch project and tries what a command must not do: write to
//! the home, to a directory outside the writable ones and to /usr; see a file in the host's /tmp; read a hidden path;
//! see a variable the launcher had; and, with the network configured off, connect to a listener the doctor opened on
//! the host's loopback (no outside traffic). What counts is what the host sees afterwards, not what the probe reports.

use crate::launcher::{Launcher, Mechanism, Policy};
use crate::settings::Mode;
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const PROBE: &str = r#"out() { printf 'PROBE %s=%s\n' "$1" "$2"; }
name=$1; project=$2; home=$3; outside=$4; host_tmp=$5; port=$6; shift 6
( : > "$project/$name" ) 2>/dev/null && out project writable || out project read-only
( : > "$home/$name" ) 2>/dev/null && out home writable || out home read-only
( : > "$outside/$name" ) 2>/dev/null && out outside writable || out outside read-only
( : > "/usr/$name" ) 2>/dev/null && out system writable || out system read-only
if [ -e "$host_tmp" ]; then out hostTmp visible; else out hostTmp hidden; fi
if ( exec 3<>"/dev/tcp/127.0.0.1/$port" ) 2>/dev/null; then out network reachable; else out network unreachable; fi
if [ -n "${ULTRON_SANDBOX_CANARY_TOKEN:-}" ]; then out token present; else out token absent; fi
exposed=
for path in "$@"; do
  if [ -d "$path" ]; then [ -z "$(ls -A "$path" 2>/dev/null)" ] || exposed="$exposed $path"
  elif [ -s "$path" ]; then exposed="$exposed $path"; fi
done
out exposed "${exposed# }"
"#;

fn hex() -> String {
    let mut buffer = [0u8; 6];
    getrandom::fill(&mut buffer).expect("system randomness");
    buffer.iter().map(|b| format!("{b:02x}")).collect()
}

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Run the isolation probe for `policy` under `mechanism` (None: unisolated, to show what the checks catch).
/// `scratch_parent` holds the scratch project and outside directories (the system temp dir when None).
pub fn self_check(policy: &Policy, mechanism: Option<Mechanism>, scratch_parent: Option<&Path>) -> Value {
    let name = format!(".ultron-sandbox-probe-{}", hex());
    let parent = scratch_parent.map_or_else(std::env::temp_dir, Path::to_path_buf);
    let scratch = Scratch(parent.join(format!("ultron-sandbox-doctor-{}", hex())));
    let project = scratch.0.join("project");
    let outside = scratch.0.join("outside");
    for dir in [&project, &outside] {
        if let Err(error) = std::fs::create_dir_all(dir) {
            return json!({ "ok": false, "error": format!("could not create {}: {error}", dir.display()) });
        }
    }
    let project = std::fs::canonicalize(&project).unwrap_or(project);
    let outside = std::fs::canonicalize(&outside).unwrap_or(outside);
    let host_tmp = std::env::temp_dir().join(format!("ultron-sandbox-host-{}", hex()));
    let _ = std::fs::write(&host_tmp, "host\n");
    let listener = std::net::TcpListener::bind("127.0.0.1:0");
    let port = listener
        .as_ref()
        .map(|l| l.local_addr().map(|a| a.port()).unwrap_or(0))
        .unwrap_or(0);

    let mut policy = policy.clone();
    policy.cwd = project.clone();
    match policy.writable.first_mut() {
        Some(first) => *first = project.clone(),
        None => policy.writable.push(project.clone()),
    }
    let mut argv: Vec<String> = vec!["bash".into(), "-c".into(), PROBE.into(), "probe".into(), name.clone()];
    argv.extend([&project, &policy.home, &outside, &host_tmp].map(|p| p.to_string_lossy().into_owned()));
    argv.push(port.to_string());
    argv.extend(policy.hidden.iter().map(|p| p.to_string_lossy().into_owned()));
    let mut command = match mechanism {
        Some(mechanism) => policy.command(mechanism, &argv, &project),
        None => {
            let mut command = Command::new(&argv[0]);
            command.args(&argv[1..]);
            command
        }
    };
    command
        .env("ULTRON_SANDBOX_CANARY_TOKEN", "not-a-real-token")
        .current_dir(&project)
        .stdin(Stdio::null());
    let output = command.output();
    drop(listener);
    let _ = std::fs::remove_file(&host_tmp);
    let landed = |dir: &Path| {
        let path = dir.join(&name);
        let exists = path.exists();
        let _ = std::fs::remove_file(&path);
        exists
    };
    let (project_landed, home_landed, outside_landed) = (landed(&project), landed(&policy.home), landed(&outside));
    let system_landed = landed(Path::new("/usr"));
    let output = match output {
        Ok(output) => output,
        Err(error) => return json!({ "ok": false, "error": format!("the probe did not start: {error}") }),
    };
    let text = String::from_utf8_lossy(&output.stdout).into_owned() + &String::from_utf8_lossy(&output.stderr);
    let mut seen = Map::new();
    for line in text.lines() {
        if let Some((key, value)) = line.strip_prefix("PROBE ").and_then(|l| l.split_once('=')) {
            seen.insert(key.to_owned(), json!(value));
        }
    }
    if !seen.contains_key("exposed") {
        let tail: String = text
            .chars()
            .rev()
            .take(300)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        return json!({ "ok": false, "error": format!("the probe did not run (exit {:?}): {tail}", output.status.code()) });
    }
    let field = |key: &str| seen.get(key).and_then(Value::as_str).unwrap_or_default().to_owned();
    let exposed: Vec<String> = field("exposed").split_whitespace().map(str::to_owned).collect();
    let network_ok = policy.network || field("network") == "unreachable";
    let ok = project_landed
        && !home_landed
        && !outside_landed
        && !system_landed
        && field("hostTmp") == "hidden"
        && field("token") == "absent"
        && exposed.is_empty()
        && network_ok;
    json!({
        "ok": ok,
        "project": if project_landed { "writable" } else { "not writable" },
        "home": if home_landed { "writable" } else { "read-only" },
        "outside": if outside_landed { "writable" } else { "read-only" },
        "system": if system_landed { "writable" } else { "read-only" },
        "hostTmp": field("hostTmp"),
        "network": field("network"),
        "networkExpected": if policy.network { "on" } else { "off" },
        "token": field("token"),
        "exposed": exposed,
    })
}

/// The doctor report of a session's launcher: mode, mechanism, what it isolates, and the self-check.
pub fn doctor(launcher: &Launcher) -> Value {
    let mechanism = launcher.mechanism();
    let mut report = json!({
        "mode": launcher.mode.name(),
        "mechanism": mechanism.map(Mechanism::name),
        "isolation": launcher.describe(),
    });
    if launcher.mode == Mode::Off {
        report["ok"] = json!(true);
        report["message"] = json!("sandbox.mode is off: commands run without isolation");
        return report;
    }
    if mechanism.is_none() {
        report["ok"] = json!(false);
        report["message"] = json!(if launcher.mode == Mode::Required {
            "commands are refused: no sandbox works here (install bubblewrap, allow unprivileged user namespaces, or set sandbox.image for Docker)"
        } else {
            "commands run without isolation: no sandbox works here (install bubblewrap, allow unprivileged user namespaces, or set sandbox.image for Docker)"
        });
        return report;
    }
    let check = self_check(&launcher.policy, mechanism, None);
    report["ok"] = check["ok"].clone();
    report["selfCheck"] = check;
    report
}
