"""Records the deep pass, the structural reference, the test session and the compiled interpreter working together
(autoreview_deep.py, autoreview_reference.py, autoreview_tests.py, autoreview_compiled.py) for crates/review's glue
tests: `build_brief` with the real reference built from the head commit (and the base commit's signatures), the
reference's lookups served through `serve_request`, `start_tests` from the sandbox choice to the automatic runs, the
planner's runner availability, and `Interpreter.run` over lookups, tests, asserts, conditions and `for_each`. Asks
are scripted frames; composing an emitted finding and resolving an undecided one are the caller's (stubbed here as
the Rust test's caller does). Run by generate_glue.mjs; prints glue.json.

The repository is built in a temp dir from recorded steps (fixed identity and clock, no user config) so the Rust
test rebuilds the same commits; git runs for real, exports are real `git archive`s, the sandbox's executor is
scripted (it answers from the exported tree). Temp paths are `<root>`.
"""
import asyncio
import json
from types import SimpleNamespace
import os
import shutil
import sys
import tempfile

sys.path.insert(0, sys.argv[1])
import autoreview_api as aa  # noqa: E402
import autoreview_compiled as c  # noqa: E402
import autoreview_deep as deep  # noqa: E402
import autoreview_reference as reference_module  # noqa: E402
import autoreview_tests as testing  # noqa: E402
import review_api as ra  # noqa: E402

GIT_ENV = {
    "GIT_AUTHOR_NAME": "Ultron Fixture", "GIT_AUTHOR_EMAIL": "fixture@example.invalid",
    "GIT_COMMITTER_NAME": "Ultron Fixture", "GIT_COMMITTER_EMAIL": "fixture@example.invalid",
    "GIT_AUTHOR_DATE": "2026-02-01T00:00:00+0000", "GIT_COMMITTER_DATE": "2026-02-01T00:00:00+0000",
    "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": "/dev/null", "LC_ALL": "C", "TZ": "UTC",
}
os.environ.update(GIT_ENV)
os.environ.pop("GIT_DIR", None)
os.environ.pop("GIT_WORK_TREE", None)

ROOT = os.path.realpath(tempfile.mkdtemp(prefix="review-glue-"))

UTIL = "def clamp(value, low, high):\n    return max(low, min(value, high))\n"
APP_BASE = ("from util import clamp\n\nKINDS = [\"a\", \"b\"]\n\n\ndef page(items, size):\n"
            "    return items[: clamp(size, 1, 50)]\n")
APP_HEAD = ("import os\n\nfrom util import clamp\n\nKINDS = [\"a\", \"b\", \"c\"]\n\n\n"
            "def page(items, size=None, start=0):\n    # Pages are capped at 50 items.\n"
            "    size = size or int(os.environ[\"PAGE_SIZE\"])\n    return items[start: start + clamp(size, 1, 50)]\n\n\n"
            "def first(items):\n    return page(items, 1)\n")
TEST_BASE = "from app import page\n\n\ndef test_page():\n    assert page([1, 2, 3], 2) == [1, 2]\n"
TEST_HEAD = TEST_BASE + "\n\ndef test_size():\n    assert page([1, 2, 3], 2, 1) == [2, 3]\n"
FORMAT_BASE = "export function fmt(value: number): string {\n  return value.toFixed(2);\n}\n"
FORMAT_HEAD = ("export function fmt(value: number, digits = 2): string {\n  return value.toFixed(digits);\n}\n\n"
               "export const UNITS = [\"kb\", \"mb\", \"gb\"];\n")
MAIN_TS = "import { fmt } from \"./format\";\n\nexport function show(n: number): string {\n  return fmt(n);\n}\n"
FORMAT_TEST = "import { fmt } from \"./format\";\n\ntest(\"fmt\", () => {\n  expect(fmt(1)).toBe(\"1.00\");\n});\n"


def write(path, text):
    return ["write", path, text]


def git(*args):
    return ["git", *args]


def date(when):
    return ["date", when]


STEPS = [
    git("init", "-q", "-b", "main"),
    write("pytest.ini", "[pytest]\n"), write("src/util.py", UTIL), write("src/app.py", APP_BASE),
    write("tests/test_app.py", TEST_BASE), write("web/format.ts", FORMAT_BASE), write("web/main.ts", MAIN_TS),
    write("web/format.test.ts", FORMAT_TEST), write("NOTES.txt", "Kinds: a, b ... cut for brevity\n"),
    write("README.md", "# glue\n\n`page(items, size)` pages a list.\n"),
    git("add", "-A"), git("commit", "-qm", "base: paging"), git("tag", "base"),
    date("2026-02-02T00:00:00+0000"),
    write("src/app.py", APP_HEAD), write("tests/test_app.py", TEST_HEAD), write("web/format.ts", FORMAT_HEAD),
    write("README.md", "# glue\n\n`page(items, size)` pages a list; the size defaults to PAGE_SIZE.\n"),
    git("add", "-A"),
    git("commit", "-qm", "Page from an offset\n\nThe size defaults to PAGE_SIZE from the environment."),
    git("tag", "head"),
    write("secret.txt", "hunter2\n"),
]


def run_steps(base, steps):
    env = dict(os.environ)
    for step in steps:
        if step[0] == "write":
            path = os.path.join(base, step[1])
            os.makedirs(os.path.dirname(path), exist_ok=True)
            with open(path, "w", encoding="utf-8") as handle:
                handle.write(step[2])
        elif step[0] == "date":
            os.environ["GIT_AUTHOR_DATE"] = os.environ["GIT_COMMITTER_DATE"] = step[1]
        else:
            code, out, err = ra._run_process(["git", *step[1:]], base, 30)
            if code != 0:
                raise SystemExit(f"step {step} failed: {err}{out}")
    os.environ.clear()
    os.environ.update(env)


REPO_DIR = os.path.join(ROOT, "glue")
os.makedirs(REPO_DIR)
run_steps(REPO_DIR, STEPS)
GIT = ra.Git(REPO_DIR)
REVS = {name: GIT.out("rev-parse", name).strip() for name in ("base", "head")}
DIFF = GIT.out("diff", "-U3", "base", "head", "--")
FILES = ra.parse_diff(DIFF)
WEB_FILES = [item for item in FILES if item.path.startswith("web/")]

# The reference's build time is a clock reading: fixed, so its stats are stable.
reference_module.Reference.__init__.__kwdefaults__["clock"] = lambda: 0.0


def rooted(value):
    if isinstance(value, str):
        return value.replace(ROOT, "<root>")
    if isinstance(value, list):
        return [rooted(item) for item in value]
    if isinstance(value, tuple):
        return [rooted(item) for item in value]
    if isinstance(value, dict):
        return {rooted(key): rooted(item) for key, item in value.items()}
    return value


def head_repo():
    return deep.Repo(REPO_DIR, REVS["head"])


def reader(repo):
    return lambda path: repo.lines(path)


def brief_json(brief):
    return {"text": brief.text, "lenses": brief.lenses, "symbols": brief.symbols, "callers": brief.callers,
            "tests": brief.tests, "claims": brief.claims, "required": brief.required,
            "claim_list": [list(item) for item in brief.claim_list], "helpers": brief.helpers,
            "extracted": {key: [list(item) if isinstance(item, tuple) else item for item in items]
                          for key, items in brief.extracted.items()},
            "reference": brief.reference is not None, "reference_text": brief.reference_text,
            "reference_stats": brief.reference_stats}


def answer(repo, request, session=None):
    try:
        return {"ok": list(deep.serve_request(repo, request, session))}
    except deep.Rejected as error:
        return {"rejected": str(error)}


# --- The map with the real reference ----------------------------------------------------------------------------

LOOKUPS = [
    {"symbol": {"name": "page"}}, {"symbol": {"name": "fmt"}}, {"callers": {"symbol": "clamp"}},
    {"callers": {"symbol": "fmt"}}, {"callees": {"symbol": "page"}}, {"tests_of": {"symbol": "page"}},
    {"definition": {"symbol": "clamp"}}, {"definition": {"symbol": "page"}}, {"references": {"symbol": "clamp"}},
    {"references": {"symbol": "page"}}, {"symbol": {"name": "nope_zz"}}, {"callees": {"symbol": "nope_zz"}},
    {"tests_of": {"symbol": "fmt"}},
]
BRIEF_CASES = [
    {"name": "full", "web": False, "base": True, "limit": 8000, "title": "Page from an offset",
     "description": "- The size now defaults to PAGE_SIZE from the environment.\n- Formatting takes a digit count."},
    {"name": "no base", "web": False, "base": False, "limit": 900, "title": "", "description": ""},
    {"name": "web", "web": True, "base": True, "limit": 8000, "title": "", "description": ""},
]
briefs = []
for case in BRIEF_CASES:
    repo = head_repo()
    files = WEB_FILES if case["web"] else FILES
    brief = deep.build_brief(repo, files, reader(repo), case["limit"], title=case["title"],
                             description=case["description"], base=REVS["base"] if case["base"] else None)
    briefs.append({**case, "brief": brief_json(brief), "repo_reference": repo.reference is not None,
                   "lookups": [[request, answer(repo, request)] for request in LOOKUPS],
                   "facts": brief.reference.facts(), "structural": brief.reference.structural_findings()})


# --- Opening the test session -----------------------------------------------------------------------------------


def executor(argv, cwd, env, timeout):
    """The sandbox, scripted: it answers from the exported tree it runs in."""
    with open(os.path.join(cwd, "src/app.py"), encoding="utf-8") as handle:
        app = handle.read()
    if "MUTANT" in app:
        return 1, "FAILED tests/test_app.py::test_page - AssertionError: mutant\n1 failed\n"
    if "UNAVAILABLE" in " ".join(argv) or os.path.exists(os.path.join(cwd, "UNAVAILABLE")):
        return 4, "ModuleNotFoundError: No module named 'app'\n"
    if "PAGE_SIZE" in app:
        return 1, ("PASSED tests/test_app.py::test_page\nFAILED tests/test_app.py::test_size - AssertionError: offset\n"
                   "1 failed, 1 passed\n")
    return 0, "PASSED tests/test_app.py::test_page\nPASSED tests/test_app.py::test_size\n2 passed\n"


def unavailable_executor(argv, cwd, env, timeout):
    return 4, "ModuleNotFoundError: No module named 'app'\n"


EXECUTORS = {"scripted": executor, "unavailable": unavailable_executor}


def session_json(session):
    if session is None:
        return None
    lines, unavailable = c.runner_availability(session)
    return {"records": session.records, "limit": session.limit, "env": session.env_kind,
            "runner": [session.runner.name, session.runner.because] if session.runner else None,
            "availability": [lines, sorted(list(pair) for pair in unavailable)]}


START_CASES = [
    {"name": "no tests", "tests": None, "web": False},
    {"name": "no sandbox", "tests": {"sandbox": None}, "web": False},
    {"name": "nothing detected", "tests": {"detect": "none"}, "web": False},
    {"name": "detected", "tests": {"detect": "bwrap", "executor": "scripted", "runs": 4, "base": True}, "web": False},
    {"name": "regression", "tests": {"executor": "scripted", "runs": 4, "base": True}, "web": False},
    {"name": "one run left", "tests": {"executor": "scripted", "runs": 1, "base": True}, "web": False},
    {"name": "no runs", "tests": {"executor": "scripted", "runs": 0, "base": True}, "web": False},
    {"name": "unavailable", "tests": {"executor": "unavailable", "runs": 4, "base": True}, "web": False},
    {"name": "no runner", "tests": {"executor": "scripted", "runs": 4, "base": True}, "web": True},
]


def start(case):
    repo = head_repo()
    files = WEB_FILES if case["web"] else FILES
    brief = deep.build_brief(repo, files, reader(repo), base=REVS["base"])
    spec = case["tests"]
    tests = None
    if spec is not None:
        tests = {"base": REVS["base"] if spec.get("base") else None, "runs": spec.get("runs", 6),
                 "export": testing.export_commit, "clock": lambda: 0.0}
        if "executor" in spec:
            tests["executor"] = EXECUTORS[spec["executor"]]
        if "detect" in spec:
            found = spec["detect"]
            testing.detect_sandbox.__kwdefaults__["which"] = lambda name: f"/usr/bin/{name}" if name == found else None
            testing.detect_sandbox.__kwdefaults__["executor"] = lambda argv, cwd, env, timeout: (0, "")
        else:
            tests["sandbox"] = spec["sandbox"] if "sandbox" in spec else testing.Sandbox("bwrap")
    started = deep.start_tests(repo, files, tests, brief, root=REPO_DIR, rev=REVS["head"], clock=lambda: 0.0)
    session = started.pop("session")
    started.pop("sandbox")
    out = {**case, "started": started, "session": session_json(session)}
    if session is not None:
        session.close()
    return out


starts = [start(case) for case in START_CASES]


# --- The compiled interpreter's run -----------------------------------------------------------------------------


def F(sid, gate, negate=False, evidence=None):
    return {"id": sid, "op": "finding", "when": {"step": gate, "not": negate}, "file": "src/app.py", "line": 9,
            "level": "medium", "category": "bugs", "claim": f"claim of {sid}", "why": "w", "scenario": "s",
            "evidence": evidence or [gate]}


LOOKUP_PROGRAM = {"summary": "lookups", "steps": [
    {"id": "g", "op": "grep", "args": {"pattern": "clamp"}},
    {"id": "a1", "op": "assert", "step": "g", "predicate": "count >= 2", "expect": True, "holds": "clamp is used {{g.count}} times"},
    {"id": "r", "op": "read", "args": {"path": "src/app.py", "start": 1, "end": 12}},
    {"id": "a2", "op": "assert", "step": "r", "predicate": "contains PAGE_SIZE", "expect": True, "holds": "the size has a default"},
    {"id": "ls", "op": "list", "args": {"dir": "src"}},
    {"id": "d", "op": "definition", "args": {"symbol": "page"}},
    {"id": "refs", "op": "references", "args": {"symbol": "clamp"}},
    {"id": "h", "op": "history", "args": {"path": "src/app.py"}},
    {"id": "b", "op": "blame_range", "args": {"path": "src/app.py", "start": 1, "end": 3}},
    {"id": "p", "op": "pickaxe", "args": {"string": "PAGE_SIZE"}},
    {"id": "bad", "op": "read", "args": {"path": "secret.txt"}},
    {"id": "notes", "op": "read", "args": {"path": "NOTES.txt"}},
    {"id": "an", "op": "assert", "step": "notes", "predicate": "contains kinds c", "expect": False},
    {"id": "ab", "op": "assert", "step": "bad", "predicate": "contains hunter2", "expect": False},
    {"id": "gc", "op": "grep", "args": {"pattern": "clamp", "count_only": True}},
    {"id": "ac", "op": "assert", "step": "gc", "predicate": "count >= 2", "expect": True},
    {"id": "gbad", "op": "grep", "args": {"pattern": "clamp", "count_only": True, "path_glob": "../x"}},
    {"id": "abad", "op": "assert", "step": "gbad", "predicate": "count == 0", "expect": True},
    {"id": "fe", "op": "for_each", "over": "g", "max_items": 2, "steps": [
        {"id": "rr", "op": "read", "args": {"path": "{{item.path}}", "start": "{{item.line}}", "end": "{{item.line}}"}},
        {"id": "ar", "op": "assert", "step": "rr", "predicate": "contains clamp", "expect": True},
    ]},
    {"id": "skipped", "op": "grep", "args": {"pattern": "x"}, "when": {"step": "a2", "not": True}},
    {"id": "undecided", "op": "grep", "args": {"pattern": "y"}, "when": {"step": "ab", "not": False}},
    {"id": "q", "op": "ask", "question": "Is {{d.count}} definition of page bounded?", "context": ["r"]},
    {"id": "aq", "op": "assert", "all": ["q", "a1"], "expect": True},
    {"id": "a3", "op": "assert", "step": "gc", "predicate": "count == 0", "expect": True},
    F("f1", "ac", evidence=["gc"]), F("f2", "aq", evidence=["q"]), F("f3", "a3", evidence=["gc"]),
    F("f4", "ac", negate=True, evidence=["gc"]), F("f5", "abad", evidence=["gbad"]),
]}
TEST_PROGRAM = {"summary": "tests", "steps": [
    {"id": "t", "op": "run_tests", "args": {"paths": ["tests/test_app.py"]}},
    {"id": "at", "op": "assert", "step": "t", "predicate": "status == failed", "expect": True},
    {"id": "ts", "op": "run_tests", "args": {"paths": ["tests/test_app.py"], "select": "test_page"}},
    {"id": "m", "op": "mutation_check", "args": {"path": "src/app.py", "line": 11, "replacement": "    MUTANT = 1",
                                                 "tests": ["tests/test_app.py"]}},
    {"id": "am", "op": "assert", "step": "m", "predicate": "status == failed", "expect": True},
    {"id": "u", "op": "run_tests", "args": {"paths": ["web/format.test.ts"]}},
    {"id": "au", "op": "assert", "step": "u", "predicate": "status == passed", "expect": True},
    {"id": "bad", "op": "mutation_check", "args": {"path": "src/app.py", "line": 999, "replacement": "x",
                                                   "tests": ["tests/test_app.py"]}},
    F("ft", "at", evidence=["t"]), F("fm", "am", evidence=["m"]), F("fu", "au", evidence=["u"]),
]}
UNAVAILABLE_PROGRAM = {"summary": "unavailable", "steps": [
    {"id": "t1", "op": "run_tests", "args": {"paths": ["tests/test_app.py"]}},
    {"id": "t2", "op": "run_tests", "args": {"paths": ["tests/test_app.py"]}, "needs": ["t1"]},
    {"id": "m2", "op": "mutation_check", "args": {"path": "src/app.py", "line": 11, "replacement": "x",
                                                  "tests": ["tests/test_app.py"]}, "needs": ["t1"]},
]}
STUCK_PROGRAM = {"summary": "stuck", "steps": [
    {"id": "g", "op": "grep", "args": {"pattern": "clamp"}},
    {"id": "fe", "op": "for_each", "over": "g", "max_items": 1, "steps": [
        {"id": "q", "op": "ask", "question": "about {{item}}?", "context": ["g"]},
    ]},
    {"id": "after", "op": "grep", "args": {"pattern": "page"}, "needs": ["fe"]},
]}
RUN_CASES = [
    {"name": "lookups", "program": LOOKUP_PROGRAM, "session": None, "replies": {"q": {"answer": "yes", "quote": "def page", "why": "bounded"}}},
    {"name": "tests", "program": TEST_PROGRAM, "session": "scripted", "replies": {}},
    {"name": "no session", "program": TEST_PROGRAM, "session": None, "replies": {}},
    {"name": "unavailable", "program": UNAVAILABLE_PROGRAM, "session": "unavailable", "replies": {}},
    {"name": "seeded", "program": UNAVAILABLE_PROGRAM, "session": "scripted", "replies": {}, "unavailable": [["pytest", "."]]},
    {"name": "stuck", "program": STUCK_PROGRAM, "session": None, "replies": {}},
]


class Frames:
    def __init__(self, replies):
        self.replies = replies
        self.timings = []

    async def run(self, phase, jobs, **kwargs):
        return [self.replies.get(job[0]) for job in jobs]


def emit(self, step, scope, began, resolution, **extra):
    self._record(step, {"op": "finding", "status": "ok", "emitted": True, "text": "", "detail": "emitted by the caller"},
                 began, None)


async def resolve(self, deferred):
    for step, scope, why in deferred:
        began = self.clock()
        self._record(step, {"op": "finding", "status": "ok", "emitted": False, "text": "", "detail": f"deferred: {why}"},
                     began, None)


ORIGINAL_ASKS = c.Interpreter._asks


async def asks(self, steps):
    """The real `_asks` over scripted frames; what it recorded is kept for the Rust caller to replay."""
    before = len(self.records)
    await ORIGINAL_ASKS(self, steps)
    for record in self.records[before:]:
        result = {key: value for key, value in self.results[record["id"]].items() if key != "summary"}
        self.replayed.append({"id": record["id"], "result": result, "input": json.loads(record["input"])
                              if record["input"] not in ("null",) else None})


c.Interpreter._emit = emit
c.Interpreter._resolve = resolve
c.Interpreter._asks = asks


def run_case(case):
    program, errors = c.validate(case["program"])
    if program is None:
        raise SystemExit(f"{case['name']}: {errors}")
    repo = head_repo()
    deep.build_brief(repo, FILES, reader(repo), base=REVS["base"])
    session = None
    if case["session"] is not None:
        session = testing.TestSession(REPO_DIR, REVS["head"], REVS["base"], repo.files(),
                                      lambda path: "\n".join(repo.lines(path) or []), testing.Sandbox("bwrap"),
                                      runs=6, executor=EXECUTORS[case["session"]], export=testing.export_commit,
                                      clock=lambda: 0.0)
    ticks = iter(range(1, 1_000_000))
    it = c.Interpreter(program, repo, frames=Frames(case["replies"]), session=session, diff_lines={}, ask_model=None,
                       ask_thinking=None, cutoff=None, clock=lambda: float(next(ticks)), cap=lambda *a, **k: "low",
                       unavailable={tuple(pair) for pair in case.get("unavailable", [])})
    it.replayed = []
    asyncio.run(it.run())
    out = {"name": case["name"], "program": case["program"], "session": case["session"],
           "unavailable_before": case.get("unavailable", []), "records": it.records, "results": it.results,
           "asks": it.replayed, "truncated": it.truncated, "expanded": it.expanded, "not_emitted": it.not_emitted,
           "unavailable": sorted(list(pair) for pair in it.unavailable),
           "session_records": session.records if session is not None else None}
    if session is not None:
        session.close()
    return out


runs = [run_case(case) for case in RUN_CASES]

# --- The author's stated intent --------------------------------------------------------------------------------

COMMENTS = "".join(f"+# Comment {i} explains why the module pages this way, in some detail: " + "x" * 150 + "\n"
                   for i in range(12))
NOTES_DIFF = f"diff --git a/notes.py b/notes.py\n--- a/notes.py\n+++ b/notes.py\n@@ -1,0 +1,12 @@\n{COMMENTS}"
INTENT_CASES = [
    {"context": {"title": "Page from an offset", "description": "Adds an offset.\r\nThe size defaults to PAGE_SIZE."},
     "spec": {"repoDir": REPO_DIR, "base": REVS["base"], "head": REVS["head"]}, "git": True, "diff": None},
    {"context": {"title": "  ", "description": 7}, "spec": {"baseSha": REVS["base"]}, "git": True, "diff": None},
    {"context": {"title": "t"}, "spec": {"repoDir": REPO_DIR, "base": REVS["base"]}, "git": True, "diff": None},
    {"context": {"title": "t"}, "spec": {"repoDir": "", "base": REVS["base"], "baseSha": "-x"}, "git": True, "diff": None},
    {"context": {"title": "t"}, "spec": {"baseSha": REVS["base"]}, "git": False, "diff": None},
    {"context": {"description": "d" * 700}, "spec": {"baseSha": "nope"}, "git": True, "diff": None},
    {"context": {"title": "x" * 400, "description": "y" * 700}, "spec": {}, "git": True, "diff": NOTES_DIFF},
    {"context": {}, "spec": {}, "git": True, "diff": ""},
]
intents = []
for case in INTENT_CASES:
    repo = head_repo()
    files = FILES if case["diff"] is None else ra.parse_diff(case["diff"])
    scope = SimpleNamespace(files=files, read_file=reader(repo))
    intents.append({**case, "out": aa.stated_intent(case["context"], scope, GIT if case["git"] else None, case["spec"])})

shutil.rmtree(ROOT, ignore_errors=True)
print(json.dumps(rooted({"git_env": GIT_ENV, "steps": STEPS, "revs": REVS, "diff": DIFF, "briefs": briefs,
                         "starts": starts, "runs": runs, "intents": intents}), indent=1, sort_keys=False))
