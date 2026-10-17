//! The skills `ultron guide --for claude` and `ultron claude --print-config` load (crates/claude-root `guide`, over
//! session-core's session resource resolver), replayed against crates/claude-root/fixtures/resources.json
//! (generate-resources.mjs: the TypeScript `loadClaudePromptResources`, i.e. the session worker's
//! DefaultResourceLoader): settings `skills` entries (absolute, relative, exclusion patterns, project settings),
//! `~/.agents/skills`, project `.agents/skills` and `.pi/skills`, a user skill shadowing a bundled one, a distrusted
//! project, `ULTRON_BUNDLED_SKILLS=off` and `-ns`. Each case runs in a private temp root with a FAKE `claude` first on
//! PATH; stdout, stderr and the exit code must equal the recording after the generator's normalization.

mod executable_support;

use regex::Regex;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::LazyLock;

const BIN: &str = env!("CARGO_BIN_EXE_ultron");

fn repo() -> PathBuf {
    std::fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")).unwrap()
}

fn python() -> String {
    std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into())
}

struct Root {
    dir: tempfile::TempDir,
}

impl Root {
    fn new() -> Root {
        let dir = tempfile::Builder::new()
            .prefix("ucr-")
            .rand_bytes(6)
            .tempdir_in("/tmp")
            .unwrap();
        for sub in ["h", "a", "s", "r", "w", "bin", "tmp"] {
            std::fs::create_dir(dir.path().join(sub)).unwrap();
        }
        let fake = std::fs::read_to_string(repo().join("crates/claude-root/fixtures/fake_claude.py")).unwrap();
        executable_support::install(&dir.path().join("bin/claude"), &format!("#!{} -I\n{fake}", python()));
        Root { dir }
    }

    fn sub(&self, text: &str) -> String {
        text.replace("<T>", &self.dir.path().display().to_string())
    }

    fn normalize(&self, text: &str) -> String {
        static DATE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"- Date: \d{4}-\d{2}-\d{2}").unwrap());
        let text = text
            .replace(&self.dir.path().display().to_string(), "<T>")
            .replace(&repo().display().to_string(), "<REPO>");
        DATE.replace_all(&text, "- Date: <DATE>").into_owned()
    }

    fn run(&self, case: &Value, args: &[String]) -> (String, String, Option<i32>) {
        let p = |s: &str| self.dir.path().join(s).display().to_string();
        let mut command = Command::new(BIN);
        command
            .args(args.iter().map(|a| self.sub(a)))
            .current_dir(self.dir.path().join("w"))
            .env_clear()
            .env("PATH", format!("{}:/usr/bin:/bin", p("bin")))
            .env("HOME", p("h"))
            .env("ULTRON_CODING_AGENT_DIR", p("a"))
            .env("ULTRON_SERVER_DIR", p("s"))
            .env("XDG_RUNTIME_DIR", p("r"))
            .env("TMPDIR", p("tmp"))
            .env("ULTRON_SELF_COMMAND", r#"["/opt/ultron/bin/ultron"]"#)
            .env("ULTRON_LOKI", "off")
            .env("ULTRON_PYTHON", python())
            .env("ULTRON_OFFLINE", "1")
            .env("FAKE_CLAUDE_OUT", p("launch.json"))
            .env("LANG", "C.UTF-8")
            .stdin(Stdio::null());
        for (key, value) in case.get("env").and_then(Value::as_object).into_iter().flatten() {
            command.env(key, self.sub(value.as_str().unwrap()));
        }
        let output = command.output().unwrap();
        (
            self.normalize(&String::from_utf8_lossy(&output.stdout)),
            self.normalize(&String::from_utf8_lossy(&output.stderr)),
            output.status.code(),
        )
    }
}

/// The guide from its `<skills>` section on (to the `# Environment` section and through it): what the resources
/// render. The runtime guide before it is checked against claude.json, whose wording this build changed on purpose
/// (CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS, worktree commits).
fn resources_part(text: &str) -> &str {
    let at = text
        .find("\n\n<skills>")
        .or_else(|| text.find("\n\n# Environment"))
        .unwrap_or(text.len());
    &text[at..]
}

/// The first line where `got` leaves `want`, with both lines.
fn first_difference(want: &str, got: &str) -> String {
    let (want_lines, got_lines): (Vec<&str>, Vec<&str>) = (want.lines().collect(), got.lines().collect());
    let at = (0..want_lines.len().max(got_lines.len()))
        .find(|i| want_lines.get(*i) != got_lines.get(*i))
        .unwrap_or(0);
    format!(
        "line {}:\n  want {:?}\n  got  {:?}",
        at + 1,
        want_lines.get(at),
        got_lines.get(at)
    )
}

fn run_case(case: &Value) -> Vec<String> {
    let name = case["name"].as_str().unwrap();
    let extra: Vec<String> = case
        .get("extra")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    let mut diffs = Vec::new();
    let place = |root: &Root| {
        for (path, content) in case["files"].as_object().unwrap() {
            let target = root.dir.path().join(path);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::write(target, root.sub(content.as_str().unwrap())).unwrap();
        }
    };
    let guide_args: Vec<String> = ["guide", "--for", "claude", "--cwd", "<T>/w", "-nc"]
        .iter()
        .map(|s| s.to_string())
        .chain(extra.iter().cloned())
        .collect();
    let root = Root::new();
    place(&root);
    let (stdout, stderr, code) = root.run(case, &guide_args);
    let want = &case["guide"];
    let want_text = resources_part(want["stdout"].as_str().unwrap_or(""));
    if want_text != resources_part(&stdout) {
        diffs.push(format!(
            "{name} guide stdout: {}",
            first_difference(want_text, resources_part(&stdout))
        ));
    }
    if want["stderr"].as_str() != Some(stderr.as_str()) {
        diffs.push(format!("{name} guide stderr: want {} got {stderr:?}", want["stderr"]));
    }
    if want["code"].as_i64().map(|c| c as i32) != code {
        diffs.push(format!("{name} guide code: want {} got {code:?}", want["code"]));
    }
    let print_args: Vec<String> = ["claude", "--print-config", "-nc"]
        .iter()
        .map(|s| s.to_string())
        .chain(extra.iter().cloned())
        .collect();
    let root = Root::new();
    place(&root);
    let (stdout, stderr, code) = root.run(case, &print_args);
    let want = &case["printConfig"];
    let got = serde_json::from_str::<Value>(&stdout).ok().map(|printed| {
        serde_json::json!({
            "contextFiles": printed["systemPrompt"]["contextFiles"],
            "skills": printed["systemPrompt"]["skills"],
        })
    });
    if got.as_ref() != Some(&want["resources"]) {
        diffs.push(format!(
            "{name} print-config resources: want {} got {got:?}",
            want["resources"]
        ));
    }
    if want["stderr"].as_str() != Some(stderr.as_str()) || want["code"].as_i64().map(|c| c as i32) != code {
        diffs.push(format!("{name} print-config: stderr {stderr:?} code {code:?}"));
    }
    diffs
}

#[test]
fn cli_claude_root_resources_match_the_session_resource_loader() {
    let fixture: Value = serde_json::from_str(
        &std::fs::read_to_string(repo().join("crates/claude-root/fixtures/resources.json")).unwrap(),
    )
    .unwrap();
    let cases = fixture["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 9);
    let diffs: Vec<String> = std::thread::scope(|scope| {
        let handles: Vec<_> = cases.iter().map(|case| scope.spawn(|| run_case(case))).collect();
        handles.into_iter().flat_map(|h| h.join().unwrap()).collect()
    });
    assert!(diffs.is_empty(), "{} differences:\n{}", diffs.len(), diffs.join("\n\n"));
}
