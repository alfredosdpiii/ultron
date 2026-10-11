"""Records review_prompts.py / review_api.py outputs for crates/review tests. Run by generate.mjs."""
import json
import sys

sys.path.insert(0, sys.argv[1])
import review_api as ra  # noqa: E402
import review_prompts as rp  # noqa: E402


def outcome(run):
    try:
        return {"ok": run()}
    except ra.ReviewError as error:
        return {"error": str(error)}


def options(o):
    return {"targets": o.targets, "explicit_paths": o.explicit_paths, "only": o.only, "budget_tokens": o.budget_tokens,
            "model": o.model, "post": o.post, "deep": o.deep, "plan": o.plan, "help": o.help}


def file_diff(f):
    return {"path": f.path, "old_path": f.old_path, "status": f.status, "binary": f.binary,
            "added": f.added, "removed": f.removed, "skip": ra.skip_reason(f), "kind": ra.file_kind(f.path),
            "hunks": [{"old_start": h.old_start, "old_len": h.old_len, "new_start": h.new_start, "new_len": h.new_len,
                       "header": h.header, "new_first": h.new_first, "new_last": h.new_last,
                       "lines": [[l.kind, l.old, l.new, l.text] for l in h.lines], "rendered": ra.render_hunk(h)}
                      for h in f.hunks]}


prompts = {
    "finder": {k: rp.finder_task(r) for k, r in rp.REVIEWERS.items()},
    "autoreview_finder": {k: rp.autoreview_finder_task(r) for k, r in rp.REVIEWERS.items()},
    "deep": {f"{lens}/{tests}": rp.deep_task(lens, tests) for lens in rp.DEEP_LENSES for tests in (False, True)},
    "cell_planner": {str(t): rp.cell_planner_task(t) for t in (False, True)},
    "compiled_planner": {str(t): rp.compiled_planner_task(t) for t in (False, True)},
    "catalogue": rp.catalogue_text(),
}

ARGS = [
    ["", {}], ["main", {}], ["--only bugs,SEC,qa,bugs --budget 1.5m", {}], ["--budget=200k --post --plan", {}],
    ["--budget 9k", {}], ["--budget lots", {}], ["--budget 1_000,0", {}], ["--only ,", {}], ["--only nope", {}],
    ["--model openai/gpt-5 #12", {}], ["--model gpt5", {}], ["--model", {}], ["--deep", {}], ["--deep=4", {}],
    ["--deep=11", {}], ["--deep=0", {}], ["--dry-run -h", {}], ["--frob", {}], ["- -- --post a b", {}],
    ['"a b" \'c d\' e\\ f "x\\"y" "p\\$q" a#b #c', {}], ['"open', {}], ["trail\\", {}],
    ["--budget=", {}], ["", {"ULTRON_REVIEW_BUDGET": " 50k ", "ULTRON_REVIEW_ONLY": "ai", "ULTRON_REVIEW_MODEL": " a/b "}],
    ["--model c/d/e", {"ULTRON_REVIEW_MODEL": "a/b"}], ["", {"ULTRON_REVIEW_BUDGET": "  "}],
    ["", {"ULTRON_REVIEW_ONLY": "x"}], ["--only=llm,prompt --deep=10 'it''s'", {}], ["--help=yes x", {}],
]
args = [{"text": t, "env": e, "result": outcome(lambda: options(ra.parse_args(t, e)))} for t, e in ARGS]

TARGETS = [
    [["12"], [], []], [["#12", "https://github.com/o/r/pull/12/files"], [], []], [["12", "13"], [], []],
    [["12"], ["12"], []], [["src", "main"], ["src"], ["main"]], [["main", "dev"], [], ["main", "dev"]],
    [["7", "main"], [], ["main"]], [["nothing"], [], []], [["http://x/o/r/pull/9?x"], [], []],
]
targets = []
for tokens, exist, refs in TARGETS:
    o = ra.Options(targets=tokens, explicit_paths=["given"])
    def run(o=o, exist=exist, refs=refs):
        t = ra.classify_targets(o, exists=lambda p: p in exist, is_ref=lambda r: r in refs)
        return {"pr": t.pr, "base": t.base, "paths": t.paths}
    targets.append({"tokens": tokens, "exists": exist, "refs": refs, "result": outcome(run)})

DIFFS = [
    "diff --git a/src/app.py b/src/app.py\nindex 1..2 100644\n--- a/src/app.py\n+++ b/src/app.py\n"
    "@@ -1,4 +1,5 @@ def main():\n import os\n-x = 1\n+x = 2\n+y = 3\n \n z\n\\ No newline at end of file\n"
    "@@ -40 +41,2 @@\n-old\n+new\n+" + "w" * 450 + "\n",
    "diff --git a/old name.txt b/new name.txt\nsimilarity index 90%\nrename from old name.txt\nrename to new name.txt\n"
    "--- a/old name.txt\n+++ b/new name.txt\n@@ -3,2 +3,2 @@\n keep\n-a\n+b\n",
    'diff --git "a/q\\"uote\\303\\251.md" "b/q\\"uote\\303\\251.md"\nnew file mode 100644\n--- /dev/null\n'
    '+++ "b/q\\"uote\\303\\251.md"\n@@ -0,0 +1,2 @@\n+# Title\n+café   text\n',
    "diff --git a/img.PNG b/img.PNG\nBinary files a/img.PNG and b/img.PNG differ\n"
    "diff --git a/gone.rs b/gone.rs\ndeleted file mode 100644\n--- a/gone.rs\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-a\n-b\n"
    "diff --git a/package-lock.json b/package-lock.json\n--- a/package-lock.json\n+++ b/package-lock.json\n@@ -1 +1 @@\n-1\n+2\n"
    "diff --git a/mode.sh b/mode.sh\nold mode 100644\nnew mode 100755\n",
    "preamble\ndiff --git a/t/x_test.go b/t/x_test.go\n--- a/t/x_test.go\n+++ b/t/x_test.go\n@@ -1,3 +1,3 @@\n a\r\n-b\n"
    "garbage line\n+c\n@@ -10,1 +10,1 @@\n-p\x0cq\n+r\x1ds\n",
    "diff --git a/a.txt b/b.txt\n--- a/a.txt\t2020-01-01\n+++ b/b.txt\t2020-01-02\n@@ -1 +1 @@\n-x\n+y\n"
    "diff --git weird line without prefixes b/z.c\n+++ b/z.c\n@@ -5,0 +6 @@\n+int z;\n",
]
diffs = [{"text": d, "files": [file_diff(f) for f in ra.parse_diff(d)]} for d in DIFFS]
added = [{"path": p, "text": t, "file": file_diff(ra.added_file(p, t))} for p, t in
         [("new.py", "a\nb\n\nc"), ("empty.txt", ""), ("dist/out.min.js", "x\r\ny")]]

KINDS = ["README.md", "docs/a.MDX", "notes.txt", "tests/a.py", "src/__tests__/x.ts", "a.test.ts", "a_spec.rb",
         "test_x.py", "pkg/test_x.py", "spec/a.rb", "src/main.rs", "contest/a.rs", "latest.py", "x.Test.TS"]
kinds = {p: ra.file_kind(p) for p in KINDS}

CATS = [["Security issue", None], ["", "bugs"], ["", None], ["Prompt injection", "bugs"], ["weird", "tests"],
        ["weird", None], ["  CORRECTNESS ", None], ["flaky test", "ai"], ["maintainability", None], ["auth", "x"],
        ["Off-by-one", None], ["style", "arch"]]
categories = [[c, r, ra.normalize_category(c, r)] for c, r in CATS]

CLAIMS = [
    ["The loop drops the last element of items", "Loop drops last element in items list"],
    ["a b c", "a b c"], ["null pointer when config missing entirely", "config missing causes crash on startup"],
    ["The the and for with", "the and for with that"], ["Value overflow in counter_total", "counter_total overflows value"],
    ["foo bar baz", "foo bar baz qux"],
]
claims = [[a, b, ra.similar_claims(a, b)] for a, b in CLAIMS]

chunk_files = ra.parse_diff(DIFFS[0])
chunk = ra.Chunk(7, chunk_files[0].path, "code", "modified", chunk_files[0].hunks, "")
RAW = [
    None, "x", [], [{"claim": "  a\n claim  ", "line": 3, "severity": "MAJOR", "confidence": 0.9, "category": "Race",
                     "why": "w" * 700, "suggested_fix": None, "file": "other.py"}],
    [{"claim": ""}, 5, {"claim": "c" * 400, "line": "42", "severity": "blocker", "confidence": "0.2"},
     {"claim": "x", "line": 100, "confidence": 3}, {"claim": "y", "line": -1, "confidence": -2},
     {"claim": "z", "line": 2.9, "severity": "nit", "confidence": "bad", "category": "docs"},
     {"claim": "t", "line": "bad", "severity": None}, {"claim": "u", "line": True, "category": 5},
     {"claim": 12, "line": None}, {"claim": "after eight"}],
    [{"claim": "n", "confidence": "nan"}, {"claim": "i", "confidence": " -inf ", "line": " 1_2 "},
     {"claim": "f", "confidence": None, "line": "+3", "category": [1, {"a": None}]}, {"claim": 1.5e20, "line": 0.5},
     {"claim": True, "severity": " Nit ", "why": "a\x1cb\u2028c  d", "suggested_fix": 0.1}, {"claim": 2.0, "line": 4}],
]
normalized = [{"raw": r, "reviewer": k, "line_count": n, "chunk": {"id": 7, "diff": 0, "file": 0},
               "result": ra.normalize_findings(r, rp.REVIEWERS[k], chunk, n)}
              for r in RAW for k, n in (("bugs", None), ("tests", 60))]


def f(file, line, sev, conf, cat, claim, who):
    return {"file": file, "line": line, "severity": sev, "category": cat, "claim": claim, "why": "", "suggested_fix": "",
            "confidence": conf, "reviewers": [who], "chunk": 1}


DEDUPE = [
    [f("a.py", 10, "minor", 0.5, "correctness", "x", "bugs"), f("a.py", 12, "major", 0.4, "correctness", "y", "arch"),
     f("a.py", 13, "nit", 0.9, "correctness", "z", "bugs"), f("a.py", 30, "blocker", 0.1, "security", "s", "security"),
     f("b.py", 1, "minor", 0.8, "tests", "loop drops the last element always", "tests"),
     f("b.py", 4, "minor", 0.6, "correctness", "the loop drops last element", "bugs"),
     f("b.py", 4, "minor", 0.95, "ai", "unrelated words entirely here", "ai")],
    [f("c.py", 5, "major", 0.5, "tests", "same", "tests"), f("c.py", 5, "major", 0.5, "tests", "same2", "bugs"),
     f("c.py", 9, "major", 0.7, "tests", "same3", "arch"), f("c.py", 2, "major", 0.5, "tests", "same4", "tests")],
    [f("d.py", 5, "minor", 0.3, "correctness", "weak", "bugs"), f("d.py", 5, "major", 0.9, "correctness", "strong", "arch")],
    [],
]
dedupes = [{"input": d, "result": ra.dedupe([dict(x) for x in d])} for d in DEDUPE]

SOURCE = ">  12 | if items[i] is None:\n   13 |     return  total\n   14 | x\n   41 + new value here\n"
EVIDENCE = [
    ["The guard `if items[i] is None:` returns early", SOURCE], ["line 13 does return total", SOURCE],
    ["nothing relevant", SOURCE], ['quote "new value here"', SOURCE], ["", SOURCE], ["x", ""],
    ["mentions 'return total' only", SOURCE], ["  if   items[i] is None: tail", SOURCE],
]
evidence = [[e, s, ra.evidence_quotes_source(e, s)] for e, s in EVIDENCE]

JSONS = ['{"verdict": "confirmed", "evidence": "x"}', 'text {"a": 1} then {"verdict": "rejected"} end {broken',
         '{"verdict": 1}{"verdict": 2}', 'no json', '{"outer": {"verdict": "inner"}}', '```json\n{"verdict":"x","n":[1,2]}\n```',
         '{"a": 1}', '{"verdict": "a", "verdict": "b"}']
jsons = [[t, ra.parse_json_object(t)] for t in JSONS]

base = f("a.py", 10, "minor", 0.5, "correctness", "c", "bugs")
VERDICTS = [
    [{"verdict": "confirmed", "evidence": "`if items[i] is None:` decides", "corrected_line": 13}, SOURCE, 20],
    [{"verdict": "confirmed", "evidence": "trust me", "corrected_line": 99}, SOURCE, 20],
    [{"verdict": "rejected", "evidence": "no", "corrected_line": True}, SOURCE, None],
    [{"verdict": "uncertain", "corrected_line": 0}, SOURCE, None], [{"verdict": "uncertain", "evidence": "  hm  "}, SOURCE, 5],
    [None, SOURCE, 5], ["text", SOURCE, 5], [{"verdict": "confirmed", "corrected_line": 7}, SOURCE, None],
]
c, u, r = ra.apply_verdicts([dict(base) for _ in VERDICTS], [v for v, _, _ in VERDICTS], [s for _, s, _ in VERDICTS],
                            [n for _, _, n in VERDICTS])
verdicts = {"finding": base, "cases": VERDICTS, "confirmed": c, "uncertain": u, "rejected": r}

import autoreview_api as aa  # noqa: E402
import infer_api  # noqa: E402

LEVEL_VALUES = [None, "", " MAJOR ", "Critical", "medium", "blocker", "minor", "nit", "bogus", True, 3, 2.5, ["high"]]
to_levels = [[v, aa.to_level(v), aa.to_level(v, None)] for v in LEVEL_VALUES]
HOLDS = [True, False, None, "yes", " No ", "TRUE", "false", "unknown", 1, 0, "maybe"]
holds = [[v, aa._holds(v)] for v in HOLDS]
caps = [[lvl, cat, sc, h, aa.capped_level(lvl, cat, sc, h)]
        for lvl in ("critical", "high", "medium", "low")
        for cat in ("correctness", "tests", "maintainability")
        for sc in ("", "  ", "input x fails")
        for h in (None, True, False)]
KIND_FINDINGS = [{}, {"source": "fast"}, {"source": "deep:structure", "shape": "idtoken"}, {"source": "deep:structure"},
                 {"source": "compiled:s3"}, {"source": "hybrid"}, {"source": "deep:claims"}, {"source": ""}, {"source": 7}]
kinds_of = [[f, aa.finding_kind(f)] for f in KIND_FINDINGS]

GENERIC = [
    {"category": "tests", "file": "a.py"},
    {"category": "tests", "file": "a.py", "unpinned": {"behaviour": "short", "change": "long enough change"}},
    {"category": "tests", "file": "a.py", "unpinned": {"behaviour": "returns total", "change": "drop the last"}},
    {"category": "tests", "file": "ci.YML", "unpinned": {"behaviour": "returns total", "change": "drop the last"}},
    {"category": "tests", "file": "a.py", "unpinned": {"behaviour": "returns total", "change": "drop the last",
                                                       "mutation": {"path": "deploy/Dockerfile"}}},
    {"category": "tests", "file": "src/.env.local", "unpinned": {"behaviour": "returns total", "change": "drop the last"}},
    {"category": "tests", "file": "x/Makefile.py", "unpinned": {"behaviour": "returns total", "change": "drop the last",
                                                               "mutation": None}},
    {"category": "maintainability", "level": "low", "consequence": "duplicated"},
    {"category": "maintainability", "level": "low", "consequence": "the copy in src/a.py:12 already drifted"},
    {"category": "maintainability", "level": "low", "consequence": "it already diverged at LINE 40 here"},
    {"category": "maintainability", "level": "nit", "consequence": ""},
    {"category": "maintainability", "consequence": "no place named in this long text"},
    {"category": "correctness"},
]
generic = [[f, aa.generic_reason(f)] for f in GENERIC]

FINAL = [
    [{"level": "high", "category": "correctness", "scenario": "x fails"}, {"severity": "critical", "scenario_holds": True}],
    [{"level": "high", "category": "correctness", "scenario": "x fails"}, {"severity": "high", "scenario_holds": "unknown"}],
    [{"level": "medium", "category": "correctness", "scenario": ""}, {"severity": "major", "scenario_holds": "yes"}],
    [{"level": "medium", "category": "correctness", "scenario": ""}, {"scenario_holds": False}],
    [{"level": "low", "category": "correctness"}, "not a dict"],
    [{"level": "medium", "category": "tests", "scenario": "s"}, {"severity": "medium", "scenario_holds": True}],
    [{"level": "medium", "category": "tests", "scenario": "s", "proof": "proven"}, {"severity": "high", "scenario_holds": True}],
    [{"level": "medium", "category": "tests", "scenario": "s", "test_run": {"x": 1}}, {"severity": "medium"}],
    [{"level": "high", "category": "maintainability", "scenario": "s"}, {"severity": "high", "scenario_holds": True}],
    [{"level": "high", "category": "correctness", "scenario": "  "}, {"severity": "bogus", "scenario_holds": "TRUE"}],
    [{"level": "nit", "category": "docs"}, {"severity": "", "scenario_holds": None}],
]
finals = []
for finding, verdict in FINAL:
    out = dict(finding)
    level = aa.final_level(out, verdict)
    finals.append({"finding": finding, "verdict": verdict, "level": level, "after": out})

LESSON_FINDINGS = [
    {"category": "correctness", "level": "high", "severity": "major"},
    {"category": "correctness", "severity": "blocker", "source": "fast"},
    {"category": "correctness", "level": "nit", "severity": "nit"},
    {"category": "tests", "level": "medium", "severity": "minor", "source": "deep:tests"},
    {"category": "arch", "level": "critical", "severity": "blocker", "source": "deep:structure", "shape": "perm"},
    {"category": "security", "level": "high", "severity": "major", "source": "fast"},
    {"category": "correctness", "level": "low", "severity": "minor"},
]
LESSONS = [
    {"category": "correctness", "kind": "fast", "accepted": 1, "rejected": 6, "lowered": True},
    {"category": "tests", "kind": "deep:tests", "accepted": 0, "rejected": 5, "lowered": True},
    {"category": "arch", "kind": "structure:perm", "accepted": 2, "rejected": None, "lowered": True},
    {"category": "security", "kind": "fast", "accepted": 4, "rejected": 1, "lowered": False},
    "junk",
    # The same key twice: the later rule wins, as in a dict comprehension.
    {"category": "tests", "kind": "deep:tests", "accepted": 3, "rejected": 3, "lowered": True},
]
lesson_findings = [dict(f) for f in LESSON_FINDINGS]
lessons_changed = aa.apply_lessons(lesson_findings, LESSONS)
lessons_out = {"findings": LESSON_FINDINGS, "lessons": LESSONS, "changed": lessons_changed, "after": lesson_findings}

ERRORS = [
    "HTTP 429 Too Many Requests", "status 503", "error 5030", "Rate limit exceeded; retry-after: 12", "rate_limited",
    "request timed out", "Timeout after 30s", "ECONNRESET", "socket hang up", "fetch failed", "Connection closed by peer",
    "Not logged in. Please run /login", "401 Unauthorized", "unauthorised", "model_cooldown: reset_seconds=41.5",
    "overloaded_error", "temporarily unavailable", "invalid request: bad schema", aa.DEADLINE_ERROR,
    "Please try again in 7.25s.", "try again in 3 s", "Retry_After=abc 9", "reset in 5", "reset-after: x 2.5",
    "retry after a while", "context length exceeded", "network error", "Error 409 conflict", "4290 things",
    "Retry-After 1.", "reset seconds 4",
]
retries = [{"error": e,
            "transient": aa.is_transient(infer_api.FrameError({"error": e})),
            "auth": aa.is_auth_failure(infer_api.FrameError({"error": e})),
            "throttled": aa.is_throttled(infer_api.FrameError({"error": e})),
            "after": aa.retry_after(e)} for e in ERRORS]
autoreview = {"to_level": to_levels, "holds": holds, "caps": caps, "kinds": kinds_of, "generic": generic,
              "final": finals, "lessons": lessons_out, "retries": retries, "deadline_error": aa.DEADLINE_ERROR}

print(json.dumps({"autoreview": autoreview, "prompts": prompts, "args": args, "targets": targets, "diffs": diffs, "added": added, "kinds": kinds,
                  "categories": categories, "claims": claims, "normalized": normalized, "dedupe": dedupes,
                  "evidence": evidence, "json_objects": jsons, "verdicts": verdicts}, ensure_ascii=False, indent=1))
