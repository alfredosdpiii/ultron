//! Replays crates/review/fixtures/scope.json, recorded from the Python /review implementation
//! (fixtures/record_scope.py): chunking, find plans, verifier views, the report, scope resolution over local git
//! repositories rebuilt here from the recorded steps (gh scripted, git real), related code, save and post.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;

use review::args::parse_args;
use review::chunks::{CHUNK_CHARS, CONTEXT_LINES, Chunk, applies, build_chunks, plan_find, plan_report};
use review::diff::{FileDiff, Hunk, added_file, parse_diff};
use review::findings::Finding;
use review::prompt_text::{REVIEWERS, Reviewer};
use review::prompts::reviewer;
use review::report::{ReportInput, Stats, post, render_report, save};
use review::scope::{Git, GrepIn, Output, ProcessRunner, Runner, related_code, resolve_scope};
use review::views::{hunk_for, source_window, symbols_for};
use serde_json::{Value, json};

fn fixture() -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/scope.json");
    serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("fixture json")
}

fn hunk_json(hunk: &Hunk) -> Value {
    json!({
        "old_start": hunk.old_start, "old_len": hunk.old_len, "new_start": hunk.new_start, "new_len": hunk.new_len,
        "header": hunk.header,
        "lines": hunk.lines.iter().map(|l| json!([l.kind.to_string(), l.old, l.new, l.text])).collect::<Vec<_>>(),
    })
}

fn file_json(file: &FileDiff) -> Value {
    json!({ "path": file.path, "old_path": file.old_path, "status": file.status, "binary": file.binary,
            "hunks": file.hunks.iter().map(hunk_json).collect::<Vec<_>>() })
}

fn chunk_json(chunk: &Chunk) -> Value {
    json!({ "id": chunk.id, "path": chunk.path, "kind": chunk.kind, "status": chunk.status, "part": chunk.part,
            "parts": chunk.parts, "text": chunk.text, "hunks": chunk.hunks.iter().map(hunk_json).collect::<Vec<_>>() })
}

fn lines(value: &Value) -> Option<Vec<String>> {
    serde_json::from_value(value.clone()).ok()
}

fn reader(sources: &Value) -> impl FnMut(&str) -> Option<Vec<String>> + '_ {
    move |path| lines(&sources[path])
}

fn skipped_json(skipped: &[(String, String)]) -> Value {
    json!(
        skipped
            .iter()
            .map(|(path, reason)| json!([path, reason]))
            .collect::<Vec<_>>()
    )
}

#[test]
fn review_build_chunks_matches_python() {
    let fx = fixture();
    for case in fx["chunks"].as_array().expect("chunks") {
        let max_chars = case["max_chars"].as_u64().expect("max") as usize;
        let context = case["context"].as_i64().expect("context");
        let (chunks, skipped) = if let Some(index) = case["diff"].as_u64() {
            let files = parse_diff(fx["chunk_diffs"][index as usize].as_str().expect("diff"));
            build_chunks(&files, reader(&fx["src"]), max_chars, context)
        } else {
            let many: Vec<FileDiff> = (0..34).map(|i| added_file(&format!("f{i:02}.py"), "x = 1\n")).collect();
            let files: Vec<FileDiff> = many[..2].iter().chain(&many).cloned().collect();
            let (chunks, skipped) = build_chunks(&files, |_| None, max_chars, context);
            (chunks.into_iter().take(3).collect(), skipped)
        };
        let got: Vec<Value> = chunks.iter().map(chunk_json).collect();
        assert_eq!(
            Value::Array(got),
            case["chunks"],
            "{max_chars} {context} {}",
            case["diff"]
        );
        assert_eq!(skipped_json(&skipped), case["skipped"]);
    }
    let files = parse_diff(fx["chunk_diffs"][0].as_str().expect("diff"));
    for case in fx["sweep"].as_array().expect("sweep") {
        let max_chars = case[0].as_u64().expect("max") as usize;
        let (chunks, _) = build_chunks(&files, reader(&fx["src"]), max_chars, 2);
        let got: Vec<Value> = chunks.iter().map(|c| json!([c.part, c.parts, c.hunks.len()])).collect();
        assert_eq!(Value::Array(got), case[1], "{max_chars}");
    }
}

fn reviewers_of(keys: &Value) -> Vec<&'static Reviewer> {
    keys.as_array()
        .expect("keys")
        .iter()
        .map(|key| reviewer(key.as_str().expect("key")).expect("reviewer"))
        .collect()
}

#[test]
fn review_plan_find_and_plan_report_match_python() {
    let fx = fixture();
    let files = parse_diff(fx["chunk_diffs"][0].as_str().expect("diff"));
    let (chunks, _) = build_chunks(&files, reader(&fx["src"]), CHUNK_CHARS, CONTEXT_LINES);
    let wide = build_chunks(
        &[added_file("wide.py", &"café ünïcödé\n".repeat(200))],
        |_| None,
        CHUNK_CHARS,
        CONTEXT_LINES,
    )
    .0;
    for case in fx["plans"].as_array().expect("plans") {
        let budget = case["budget"].as_u64().expect("budget");
        let chunks = if case["wide"] == true { &wide } else { &chunks };
        let plan = plan_find(chunks, &reviewers_of(&case["reviewers"]), budget);
        let pairs = |frames: &[(&Reviewer, usize)]| -> Value {
            json!(
                frames
                    .iter()
                    .map(|(r, i)| json!([r.key, chunks[*i].id]))
                    .collect::<Vec<_>>()
            )
        };
        let got = json!({ "frames": pairs(&plan.frames), "dropped": pairs(&plan.dropped),
                          "not_applicable": plan.not_applicable, "estimate": plan.estimate });
        assert_eq!(got, case["plan"], "{}", case["reviewers"]);
        let skipped = [("yarn.lock".to_string(), "lockfile".to_string())];
        let (label, skipped): (&str, &[(String, String)]) = if case["wide"] == true {
            ("wide", &[])
        } else {
            ("branch x", &skipped)
        };
        assert_eq!(
            plan_report(label, chunks, &plan, skipped, budget),
            case["report"].as_str().expect("report")
        );
    }
    for case in fx["applies"].as_array().expect("applies") {
        let chunk = Chunk {
            id: 1,
            path: "x".to_string(),
            kind: if case[1] == "doc" { "doc" } else { "code" },
            status: review::diff::Status::Modified,
            hunks: Vec::new(),
            text: case[0].as_str().expect("text").to_string(),
            part: 1,
            parts: 1,
        };
        for reviewer in REVIEWERS {
            assert_eq!(json!(applies(reviewer, &chunk)), case[2][reviewer.key], "{case}");
        }
    }
}

#[test]
fn review_verifier_views_match_python() {
    let fx = fixture();
    let source = lines(&fx["view_src"]).expect("source");
    for case in fx["symbols"].as_array().expect("symbols") {
        let (line, limit) = (
            case[0].as_i64().expect("line"),
            case[1].as_u64().expect("limit") as usize,
        );
        let got = match line {
            5 if case[2] == json!([]) => symbols_for(None, line, limit),
            90 | 80 | 79 => {
                let far: Vec<String> = std::iter::once("def far():".to_string())
                    .chain(std::iter::repeat_n("x".to_string(), 100))
                    .collect();
                symbols_for(Some(&far), line, limit)
            }
            _ => symbols_for(Some(&source), line, limit),
        };
        assert_eq!(json!(got), case[2], "{case}");
    }
    let windows = fx["windows"].as_array().expect("windows");
    let count = windows.len();
    for (index, case) in windows.iter().enumerate() {
        let (line, radius) = (case[0].as_i64().expect("line"), case[1].as_i64().expect("radius"));
        let wide = ["a".to_string(), "w".repeat(500), "c".to_string()];
        let got = match index {
            i if i == count - 2 => source_window(None, line, radius),
            i if i == count - 1 => source_window(Some(&wide), line, radius),
            _ => source_window(Some(&source), line, radius),
        };
        assert_eq!(got, case[2].as_str().expect("window"), "{case}");
    }
    let files = parse_diff(fx["chunk_diffs"][0].as_str().expect("diff"));
    let cases = fx["hunk_for"].as_array().expect("hunk_for");
    for case in &cases[..cases.len() - 2] {
        let got = hunk_for(Some(&files[0]), case[0].as_i64().expect("line")).map(hunk_json);
        assert_eq!(got.unwrap_or(Value::Null), case[1], "{case}");
    }
    assert!(hunk_for(None, 5).is_none());
    assert!(hunk_for(Some(&added_file("x", "")), 5).is_none());
}

#[test]
fn review_render_report_matches_python() {
    let fx = fixture();
    let files = parse_diff(fx["chunk_diffs"][0].as_str().expect("diff"));
    for case in fx["reports"].as_array().expect("reports") {
        let findings = |key: &str| -> Vec<Finding> { serde_json::from_value(case[key].clone()).expect("findings") };
        let (confirmed, uncertain, rejected) = (findings("confirmed"), findings("uncertain"), findings("rejected"));
        let stats: Stats = serde_json::from_value(case["stats"].clone()).expect("stats");
        let not_checked: Vec<String> = serde_json::from_value(case["not_checked"].clone()).expect("gaps");
        let reviewers = reviewers_of(&case["reviewers"]);
        let empty = case["name"] == "empty";
        let input = ReportInput {
            label: "branch x",
            files: if empty { &[] } else { &files },
            reviewers: &reviewers,
            confirmed: &confirmed,
            uncertain: &uncertain,
            rejected: &rejected,
            stats: &stats,
            not_checked: &not_checked,
            path: case["path"].as_str(),
            post_line: case["post_line"].as_str(),
        };
        assert_eq!(
            render_report(&input, reader(&fx["report_src"])),
            case["report"].as_str().expect("report"),
            "{}",
            case["name"]
        );
    }
}

/// Real git with the recorded identity and clock; gh answered from the case; every call recorded.
struct Recorder {
    git: ProcessRunner,
    gh: Value,
    calls: RefCell<Vec<Value>>,
}

impl Runner for Recorder {
    fn run(&self, argv: &[String], cwd: &str, timeout_s: u64) -> Output {
        self.calls.borrow_mut().push(json!([argv, cwd, timeout_s]));
        if argv[0] == "gh" {
            let scripted = &self.gh[argv[1..3].join(" ")];
            if scripted.is_null() {
                return Output {
                    code: 1,
                    stdout: String::new(),
                    stderr: "unscripted".to_string(),
                };
            }
            return Output {
                code: scripted[0].as_i64().expect("code") as i32,
                stdout: scripted[1].as_str().expect("out").to_string(),
                stderr: scripted[2].as_str().expect("err").to_string(),
            };
        }
        self.git.run(argv, cwd, timeout_s)
    }
}

fn git_runner(fx: &Value) -> ProcessRunner {
    let env: HashMap<String, String> = serde_json::from_value(fx["git_env"].clone()).expect("env");
    ProcessRunner {
        env: env.into_iter().collect(),
    }
}

fn build_repo(base: &Path, steps: &Value, runner: &ProcessRunner) {
    std::fs::create_dir_all(base).expect("repo dir");
    for step in steps.as_array().expect("steps") {
        let path = |index: usize| base.join(step[index].as_str().expect("path"));
        let write = |bytes: Vec<u8>| {
            std::fs::create_dir_all(path(1).parent().expect("parent")).expect("dirs");
            std::fs::write(path(1), bytes).expect("write");
        };
        match step[0].as_str().expect("op") {
            "write" => match &step[2] {
                Value::String(text) => write(text.clone().into_bytes()),
                bytes => write(serde_json::from_value(bytes.clone()).expect("bytes")),
            },
            "fill" => write(
                step[2]
                    .as_str()
                    .expect("text")
                    .repeat(step[3].as_u64().expect("n") as usize)
                    .into_bytes(),
            ),
            "git" => {
                let argv: Vec<String> = serde_json::from_value(step.clone()).expect("argv");
                let out = runner.run(&argv, base.to_str().expect("utf-8 path"), 30);
                assert_eq!(out.code, 0, "{step}: {}", out.stderr);
            }
            other => panic!("unknown step {other}"),
        }
    }
}

/// Replace the temp root with `<root>` everywhere, as the recorder did.
fn rooted(value: Value, root: &str) -> Value {
    match value {
        Value::String(text) => Value::String(text.replace(root, "<root>")),
        Value::Array(items) => Value::Array(items.into_iter().map(|v| rooted(v, root)).collect()),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| (k.replace(root, "<root>"), rooted(v, root)))
                .collect(),
        ),
        other => other,
    }
}

#[test]
fn review_scope_over_local_repositories_matches_python() {
    let fx = fixture();
    let temp = tempfile::tempdir().expect("temp");
    let root_path = std::fs::canonicalize(temp.path()).expect("real temp");
    let root = root_path.to_str().expect("utf-8").to_string();
    let git_runner = git_runner(&fx);
    for (name, steps) in fx["repos"].as_object().expect("repos") {
        build_repo(&root_path.join(name), steps, &git_runner);
    }
    let reads = [
        "lib.py",
        "pkg/util.py",
        "pkg/new.py",
        "../escape.py",
        "../plain/a.py",
        "/etc/hostname",
        "missing.py",
        "a.py",
        "pkg",
        "other/z.py",
    ];
    for case in fx["scopes"].as_array().expect("scopes") {
        let recorder = Recorder {
            git: git_runner.clone(),
            gh: case["gh"].clone(),
            calls: RefCell::new(Vec::new()),
        };
        let gh_installed = !case["gh"].is_null();
        let which = |tool: &str| (gh_installed || tool != "gh").then(|| format!("/usr/bin/{tool}"));
        let base = root_path.join(case["repo"].as_str().expect("repo"));
        // os.path.join(base, "") ends in a slash; the recorded calls keep it.
        let cwd = format!(
            "{}/{}",
            base.to_str().expect("utf-8"),
            case["cwd"].as_str().expect("cwd")
        );
        let options = parse_args(case["args"].as_str().expect("args"), &HashMap::new()).expect("options");
        let what = format!("{} {}", case["repo"], case["args"]);
        let scope = match resolve_scope(&options, &cwd, &recorder, which) {
            Ok(scope) => scope,
            Err(error) => {
                assert_eq!(rooted(json!(error.0), &root), case["error"], "{what}");
                assert_eq!(rooted(json!(recorder.calls.take()), &root), case["calls"], "{what}");
                continue;
            }
        };
        let grep = match &scope.grep {
            GrepIn::WorkingTree => Value::Null,
            GrepIn::Commit(rev) => json!(rev),
            GrepIn::Nowhere => json!(""),
        };
        let got = json!({ "label": scope.label, "files": scope.files.iter().map(file_json).collect::<Vec<_>>(),
                          "root": scope.root, "grep_rev": grep, "pr": scope.pr, "pr_url": scope.pr_url,
                          "notes": scope.notes });
        assert_eq!(rooted(got, &root), case["scope"], "{what}");
        let mut read_map = serde_json::Map::new();
        for path in reads {
            read_map.insert(path.to_string(), json!(scope.read_file(path)));
        }
        assert_eq!(rooted(Value::Object(read_map), &root), case["reads"], "{what}");
        let (chunks, skipped) = build_chunks(&scope.files, |path| scope.read_file(path), CHUNK_CHARS, CONTEXT_LINES);
        assert_eq!(
            rooted(json!(chunks.iter().map(chunk_json).collect::<Vec<_>>()), &root),
            case["chunks"],
            "{what}"
        );
        assert_eq!(skipped_json(&skipped), case["skipped"], "{what}");
        for related in case["related"].as_array().expect("related") {
            let (file, line) = (
                related[0]["file"].as_str().expect("file"),
                related[0]["line"].as_i64().expect("line"),
            );
            let source = scope.read_file(file);
            assert_eq!(
                rooted(json!(related_code(&scope, file, line, source.as_deref())), &root),
                related[1],
                "{what} {file}:{line}"
            );
        }
        assert_eq!(rooted(json!(recorder.calls.take()), &root), case["calls"], "{what}");
    }

    // Save: under the git dir, else the fallback directory.
    let stamp = "20261011-120000";
    let tmp = root_path.join("tmp");
    let feature = Git::new(root_path.join("feature").to_str().expect("utf-8"), &git_runner);
    let saved = save("report text\n", Some(&feature), stamp, &tmp).expect("saved");
    assert_eq!(rooted(json!(saved), &root), fx["saved"]["git"]);
    assert_eq!(
        std::fs::read_to_string(&saved).expect("read"),
        fx["saved_text"].as_str().expect("text")
    );
    let plain = Git::new(root_path.join("plain").to_str().expect("utf-8"), &git_runner);
    assert_eq!(
        rooted(json!(save("x", Some(&plain), stamp, &tmp)), &root),
        fx["saved"]["plain"]
    );

    // Post: only with confirm, a pull request, a saved report and gh; gh's output or a line naming the PR.
    let report_path = root_path.join("saved.md");
    std::fs::write(&report_path, "body\n").expect("report");
    let report = report_path.to_str().expect("utf-8");
    let missing = root_path.join("missing.md");
    for case in fx["posts"].as_array().expect("posts") {
        let gh = if case["gh"].is_null() {
            Value::Null
        } else {
            json!({ "pr comment": case["gh"] })
        };
        let recorder = Recorder {
            git: git_runner.clone(),
            gh,
            calls: RefCell::new(Vec::new()),
        };
        let installed = !case["gh"].is_null();
        let which = |tool: &str| installed.then(|| format!("/usr/bin/{tool}"));
        let (path, review_pr) = match case["review"].as_str().expect("review") {
            "review" => (Some(report), Some(7)),
            "path" => (Some(report), None),
            "missing" => (missing.to_str(), None),
            _ => (Some(report), None),
        };
        let pr = case["pr"].as_u64().or(review_pr);
        let confirm = case["confirm"].as_bool().expect("confirm");
        let result = post(path, pr, confirm, &recorder, which, &root);
        match result {
            Ok(text) => {
                assert_eq!(json!(text), case["ok"], "{case}");
                if case.get("post_pending").is_some() {
                    assert_eq!(case["post_pending"], json!(false));
                }
            }
            Err(error) => assert_eq!(json!(error.0), case["error"], "{case}"),
        }
        assert_eq!(rooted(json!(recorder.calls.take()), &root), case["calls"], "{case}");
    }
}

#[test]
fn review_process_runner_reports_missing_programs_and_timeouts() {
    let runner = ProcessRunner::default();
    let missing = runner.run(&["no-such-program-xyz".to_string()], ".", 5);
    assert_eq!(
        missing,
        Output {
            code: 127,
            stdout: String::new(),
            stderr: "no-such-program-xyz: not found".to_string()
        }
    );
    let argv: Vec<String> = ["sh", "-c", "sleep 5", "extra"].iter().map(|s| s.to_string()).collect();
    let slow = runner.run(&argv, ".", 0);
    assert_eq!(slow.code, 124);
    assert_eq!(slow.stderr, "sh -c sleep 5: timed out after 0s");
    let ok = runner.run(
        &[
            "sh".to_string(),
            "-c".to_string(),
            "printf 'a\\377'; echo err >&2; exit 3".to_string(),
        ],
        ".",
        5,
    );
    assert_eq!(
        (ok.code, ok.stdout.as_str(), ok.stderr.as_str()),
        (3, "a\u{fffd}", "err\n")
    );
}
