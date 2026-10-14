"""Records autoreview_deep.py for crates/review tests: the read-only `Repo` over local git repositories, request
serving (every kind, with and without a structural reference and a test session), the map (`extract`,
`build_brief`), citation checks, `normalize_deep`, `cited_windows`, `merge`, shapes, regression findings, shared
views, the tests block, `start_tests` after the session opens, `prove_unpinned` and `assurance`. Run by
generate_deep.mjs; prints deep.json.

Repositories are built in a temp dir from recorded steps (written files and git commands under a fixed identity and
clock), so the Rust test rebuilds the same commits; git runs for real. The structural reference and the test session
are scripted fakes whose data is recorded too, so the Rust test drives the same fakes. Temp paths are `<root>`.
"""
import json
import os
import shutil
import sys
import tempfile

sys.path.insert(0, sys.argv[1])
import autoreview_api as aa  # noqa: E402
import autoreview_deep as deep  # noqa: E402
import review_api as ra  # noqa: E402

GIT_ENV = {
    "GIT_AUTHOR_NAME": "Ultron Fixture", "GIT_AUTHOR_EMAIL": "fixture@example.invalid",
    "GIT_COMMITTER_NAME": "Ultron Fixture", "GIT_COMMITTER_EMAIL": "fixture@example.invalid",
    "GIT_AUTHOR_DATE": "2026-01-01T00:00:00+0000", "GIT_COMMITTER_DATE": "2026-01-01T00:00:00+0000",
    "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": "/dev/null", "LC_ALL": "C", "TZ": "UTC",
}
os.environ.update(GIT_ENV)
os.environ.pop("GIT_DIR", None)
os.environ.pop("GIT_WORK_TREE", None)

ROOT = os.path.realpath(tempfile.mkdtemp(prefix="review-deep-"))


def write(path, text):
    return ["write", path, text]


def git(*args):
    return ["git", *args]


def date(when):
    # A commit clock step: later commits get later dates, so history is ordered and stable.
    return ["date", when]


def run_steps(base, steps):
    env = dict(os.environ)
    for step in steps:
        if step[0] == "write":
            path = os.path.join(base, step[1])
            os.makedirs(os.path.dirname(path), exist_ok=True)
            with open(path, "wb") as handle:
                handle.write(step[2].encode("utf-8") if isinstance(step[2], str) else bytes(step[2]))
        elif step[0] == "fill":
            path = os.path.join(base, step[1])
            os.makedirs(os.path.dirname(path), exist_ok=True)
            with open(path, "w", encoding="utf-8") as handle:
                handle.write(step[2] * step[3])
        elif step[0] == "date":
            os.environ["GIT_AUTHOR_DATE"] = os.environ["GIT_COMMITTER_DATE"] = step[1]
        else:
            code, out, err = ra._run_process(["git", *step[1:]], base, 30)
            if code != 0:
                raise SystemExit(f"step {step} failed: {err}{out}")
    os.environ.clear()
    os.environ.update(env)


HELPER = 'def load(path):\n    try:\n        return open(path).read()\n    except OSError:\n        return "ERROR: unreadable"\n'
APP_BASE = 'from helper import load\n\nKINDS = ["a", "b"]\n\n\ndef show(path):\n    return load(path)\n'
APP_HEAD = ('from helper import load\n\nKINDS = ["a", "b", "c"]\n\n\ndef show(path):\n    # Raises OSError when the file is'
            ' missing.\n    try:\n        return load(path)\n    except OSError:\n        return None\n')
TEST_APP = ('import pytest\nfrom app import KINDS, show\n\n\n@pytest.mark.parametrize("kind", ["a", "b"])\ndef test_kind(kind):\n'
            '    assert kind in KINDS\n')

API_BASE = "import os\n\n\ndef handler(request):\n    return None\n"
API_HEAD = ('import os\n\n\ndef handler(request):\n    """Serve one page of users."""\n    limit = int(os.environ["PAGE_LIMIT"])\n'
            '    token = request.args["token"]\n    if len(token) > 40:\n        raise ValueError("token too long")\n'
            '    return db.execute("select * from users where token = " + token, limit)\n\n\n'
            'def paginate(rows, size):\n    return [rows[i:i + size] for i in range(0, len(rows), size)]\n')
MIGRATION = "CREATE TABLE IF NOT EXISTS audit_log (id int);\nALTER TABLE users ADD COLUMN last_seen int;\n"
CONFIG_HEAD = "server:\n  port: 8080\n  page_limit: 50\n# comment: ignored\nfeature_flags: on\n"
DOCS_HEAD = "# Service\n\nRun `serve --dry-run` to check the configuration before deploying it.\nShort line.\n"
WORKFLOW = "name: ci\non: push\njobs:\n  test:\n    runs-on: ubuntu-latest\n    env:\n      PAGE_LIMIT: 10\n"
TEST_API = ("from api import handler, paginate\n\n\ndef test_paginate():\n    assert paginate([1, 2, 3], 2) == [[1, 2], [3]]\n\n\n"
            "def test_handler_rejects_long_token():\n    assert handler\n")

REPOS = {
    "deep": [
        git("init", "-q", "-b", "main"), write("src/helper.py", HELPER),
        write("src/store.py", "def save(path, text):\n    open(path, 'w').write(text)\n"),
        write("src/app.py", APP_BASE), write("tests/test_app.py", TEST_APP),
        write("README.md", "# app\n\n`show(path)` prints a file.\n"),
        git("add", "."), git("commit", "-qm", "base"), git("tag", "base"),
        date("2026-01-02T00:00:00+0000"),
        write("src/app.py", APP_HEAD), git("commit", "-qam", "handle missing files"), git("tag", "head"),
        # Not tracked: the host must not serve it.
        write("secret.txt", "hunter2\n"),
    ],
    "wide": [
        git("init", "-q", "-b", "main"),
        write("api.py", API_BASE), write("db/migrate.sql", "-- none\n"), write("config.yaml", "server:\n  port: 8080\n"),
        write("docs/guide.md", "# Service\n"), write("tests/test_api.py", TEST_API),
        write(".github/workflows/ci.yml", WORKFLOW), write("lib/api.py", "def handler():\n    pass\n"),
        write("image.png", [137, 80, 78, 71, 0, 0, 1, 2]), write("bin/data.bin", [0, 1, 2, 3, 0]),
        ["fill", "big.txt", "abcdefghi\n", 200_001],
        *[write(f"gen/f{index:03d}.py", f"VALUE_{index} = {index}\n") for index in range(203)],
        write("pkg/deep/nested/mod.py", "def nested_fn():\n    return 1\n"),
        git("add", "-A"), git("commit", "-qm", "base: service skeleton"), git("tag", "base"),
        date("2026-01-03T00:00:00+0000"),
        write("api.py", API_HEAD), write("db/migrate.sql", MIGRATION), write("config.yaml", CONFIG_HEAD),
        write("docs/guide.md", DOCS_HEAD),
        git("add", "-A"),
        git("commit", "-qm", "Add paging to the handler\n\nThe handler now reads PAGE_LIMIT from the environment.\n"
            "Long tokens are rejected.\nA fourth line that is not kept."),
        date("2026-01-04T00:00:00+0000"),
        write("tests/test_api.py", TEST_API + "\n\ndef test_more():\n    assert paginate([], 1) == []\n"),
        git("commit", "-qam", "more tests"), git("tag", "head"),
    ],
}

for name, steps in REPOS.items():
    os.makedirs(os.path.join(ROOT, name))
    run_steps(os.path.join(ROOT, name), steps)


def rev_of(name, ref):
    return ra.Git(os.path.join(ROOT, name)).out("rev-parse", ref).strip()


class Recording:
    def __init__(self):
        self.calls = []

    def __call__(self, argv, cwd, timeout):
        self.calls.append(list(argv))
        return ra._run_process(argv, cwd, timeout)


def repo_at(name, ref="head", runner=None):
    return deep.Repo(os.path.join(ROOT, name), rev_of(name, ref), runner or Recording())


def diff_text(name):
    return ra.Git(os.path.join(ROOT, name)).out("diff", "-U3", rev_of(name, "base"), rev_of(name, "head"), "--")


# --- The scripted structural reference --------------------------------------------------------------------------


class Symbol:
    def __init__(self, data):
        self.key, self.path, self.line, self.name, self.qualname = (data["key"], data["path"], data["line"],
                                                                   data["name"], data["qualname"])


class Call:
    def __init__(self, data):
        self.path, self.line, self.text, self.enclosing = data["path"], data["line"], data["text"], data["enclosing"]


class FakeReference:
    """What autoreview_deep reads of a Reference, answered from `data` (recorded; the Rust test builds the same)."""

    def __init__(self, data, repo):
        self.data = data
        self.repo = repo
        self.index = set(data["indexed"])
        self.stats = dict(data["stats"])

    def build(self):
        self.repo.reference = self
        return self

    def symbols_named(self, name):
        leaf = name.rsplit(".", 1)[-1]
        return [Symbol(item) for item in self.data["symbols"] if item["qualname"] == name or item["name"] == leaf]

    def callers(self, name, outside_changes=True):
        leaf = name.rsplit(".", 1)[-1]
        return [Call(item) for item in self.data["calls"]
                if item["callee"] == leaf and not (outside_changes and item["changed"])]

    def symbol_view(self, symbol, body=False):
        return f"VIEW {symbol.key} at {symbol.path}:{symbol.line} body={body}"

    def block(self):
        return self.data["block"]

    def scripted(self, kind, args):
        name = args.get("symbol") if kind != "symbol" else (args.get("name") or args.get("symbol"))
        name = name.strip() if isinstance(name, str) else ""
        answer = self.data["serve"].get(f"{kind} {name}")
        return tuple(answer) if answer else None


def fake_serve(reference, kind, args):
    return reference.scripted(kind, args)


deep.reference_module.serve = fake_serve

REFERENCE = {
    "indexed": ["src/app.py", "src/helper.py"],
    "stats": {"files": 2, "symbols": 3, "calls": 2},
    "symbols": [
        {"key": "src/helper.py:load", "path": "src/helper.py", "line": 1, "name": "load", "qualname": "load"},
        {"key": "src/app.py:show", "path": "src/app.py", "line": 6, "name": "show", "qualname": "show"},
        {"key": "src/app.py:KINDS", "path": "src/app.py", "line": 3, "name": "KINDS", "qualname": "KINDS"},
    ],
    "calls": [
        {"path": "src/app.py", "line": 9, "text": "return load(path)", "enclosing": "show", "callee": "load", "changed": True},
        {"path": "src/cli.py", "line": 4, "text": "show(sys.argv[1])", "enclosing": None, "callee": "show", "changed": False},
        {"path": "tests/test_app.py", "line": 7, "text": "assert kind in KINDS", "enclosing": "", "callee": "KINDS",
         "changed": False},
    ],
    "block": "Reference: show (src/app.py:6) is called from src/cli.py:4.",
    "serve": {
        "symbol show": ["symbol show -> 1 definition(s)", "src/app.py:6-11 show(path)"],
        "callers show": ["callers show -> 1 call sites", "src/cli.py:4 (resolved): show(sys.argv[1])"],
        "tests_of KINDS": ["tests_of KINDS -> 1 test(s)", "tests/test_app.py:7 test_kind"],
    },
}


def with_reference(repo, mode):
    """mode: None (no reference set), "fake" (the scripted reference)."""
    repo.reference = FakeReference(REFERENCE, repo) if mode == "fake" else None
    return repo


class Failing:
    def __init__(self, *args, **kwargs):
        raise RuntimeError("the index could not be built")


def reference_factory(mode):
    if mode == "fake":
        return lambda repo, files, base_repo=None: FakeReference(REFERENCE, repo)
    return Failing


# --- The scripted test session ----------------------------------------------------------------------------------


class Rejected(Exception):
    pass


deep.testing.TestsRejected = Rejected


class FakeSession:
    """What autoreview_deep reads of a TestSession. Runs are scripted: a path containing "fail" fails, one
    containing "gone" is unavailable; a mutation whose replacement contains "caught" is caught. The Rust test drives
    the same fake."""

    def __init__(self, limit, tracked, records=None, regressions=None, planned=True):
        self.limit = limit
        self.tracked = set(tracked)
        self.records = [dict(record) for record in records or []]
        self.regressions = regressions or []
        self.planned = planned

    def paths(self, raw):
        if not isinstance(raw, list) or not raw or not all(isinstance(item, str) for item in raw):
            raise Rejected("paths must be a list of tracked test files")
        for item in raw:
            if item not in self.tracked:
                raise Rejected(f"{item} is not a tracked test file")
        return list(raw)

    def _record(self, kind, paths, status, extra):
        if len(self.records) >= self.limit:
            raise Rejected("no test executions left in this review")
        tests = [{"id": f"{path}::test_x", "status": "failed" if status == "failed" else "passed",
                  "detail": "assert 1 == 2" if status == "failed" else ""} for path in paths]
        record = {"n": len(self.records) + 1, "kind": kind, "rev": "head", "runner": "pytest",
                  "command": "pytest -q " + " ".join(paths), "cwd": ".", "status": status, "tests": tests,
                  "passed": sum(1 for test in tests if test["status"] == "passed"),
                  "failed": sum(1 for test in tests if test["status"] == "failed"),
                  "output": "\n".join(f"line {index}" for index in range(30)) if status != "passed" else "ok",
                  "paths": paths, **extra}
        self.records.append(record)
        return record

    def run(self, paths, select=None):
        joined = " ".join(paths)
        status = "unavailable" if "gone" in joined else "failed" if "fail" in joined else "passed"
        return self._record("run", paths, status, {"select": select})

    def mutation(self, path, line, replacement, tests):
        if not isinstance(path, str) or not isinstance(line, int) or not isinstance(replacement, str):
            raise Rejected("a mutation names a path, a line and a replacement")
        paths = self.paths(tests)
        caught = "caught" in replacement
        record = self._record("mutation", paths, "failed" if caught else "passed",
                              {"mutation": {"path": path, "line": line, "was": "old text", "replacement": replacement},
                               "caught": caught})
        return record

    def plan(self, paths):
        return [("pytest", ".", paths)] if self.planned else []

    def compare(self, paths):
        runs = [self._record("automatic", [path], "unavailable" if "gone" in path else
                             "failed" if "fail" in path else "passed", {}) for path in paths]
        return {"runs": runs, "regressions": self.regressions}

    def close(self):
        pass


def session_of(spec):
    if spec is None:
        return None
    return FakeSession(spec["limit"], spec["tracked"], spec.get("records"), spec.get("regressions"),
                       spec.get("planned", True))


def answer(call):
    try:
        return {"ok": list(call())}
    except deep.Rejected as error:
        return {"rejected": str(error)}


# --- Repo ---------------------------------------------------------------------------------------------------------

repo_cases = []
for name in REPOS:
    runner = Recording()
    repo = repo_at(name, runner=runner)
    paths = []
    for raw in ["src/app.py", "./src/app.py", "src//app.py", "src/./app.py", " api.py ", "../x", "src/../x", "/etc/passwd",
                "~/x", ":x", "-x", "a\\b", "", "  ", "a\nb", "a\0b", 5, None, ["api.py"], "secret.txt", "big.txt",
                "api.py", "gen", "image.png"]:
        try:
            paths.append({"raw": raw, "ok": repo.path(raw)})
        except deep.Rejected as error:
            paths.append({"raw": raw, "rejected": str(error)})
    lines = {path: repo.lines(path) for path in ["src/app.py", "api.py", "big.txt", "image.png", "bin/data.bin",
                                                 "missing.py", "config.yaml"]}
    greps = []
    for pattern, fixed, word, pathspec, limit in [
            ("load", False, False, None, 20), ("KINDS", True, True, None, 2), ("def ", False, False, ":(glob)src/**", 20),
            ("parametrize|fixture", False, False, "tests/test_app.py", 8), ("(?:x", False, False, None, 5),
            ("VALUE_1", True, False, None, 1000), ("page", False, True, None, 20), ("nothing at all", True, False, None, 5)]:
        try:
            greps.append({"args": [pattern, fixed, word, pathspec, limit],
                          "hits": repo.grep(pattern, fixed=fixed, word=word, pathspec=pathspec, limit=limit)})
        except deep.Rejected as error:
            greps.append({"args": [pattern, fixed, word, pathspec, limit], "rejected": str(error)})
    first = repo.files()[0] if repo.files() else ""
    history = {"file": repo.history(first, 3), "app": repo.history("src/app.py" if name == "deep" else "api.py", 5),
               "lines": repo.line_history("src/app.py" if name == "deep" else "api.py", 3, 6, 5),
               "pickaxe": repo.pickaxe("KINDS" if name == "deep" else "PAGE_LIMIT", 5),
               "messages": repo.messages(rev_of(name, "base")), "messages_one": repo.messages(rev_of(name, "base"), 1),
               "messages_none": repo.messages(None), "messages_bad": repo.messages("no-such-rev")}
    try:
        repo._git("status")
        refused = "ran"
    except AssertionError as error:
        refused = str(error)
    bad_rev = deep.Repo(os.path.join(ROOT, name), "0" * 40, Recording())
    repo_cases.append({"repo": name, "files": repo.files(), "paths": paths, "lines": lines, "greps": greps,
                       "history": history, "refused": refused, "calls": runner.calls, "count": repo.calls,
                       "bad_rev_files": bad_rev.files()})

# --- serve_request ----------------------------------------------------------------------------------------------

DEEP_REQUESTS = [
    {"read": {"path": "src/helper.py", "start": 4, "end": 99999}}, {"read": {"path": "./src/app.py"}},
    {"read": {"path": "src/app.py", "start": "3", "end": 2}}, {"read": {"path": "src/app.py", "start": 0, "end": 0}},
    {"read": {"path": "src/app.py", "start": -5, "end": 4}}, {"read": {"path": "src/app.py", "start": [1]}},
    {"read": {"path": "src/app.py", "start": "x"}}, {"read": {"path": "src/app.py", "start": 2.7, "end": "5"}},
    {"read": {"path": "src/app.py", "start": True}},
    {"grep": {"pattern": "parametrize", "path_glob": "tests/**", "max": 5}}, {"grep": {"pattern": "no such text anywhere"}},
    {"grep": {"pattern": "load", "max": 1}}, {"grep": {"pattern": "load", "max": "x"}}, {"grep": {"pattern": "load", "max": 0}},
    {"grep": {"pattern": "load", "max": 500}}, {"grep": {"pattern": "load", "start": 2, "end": 7}},
    {"grep": {"pattern": "load", "start": 5}}, {"grep": {"pattern": "load", "end": 3}},
    {"grep": {"pattern": "load", "count_only": True, "max": 1}}, {"grep": {"pattern": "load", "count_only": 1}},
    {"grep": {"pattern": "o", "max": 2}}, {"grep": {"pattern": "(?:x)"}}, {"grep": {"pattern": "a(?=b)"}},
    {"grep": {"pattern": "load", "path_glob": None}}, {"grep": {"pattern": "load", "path_glob": "src/*.py", "start": "x"}},
    {"list": {"dir": "src"}}, {"list": {"dir": ""}}, {"list": {"dir": "."}}, {"list": {"dir": " tests "}},
    {"list": {"dir": "src/"}}, {"list": {"dir": "./src"}},
    {"definition": {"symbol": "load"}}, {"definition": {"symbol": "helper.load"}}, {"definition": {"symbol": "nothing_here"}},
    {"definition": {"symbol": "show"}}, {"references": {"symbol": "KINDS"}}, {"references": {"symbol": "app.show"}},
    {"references": {"symbol": "load"}},
    {"symbol": {"name": "show"}}, {"symbol": {"name": " load "}}, {"callers": {"symbol": "show"}}, {"callers": {"symbol": "load"}},
    {"callees": {"symbol": "show"}}, {"tests_of": {"symbol": "KINDS"}}, {"tests_of": {"symbol": "show"}},
    {"symbol": {"symbol": "show"}}, {"callers": {"name": "show"}}, {"symbol": {"name": "a b"}},
    {"history": {"path": "src/app.py", "n": 5}}, {"history": {"path": "src/app.py", "n": 0}}, {"history": {"path": "README.md"}},
    {"blame_range": {"path": "src/app.py", "start": 3, "end": 3}}, {"blame_range": {"path": "src/app.py", "start": 6}},
    {"blame_range": {"path": "src/app.py", "start": 1, "end": 500}}, {"blame_range": {"path": "src/app.py", "start": 9, "end": 2}},
    {"pickaxe": {"string": "except OSError", "n": 5}}, {"pickaxe": {"string": "never written anywhere"}},
    {"pickaxe": {"string": "KINDS", "n": "1"}},
    {"run_tests": {"paths": ["tests/test_app.py"]}}, {"mutation_check": {"path": "src/app.py", "line": 3,
                                                                         "replacement": "KINDS = []", "tests": ["tests/test_app.py"]}},
    # Refused.
    {"read": {"path": "../outside.txt"}}, {"read": {"path": "src/../../etc/passwd"}}, {"read": {"path": "/etc/passwd"}},
    {"read": {"path": "secret.txt"}}, {"read": {"path": "src/helper.py", "start": 900}}, {"read": {"path": "-x"}},
    {"grep": {"pattern": "(unclosed"}}, {"grep": {"pattern": "x" * 500}}, {"grep": {"pattern": "a\nb"}},
    {"grep": {"pattern": ""}}, {"grep": {"pattern": 5}},
    {"grep": {"pattern": "load", "path_glob": "../**"}}, {"grep": {"pattern": "load", "path_glob": ":(top)x"}},
    {"grep": {"pattern": "load", "path_glob": "/abs"}}, {"grep": {"pattern": "load", "path_glob": 3}},
    {"grep": {"pattern": "load", "path_glob": ""}},
    {"list": {"dir": "../.."}}, {"list": {"dir": "nowhere"}}, {"list": {"dir": 3}}, {"list": {"dir": " .."}},
    {"definition": {"symbol": "load; rm -rf /"}}, {"definition": {"symbol": ""}}, {"references": {"symbol": None}},
    {"callees": {"symbol": "nothing_here"}}, {"symbol": {"name": 4}},
    {"history": {"path": "../x"}}, {"history": {"path": "secret.txt"}},
    {"blame_range": {"path": "src/app.py", "start": 500, "end": 600}},
    {"pickaxe": {"string": "x"}}, {"pickaxe": {"string": "a\nb"}}, {"pickaxe": {"string": "y" * 121}},
    {"run": {"cmd": "ls"}}, {"read": {"path": "src/app.py"}, "list": {"dir": "."}}, "read everything", {},
    {"read": "src/app.py"}, {"run_tests": {"paths": "x"}}, {"run_tests": {"paths": ["nope.py"]}},
    {"mutation_check": {"path": "src/app.py", "line": "3", "replacement": "x", "tests": ["tests/test_app.py"]}},
]
WIDE_REQUESTS = [
    {"list": {"dir": "gen"}}, {"list": {"dir": "."}}, {"list": {"dir": "pkg"}}, {"read": {"path": "big.txt"}},
    {"read": {"path": "bin/data.bin"}}, {"read": {"path": "image.png"}}, {"read": {"path": "api.py", "start": 2}},
    {"grep": {"pattern": "VALUE_", "max": 3}}, {"grep": {"pattern": "VALUE_", "count_only": True}},
    {"grep": {"pattern": "VALUE_1", "path_glob": "gen/f1*.py", "max": 50}},
    {"grep": {"pattern": "VALUE", "start": 1, "end": 1, "max": 2}},
    {"references": {"symbol": "handler"}}, {"definition": {"symbol": "handler"}}, {"definition": {"symbol": "paginate"}},
    {"definition": {"symbol": "VALUE_7"}}, {"definition": {"symbol": "port"}},
    {"history": {"path": "tests/test_api.py", "n": 50}}, {"blame_range": {"path": "api.py", "start": 4, "end": 200}},
    {"pickaxe": {"string": "PAGE_LIMIT", "n": 99}},
    {"run_tests": {"paths": ["tests/test_api.py"], "select": "paginate"}},
    {"run_tests": {"paths": ["tests/fail_test.py"]}},
    {"mutation_check": {"path": "api.py", "line": 6, "replacement": "limit = 1  # caught", "tests": ["tests/test_api.py"]}},
    {"run_tests": {"paths": ["tests/test_api.py"]}},
]

SESSION = {"limit": 3, "tracked": ["tests/test_app.py", "tests/test_api.py", "tests/fail_test.py"]}
serve_cases = []
for name, requests in [("deep", DEEP_REQUESTS), ("wide", WIDE_REQUESTS)]:
    for reference in [None, "fake"] if name == "deep" else [None]:
        for tests in [None, SESSION]:
            runner = Recording()
            repo = with_reference(repo_at(name, runner=runner), reference)
            session = session_of(tests)
            results = []
            for request in requests:
                before = len(runner.calls)
                result = answer(lambda: deep.serve_request(repo, request, session))
                result["git"] = [argv[1] for argv in runner.calls[before:]]
                results.append(result)
            serve_cases.append({"repo": name, "reference": reference, "tests": tests, "requests": requests,
                                "results": results, "records": session.records if session else None})

rounds = []
for name, requests, limit, tests in [
        ("deep", [{"read": {"path": "src/app.py"}}] * 9 + [{"read": {"path": "nope"}}], deep.ROUND_CHARS, None),
        ("deep", [{"read": {"path": "src/app.py"}}] * 8, 300, None),
        ("deep", [{"read": {"path": "src/app.py"}}, {"grep": {"pattern": "load"}}, "bad", {"list": {"dir": "src"}}], 320, None),
        ("deep", [{"read": {"path": "src/app.py"}}, {"read": {"path": "src/helper.py"}}], 293, None),
        ("deep", [{"read": {"path": "nope" * 60}}, {"grep": {"pattern": "é ü"}}, {"π": {}}], deep.ROUND_CHARS, None),
        ("deep", "not a list", deep.ROUND_CHARS, None), ("deep", [], deep.ROUND_CHARS, None),
        ("deep", [{"run_tests": {"paths": ["tests/test_app.py"]}}] * 2, deep.ROUND_CHARS, SESSION),
        ("wide", [{"read": {"path": "big.txt"}}, {"list": {"dir": "gen"}}, {"list": {"dir": "gen"}}], 2_500, None),
]:
    repo = repo_at(name)
    session = session_of(tests)
    blocks, served, rejected = deep.serve_blocks(repo, requests, limit, session)
    session = session_of(tests)
    text, served_again, rejected_again = deep.serve(repo_at(name), requests, limit, session)
    rounds.append({"repo": name, "requests": requests, "limit": limit, "tests": tests,
                   "blocks": [list(block) for block in blocks], "served": served, "rejected": rejected,
                   "text": text, "again": [served_again, rejected_again]})

# --- The map ------------------------------------------------------------------------------------------------------

EXTRACT_CASES = [
    {"diff": diff_text("deep"), "sources": None},
    {"diff": diff_text("wide"), "sources": None},
    {"diff": ("diff --git a/src/api.py b/src/api.py\n--- a/src/api.py\n+++ b/src/api.py\n@@ -1,1 +1,3 @@\n def handler(request):\n"
              "+    limit = int(os.environ[\"PAGE_LIMIT\"])\n+    return db.execute(\"select * from users where token = \" + request.args[\"token\"])\n"),
     "sources": {}},
    {"diff": ("diff --git a/app.ts b/app.ts\n--- a/app.ts\n+++ b/app.ts\n@@ -10,2 +10,9 @@ class A {\n   run() {\n"
              "+    const KEY_NAME = process.env.API_KEY_VALUE;\n+    // This comment claims something about the run.\n"
              "+    { key: 'metrics_ingress_cidrs', value: 1 }\n+    const handler = async (req) => process(req.body.user_id);\n"
              "+    export const MAX_ITEMS = 10;\n+    run(\"--force-all\", '--no-cache');\n+    db.query(`ALTER TABLE accounts ADD COLUMN flag`);\n"
              "   }\n"),
     "sources": {"app.ts": ["// header"] * 3 + ["export function outer(x) {"] + ["  x;"] * 5 + ["class A {", "  run() {",
                                                                                              "    stuff();", "  }", "}"]}},
    {"diff": ("diff --git a/docs/a.md b/docs/a.md\n--- a/docs/a.md\n+++ b/docs/a.md\n@@ -1 +1,4 @@\n # T\n"
              "+This document claims the service retries three times.\n+short\n+Use `--retry-max` with care, it is risky.\n"
              "diff --git a/settings.toml b/settings.toml\n--- a/settings.toml\n+++ b/settings.toml\n@@ -1 +1,4 @@\n [a]\n"
              "+retry_max = 3\n+# commented_key = 1\n+// slash_key = 2\n"
              "diff --git a/yarn.lock b/yarn.lock\n--- a/yarn.lock\n+++ b/yarn.lock\n@@ -1 +1 @@\n-a\n+def locked():\n"),
     "sources": {"settings.toml": ["[a]", "retry_max = 3"]}},
    {"diff": ("diff --git a/m.py b/m.py\n--- a/m.py\n+++ b/m.py\n@@ -1,3 +1,6 @@\n def first():\n"
              + "".join(f"+    call_{index}(x)\n" for index in range(20))
              + "+    help = 'A description long enough to be a claim here'\n+    '''Docstring that claims a lot about the code'''\n"),
     "sources": {"m.py": ["def first():"] + ["    pass"] * 30}},
    {"diff": ("diff --git a/far.py b/far.py\n--- a/far.py\n+++ b/far.py\n@@ -100,1 +100,2 @@\n x = 1\n+y = compute(1)\n"),
     "sources": {"far.py": ["def far_away():"] + ["    pass"] * 120}},
    {"diff": ("diff --git a/near.py b/near.py\n--- a/near.py\n+++ b/near.py\n@@ -82,1 +82,2 @@\n x = 1\n+y = compute(1)\n"),
     "sources": {"near.py": ["def near_enough():"] + ["    pass"] * 120}},
    # The definition 78 lines above the added line is the farthest the search reaches.
    {"diff": ("diff --git a/edge.py b/edge.py\n--- a/edge.py\n+++ b/edge.py\n@@ -78,1 +78,2 @@\n x = 1\n+y = compute(1)\n"),
     "sources": {"edge.py": ["def edge_def():"] + ["    pass"] * 120}},
    {"diff": ("diff --git a/edge.py b/edge.py\n--- a/edge.py\n+++ b/edge.py\n@@ -79,1 +79,2 @@\n x = 1\n+y = compute(1)\n"),
     "sources": {"edge.py": ["def edge_def():"] + ["    pass"] * 120}},
]


def reader_of(name, sources):
    if sources is None:
        return ra._rev_reader(ra.Git(os.path.join(ROOT, name)), rev_of(name, "head"))
    return lambda path: sources.get(path)


extracts = []
for index, case in enumerate(EXTRACT_CASES):
    name = "deep" if index == 0 else "wide"
    found = deep.extract(ra.parse_diff(case["diff"]), reader_of(name, case["sources"]))
    extracts.append({**case, "repo": name, "found": found})

BRIEF_CASES = [
    {"repo": "deep", "diff": diff_text("deep"), "sources": None, "base": True, "reference": None},
    {"repo": "deep", "diff": diff_text("deep"), "sources": None, "base": False, "reference": "fake",
     "title": "Make `show` safe for missing files", "description": "Short.\n- Callers get None instead of an exception.\n"
                                                                  "* Another description line that is long enough\n"
                                                                  "  -- dashes and stars stripped from this line --  "},
    {"repo": "deep", "diff": diff_text("deep"), "sources": None, "base": True, "reference": None, "limit": 500},
    {"repo": "wide", "diff": diff_text("wide"), "sources": None, "base": True, "reference": None,
     "title": "Add paging to the handler", "description": "\n".join(f"Line {index} of a long description here" for index in range(8))},
    {"repo": "wide", "diff": EXTRACT_CASES[2]["diff"], "sources": {}, "base": False, "reference": None},
    {"repo": "wide", "diff": EXTRACT_CASES[3]["diff"], "sources": EXTRACT_CASES[3]["sources"], "base": False, "reference": "fake"},
    {"repo": "wide", "diff": EXTRACT_CASES[4]["diff"], "sources": EXTRACT_CASES[4]["sources"], "base": False, "reference": None},
    {"repo": "wide", "diff": ("diff --git a/api.py b/api.py\n--- a/api.py\n+++ b/api.py\n@@ -4,1 +4,3 @@\n def handler(request):\n"
                              "+    # just a comment that is long enough\n+    \"\"\"A docstring line.\"\"\"\n"),
     "sources": None, "base": False, "reference": None},
    {"repo": "wide", "diff": ("diff --git a/tests/test_api.py b/tests/test_api.py\n--- a/tests/test_api.py\n+++ b/tests/test_api.py\n"
                              "@@ -1,1 +1,2 @@\n from api import handler, paginate\n+import pytest\n"),
     "sources": None, "base": False, "reference": None},
    {"repo": "wide", "diff": ("diff --git a/Dockerfile b/Dockerfile\nnew file mode 100644\n--- /dev/null\n+++ b/Dockerfile\n"
                              "@@ -0,0 +1 @@\n+FROM python:3\n"), "sources": {}, "base": False, "reference": None},
    {"repo": "wide", "diff": "", "sources": {}, "base": False, "reference": None},
]
briefs = []
for case in BRIEF_CASES:
    name = case["repo"]
    deep.reference_module.Reference = reference_factory(case["reference"])
    repo = repo_at(name)
    files = ra.parse_diff(case["diff"])
    brief = deep.build_brief(repo, files, reader_of(name, case["sources"]), case.get("limit", deep.BRIEF_CHARS),
                             title=case.get("title", ""), description=case.get("description", ""),
                             base=rev_of(name, "base") if case["base"] else None)
    briefs.append({**case, "brief": {
        "text": brief.text, "lenses": brief.lenses, "symbols": brief.symbols, "callers": brief.callers,
        "tests": brief.tests, "claims": brief.claims, "required": brief.required,
        "claim_list": [list(item) for item in brief.claim_list], "helpers": brief.helpers, "extracted": brief.extracted,
        "reference": brief.reference is not None, "reference_text": brief.reference_text,
        "reference_stats": brief.reference_stats}, "repo_reference": repo.reference is not None})

behaviour = {line: bool(deep._BEHAVIOUR.match(line)) for line in [
    "x = 1", "   x = 1", "# c", "  // c", "/* c", " * c", "-- sql", "<!-- c", "--> c", '"""doc"""', '"""open', "'''doc'''",
    "'''x", '"just a string"', "'a', ", '"a" + b', '"""a""" + x', "", "   ", "\t", "return 1", "*args", "x # y",
    '"""a"b"""', "'x' ,  ", '"""x\n']}
helpers = {
    "numbered": [deep._numbered(["a", "b" * 400, "c"], 1, 3), deep._numbered(["a", "b"], 2, 1)],
    "hits": [deep._hits([]), deep._hits([("a.py", 3, "  x  "), ("b.py", 10, "y" * 300)])],
    "definition": [deep._definition_pattern(name) for name in ["load", "a.b.c_d", "x$y", "with-dash", "plain"]],
    "is_code": {path: deep.is_code(path) for path in ["a.py", "a.yaml", "Dockerfile", "x/Makefile", ".env.local", "a.md",
                                                       "src/app.ts", "a.LOCK", "dir.sql/x.py"]},
    "behaviour": behaviour,
}

# --- Evidence -----------------------------------------------------------------------------------------------------

DEEP_DIFF_LINES = {"src/app.py": [3, 7, 8, 10, 11]}
CITATIONS = [
    [{"path": "src/app.py", "line": 7, "quote": "# Raises OSError when the file is missing."}],
    [{"path": "src/helper.py", "line": 5, "quote": 'return "ERROR: unreadable"'}],
    [{"path": "src/helper.py", "line": 3, "quote": 'return "ERROR: unreadable"'}],
    [{"path": "src/helper.py", "line": 2, "quote": 'return "ERROR: unreadable"'}],
    [{"path": "src/app.py", "line": 3, "quote": 'KINDS = ["a", "b", "c"] # and more text around it'}],
    [{"path": "src/app.py", "line": 6, "quote": "def"}],
    [{"path": "src/app.py", "line": 6, "quote": 5}],
    [{"path": "nope.py", "line": 1, "quote": "abcd"}],
    [{"path": "src/app.py", "line": "x", "quote": "abcd"}],
    [{"path": "src/app.py", "line": None, "quote": "abcd"}],
    [{"path": "src/app.py", "line": [1], "quote": "abcd"}],
    [{"path": "src/app.py", "line": "4", "quote": "  def   show(path):  "}],
    [{"path": "src/app.py", "line": 1, "quote": "never said anywhere at all"}],
    ["not an object"],
    [],
    "not a list",
    None,
    [{"path": "src/app.py", "line": 9, "quote": "return load(path)"}, {"path": "src/helper.py", "line": 1, "quote": "def load"}],
    [{"path": "src/app.py", "line": 9, "quote": "return load(path)"}] * 8,
    [{"path": "src/app.py", "line": 9, "quote": "return load(path)"}, "bad"],
    [{"path": "src/app.py", "line": 9, "quote": "x" * 100}],
    [{"path": "src/app.py", "line": -3, "quote": "from helper"}],
    [{"path": "src/app.py", "line": 100, "quote": "return None"}],
]
citation_cases = []
diff_sets = {path: set(lines) for path, lines in DEEP_DIFF_LINES.items()}
for raw in CITATIONS:
    citations, problem, beyond = deep.check_citations(repo_at("deep"), raw, diff_sets)
    citation_cases.append({"raw": raw, "citations": citations, "problem": problem, "beyond": beyond})

RUNS = [{"n": 1, "kind": "run", "rev": "head", "command": "pytest -q tests/test_app.py", "cwd": ".", "status": "passed",
         "tests": [{"id": "tests/test_app.py::test_kind", "status": "passed"}], "passed": 1, "failed": 0, "output": "ok",
         "paths": ["tests/test_app.py"]},
        {"n": 2, "kind": "mutation", "rev": "head", "command": "pytest -q tests/test_app.py", "cwd": "sub", "status": "failed",
         "tests": [{"id": "tests/test_app.py::test_kind[c]", "status": "failed", "detail": "assert 'c' in KINDS"}],
         "passed": 0, "failed": 1, "output": "\n".join(f"out {index}" for index in range(40)),
         "mutation": {"path": "src/app.py", "line": 3, "was": 'KINDS = ["a", "b", "c"]', "replacement": "KINDS = []"},
         "caught": True, "paths": ["tests/test_app.py"]},
        {"n": 3, "kind": "run", "rev": "head", "command": "pytest", "status": "unavailable", "tests": [], "passed": 0,
         "failed": 0, "output": "ModuleNotFoundError", "paths": []}]

GOOD = {"file": "src/app.py", "line": 10, "severity": "high", "category": "bug",
        "claim": "show() catches OSError, but load() never raises it.",
        "why": "load returns a sentinel string.", "scenario": "show('missing') returns 'ERROR: unreadable'.",
        "suggested_fix": "Check the sentinel.", "confidence": 0.8,
        "evidence": [{"path": "src/helper.py", "line": 5, "quote": 'return "ERROR: unreadable"'}],
        "unpinned": {"behaviour": "the sentinel string is returned", "change": "return None instead of the sentinel",
                     "closest_test": {"path": "tests/test_app.py", "line": 6},
                     "mutation": {"path": "src/helper.py", "line": 5, "replacement": "        return None"}},
        "consequence": "Callers print the sentinel."}


def variant(**changes):
    out = dict(GOOD)
    for key, value in changes.items():
        if value is KeyError:
            out.pop(key, None)
        else:
            out[key] = value
    return out


NORMALIZE = [
    {"raw": GOOD, "lens": "claims"},
    {"raw": GOOD, "lens": "tests", "to_level": True, "enrich": True},
    {"raw": variant(category="tests", severity="critical"), "lens": "tests", "enrich": True},
    {"raw": variant(severity="major", category="", scenario=""), "lens": "siblings", "to_level": True},
    {"raw": variant(severity="major"), "lens": "siblings"},
    {"raw": variant(severity=None), "lens": "claims"}, {"raw": variant(severity=KeyError), "lens": "claims"},
    {"raw": variant(severity=" NIT "), "lens": "claims"},
    {"raw": variant(confidence="0.3"), "lens": "claims"}, {"raw": variant(confidence=None), "lens": "claims"},
    {"raw": variant(confidence=7), "lens": "claims"}, {"raw": variant(confidence=-1), "lens": "claims"},
    {"raw": variant(confidence="high"), "lens": "claims"}, {"raw": variant(confidence=True), "lens": "claims"},
    {"raw": variant(confidence=KeyError), "lens": "claims"},
    {"raw": variant(claim="   "), "lens": "claims"}, {"raw": variant(claim=KeyError), "lens": "claims"},
    {"raw": variant(file="nope.py"), "lens": "claims"}, {"raw": variant(line="ten"), "lens": "claims"},
    {"raw": variant(line=99), "lens": "claims"}, {"raw": variant(line=0), "lens": "claims"},
    {"raw": variant(line="11"), "lens": "claims"},
    {"raw": variant(evidence=[]), "lens": "claims"},
    {"raw": variant(evidence=[], test_run=1), "lens": "tests"},
    {"raw": variant(evidence=[], test_run=3), "lens": "tests"},
    {"raw": variant(evidence=[], test_run=True), "lens": "tests"},
    {"raw": variant(evidence=[], test_run=1.0), "lens": "tests"},
    {"raw": variant(test_run=2), "lens": "tests"},
    {"raw": variant(evidence=[{"path": "src/app.py", "line": 1, "quote": "nothing like it"}], test_run=1), "lens": "tests"},
    {"raw": variant(evidence=[{"path": "src/app.py", "line": 7, "quote": "Raises OSError when the file"}]), "lens": "claims"},
    {"raw": variant(claim="x" * 400, why="y " * 400, scenario="z" * 600, suggested_fix="w" * 600, category="Security hole"),
     "lens": "inputs"},
    {"raw": variant(category="weird thing"), "lens": "claims"}, {"raw": variant(category=None), "lens": "claims"},
    {"raw": "not an object", "lens": "claims"}, {"raw": ["x"], "lens": "claims"},
]
normalized = []
for case in NORMALIZE:
    finding, problem = deep.normalize_deep(
        repo_at("deep"), case["raw"], case["lens"], diff_sets, aa.capped_level, RUNS,
        aa.to_level if case.get("to_level") else None, aa.parse_rules if case.get("enrich") else None)
    normalized.append({**case, "finding": finding, "problem": problem})

windows = []
for finding in [{"citations": [{"path": "src/app.py", "line": 3}, {"path": "src/helper.py", "line": 5}]},
                {"citations": [{"path": "src/app.py", "line": 11}]}, {"citations": [{"path": "nope.py", "line": 1}]},
                {"citations": []}, {}, {"citations": None}]:
    windows.append({"finding": finding, "text": deep.cited_windows(repo_at("deep"), finding)})


def fnd(file, line, category, claim, source=None, level="medium", **extra):
    out = {"file": file, "line": line, "category": category, "claim": claim, "level": level,
           "reviewers": [source or "bugs"], **extra}
    if source is not None:
        out["source"] = source
    return out


MERGE_CASES = [
    {"fast": [fnd("a.py", 10, "correctness", "the loop skips the last item in the list", None, replacement="fixed", end_line=11)],
     "deep": [fnd("a.py", 12, "correctness", "a different claim entirely about other things", "deep:claims", beyond_diff=False,
                  citations=[{"path": "a.py", "line": 12}])]},
    {"fast": [fnd("a.py", 10, "correctness", "the loop skips the last item in the list", None, replacement="fixed", end_line=11)],
     "deep": [fnd("a.py", 10, "correctness", "the loop skips the final item of the list", "deep:claims", beyond_diff=True,
                  citations=[{"path": "b.py", "line": 1}]),
              fnd("a.py", 30, "correctness", "the loop skips the last item in the list again", "deep:tests", level="high",
                  beyond_diff=True, citations=[])]},
    {"fast": [],
     "deep": [fnd("a.py", 10, "correctness", "first claim about a thing here", "deep:claims", level="low",
                  citations=[{}], reviewers=["deep:claims"]),
              fnd("a.py", 12, "correctness", "second claim about another thing", "deep:siblings", level="critical",
                  citations=[{}, {}], reviewers=["deep:siblings", "deep:claims"]),
              fnd("a.py", 13, "correctness", "third claim about yet another", "deep:siblings", level="critical",
                  citations=[{}], reviewers=["deep:siblings"]),
              fnd("a.py", 11, "tests", "tests category never merges with correctness", "deep:tests", level="medium",
                  citations=[]),
              fnd("b.py", 11, "correctness", "first claim about a thing here", "deep:inputs", level="nit")]},
    {"fast": [fnd("c.py", 5, "security", "token is logged in plain text to stdout", "bugs", reviewers=["security", "bugs"],
                  replacement="redacted"),
              fnd("c.py", 50, "security", "unrelated", "bugs")],
     "deep": [fnd("c.py", 7, "security", "the token reaches the log", "deep:inputs", beyond_diff=True,
                  citations=[{"path": "c.py", "line": 7}], replacement="kept"),
              fnd("c.py", 5, "security", "token is logged in plain text to stdout", "deep:claims", beyond_diff=False,
                  citations=[])]},
    {"fast": [fnd("d.py", 1, "correctness", "x", None)],
     "deep": [fnd("d.py", 1, "correctness", "x", "deep:a", beyond_diff=True), fnd("d.py", 1, "correctness", "x", "deep:a",
                                                                                 beyond_diff=True)]},
    {"fast": [fnd("e.py", 20, "correctness", "fast finding near a deep one", "fast", replacement="r", end_line=22)],
     "deep": [fnd("e.py", 20, "correctness", "deep finding at the same line", "deep:claims", beyond_diff=True, citations=[])]},
]
merges = []
for case in MERGE_CASES:
    records = []
    merged, duplicates = deep.merge(json.loads(json.dumps(case["fast"])), json.loads(json.dumps(case["deep"])), records)
    plain, plain_duplicates = deep.merge(json.loads(json.dumps(case["fast"])), json.loads(json.dumps(case["deep"])))
    merges.append({**case, "merged": merged, "duplicates": duplicates, "records": records,
                   "plain": [plain == merged, plain_duplicates]})

identical = [[a, b, deep._nearly_identical(a, b)] for a, b in [
    ("the loop skips the last item", "the loop skips the final item"), ("one two three four five", "one two three four six"),
    ("ab cd", "ab cd"), ("Alpha_beta GAMMA delta", "alpha_beta gamma DELTA"), ("x1 y22 z333 w4444", "z333 w4444 v55555 y22"),
    ("über straße größe", "über straße größe"), ("", "")]]

SHAPES = [{"id": "T1", "name": "registry-member", "text": "a"}, {"id": "T2", "name": "unpinned-behaviour", "text": "b"},
          {"id": "T3", "name": "env-in-deploy", "text": "c"}, {"id": "T4", "name": "comment-vs-code", "text": "d"},
          {"id": "T5", "name": "unknown-shape", "text": "e"}, {"id": "T6", "text": "no name"},
          {"id": "T7", "name": "input-defeats-guard", "text": "f"}]
shape_cases = [{"lenses": lenses, "out": deep.shapes_by_lens(SHAPES, lenses)} for lenses in [
    ["claims", "siblings", "deployment", "tests", "inputs"], ["tests", "claims"], [], ["siblings", "siblings"], ["inputs"]]]
mention_cases = [[text, item, deep._mentions(text, item)] for text, item in [
    ("T1: not applicable", SHAPES[0]), ("T12 checked", SHAPES[0]), ("xT1 checked", SHAPES[0]), ("(T1)", SHAPES[0]),
    ("T1a", SHAPES[0]), ("_T1", SHAPES[0]), ("checked the registry-member list", SHAPES[0]), ("nothing", SHAPES[0]),
    ("anything", {"id": "T9"}), ("", {"id": "T9"}), ("T9", {"id": "T9", "name": "zzz"}), ("ÉT1", SHAPES[0]),
    ("T1T1", SHAPES[0]), ("a.T2", {"id": "T2", "name": "q"})]]

test_lines = [[test_id, *deep._test_line(repo_at("deep"), test_id)] for test_id in [
    "tests/test_app.py::test_kind", "tests/test_app.py::test_kind[c]", "tests/test_app.py::TestX::test_kind",
    "tests/test_app.py", "tests/test_app.py::missing_test", "nope.py::test_kind", "tests/test_app.py::KINDS:extra",
    "src/app.py::show", "../x::y"]]
COMPARED = {"runs": [], "regressions": [
    {"id": "tests/test_app.py::test_kind[c]", "head": {"n": 4, "command": "pytest tests/test_app.py"}, "base": {"n": 5},
     "detail": "AssertionError:   'c'\n not in list"},
    {"id": "nope.py::test_x", "head": {"n": 6, "command": "x"}, "base": {"n": 7}},
    {"id": "tests/test_app.py", "head": {"n": 8, "command": "pytest"}, "base": {"n": 9}, "detail": None},
    {"id": "src/app.py::show", "head": {"n": 10, "command": "c"}, "base": {"n": 11}, "detail": "d" * 300},
    {"id": "tests/test_app.py::a", "head": {"n": 12, "command": "c"}, "base": {"n": 13}},
    {"id": "tests/test_app.py::b", "head": {"n": 14, "command": "c"}, "base": {"n": 15}},
]}
regressions = deep.regression_findings(repo_at("deep"), COMPARED)

views = []
for args in [("diff", "brief", "retrieved", "context", "intent", "guidance", "reference"), ("d" * 40_005, None, "", "", "", "", ""),
             ("d" * 40_000, "", "", "c", "", "g", ""), ("", "b", "", "", "i", "", "r")]:
    views.append({"args": list(args), "views": deep.shared_views(*args)})

# --- Tests: start_tests (fake session), tests_block, prove_unpinned, summaries ----------------------------------------

summaries = [{"record": record, "text": deep.testing.summarize(record)} for record in RUNS + [
    {"n": 4, "kind": "mutation", "rev": "head", "command": "c", "status": "passed", "tests": [], "output": "",
     "mutation": {"path": "a.py", "line": 2, "was": "x", "replacement": "y"}, "caught": False},
    {"n": 5, "kind": "mutation", "rev": "base", "command": "c", "status": "timeout", "tests": [], "output": "a\nb",
     "mutation": {"path": "a.py", "line": 2, "was": "x", "replacement": "y"}, "caught": False, "cwd": None},
    {"n": 6, "kind": "run", "rev": "head", "command": "c", "status": "failed", "passed": 0, "failed": 9, "output": "z",
     "tests": [{"id": f"t{index}", "status": "error", "detail": "" if index % 2 else "boom"} for index in range(10)]}]]


class FakeSandbox:
    mechanism = "bwrap"


def fake_session_factory(spec):
    def make(root, rev, base, tracked, read, sandbox, **options):
        session = session_of(spec)
        session.env_kind = "uv"
        session.toolchain = ["python3.12"]
        return session
    return make


START_CASES = [
    {"repo": "deep", "brief": 0, "session": {"limit": 4, "tracked": ["tests/test_app.py"], "regressions": COMPARED["regressions"][:2]}},
    {"repo": "deep", "brief": 0, "session": {"limit": 0, "tracked": []}},
    {"repo": "deep", "brief": 0, "session": {"limit": 4, "tracked": [], "planned": False}},
    {"repo": "wide", "brief": 3, "session": {"limit": 4, "tracked": ["tests/test_api.py"]}},
    {"repo": "wide", "brief": 4, "session": {"limit": 4, "tracked": []}},
    {"repo": "deep", "brief": 0, "session": None},
]
started_cases = []
for case in START_CASES:
    brief_case = BRIEF_CASES[case["brief"]]
    deep.reference_module.Reference = reference_factory(None)
    repo = repo_at(case["repo"])
    files = ra.parse_diff(brief_case["diff"])
    brief = deep.build_brief(repo, files, reader_of(case["repo"], brief_case["sources"]),
                             base=rev_of(case["repo"], "base") if brief_case["base"] else None)
    if case["session"] is None:
        started = deep.start_tests(repo, files, None, brief, root=repo.root, rev=repo.rev, clock=lambda: 0.0)
    else:
        deep.testing.TestSession = fake_session_factory(case["session"])
        started = deep.start_tests(repo, files, {"sandbox": FakeSandbox()}, brief, root=repo.root, rev=repo.rev,
                                   clock=lambda: 0.0)
    session = started["session"]
    started_cases.append({**case, "started": {key: value for key, value in started.items() if key not in ("session", "sandbox")},
                          "block": deep.tests_block(started), "records": session.records if session else None})
no_sandbox = deep.start_tests(repo_at("deep"), [], {"sandbox": None}, brief, root="", rev="", clock=lambda: 0.0)

PROVE_FINDINGS = [
    {"category": "tests", "level": "low", "claim": "p1", "unpinned": {"mutation": {"path": "src/app.py", "line": 3,
                                                                                    "replacement": "KINDS = []"},
                                                                       "closest_test": {"path": "tests/test_app.py"}}},
    {"category": "tests", "level": "high", "claim": "p2", "unpinned": {"mutation": {"path": "src/app.py", "line": 9,
                                                                                     "replacement": "return 1 # caught"}}},
    {"category": "tests", "level": "medium", "claim": "p3", "proof": "proven",
     "unpinned": {"mutation": {"path": "a", "line": 1, "replacement": "x"}}},
    {"category": "correctness", "level": "critical", "claim": "p4",
     "unpinned": {"mutation": {"path": "a", "line": 1, "replacement": "x"}}},
    {"category": "tests", "claim": "p5", "unpinned": {"mutation": {"path": "src/app.py", "line": "bad", "replacement": "x"},
                                                       "closest_test": {"path": "src/app.py"}}},
    {"category": "tests", "level": "nit", "claim": "p6", "unpinned": {"change": "no mutation"}},
    {"category": "tests", "level": "medium", "claim": "p7", "unpinned": {"mutation": {"path": "src/app.py", "line": 6,
                                                                                       "replacement": "def show(p): caught"},
                                                                          "closest_test": {"path": "tests/missing_test.py"}}},
    {"category": "tests", "level": "weird", "claim": "p8", "unpinned": {"mutation": {"path": "src/app.py", "line": 2,
                                                                                      "replacement": "pass"}}},
]
PROVE_CASES = [
    {"session": {"limit": 9, "tracked": ["tests/test_app.py", "tests/fail_test.py"]}, "tests": ["tests/test_app.py"]},
    {"session": {"limit": 2, "tracked": ["tests/test_app.py"]}, "tests": ["tests/test_app.py"]},
    {"session": {"limit": 9, "tracked": ["tests/test_app.py"],
                 "records": [{"n": 1, "kind": "automatic", "status": "failed", "paths": ["tests/test_app.py"]}]},
     "tests": ["tests/test_app.py", "tests/other_test.py"]},
    {"session": {"limit": 9, "tracked": ["tests/test_app.py"]}, "tests": []},
    # The suite already failed before the mutation: a failing mutant refutes nothing.
    {"session": {"limit": 9, "tracked": ["tests/test_app.py"],
                 "records": [{"n": 1, "kind": "automatic", "status": "failed", "paths": ["tests/test_app.py"]}]},
     "tests": ["tests/test_app.py"]},
    {"session": {"limit": 9, "tracked": ["tests/test_app.py"],
                 "records": [{"n": 1, "kind": "run", "status": "failed", "paths": ["tests/test_app.py"]}]},
     "tests": ["tests/test_app.py"]},
    {"session": None, "tests": ["tests/test_app.py"]},
]
proved = []
deep.reference_module.Reference = reference_factory(None)
proof_brief = deep.build_brief(repo_at("deep"), ra.parse_diff(diff_text("deep")), reader_of("deep", None))
for case in PROVE_CASES:
    findings = json.loads(json.dumps(PROVE_FINDINGS))
    session = session_of(case["session"])
    proof_brief.tests = list(case["tests"])
    deep.prove_unpinned(session, repo_at("deep"), findings, proof_brief)
    proved.append({**case, "findings": findings, "records": session.records if session else None})

ASSURANCE_CASES = [
    {"brief": 0, "investigators": [{"lens": "claims", "requests": 4, "status": "done"}, {"lens": "tests", "requests": 1,
                                                                                         "status": "rounds exhausted"}],
     "runs": [], "mechanism": None, "checked": ["show() returns None on a missing file.", "b.", "c.", "d."]},
    {"brief": 3, "investigators": [{"lens": "claims", "requests": 1, "status": "done"}],
     "runs": [{"status": "passed"}, {"status": "failed"}, {"status": "passed"}], "mechanism": "bwrap", "checked": []},
    {"brief": 4, "investigators": [{"lens": "claims", "requests": 0, "status": "failed"}], "runs": [], "mechanism": None,
     "checked": []},
    {"brief": 10, "investigators": [{"lens": "deployment", "requests": 2, "status": "done"}], "runs": [{"status": "passed"}],
     "mechanism": None, "checked": ["x."]},
    {"brief": 1, "investigators": [], "runs": [], "mechanism": None, "checked": []},
]
assurances = []
for case in ASSURANCE_CASES:
    brief_case = BRIEF_CASES[case["brief"]]
    deep.reference_module.Reference = reference_factory(None)
    brief = deep.build_brief(repo_at(brief_case["repo"]), ra.parse_diff(brief_case["diff"]),
                             reader_of(brief_case["repo"], brief_case["sources"]),
                             title=brief_case.get("title", ""), description=brief_case.get("description", ""),
                             base=rev_of(brief_case["repo"], "base") if brief_case["base"] else None)
    out = deep.assurance({"brief": brief, "investigators": case["investigators"],
                          "tests": {"runs": case["runs"], "mechanism": case["mechanism"]}, "checked": case["checked"]})
    assurances.append({**case, "assurance": out})

dumps = [[value, json.dumps(value)] for value in [{"read": {"path": "é/π"}}, ["a\"b\\c\n\t\x01\x7f", 1.5, 2, None, True],
                                                    {"x": "😀"}, "plain", {"e": {}, "l": []}, 1e20, 0.1]]

# --- Investigators: the retrieval loop over the real Frames scheduler, with scripted rlm.map replies ---------

import asyncio  # noqa: E402

import infer_api  # noqa: E402


class FakeRlm:
    """Scripted `rlm.map` (one reply per frame, in order); every request recorded. The clock advances a second per
    frame."""

    def __init__(self, script, clock):
        self.script = list(script)
        self.clock = clock
        self.requests = []

    async def map(self, tasks, items, *, context=None, contract=None, budget=None, model=None, thinking=None,
                  shared_prefix=False, concurrency=None, timeout_ms=None):
        self.requests.append({"task": tasks[0], "item": items[0], "context": context, "contract": contract,
                              "model": model, "thinking": thinking, "shared_prefix": shared_prefix})
        step = self.script.pop(0) if self.script else {"value": {"findings": [], "requests": [], "done": True}}
        self.clock.now += 1.0
        results = infer_api.MapResults() if hasattr(infer_api, "MapResults") else []
        if "value" in step:
            results.append(step["value"])
        elif "incomplete" in step:
            results.append(infer_api.Incomplete({"reason": step["incomplete"], "detail": "d"}))
        elif "error" in step:
            results.append(infer_api.FrameError({"error": step["error"]}))
        results.spent = {"calls": 1, "tokens": step.get("tokens", 100)}
        results.usage = {}
        return results


class VirtualClock:
    def __init__(self):
        self.now = 1000.0

    def __call__(self):
        return self.now

    async def sleep(self, seconds):
        self.now += seconds


def reply(findings=(), requests=(), done=False, checked=None):
    out = {"findings": list(findings), "requests": list(requests), "done": done}
    if checked is not None:
        out["checked"] = checked
    return {"value": out}


FINDING = {"file": "src/app.py", "line": 10, "severity": "high", "category": "bug", "claim": "T1 shows the comment lies.",
           "why": "w", "scenario": "s", "evidence": []}
INVESTIGATIONS = [
    {"name": "sent back, then done", "lens": "claims", "rounds": 4, "outside": True, "required": ["show", "--dry-run"],
     "shapes": [SHAPES[3], SHAPES[0]], "script": [
         reply(done=True),
         reply(requests=[{"references": {"symbol": "show"}}, {"grep": {"pattern": "dry-run"}}, {"recall": {"id": "r9.9"}},
                         {"read": {"path": "src/app.py", "start": 1, "end": 3}}]),
         reply(findings=[FINDING], requests=[{"recall": {"id": "r2.1"}}, {"recall": "bad"}, {"recall": {"id": None}}]),
         reply(findings=[FINDING], done=True, checked=["T1: not applicable here", 5, None])]},
    {"name": "rounds exhausted, with leads and tests", "lens": "tests", "rounds": 3, "outside": False, "required": [],
     "shapes": [], "leads": "Leads from the first pass: one.", "tests": {"limit": 2, "tracked": ["tests/test_app.py"]},
     "script": [
         reply(requests=[{"run_tests": {"paths": ["tests/test_app.py"]}}, {"definition": {"symbol": "load"}}] +
               [{"read": {"path": "src/helper.py"}}] * 8),
         reply(requests=[{"pickaxe": {"string": "OSError"}}, {"recall": {"id": "r1.2"}}]),
         reply(requests=[{"read": {"path": "src/app.py"}}])]},
    {"name": "incomplete", "lens": "siblings", "rounds": 4, "outside": True, "required": ["show"], "shapes": [SHAPES[0]],
     "script": [reply(requests=[{"list": {"dir": "src"}}]), {"incomplete": "budget"}]},
    {"name": "frame error", "lens": "deployment", "rounds": 2, "outside": True, "required": [], "shapes": [],
     "script": [{"error": "provider down"}]},
    {"name": "not an object", "lens": "inputs", "rounds": 2, "outside": False, "required": [], "shapes": [],
     "script": [{"value": "just text"}]},
    {"name": "done with requests on the last round", "lens": "claims", "rounds": 2, "outside": True, "required": ["KINDS"],
     "shapes": [], "shared": ["shared view one", "shared view two"], "model": "m1", "thinking": "low", "script": [
         reply(requests=[{"grep": {"pattern": "KIND"}}], done=True),
         reply(requests=[{"read": {"path": "src/app.py"}}], done=True)]},
    {"name": "one round", "lens": "claims", "rounds": 1, "outside": True, "required": ["x"], "shapes": [SHAPES[2]],
     "script": [reply(done=True)]},
    {"name": "done in round 1 after three lookups", "lens": "claims", "rounds": 3, "outside": True, "required": [],
     "shapes": [], "script": [reply(requests=[{"list": {"dir": "src"}}, {"grep": {"pattern": "load"}},
                                             {"read": {"path": "src/app.py"}}], done=True)]},
    {"name": "done in round 1 after two lookups", "lens": "claims", "rounds": 3, "outside": True, "required": [],
     "shapes": [], "script": [reply(requests=[{"list": {"dir": "src"}}, {"grep": {"pattern": "load"}}], done=True),
                              reply(done=True)]},
    {"name": "done at once, nothing owed", "lens": "claims", "rounds": 3, "outside": False, "required": [], "shapes": [],
     "script": [reply(done=True, checked=["all good"])]},
]


async def investigation(spec):
    clock = VirtualClock()
    rlm = FakeRlm(spec["script"], clock)
    frames = aa.Frames(rlm, cap=None, usage=aa._Usage(), clock=clock, sleep=clock.sleep, rng=lambda: 0.5, concurrency=1)
    leads = None
    if "leads" in spec:
        leads = asyncio.get_running_loop().create_future()
        leads.set_result(spec["leads"])
    session = session_of(spec.get("tests"))
    outcome = await deep.investigate(
        frames, spec["lens"], ["base view"], with_reference(repo_at("deep"), None), rounds=spec["rounds"],
        model=spec.get("model"), thinking=spec.get("thinking"), cutoff=None, clock=clock, tests=session,
        something_outside=spec["outside"], required=spec["required"], shapes=spec["shapes"], shared=spec.get("shared"),
        leads_view=leads)
    return {**spec, "outcome": outcome, "requests": rlm.requests, "records": session.records if session else None}


investigations = [asyncio.run(investigation(spec)) for spec in INVESTIGATIONS]

result = {"git_env": GIT_ENV, "repos": REPOS, "revs": {name: {"base": rev_of(name, "base"), "head": rev_of(name, "head")}
                                                       for name in REPOS},
          "reference": REFERENCE, "repo_cases": repo_cases, "serve": serve_cases, "rounds": rounds, "extracts": extracts,
          "briefs": briefs, "helpers": helpers, "diff_lines": DEEP_DIFF_LINES, "citations": citation_cases, "runs": RUNS,
          "normalized": normalized, "windows": windows, "merges": merges, "identical": identical, "shapes": SHAPES,
          "shape_cases": shape_cases, "mentions": mention_cases, "test_lines": test_lines, "compared": COMPARED,
          "regressions": regressions, "views": views, "summaries": summaries, "started": started_cases,
          "no_sandbox": {key: value for key, value in no_sandbox.items() if key != "session"},
          "prove_findings": PROVE_FINDINGS, "investigations": investigations, "proved": proved, "assurances": assurances, "dumps": dumps}
text = json.dumps(result, ensure_ascii=False, indent=1, default=sorted).replace(ROOT, "<root>")
shutil.rmtree(ROOT)
print(text)
