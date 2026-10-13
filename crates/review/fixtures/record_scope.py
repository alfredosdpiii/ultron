"""Records review_api.py chunking, find planning, scope resolution over local git repositories, related code,
the report and posting, for crates/review tests. Run by generate.mjs; prints scope.json.

Repositories are built in a temp dir from recorded steps (written files and git commands under a fixed identity and
clock), so the Rust test rebuilds the same commits. gh is answered by a scripted runner; git runs for real.
Temp paths are recorded as `<root>`.
"""
import json
import os
import shutil
import sys
import tempfile

sys.path.insert(0, sys.argv[1])
import review_api as ra  # noqa: E402
import review_prompts as rp  # noqa: E402

GIT_ENV = {
    "GIT_AUTHOR_NAME": "Ultron Fixture", "GIT_AUTHOR_EMAIL": "fixture@example.invalid",
    "GIT_COMMITTER_NAME": "Ultron Fixture", "GIT_COMMITTER_EMAIL": "fixture@example.invalid",
    "GIT_AUTHOR_DATE": "2026-01-01T00:00:00+0000", "GIT_COMMITTER_DATE": "2026-01-01T00:00:00+0000",
    "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": "/dev/null", "LC_ALL": "C", "TZ": "UTC",
}
os.environ.update(GIT_ENV)
os.environ.pop("GIT_DIR", None)
os.environ.pop("GIT_WORK_TREE", None)


def hunk_json(h):
    return {"old_start": h.old_start, "old_len": h.old_len, "new_start": h.new_start, "new_len": h.new_len,
            "header": h.header, "lines": [[l.kind, l.old, l.new, l.text] for l in h.lines]}


def file_json(f):
    return {"path": f.path, "old_path": f.old_path, "status": f.status, "binary": f.binary,
            "hunks": [hunk_json(h) for h in f.hunks]}


def chunk_json(c):
    return {"id": c.id, "path": c.path, "kind": c.kind, "status": c.status, "part": c.part, "parts": c.parts,
            "text": c.text, "hunks": [hunk_json(h) for h in c.hunks]}


def plan_json(plan):
    return {"frames": [[r.key, c.id] for r, c in plan["frames"]], "dropped": [[r.key, c.id] for r, c in plan["dropped"]],
            "not_applicable": plan["not_applicable"], "estimate": plan["estimate"]}


def reader(sources):
    return lambda path: sources.get(path)


# --- Chunks and find plans ------------------------------------------------------------------------------------

def body(n, start=1, word="line"):
    return [f"{word} {i}" for i in range(start, start + n)]


SRC = {"app.py": body(120), "renamed.py": body(30), "big.py": body(400, word="v" * 40)}
CHUNK_DIFFS = [
    # Two hunks with context between them; a rename; a deleted and a lock file skipped; a binary.
    ("diff --git a/app.py b/app.py\n--- a/app.py\n+++ b/app.py\n@@ -10,3 +10,4 @@ def main():\n line 10\n-old\n+line 11\n"
     "+line 12\n line 13\n@@ -40,2 +41,2 @@ class X:  \n-a\n+line 41\n line 42\n"
     "diff --git a/old.py b/renamed.py\nsimilarity index 90%\nrename from old.py\nrename to renamed.py\n"
     "--- a/old.py\n+++ b/renamed.py\n@@ -1,2 +1,2 @@\n-x\n+line 1\n line 2\n"
     "diff --git a/gone.py b/gone.py\ndeleted file mode 100644\n--- a/gone.py\n+++ /dev/null\n@@ -1 +0,0 @@\n-a\n"
     "diff --git a/yarn.lock b/yarn.lock\n--- a/yarn.lock\n+++ b/yarn.lock\n@@ -1 +1 @@\n-1\n+2\n"
     "diff --git a/missing.py b/missing.py\n--- a/missing.py\n+++ b/missing.py\n@@ -5,1 +5,1 @@\n-p\n+q\n"
     "diff --git a/README.md b/README.md\n--- a/README.md\n+++ b/README.md\n@@ -1 +1,2 @@\n # T\n+Use the OpenAI prompt.\n"
     "diff --git a/tests/test_app.py b/tests/test_app.py\nnew file mode 100644\n--- /dev/null\n+++ b/tests/test_app.py\n"
     "@@ -0,0 +1,2 @@\n+def test_x():\n+    assert messages.create(prompt)\n"),
    # One hunk far longer than a chunk: split into pieces, grouped into parts.
    "diff --git a/big.py b/big.py\n--- a/big.py\n+++ b/big.py\n@@ -1,300 +1,300 @@\n"
    + "".join(f"-old {i}\n+{'v' * 40} {i}\n" for i in range(1, 301)),
]
chunk_cases = []
for index, diff in enumerate(CHUNK_DIFFS):
    files = ra.parse_diff(diff)
    for max_chars, context in ((ra.CHUNK_CHARS, ra.CONTEXT_LINES), (900, 3), (3_000, 0)):
        chunks, skipped = ra.build_chunks(files, reader(SRC), max_chars=max_chars, context=context)
        chunk_cases.append({"diff": index, "max_chars": max_chars, "context": context,
                            "chunks": [chunk_json(c) for c in chunks], "skipped": skipped})
many = [ra.added_file(f"f{i:02}.py", "x = 1\n") for i in range(34)]
many_chunks, _ = ra.build_chunks(many[:2] + many, reader({}))
chunk_cases.append({"many": 34, "max_chars": ra.CHUNK_CHARS, "context": ra.CONTEXT_LINES,
                    "chunks": [chunk_json(c) for c in many_chunks[:3]], "skipped": []})

plan_files = ra.parse_diff(CHUNK_DIFFS[0])
plan_chunks, _ = ra.build_chunks(plan_files, reader(SRC))
everyone = list(rp.REVIEWERS.values())
plans = []
for keys, budget in ((None, 1_000_000), (None, 9_000), (["bugs", "ai"], 1_000_000), (["tests", "security"], 4_000),
                     (["ai"], 0)):
    chosen = everyone if keys is None else [rp.REVIEWERS[k] for k in keys]
    plan = ra.plan_find(plan_chunks, chosen, budget)
    options = ra.Options(budget_tokens=budget)
    plans.append({"reviewers": [r.key for r in chosen], "budget": budget, "plan": plan_json(plan),
                  "report": ra._plan_report("branch x", plan_chunks, plan, [("yarn.lock", "lockfile")], options)})
# Non-ASCII text: estimates count characters, not bytes.
wide_chunks, _ = ra.build_chunks([ra.added_file("wide.py", "café ünïcödé\n" * 200)], reader({}))
plans.append({"wide": True, "reviewers": ["bugs"], "budget": 1_000_000,
              "plan": plan_json(ra.plan_find(wide_chunks, [rp.REVIEWERS["bugs"]], 1_000_000)),
              "report": ra._plan_report("wide", wide_chunks, ra.plan_find(wide_chunks, [rp.REVIEWERS["bugs"]], 1_000_000),
                                        [], ra.Options(budget_tokens=1_000_000))})
# Every slice size between 100 and 600: where a group closes (a block that exactly fills a chunk stays in it).
sweep = [[m, [[c.part, c.parts, len(c.hunks)] for c in ra.build_chunks(ra.parse_diff(CHUNK_DIFFS[0]), reader(SRC),
                                                                         max_chars=m, context=2)[0]]]
         for m in range(100, 600)]
APPLIES_TEXT = ["calls OpenAI here", "the LLM", "llmish", "chat.completion", "chatcompletion", "completions.create(",
                "rlm.map(x)", "rlm.mapper", "nothing", "SYSTEM_PROMPT", "prompting", "max_tokens=3", "toolcall"]
applies = []
for text in APPLIES_TEXT:
    for kind in ("code", "doc"):
        chunk = ra.Chunk(1, "x", kind, "modified", [], text)
        applies.append([text, kind, {k: ra.applies(r, chunk) for k, r in rp.REVIEWERS.items()}])

# --- Verification views ---------------------------------------------------------------------------------------

VIEW_SRC = [
    "import os", "", "def total(items):", "    count = helper_fn(items)", "    for i in range(len(items) - 1):",
    "        value = compute_value(items[i]) + other_call(x)", "    return count", "",
    "export async function fetchThing(a) {", "  return await doFetch(a, Error(len(a)))", "}",
    "const handler = async (req) => process(req)", "let plain = 5", "class Widget:", "    def render(self): pass",
]
symbols = [[line, limit, ra.symbols_for(VIEW_SRC, line, limit)] for line in (1, 3, 4, 6, 7, 10, 12, 13, 15, 40, 0)
           for limit in (3, 1)]
symbols.append([5, 3, ra.symbols_for(None, 5)])
symbols.append([90, 3, ra.symbols_for(["def far():"] + ["x"] * 100, 90)])
symbols.append([80, 3, ra.symbols_for(["def far():"] + ["x"] * 100, 80)])
symbols.append([79, 3, ra.symbols_for(["def far():"] + ["x"] * 100, 79)])
windows = [[line, radius, ra.source_window(VIEW_SRC, line, radius)] for line in (1, 8, 15, 30) for radius in (15, 2)]
windows.append([3, 15, ra.source_window(None, 3)])
windows.append([2, 1, ra.source_window(["a", "w" * 500, "c"], 2, 1)])
hunk_files = ra.parse_diff(CHUNK_DIFFS[0])
hunk_for = [[line, (lambda h: hunk_json(h) if h else None)(ra.hunk_for(hunk_files[0], line))]
            for line in (1, 11, 12, 20, 26, 27, 41, 99)]
hunk_for.append([5, ra.hunk_for(None, 5)])
hunk_for.append([5, ra.hunk_for(ra.FileDiff("x"), 5)])


# --- Report ---------------------------------------------------------------------------------------------------

def fnd(file, line, severity, claim, conf=0.85, **extra):
    out = {"file": file, "line": line, "severity": severity, "category": "correctness", "claim": claim, "why": "",
           "suggested_fix": "", "confidence": conf, "reviewers": ["bugs"], "chunk": 1}
    out.update(extra)
    return out


REPORT_SRC = {"app.py": ["def f():", "    return   1   \t", "x" * 300], "b.py": ["only"]}
confirmed = [
    fnd("app.py", 2, "blocker", "Returns one.", 0.25, reviewers=["bugs", "arch"], evidence="`return 1`", why="w",
        suggested_fix="fix"),
    fnd("app.py", 3, "major", "Long line.", 0.75, why="because"),
    fnd("app.py", 9, "minor", "Out of range.", 0.05),
    fnd("nofile.py", 1, "nit", "No source.", 0.35, suggested_fix="s"),
] + [fnd("b.py", i, "minor", f"claim {i}", 0.5) for i in range(1, 30)]
uncertain = [fnd("b.py", 1, "major", "u0", verification="not verified (budget)"), fnd("b.py", 2, "nit", "u1")] + \
            [fnd("c.py", i, "minor", f"u{i}") for i in range(3, 33)]
stats_full = {"raised": 40, "merged": 35, "tokens": 1234567, "frames": 12, "find_frames": 9, "verify_frames": 3,
              "budget": 300000, "subagents": 2}
files = ra.parse_diff(CHUNK_DIFFS[0])
reports = []
for name, c, u, r, stats, not_checked, path, post_line in (
    ("full", confirmed, uncertain, [fnd("x", 1, "nit", "r")], stats_full, ["gap one", "gap two"], "/r/review.md",
     "Post pending: nothing has been posted to PR #4."),
    ("empty", [], [], [], {}, [], None, None),
    ("few", confirmed[:2], uncertain[:1], [], {"budget": 50000, "tokens": 999, "subagents": 1}, [], None, None),
):
    reports.append({"name": name, "confirmed": c, "uncertain": u, "rejected": r, "stats": stats,
                    "not_checked": not_checked, "path": path, "post_line": post_line,
                    "reviewers": ["bugs", "ai"] if name != "empty" else [],
                    "report": ra.render_report(label="branch x", files=files if name != "empty" else [],
                                               reviewers=[rp.REVIEWERS["bugs"], rp.REVIEWERS["ai"]] if name != "empty" else [],
                                               confirmed=c, uncertain=u, rejected=r, stats=stats,
                                               not_checked=not_checked, read_file=reader(REPORT_SRC), path=path,
                                               post_line=post_line)})

# --- Scope over local repositories ----------------------------------------------------------------------------

ROOT = os.path.realpath(tempfile.mkdtemp(prefix="review-scope-"))


def run_steps(base, steps):
    for step in steps:
        if step[0] == "write":
            path = os.path.join(base, step[1])
            os.makedirs(os.path.dirname(path), exist_ok=True)
            with open(path, "wb") as handle:
                handle.write(step[2].encode("utf-8") if isinstance(step[2], str) else bytes(step[2]))
        elif step[0] == "fill":
            with open(os.path.join(base, step[1]), "w", encoding="utf-8") as handle:
                handle.write(step[2] * step[3])
        elif step[0] == "mkdir":
            os.makedirs(os.path.join(base, step[1]), exist_ok=True)
        else:
            code, out, err = ra._run_process(["git", *step[1:]], base, 30)
            if code != 0:
                raise SystemExit(f"step {step} failed: {err}{out}")


def write(path, text):
    return ["write", path, text]


def git(*args):
    return ["git", *args]


LIB = "".join(f"def fn_{i}(x):\n    return helper_{i}(x) + {i}\n\n" for i in range(12))
BASE_STEPS = [
    git("init", "-q", "-b", "main"), write("lib.py", LIB), write("pkg/util.py", "def helper_3(x):\n    return x\n"),
    write("README.md", "# Repo\n"), write("yarn.lock", "a\n"), git("add", "-A"), git("commit", "-q", "-m", "base"),
    git("checkout", "-q", "-b", "feature"),
    write("lib.py", LIB.replace("+ 3\n", "+ 33\n").replace("+ 9\n", "+ compute_value(x)\n")),
    write("pkg/new.py", "from util import helper_3\n\nvalue = helper_3(1)\n"), git("add", "-A"),
    git("commit", "-q", "-m", "feature"),
    write("pkg/util.py", "def helper_3(x):\n    return x * 2\n"), write("yarn.lock", "b\n"),
    write("notes/todo.txt", "remember\n"), write("pkg/blob.bin", [0, 1, 2, 3]), ["fill", "big.txt", "y", 200_001],
]
REPOS = {
    "feature": BASE_STEPS,
    "main_dirty": [git("init", "-q", "-b", "main"), write("a.py", "x = 1\n"), git("add", "-A"), git("commit", "-q", "-m", "a"),
                   write("a.py", "x = 2\n"), write("b.py", "y = 1\n")],
    "origin_head": BASE_STEPS[:7] + [git("update-ref", "refs/remotes/origin/trunk", "HEAD"),
                                     git("symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/trunk"),
                                     write("a.py", "x = 1\n"), git("add", "-A"), git("commit", "-q", "-m", "a")],
    "master": [git("init", "-q", "-b", "master"), write("a.py", "x = 1\n"), git("add", "-A"), git("commit", "-q", "-m", "a"),
               git("checkout", "-q", "--detach"), write("a.py", "x = 3\n")],
    "unborn": [git("init", "-q", "-b", "main"), write("a.py", "x = 1\n"), write("s/b.py", "b\n"), git("add", "a.py")],
    "many_untracked": [git("init", "-q", "-b", "main"), write("k.py", "k\n"), git("add", "-A"), git("commit", "-q", "-m", "k")]
    + [write(f"u/f{i:02}.py", f"v = {i}\n") for i in range(43)],
    "plain": [write("a.py", "x\n")],
    # related_code: hits 15 and 16 lines from the cited line, and more hits than RELATED_CHARS holds.
    "related": [git("init", "-q", "-b", "main"),
                write("core.py", "def target_fn(x):\n" + "    pass\n" * 2 + "    target_fn(2)\n    target_fn(1)\n"
                      + "    pass\n" * 14 + "    return helper_long(x) + other_long(x)\n")]
    + [write(f"z{i}.py", f"v{i} = helper_long(1) + other_long(2) + target_fn(3)  # " + "c" * 220 + "\n") for i in range(9)]
    + [git("add", "-A"), git("commit", "-q", "-m", "r")],
}
built = {}
for name, steps in REPOS.items():
    base = os.path.join(ROOT, name)
    os.makedirs(base)
    run_steps(base, steps)
    built[name] = base


def rev(name, ref):
    return ra.Git(built[name]).value("rev-parse", ref)


PR_DIFF = ("diff --git a/lib.py b/lib.py\n--- a/lib.py\n+++ b/lib.py\n@@ -10,3 +10,3 @@ def fn_3(x):\n"
           " def fn_3(x):\n-    return helper_3(x) + 3\n+    return helper_3(x) + 33\n \n"
           "diff --git a/other/z.py b/other/z.py\nnew file mode 100644\n--- /dev/null\n+++ b/other/z.py\n@@ -0,0 +1 @@\n+z = 1\n")


def gh_meta(**meta):
    return json.dumps(meta)


SCOPE_CASES = [
    {"repo": "feature", "args": ""},
    {"repo": "feature", "args": "pkg", "cwd": ""},
    {"repo": "feature", "args": "util.py .", "cwd": "pkg"},
    {"repo": "feature", "args": "..", "cwd": "pkg"},
    {"repo": "feature", "args": "main"},
    {"repo": "feature", "args": "main pkg/util.py"},
    {"repo": "feature", "args": "nothing-here"},
    {"repo": "main_dirty", "args": ""},
    {"repo": "origin_head", "args": ""},
    {"repo": "master", "args": ""},
    {"repo": "unborn", "args": ""},
    {"repo": "unborn", "args": "main"},
    {"repo": "many_untracked", "args": ""},
    {"repo": "plain", "args": ""},
    {"repo": "related", "args": ""},
    {"repo": "feature", "args": "12", "gh": {"pr diff": [0, PR_DIFF, ""],
                                             "pr view": [0, gh_meta(number=12, title="Bump", url="https://g/o/r/pull/12",
                                                                    headRefOid="<HEAD>", baseRefName="main"), ""]}},
    {"repo": "feature", "args": "#12 lib.py", "gh": {"pr diff": [0, PR_DIFF, ""],
                                                     "pr view": [0, gh_meta(title="", headRefOid="<main>"), ""]}},
    {"repo": "feature", "args": "12", "gh": {"pr diff": [0, PR_DIFF, ""], "pr view": [1, "", "no"]}},
    {"repo": "feature", "args": "12", "gh": {"pr diff": [0, PR_DIFF, ""], "pr view": [0, "not json", ""]}},
    {"repo": "feature", "args": "12", "gh": {"pr diff": [0, PR_DIFF, ""],
                                             "pr view": [0, gh_meta(headRefOid="0" * 40, title="T"), ""]}},
    {"repo": "feature", "args": "12", "gh": {"pr diff": [1, "", "  HTTP 404: Not Found  " + "e" * 400]}},
    {"repo": "feature", "args": "12", "gh": None},
]


class Recorder:
    def __init__(self, gh):
        self.calls = []
        self.gh = gh

    def __call__(self, argv, cwd, timeout):
        self.calls.append([argv, cwd, timeout])
        if argv[0] == "gh":
            code, out, err = self.gh.get(" ".join(argv[1:3]), [1, "", "unscripted"])
            return code, out, err
        return ra._run_process(argv, cwd, timeout)


def substitute(value, name):
    if isinstance(value, str):
        value = value.replace("<HEAD>", rev(name, "HEAD") or "").replace("<main>", rev(name, "main") or "")
        return value
    if isinstance(value, list):
        return [substitute(v, name) for v in value]
    if isinstance(value, dict):
        return {k: substitute(v, name) for k, v in value.items()}
    return value


READS = ["lib.py", "pkg/util.py", "pkg/new.py", "../escape.py", "../plain/a.py", "/etc/hostname", "missing.py", "a.py", "pkg", "other/z.py"]
scopes = []
for case in SCOPE_CASES:
    name = case["repo"]
    gh = substitute(case.get("gh"), name)
    recorder = Recorder(gh or {})
    cwd = os.path.join(built[name], case.get("cwd", ""))
    which = (lambda tool: f"/usr/bin/{tool}") if gh is not None else (lambda tool: None if tool == "gh" else tool)
    options = ra.parse_args(case["args"], {})
    entry = {"repo": name, "args": case["args"], "cwd": case.get("cwd", ""), "gh": gh}
    try:
        scope = ra.resolve_scope(options, cwd, runner=recorder, which=which)
    except ra.ReviewError as error:
        entry.update(error=str(error), calls=recorder.calls)
        scopes.append(entry)
        continue
    entry["scope"] = {"label": scope.label, "files": [file_json(f) for f in scope.files], "root": scope.root,
                      "grep_rev": scope.grep_rev, "pr": scope.pr, "pr_url": scope.pr_url, "notes": scope.notes}
    entry["reads"] = {path: scope.read_file(path) for path in READS}
    chunks, skipped = ra.build_chunks(scope.files, scope.read_file)
    entry["chunks"] = [chunk_json(c) for c in chunks]
    entry["skipped"] = skipped
    git_ = ra.Git(scope.root, recorder)
    related = []
    for finding in ({"file": "lib.py", "line": 11}, {"file": "lib.py", "line": 29}, {"file": "pkg/new.py", "line": 3},
                    {"file": "a.py", "line": 1}, {"file": "core.py", "line": 20}):
        related.append([finding, ra.related_code(git_, scope, finding, scope.read_file(finding["file"]))])
    entry["related"] = related
    entry["calls"] = recorder.calls
    scopes.append(entry)

# --- Save and post --------------------------------------------------------------------------------------------

ra.time.strftime = lambda fmt: "20261011-120000"
tempfile.tempdir = os.path.join(ROOT, "tmp")  # the fallback directory, kept inside the temp root
saved = {"git": ra._save("report text\n", ra.Git(built["feature"])),
         "plain": ra._save("x", ra.Git(built["plain"]))}
saved_text = open(saved["git"], encoding="utf-8").read()
report_path = os.path.join(ROOT, "saved.md")
with open(report_path, "w", encoding="utf-8") as handle:
    handle.write("body\n")
review = ra.Review("r", report_path, pr=7, post_pending=True)
POSTS = [
    {"review": "review", "confirm": False}, {"review": "review", "confirm": True, "gh": [0, "https://x/7#c\n", ""]},
    {"review": "review", "confirm": True, "gh": [0, "  ", ""], "pr": 9},
    {"review": "path", "confirm": True, "gh": [0, "", ""]}, {"review": "path", "confirm": True, "gh": [0, "", ""], "pr": 3},
    {"review": "missing", "confirm": True, "gh": [0, "", ""], "pr": 3},
    {"review": "review", "confirm": True, "gh": None},
    {"review": "review", "confirm": True, "gh": [1, "out", "  denied  "]},
    {"review": "nopr", "confirm": True, "gh": [0, "", ""]},
]
import asyncio  # noqa: E402

posts = []
for case in POSTS:
    gh = case.get("gh")
    recorder = Recorder({"pr comment": gh} if gh else {})
    which = (lambda tool: f"/usr/bin/{tool}") if gh is not None else (lambda tool: None)
    target = {"review": ra.Review("r", report_path, pr=7, post_pending=True), "path": report_path,
              "missing": os.path.join(ROOT, "missing.md"), "nopr": ra.Review("r", report_path)}[case["review"]]
    try:
        out = {"ok": asyncio.run(ra.post(target, confirm=case["confirm"], pr=case.get("pr"), runner=recorder,
                                         which=which, cwd=ROOT))}
        if isinstance(target, ra.Review):
            out["post_pending"] = target.post_pending
    except ra.ReviewError as error:
        out = {"error": str(error)}
    posts.append({**case, **out, "calls": recorder.calls})

result = {"git_env": GIT_ENV, "repos": REPOS, "src": SRC, "chunk_diffs": CHUNK_DIFFS, "chunks": chunk_cases,
          "plans": plans, "sweep": sweep, "applies": applies, "view_src": VIEW_SRC, "symbols": symbols, "windows": windows,
          "hunk_for": hunk_for, "report_src": REPORT_SRC, "reports": reports, "scopes": scopes,
          "saved": saved, "saved_text": saved_text, "posts": posts}
text = json.dumps(result, ensure_ascii=False, indent=1).replace(ROOT, "<root>")
shutil.rmtree(ROOT)
print(text)
