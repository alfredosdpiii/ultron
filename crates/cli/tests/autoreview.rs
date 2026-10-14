//! `ultron autoreview` end to end: `review --repo-dir ... --json` (the offline entry, no GitHub access) against a local
//! stub chat-completions provider, replayed from crates/cli/fixtures/autoreview_offline.json, which
//! fixtures/autoreview_offline.mjs recorded from the TypeScript command over the same repository (same commits, fixed
//! identity and clock) and the same stub (the reply rules of ultron-autoreview-engine.test.ts, "the offline JSON
//! contract, with a stub provider"). Exit code, stdout, the command's stderr lines and every provider request (model,
//! messages, output cap) must be equal. Plus the literal expectations of the ported TypeScript tests.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

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
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/autoreview_offline.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
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
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

/// What the stub says, shared with the server thread.
#[derive(Default)]
struct Stub {
    rate_limited: u32,
    scenario_holds: bool,
    requests: Vec<String>,
    /// Each request's `Authorization` and `x-rev` headers.
    auth: Vec<(String, String)>,
}

fn verdict_for(body: &str, holds: bool) -> Value {
    if body.contains("wrong mean") {
        return json!({"verdict": "confirmed", "evidence": "`return total(items) / count` divides the truncated sum by the full count.", "corrected_line": null, "severity": "major", "scenario_holds": true});
    }
    if body.contains("skips the last item") {
        return json!({"verdict": "confirmed", "evidence": "`for i in range(len(items) - 1):` stops before the last index.", "corrected_line": null, "severity": "major", "scenario_holds": holds});
    }
    json!({"verdict": "rejected", "evidence": "`if count == 0:` returns first.", "corrected_line": null, "severity": "minor", "scenario_holds": false})
}

/// The reply rules of the TypeScript test, over the request's messages as one decoded text.
fn reply(body: &str, holds: bool) -> String {
    if body.contains("You check several findings of an automated pull request review") {
        let sections: Vec<Value> = body
            .split("=== Finding ")
            .skip(1)
            .enumerate()
            .map(|(index, section)| {
                let mut verdict = verdict_for(section, holds);
                verdict["finding"] = json!(index + 1);
                verdict
            })
            .collect();
        return Value::Array(sections).to_string();
    }
    if body.contains("You check one finding of an automated pull request review") {
        return verdict_for(body, holds).to_string();
    }
    if body.contains("You investigate one pull request beyond its diff") {
        if !body.contains("Your part: the claims") {
            return json!({"findings": [], "requests": [], "done": true}).to_string();
        }
        if !body.contains("Results of your requests, round 1") {
            return json!({"findings": [], "requests": [{"read": {"path": "calc.py", "start": 1, "end": 12}}, {"read": {"path": "../etc/passwd"}}], "done": false}).to_string();
        }
        return json!({
            "findings": [{"file": "calc.py", "line": 12, "severity": "major", "category": "correctness", "claim": "average() returns a wrong mean for every non-empty list.", "why": "average() divides the truncated total by the full count.", "scenario": "average([{'price': 2}, {'price': 4}], 2) returns 1.0; it should return 3.0.", "suggested_fix": "Fix total().", "confidence": 0.8, "evidence": [{"path": "calc.py", "line": 12, "quote": "return total(items) / count"}]}],
            "requests": [],
            "checked": ["average() guards count == 0 before dividing (calc.py)"],
            "done": true,
        })
        .to_string();
    }
    if body.contains("Your specialty: Correctness") && body.contains("File: calc.py") {
        return json!([
            {"file": "calc.py", "line": 4, "end_line": 4, "severity": "major", "category": "correctness", "claim": "total() skips the last item.", "why": "range(len(items) - 1) stops one short.", "suggested_fix": "Use range(len(items)).", "confidence": 0.9, "replacement": "    for i in range(len(items)):", "scenario": "total([{'price': 1}, {'price': 2}]) returns 1; it should return 3."},
            {"file": "calc.py", "line": 12, "end_line": 12, "severity": "major", "category": "correctness", "claim": "average() divides by zero.", "why": "count may be 0.", "suggested_fix": "Guard it.", "confidence": 0.6, "replacement": null, "scenario": "average([], 0) raises ZeroDivisionError."}
        ])
        .to_string();
    }
    "[]".into()
}

fn chunk(delta: Value, finish: Value, usage: Option<Value>) -> String {
    let mut event = json!({"id": "x", "object": "chat.completion.chunk", "created": 0, "model": "stub",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]});
    if let Some(usage) = usage {
        event["usage"] = usage;
    }
    format!("data: {event}\n\n")
}

fn serve(listener: TcpListener, stub: Arc<Mutex<Stub>>) {
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { return };
        let stub = stub.clone();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            let mut length = 0;
            let (mut authorization, mut rev) = (String::new(), String::new());
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                if header.trim().is_empty() {
                    break;
                }
                if let Some((key, value)) = header.split_once(':') {
                    if key.eq_ignore_ascii_case("content-length") {
                        length = value.trim().parse().unwrap();
                    } else if key.eq_ignore_ascii_case("authorization") {
                        authorization = value.trim().to_string();
                    } else if key.eq_ignore_ascii_case("x-rev") {
                        rev = value.trim().to_string();
                    }
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let body = String::from_utf8(body).unwrap();
            let (limited, holds) = {
                let mut stub = stub.lock().unwrap();
                stub.requests.push(body.clone());
                stub.auth.push((authorization, rev));
                let limited = stub.rate_limited > 0;
                if limited {
                    stub.rate_limited -= 1;
                }
                (limited, stub.scenario_holds)
            };
            let (status, kind, text) = if limited {
                (
                    "429 Too Many Requests",
                    "application/json\r\nretry-after: 1",
                    json!({"error": {"code": "1302", "message": "Rate limit reached for requests"}}).to_string(),
                )
            } else {
                let parsed: Value = serde_json::from_str(&body).unwrap();
                let decoded = parsed["messages"]
                    .to_string()
                    .replace("\\n", "\n")
                    .replace("\\\"", "\"");
                let content = reply(&decoded, holds);
                let text = chunk(json!({"role": "assistant", "content": content}), Value::Null, None)
                    + &chunk(
                        json!({}),
                        json!("stop"),
                        Some(json!({"prompt_tokens": 100, "completion_tokens": 20, "total_tokens": 120})),
                    )
                    + "data: [DONE]\n\n";
                ("200 OK", "text/event-stream", text)
            };
            let head = format!(
                "HTTP/1.1 {status}\r\ncontent-type: {kind}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                text.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(text.as_bytes());
        });
    }
}

struct World {
    _tmp: tempfile::TempDir,
    work: PathBuf,
    repo: PathBuf,
    base: String,
    head: String,
    stub: Arc<Mutex<Stub>>,
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
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let stub = Arc::new(Mutex::new(Stub {
            scenario_holds: true,
            ..Stub::default()
        }));
        let shared = stub.clone();
        std::thread::spawn(move || serve(listener, shared));
        let agent = work.join("agent");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::create_dir_all(work.join("run")).unwrap();
        std::fs::write(
            agent.join("models.json"),
            json!({"providers": {"groq": {"baseUrl": format!("http://127.0.0.1:{port}/v1")}}}).to_string(),
        )
        .unwrap();
        std::fs::write(
            agent.join("settings.json"),
            json!({"defaultProvider": "groq", "defaultModel": "llama-3.1-8b-instant", "hindsightUrl": "off"})
                .to_string(),
        )
        .unwrap();
        World {
            _tmp: tmp,
            work,
            repo,
            base,
            head,
            stub,
        }
    }

    fn args(&self, args: &Value) -> Vec<String> {
        args.as_array()
            .unwrap()
            .iter()
            .map(|arg| match arg.as_str().unwrap() {
                "<repo>" => self.repo.display().to_string(),
                "<base>" => self.base.clone(),
                "<head>" => self.head.clone(),
                other => other.to_string(),
            })
            .collect()
    }

    fn run(&self, args: &[String]) -> (i32, String, String) {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ultron"));
        command
            .arg("autoreview")
            .args(args)
            .current_dir(&self.work)
            .env_clear()
            // No `claude` on PATH: the recommended Claude Code model must not lead the fallback chain.
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &self.work)
            .env("TZ", "UTC")
            .env("ULTRON_CODING_AGENT_DIR", self.work.join("agent"))
            .env("ULTRON_SERVER_DIR", self.work.join("server"))
            .env("XDG_RUNTIME_DIR", self.work.join("run"))
            .env("ULTRON_LOKI", "off")
            .env("ULTRON_HINDSIGHT_URL", "off")
            .env("GROQ_API_KEY", "stub-key")
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

    fn set(&self, rate_limited: u32, holds: bool) {
        let mut stub = self.stub.lock().unwrap();
        stub.rate_limited = rate_limited;
        stub.scenario_holds = holds;
        stub.requests.clear();
        stub.auth.clear();
    }

    fn requests(&self) -> Vec<String> {
        self.stub.lock().unwrap().requests.clone()
    }

    fn work_text(&self) -> String {
        self.work.display().to_string()
    }
}

fn sort_by_json(items: &mut [Value]) {
    items.sort_by_key(|item| item.to_string());
}

/// Every duration (a key `ms` or ending in `Ms`) zeroed.
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

/// The JSON with every duration under `timing` zeroed and frame timings sorted (they finish in any order).
fn zero_times(mut json: Value) -> Value {
    zero_durations(&mut json["timing"]);
    for key in ["frames", "investigators"] {
        sort_by_json(json["timing"][key].as_array_mut().unwrap());
    }
    json
}

fn normalize_ids(text: &str) -> String {
    let pattern = regex::Regex::new(r"(frame-|budget-)[0-9a-f-]{36}").unwrap();
    pattern.replace_all(text, "${1}ID").into_owned()
}

fn request_record(body: &str, work: &str) -> Value {
    let parsed: Value = serde_json::from_str(body).unwrap();
    let text_of = |content: &Value| match content {
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect::<String>(),
        other => other.as_str().unwrap_or_default().to_string(),
    };
    let messages: Vec<Value> = parsed["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| json!({"role": m["role"], "content": normalize_ids(&text_of(&m["content"]))}))
        .collect();
    let cap = parsed
        .get("max_completion_tokens")
        .or_else(|| parsed.get("max_tokens"))
        .cloned()
        .unwrap_or(Value::Null);
    let record = json!({"model": parsed["model"], "max_completion_tokens": cap, "messages": messages});
    serde_json::from_str(&record.to_string().replace(work, "<work>")).unwrap()
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

/// Runs one recorded case; returns (code, stdout as recorded, stderr lines, requests, raw stdout).
fn run_case(world: &World, case: &Value) -> (i32, Value, Vec<String>, Vec<Value>, String) {
    world.set(
        case["rateLimited"].as_u64().unwrap() as u32,
        case["scenarioHolds"].as_bool().unwrap(),
    );
    let (code, stdout, stderr) = world.run(&world.args(&case["args"]));
    let work = world.work_text();
    let shown = match serde_json::from_str::<Value>(&stdout) {
        Ok(json) if json.is_object() => zero_times(json),
        _ => Value::String(zero_duration(&stdout.replace(&work, "<work>"))),
    };
    let mut requests: Vec<Value> = world
        .requests()
        .iter()
        .map(|body| request_record(body, &work))
        .collect();
    sort_by_json(&mut requests);
    (code, shown, stderr_lines(&stderr, &work), requests, stdout)
}

/// The JSON paths where two values differ (for a readable failure).
fn differences(a: &Value, b: &Value, path: &str) -> Vec<String> {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            let mut keys: Vec<&String> = x.keys().chain(y.keys()).collect();
            keys.sort();
            keys.dedup();
            keys.into_iter()
                .flat_map(|key| {
                    differences(
                        x.get(key).unwrap_or(&Value::Null),
                        y.get(key).unwrap_or(&Value::Null),
                        &format!("{path}.{key}"),
                    )
                })
                .collect()
        }
        (Value::Array(x), Value::Array(y)) if x.len() == y.len() => x
            .iter()
            .zip(y)
            .enumerate()
            .flat_map(|(index, (x, y))| differences(x, y, &format!("{path}[{index}]")))
            .collect(),
        _ if a == b => Vec::new(),
        _ => vec![format!("{path}: {a} != {b}")],
    }
}

/// The text review's duration line.
fn zero_duration(text: &str) -> String {
    regex::Regex::new(r"Reviewed in \d+(\.\d+)? s")
        .unwrap()
        .replace_all(text, "Reviewed in N s")
        .into_owned()
}

fn case<'a>(recorded: &'a Value, name: &str) -> &'a Value {
    recorded["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["name"] == name)
        .unwrap()
}

fn assert_case(world: &World, recorded: &Value, name: &str) -> (Value, String) {
    let mut expected = case(recorded, name).clone();
    if name == "rate_limited" {
        // Known difference: the TS frame lane's harness counts the retried request as one more call of the frame
        // (usage.frames 5, find 4); here the engine retries below the frame (review::engine::RetryingTransport), so
        // the inference ledger sees one call per frame. Everything else, the retry included, is equal.
        let usage = &mut expected["stdout"]["usage"];
        assert_eq!(usage["frames"], 5);
        usage["frames"] = json!(4);
        usage["byPhase"]["find"]["frames"] = json!(3);
    }
    let expected = &expected;
    let (code, stdout, stderr, requests, raw) = run_case(world, expected);
    assert_eq!(
        code as i64,
        expected["code"].as_i64().unwrap(),
        "{name}: exit code; stderr {stderr:?}"
    );
    assert_eq!(
        stdout,
        expected["stdout"],
        "{name}: stdout differs at {:?}",
        differences(&stdout, &expected["stdout"], "$")
    );
    assert_eq!(stderr, *expected["stderr"].as_array().unwrap(), "{name}: stderr");
    let mut wanted = expected["requests"].as_array().unwrap().clone();
    assert_eq!(requests.len(), wanted.len(), "{name}: request count");
    let mut requests = requests;
    if name == "rate_limited" {
        // Which finder request the stub refuses first depends on arrival order: compare the distinct requests.
        requests.dedup();
        wanted.dedup();
    }
    assert_eq!(requests.len(), wanted.len(), "{name}: distinct requests");
    for (index, (got, want)) in requests.iter().zip(&wanted).enumerate() {
        assert_eq!(got, want, "{name}: request {index}");
    }
    (stdout, raw)
}

#[test]
fn cli_autoreview_offline_fixture_repository_matches() {
    let world = World::new();
    let recorded = fixture();
    // The same commits as the recording: the fixed identity and clock make the shas equal.
    assert_eq!(world.base, recorded["base"].as_str().unwrap());
    assert_eq!(world.head, recorded["head"].as_str().unwrap());
}

/// "--help names the command, exits 0 and starts nothing".
#[test]
fn cli_autoreview_help_starts_nothing() {
    let world = World::new();
    let recorded = fixture();
    for name in ["help", "bare"] {
        let (stdout, _) = assert_case(&world, &recorded, name);
        assert!(stdout.as_str().unwrap().contains("ultron autoreview <command>"));
    }
    assert!(world.requests().is_empty());
    assert!(!world.work.join("agent").join("autoreview").exists());
}

/// "one JSON object on stdout with the agreed fields; logs on stderr; no root-model call".
#[test]
fn cli_autoreview_fast_json_contract() {
    let world = World::new();
    let recorded = fixture();
    let (json, raw) = assert_case(&world, &recorded, "fast");
    // Exactly one line, one object.
    assert_eq!(raw.trim().lines().count(), 1);
    let mut keys: Vec<&String> = json.as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "askModel",
            "askThinking",
            "assurance",
            "complete",
            "deepModel",
            "deepThinking",
            "dropped",
            "findings",
            "mode",
            "model",
            "notChecked",
            "planModel",
            "planStyle",
            "planThinking",
            "program",
            "tests",
            "thinking",
            "timing",
            "usage",
            "verdict",
            "verification",
            "verifyModel",
            "verifyThinking"
        ]
    );
    assert_eq!(json["program"], Value::Null);
    assert_eq!(json["thinking"], "medium");
    assert_eq!(json["verifyThinking"], "low");
    let mut frames: Vec<String> = json["timing"]["frames"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            format!(
                "{}:{}:{}",
                f["phase"].as_str().unwrap(),
                f["reviewer"].as_str().unwrap(),
                f["status"].as_str().unwrap()
            )
        })
        .collect();
    frames.sort();
    assert_eq!(
        frames,
        [
            "find:bugs:ok",
            "find:security:ok",
            "find:tests:ok",
            "verify:verifier:ok"
        ]
    );
    assert_eq!(json["verdict"], "request_changes");
    assert_eq!(json["complete"], true);
    assert_eq!(json["findings"].as_array().unwrap().len(), 1);
    assert_eq!(
        json["findings"][0]["howVerified"],
        "a verifier confirmed it against the source of calc.py"
    );
    assert_eq!(
        json["dropped"],
        json!({"rejected": 1, "duplicates": 0, "generic": 0, "refutedByTest": 0, "duplicateOf": []})
    );
    assert_eq!(json["model"], "groq/llama-3.1-8b-instant");
    assert_eq!(json["verifyModel"], "groq/llama-3.3-70b-versatile");
    assert_eq!(json["usage"]["frames"], 4);
    assert_eq!(
        json["usage"]["byPhase"],
        json!({"find": {"frames": 3, "tokens": 360}, "verify": {"frames": 1, "tokens": 120}})
    );
    assert_eq!(
        json["notChecked"],
        json!(["AI and LLM integration reviewer skipped 1 slice(s) with no LLM-related code."])
    );
    // Every request was a frame of the pipeline on the model asked for: nothing prompted a root model.
    let models: Vec<String> = world
        .requests()
        .iter()
        .map(|body| {
            serde_json::from_str::<Value>(body).unwrap()["model"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(models.iter().filter(|m| *m == "llama-3.1-8b-instant").count(), 3);
    assert_eq!(models.iter().filter(|m| *m == "llama-3.3-70b-versatile").count(), 1);
    for body in world.requests() {
        assert!(!body.contains("\"tools\""));
    }
    // Offline: nothing is written under the dry-run or log directories, and no state.
    let mut entries: Vec<String> = std::fs::read_dir(world.work.join("agent").join("autoreview"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    entries.sort();
    assert_eq!(entries, ["sessions", "work"]);
}

/// "a provider rate limit on a frame is retried; the review still completes".
#[test]
fn cli_autoreview_rate_limit_is_retried() {
    let world = World::new();
    let recorded = fixture();
    let (json, _) = assert_case(&world, &recorded, "rate_limited");
    assert_eq!(json["complete"], true);
    assert_eq!(json["findings"].as_array().unwrap().len(), 1);
    assert_eq!(json["thinking"], "low");
    assert!(
        json["timing"]["frames"]
            .as_array()
            .unwrap()
            .iter()
            .all(|f| f["status"] == "ok")
    );
    // Four frames (three finders, one batched verifier), and the one request that was refused sent again.
    assert_eq!(world.requests().len(), 5);
}

/// "the verdict follows the final severity: a major whose scenario does not hold is a minor comment, not a block".
#[test]
fn cli_autoreview_verdict_follows_final_severity() {
    let world = World::new();
    let recorded = fixture();
    let (json, _) = assert_case(&world, &recorded, "scenario_fails");
    assert_eq!(json["findings"][0]["finderSeverity"], "major");
    assert_eq!(json["findings"][0]["severity"], "minor");
    assert_eq!(json["findings"][0]["verification"], "confirmed");
    assert_eq!(json["verdict"], "approve");
}

/// Without --json, the review body the poster would post.
#[test]
fn cli_autoreview_text_is_the_review_body() {
    let world = World::new();
    let recorded = fixture();
    let (text, _) = assert_case(&world, &recorded, "text");
    assert!(text.as_str().unwrap().contains("**Request changes.**"));
}

/// "--mode both: the fast pass, then investigators with read-only lookups; one JSON object".
#[test]
fn cli_autoreview_both_mode_investigates() {
    let world = World::new();
    let recorded = fixture();
    let (json, raw) = assert_case(&world, &recorded, "both");
    assert_eq!(raw.trim().lines().count(), 1);
    assert_eq!(json["mode"], "both");
    assert_eq!(json["deepModel"], "groq/llama-3.1-8b-instant");
    assert_eq!(json["deepThinking"], "low");
    assert_eq!(json["verdict"], "request_changes");
    let places: Vec<Value> = json["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| json!([f["file"], f["line"], f["source"]]))
        .collect();
    assert_eq!(
        places,
        [json!(["calc.py", 4, "fast"]), json!(["calc.py", 12, "deep:claims"])]
    );
    assert_eq!(
        json["findings"][1]["evidence"],
        "calc.py:12 `return total(items) / count` | `return total(items) / count` divides the truncated sum by the full count."
    );
    assert!(json["assurance"].as_str().unwrap().contains("nothing executed."));
    let claims = json["timing"]["investigators"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["lens"] == "claims")
        .unwrap();
    assert_eq!(claims["rounds"], 2);
    assert_eq!(claims["requests"], 1);
    assert_eq!(claims["rejected"], 1);
    assert_eq!(claims["tokens"], 240);
    let second = world
        .requests()
        .into_iter()
        .find(|body| body.contains("Results of your requests, round 1"))
        .unwrap();
    assert!(second.contains("read calc.py:1-12 (of 12 lines)"));
    assert!(second.contains("leaves the repository"));
}

/// "a bad commit is an error on stderr, exit code 1, and no JSON"; usage errors exit 2.
#[test]
fn cli_autoreview_errors_and_usage() {
    let world = World::new();
    let recorded = fixture();
    let (stdout, _) = assert_case(&world, &recorded, "bad_commit");
    assert_eq!(stdout, "");
    for name in ["usage_error", "unknown_option"] {
        assert_case(&world, &recorded, name);
    }
}

/// "install writes a restarting user unit and prints how to enable it; uninstall removes it" (Linux).
#[test]
fn cli_autoreview_install_and_uninstall_service() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().canonicalize().unwrap();
    let run = |command: &str| {
        let output = Command::new(env!("CARGO_BIN_EXE_ultron"))
            .args(["autoreview", command])
            .current_dir(&home)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &home)
            .env("ULTRON_CODING_AGENT_DIR", home.join("agent"))
            .env("ULTRON_SERVER_DIR", home.join("server"))
            .env("ULTRON_SELF_COMMAND", "[\"/usr/bin/ultron\"]")
            .output()
            .unwrap();
        (output.status.code().unwrap(), String::from_utf8(output.stdout).unwrap())
    };
    let (code, out) = run("install");
    assert_eq!(code, 0);
    let unit = home.join(".config/systemd/user/ultron-autoreview.service");
    let text = std::fs::read_to_string(&unit).unwrap();
    assert!(text.contains("ExecStart=/usr/bin/ultron autoreview run"));
    assert!(text.contains("Restart=always"));
    assert!(text.contains("Environment=PATH=/usr/bin:/bin"));
    assert!(text.contains(&format!(
        "Environment=ULTRON_CODING_AGENT_DIR={}",
        home.join("agent").display()
    )));
    assert!(
        out.starts_with(&format!("Wrote {}\nModels:\n", unit.display())),
        "{out}"
    );
    assert!(out.contains("Not enabled."));
    assert!(out.contains("systemctl --user enable --now ultron-autoreview.service"));
    let (code, out) = run("uninstall");
    assert_eq!(code, 0);
    assert!(!unit.exists());
    assert!(out.contains("If it is still running:\n  systemctl --user disable --now ultron-autoreview.service"));
    let (code, out) = run("uninstall");
    assert_eq!(code, 0);
    assert_eq!(out, format!("No service file at {}\n", unit.display()));
}

/// The review engine resolves models of a models.json custom provider as sessions do: its frames call the provider's
/// URL with the model id, the configured key (`$VAR`) and the provider's headers.
#[test]
fn cli_autoreview_custom_provider_models_resolve() {
    let world = World::new();
    let models_path = world.work.join("agent/models.json");
    let groq: Value = serde_json::from_str(&std::fs::read_to_string(&models_path).unwrap()).unwrap();
    let url = groq["providers"]["groq"]["baseUrl"].as_str().unwrap().to_string();
    std::fs::write(
        &models_path,
        json!({"providers": {"revproxy": {
            "baseUrl": url, "api": "openai-completions", "apiKey": "$REV_KEY", "headers": {"x-rev": "yes"},
            "models": [{"id": "rev-1"}, {"id": "rev-verify"}],
        }}})
        .to_string(),
    )
    .unwrap();
    let args: Vec<String> = [
        "review",
        "--mode",
        "fast",
        "--repo-dir",
        "<repo>",
        "--base",
        "<base>",
        "--head",
        "<head>",
        "--model",
        "revproxy/rev-1",
        "--verify-model",
        "revproxy/rev-verify",
        "--json",
        "--dry-run",
    ]
    .iter()
    .map(|a| a.to_string())
    .collect();
    let args = world.args(&json!(args));
    // World::run's environment, with the custom provider's key instead of GROQ_API_KEY.
    let mut command = Command::new(env!("CARGO_BIN_EXE_ultron"));
    command
        .arg("autoreview")
        .args(&args)
        .current_dir(&world.work)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &world.work)
        .env("TZ", "UTC")
        .env("ULTRON_CODING_AGENT_DIR", world.work.join("agent"))
        .env("ULTRON_SERVER_DIR", world.work.join("server"))
        .env("XDG_RUNTIME_DIR", world.work.join("run"))
        .env("ULTRON_LOKI", "off")
        .env("ULTRON_HINDSIGHT_URL", "off")
        .env("REV_KEY", "rev-key-1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    if let Ok(python) = std::env::var("ULTRON_PYTHON") {
        command.env("ULTRON_PYTHON", python);
    }
    let output = command.output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "stderr: {stderr}");
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["model"], "revproxy/rev-1");
    let requests = world.requests();
    assert!(!requests.is_empty());
    let models: std::collections::BTreeSet<String> = requests
        .iter()
        .map(|body| {
            serde_json::from_str::<Value>(body).unwrap()["model"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert!(models.contains("rev-1"), "{models:?}");
    assert!(models.iter().all(|m| m == "rev-1" || m == "rev-verify"), "{models:?}");
    let auth = world.stub.lock().unwrap().auth.clone();
    assert!(
        auth.iter().all(|(a, rev)| a == "Bearer rev-key-1" && rev == "yes"),
        "{auth:?}"
    );
}
