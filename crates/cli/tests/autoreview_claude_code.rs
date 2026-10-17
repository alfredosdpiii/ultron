//! `ultron autoreview review --repo-dir ...` with its frames on the Claude Code CLI, replayed from
//! crates/cli/fixtures/autoreview_claude_code.json (fixtures/autoreview_claude_code.mjs recorded the TypeScript
//! command over the same repository and the same fake `claude`, fixtures/fake_claude_review.py — never the real
//! CLI). With the fake the only `claude` on PATH and no model configured, the recommended
//! claude-code/claude-haiku-5-5 is chosen and resolves (it used to fail with `Model "claude-code/claude-haiku-5-5"
//! not found`); every frame is one `claude -p` request. Exit code, stdout, the command's stderr lines and every
//! request the fake served (model, effort, schema flag, output cap, prompts) must be equal.

use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const CALC: &str = "def total(items):
    \"\"\"Sum of item prices.\"\"\"
    result = 0
    for i in range(len(items)):
        result += items[i][\"price\"]
    return result


def average(items, count):
    if count == 0:
        return 0
    return total(items) / count
";

fn fixture() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/autoreview_claude_code.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// The kernel's interpreter (`ULTRON_PYTHON`, else /usr/bin/python3) also runs the fake CLI.
fn python() -> String {
    std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into())
}

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_AUTHOR_NAME", "Review Test")
        .env("GIT_AUTHOR_EMAIL", "review@test")
        .env("GIT_COMMITTER_NAME", "Review Test")
        .env("GIT_COMMITTER_EMAIL", "review@test")
        .env("GIT_AUTHOR_DATE", "2026-10-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-10-01T00:00:00Z")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

struct World {
    _tmp: tempfile::TempDir,
    work: PathBuf,
    repo: PathBuf,
    base: String,
    head: String,
}

impl World {
    fn new() -> World {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().canonicalize().unwrap();
        let repo = work.join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("calc.py"), CALC).unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "calc"]);
        let base = git(&repo, &["rev-parse", "HEAD"]);
        std::fs::write(
            repo.join("calc.py"),
            CALC.replace("range(len(items))", "range(len(items) - 1)"),
        )
        .unwrap();
        git(&repo, &["commit", "-qam", "tweak total"]);
        let head = git(&repo, &["rev-parse", "HEAD"]);
        let bin = work.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let fake = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/fake_claude_review.py");
        std::fs::write(
            bin.join("claude"),
            format!("#!/bin/sh\nexec {} {} \"$@\"\n", python(), fake.display()),
        )
        .unwrap();
        std::fs::set_permissions(bin.join("claude"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let agent = work.join("agent");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::create_dir_all(work.join("run")).unwrap();
        std::fs::write(agent.join("settings.json"), json!({"hindsightUrl": "off"}).to_string()).unwrap();
        World {
            _tmp: tmp,
            work,
            repo,
            base,
            head,
        }
    }

    fn log(&self) -> PathBuf {
        self.work.join("claude.log")
    }

    fn run(&self, args: &Value) -> (i32, String, String) {
        let args: Vec<String> = args
            .as_array()
            .unwrap()
            .iter()
            .map(|arg| match arg.as_str().unwrap() {
                "<repo>" => self.repo.display().to_string(),
                "<base>" => self.base.clone(),
                "<head>" => self.head.clone(),
                other => other.to_string(),
            })
            .collect();
        std::fs::write(self.log(), "").unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_ultron"));
        command
            .arg("autoreview")
            .args(&args)
            .current_dir(&self.work)
            .env_clear()
            .env("PATH", format!("{}:/usr/bin:/bin", self.work.join("bin").display()))
            .env("HOME", &self.work)
            .env("TZ", "UTC")
            .env("ULTRON_CODING_AGENT_DIR", self.work.join("agent"))
            .env("ULTRON_SERVER_DIR", self.work.join("server"))
            .env("XDG_RUNTIME_DIR", self.work.join("run"))
            .env("TMPDIR", self.work.join("run"))
            .env("ULTRON_LOKI", "off")
            .env("ULTRON_HINDSIGHT_URL", "off")
            .env("FAKE_CLAUDE_LOG", self.log())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1");
        if let Ok(python) = std::env::var("ULTRON_PYTHON") {
            command.env("ULTRON_PYTHON", python);
        }
        let output = command.output().unwrap();
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8(output.stdout).unwrap(),
            String::from_utf8(output.stderr).unwrap(),
        )
    }

    /// The fake's request log, normalized as the generator did, sorted.
    fn requests(&self) -> Vec<Value> {
        let ids = regex::Regex::new(r"(frame-|budget-)[0-9a-f-]{36}").unwrap();
        let work = self.work.display().to_string();
        let mut requests: Vec<Value> = std::fs::read_to_string(self.log())
            .unwrap()
            .lines()
            .filter(|l| !l.is_empty())
            .map(|line| {
                let line = ids.replace_all(line, "${1}ID").replace(&work, "<work>");
                serde_json::from_str(&line).unwrap()
            })
            .collect();
        requests.sort_by_key(|r| r.to_string());
        requests
    }
}

fn zero_durations(value: &mut Value) {
    match value {
        Value::Array(items) => items.iter_mut().for_each(zero_durations),
        Value::Object(map) => {
            for (key, item) in map.iter_mut() {
                if (key == "ms" || key.ends_with("Ms")) && item.is_number() {
                    *item = json!(0);
                } else {
                    zero_durations(item);
                }
            }
        }
        _ => {}
    }
}

fn shown(stdout: &str, work: &str) -> Value {
    match serde_json::from_str::<Value>(stdout) {
        Ok(mut json) if json.is_object() => {
            zero_durations(&mut json["timing"]);
            for key in ["frames", "investigators"] {
                json["timing"][key]
                    .as_array_mut()
                    .unwrap()
                    .sort_by_key(|v| v.to_string());
            }
            json
        }
        _ => {
            let text = stdout.replace(work, "<work>");
            let duration = regex::Regex::new(r"Reviewed in \d+(\.\d+)? s").unwrap();
            Value::String(duration.replace_all(&text, "Reviewed in N s").into_owned())
        }
    }
}

fn stderr_lines(stderr: &str, work: &str) -> Vec<String> {
    let stamp = regex::Regex::new(r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{3}Z ").unwrap();
    let ready = regex::Regex::new(r"engine ready in \d+ ms").unwrap();
    stderr
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let line = line.replace(work, "<work>");
            let line = stamp.replace(&line, "<time> ");
            ready.replace(&line, "engine ready in N ms").into_owned()
        })
        .collect()
}

fn assert_case(world: &World, name: &str) {
    let recorded = fixture();
    let expected = recorded["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == name)
        .unwrap();
    let (code, stdout, stderr) = world.run(&expected["args"]);
    let work = world.work.display().to_string();
    let stderr = stderr_lines(&stderr, &work);
    assert_eq!(
        code as i64,
        expected["code"].as_i64().unwrap(),
        "{name}: exit; stderr {stderr:?}"
    );
    assert_eq!(stderr, *expected["stderr"].as_array().unwrap(), "{name}: stderr");
    assert_eq!(shown(&stdout, &work), expected["stdout"], "{name}: stdout");
    let requests = world.requests();
    let wanted = expected["requests"].as_array().unwrap();
    assert_eq!(requests.len(), wanted.len(), "{name}: request count");
    for (index, (got, want)) in requests.iter().zip(wanted).enumerate() {
        for key in ["model", "effort", "schema", "maxTokens", "system", "text"] {
            assert_eq!(got[key], want[key], "{name}: request {index} {key}");
        }
    }
}

#[test]
fn cli_autoreview_claude_code_fixture_repository_matches() {
    let world = World::new();
    let recorded = fixture();
    assert_eq!(world.base, recorded["base"].as_str().unwrap());
    assert_eq!(world.head, recorded["head"].as_str().unwrap());
}

#[test]
fn cli_autoreview_claude_code_recommended_model_runs_on_the_cli() {
    let world = World::new();
    assert_case(&world, "recommended");
}

#[test]
fn cli_autoreview_claude_code_explicit_models_run_on_the_cli() {
    let world = World::new();
    assert_case(&world, "explicit");
}
