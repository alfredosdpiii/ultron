//! A53: a proposed code skill whose test fails is never importable; a passing one is, survives worker restart, and
//! rolls back cleanly. Ported from packages/coding-agent/test/ultron-a53-code-skills.test.ts (all six cases) with its
//! literal expectations: `kernel_apis::CodeSkills` (test gating in a fresh kernel, versions, rollback), the
//! `skills.*` module behind real runtime.py kernels, and kernels started as a worker starts them
//! (`ULTRON_CODE_SKILLS_DIR`). Where the TS test calls `loadSkillsFromDir` and `rlmRuntimePrompt`, this test reads
//! the same facts from the `ultron` binary itself: the system prompt of a run whose agent dir holds the skills lists
//! the published Pi skill (or none) and carries the code-skills line of the runtime guide.

mod common;

use common::{Install, Provider, Reply, runtime};
use kernel::{Kernel, KernelOptions, Signal, Status};
use kernel_apis::util::system_clock;
use kernel_apis::{
    CodeSkillKind, CodeSkills, CodeSkillsOptions, DocumentStore, KernelApis, MemoryDocumentStore, SkillLoader,
    SkillsModule,
};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const SLUG_V1: &str = r#""""Turn free text into URL slugs."""
import re

def slugify(text: str) -> str:
    """Lowercase, keep alphanumerics, join words with hyphens."""
    return "-".join(re.findall(r"[a-z0-9]+", text.lower()))
"#;
const SLUG_V2: &str = r#""""Turn free text into URL slugs (v2: underscores)."""
import re

def slugify(text: str) -> str:
    return "_".join(re.findall(r"[a-z0-9]+", text.lower()))
"#;
const SLUG_TEST_V1: &str = "from code_skills import slug
def test_slugify():
    assert slug.slugify(\"Hello, World!\") == \"hello-world\"
";
const SLUG_TEST_V2: &str = "from skill import slugify
async def test_slugify():
    assert slugify(\"Hello, World!\") == \"hello_world\"
";
const BROKEN: &str = "def slugify(text):\n    return text.upper()\n";
const BROKEN_TEST: &str =
    "from code_skills import slug\ndef test_slugify():\n    assert slug.slugify(\"a b\") == \"a-b\"\n";

/// The runtime guide's code-skills line (`CODE_SKILLS_PROMPT`).
const CODE_SKILLS_PROMPT: &str = "- Save a working procedure that will recur as a tested code skill (`await skills.propose_code(...)`); reuse one with `from code_skills import <name>`. `help(skills)`.";

fn payload(value: Value) -> Map<String, Value> {
    value.as_object().unwrap().clone()
}

fn code_skills(dir: &Path) -> CodeSkills {
    CodeSkills::new(CodeSkillsOptions::new(dir, runtime()))
}

fn propose(skills: &CodeSkills, kind: CodeSkillKind, value: Value) -> Value {
    skills.propose(kind, &payload(value), &Signal::new()).unwrap()
}

fn code(skills: &CodeSkills, name: &str, source: &str, test: &str, evidence: Value) -> Value {
    propose(
        skills,
        CodeSkillKind::Code,
        json!({"name": name, "source": source, "test_source": test, "evidence": evidence}),
    )
}

/// The `skills.*` module over a memory store, as a worker builds it (no Pi skills in the catalog).
fn module(store: Arc<MemoryDocumentStore>, dir: &Path) -> Arc<SkillsModule> {
    let loader: SkillLoader = Arc::new(|| Ok(Vec::new()));
    Arc::new(SkillsModule::new(store, loader, system_clock(), Some(code_skills(dir))))
}

/// A kernel as a worker starts one: code skills come from the profile's skills directory.
fn kernel(dir: &Path, module: Option<Arc<SkillsModule>>) -> Kernel {
    let mut options = KernelOptions::new(std::env::temp_dir(), runtime());
    options.env = vec![("ULTRON_CODE_SKILLS_DIR".into(), dir.display().to_string())];
    let mut apis = KernelApis::new(common::null_handler());
    if let Some(module) = module {
        apis = apis.with(module);
    }
    Kernel::new(options, Arc::new(apis)).unwrap()
}

/// The TS `run`: the error as `ERROR <ename>: <evalue>`, else stdout and a non-None result.
fn run(kernel: &Kernel, code: &str) -> String {
    let result = kernel.execute(code, None, None).unwrap();
    if result.status == Status::Error {
        let error = result.error.unwrap();
        return format!("ERROR {}: {}", error.ename, error.evalue);
    }
    let shown = result.result.filter(|r| r != "None").unwrap_or_default();
    [result.stdout, shown]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

fn states(skills: &CodeSkills, name: &str) -> Vec<Value> {
    skills.history(Some(&json!(name))).unwrap()["versions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["state"].clone())
        .collect()
}

/// A private `ultron` install whose profile skills directory (`<agent>/skills`) holds the code skills.
struct Profile {
    provider: Provider,
    install: Install,
}

impl Profile {
    fn new() -> Profile {
        let provider = Provider::start(|_| Reply::Text("OK".into()));
        let install = Install::new(&provider);
        Profile { provider, install }
    }

    fn dir(&self) -> PathBuf {
        self.install.agent().join("skills")
    }

    /// The system prompt of one `ultron -p` run in this profile.
    fn system_prompt(&self) -> String {
        let before = self.provider.requests().len();
        common::stdout_of(&self.install.prompt("hi", &[]));
        self.provider.requests()[before].system()
    }
}

/// The Pi skills the system prompt lists, as `(name, description)`.
fn listed_skills(system: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for block in system.split("<skill>").skip(1) {
        let field = |tag: &str| {
            block
                .split(&format!("<{tag}>"))
                .nth(1)
                .and_then(|rest| rest.split(&format!("</{tag}>")).next())
                .unwrap_or_default()
                .to_string()
        };
        out.push((field("name"), field("description")));
    }
    out
}

#[test]
fn acceptance_a53_a_proposed_skill_whose_test_fails_is_never_importable() {
    let profile = Profile::new();
    let dir = profile.dir();
    let skills = code_skills(&dir);
    let rejected = code(&skills, "slug", BROKEN, BROKEN_TEST, json!("first try"));
    assert_eq!(rejected["status"], "rejected");
    assert_eq!(rejected["test"]["passed"], false);
    assert!(
        rejected["test"]["tests"][0]["error"]
            .as_str()
            .unwrap()
            .contains("AssertionError")
    );
    // The version is kept for inspection, but nothing is published or importable.
    assert!(dir.join("slug/.versions/1/skill.py").exists());
    assert!(!dir.join("slug/skill.py").exists());
    assert_eq!(listed_skills(&profile.system_prompt()), Vec::<(String, String)>::new());
    let fresh = kernel(&dir, None);
    assert!(run(&fresh, "from code_skills import slug").starts_with("ERROR ImportError"));
    assert!(run(&fresh, "import code_skills.slug").starts_with("ERROR ModuleNotFoundError"));
    // A test with no test functions, and a skill that does not import, fail too.
    let empty = code(&skills, "slug", SLUG_V1, "x = 1\n", json!("no tests"));
    assert_eq!(empty["status"], "rejected");
    assert!(empty["test"]["error"].as_str().unwrap().contains("no test_* functions"));
    let syntax = code(&skills, "slug", "def slugify(:\n", SLUG_TEST_V1, json!("typo"));
    assert_eq!(syntax["status"], "rejected");
    assert!(syntax["test"]["error"].as_str().unwrap().contains("SyntaxError"));
    assert!(run(&fresh, "from code_skills import slug").starts_with("ERROR ImportError"));

    // Once a version is active, a failing successor never replaces it.
    assert_eq!(
        code(&skills, "slug", SLUG_V1, SLUG_TEST_V1, json!("ok"))["status"],
        "active"
    );
    let regression = code(&skills, "slug", BROKEN, BROKEN_TEST, json!("regression"));
    assert_eq!(regression["status"], "rejected");
    assert_eq!(
        run(
            &kernel(&dir, None),
            "from code_skills import slug\nprint(slug.__skill_version__, slug.slugify('A b'))"
        ),
        "4 a-b"
    );
    assert_eq!(
        states(&skills, "slug"),
        [
            json!("rejected"),
            json!("rejected"),
            json!("rejected"),
            json!("active"),
            json!("rejected")
        ]
    );
}

#[test]
fn acceptance_a53_the_test_runs_in_a_fresh_isolated_kernel_without_host_capabilities() {
    let root = tempfile::tempdir().unwrap();
    let skills = code_skills(&root.path().join("skills"));
    let probe = code(
        &skills,
        "probe",
        "\"\"\"Probe.\"\"\"\nimport os\nMARKER = os.getcwd()\n",
        "import os\nfrom code_skills import probe\ndef test_fresh_kernel():\n    assert \"state\" not in globals() or globals()[\"state\"] == {}\n    assert os.path.basename(os.getcwd()).startswith(\"ultron-skill-test-\")\nasync def test_no_host():\n    import __main__\n    assert \"skills\" not in globals()\n    try:\n        await __main__._STATE.namespace[\"skills\"].list()\n    except RuntimeError as error:\n        assert \"unavailable\" in str(error)\n    else:\n        raise AssertionError(\"host request succeeded\")\n",
        json!({"ref": "isolation"}),
    );
    assert_eq!(probe["status"], "active", "{probe}");
    assert_eq!(
        probe["test"]["tests"],
        json!([{"name": "test_fresh_kernel", "ok": true}, {"name": "test_no_host", "ok": true}])
    );
    // The proposal holds no secret, and the check that cleared it is recorded.
    assert_eq!(probe["secretCheck"], json!({"status": "clean"}));
}

#[test]
fn acceptance_a53_a_passing_skill_is_importable_listed_and_survives_worker_restart() {
    let profile = Profile::new();
    let dir = profile.dir();
    let store = Arc::new(MemoryDocumentStore::new(None));
    // The model proposes through the kernel API, as it would in the rlm tool.
    let proposer = kernel(&dir, Some(module(store.clone(), &dir)));
    let output = run(
        &proposer,
        &format!(
            "r = await skills.propose_code(\"slug\", {}, {}, {{\"task\": \"slugs\", \"worked\": True}})\nprint(r[\"status\"], r[\"version\"], r[\"import\"])",
            serde_json::to_string(SLUG_V1).unwrap(),
            serde_json::to_string(SLUG_TEST_V1).unwrap()
        ),
    );
    assert_eq!(output, "active 1 from code_skills import slug");
    // The already-running kernel sees it without restarting.
    assert_eq!(
        run(&proposer, "from code_skills import slug\nslug.slugify('Hi There')"),
        "'hi-there'"
    );
    // The journal recorded the outcome.
    let journal = store.read().unwrap().unwrap();
    let records = journal["code"].as_array().unwrap();
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(
        (
            &record["action"],
            &record["name"],
            &record["version"],
            &record["status"]
        ),
        (&json!("propose_code"), &json!("slug"), &json!(1), &json!("active"))
    );
    assert_eq!(
        (&record["test"]["passed"], &record["test"]["tests"]),
        (&json!(true), &json!(1))
    );
    proposer.shutdown();

    // A new worker (fresh CodeSkills, fresh kernels) finds the active version on disk.
    let restarted = code_skills(&dir);
    let listed = restarted.list(None, None);
    assert_eq!(listed.len(), 1);
    assert_eq!(
        (
            &listed[0]["name"],
            &listed[0]["version"],
            &listed[0]["kind"],
            &listed[0]["doc"]
        ),
        (
            &json!("slug"),
            &json!(1),
            &json!("code"),
            &json!("Turn free text into URL slugs.")
        )
    );
    let functions = listed[0]["functions"].as_array().unwrap();
    assert_eq!(functions.len(), 1);
    assert_eq!(
        (&functions[0]["name"], &functions[0]["signature"]),
        (&json!("slugify"), &json!("slugify(text: str) -> str"))
    );
    // The tool description's listing names the active skill.
    assert!(
        restarted
            .describe()
            .contains("- slug v1: Turn free text into URL slugs. [slugify(text: str) -> str]"),
        "{}",
        restarted.describe()
    );
    for statement in [
        "from code_skills import slug\nprint(slug.slugify('A  B'))",
        "import code_skills.slug\nprint(code_skills.slug.slugify('A  B'))",
        "from code_skills.slug import slugify\nprint(slugify('A  B'))",
    ] {
        assert_eq!(run(&kernel(&dir, None), statement), "a-b");
    }
    // The published SKILL.md is a valid Pi skill: the binary's system prompt lists it, next to the runtime guide.
    let system = profile.system_prompt();
    assert!(system.contains(CODE_SKILLS_PROMPT), "{system}");
    assert_eq!(
        listed_skills(&system),
        [("slug".to_string(), "Turn free text into URL slugs.".to_string())]
    );
    assert!(
        std::fs::read_to_string(dir.join("slug/SKILL.md"))
            .unwrap()
            .contains("from code_skills import slug")
    );
    // The `skills` API object is untouched by the package.
    assert_eq!(run(&kernel(&dir, None), "type(skills).__name__"), "'Skills'");
}

#[test]
fn acceptance_a53_rollback_follows_refinement_semantics_and_never_leaves_a_stale_import() {
    let profile = Profile::new();
    let dir = profile.dir();
    let skills = code_skills(&dir);
    assert_eq!(
        code(&skills, "slug", SLUG_V1, SLUG_TEST_V1, json!("v1"))["status"],
        "active"
    );
    let live = kernel(&dir, None);
    assert_eq!(run(&live, "from code_skills import slug\nslug.slugify('A b')"), "'a-b'");
    let second = code(&skills, "slug", SLUG_V2, SLUG_TEST_V2, json!("v2"));
    assert_eq!(
        (&second["status"], &second["version"], &second["previous"]),
        (&json!("active"), &json!(2), &json!(1))
    );
    assert_eq!(run(&live, "from code_skills import slug\nslug.slugify('A b')"), "'a_b'");
    // Only the active version can be rolled back.
    let refused = skills
        .rollback(&payload(json!({"name": "slug", "version": 1})))
        .unwrap_err();
    assert!(refused.contains("Only the active version"), "{refused}");
    assert_eq!(
        skills
            .rollback(&payload(json!({"name": "slug", "version": 2})))
            .unwrap(),
        json!({"name": "slug", "rolledBack": 2, "active": 1})
    );
    assert_eq!(
        run(
            &live,
            "from code_skills import slug\nprint(slug.__skill_version__, slug.slugify('A b'))"
        ),
        "1 a-b"
    );
    assert_eq!(std::fs::read_to_string(dir.join("slug/skill.py")).unwrap(), SLUG_V1);
    let versions: Vec<(Value, Value)> = skills.history(Some(&json!("slug"))).unwrap()["versions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| (v["version"].clone(), v["state"].clone()))
        .collect();
    assert_eq!(
        versions,
        [(json!(1), json!("active")), (json!(2), json!("rolled_back"))]
    );
    // Rolling back the first version leaves nothing importable and nothing published.
    assert_eq!(
        skills
            .rollback(&payload(json!({"name": "slug", "version": 1})))
            .unwrap(),
        json!({"name": "slug", "rolledBack": 1, "active": null})
    );
    assert!(run(&live, "from code_skills import slug").starts_with("ERROR ImportError"));
    let published: Vec<String> = std::fs::read_dir(dir.join("slug"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|name| !name.starts_with('.'))
        .collect();
    assert_eq!(published, Vec::<String>::new());
    assert_eq!(listed_skills(&profile.system_prompt()), Vec::<(String, String)>::new());
    // A later passing proposal starts a fresh chain.
    let third = code(&skills, "slug", SLUG_V1, SLUG_TEST_V1, json!("v3"));
    assert_eq!(
        (&third["status"], &third["version"], &third["previous"]),
        (&json!("active"), &json!(3), &Value::Null)
    );
    // A file tampered with after activation is refused rather than imported.
    std::fs::write(
        dir.join("slug/.versions/3/skill.py"),
        "def slugify(text):\n    return 'tampered'\n",
    )
    .unwrap();
    assert!(run(&kernel(&dir, None), "from code_skills import slug").starts_with("ERROR ImportError"));
}

#[test]
fn acceptance_a53_a_proposal_holding_a_secret_is_refused_before_anything_is_written() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("skills");
    let skills = code_skills(&dir);
    let clean = code(&skills, "slug", SLUG_V1, SLUG_TEST_V1, json!("slugs for the blog"));
    assert_eq!(
        (&clean["status"], &clean["secretCheck"]),
        (&json!("active"), &json!({"status": "clean"}))
    );
    // A key in the source, the test or the evidence refuses the proposal; no version directory is created.
    let aws = format!("the key is {}ABCDEFGHIJKLMNOP", "AKIA");
    for (name, source, test, evidence) in [
        (
            "deploy",
            "API_KEY = \"sk-abcdefghijklmnop1234\"\ndef deploy():\n    return API_KEY\n",
            "def test_x():\n    pass\n",
            "deploys",
        ),
        (
            "fetcher",
            SLUG_V1,
            "TOKEN = \"ghp_abcdefghijklmnopqrstuvwx\"\ndef test_x():\n    pass\n",
            "fetches",
        ),
        ("notes", SLUG_V1, SLUG_TEST_V1, aws.as_str()),
    ] {
        let refused = code(&skills, name, source, test, json!(evidence));
        assert_eq!(
            (
                &refused["status"],
                &refused["version"],
                &refused["test"],
                &refused["secretCheck"]["status"]
            ),
            (&json!("refused"), &Value::Null, &Value::Null, &json!("sensitive")),
            "{name}: {refused}"
        );
        assert!(refused["reason"].as_str().unwrap().contains("secret or credential"));
        assert!(!dir.join(name).join(".versions").exists(), "{name}");
    }
}

#[test]
fn acceptance_a53_policy_skills_store_any_plan_function_with_a_test_filterable_by_family() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("skills");
    let skills = code_skills(&dir);
    let plan = "\"\"\"Split a log into per-hour tasks.\"\"\"\ndef plan(handle, hours=24):\n    return [{\"task\": f\"summarize hour {h}\", \"slice\": [h * 100, (h + 1) * 100]} for h in range(hours)]\n";
    let test = "from code_skills import hourly\ndef test_plan():\n    tasks = hourly.plan(None, hours=3)\n    assert [t[\"slice\"] for t in tasks] == [[0, 100], [100, 200], [200, 300]]\n";
    let stored = propose(
        &skills,
        CodeSkillKind::Policy,
        json!({"name": "hourly", "source": plan, "test_source": test,
            "evidence": {"map": "8 frames, all complete"}, "family": "logs-forensics"}),
    );
    assert_eq!(
        (&stored["status"], &stored["kind"], &stored["version"]),
        (&json!("active"), &json!("policy"), &json!(1))
    );
    let names: Vec<&Value> = stored["test"]["tests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| &t["name"])
        .collect();
    assert_eq!(names, [&json!("policy_defines_plan"), &json!("test_plan")]);
    let listed: Vec<Value> = skills
        .list(Some("policy"), Some("logs-forensics"))
        .iter()
        .map(|s| s["name"].clone())
        .collect();
    assert_eq!(listed, [json!("hourly")]);
    assert_eq!(skills.list(None, Some("other")), Vec::<Value>::new());
    assert!(
        skills
            .describe()
            .contains("- hourly v1 (policy, logs-forensics): Split a log into per-hour tasks.")
    );
    // A module without plan() is not a policy, whatever its own test says.
    let no_plan = propose(
        &skills,
        CodeSkillKind::Policy,
        json!({"name": "noplan", "source": "def split(x):\n    return [x]\n",
            "test_source": "def test_ok():\n    pass\n", "evidence": "x"}),
    );
    assert_eq!(no_plan["status"], "rejected");
    assert_eq!(
        (&no_plan["test"]["tests"][0]["name"], &no_plan["test"]["tests"][0]["ok"]),
        (&json!("policy_defines_plan"), &json!(false))
    );

    // Through the kernel API a plan function importable from a file is accepted as-is; its name is aliased to plan.
    let store = Arc::new(MemoryDocumentStore::new(None));
    let module = module(store.clone(), &dir);
    let live = kernel(&dir, Some(module.clone()));
    let output = run(
        &live,
        r#"import textwrap, types, linecache
src = "def by_chunks(handle, n=2):\n    return [{'task': 'chunk', 'index': i} for i in range(n)]\n"
import tempfile, os, importlib.util
path = os.path.join(tempfile.mkdtemp(), "planner.py")
open(path, "w").write(src)
spec = importlib.util.spec_from_file_location("planner", path); planner = importlib.util.module_from_spec(spec); spec.loader.exec_module(planner)
r = await skills.propose_policy("chunked", planner.by_chunks, "from code_skills import chunked\ndef test_plan():\n    assert len(chunked.plan(None, n=3)) == 3\n", "worked on logs", family="logs")
from code_skills import chunked
print(r["status"], r["kind"], len(chunked.plan(None)))
h = await skills.code_history("chunked")
print(h["versions"][0]["family"], [s["name"] for s in await skills.code_list(kind="policy", family="logs")])"#,
    );
    assert_eq!(output, "active policy 2\nlogs ['chunked']");
    let bogus = module
        .handle(
            "skills.propose_policy",
            &payload(json!({"name": "x", "source": "", "test_source": "", "evidence": "e", "bogus": 1})),
            None,
            &Signal::new(),
        )
        .unwrap_err();
    assert!(bogus.contains("Unknown payload field: bogus"), "{bogus}");
    let journal = store.read().unwrap().unwrap();
    let actions: Vec<(Value, Value)> = journal["code"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["action"].clone(), r["status"].clone()))
        .collect();
    assert_eq!(
        actions,
        [
            (json!("propose_policy"), json!("active")),
            (json!("propose_policy"), json!("failed"))
        ]
    );
}
