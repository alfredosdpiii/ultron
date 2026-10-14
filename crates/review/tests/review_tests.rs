//! Replays crates/review/fixtures/tests.json, recorded from `autoreview_tests.py` (fixtures/record_tests.py): sandbox
//! command lines and environments, detection, the self-check, the doctor report and command line, environment
//! discovery over trees rebuilt here, runner detection, outcomes, test sessions with a scripted executor and export,
//! summaries, real `run_process` runs and a real `git archive` export. Then the cases of
//! packages/coding-agent/test/ultron-autoreview-tests.test.ts with their literal expectations.

#[path = "../../sandbox/tests/backends_support/mod.rs"]
mod backends_support;

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::path::Path;
use std::process::Command;
use std::rc::Rc;
use std::time::{Duration, Instant};

use regex::Regex;
use review::review_tests::{
    Bind, CliOutput, Env, Executor, Mechanism, Record, Rev, Sandbox, SessionOptions, SystemDir, TestRunner,
    TestSession, TestsError, command_line, detect_runner, detect_sandbox, detect_sandbox_with, discover_environment,
    doctor_with, dumps, export_commit, interpreter_home, launch_env, launch_env_from, list_environments, make_temp_dir,
    nearest, parse_outcome, realpath, run_process, runner_for, sandbox_env, self_check, summarize, which,
};
use serde_json::{Value, json};

fn fixture() -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/tests.json");
    serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("fixture json")
}

fn strings(value: &Value) -> Vec<String> {
    serde_json::from_value(value.clone()).expect("strings")
}

fn env_of(value: &Value) -> Env {
    serde_json::from_value(value.clone()).expect("env")
}

fn layout(fx: &Value, name: &str) -> Vec<(String, SystemDir)> {
    fx["layouts"][name]
        .as_array()
        .expect("layout")
        .iter()
        .map(|item| {
            let path = item[0].as_str().expect("path").to_string();
            let kind = match item[1].as_str() {
                Some("link") => SystemDir::Link(item[2].as_str().expect("target").to_string()),
                Some("dir") => SystemDir::Dir,
                _ => SystemDir::Absent,
            };
            (path, kind)
        })
        .collect()
}

fn sandbox(fx: &Value, mechanism: &str, image: Option<&str>, home: &str, layout_name: &str) -> Sandbox {
    Sandbox {
        mechanism: Mechanism::parse(mechanism).expect("mechanism"),
        image: image.map(str::to_string),
        home: home.to_string(),
        user: fx["user"].as_str().expect("user").to_string(),
        system: layout(fx, layout_name),
        // The recorder scripts setpriv as present unless a case says otherwise.
        setpriv: true,
    }
}

/// A temp directory, symlinks resolved (as Python's `os.path.realpath(tempfile.mkdtemp())`).
fn temp_root() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = realpath(&dir.path().to_string_lossy());
    (dir, root)
}

fn build(root: &str, tree: &Value) {
    for entry in tree.as_array().expect("tree") {
        let path = Path::new(root).join(entry[1].as_str().expect("path"));
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        match entry[0].as_str() {
            Some("dir") => std::fs::create_dir_all(&path).expect("dir"),
            Some("link") => {
                std::os::unix::fs::symlink(entry[2].as_str().expect("target").replace("<root>", root), &path)
                    .expect("link")
            }
            _ => std::fs::write(&path, entry[2].as_str().expect("text")).expect("file"),
        }
    }
}

fn unroot(value: &Value, root: &str) -> Value {
    serde_json::from_str(&serde_json::to_string(value).expect("json").replace(root, "<root>")).expect("json")
}

fn reroot(text: &str, root: &str) -> String {
    text.replace("<root>", root)
}

const FIXED_PATH: &str = "/usr/bin:/bin";

// --- Environments and command lines -------------------------------------------------------------------------------

#[test]
fn review_tests_sandbox_env_matches_python() {
    let fx = fixture();
    for case in fx["sandbox_env"].as_array().expect("cases") {
        let got = sandbox_env(case["extra"].as_str());
        assert_eq!(
            serde_json::to_value(&got).expect("env"),
            case["env"],
            "{}",
            case["extra"]
        );
    }
    for case in fx["launch_env"].as_array().expect("cases") {
        let environ = case["environ"].clone();
        let got = launch_env_from(&|name| environ.get(name).and_then(Value::as_str).map(str::to_string));
        assert_eq!(
            serde_json::to_value(&got).expect("env"),
            case["env"],
            "{}",
            case["environ"]
        );
    }
}

#[test]
fn review_tests_wrap_matches_python() {
    let fx = fixture();
    for case in fx["wrap"].as_array().expect("cases") {
        let sb = Sandbox {
            setpriv: case["setpriv"].as_bool().expect("setpriv"),
            ..sandbox(
                &fx,
                case["mechanism"].as_str().expect("m"),
                case["image"].as_str(),
                case["home"].as_str().expect("home"),
                case["layout"].as_str().expect("layout"),
            )
        };
        let binds: Vec<Bind> = case["binds"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| match item {
                        Value::String(path) => Some(Bind::Same(path.clone())),
                        Value::Array(pair) => Some(Bind::Pair(
                            pair[0].as_str().expect("source").to_string(),
                            pair[1].as_str().expect("target").to_string(),
                        )),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        let argv = sb.wrap(
            &strings(&case["command"]),
            case["workdir"].as_str().expect("workdir"),
            &env_of(&case["env"]),
            &binds,
            case["cwd"].as_str().expect("cwd"),
        );
        assert_eq!(json!(argv), case["argv"], "{case}");
    }
    for case in fx["describe"].as_array().expect("cases") {
        let sb = sandbox(
            &fx,
            case["mechanism"].as_str().expect("m"),
            case["image"].as_str(),
            "/h",
            "merged",
        );
        assert_eq!(json!(sb.describe()), case["text"]);
    }
}

/// A scripted executor: a function of argv; every call is recorded.
type Calls = Rc<RefCell<Vec<Value>>>;

fn timeout_json(timeout: f64) -> Value {
    if timeout.fract() == 0.0 {
        json!(timeout as i64)
    } else {
        json!(timeout)
    }
}

#[test]
fn review_tests_detect_sandbox_matches_python() {
    let fx = fixture();
    for case in fx["detect"].as_array().expect("cases") {
        let have = strings(&case["have"]);
        let mode = case["executor"].as_str().expect("executor").to_string();
        let calls: Calls = Rc::default();
        let probes: Rc<RefCell<Vec<String>>> = Rc::default();
        let (calls2, probes2) = (calls.clone(), probes.clone());
        let executor = move |argv: &[String], cwd: &str, env: &Env, timeout: f64| {
            probes2.borrow_mut().push(cwd.to_string());
            assert_eq!(env, &launch_env(), "the launcher gets only the launch environment");
            calls2.borrow_mut().push(json!({
                "argv": argv.iter().map(|part| part.replace(cwd, "<probe>")).collect::<Vec<_>>(), "cwd": "<probe>",
                "env": {"PATH": FIXED_PATH}, "timeout": timeout_json(timeout), "probeExists": Path::new(cwd).is_dir()}));
            Ok(match mode.as_str() {
                "ok" => (0, String::new()),
                "broken" => (1, "bwrap: setting up uid map: Permission denied".to_string()),
                other => (
                    i32::from(argv[0] != other.split(':').nth(1).expect("name")),
                    String::new(),
                ),
            })
        };
        let only = case["only"].as_str().map(Mechanism::parse);
        // An `only` that names no mechanism matches none.
        let picked = if only == Some(None) {
            None
        } else {
            detect_sandbox_with(
                case["image"].as_str(),
                &|name| have.contains(&name.to_string()).then(|| format!("/usr/bin/{name}")),
                &executor,
                only.flatten(),
                &|mechanism, image| Sandbox {
                    mechanism,
                    image,
                    home: "/home/u".to_string(),
                    user: fx["user"].as_str().expect("user").to_string(),
                    system: layout(&fx, "merged"),
                    setpriv: true,
                },
            )
        };
        let picked = picked.map(|sb| json!([sb.mechanism.as_str(), sb.image]));
        assert_eq!(picked.unwrap_or(Value::Null), case["picked"], "{case}");
        // Unshare's argv names the sandbox's home; the recorder's was `<root>/fakehome`.
        let got: Vec<Value> = calls
            .borrow()
            .iter()
            .map(|call| unroot(call, "/home/u").clone())
            .collect();
        let want: Vec<Value> = case["calls"]
            .as_array()
            .expect("calls")
            .iter()
            .map(|call| serde_json::from_str(&call.to_string().replace("<root>/fakehome", "<root>")).expect("json"))
            .collect();
        assert_eq!(got, want, "{case}");
        assert!(
            probes.borrow().iter().all(|probe| !Path::new(probe).exists()),
            "every probe directory is removed"
        );
        assert_eq!(case["probesLeft"], json!(0));
    }
}

fn canary_free(text: &str) -> String {
    Regex::new(r"\.ultron-autoreview-canary-[0-9a-f]{8}")
        .expect("re")
        .replace_all(text, "<canary>")
        .into_owned()
}

fn check_executor(code: i32, output: String, calls: Calls, root: String) -> impl Executor {
    move |argv: &[String], cwd: &str, env: &Env, timeout: f64| {
        let canary = argv.last().cloned().unwrap_or_default();
        let mut env = env.clone();
        env.insert("PATH".to_string(), FIXED_PATH.to_string());
        let norm = |text: &str| canary_free(&text.replace(cwd, "<work>")).replace(&root, "<root>");
        calls.borrow_mut().push(json!({
            "argv": argv.iter().map(|part| norm(part)).collect::<Vec<_>>(), "env": env,
            "timeout": timeout_json(timeout), "canaryExists": Path::new(&canary).exists(), "canary": norm(&canary)}));
        Ok((code, output.clone()))
    }
}

#[test]
fn review_tests_self_check_matches_python() {
    let fx = fixture();
    for case in fx["self_check"].as_array().expect("cases") {
        let (_dir, root) = temp_root();
        let home = format!("{root}/fakehome");
        std::fs::create_dir_all(&home).expect("home");
        let mechanism = case["mechanism"].as_str().expect("m");
        let sb = sandbox(
            &fx,
            mechanism,
            (mechanism == "docker").then_some("img"),
            &home,
            "merged",
        );
        let calls: Calls = Rc::default();
        let executor = check_executor(
            case["code"].as_i64().expect("code") as i32,
            case["output"].as_str().expect("output").to_string(),
            calls.clone(),
            root.clone(),
        );
        let result = self_check(&sb, &executor, &home).expect("self-check");
        assert_eq!(result, case["result"], "{}", case["output"]);
        assert_eq!(json!(*calls.borrow()), case["calls"]);
        let left = std::fs::read_dir(&home).expect("home").count();
        assert_eq!(json!(left), case["left"], "the canary is removed");
    }
}

#[test]
fn review_tests_doctor_and_command_line_match_python() {
    let fx = fixture();
    for case in fx["doctor"].as_array().expect("cases") {
        let (_dir, root) = temp_root();
        let home = format!("{root}/fakehome");
        std::fs::create_dir_all(&home).expect("home");
        let have = strings(&case["have"]);
        let output = case["output"].as_str().expect("output").to_string();
        let detect = |image: Option<&str>| {
            detect_sandbox_with(
                image,
                &|name| have.contains(&name.to_string()).then(|| format!("/usr/bin/{name}")),
                &|_: &[String], _: &str, _: &Env, _: f64| Ok((0, String::new())),
                None,
                &|mechanism, image| Sandbox {
                    mechanism,
                    image,
                    home: home.clone(),
                    user: "1:1".to_string(),
                    system: layout(&fx, "merged"),
                    setpriv: true,
                },
            )
        };
        let check = |sb: &Sandbox| {
            self_check(
                sb,
                &check_executor(0, output.clone(), Rc::default(), root.clone()),
                &home,
            )
        };
        let report = doctor_with(case["image"].as_str(), &detect, &check).expect("doctor");
        assert_eq!(report, case["report"]);
        assert_eq!(json!(dumps(&report)), case["json"]);
        let mut args = vec!["doctor".to_string()];
        args.extend(case["image"].as_str().map(str::to_string));
        let printed = command_line(&args, &|image| doctor_with(image, &detect, &check));
        assert_eq!(
            printed,
            CliOutput {
                code: 0,
                stdout: format!("{}\n", case["json"].as_str().expect("json")),
                stderr: String::new()
            }
        );
    }
    let (_dir, root) = temp_root();
    build(&root, &fx["tree"]);
    for case in fx["cli"].as_array().expect("cases") {
        let args: Vec<String> = strings(&case["args"]).iter().map(|arg| reroot(arg, &root)).collect();
        let got = command_line(&args, &|_| panic!("doctor is not asked for"));
        assert_eq!(json!(got.code), case["code"], "{args:?}");
        assert_eq!(json!(got.stdout.replace(&root, "<root>")), case["stdout"], "{args:?}");
        assert_eq!(json!(got.stderr), case["stderr"], "{args:?}");
    }
}

#[test]
fn review_tests_environments_match_python() {
    let fx = fixture();
    let (_dir, root) = temp_root();
    build(&root, &fx["tree"]);
    let at = |path: &str| {
        if path.starts_with('/') {
            path.to_string()
        } else {
            format!("{root}/{path}")
        }
    };
    for case in fx["interpreter_home"].as_array().expect("cases") {
        let home = case["home"].as_str().map(at);
        // A home of None is the process's: the recorder's was `<root>/fakehome`.
        let home = home.or_else(|| Some(at("fakehome")));
        let got = interpreter_home(&at(case["binary"].as_str().expect("binary")), home.as_deref());
        assert_eq!(unroot(&json!(got), &root), case["prefix"], "{case}");
    }
    for case in fx["discover"].as_array().expect("cases") {
        let home = Some(at(case["home"].as_str().unwrap_or("fakehome")));
        let got = discover_environment(
            &at(case["checkout"].as_str().expect("checkout")),
            case["directory"].as_str().expect("dir"),
            home.as_deref(),
        );
        assert_eq!(
            unroot(&serde_json::to_value(&got).expect("json"), &root),
            case["found"],
            "{case}"
        );
    }
    for case in fx["list"].as_array().expect("cases") {
        let got = list_environments(
            &at(case["checkout"].as_str().expect("checkout")),
            case["depth"].as_u64().expect("depth") as usize,
            Some(&at("fakehome")),
        );
        assert_eq!(unroot(&json!(got), &root), case["found"], "{case}");
    }
}

#[test]
fn review_tests_top_level_install_directory_is_never_bound() {
    let fx = fixture();
    let case = &fx["top_level"];
    assert_eq!(case["prefix"], Value::Null);
    let (_dir, top) = temp_root();
    // The recorded case is a child of a top-level directory; a temp dir elsewhere cannot replay it.
    if json!(top.matches('/').count()) != case["slashes"] {
        eprintln!("skipped: the temp directory {top} is not at the recorded depth");
        return;
    }
    std::fs::create_dir_all(format!("{top}/bin")).expect("bin");
    std::fs::create_dir_all(format!("{top}/lib")).expect("lib");
    std::fs::write(format!("{top}/bin/python"), "").expect("python");
    assert_eq!(
        interpreter_home(&format!("{top}/bin/python"), Some("/nonexistent-home")),
        None
    );
    // One level deeper, the same layout is an install directory of its own.
    std::fs::create_dir_all(format!("{top}/py/bin")).expect("bin");
    std::fs::create_dir_all(format!("{top}/py/lib")).expect("lib");
    std::fs::write(format!("{top}/py/bin/python"), "").expect("python");
    assert_eq!(
        interpreter_home(&format!("{top}/py/bin/python"), Some("/nonexistent-home")),
        Some(format!("{top}/py"))
    );
}

// --- Runners and outcomes -----------------------------------------------------------------------------------------

fn runner_json(runner: &Option<TestRunner>) -> Value {
    runner
        .as_ref()
        .map_or(Value::Null, |runner| json!([runner.name, runner.because]))
}

fn command_cases(value: &Value) -> Vec<(Vec<String>, Option<String>)> {
    value
        .as_array()
        .expect("commands")
        .iter()
        .map(|item| (strings(&item[0]), item[1].as_str().map(str::to_string)))
        .collect()
}

#[test]
fn review_tests_runner_detection_matches_python() {
    let fx = fixture();
    for case in fx["detect_runner"].as_array().expect("cases") {
        let files = case["files"].as_object().expect("files").clone();
        let tracked: Vec<String> = files.keys().cloned().collect();
        let runner = detect_runner(&tracked, &|path| {
            files.get(path).and_then(Value::as_str).map(str::to_string)
        });
        assert_eq!(runner_json(&runner), case["runner"], "{}", case["files"]);
        let commands: Vec<Value> = match &runner {
            Some(runner) => command_cases(&json!([
                [["tests/test_a.py"], "name"],
                [[], null],
                [["a/x_test.go", "a/y_test.go", "b_test.go"], null],
                [["x.ts", "y.ts"], ""]
            ]))
            .into_iter()
            .map(|(paths, select)| json!([paths, select, runner.command(&paths, select.as_deref())]))
            .collect(),
            None => Vec::new(),
        };
        assert_eq!(json!(commands), case["commands"]);
    }
    for case in fx["commands"].as_array().expect("cases") {
        let runner = TestRunner::new(case["runner"].as_str().expect("runner"), "x");
        let argv = runner.command(&strings(&case["paths"]), case["select"].as_str());
        assert_eq!(json!(argv), case["argv"], "{case}");
    }
    let files = fx["runner_for"]["files"].as_object().expect("files").clone();
    let tracked: HashSet<String> = files.keys().cloned().collect();
    let read = |path: &str| files.get(path).and_then(Value::as_str).map(str::to_string);
    for case in fx["runner_for"]["cases"].as_array().expect("cases") {
        let found = runner_for(case[0].as_str().expect("path"), &tracked, &read)
            .map(|(runner, dir)| json!([runner.name, runner.because, dir]));
        assert_eq!(found.unwrap_or(Value::Null), case[1], "{}", case[0]);
    }
    for case in fx["nearest"].as_array().expect("cases") {
        let names = strings(&case[1]);
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        assert_eq!(
            json!(nearest(&tracked, case[0].as_str().expect("path"), &names)),
            case[2],
            "{case}"
        );
    }
}

#[test]
fn review_tests_parse_outcome_matches_python() {
    let fx = fixture();
    for case in fx["parse_outcome"].as_array().expect("cases") {
        let (status, tests) = parse_outcome(
            case["runner"].as_str().expect("runner"),
            case["code"].as_i64().expect("code") as i32,
            case["output"].as_str().expect("output"),
        );
        assert_eq!(json!(status), case["status"], "{case}");
        assert_eq!(serde_json::to_value(&tests).expect("tests"), case["tests"], "{case}");
    }
}

#[test]
fn review_tests_summaries_match_python() {
    let fx = fixture();
    for case in fx["summaries"].as_array().expect("cases") {
        let record: Record = serde_json::from_value(case["record"].clone()).expect("record");
        assert_eq!(json!(summarize(&record)), case["text"]);
    }
}

// --- Test sessions --------------------------------------------------------------------------------------------------

/// What a replayed session shares with its scripted executor and export.
#[derive(Clone, Default)]
struct Shared {
    calls: Calls,
    labels: Rc<RefCell<Vec<(String, String)>>>,
    exported: Rc<RefCell<Vec<String>>>,
    root: String,
}

impl Shared {
    fn norm(&self, text: &str) -> String {
        let mut text = text.to_string();
        for (path, label) in self.labels.borrow().iter() {
            text = text.replace(path, label);
        }
        text.replace(&self.root, "<root>")
    }

    fn norm_value(&self, value: &Value) -> Value {
        serde_json::from_str(&self.norm(&serde_json::to_string(value).expect("json"))).expect("json")
    }
}

fn read_text(path: &str) -> Option<String> {
    std::fs::read(path)
        .ok()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

fn scripted(spec: Value, shared: Shared) -> Box<dyn Executor> {
    Box::new(move |argv: &[String], cwd: &str, env: &Env, timeout: f64| {
        let mut watched = serde_json::Map::new();
        for name in spec["watch"].as_array().into_iter().flatten() {
            let name = name.as_str().expect("watch");
            watched.insert(name.to_string(), json!(read_text(&format!("{cwd}/{name}"))));
        }
        shared.calls.borrow_mut().push(json!({
            "argv": argv.iter().map(|part| shared.norm(part)).collect::<Vec<_>>(), "cwd": shared.norm(cwd),
            "env": env, "timeout": timeout_json(timeout), "watched": watched}));
        match spec["kind"].as_str().expect("kind") {
            "explode" => Err(spec["message"].as_str().expect("message").to_string()),
            "fixture" => {
                let source = read_text(&format!("{cwd}/src/app.py")).expect("app.py");
                let kinds = if source.contains("\"c\"") {
                    "FAILED tests/test_app.py::test_kinds - AssertionError: assert ['a', 'b', 'c'] == ['a', 'b']"
                } else {
                    "PASSED tests/test_app.py::test_kinds"
                };
                let show = if source.contains("upper()") {
                    "PASSED tests/test_app.py::test_show"
                } else {
                    "FAILED tests/test_app.py::test_show - AssertionError"
                };
                let code = i32::from(kinds.contains("FAILED") || show.contains("FAILED"));
                Ok((code, format!("{kinds}\n{show}\n")))
            }
            "make" => Ok(if read_text(&format!("{cwd}/value.txt")).as_deref() == Some("1\n") {
                (0, "home=/tmp/home\n".to_string())
            } else {
                (2, "make: *** [Makefile:2: test] Error 1\n".to_string())
            }),
            _ => {
                let items = spec["items"].as_array().expect("items");
                let item = &items[(shared.calls.borrow().len() - 1).min(items.len() - 1)];
                Ok((
                    item[0].as_i64().expect("code") as i32,
                    item[1].as_str().expect("out").replace("{cwd}", cwd),
                ))
            }
        }
    })
}

fn error_json(error: &TestsError, shared: &Shared) -> Value {
    json!({"error": [error.python_type(), shared.norm(&error.to_string())]})
}

/// Where two JSON values first differ, for failure messages.
fn first_diff(a: &Value, b: &Value) -> String {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            for key in x.keys().chain(y.keys()) {
                if x.get(key) != y.get(key) {
                    return format!(
                        ".{key}{}",
                        first_diff(x.get(key).unwrap_or(&Value::Null), y.get(key).unwrap_or(&Value::Null))
                    );
                }
            }
            String::new()
        }
        (Value::Array(x), Value::Array(y)) if x.len() == y.len() => x
            .iter()
            .zip(y)
            .enumerate()
            .find(|(_, (p, q))| p != q)
            .map_or(String::new(), |(i, (p, q))| format!("[{i}]{}", first_diff(p, q))),
        _ => format!(" = {a} vs {b}"),
    }
}

fn replay_session(fx: &Value, case: &Value) {
    let name = case["name"].as_str().expect("name");
    let (_dir, root) = temp_root();
    build(&root, &fx["tree"]);
    let shared = Shared {
        root: root.clone(),
        ..Shared::default()
    };
    let head = case["head"].as_str().expect("head").to_string();
    let base = case["base"].as_str().map(str::to_string);
    let trees = case["trees"].clone();
    let files = trees[&head].as_object().cloned().unwrap_or_default();
    let fail: Vec<String> = strings(&case["exportFail"]);
    let export = {
        let (shared, trees, head) = (shared.clone(), trees.clone(), head.clone());
        move |_root: &str, rev: &str| -> Result<String, String> {
            if fail.contains(&rev.to_string()) {
                return Err(format!(
                    "the commit {} could not be exported",
                    rev.chars().take(12).collect::<String>()
                ));
            }
            let path = make_temp_dir("ultron-autoreview-run-")?;
            for (file, text) in trees[rev].as_object().into_iter().flatten() {
                let target = Path::new(&path).join(file);
                std::fs::create_dir_all(target.parent().expect("parent")).expect("mkdir");
                std::fs::write(&target, text.as_str().expect("text")).expect("write");
            }
            let label = if rev == head { "<head>" } else { "<base>" };
            shared.labels.borrow_mut().push((path.clone(), label.to_string()));
            shared.exported.borrow_mut().push(path.clone());
            Ok(path)
        }
    };
    let ticks = Rc::new(Cell::new(0u32));
    let clock = move || {
        ticks.set(ticks.get() + 1);
        100.0 + 0.375 * f64::from(ticks.get())
    };
    let options = &case["options"];
    let node = case["node"].as_str().map(|path| format!("{root}/{path}"));
    let mechanism = case["mechanism"].as_str().expect("mechanism");
    let mut session = TestSession::new(
        "/repo",
        &head,
        base.as_deref(),
        &strings(&case["tracked"]),
        Box::new(move |path| files.get(path).and_then(Value::as_str).map(str::to_string)),
        sandbox(
            fx,
            mechanism,
            (mechanism == "docker").then_some("img"),
            &format!("{root}/fakehome"),
            "merged",
        ),
        SessionOptions {
            runs: options["runs"].as_i64().unwrap_or(6),
            timeout_s: options["timeout_s"].as_f64().unwrap_or(300.0),
            env_dir: options["env_dir"].as_str().map(|dir| reroot(dir, &root)),
            checkout: options["checkout"].as_str().map(|dir| reroot(dir, &root)),
            toolchain: options["toolchain"].as_array().map_or_else(Vec::new, |items| {
                items
                    .iter()
                    .map(|item| reroot(item.as_str().expect("tool"), &root))
                    .collect()
            }),
            env_kind: options["env_kind"].as_str().map(str::to_string),
            executor: scripted(case["executor"].clone(), shared.clone()),
            export: Box::new(export),
            clock: Box::new(clock),
            which: Box::new(move |name| if name == "node" { node.clone() } else { which(name) }),
            launch_env: Some(Env::from([("PATH".to_string(), FIXED_PATH.to_string())])),
            home: Some(format!("{root}/fakehome")),
        },
    );
    let mut results = Vec::new();
    for step in case["steps"].as_array().expect("steps") {
        let record = |record: Result<Record, TestsError>| match record {
            Ok(record) => json!({"ok": shared.norm_value(&serde_json::to_value(record).expect("record"))}),
            Err(error) => error_json(&error, &shared),
        };
        let result = match step["op"].as_str().expect("op") {
            "paths" => match session.paths(step.get("raw")) {
                Ok(paths) => json!({"ok": paths}),
                Err(error) => error_json(&TestsError::Rejected(error), &shared),
            },
            "plan" => {
                let plan = session.plan(&strings(&step["paths"]));
                json!({"ok": plan.iter().map(|(runner, dir, paths)| json!([runner.name, runner.because, dir, paths]))
                    .collect::<Vec<_>>()})
            }
            "run" => {
                let which = if step["which"].as_str() == Some("base") {
                    Rev::Base
                } else {
                    Rev::Head
                };
                let kind = step["kind"].as_str().unwrap_or("run");
                record(session.run(&strings(&step["paths"]), step["select"].as_str(), which, kind))
            }
            "serve" => match session.serve(step["kind"].as_str().expect("kind"), &step["args"]) {
                Ok((label, summary)) => json!({"ok": [label, shared.norm(&summary)]}),
                Err(error) => error_json(&error, &shared),
            },
            "mutation" => record(session.mutation(
                step.get("path"),
                step.get("line"),
                step.get("replacement"),
                step.get("tests"),
            )),
            "compare" => match session.compare(&strings(&step["paths"])) {
                Ok(compared) => json!({"ok": shared.norm_value(&serde_json::to_value(compared).expect("compared"))}),
                Err(error) => error_json(&error, &shared),
            },
            "executor" => {
                session.set_executor(scripted(step["spec"].clone(), shared.clone()));
                json!({"ok": null})
            }
            "file" => {
                let dir = session.export_dir(Rev::Head).expect("head export").to_string();
                json!({"ok": read_text(&format!("{dir}/{}", step["path"].as_str().expect("path")))})
            }
            "close" => {
                session.close();
                json!({"ok": shared.exported.borrow().iter().map(|path| Path::new(path).exists()).collect::<Vec<_>>()})
            }
            other => panic!("unknown op {other}"),
        };
        results.push(result);
    }
    for (index, (got, want)) in results
        .iter()
        .zip(case["results"].as_array().expect("results"))
        .enumerate()
    {
        assert_eq!(
            got,
            want,
            "{name} step {index}: {}: {}",
            case["steps"][index],
            first_diff(got, want)
        );
    }
    assert_eq!(results.len(), case["results"].as_array().expect("results").len());
    assert_eq!(json!(*shared.calls.borrow()), case["calls"], "{name} calls");
    let records: Vec<Value> = session
        .records
        .iter()
        .map(|r| shared.norm_value(&serde_json::to_value(r).expect("record")))
        .collect();
    assert_eq!(json!(records), case["records"], "{name} records");
    let summaries: Vec<String> = session.records.iter().map(|r| shared.norm(&summarize(r))).collect();
    assert_eq!(json!(summaries), case["summaries"], "{name} summaries");
    assert_eq!(runner_json(&session.runner), case["runner"], "{name}");
    assert_eq!(unroot(&json!(session.toolchain), &root), case["toolchain"], "{name}");
    assert_eq!(json!(session.env_kind), case["envKind"], "{name}");
    assert_eq!(json!(session.limit), case["limit"], "{name}");
    let labels: Vec<String> = shared.labels.borrow().iter().map(|(_, label)| label.clone()).collect();
    assert_eq!(json!(labels), case["exports"], "{name}");
    session.close();
    assert!(
        shared.exported.borrow().iter().all(|path| !Path::new(path).exists()),
        "{name}: exports removed"
    );
}

#[test]
fn review_tests_sessions_match_python() {
    let fx = fixture();
    let sessions = fx["sessions"].as_array().expect("sessions");
    assert!(sessions.len() >= 25);
    for case in sessions {
        replay_session(&fx, case);
    }
}

// --- Real processes -------------------------------------------------------------------------------------------------

#[test]
fn review_tests_run_process_matches_python() {
    let fx = fixture();
    let (_dir, root) = temp_root();
    for case in fx["processes"].as_array().expect("cases") {
        let (code, output) = run_process(
            &strings(&case["argv"]),
            &reroot(case["cwd"].as_str().expect("cwd"), &root),
            &env_of(&case["env"]),
            60.0,
        )
        .expect("ran");
        assert_eq!(json!(code), case["code"], "{case}");
        assert_eq!(
            json!(output.replace(&root, "<root>").chars().count()),
            case["length"],
            "{}",
            case["argv"]
        );
        let shown = if output.chars().count() < 1000 {
            output.clone()
        } else {
            output.chars().skip(output.chars().count() - 200).collect()
        };
        assert_eq!(
            json!(shown.replace(&root, "<root>")),
            case["output"],
            "{}",
            case["argv"]
        );
    }
}

fn alive(pid: &str) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rsplit(')')
            .next()
            .is_some_and(|rest| !rest.trim_start().starts_with('Z'))
    })
}

#[test]
fn review_tests_run_process_timeout_kills_the_group() {
    let env = Env::from([("PATH".to_string(), FIXED_PATH.to_string())]);
    // The shell starts a background sleeper (its own child, same process group) and waits on it.
    let argv: Vec<String> = ["sh", "-c", "sleep 600 & echo pid=$!; wait"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let (code, output) = run_process(&argv, "/", &env, 2.0).expect("ran");
    assert_eq!(code, 124);
    if let Some(pid) = output.strip_prefix("pid=").map(str::trim) {
        let guard = Instant::now() + Duration::from_secs(120);
        while alive(pid) {
            assert!(Instant::now() < guard, "the sleeper {pid} survived the timeout");
            std::thread::yield_now();
        }
    }
    assert!(run_process(&[], "/", &env, 1.0).is_err());
    // Exit codes and signals.
    let killed: Vec<String> = ["sh", "-c", "kill -9 $$"].iter().map(|s| s.to_string()).collect();
    assert_eq!(run_process(&killed, "/", &env, 60.0).expect("ran").0, -1);
}

fn git(dir: &str, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env_clear()
        .envs([
            ("PATH", FIXED_PATH),
            ("HOME", dir),
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_AUTHOR_NAME", "Review Test"),
            ("GIT_AUTHOR_EMAIL", "review@test"),
            ("GIT_COMMITTER_NAME", "Review Test"),
            ("GIT_COMMITTER_EMAIL", "review@test"),
            ("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z"),
            ("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z"),
        ])
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn tree_of(path: &str) -> Value {
    use std::os::unix::fs::PermissionsExt;
    fn walk(base: &Path, dir: &Path, out: &mut serde_json::Map<String, Value>) {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .expect("dir")
            .map(|e| e.expect("entry").path())
            .collect();
        entries.sort();
        for entry in entries {
            if entry.is_dir() {
                walk(base, &entry, out);
            } else {
                let mode = std::fs::metadata(&entry).expect("meta").permissions().mode();
                let relative = entry
                    .strip_prefix(base)
                    .expect("relative")
                    .to_string_lossy()
                    .into_owned();
                out.insert(
                    relative,
                    json!([std::fs::read_to_string(&entry).expect("text"), mode & 0o100 != 0]),
                );
            }
        }
    }
    let mut out = serde_json::Map::new();
    walk(Path::new(path), Path::new(path), &mut out);
    let mut sorted: Vec<(String, Value)> = out.into_iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    Value::Object(sorted.into_iter().collect())
}

fn build_repo(dir: &str, steps: &Value) {
    git(dir, &["init", "-q", "-b", "main"]);
    for step in steps.as_array().expect("steps") {
        for (path, text) in step["files"].as_object().expect("files") {
            let target = Path::new(dir).join(path);
            std::fs::create_dir_all(target.parent().expect("parent")).expect("mkdir");
            std::fs::write(&target, text.as_str().expect("text")).expect("write");
        }
        for path in step["exec"].as_array().into_iter().flatten() {
            use std::os::unix::fs::PermissionsExt;
            let target = Path::new(dir).join(path.as_str().expect("path"));
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        for path in step["remove"].as_array().into_iter().flatten() {
            std::fs::remove_file(Path::new(dir).join(path.as_str().expect("path"))).expect("rm");
        }
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-qm", step["message"].as_str().expect("message")]);
    }
}

#[test]
fn review_tests_export_commit_matches_python() {
    let fx = fixture();
    let (_dir, root) = temp_root();
    build_repo(&root, &fx["export"]["steps"]);
    for case in fx["export"]["exports"].as_array().expect("exports") {
        let rev = case["rev"].as_str().expect("rev");
        match export_commit(&root, rev) {
            Ok(target) => {
                // os.walk visits dirs in its own order; the recorded tree is keyed, so compare as sorted maps.
                let mut want: Vec<(String, Value)> =
                    case["tree"].as_object().expect("tree").clone().into_iter().collect();
                want.sort_by(|a, b| a.0.cmp(&b.0));
                assert_eq!(tree_of(&target), Value::Object(want.into_iter().collect()), "{rev}");
                std::fs::remove_dir_all(&target).expect("rm");
            }
            Err(error) => assert_eq!(json!(error), case["error"], "{rev}"),
        }
    }
}

// --- Ported: packages/coding-agent/test/ultron-autoreview-tests.test.ts -----------------------------------------------

fn s(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| item.to_string()).collect()
}

#[test]
fn review_tests_ts_runners_recognized_from_tracked_files() {
    let detect = |files: &[(&str, &str)]| {
        let map: Vec<(String, String)> = files.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
        let tracked: Vec<String> = map.iter().map(|(a, _)| a.clone()).collect();
        let runner = detect_runner(&tracked, &|path| {
            map.iter().find(|(a, _)| a == path).map(|(_, b)| b.clone())
        });
        let command = runner
            .as_ref()
            .map_or_else(Vec::new, |r| r.command(&s(&["tests/test_a.py"]), Some("name")));
        (runner.map(|r| r.name), command)
    };
    let vitest = json!({"devDependencies": {"vitest": "1"}}).to_string();
    let jest = json!({"scripts": {"test": "jest --ci"}}).to_string();
    let pnpm = json!({"scripts": {"test": "node test.js"}}).to_string();
    let none = json!({"scripts": {"test": "echo Error: no test specified && exit 1"}}).to_string();
    let out = [
        detect(&[("pytest.ini", ""), ("tests/test_a.py", "")]),
        detect(&[("pyproject.toml", "[tool.pytest.ini_options]"), ("a.py", "")]),
        detect(&[("setup.cfg", "[tool:pytest]")]),
        detect(&[("src/x.py", ""), ("tests/test_x.py", "")]),
        detect(&[("package.json", &vitest)]),
        detect(&[("package.json", &jest)]),
        detect(&[("package.json", &pnpm), ("pnpm-lock.yaml", "")]),
        detect(&[("package.json", &none)]),
        detect(&[("go.mod", "module x")]),
        detect(&[("Cargo.toml", "")]),
        detect(&[("Makefile", "build:\n\tcc x.c\ntest:\n\t./run\n")]),
        detect(&[("Makefile", "build:\n\tcc x.c\n"), ("README.md", "")]),
    ];
    let names: Vec<Option<&str>> = out.iter().map(|(name, _)| name.as_deref()).collect();
    assert_eq!(
        names,
        [
            Some("pytest"),
            Some("pytest"),
            Some("pytest"),
            Some("pytest"),
            Some("vitest"),
            Some("jest"),
            Some("pnpm"),
            None,
            Some("go"),
            Some("cargo"),
            Some("make"),
            None
        ]
    );
    assert_eq!(
        out[0].1,
        s(&[
            "python3",
            "-m",
            "pytest",
            "-q",
            "-rA",
            "--no-header",
            "-p",
            "no:cacheprovider",
            "--tb=short",
            "-k",
            "name",
            "tests/test_a.py"
        ])
    );
    assert_eq!(
        out[4].1,
        s(&["npx", "--no-install", "vitest", "run", "-t", "name", "tests/test_a.py"])
    );
    assert_eq!(out[8].1, s(&["go", "test", "-run", "name", "./tests"]));
    assert_eq!(out[9].1, s(&["cargo", "test", "--offline", "name"]));
}

#[test]
fn review_tests_ts_outcome_passed_failed_timeout_unavailable() {
    let cases: [(i32, &str); 8] = [
        (0, "PASSED tests/a.py::t"),
        (1, "FAILED tests/a.py::t - assert 1 == 2"),
        (
            1,
            "ImportError while importing test module 'tests/a.py'\nModuleNotFoundError: No module named 'requests'",
        ),
        (1, "/usr/bin/python3: No module named pytest"),
        (5, "no tests ran"),
        (127, "npx: not found"),
        (124, ""),
        (1, "FAILED tests/a.py::t - ModuleNotFoundError: No module named 'x'"),
    ];
    let mut out: Vec<String> = cases
        .iter()
        .map(|(code, output)| parse_outcome("pytest", *code, output).0)
        .collect();
    out.push(parse_outcome("vitest", 1, "Error: Cannot find module 'vitest'").0);
    out.push(parse_outcome("go", 1, "--- FAIL: TestX (0.00s)").0);
    assert_eq!(
        out,
        s(&[
            "passed",
            "failed",
            "unavailable",
            "unavailable",
            "unavailable",
            "unavailable",
            "timeout",
            "failed",
            "unavailable",
            "failed"
        ])
    );
}

#[test]
fn review_tests_ts_monorepo_packages_run_their_own_tests() {
    let files: Vec<(String, String)> = [
        ("pyproject.toml", "[tool.pytest.ini_options]".to_string()),
        ("api/tests/test_users.py", String::new()),
        ("api/conftest.py", String::new()),
        (
            "web/package.json",
            json!({"devDependencies": {"vitest": "1"}}).to_string(),
        ),
        ("web/src/Resources.test.tsx", String::new()),
        ("web/src/Resources.tsx", String::new()),
        (
            "packages/ui/package.json",
            json!({"scripts": {"test": "jest"}}).to_string(),
        ),
        ("packages/ui/src/Button.test.jsx", String::new()),
        (
            "packages/ui/src/deep/package.json",
            json!({"name": "no runner here"}).to_string(),
        ),
        ("packages/ui/src/deep/x.spec.ts", String::new()),
        ("tools/loose.test.ts", String::new()),
        ("svc/go.mod", "module svc".to_string()),
        ("svc/pkg/a_test.go", String::new()),
        ("crate/Cargo.toml", String::new()),
        ("crate/src/lib.rs", String::new()),
        ("notes/readme.txt", String::new()),
    ]
    .into_iter()
    .map(|(a, b)| (a.to_string(), b))
    .collect();
    let tracked: HashSet<String> = files.iter().map(|(a, _)| a.clone()).collect();
    let lookup = files.clone();
    let read = move |path: &str| lookup.iter().find(|(a, _)| a == path).map(|(_, b)| b.clone());
    let names = s(&[
        "api/tests/test_users.py",
        "web/src/Resources.test.tsx",
        "packages/ui/src/Button.test.jsx",
        "packages/ui/src/deep/x.spec.ts",
        "tools/loose.test.ts",
        "svc/pkg/a_test.go",
        "crate/src/lib.rs",
    ]);
    let runners: Vec<Option<(String, String)>> = names
        .iter()
        .map(|name| runner_for(name, &tracked, &read).map(|(r, d)| (r.name, d)))
        .collect();
    let pair = |a: &str, b: &str| Some((a.to_string(), b.to_string()));
    assert_eq!(
        runners,
        [
            pair("pytest", "api"),
            // The file that was once handed to pytest.
            pair("vitest", "web"),
            pair("jest", "packages/ui"),
            // A package.json that names no runner defers to the one above it.
            pair("jest", "packages/ui"),
            // No package.json above it declares a runner: nothing is run, least of all pytest.
            None,
            pair("go", "svc"),
            pair("cargo", "crate"),
        ]
    );
    let calls: Rc<RefCell<Vec<Vec<String>>>> = Rc::default();
    let seen = calls.clone();
    let mut tracked_sorted: Vec<String> = tracked.iter().cloned().collect();
    tracked_sorted.sort();
    let mut session = TestSession::new(
        "/nowhere",
        "HEAD",
        None,
        &tracked_sorted,
        Box::new(read),
        Sandbox::new(Mechanism::Bwrap, None),
        SessionOptions {
            runs: 10,
            executor: Box::new(move |argv: &[String], _: &str, _: &Env, _: f64| {
                seen.borrow_mut().push(argv.to_vec());
                Ok((0, "PASSED tests/test_users.py::test_ok".to_string()))
            }),
            export: Box::new(|_: &str, _: &str| make_temp_dir("ultron-autoreview-run-")),
            ..SessionOptions::default()
        },
    );
    let plan: Vec<(String, String, Vec<String>)> = session
        .plan(&names)
        .into_iter()
        .map(|(runner, dir, paths)| (runner.name, dir, paths))
        .collect();
    let group = |a: &str, b: &str, c: &[&str]| (a.to_string(), b.to_string(), s(c));
    assert_eq!(
        plan,
        [
            group("pytest", "api", &["tests/test_users.py"]),
            group("vitest", "web", &["src/Resources.test.tsx"]),
            group("jest", "packages/ui", &["src/Button.test.jsx", "src/deep/x.spec.ts"]),
            group("go", "svc", &["pkg/a_test.go"]),
            group("cargo", "crate", &["src/lib.rs"]),
        ]
    );
    session
        .compare(&s(&[
            "web/src/Resources.test.tsx",
            "api/tests/test_users.py",
            "packages/ui/src/Button.test.jsx",
        ]))
        .expect("compare");
    // One execution per runner and directory, each run inside its package, paths relative to it.
    let commands: Vec<(String, String)> = session
        .records
        .iter()
        .map(|record| (record.cwd.clone(), record.command.clone()))
        .collect();
    let row = |a: &str, b: &str| (a.to_string(), b.to_string());
    assert_eq!(
        commands,
        [
            row("web", "npx --no-install vitest run src/Resources.test.tsx"),
            row(
                "api",
                "python3 -m pytest -q -rA --no-header -p no:cacheprovider --tb=short tests/test_users.py"
            ),
            row("packages/ui", "npx --no-install jest src/Button.test.jsx"),
        ]
    );
    let chdirs: Vec<String> = calls
        .borrow()
        .iter()
        .map(|argv| {
            let at = argv.iter().position(|part| part == "--chdir").expect("chdir");
            let dir = &argv[at + 1];
            ["web", "api", "packages/ui"]
                .iter()
                .find(|name| dir.ends_with(*name))
                .expect("package")
                .to_string()
        })
        .collect();
    assert_eq!(chdirs, s(&["web", "api", "packages/ui"]));
    // Test ids come back as repository paths.
    let ids: Vec<&str> = session.records[1].tests.iter().map(|item| item.id.as_str()).collect();
    assert_eq!(ids, ["api/tests/test_users.py::test_ok"]);
    let refused = |session: &mut TestSession, paths: &[&str]| match session.run(&s(paths), None, Rev::Head, "run") {
        Ok(_) => "RAN".to_string(),
        Err(error) => error.to_string(),
    };
    assert!(
        refused(&mut session, &["web/src/Resources.test.tsx", "api/tests/test_users.py"])
            .contains("these paths belong to different test runners (vitest in web; pytest in api)")
    );
    assert_eq!(
        refused(&mut session, &["tools/loose.test.ts"]),
        "no test runner is known for tools/loose.test.ts"
    );
}

#[test]
fn review_tests_ts_sandbox_env_has_no_credentials() {
    let env = sandbox_env(Some("/opt/venv/bin"));
    let mut keys: Vec<&str> = env.keys().map(String::as_str).collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "CI",
            "HOME",
            "LANG",
            "NO_COLOR",
            "PATH",
            "PYTHONDONTWRITEBYTECODE",
            "PYTHONHASHSEED",
            "TERM",
            "TMPDIR"
        ]
    );
    assert_eq!(env["HOME"], "/tmp/home");
    assert_eq!(env["PATH"], "/opt/venv/bin:/usr/local/bin:/usr/bin:/bin");
    // Even the sandbox launcher gets only a PATH.
    let launch = launch_env_from(&|name| match name {
        "PATH" => Some("/usr/bin".to_string()),
        "GH_TOKEN" => Some("ghp_shouldneverappear".to_string()),
        "ANTHROPIC_API_KEY" => Some("sk-x".to_string()),
        _ => None,
    });
    assert_eq!(launch.keys().collect::<Vec<_>>(), ["PATH"]);
    let text = format!("{env:?}{launch:?}");
    assert!(!Regex::new("ghp_|sk-|AWS|SSH|TOKEN|KEY").expect("re").is_match(&text));
}

#[test]
fn review_tests_ts_bwrap_argv() {
    let home = "/home/someone".to_string();
    let sb = Sandbox {
        home: home.clone(),
        ..Sandbox::new(Mechanism::Bwrap, None)
    };
    let argv = sb.wrap(
        &s(&["python3", "-m", "pytest"]),
        "/tmp/export-1",
        &sandbox_env(None),
        &[Bind::Same("/opt/venv".to_string())],
        "",
    );
    assert_eq!(
        argv[..7],
        s(&[
            "bwrap",
            "--unshare-all",
            "--die-with-parent",
            "--new-session",
            "--cap-drop",
            "ALL",
            "--clearenv"
        ])
    );
    let joined = argv.join(" ");
    assert!(joined.contains(
        "--bind /tmp/export-1 /tmp/export-1 --ro-bind /opt/venv /opt/venv --chdir /tmp/export-1 -- python3 -m pytest"
    ));
    // Exactly one writable bind; everything else is read-only, a tmpfs or a symlink.
    assert_eq!(argv.iter().filter(|part| *part == "--bind").count(), 1);
    assert!(joined.contains("--tmpfs /tmp --dir /tmp/home"));
    assert!(joined.contains("--setenv HOME /tmp/home"));
    // Nothing of the user's home, the Docker socket, /run or /var is bound.
    let bound: Vec<&String> = argv
        .iter()
        .enumerate()
        .filter(|(index, _)| *index > 0 && ["--ro-bind", "--ro-bind-try", "--bind"].contains(&argv[index - 1].as_str()))
        .map(|(_, part)| part)
        .collect();
    assert!(
        !bound
            .iter()
            .any(|path| **path == home || path.starts_with(&format!("{home}/")))
    );
    let system = Regex::new(r"^/(home|root|run|var|mnt|media)(/|$)").expect("re");
    assert!(!bound.iter().any(|path| system.is_match(path)));
    assert!(!Regex::new(r"docker\.sock|--share-net").expect("re").is_match(&joined));
}

#[test]
fn review_tests_ts_unshare_and_docker_argv() {
    let unshare = Sandbox {
        home: "/home/someone".to_string(),
        ..Sandbox::new(Mechanism::Unshare, None)
    }
    .wrap(&s(&["make", "test"]), "/tmp/export-1", &sandbox_env(None), &[], "");
    assert_eq!(
        unshare[..10],
        s(&[
            "unshare",
            "--user",
            "--map-root-user",
            "--mount",
            "--net",
            "--pid",
            "--fork",
            "--ipc",
            "--uts",
            "--kill-child"
        ])
    );
    assert_eq!(unshare[10..12], s(&["env", "-i"]));
    let script = &unshare[unshare.iter().position(|part| part == "-c").expect("-c") + 1];
    assert!(script.contains("mount -t tmpfs tmpfs /tmp"));
    let after = &unshare[unshare.iter().position(|part| part == "sandbox").expect("sandbox") + 1..];
    assert_eq!(
        after,
        s(&[
            "/tmp/export-1",
            "/home",
            "/root",
            "/run",
            "/media",
            "/srv",
            "/var/run",
            "/var/lib/docker",
            "/home/someone",
            "--",
            "make",
            "test"
        ])
    );
    let docker = Sandbox::new(Mechanism::Docker, Some("python:3.12".to_string())).wrap(
        &s(&["make", "test"]),
        "/tmp/export-1",
        &sandbox_env(None),
        &[Bind::Same("/opt/venv".to_string())],
        "",
    );
    let joined = docker.join(" ");
    assert!(
        joined.contains("docker run --rm --init -i --network none --cap-drop ALL --security-opt no-new-privileges")
    );
    assert_eq!(docker.iter().filter(|part| *part == "-v").count(), 2);
    assert!(joined.contains("-v /tmp/export-1:/tmp/export-1:rw"));
    assert!(joined.contains("-v /opt/venv:/opt/venv:ro"));
    assert!(!joined.contains("docker.sock"));
    // The environment is set inside with `env -i`, under the watcher that ends the container with its client.
    assert!(!docker.contains(&"-e".to_string()));
    let inside = &docker[docker.iter().position(|part| part == "python:3.12").expect("image") + 1..];
    assert_eq!(inside[..2], s(&["sh", "-c"]));
    assert_eq!(inside[3..6], s(&["sh", "env", "-i"]));
    assert!(inside.contains(&"HOME=/tmp/home".to_string()));
    assert_eq!(inside[inside.len() - 2..], s(&["make", "test"]));
}

#[test]
fn review_tests_ts_strongest_working_mechanism_is_chosen() {
    let pick = |have: &[&str], executor: &dyn Executor, image: Option<&str>| {
        let have = s(have);
        detect_sandbox_with(
            image,
            &|name| have.contains(&name.to_string()).then(|| format!("/usr/bin/{name}")),
            executor,
            None,
            &Sandbox::new,
        )
        .map(|sb| sb.mechanism.as_str())
    };
    let ok = |_: &[String], _: &str, _: &Env, _: f64| Ok((0, String::new()));
    let broken =
        |_: &[String], _: &str, _: &Env, _: f64| Ok((1, "bwrap: setting up uid map: Permission denied".to_string()));
    let only_unshare = |argv: &[String], _: &str, _: &Env, _: f64| Ok((i32::from(argv[0] != "unshare"), String::new()));
    let out = [
        pick(&["bwrap", "unshare", "docker"], &ok, Some("img")),
        pick(&["unshare", "docker"], &ok, Some("img")),
        pick(&["bwrap", "unshare"], &only_unshare, None),
        pick(&["docker"], &ok, Some("img")),
        pick(&["docker"], &ok, None),
        pick(&["bwrap", "unshare"], &broken, None),
        pick(&[], &ok, None),
    ];
    assert_eq!(
        out,
        [
            Some("bwrap"),
            Some("unshare"),
            Some("unshare"),
            Some("docker"),
            None,
            None,
            None
        ]
    );
}

#[test]
fn review_tests_ts_real_self_check_on_this_machine() {
    // Skipped when this machine has no sandbox mechanism, as the TypeScript test is. The canary goes into a temp
    // "home", never the real one.
    let (_dir, root) = temp_root();
    let mut checks = Vec::new();
    for mechanism in [Mechanism::Bwrap, Mechanism::Unshare] {
        if let Some(sb) = detect_sandbox(None, Some(mechanism)) {
            checks.push(self_check(&sb, &review::review_tests::ProcessExecutor, &root).expect("self-check"));
        }
    }
    if detect_sandbox(None, None).is_none() || which("python3").is_none() {
        eprintln!("skipped: no sandbox mechanism or python3 on this machine");
        return;
    }
    assert!(!checks.is_empty());
    for check in &checks {
        assert_eq!(
            check,
            &json!({"network": "unreachable", "canary": "unreadable", "token": "absent", "credentialVariables": [],
                    "home": "/tmp/home", "workdir": "writable", "system": "read-only", "dockerSocket": "hidden",
                    "ok": true})
        );
    }
    assert_eq!(
        std::fs::read_dir(&root).expect("root").count(),
        0,
        "the canary is removed"
    );
}

/// The same self-check under Docker, on a local image with python3 (never pulled): skipped with a `SKIPPED` line
/// when there is none or no docker daemon (`backends_support`; ULTRON_SANDBOX_TEST_REQUIRE=docker makes it fail).
#[test]
fn review_tests_real_self_check_under_docker() {
    let (_dir, root) = temp_root();
    let image =
        match backends_support::local_image(&["python:3.12-slim", "python:3.13-slim", "python:3.12", "python:3.13"]) {
            Ok(image) => image,
            Err(reason) => return backends_support::skip("docker", &reason),
        };
    if let Err(reason) = backends_support::machine_runs("docker", Some(&image)) {
        return backends_support::skip("docker", &reason);
    }
    let sb = detect_sandbox(Some(&image), Some(Mechanism::Docker)).expect("docker runs here, so detection finds it");
    // The python images define GPG_KEY (credential-looking): `env -i` inside keeps the image's own ENV out too.
    let check = self_check(&sb, &review::review_tests::ProcessExecutor, &root).expect("self-check");
    assert_eq!(
        check,
        json!({"network": "unreachable", "canary": "unreadable", "token": "absent", "credentialVariables": [],
                "home": "/tmp/home", "workdir": "writable", "system": "read-only", "dockerSocket": "hidden",
                "ok": true})
    );
    assert_eq!(
        std::fs::read_dir(&root).expect("root").count(),
        0,
        "the canary is removed"
    );
}

/// A review test command that times out is stopped inside the sandbox too: `run_process` kills the client's process
/// group, and a container (no process of ours) used to keep running. Every backend this machine runs.
#[test]
fn review_tests_timed_out_commands_stop_inside_every_sandbox() {
    for name in backends_support::machine_backends() {
        let mechanism = Mechanism::parse(name).expect("mechanism");
        let image = (mechanism == Mechanism::Docker).then(|| backends_support::docker_image().expect("image"));
        let sb = Sandbox::new(mechanism, image);
        let (_dir, root) = temp_root();
        let tick = Path::new(&root).join("tick");
        let argv = sb.wrap(
            &s(&["sh", "-c", "while :; do date +%s%N > tick; sleep 0.05; done"]),
            &root,
            &sandbox_env(None),
            &[],
            "",
        );
        // A sandbox can take longer than a short timeout to start under load: grow the timeout until the loop ran
        // before it fired (the last step is the 120 s hang guard).
        let mut output = String::new();
        for timeout in [2.0, 8.0, 30.0, 120.0] {
            let (code, out) = run_process(&argv, &root, &launch_env(), timeout).expect("run");
            assert_eq!(code, 124, "{name}: {out}");
            output = out;
            if tick.exists() {
                break;
            }
        }
        assert!(tick.exists(), "{name}: the loop never ran: {output}");
        // Rewritten every 50 ms while the loop lives: wait until it holds still for 400 ms (hang guard 120 s).
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let before = std::fs::read_to_string(&tick).unwrap_or_default();
            std::thread::sleep(Duration::from_millis(400));
            if before == std::fs::read_to_string(&tick).unwrap_or_default() {
                break;
            }
            assert!(Instant::now() < deadline, "{name}: the command outlived its timeout");
        }
    }
}

/// A checkout with a virtualenv, node_modules at the root and in a package, a secret and a decoy.
fn ts_checkout(root: &str) -> String {
    let dir = format!("{root}/checkout");
    for sub in [
        ".venv/bin",
        ".venv/lib",
        "node_modules/.bin",
        "web/node_modules/.bin",
        "web/src",
        ".git",
        "outside-env/bin",
    ] {
        std::fs::create_dir_all(format!("{dir}/{sub}")).expect("mkdir");
    }
    std::os::unix::fs::symlink("/usr/bin/python3", format!("{dir}/.venv/bin/python")).expect("link");
    std::fs::write(format!("{dir}/.venv/lib/marker.txt"), "site-packages\n").expect("write");
    std::fs::write(format!("{dir}/web/node_modules/.bin/vitest"), "#!/bin/sh\n").expect("write");
    std::fs::write(format!("{dir}/.env"), "SECRET=hunter2\n").expect("write");
    std::fs::write(format!("{dir}/source.py"), "print('checkout source')\n").expect("write");
    std::fs::write(format!("{dir}/.git/config"), "[remote]\n").expect("write");
    dir
}

#[test]
fn review_tests_ts_only_prepared_environments_are_found() {
    let (_dir, root) = temp_root();
    let dir = ts_checkout(&root);
    let elsewhere = format!("{root}/elsewhere");
    std::fs::create_dir_all(format!("{elsewhere}/bin")).expect("mkdir");
    std::os::unix::fs::symlink("/usr/bin/python3", format!("{elsewhere}/bin/python")).expect("link");
    // A "venv" that is a symlink out of the checkout is not an environment of the checkout.
    std::fs::create_dir_all(format!("{dir}/svc")).expect("mkdir");
    std::os::unix::fs::symlink(&elsewhere, format!("{dir}/svc/venv")).expect("link");
    let found = |directory: &str| {
        let out = discover_environment(&dir, directory, None);
        let binds: Vec<(String, String)> = out
            .binds
            .iter()
            .map(|(source, target)| (review::review_tests::relpath(source, &dir), target.clone()))
            .collect();
        (binds, out.python, out.interpreters)
    };
    let pair = |a: &str, b: &str| (a.to_string(), b.to_string());
    assert_eq!(
        found(""),
        (
            vec![pair(".venv", ".venv"), pair("node_modules", "node_modules")],
            Some(".venv/bin/python".to_string()),
            vec![]
        )
    );
    // From a package: its own node_modules first, then the root's; the root virtualenv serves it too.
    assert_eq!(
        found("web/src"),
        (
            vec![
                pair("web/node_modules", "web/node_modules"),
                pair(".venv", ".venv"),
                pair("node_modules", "node_modules")
            ],
            Some(".venv/bin/python".to_string()),
            vec![]
        )
    );
    // The symlinked "venv" is skipped; the root one is used instead.
    assert_eq!(found("svc").1.as_deref(), Some(".venv/bin/python"));
    assert_eq!(
        json!(list_environments(&dir, 3, None)),
        json!([{"path": ".venv", "kind": "virtualenv", "interpreter": "system"},
               {"path": "node_modules", "kind": "node_modules"}, {"path": "web/node_modules", "kind": "node_modules"}])
    );
    assert_eq!(discover_environment("/no/such/checkout", "", None), Default::default());
    let text = format!("{:?}{:?}", found(""), found("web/src"));
    assert!(
        !Regex::new(r#"source\.py|\.git|\.env"|outside-env"#)
            .expect("re")
            .is_match(&text)
    );
}

#[test]
fn review_tests_ts_interpreter_resolved_to_its_install_directory() {
    let (_dir, root) = temp_root();
    let home = format!("{root}/fakehome");
    let install = format!("{home}/.local/share/uv/python/cpython-3.12");
    std::fs::create_dir_all(format!("{install}/bin")).expect("mkdir");
    std::fs::create_dir_all(format!("{install}/lib")).expect("mkdir");
    std::fs::write(format!("{install}/bin/python3.12"), "").expect("write");
    std::os::unix::fs::symlink("python3.12", format!("{install}/bin/python3")).expect("link");
    let dir = format!("{root}/checkout");
    std::fs::create_dir_all(format!("{dir}/.venv/bin")).expect("mkdir");
    std::os::unix::fs::symlink(format!("{install}/bin/python3"), format!("{dir}/.venv/bin/python")).expect("link");
    // A binary directly under the home's bin: binding its "install directory" would bind the home.
    std::fs::create_dir_all(format!("{home}/bin")).expect("mkdir");
    std::fs::create_dir_all(format!("{home}/lib")).expect("mkdir");
    std::fs::write(format!("{home}/bin/python3"), "").expect("write");
    let env = discover_environment(&dir, "", Some(&home));
    let mut binds: Vec<Bind> = env
        .binds
        .iter()
        .map(|(source, target)| Bind::Pair(source.clone(), format!("/tmp/export-1/{target}")))
        .collect();
    binds.extend(env.interpreters.iter().map(|path| Bind::Same(path.clone())));
    let real = realpath(&install);
    assert_eq!(
        interpreter_home(&format!("{dir}/.venv/bin/python"), Some(&home)),
        Some(real.clone())
    );
    assert_eq!(interpreter_home("/usr/bin/python3", Some(&home)), None);
    // Never the home directory itself.
    assert_eq!(interpreter_home(&format!("{home}/bin/python3"), Some(&home)), None);
    assert_eq!(interpreter_home("/no/such/python", Some(&home)), None);
    assert_eq!(env.interpreters, std::slice::from_ref(&real));
    let argv = Sandbox::new(Mechanism::Bwrap, None).wrap(&s(&["x"]), "/tmp/export-1", &sandbox_env(None), &binds, "");
    let joined = argv.join(" ");
    assert!(
        Regex::new(r"--bind /tmp/export-1 /tmp/export-1 --ro-bind \S+/\.venv /tmp/export-1/\.venv --ro-bind ")
            .expect("re")
            .is_match(&joined)
    );
    assert!(joined.contains(&format!("--ro-bind {real} {real} --chdir")));
    assert_eq!(argv.iter().filter(|part| *part == "--bind").count(), 1);
}

/// base: KINDS has a and b, and the parametrized test covers both. head: KINDS gains c; the test file is as before.
fn ts_fixture(root: &str) -> (String, String, String) {
    let dir = format!("{root}/repo");
    std::fs::create_dir_all(format!("{dir}/src")).expect("mkdir");
    std::fs::create_dir_all(format!("{dir}/tests")).expect("mkdir");
    std::fs::write(format!("{dir}/pytest.ini"), "[pytest]\n").expect("write");
    std::fs::write(
        format!("{dir}/src/app.py"),
        "KINDS = [\"a\", \"b\"]\n\n\ndef show(kind):\n    return kind.upper()\n",
    )
    .expect("write");
    std::fs::write(
        format!("{dir}/tests/test_app.py"),
        "from app import KINDS, show\n\n\ndef test_kinds():\n    assert KINDS == [\"a\", \"b\"]\n\n\ndef test_show():\n    assert show(\"a\")\n",
    )
    .expect("write");
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "base"]);
    let base = git(&dir, &["rev-parse", "HEAD"]);
    std::fs::write(
        format!("{dir}/src/app.py"),
        "KINDS = [\"a\", \"b\", \"c\"]\n\n\ndef show(kind):\n    return kind.upper()\n",
    )
    .expect("write");
    git(&dir, &["commit", "-qam", "add c"]);
    let head = git(&dir, &["rev-parse", "HEAD"]);
    (dir, base, head)
}

/// A scripted call: argv, cwd, env, timeout, and the exported src/app.py it saw.
type Executed = Rc<RefCell<Vec<(Vec<String>, String, Env, f64, String)>>>;
/// argv and cwd.
type Call = (Vec<String>, String);

struct TsSession {
    session: TestSession,
    executed: Executed,
    exports: Rc<RefCell<Vec<String>>>,
}

/// A session over the fixture whose executor is scripted: it "runs pytest" by reading the exported tree.
fn ts_session(
    repo: &(String, String, String),
    sandbox: Sandbox,
    configure: impl FnOnce(&mut SessionOptions),
) -> TsSession {
    let (dir, base, head) = repo;
    let tracked: Vec<String> = git(dir, &["ls-tree", "-r", "--name-only", head])
        .lines()
        .map(str::to_string)
        .collect();
    let root = dir.clone();
    let read = move |path: &str| std::fs::read_to_string(format!("{root}/{path}")).ok();
    let executed: Executed = Rc::default();
    let exports: Rc<RefCell<Vec<String>>> = Rc::default();
    let (seen, made) = (executed.clone(), exports.clone());
    let mut options = SessionOptions {
        executor: Box::new(move |argv: &[String], cwd: &str, env: &Env, timeout: f64| {
            let source = std::fs::read_to_string(format!("{cwd}/src/app.py")).expect("app.py");
            seen.borrow_mut()
                .push((argv.to_vec(), cwd.to_string(), env.clone(), timeout, source.clone()));
            let kinds = if source.contains("\"c\"") {
                "FAILED tests/test_app.py::test_kinds - AssertionError: assert ['a', 'b', 'c'] == ['a', 'b']"
            } else {
                "PASSED tests/test_app.py::test_kinds"
            };
            let show = if source.contains("upper()") {
                "PASSED tests/test_app.py::test_show"
            } else {
                "FAILED tests/test_app.py::test_show - AssertionError"
            };
            Ok((
                i32::from(format!("{kinds}{show}").contains("FAILED")),
                format!("{kinds}\n{show}\n"),
            ))
        }),
        export: Box::new(move |root: &str, rev: &str| {
            let path = export_commit(root, rev)?;
            made.borrow_mut().push(path.clone());
            Ok(path)
        }),
        ..SessionOptions::default()
    };
    configure(&mut options);
    let session = TestSession::new(dir, head, Some(base), &tracked, Box::new(read), sandbox, options);
    TsSession {
        session,
        executed,
        exports,
    }
}

#[test]
fn review_tests_ts_requests_are_validated() {
    let (_dir, root) = temp_root();
    let repo = ts_fixture(&root);
    let mut ts = ts_session(&repo, Sandbox::new(Mechanism::Bwrap, None), |_| {});
    let s1 = &mut ts.session;
    let one = |text: &str| Some(json!([text]));
    let mut bad: Vec<String> = Vec::new();
    let mut refuse =
        |result: Result<(), TestsError>| bad.push(result.map_or_else(|e| e.to_string(), |()| "RAN".into()));
    refuse(
        s1.paths(one("../outside").as_ref())
            .map(|_| ())
            .map_err(TestsError::Rejected),
    );
    refuse(
        s1.paths(one("/etc/passwd").as_ref())
            .map(|_| ())
            .map_err(TestsError::Rejected),
    );
    refuse(
        s1.paths(one("tests/missing.py").as_ref())
            .map(|_| ())
            .map_err(TestsError::Rejected),
    );
    refuse(s1.paths(one("-rf").as_ref()).map(|_| ()).map_err(TestsError::Rejected));
    refuse(s1.paths(Some(&json!([]))).map(|_| ()).map_err(TestsError::Rejected));
    refuse(
        s1.paths(Some(&json!(vec!["tests"; 20])))
            .map(|_| ())
            .map_err(TestsError::Rejected),
    );
    refuse(
        s1.run(&s(&["tests/test_app.py"]), Some("x; rm -rf /"), Rev::Head, "run")
            .map(|_| ()),
    );
    refuse(
        s1.run(&s(&["tests/test_app.py"]), Some("$(id)"), Rev::Head, "run")
            .map(|_| ()),
    );
    let tests = json!(["tests/test_app.py"]);
    let path = json!("src/app.py");
    refuse(
        s1.mutation(Some(&path), Some(&json!(1)), Some(&json!("a\nb")), Some(&tests))
            .map(|_| ()),
    );
    refuse(
        s1.mutation(Some(&path), Some(&json!(99)), Some(&json!("x")), Some(&tests))
            .map(|_| ()),
    );
    refuse(
        s1.mutation(
            Some(&json!("src/nope.py")),
            Some(&json!(1)),
            Some(&json!("x")),
            Some(&tests),
        )
        .map(|_| ()),
    );
    refuse(
        s1.mutation(
            Some(&path),
            Some(&json!(1)),
            Some(&json!("x")),
            Some(&json!(["../tests"])),
        )
        .map(|_| ()),
    );
    assert!(!bad.contains(&"RAN".to_string()));
    assert_eq!(bad.len(), 12);
    assert!(bad[0].contains("is not a path inside the repository"));
    assert_eq!(
        bad[2],
        "tests/missing.py is not a tracked file or directory at the reviewed commit"
    );
    assert_eq!(bad[6], "select must be a short test name expression");
    assert!(bad[8].contains("replacement must be one line"));
    assert_eq!(bad[9], "src/app.py has no line 99");
    // Nothing was executed for any refused request.
    assert_eq!(ts.executed.borrow().len(), 0);
    assert_eq!(
        ts.session
            .paths(Some(&json!(["tests", "tests/test_app.py"])))
            .expect("ok"),
        s(&["tests", "tests/test_app.py"])
    );
}

#[test]
fn review_tests_ts_every_execution_is_sandboxed_exported_limited() {
    let (_dir, root) = temp_root();
    let repo = ts_fixture(&root);
    let env_dir = repo.0.clone();
    let mut ts = ts_session(&repo, Sandbox::new(Mechanism::Bwrap, None), |options| {
        options.runs = 2;
        options.timeout_s = 45.0;
        options.env_dir = Some(env_dir);
    });
    let first = ts
        .session
        .run(&s(&["tests/test_app.py"]), None, Rev::Head, "run")
        .expect("first");
    ts.session
        .run(&s(&["tests/test_app.py"]), Some("test_show"), Rev::Head, "run")
        .expect("second");
    let limit = ts
        .session
        .run(&s(&["tests/test_app.py"]), None, Rev::Head, "run")
        .map_or_else(|e| e.to_string(), |_| "RAN".into());
    assert_eq!(
        (
            first.n,
            first.kind.as_str(),
            first.rev.as_str(),
            first.status.as_str(),
            first.passed,
            first.failed
        ),
        (1, "run", "head", "failed", 1, 1)
    );
    assert_eq!(
        first.command,
        "python3 -m pytest -q -rA --no-header -p no:cacheprovider --tb=short tests/test_app.py"
    );
    let (argv, cwd, env, timeout, _) = ts.executed.borrow()[0].clone();
    // The command is the sandbox's, run in a temporary export (not the repository), with the time limit.
    assert_eq!(argv[0], "bwrap");
    assert!(argv.contains(&"--unshare-all".to_string()));
    let joined = argv.join(" ");
    assert!(joined.contains(&format!("--bind {cwd} {cwd}")));
    assert!(joined.contains(&format!("--ro-bind {0} {0}", repo.0)));
    assert!(!cwd.starts_with(&repo.0));
    assert_eq!(timeout, 45.0);
    assert_eq!(env.keys().collect::<Vec<_>>(), ["PATH"]);
    assert_eq!(limit, "the limit of 2 test executions per review is reached");
    assert_eq!(ts.session.records.len(), 2);
    ts.session.close();
    // The export is removed when the session closes.
    assert!(!Path::new(&cwd).exists());
    let summary = summarize(&first);
    assert!(summary.contains("-> failed (1 passed, 1 failed)"));
    assert!(summary.contains("failed: tests/test_app.py::test_kinds - AssertionError"));
}

#[test]
fn review_tests_ts_base_versus_head() {
    let (_dir, root) = temp_root();
    let repo = ts_fixture(&root);
    let mut ts = ts_session(&repo, Sandbox::new(Mechanism::Bwrap, None), |_| {});
    let compared = ts.session.compare(&s(&["tests/test_app.py"])).expect("compare");
    let ids: Vec<&str> = compared.regressions.iter().map(|item| item.test.id.as_str()).collect();
    assert_eq!(ids, ["tests/test_app.py::test_kinds"]);
    let revs: Vec<&str> = ts.session.records.iter().map(|record| record.rev.as_str()).collect();
    assert_eq!(revs, ["head", "base"]);
    // Each regression knows the two runs that showed it.
    let pairs: Vec<(usize, usize)> = compared
        .regressions
        .iter()
        .map(|item| (item.head.n, item.base.n))
        .collect();
    assert_eq!(pairs, [(1, 2)]);
    // A test that already failed at the base commit: not this change's.
    let mut old = ts_session(&repo, Sandbox::new(Mechanism::Bwrap, None), |options| {
        options.executor = Box::new(|_: &[String], _: &str, _: &Env, _: f64| {
            Ok((
                1,
                "FAILED tests/test_app.py::test_kinds - AssertionError\nPASSED tests/test_app.py::test_show"
                    .to_string(),
            ))
        });
    });
    let before = old.session.compare(&s(&["tests/test_app.py"])).expect("compare");
    assert!(before.regressions.is_empty());
    assert_eq!(old.session.records.len(), 2);
    // Everything passes at head: the base commit is not run at all.
    let mut passing = ts_session(&repo, Sandbox::new(Mechanism::Bwrap, None), |options| {
        options.executor = Box::new(|_: &[String], _: &str, _: &Env, _: f64| {
            Ok((0, "PASSED tests/test_app.py::test_kinds".to_string()))
        });
    });
    let clean = passing.session.compare(&s(&["tests/test_app.py"])).expect("compare");
    assert_eq!(passing.session.records.len(), 1);
    assert!(clean.regressions.is_empty());
}

#[test]
fn review_tests_ts_mutation_applied_and_reverted() {
    let (_dir, root) = temp_root();
    let repo = ts_fixture(&root);
    let mut ts = ts_session(&repo, Sandbox::new(Mechanism::Bwrap, None), |_| {});
    let tests = json!(["tests/test_app.py"]);
    let path = json!("src/app.py");
    let caught = ts
        .session
        .mutation(
            Some(&path),
            Some(&json!(5)),
            Some(&json!("    return kind")),
            Some(&tests),
        )
        .expect("caught");
    let during = ts.executed.borrow().last().expect("call").4.clone();
    let exported = format!("{}/src/app.py", ts.exports.borrow()[0]);
    let after = std::fs::read_to_string(&exported).expect("after");
    // A change the tests do not notice: both outcomes equal the unmutated run.
    let survived = ts
        .session
        .mutation(Some(&path), Some(&json!(2)), Some(&json!("# nothing")), Some(&tests))
        .expect("survived");
    ts.session
        .set_executor(Box::new(|_: &[String], _: &str, _: &Env, _: f64| {
            Err("the sandbox could not start".to_string())
        }));
    let error = ts.session.mutation(
        Some(&path),
        Some(&json!(5)),
        Some(&json!("    return None")),
        Some(&tests),
    );
    let after_error = std::fs::read_to_string(&exported).expect("after");
    let original = "KINDS = [\"a\", \"b\", \"c\"]\n\n\ndef show(kind):\n    return kind.upper()\n";
    assert_eq!(
        during,
        "KINDS = [\"a\", \"b\", \"c\"]\n\n\ndef show(kind):\n    return kind\n"
    );
    assert_eq!(after, original);
    assert_eq!(after_error, original);
    assert_eq!(
        error.map_err(|e| e.to_string()).err().as_deref(),
        Some("the sandbox could not start")
    );
    assert_eq!(
        std::fs::read_to_string(format!("{}/src/app.py", repo.0)).expect("repo"),
        original
    );
    assert_eq!((caught.kind.as_str(), caught.caught), ("mutation", Some(true)));
    let mutation = caught.mutation.clone().expect("mutation");
    assert_eq!(
        (
            mutation.path.as_str(),
            mutation.line,
            mutation.was.as_str(),
            mutation.replacement.as_str()
        ),
        ("src/app.py", 5, "    return kind.upper()", "    return kind")
    );
    assert!(summarize(&caught).contains(
        "with src/app.py:5 changed from `    return kind.upper()` to `    return kind`: the tests caught it"
    ));
    assert_eq!(survived.kind, "mutation");
}

#[test]
fn review_tests_ts_checkout_environments_in_a_run() {
    let (_dir, root) = temp_root();
    let repo = ts_fixture(&root);
    let checkout = realpath(&ts_checkout(&root));
    let co = checkout.clone();
    let mut ts = ts_session(&repo, Sandbox::new(Mechanism::Bwrap, None), |options| {
        options.checkout = Some(co);
    });
    let record = ts
        .session
        .run(&s(&["tests/test_app.py"]), None, Rev::Head, "run")
        .expect("run");
    let workdir = ts.executed.borrow()[0].1.clone();
    let strip = |argv: &[String]| -> Vec<String> {
        argv.iter()
            .map(|part| part.replace(&workdir, "<export>").replace(&checkout, "<checkout>"))
            .collect()
    };
    let argv = strip(&ts.executed.borrow()[0].0);
    // pytest runs with the virtualenv's python, which sits in the export where the checkout has it.
    assert_eq!(
        record.command.replace(&workdir, "<export>"),
        "<export>/.venv/bin/python -m pytest -q -rA --no-header -p no:cacheprovider --tb=short tests/test_app.py"
    );
    assert_eq!(record.environment, s(&[".venv", "node_modules"]));
    let joined = argv.join(" ");
    assert!(joined.contains("--bind <export> <export> --ro-bind <checkout>/.venv <export>/.venv --ro-bind <checkout>/node_modules <export>/node_modules --chdir <export> -- <export>/.venv/bin/python -m pytest"));
    // The checkout itself, its source, its .git and its .env are never bound.
    let sources: Vec<&String> = argv
        .iter()
        .enumerate()
        .filter(|(index, part)| *index > 0 && argv[index - 1] == "--ro-bind" && part.starts_with("<checkout>"))
        .map(|(_, part)| part)
        .collect();
    assert_eq!(sources, ["<checkout>/.venv", "<checkout>/node_modules"]);
    assert!(
        !Regex::new(r"<checkout>( |$)|\.git|\.env|source\.py")
            .expect("re")
            .is_match(&joined)
    );
    ts.session.close();

    // vitest from the package's own node_modules, run in the package.
    let calls: Rc<RefCell<Vec<Call>>> = Rc::default();
    let seen = calls.clone();
    let files = [
        (
            "web/package.json".to_string(),
            json!({"devDependencies": {"vitest": "1"}}).to_string(),
        ),
        ("web/src/a.test.ts".to_string(), String::new()),
    ];
    let lookup = files.clone();
    let mut js = TestSession::new(
        "/nowhere",
        "HEAD",
        None,
        &files.iter().map(|(a, _)| a.clone()).collect::<Vec<_>>(),
        Box::new(move |path| lookup.iter().find(|(a, _)| a == path).map(|(_, b)| b.clone())),
        Sandbox::new(Mechanism::Bwrap, None),
        SessionOptions {
            checkout: Some(checkout.clone()),
            executor: Box::new(move |argv: &[String], cwd: &str, _: &Env, _: f64| {
                seen.borrow_mut().push((argv.to_vec(), cwd.to_string()));
                Ok((0, String::new()))
            }),
            export: Box::new(|_: &str, _: &str| make_temp_dir("ultron-autoreview-run-")),
            ..SessionOptions::default()
        },
    );
    js.run(&s(&["web/src/a.test.ts"]), None, Rev::Head, "run")
        .expect("vitest");
    let (vargv, vcwd) = calls.borrow()[0].clone();
    let vitest: Vec<String> = vargv
        .iter()
        .map(|part| part.replace(&vcwd, "<export>").replace(&checkout, "<checkout>"))
        .collect();
    let vjoined = vitest.join(" ");
    assert!(vjoined.contains("--ro-bind <checkout>/web/node_modules <export>/web/node_modules --ro-bind <checkout>/.venv <export>/.venv --ro-bind <checkout>/node_modules <export>/node_modules"));
    assert!(vjoined.contains("--chdir <export>/web -- <export>/web/node_modules/.bin/vitest run src/a.test.ts"));
    js.close();

    // autoreview.testEnv wins: the checkout's environments are then not looked for.
    let (co, env) = (checkout.clone(), checkout.clone());
    let mut over = ts_session(&repo, Sandbox::new(Mechanism::Bwrap, None), |options| {
        options.checkout = Some(co);
        options.env_dir = Some(env);
    });
    over.session
        .run(&s(&["tests/test_app.py"]), None, Rev::Head, "run")
        .expect("override");
    let override_argv = over.executed.borrow()[0]
        .0
        .join(" ")
        .replace(&over.executed.borrow()[0].1, "<export>")
        .replace(&checkout, "<checkout>");
    assert!(override_argv.contains("--ro-bind <checkout> <checkout> --chdir "));
    assert!(override_argv.contains(" -- python3 -m pytest"));
    assert!(!override_argv.contains(".venv"));
    // unshare cannot place binds: it runs without the environment rather than exposing anything.
    let co = checkout.clone();
    let mut plain = ts_session(&repo, Sandbox::new(Mechanism::Unshare, None), |options| {
        options.checkout = Some(co);
    });
    plain
        .session
        .run(&s(&["tests/test_app.py"]), None, Rev::Head, "run")
        .expect("unshare");
    assert!(!plain.executed.borrow()[0].0.join(" ").contains(".venv"));
}

#[test]
fn review_tests_ts_real_sandboxed_make_run() {
    // Skipped without a sandbox mechanism or make, as the TypeScript test is.
    let Some(sb) = detect_sandbox(None, None) else {
        eprintln!("skipped: no sandbox mechanism on this machine");
        return;
    };
    if which("make").is_none() {
        eprintln!("skipped: make is not installed");
        return;
    }
    let (_dir, root) = temp_root();
    let dir = format!("{root}/real");
    std::fs::create_dir_all(&dir).expect("mkdir");
    git(&dir, &["init", "-q", "-b", "main"]);
    // The test passes when value.txt holds 1; it also tries to leave a file behind and to read the real home.
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    std::fs::write(
        format!("{dir}/Makefile"),
        format!("test:\n\t@touch ran-here.txt\n\t@echo \"home=$$HOME\"\n\t@ls -A {home} 2>/dev/null | head -1 | sed \"s/^/seen=/\"\n\t@test \"$$(cat value.txt)\" = \"1\"\n"),
    )
    .expect("write");
    std::fs::write(format!("{dir}/value.txt"), "1\n").expect("write");
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "base"]);
    let base = git(&dir, &["rev-parse", "HEAD"]);
    std::fs::write(format!("{dir}/value.txt"), "2\n").expect("write");
    git(&dir, &["commit", "-qam", "break it"]);
    let head = git(&dir, &["rev-parse", "HEAD"]);
    let tracked: Vec<String> = git(&dir, &["ls-tree", "-r", "--name-only", &head])
        .lines()
        .map(str::to_string)
        .collect();
    let reader = dir.clone();
    let mut session = TestSession::new(
        &dir,
        &head,
        Some(&base),
        &tracked,
        Box::new(move |path| std::fs::read_to_string(format!("{reader}/{path}")).ok()),
        sb,
        SessionOptions {
            timeout_s: 60.0,
            ..SessionOptions::default()
        },
    );
    let compared = session.compare(&s(&["value.txt"])).expect("compare");
    let exported = session.exports();
    session.close();
    // Failed at the head commit, passed at the base commit: a regression, observed by really running both.
    let head_run = &compared.runs[0];
    assert_eq!(
        (
            head_run.status.as_str(),
            head_run.rev.as_str(),
            head_run.command.as_str()
        ),
        ("failed", "head", "make test")
    );
    assert_eq!(compared.regressions.len(), 1);
    let base_run = &compared.regressions[0].base;
    assert_eq!((base_run.status.as_str(), base_run.rev.as_str()), ("passed", "base"));
    // Inside: the sandbox's own empty HOME, and nothing of the real one.
    assert!(base_run.output.contains("home=/tmp/home"));
    assert!(!base_run.output.contains("seen="));
    // The test wrote into its export, not into the repository; the exports are removed.
    assert!(!Path::new(&format!("{dir}/ran-here.txt")).exists());
    assert!(exported.iter().all(|path| !Path::new(path).exists()));
}

#[test]
fn review_tests_serve_replies_like_the_deep_pass() {
    let (_dir, root) = temp_root();
    let repo = ts_fixture(&root);
    let mut ts = ts_session(&repo, Sandbox::new(Mechanism::Bwrap, None), |options| options.runs = 2);
    let (label, summary) = ts
        .session
        .serve("run_tests", &json!({"paths": ["tests/test_app.py"]}))
        .expect("served");
    assert_eq!(label, "test run 1");
    assert!(summary.starts_with("run 1 (run, at the head commit, sandboxed, no network): `python3 -m pytest"));
    let refused = ts
        .session
        .serve("run_tests", &json!({"paths": ["../x"]}))
        .expect_err("refused");
    assert_eq!(refused.to_string(), "'../x' is not a path inside the repository");
    let (label, summary) = ts
        .session
        .serve(
            "mutation_check",
            &json!({"path": "src/app.py", "line": 5, "replacement": "    return kind",
                                         "tests": ["tests/test_app.py"]}),
        )
        .expect("served");
    assert_eq!(label, "test run 2");
    assert!(summary.contains(": the tests caught it"));
    assert_eq!(ts.session.left(), 0);
}
