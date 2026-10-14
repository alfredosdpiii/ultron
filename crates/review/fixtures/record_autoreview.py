"""Records autoreview_api.py context, guides, line mapping, slices, merging and the public finding shape for
crates/review tests. Run by generate.mjs; prints autoreview.json. Guide trees are built in a temp dir from recorded
entries; temp paths are recorded as `<root>`.
"""
import json
import os
import shutil
import sys
import tempfile

sys.path.insert(0, sys.argv[1])
import autoreview_api as aa  # noqa: E402
import infer_api  # noqa: E402
import review_api as ra  # noqa: E402
import review_prompts as rp  # noqa: E402

ROOT = os.path.realpath(tempfile.mkdtemp(prefix="review-autoreview-"))


def reader(sources):
    return lambda path: sources.get(path)


def hunk_json(h):
    return {"old_start": h.old_start, "old_len": h.old_len, "new_start": h.new_start, "new_len": h.new_len,
            "header": h.header, "lines": [[l.kind, l.old, l.new, l.text] for l in h.lines]}


def chunk_json(c):
    return {"id": c.id, "path": c.path, "kind": c.kind, "status": c.status, "part": c.part, "parts": c.parts,
            "text": c.text, "hunks": [hunk_json(h) for h in c.hunks]}


# --- Context --------------------------------------------------------------------------------------------------

comment = lambda i, **kw: {"author": f"user{i}", "path": "a.py", "line": i, "body": f" body\n {i}  text ", **kw}  # noqa: E731
CONTEXTS = [
    [None, {}],
    [{}, {"AGENTS.md": []}],
    [{"title": "  Fix\r\nthe thing  ", "description": "d" * 2100, "ci": "lint: failed\r\n" + "x" * 900,
      "comments": [comment(1), comment(2, path=None), "junk", comment(3, path="", line=None, author=None),
                   comment(4, line="7", body=None, author="a  b" * 20)]},
     {"AGENTS.md": ["# Agents", "  rule one  "], "CLAUDE.md": [], "CONTRIBUTING.md": ["c" * 3000]}],
    [{"title": 5, "description": "", "ci": None, "comments": [comment(i) for i in range(33)]}, {}],
    [{"comments": "not a list", "title": "t"}, {"CLAUDE.md": ["x"]}],
]
contexts = [{"context": c, "files": f, "block": aa.context_block(c, reader(f))} for c, f in CONTEXTS]

# --- Guides ---------------------------------------------------------------------------------------------------

GUIDE_TREE = [
    ["guides/general.md", "  General guidance.\r\nLine two.\r\n"], ["guides/python-tips.md", "Python tips."],
    ["guides/widgets.md", "Repo-specific guide."], ["guides/acme-widgets.txt", "Full-name guide."],
    ["guides/django.mdx", "Django guide."], ["guides/notes.rst", "ignored"], ["guides/empty.md", "   \n"],
    ["guides/.hidden/secret.md", "hidden"], ["guides/sub/react.md", "React guide."],
    ["guides/sub/deeper/pytest.md", "Pytest guide."], ["guides/sub/deeper/deepest/too-deep.md", "too deep"],
    ["guides/UPPER.MD", "Upper case ext."], ["guides/.dotfile.md", "dot file"],
    ["single.md", "Single file guide " + "s" * 400], ["other/x.md", "Other dir."],
    ["long/a.md", "a" * 700], ["long/b.md", "b" * 300], ["long/c.md", "c" * 50],
    ["fakehome/guides/mine.md", "Home guide."],
]
GUIDE_LINKS = [["guides/linked", "other"]]
for path, text in GUIDE_TREE:
    full = os.path.join(ROOT, path)
    os.makedirs(os.path.dirname(full), exist_ok=True)
    with open(full, "w", encoding="utf-8", newline="") as handle:
        handle.write(text)
for path, target in GUIDE_LINKS:
    os.symlink(target, os.path.join(ROOT, path))
# Not "home": a recorded `<root>/home/<dir>/` path reads as a real home directory to scripts/secret-scan.mjs.
os.environ["HOME"] = os.path.join(ROOT, "fakehome")
guide_diff = ra.parse_diff(
    "diff --git a/app/views.py b/app/views.py\n--- a/app/views.py\n+++ b/app/views.py\n@@ -1 +1,2 @@\n x\n"
    "+from django.http import HttpResponse  # React? no\n"
    "diff --git a/web/App.TSX b/web/App.TSX\n--- a/web/App.TSX\n+++ b/web/App.TSX\n@@ -1 +1 @@\n-a\n+b\n")
GUIDE_CASES = [
    [["<root>/guides"], "acme/widgets", 12_000], [["<root>/guides/"], "acme/widgets", 12_000],
    [["<root>/guides", "<root>/single.md", "<root>/guides/general.md", "  ", 5, "<root>/missing"], "", 12_000],
    [["<root>/long"], "o/r", 1_000], [["<root>/long"], "o/r", 650], [["<root>/long"], "o/r", 150],
    [["~/guides", "~"], "x/y", 12_000], ["<root>/guides", "x", 12_000], [[], "x", 12_000],
]
guides = []
for paths, repo, limit in GUIDE_CASES:
    real = json.loads(json.dumps(paths).replace("<root>", ROOT))
    for files in (guide_diff, []):
        guides.append({"paths": paths, "repo": repo, "limit": limit, "with_files": bool(files),
                       "result": list(aa.load_guides(real, repo, files, limit))})

# --- Others' comments -----------------------------------------------------------------------------------------

FINDING = {"file": "a.py", "line": 10, "claim": "The loop drops the last element of items", "why": "range stops early"}
OTHERS = [
    {"author": "ann", "path": "a.py", "line": 12, "body": "Loop drops last element in items list"},
    {"author": "ann", "path": "a.py", "line": 9, "body": "the loop drops the last element"},
    {"author": "bob", "path": "a.py", "line": 16, "body": "Loop drops last element in items list"},
    {"author": "cy", "path": "b.py", "line": 10, "body": "Loop drops last element in items list"},
    {"author": "di", "path": "a.py", "line": "10", "body": "Loop drops last element in items list"},
    {"author": "ed", "path": "a.py", "line": 11, "body": "range stops early here, the loop is wrong"},
    {"author": "", "path": "a.py", "line": 10, "body": "Loop drops last element in items list"},
    {"author": None, "path": "a.py", "line": 10, "body": "Loop drops last element in items list"},
    {"author": "fy", "path": "a.py", "line": True, "body": "Loop drops last element in items list"},
    {"author": "gu", "path": "a.py", "line": 5, "body": 7},
    {"author": "hy " * 30, "path": "a.py", "line": 15, "body": "Loop drops last element in items list"},
]
others = [[window, aa.raised_by_others(FINDING, OTHERS, window)] for window in (5, 0, 9)]

# --- Line mapping ---------------------------------------------------------------------------------------------

MAP_DIFF = ("diff --git a/m.py b/m.py\n--- a/m.py\n+++ b/m.py\n@@ -3,4 +3,5 @@ def f():\n a\n-b\n-c\n+B\n+C\n+D\n d\n"
            "@@ -20,3 +21,1 @@\n-x\n-y\n z\n@@ -30,2 +29,2 @@\n p\n-q\n"
            "diff --git a/gone.py b/gone.py\ndeleted file mode 100644\n--- a/gone.py\n+++ /dev/null\n@@ -1 +0,0 @@\n-a\n"
            "diff --git a/tail.py b/tail.py\n--- a/tail.py\n+++ b/tail.py\n@@ -1,2 +1,1 @@\n keep\n-last\n"
            "diff --git a/img.png b/img.png\nBinary files a/img.png and b/img.png differ\n"
            "diff --git a/mode.sh b/mode.sh\nold mode 100644\nnew mode 100755\n")
map_files = ra.parse_diff(MAP_DIFF)
mapped = [[index, line, nearest, aa.map_line(item, line, nearest=nearest)]
          for index, item in enumerate(map_files) for line in range(0, 36) for nearest in (False, True)]
mapped += [[None, line, False, aa.map_line(None, line)] for line in (1, 7)]
ranges = aa.diff_line_ranges(map_files)
hunks_text = [[index, limit, aa._hunks_text(item, limit)] for index, item in enumerate(map_files)
              for limit in (4_000, 30)]
hunks_text.append([None, 4000, aa._hunks_text(None)])

KEYS = [None, "", [], "bugs", "BUGS, sec ,qa,bugs", ["ai", "Correctness"], "nope", [5], ["arch", None], ",", 0, "llm"]
keys = []
for value in KEYS:
    try:
        keys.append([value, {"ok": aa._reviewer_keys(value)}])
    except ra.ReviewError as error:
        keys.append([value, {"error": str(error)}])

# --- Slices ---------------------------------------------------------------------------------------------------

SLICE_DIFF = "".join(
    f"diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1,2 +1,2 @@{' ' + header if header else ''}\n x\n-y\n+{body}\n"
    + (f"@@ -40,1 +40,1 @@\n-p\n+q\n" if two else "")
    for path, header, body, two in [
        ("src/z.py", "def z():", "z = 1", False), ("src/a.py", "", "a = " + "1" * 300, True), ("docs/guide.md", "", "doc", False),
        ("tests/test_a.py", "", "assert a", False), ("src/m.py", "", "m", False), ("README.md", "", "readme", False),
        ("lib/b.py", "", "b", True),
    ])
slice_files = ra.parse_diff(SLICE_DIFF)
slice_chunks, _ = ra.build_chunks(slice_files, reader({}), max_chars=200)
changed = [item.path for item in slice_files]
# Every slice size from 50 to 1500: a chunk that exactly fills a slice stays in it.
pack_sweep = [[m, [len(g) for g in aa.pack_chunks(slice_chunks, changed, m)[1].values()]] for m in range(50, 1500)]
packs = []
for max_chars, single, names in ((ra.CHUNK_CHARS, False, changed), (600, False, changed), (600, True, changed),
                                 (100, False, changed[:1]), (ra.CHUNK_CHARS, False, [f"f{i}" for i in range(33)])):
    slices, members = aa.pack_chunks(slice_chunks, names, max_chars, single)
    packs.append({"max_chars": max_chars, "single": single, "changed": names, "slices": [chunk_json(s) for s in slices],
                  "members": [[slice_chunks.index(c) for c in members[s.id]] for s in slices]})

everyone = list(rp.REVIEWERS)
merges = []
for keys_, lines in ((everyone, 149), (everyone, 150), (["arch", "bugs"], 0), (["arch", "tests"], 10), (["bugs"], 10),
                     (["tests", "arch", "ai", "bugs"], 3)):
    out = aa.merged_reviewers([rp.REVIEWERS[k] for k in keys_], lines)
    merges.append({"keys": keys_, "lines": lines, "out": [{"key": r.key, "title": r.title, "focus": r.focus,
                                                         "checklist": list(r.checklist), "reads_docs": r.reads_docs,
                                                         "llm_only": r.llm_only} for r in out]})
merged_task = rp.autoreview_finder_task(aa.merged_reviewers([rp.REVIEWERS["bugs"], rp.REVIEWERS["arch"]], 1)[0])

big_slices, big_members = aa.pack_chunks(slice_chunks, changed, ra.CHUNK_CHARS)
RAWS = [{"file": "src/a.py", "line": 41}, {"file": "src/a.py", "line": 2}, {"file": "src/a.py"},
        {"file": "./src/a.py", "line": "40"}, {"file": "b/src/m.py"}, {"file": "a.py"}, {"file": "`src/z.py`", "line": 1},
        {"file": "/abs/src/a.py", "line": 3}, {"file": "nope.py"}, {"file": "", "line": 1}, {"line": 1}, "junk",
        {"file": "py", "line": 1}, {"file": "src", "line": 1}, {"file": "src/a.py", "line": None},
        {"file": "src/a.py", "line": 2.9}, {"file": "a/src/z.py"}]
members_out = []
for number, group in big_members.items():
    for raw in RAWS:
        found = aa.member_for(group, raw)
        members_out.append([number, raw, None if found is None else slice_chunks.index(found)])
single_group = [slice_chunks[0]]
for raw in RAWS[:3] + ["junk", {"file": "other.py"}]:
    found = aa.member_for(single_group, raw)
    members_out.append([0, raw, None if found is None else 0])

FAILURES = [infer_api.Incomplete({"reason": "budget"}), infer_api.Incomplete({}),
            infer_api.FrameError({"error": aa.DEADLINE_ERROR}), infer_api.FrameError({"error": "boom  \n" + "x" * 200}),
            infer_api.FrameError({}), {"value": 1}]
failures = [[f.status if isinstance(f, infer_api.Incomplete) else None,
             f.error if isinstance(f, infer_api.FrameError) else None, aa.describe_failure(f)] for f in FAILURES]

# --- Finding records ------------------------------------------------------------------------------------------

RULE_RAWS = [
    "junk", {}, {"consequence": "  it  already\nbroke  " + "c" * 500},
    {"unpinned": {"behaviour": "  returns\n total ", "change": "c" * 400,
                  "closest_test": {"path": " tests/t.py ", "line": "7"},
                  "mutation": {"path": " a.py ", "line": "3", "replacement": "x = 2" + "y" * 400}}},
    {"unpinned": {"closest_test": {"path": "t.py", "line": 0}, "mutation": {"path": "a.py", "line": None, "replacement": "x"}}},
    {"unpinned": {"closest_test": {"path": "t.py", "line": "bad"}, "mutation": {"path": "a.py", "line": 2, "replacement": "a\nb"}}},
    {"unpinned": {"closest_test": {"path": "t.py", "line": -5}, "mutation": {"path": 1, "line": 2, "replacement": "x"}}},
    {"unpinned": {"closest_test": {"path": 3}, "mutation": {"path": "a.py", "line": 2.7, "replacement": "ok"}}},
    {"unpinned": "not a dict", "consequence": None}, {"unpinned": {"behaviour": None, "change": 12}},
]
rules = []
for raw in RULE_RAWS:
    finding = {"file": "a.py"}
    aa.parse_rules(raw, finding)
    rules.append([raw, finding])

EXTRAS = [
    [{"severity": "critical", "scenario": "input [] returns 0", "line": 10, "end_line": 12, "replacement": "a\r\nb\n\n"},
     {"category": "correctness", "line": 10}],
    [{"severity": "high", "scenario": "", "line": 10, "end_line": 41}, {"category": "correctness", "line": 10}],
    [{"severity": "major", "scenario": "s", "line": "10", "end_line": 40, "replacement": "   "}, {"category": "tests", "line": 10}],
    [{"severity": "blocker", "scenario": "s", "line": 12, "end_line": 13, "replacement": "x"}, {"category": "correctness", "line": 10}],
    [{"severity": "high", "scenario": "s", "line": 10, "end_line": True, "replacement": "r" * 2001},
     {"category": "maintainability", "line": 10}],
    [{"severity": "nit", "line": None, "end_line": 11}, {"category": "docs", "line": 10}],
    [{"severity": "bogus", "scenario": " x ", "line": 10.0, "end_line": 10, "replacement": "keep\n",
      "unpinned": {"behaviour": "b"}}, {"category": "correctness", "line": 10}],
]
extras = []
for raw, finding in EXTRAS:
    out = dict(finding)
    aa._extras(raw, out)
    extras.append([raw, finding, out])


def af(file, line, level, category, claim, conf=0.5, **extra):
    out = {"file": file, "line": line, "level": level, "severity": aa.LEVEL_TO_OLD[level], "category": category,
           "claim": claim, "why": "w", "suggested_fix": "", "confidence": conf, "reviewers": ["bugs"]}
    out.update(extra)
    return out


RANKS = [{"level": "high", "confidence": 0.5}, {"severity": "blocker"}, {"severity": "x", "confidence": "0.25"},
         {"level": "", "severity": "nit", "confidence": 0}, {}, {"level": "medium", "confidence": None}]
ranks = [[f, list(aa.level_rank(f))] for f in RANKS]
PAIRS = [["The loop drops the last element of items", "the loop drops the last element of items list"],
         ["alpha beta gamma delta", "alpha beta gamma epsilon"], ["a b", "a b"],
         ["one two three four five", "one two three six seven"]]
identical = [[a, b, t, aa.nearly_identical(a, b, t)] for a, b in PAIRS for t in (0.8, 0.5, 0.9)]

DEDUPE = [
    [af("a.py", 10, "low", "correctness", "first claim here", 0.4, reviewers=["bugs"]),
     af("a.py", 13, "high", "correctness", "second claim here", 0.3, reviewers=["arch", "bugs"]),
     af("a.py", 16, "medium", "correctness", "third claim words", 0.9, reviewers=["ai"]),
     af("a.py", 30, "high", "security", "token leaks into logs", 0.8, reviewers=["security"]),
     af("b.py", 1, "low", "security", "token leaks into logs now", 0.95, reviewers=["bugs"], source="deep:claims"),
     af("b.py", 2, "nit", "tests", "x", 1, reviewers=["tests"]),
     af("a.py", 13, "critical", "correctness", "same line twin", 0.1, reviewers=["tests"])],
    [af("c.py", 5, "medium", "tests", "alpha", 0.5), af("c.py", 5, "medium", "tests", "beta", 0.5),
     af("c.py", 9, "medium", "tests", "gamma", 0.7)],
    # Exactly MERGE_WINDOW apart; equal confidences written 1.0 and 1 (the earlier one's is kept).
    [af("d.py", 20, "low", "tests", "one", 1.0), af("d.py", 25, "low", "tests", "two", 1),
     af("d.py", 31, "low", "tests", "three", 0.2)],
    [],
]
dedupes = []
for findings in DEDUPE:
    records = []
    out = aa.dedupe_fast([dict(f) for f in findings], records)
    dedupes.append({"input": findings, "out": out, "records": records})

ROOTS = [
    [af("a.py", 1, "high", "correctness", "Cache key ignores the user id field", 0.5, scenario="two users share one cache entry"),
     af("b.py", 9, "high", "correctness", "cache key ignores the user id field entirely", 0.9, test_run="t1", reviewers=["arch"]),
     af("c.py", 3, "medium", "correctness", "Cache key ignores the user id field", 0.5),
     af("d.py", 4, "high", "security", "Cache key ignores the user id field", 0.5),
     af("e.py", 5, "high", "correctness", "different words about the session", 0.5,
        scenario="two users share one cache entry", beyond_diff=True),
     af("f.py", 6, "high", "correctness", "session cache mixes users entries", 0.5,
        scenario="two users share one cache entry"),
     af("g.py", 7, "high", "correctness", "session cache mixes users entries again", 0.5,
        scenario="two users share one cache entry", host_confirmed=True)]
    + [af(f"z{i}.py", i, "low", "tests", "missing test for parse_rules branch", 0.5) for i in range(11)]
    # Claims alike (half), scenarios at 0.8 overlap: below the 0.9 the scenario rule needs.
    + [af("s1.py", 1, "medium", "ai", "cache key ignores user field badly", 0.5,
          scenario="alpha bravo charlie delta echo foxtrot golf hotel india juliet"),
       af("s2.py", 2, "medium", "ai", "cache key ignores tenant scope here", 0.5,
          scenario="alpha bravo charlie delta echo foxtrot golf hotel kilo lima")],
    [],
]
roots = []
for findings in ROOTS:
    records = []
    kept, merged = aa.group_root_causes([dict(f, reviewers=list(f["reviewers"])) for f in findings], records)
    roots.append({"input": findings, "kept": kept, "merged": merged, "records": records})

CHECKS = [{}, {"shape": "idtoken", "template": "t1", "detail": "  ran\n twice  ", "asked": {"answer": "yes", "quote": "q", "why": "w " * 200}},
          {"gate": "g", "asked": {"answer": ""}}, {"shape": 0, "template": None, "asked": "x"},
          {"asked": {"answer": True, "why": None}}]
checks = [[c, aa.check_view(c)] for c in CHECKS]

PUBLIC = [
    [af("a.py", 4, "high", "correctness", "c", 0.876), "confirmed"],
    [af("a.py", 4, "medium", "correctness", "c", 0.125, test_run="tests/t.py::x", citations=[
        {"path": "a.py", "line": 4, "quote": "x = 1"}, {"path": "b.py", "line": 9, "quote": "y"},
        {"path": "c.py", "line": 1, "quote": "z"}, {"path": "d.py", "line": 2, "quote": "w"}, {"path": "e.py", "line": 3, "quote": "v"}],
        beyond_diff=True, also_at=[{"file": "b.py", "line": 2}], end_line=9, replacement="r", suggested_fix="f",
        evidence="ev", finder_level="critical", verifier_level="medium", verifier_holds=True, shape="s", lesson="l",
        unclear=True, consequence="cons", id=3, verified_by="m/x", source="deep:claims"), "confirmed"],
    [af("a.py", 4, "low", "tests", "c", 1, unpinned={"behaviour": "b", "change": "c", "closest_test": {"path": "t", "line": 1},
        "mutation": {"path": "a.py", "line": 1, "replacement": "x"}}, proof="proven",
        citations=[{"path": "a.py", "line": 4, "quote": "q"}]), "confirmed"],
    [af("a.py", 4, "nit", "docs", "c", 0.5, verification="the views disagree", unpinned={"behaviour": "b"},
        end_line=3), "uncertain"],
    [{"file": "x.py", "line": 2, "severity": "major", "category": "c", "claim": "c", "why": "", "confidence": 0.005,
      "how_verified": "by hand", "citations": [], "verifier_level": "low", "verifier_holds": "unknown"}, "confirmed"],
    [{"file": "x.py", "line": 2, "severity": "nit", "category": "c", "claim": "c", "why": "", "confidence": "0.335"},
     "uncertain"],
]
public = [[f, v, aa._public(f, v)] for f, v in PUBLIC]

result = {"contexts": contexts, "guide_tree": GUIDE_TREE, "guide_links": GUIDE_LINKS, "guide_diff_files": 2,
          "guides": guides, "others_finding": FINDING, "others_comments": OTHERS, "others": others,
          "map_diff": MAP_DIFF, "mapped": mapped, "ranges": ranges, "hunks_text": hunks_text, "keys": keys,
          "slice_diff": SLICE_DIFF, "slice_chunks": [chunk_json(c) for c in slice_chunks], "packs": packs, "pack_sweep": pack_sweep,
          "merges": merges, "merged_task": merged_task, "members": members_out, "failures": failures,
          "rules": rules, "extras": extras, "ranks": ranks, "identical": identical, "dedupe_fast": dedupes,
          "roots": roots, "checks": checks, "public": public}
text = json.dumps(result, ensure_ascii=False, indent=1).replace(ROOT, "<root>")
shutil.rmtree(ROOT)
print(text)
