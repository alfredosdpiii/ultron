//! Replays crates/review/fixtures/reference.json, recorded from autoreview_reference.py (fixtures/record_reference.py):
//! the TypeScript scanner, the Python indexer (the embedded script run by the kernel's interpreter against the
//! module's own `index_python`), the YAML reader, the Terraform scanner, the module map, and whole references over
//! git repositories rebuilt here from the recorded steps (index, lookups, views, block, facts, structural findings,
//! served requests). Plus the cases of packages/coding-agent/test/ultron-autoreview-reference.test.ts with their
//! literal expectations.

use std::collections::HashMap;
use std::path::Path;

use review::diff::parse_diff;
use review::reference::{
    CallSite, FileIndex, GitRepo, NoPython, ProcessPython, PythonIndex, Reference, Repo, Symbol, module_map, serve,
};
use review::reference_tf::{TfBlock, parse_terraform};
use review::reference_ts::{index_ts, ts_block_end};
use review::reference_yaml::{Entry, Step, YamlNode, parse_yaml, split_flow, strip_comment, yaml_get, yaml_scalar};
use review::scope::{ProcessRunner, Runner};
use serde_json::{Value, json};

fn fixture() -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/reference.json");
    serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("fixture json")
}

fn python() -> ProcessPython {
    ProcessPython::from_env()
}

fn no_clock() -> f64 {
    0.0
}

fn index_json(index: &FileIndex) -> Value {
    serde_json::to_value(index).expect("index json")
}

fn node_json(node: Option<&YamlNode>) -> Value {
    let Some(node) = node else {
        return Value::Null;
    };
    let lines: Vec<Value> = node
        .lines
        .iter()
        .map(|(entry, line)| match entry {
            Entry::Key(key) => json!([key.to_json(), line]),
            Entry::Index(index) => json!([index, line]),
        })
        .collect();
    json!({"value": node.value.to_json(), "line": node.line, "lines": lines})
}

fn tf_json(block: &TfBlock) -> Value {
    json!({
        "kind": block.kind, "type": block.r#type, "name": block.name, "path": block.path, "line": block.line,
        "end": block.end, "address": block.address(),
        "attrs": block.attrs.iter().map(|(k, v)| json!([k, v])).collect::<Vec<_>>(),
        "nested": block.nested.iter().map(|(k, v)| json!([k, v.iter().map(|(a, b)| json!([a, b])).collect::<Vec<_>>()])).collect::<Vec<_>>(),
        "refs": block.refs,
    })
}

fn call_json(call: &CallSite) -> Value {
    json!([
        call.path,
        call.line,
        call.callee,
        call.text,
        call.enclosing,
        call.targets
    ])
}

fn keys(symbols: &[&Symbol]) -> Vec<String> {
    symbols.iter().map(|symbol| symbol.key()).collect()
}

fn texts(value: &Value) -> Vec<String> {
    serde_json::from_value(value.clone()).expect("strings")
}

// --- Readers ------------------------------------------------------------------------------------------------------

#[test]
fn review_reference_ts_block_end_matches_python() {
    let fx = fixture();
    for case in fx["block_ends"].as_array().expect("cases") {
        let lines = texts(&case[0]);
        let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
        let start = case[1].as_u64().expect("start") as usize;
        assert_eq!(
            ts_block_end(&lines, start) as u64,
            case[2].as_u64().expect("end"),
            "{case}"
        );
    }
}

#[test]
fn review_reference_index_ts_matches_python() {
    let fx = fixture();
    for case in fx["ts"].as_array().expect("cases") {
        let path = case["path"].as_str().expect("path");
        let index = index_ts(path, case["text"].as_str().expect("text"));
        assert_eq!(index_json(&index), case["index"], "{path}");
    }
}

#[test]
fn review_reference_python_indexer_matches_python() {
    let fx = fixture();
    let cases = fx["python"].as_array().expect("cases");
    let files: Vec<(String, String)> = cases
        .iter()
        .map(|case| {
            (
                case["path"].as_str().expect("path").to_string(),
                case["text"].as_str().expect("text").to_string(),
            )
        })
        .collect();
    let mut indexer = python();
    // Two requests to the one interpreter: the whole batch, then one file again.
    let records = indexer
        .index(&files)
        .expect("the kernel's interpreter indexes Python files");
    assert_eq!(records.len(), cases.len());
    for (record, case) in records.iter().zip(cases) {
        assert_eq!(index_json(record), case["index"], "{}", case["path"]);
    }
    let again = indexer.index(&files[1..2]).expect("a second request");
    assert_eq!(index_json(&again[0]), cases[1]["index"]);
    assert_eq!(indexer.index(&[]).map(|records| records.len()), Some(0));
}

#[test]
fn review_reference_yaml_matches_python() {
    let fx = fixture();
    for case in fx["scalars"].as_array().expect("scalars") {
        let got = match yaml_scalar(case[0].as_str().expect("text")) {
            Ok(value) => json!({"ok": value.to_json()}),
            Err(_) => json!({"error": "TypeError"}),
        };
        assert_eq!(got, case[1], "{}", case[0]);
    }
    for case in fx["flows"].as_array().expect("flows") {
        assert_eq!(
            json!(split_flow(case[0].as_str().expect("text"))),
            case[1],
            "{}",
            case[0]
        );
    }
    for case in fx["comments"].as_array().expect("comments") {
        assert_eq!(
            json!(strip_comment(case[0].as_str().expect("text"))),
            case[1],
            "{}",
            case[0]
        );
    }
    for case in fx["yaml"].as_array().expect("yaml") {
        let got = match parse_yaml(case[0].as_str().expect("text")) {
            Ok(node) => json!({"ok": node_json(node.as_ref())}),
            Err(_) => json!({"error": "TypeError"}),
        };
        assert_eq!(got, case[1], "{}", case[0]);
    }
    let doc = parse_yaml(fx["yaml"][0][0].as_str().expect("doc"))
        .expect("parses")
        .expect("a node");
    for case in fx["yaml_gets"].as_array().expect("gets") {
        let steps: Vec<Step<'_>> = case[0]
            .as_array()
            .expect("keys")
            .iter()
            .map(|key| match key {
                Value::String(text) => Step::Key(text),
                number => Step::Index(number.as_u64().expect("index") as usize),
            })
            .collect();
        let got = yaml_get(&doc.value, &steps).map_or(Value::Null, |value| value.to_json());
        assert_eq!(got, case[1], "{}", case[0]);
    }
}

#[test]
fn review_reference_terraform_matches_python() {
    let fx = fixture();
    for case in fx["terraform"].as_array().expect("cases") {
        let blocks = parse_terraform("infra/main.tf", case[0].as_str().expect("text"));
        assert_eq!(
            json!(blocks.iter().map(tf_json).collect::<Vec<_>>()),
            case[1],
            "{}",
            case[0]
        );
    }
}

#[test]
fn review_reference_module_map_matches_python() {
    let fx = fixture();
    for case in fx["module_maps"].as_array().expect("cases") {
        let paths = texts(&case[0]);
        let expected: HashMap<String, String> = serde_json::from_value(case[1].clone()).expect("map");
        assert_eq!(module_map(&paths), expected, "{paths:?}");
    }
}

// --- Repositories -------------------------------------------------------------------------------------------------

fn git_runner(fx: &Value) -> ProcessRunner {
    let env: HashMap<String, String> = serde_json::from_value(fx["git_env"].clone()).expect("env");
    ProcessRunner {
        env: env.into_iter().collect(),
    }
}

fn build_repo(base: &Path, steps: &Value, runner: &ProcessRunner) {
    std::fs::create_dir_all(base).expect("repo dir");
    for step in steps.as_array().expect("steps") {
        let path = base.join(step[1].as_str().unwrap_or(""));
        match step[0].as_str().expect("op") {
            "write" => {
                std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
                std::fs::write(&path, step[2].as_str().expect("text")).expect("write");
            }
            "rm" => std::fs::remove_file(&path).expect("rm"),
            "git" => {
                let argv: Vec<String> = serde_json::from_value(step.clone()).expect("argv");
                let out = runner.run(&argv, base.to_str().expect("utf-8 path"), 30);
                assert_eq!(out.code, 0, "{step}: {}", out.stderr);
            }
            other => panic!("unknown step {other}"),
        }
    }
}

/// The recorded repositories, rebuilt under a temp dir.
struct Repos {
    _temp: tempfile::TempDir,
    root: std::path::PathBuf,
    runner: ProcessRunner,
}

impl Repos {
    fn new(fx: &Value) -> Self {
        let temp = tempfile::tempdir().expect("temp");
        let root = std::fs::canonicalize(temp.path()).expect("real temp");
        let runner = git_runner(fx);
        for (name, steps) in fx["repos"].as_object().expect("repos") {
            build_repo(&root.join(name), steps, &runner);
        }
        Self {
            _temp: temp,
            root,
            runner,
        }
    }

    fn dir(&self, name: &str) -> String {
        self.root.join(name).to_str().expect("utf-8").to_string()
    }

    fn repo(&self, name: &str, rev: &str) -> GitRepo<'_> {
        GitRepo::new(&self.dir(name), rev, &self.runner)
    }

    fn diff(&self, name: &str) -> Vec<review::diff::FileDiff> {
        let argv: Vec<String> = ["git", "diff", "-U3", "base", "head", "--"].map(String::from).to_vec();
        let out = self.runner.run(&argv, &self.dir(name), 30);
        assert_eq!(out.code, 0, "{}", out.stderr);
        parse_diff(&out.stdout)
    }
}

fn build<'a>(
    repo: &'a dyn Repo,
    files: &[review::diff::FileDiff],
    base: Option<&'a dyn Repo>,
    limits: &Value,
) -> Reference<'a> {
    let mut reference = Reference::new(repo, files, base);
    if let Some(max) = limits["max_files"].as_u64() {
        reference.max_files = max as usize;
    }
    if let Some(max) = limits["max_bytes"].as_u64() {
        reference.max_bytes = max as usize;
    }
    reference.build(&mut python(), &no_clock)
}

fn served(reference: &Reference<'_>, kind: &str, args: &Value) -> Value {
    serve(reference, kind, args).map_or(Value::Null, |(title, body)| json!([title, body]))
}

#[test]
fn review_reference_repositories_match_python() {
    let fx = fixture();
    let repos = Repos::new(&fx);
    for case in fx["references"].as_array().expect("cases") {
        let name = case["repo"].as_str().expect("repo");
        let label = format!("{name} base={} limits={}", case["base"], case["limits"]);
        let repo = repos.repo(name, "head");
        let base_repo = repos.repo(name, "base");
        let base: Option<&dyn Repo> = if case["base"].as_bool().expect("base") {
            Some(&base_repo)
        } else {
            None
        };
        let files = repos.diff(name);
        let mut reference = build(&repo, &files, base, &case["limits"]);
        let mut stats = serde_json::to_value(&reference.stats).expect("stats");
        assert_eq!(stats["python_unindexed"], json!(0), "{label}");
        stats.as_object_mut().expect("object").shift_remove("python_unindexed");
        assert_eq!(stats, case["stats"], "{label}");
        assert!(reference.notes.is_empty(), "{label}");
        let indexes: Vec<Value> = reference.index.values().map(index_json).collect();
        assert_eq!(json!(indexes), case["index"], "{label}: index");
        let bases: Vec<Value> = reference.base_index.values().map(index_json).collect();
        assert_eq!(json!(bases), case["base_index"], "{label}: base index");
        let workflows: Vec<Value> = reference
            .workflows
            .iter()
            .map(|(path, node)| json!([path, node_json(Some(node))]))
            .collect();
        assert_eq!(json!(workflows), case["workflows"], "{label}: workflows");
        let terraform: Vec<Value> = reference
            .terraform
            .iter()
            .map(|(path, blocks)| json!([path, blocks.iter().map(tf_json).collect::<Vec<_>>()]))
            .collect();
        assert_eq!(json!(terraform), case["terraform"], "{label}: terraform");
        let changed: Vec<Value> = reference
            .changed_lines
            .iter()
            .map(|(path, lines)| json!([path, lines]))
            .collect();
        assert_eq!(json!(changed), case["changed_lines"], "{label}: changed lines");
        let removed: Vec<Value> = reference
            .removed_text
            .iter()
            .map(|(path, lines)| json!([path, lines]))
            .collect();
        assert_eq!(json!(removed), case["removed_text"], "{label}: removed text");
        for (key, calls) in case["callers"].as_object().expect("callers") {
            let got: Vec<Value> = reference.callers(key, true).into_iter().map(call_json).collect();
            assert_eq!(json!(got), *calls, "{label}: callers {key}");
            let got: Vec<Value> = reference.callers(key, false).into_iter().map(call_json).collect();
            assert_eq!(json!(got), case["callers_all"][key], "{label}: all callers {key}");
            let got: Vec<Value> = reference.callees(key).into_iter().map(call_json).collect();
            assert_eq!(json!(got), case["callees"][key], "{label}: callees {key}");
            assert_eq!(
                json!(reference.tests_of(key)),
                case["tests_of"][key],
                "{label}: tests of {key}"
            );
            assert_eq!(
                json!(keys(&reference.symbols_named(key))),
                case["symbols_named"][key],
                "{label}: {key}"
            );
        }
        assert_eq!(
            json!(keys(&reference.changed_symbols())),
            case["changed_symbols"],
            "{label}"
        );
        let all: Vec<Symbol> = reference
            .index
            .values()
            .flat_map(|index| index.symbols.clone())
            .collect();
        let before: Vec<Value> = all
            .iter()
            .map(|s| json!([s.key(), reference.signature_before(s)]))
            .collect();
        assert_eq!(json!(before), case["signature_before"], "{label}: signatures before");
        let overrides: Vec<Value> = all
            .iter()
            .map(|s| json!([s.key(), keys(&reference.overrides(s))]))
            .collect();
        assert_eq!(json!(overrides), case["overrides"], "{label}: overrides");
        let mut enclosing = Vec::new();
        for path in reference.index.keys() {
            for line in 0..16 {
                enclosing.push(json!([path, line, reference.enclosing(path, line).map(Symbol::key)]));
            }
        }
        assert_eq!(json!(enclosing), case["enclosing"], "{label}: enclosing");
        let families: Vec<Value> = reference
            .index
            .values()
            .flat_map(|index| &index.families)
            .map(|family| {
                let matches: Vec<Value> = reference
                    .family_matches(family)
                    .iter()
                    .map(|(other, how)| json!([other.path, other.line, how]))
                    .collect();
                json!([family.path, family.line, matches])
            })
            .collect();
        assert_eq!(json!(families), case["family_matches"], "{label}: family matches");
        for item in case["resolve_module"].as_array().expect("resolutions") {
            let (spec, from) = (item[0].as_str().expect("spec"), item[1].as_str().expect("from"));
            assert_eq!(
                json!(reference.resolve_module(spec, from)),
                item[2],
                "{label}: resolve {spec} from {from}"
            );
        }
        let views: Vec<Value> = all
            .iter()
            .filter(|s| matches!(s.kind.as_str(), "function" | "method" | "class"))
            .map(|s| json!([s.key(), reference.symbol_view(s, false), reference.symbol_view(s, true)]))
            .collect();
        assert_eq!(json!(views), case["symbol_views"], "{label}: symbol views");
        for request in case["served"].as_array().expect("served") {
            let kind = request[0].as_str().expect("kind");
            assert_eq!(
                served(&reference, kind, &request[1]),
                request[2],
                "{label}: serve {request}"
            );
        }
        assert_eq!(
            json!(reference.block(review::reference::BLOCK_CHARS)),
            case["block"],
            "{label}: block"
        );
        assert_eq!(json!(reference.facts()), case["facts"], "{label}: facts");
        assert_eq!(
            json!(reference.structural_findings()),
            case["findings"],
            "{label}: findings"
        );
        let mut small = build(&repo, &files, base, &case["limits"]);
        assert_eq!(json!(small.block(700)), case["block_small"], "{label}: small block");
    }
}

#[test]
fn review_reference_without_python_skips_python_files_and_says_so() {
    let fx = fixture();
    let repos = Repos::new(&fx);
    let repo = repos.repo("mixed", "head");
    let base = repos.repo("mixed", "base");
    let files = repos.diff("mixed");
    for missing in [
        &mut NoPython as &mut dyn PythonIndex,
        &mut ProcessPython::new("/nonexistent/python3"),
    ] {
        let mut reference = Reference::new(&repo, &files, Some(&base)).build(missing, &no_clock);
        let indexed: Vec<&String> = reference.index.keys().collect();
        assert_eq!(indexed, ["web/util.ts", "web/main.ts"]);
        // app.py (changed), then other.py and test_app.py (its importers), each read once.
        assert_eq!(reference.stats.python_unindexed, 3);
        assert_eq!(
            reference.notes,
            [
                "no Python interpreter answered: 3 Python file(s) not indexed (src/pkg/app.py, src/pkg/other.py, tests/test_app.py)"
            ]
        );
        // The TypeScript, workflow and Terraform facts stand.
        let shapes: Vec<String> = reference
            .facts()
            .iter()
            .map(|fact| fact["shape"].as_str().expect("shape").to_string())
            .collect();
        assert_eq!(
            shapes,
            [
                "signature",
                "idtoken",
                "fork-checkout",
                "run-injection",
                "tf-removed-ref",
                "tf-prevent-destroy"
            ]
        );
    }
}

// --- packages/coding-agent/test/ultron-autoreview-reference.test.ts -----------------------------------------------

#[test]
fn review_reference_resolves_definitions_callers_tests_and_signatures() {
    let fx = fixture();
    let repos = Repos::new(&fx);
    let (repo, base) = (repos.repo("mixed", "head"), repos.repo("mixed", "base"));
    let files = repos.diff("mixed");
    let mut reference = Reference::new(&repo, &files, Some(&base)).build(&mut python(), &no_clock);
    // The index reaches the importer (other.py), the imported module (helper.py), the test and the TS caller.
    assert!(reference.stats.files >= 8);
    assert!(reference.stats.symbols > 8);
    let brief = |calls: Vec<&CallSite>| -> Vec<(String, u64, String, bool)> {
        calls
            .into_iter()
            .map(|c| {
                (
                    c.path.clone(),
                    c.line,
                    c.enclosing.clone().unwrap_or_default(),
                    !c.targets.is_empty(),
                )
            })
            .collect()
    };
    assert_eq!(
        brief(reference.callers("show", true)),
        [
            ("src/pkg/other.py".to_string(), 6, "consume".to_string(), true),
            ("tests/test_app.py".to_string(), 5, "test_show".to_string(), true)
        ]
    );
    let parse_callers = brief(reference.callers("parse", true));
    assert!(parse_callers.contains(&("web/main.ts".to_string(), 4, "main".to_string(), true)));
    assert!(parse_callers.contains(&("web/util.ts".to_string(), 7, "Client.fetch".to_string(), true)));
    assert_eq!(
        reference.tests_of("show"),
        [("tests/test_app.py".to_string(), 5, "test_show".to_string())]
    );
    let callees: Vec<(String, Vec<String>)> = reference
        .callees("App.run")
        .into_iter()
        .map(|c| (c.callee.clone(), c.targets.clone()))
        .collect();
    assert_eq!(
        callees,
        [("load".to_string(), vec!["src/pkg/helper.py::load".to_string()])]
    );
    assert_eq!(
        reference.enclosing("src/pkg/app.py", 12).map(|s| s.qualname.clone()),
        Some("show".to_string())
    );
    let show = reference.symbols_named("show")[0].clone();
    assert_eq!(reference.signature_before(&show), Some("def show(path)".to_string()));
    let view = reference.symbol_view(&show, true);
    assert!(view.contains("def show(path, strict=False) [was: def show(path)]"));
    assert!(view.contains("callers outside the change (2)"));
    assert!(view.contains("   11 | def show(path, strict=False):"));
    let families: Vec<(String, u64, Option<String>, Vec<String>)> = reference.index["src/pkg/app.py"]
        .families
        .iter()
        .map(|f| (f.path.clone(), f.line, f.name.clone(), f.items.clone()))
        .collect();
    assert_eq!(
        families,
        [(
            "src/pkg/app.py".to_string(),
            3,
            Some("KINDS".to_string()),
            ["a", "b", "c", "d"].map(String::from).to_vec()
        )]
    );
    let block = reference.block(review::reference::BLOCK_CHARS);
    assert!(block.contains("[D1] src/pkg/app.py:11-12 def show(path, strict=False) [was: def show(path)]"));
    assert!(block.contains("[D2] web/util.ts:2-4 export function parse(input: string, radix = 10): number {"));
    let family = regex::Regex::new(
        r"\[F\d\] The change edits the literal family `KINDS` at src/pkg/app\.py:3 \('a', 'b', 'c', 'd'\); the same family is listed at src/pkg/other\.py:1 \(`KINDS`\): missing 'd'\. A file not in this change lists it differently\.",
    )
    .expect("regex");
    assert!(family.is_match(&block), "{block}");
    let signature = regex::Regex::new(
        r"\[F\d\] The signature of show changed: was `def show\(path\)`, now `def show\(path, strict=False\)`; 2 caller\(s\) outside the change",
    )
    .expect("regex");
    assert!(signature.is_match(&block), "{block}");
}

#[test]
fn review_reference_states_workflow_and_terraform_facts() {
    let fx = fixture();
    let repos = Repos::new(&fx);
    let repo = repos.repo("mixed", "head");
    let files = repos.diff("mixed");
    let mut reference = Reference::new(&repo, &files, None).build(&mut python(), &no_clock);
    let facts = reference.facts();
    let kinds: Vec<&str> = facts.iter().map(|fact| fact["kind"].as_str().expect("kind")).collect();
    assert_eq!(kinds.iter().filter(|kind| **kind == "workflow").count(), 3);
    assert_eq!(kinds.iter().filter(|kind| **kind == "terraform").count(), 2);
    let findings = reference.structural_findings();
    let claims: Vec<&str> = findings.iter().map(|f| f["claim"].as_str().expect("claim")).collect();
    let has = |needle: &str| claims.iter().any(|claim| claim.contains(needle));
    assert!(has("grants `id-token: write` although no step requests an OIDC token"));
    assert!(has(
        "checks out the pull request head under a pull_request_target trigger without a same-repository guard"
    ));
    assert!(has(
        "interpolates the user-controlled expression `${{ github.event.pull_request.title }}`"
    ));
    assert!(claims.contains(&"`output.o` still references `aws_s3_bucket.gone`, which this change removes."));
    assert!(has("lacks the `prevent_destroy` lifecycle rule"));
    for finding in &findings {
        assert_eq!(finding["source"], "deep:structure");
        let citations = finding["citations"].as_array().expect("citations");
        assert!(!citations.is_empty());
        for citation in citations {
            assert!(citation["quote"].as_str().expect("quote").chars().count() > 3);
        }
    }
    let find = |needle: &str| {
        findings
            .iter()
            .find(|f| f["claim"].as_str().expect("claim").contains(needle))
            .expect(needle)
    };
    assert_eq!(find("The workflow grants `id-token: write` al")["level"], "low");
    assert_eq!(find("same-repository guard")["level"], "medium");
    let removed = findings
        .iter()
        .find(|f| f["claim"].as_str().expect("claim").starts_with("`output.o`"))
        .expect("removed");
    assert_eq!(removed["file"], "infra/main.tf");
    assert_eq!(removed["line"], 14);
    assert_eq!(removed["beyond_diff"], true);
    assert_eq!(removed["citations"][0]["quote"], "value = aws_s3_bucket.gone.id");
}

#[test]
fn review_reference_external_reusable_workflow_is_a_fact_with_the_mutable_ref() {
    let fx = fixture();
    let repos = Repos::new(&fx);
    let (repo, base) = (repos.repo("external", "head"), repos.repo("external", "base"));
    let files = repos.diff("external");
    let mut reference = Reference::new(&repo, &files, Some(&base)).build(&mut python(), &no_clock);
    let facts = reference.facts();
    assert_eq!(facts.len(), 1);
    assert!(facts[0]["text"].as_str().expect("text").contains(
        ".github/workflows/review.yml:8 grants `id-token: write` to the external reusable workflow `other-org/.github/.github/workflows/review.yml@main` (a mutable ref)"
    ));
    let claims: Vec<Value> = reference
        .structural_findings()
        .iter()
        .map(|f| f["claim"].clone())
        .collect();
    assert_eq!(
        claims,
        [json!(
            "The workflow grants `id-token: write` to the external reusable workflow `other-org/.github/.github/workflows/review.yml@main`."
        )]
    );
}

#[test]
fn review_reference_yaml_and_terraform_readers_read_what_workflows_and_manifests_use() {
    let node = parse_yaml(
        "# a workflow\nname: CI\non:\n  push:\n    branches: [main, 'release/*']\n  workflow_dispatch:\nenv:\n  FOO: \"bar # not a comment\"\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n      - name: Test\n        run: |\n          echo one\n          echo two\n      - run: echo three\n    strategy:\n      matrix: {node: [18, 20]}\n",
    )
    .expect("parses")
    .expect("a node");
    let map =
        |pairs: Vec<(&str, Value)>| json!({"$map": pairs.into_iter().map(|(k, v)| json!([k, v])).collect::<Vec<_>>()});
    let expected = map(vec![
        ("name", json!("CI")),
        (
            "on",
            map(vec![
                ("push", map(vec![("branches", json!(["main", "release/*"]))])),
                ("workflow_dispatch", Value::Null),
            ]),
        ),
        ("env", map(vec![("FOO", json!("bar # not a comment"))])),
        (
            "jobs",
            map(vec![(
                "build",
                map(vec![
                    ("runs-on", json!("ubuntu-latest")),
                    (
                        "steps",
                        json!([
                            map(vec![("uses", json!("actions/checkout@v4"))]),
                            map(vec![("name", json!("Test")), ("run", json!("echo one\necho two"))]),
                            map(vec![("run", json!("echo three"))]),
                        ]),
                    ),
                    (
                        "strategy",
                        map(vec![("matrix", map(vec![("node", json!(["18", "20"]))]))]),
                    ),
                ]),
            )]),
        ),
    ]);
    assert_eq!(node.value.to_json(), expected);
    assert_eq!(
        node_json(Some(&node))["lines"],
        json!([["name", 2], ["on", 3], ["env", 7], ["jobs", 9]])
    );
    let blocks = parse_terraform(
        "main.tf",
        "resource \"aws_iam_role\" \"r\" {\n  name = \"r\"\n  assume_role_policy = <<EOF\n{ \"ref\": \"aws_s3_bucket.other.arn\" }\nEOF\n  lifecycle {\n    prevent_destroy = true\n  }\n  tags = { a = var.name }\n}\nmodule \"m\" {\n  source = \"./m\"\n  role   = aws_iam_role.r.arn\n}\n",
    );
    let got: Vec<Value> = blocks
        .iter()
        .map(|b| json!([b.kind, b.r#type, b.name, b.line, b.end, b.attrs, b.refs]))
        .collect();
    assert_eq!(
        json!(got),
        json!([
            ["resource", "aws_iam_role", "r", 1, 10, {"name": "\"r\"", "tags": "{ a = var.name }"}, ["var.name"]],
            ["module", "m", "", 11, 14, {"source": "\"./m\"", "role": "aws_iam_role.r.arn"}, ["aws_iam_role.r"]],
        ])
    );
}

#[test]
fn review_reference_serves_symbol_callers_callees_and_tests_of() {
    let fx = fixture();
    let repos = Repos::new(&fx);
    let (repo, base) = (repos.repo("mixed", "head"), repos.repo("mixed", "base"));
    let files = repos.diff("mixed");
    let reference = Reference::new(&repo, &files, Some(&base)).build(&mut python(), &no_clock);
    let ask = |kind: &str, args: Value| {
        let (title, body) = serve(&reference, kind, &args).expect("served");
        format!("{title}\n{body}")
    };
    let symbol = ask("symbol", json!({"name": "show"}));
    assert!(symbol.contains("symbol show -> 1 definition(s)"));
    assert!(symbol.contains("   11 | def show(path, strict=False):"));
    let callers = ask("callers", json!({"symbol": "load"}));
    assert!(callers.contains("callers load -> 2 call sites"));
    assert!(callers.contains("src/pkg/app.py:8 in App.run (resolved)"));
    assert!(
        ask("callees", json!({"symbol": "App.run"})).contains("src/pkg/app.py:8: load(...) -> src/pkg/helper.py::load")
    );
    assert!(ask("tests_of", json!({"symbol": "show"})).contains("tests/test_app.py:5 test_show"));
    // A name the index does not know is not served (the deep pass falls back to the text search).
    assert_eq!(serve(&reference, "callers", &json!({"symbol": "KINDS"})), None);
    assert_eq!(serve(&reference, "callees", &json!({"symbol": "nothing_here"})), None);
}
