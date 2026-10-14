"""Records autoreview_compiled.py's program language for crates/review tests: predicates, validation (valid programs
and each kind of invalid one), coverage and catalogue shapes, result reading, the interpreter's three-valued checks
over recorded step results, for_each expansion, candidate shapes and their template programs, and the planner views.
Run by generate_compiled.mjs; prints compiled.json. Repositories, briefs and sessions are plain fakes: no git, no
model, no sandbox.
"""
import json
import sys
from types import SimpleNamespace

sys.path.insert(0, sys.argv[1])
import autoreview_compiled as c  # noqa: E402
import review_api as ra  # noqa: E402


def norm(step):
    """A normalized step as JSON (the parsed predicate tuple as a list)."""
    out = {}
    for key, value in step.items():
        if key == "predicate_parsed":
            out[key] = list(value) if value is not None else None
        elif key == "steps":
            out[key] = [norm(item) for item in value]
        else:
            out[key] = value
    return out


# --- Predicates -----------------------------------------------------------------------------------------------

PREDICATES = [
    "count == 0", "count>=12", "  count  <  3 ", "count ~ 3", "count == -1", "count == 3\n", "count==", "count != 0",
    "status == passed", "status != failed", "status == could_not_run", "status == maybe", "answer == yes",
    "answer != unclear", "answer == perhaps", "answer==no", "contains show", "contains 'quoted text'", 'contains "x"',
    "not contains db.py", "not  contains   spaced  ", "contains '", "contains ''", "contains \"\"", "contains",
    "not contains", "Contains x", "contains x\ny", "", "   ", None, 5, ["count == 0"], "count == ٣", "count == 007",
    "contains 'a' ", "status == passed\n", "notcontains x",
]
predicates = [[p, (list(r) if (r := c._parse_predicate(p)) is not None else None)] for p in PREDICATES]

# --- Validation -----------------------------------------------------------------------------------------------

F = lambda sid, when, **kw: {"id": sid, "op": "finding", "when": when, "file": "x", "line": 1, "level": "low",  # noqa: E731
                             "claim": "c", **kw}
COVERAGE = [{"id": "S1", "kind": "symbol", "name": "show", "text": "the references of `show`"},
            {"id": "K1", "kind": "key", "name": "metrics_ingress_cidrs", "text": "the siblings and consumers of `metrics_ingress_cidrs`"},
            {"id": "C1", "kind": "claim", "name": "", "text": "the claim [title] Add kind c"},
            {"id": "T1", "kind": "shape", "name": "error-path", "text": "catalogue shape error-path"}]
PROGRAMS = [
    # The TS test's cases.
    [{"steps": "no"}, None, 80],
    [{"steps": []}, None, 80],
    [[1, 2], None, 80],
    [{"steps": [{"id": "bad id", "op": "grep", "args": {"pattern": "x"}}, {"id": "g", "op": "grep", "args": {"pattern": "x"}},
                {"id": "g", "op": "list", "args": {"dir": "."}}]}, None, 80],
    [{"steps": [{"id": "s", "op": "shell", "args": {"cmd": "rm -rf /"}}, {"id": "w", "op": "write", "args": {}}]}, None, 80],
    [{"steps": [{"id": "g", "op": "grep", "args": {}}, {"id": "m", "op": "mutation_check", "args": {"path": "x"}}, {"id": "r", "op": "read"}]}, None, 80],
    [{"steps": [{"id": "g", "op": "grep", "args": {"pattern": "x"}}, {"id": "a", "op": "assert", "step": "g", "predicate": "count == 0"}]}, None, 80],
    [{"steps": [{"id": "g", "op": "grep", "args": {"pattern": "x"}}, {"id": "a", "op": "assert", "step": "g", "predicate": "count == 0", "expect": True},
                F("f_capped", {"step": "a"}, evidence=["g"]),
                {"id": "r", "op": "read", "args": {"path": "x"}}, {"id": "ar", "op": "assert", "step": "r", "predicate": "contains x", "expect": True},
                F("f_read", {"step": "ar"}, evidence=["r"]),
                {"id": "ge", "op": "grep", "args": {"pattern": "x", "count_only": True}}, {"id": "ae", "op": "assert", "step": "ge", "predicate": "count == 0", "expect": True},
                F("f_exact", {"step": "ae"}, evidence=["ge"]),
                {"id": "m", "op": "mutation_check", "args": {"path": "x", "line": 1, "replacement": "y", "tests": ["t"]}},
                F("f_test", {"step": "a"}, evidence=["m"]),
                {"id": "q", "op": "ask", "question": "?", "context": ["r"]},
                F("f_ask", {"step": "q"}, evidence=["r"]),
                {"id": "at", "op": "assert", "step": "m", "predicate": "status == failed", "expect": False},
                F("f_mut", {"step": "at"}, evidence=["r"]),
                {"id": "mix", "op": "assert", "all": ["ae", "a"], "expect": True},
                F("f_mix", {"step": "mix"}, evidence=["ge"]),
                {"id": "exact2", "op": "assert", "any": ["ae", "at"], "expect": True},
                F("f_exact2", {"step": "exact2"}, evidence=["ge"]),
                {"id": "gc", "op": "grep", "args": {"pattern": "x", "count_only": "true"}}, {"id": "ac", "op": "assert", "step": "gc", "predicate": "count == 0", "expect": True},
                F("f_strtrue", {"step": "ac"}, evidence=["gc"])]}, None, 80],
    [{"steps": [{"id": "a", "op": "assert", "step": "nope", "predicate": "count == 0", "expect": True},
                {"id": "q", "op": "ask", "question": "?", "context": ["a"]},
                F("f", {"step": "q"}, evidence=["zz"])]}, None, 80],
    [{"steps": [{"id": "g", "op": "grep", "args": {"pattern": "x"}, "needs": ["h"]}, {"id": "h", "op": "grep", "args": {"pattern": "y"}, "needs": ["g"]}]}, None, 80],
    [{"steps": [{"id": "g", "op": "grep", "args": {"pattern": "x"}, "needs": ["g"]}, {"id": "h", "op": "list", "args": {"dir": "x"}, "needs": ["i"]},
                {"id": "i", "op": "list", "args": {"dir": "x"}, "needs": ["j"]}, {"id": "j", "op": "list", "args": {"dir": "x"}, "needs": ["h", "g"]}]}, None, 80],
    [{"steps": [{"id": "g", "op": "grep", "args": {"pattern": "x"}},
                {"id": "a1", "op": "assert", "step": "g", "predicate": "count ~ 3", "expect": True},
                {"id": "a2", "op": "assert", "step": "g", "predicate": "status == maybe", "expect": True},
                {"id": "a3", "op": "assert", "step": "g", "predicate": "answer == perhaps", "expect": True},
                {"id": "a4", "op": "assert", "all": [], "step": "g", "predicate": "count == 0", "expect": True},
                {"id": "a5", "op": "assert", "expect": True},
                {"id": "a6", "op": "assert", "all": ["a1", 3], "any": ["a2"], "expect": "yes"},
                {"id": "a7", "op": "assert", "step": 5, "predicate": 3, "expect": False},
                {"id": "a8", "op": "assert", "any": "a1", "expect": True},
                {"id": "a9", "op": "assert", "step": "a1", "predicate": "count == 1", "expect": True},
                {"id": "a10", "op": "assert", "all": ["g"], "expect": True}]}, None, 80],
    [{"steps": [{"id": "g", "op": "grep", "args": {"pattern": "x"}},
                F("f", {"step": "g"}, level="sev", evidence=["g"]),
                {"id": "f2", "op": "finding", "file": "x", "line": "two", "level": "low", "claim": "c", "evidence": []},
                {"id": "f3", "op": "finding", "when": "nope", "file": " ", "line": True, "claim": 5, "citations": [{"path": "x", "line": 1}, "junk"]},
                {"id": "f4", "op": "finding", "when": ["x"], "file": "x", "line": " 12 ", "level": " MAJOR ", "claim": "c", "evidence": "g"},
                {"id": "f5", "op": "finding", "when": {"step": 3}, "file": "x", "line": "{{ item.line }}", "level": None, "claim": "c"},
                {"id": "f6", "op": "finding", "when": {"step": "g", "not": "yes"}, "file": "x", "line": 1.0, "level": "blocker", "claim": "c", "evidence": ["g"]},
                {"id": "f7", "op": "finding", "when": {"step": "g", "not": True}, "file": "x", "line": "", "claim": "c", "evidence": ["g"],
                 "category": "tests", "why": "w", "fix": "f", "scenario": "s", "unpinned": {"behaviour": "b"}, "consequence": "k", "extra": 1}]}, None, 80],
    [{"steps": [{"id": "r", "op": "read", "args": {"path": "x"}},
                {"id": "fe", "op": "for_each", "over": "r", "steps": [
                    {"id": "inner", "op": "for_each", "over": "r", "steps": [{"id": "z", "op": "list", "args": {"dir": "."}}]}]},
                {"id": "fe2", "op": "for_each", "over": "r", "steps": []},
                {"id": "g", "op": "grep", "args": {"pattern": "x"}},
                {"id": "fe3", "op": "for_each", "over": "g", "steps": [{"id": "z", "op": "list", "args": {"dir": "."}}]},
                {"id": "top", "op": "assert", "step": "z", "predicate": "count == 0", "expect": True}]}, None, 80],
    [{"steps": [{"id": f"q{i}", "op": "ask", "question": "?", "context": ["r"]} for i in range(41)] + [{"id": "r", "op": "read", "args": {"path": "x"}}]}, None, 80],
    [{"steps": [{"id": f"r{i}", "op": "read", "args": {"path": "x"}} for i in range(81)]}, None, 80],
    [{"steps": [{"id": f"r{i}", "op": "read", "args": {"path": "x"}} for i in range(81)]}, None, 120],
    [{"steps": [{"id": "g", "op": "grep", "args": {"pattern": "x"}}] + [F(f"f{i}", {"step": "a"}, evidence=["g"]) for i in range(13)]
      + [{"id": "a", "op": "assert", "step": "g", "predicate": "count == 0", "expect": True}]}, None, 80],
    # A valid program with every op, for_each with placeholders, when as a string, material and covers.
    [{"summary": "  exercise\n the   language  ", "uncovered": ["C1: comment only", 5, "T1"], "steps": [
        {"id": "g", "op": "grep", "args": {"pattern": "show\\(", "max": 20}, "covers": ["S1", 3]},
        {"id": "ls", "op": "list", "args": {"dir": "src"}, "needs": ["g", 2, "g"]},
        {"id": "fe", "op": "for_each", "over": "g", "max_items": "3", "steps": [
            {"id": "line", "op": "read", "args": {"path": "{{item.path}}", "start": "{{item.line}}", "end": "{{item.line}}"}},
            {"id": "has", "op": "assert", "step": "line", "predicate": "contains show", "expect": True, "holds": "{{item.path}} calls show"},
            {"id": "q", "op": "ask", "question": "Is show called at {{item.path}}?", "context": ["line"], "material": "ab c"},
            {"id": "fq", "op": "finding", "when": {"step": "q", "not": True}, "file": "{{item.path}}", "line": "{{item.line}}", "level": "high",
             "category": "correctness", "claim": "wrong arity", "why": "arity", "evidence": ["q", "line"]}]},
        {"id": "n3", "op": "assert", "step": "g", "predicate": "count >= 3", "holds": "show is called {{g.count}} times", "expect": True},
        {"id": "q1", "op": "ask", "question": "Does a test assert it?", "context": ["ls", "g"], "material": ["  ", "m" * 4100, 7, "b", "c", "d", "e"]},
        {"id": "q2", "op": "ask", "question": "  ", "context": [], "material": {"k1": 1, " ": 2}},
        {"id": "q3", "op": "ask", "question": "x" * 1501, "context": ["g"]},
        {"id": "both", "op": "assert", "all": ["n3", "q1"], "expect": True, "holds": 5},
        {"id": "gated", "op": "read", "args": {"path": "README.md"}, "when": "n3"},
        {"id": "m", "op": "mutation_check", "args": {"path": "a", "line": 1, "replacement": "b", "tests": ["t"]}},
        {"id": "rt", "op": "run_tests", "args": {"paths": ["t"]}},
        {"id": "d", "op": "definition", "args": {"symbol": "s"}}, {"id": "rf", "op": "references", "args": {"symbol": "s"}},
        {"id": "h", "op": "history", "args": {"path": "p"}}, {"id": "b", "op": "blame_range", "args": {"path": "p"}},
        {"id": "p", "op": "pickaxe", "args": {"string": "s"}},
        F("fin", "both", evidence=["q1", "m"], level="Major", category="tests", why="w", fix="f")]}, None, 80],
    # The same, cleaned up, so that it validates.
    [{"summary": "ok", "uncovered": ["C1: comment only", "T1 : none"], "steps": [
        {"id": "g", "op": "grep", "args": {"pattern": "show\\(", "max": 20}, "covers": ["S1"]},
        {"id": "ls", "op": "list", "args": {"dir": "src"}, "needs": ["g", 2, "g"]},
        {"id": "fe", "op": "for_each", "over": "g", "max_items": 30.7, "steps": [
            {"id": "line", "op": "read", "args": {"path": "{{item.path}}", "start": "{{item.line}}", "end": "{{item.line}}"}, "needs": ["ls"]},
            {"id": "has", "op": "assert", "step": "line", "predicate": "contains 'show'", "expect": True, "holds": "{{item.path}} calls show"},
            {"id": "q", "op": "ask", "question": "Is show called at {{item.path}}?", "context": ["line"], "material": "ab c"},
            {"id": "fq", "op": "finding", "when": {"step": "q", "not": True}, "file": "{{item.path}}", "line": "{{item.line}}", "level": "high",
             "category": "correctness", "claim": "wrong arity", "why": "arity", "evidence": ["q", "line"]}]},
        {"id": "n3", "op": "assert", "step": "g", "predicate": "count >= 3", "holds": "show is called {{g.count}} times", "expect": True},
        {"id": "q1", "op": "ask", "question": "Does a test assert metrics_ingress_cidrs?", "context": ["ls", "g"], "material": ["  ", "m" * 4100, 7, "b", "c", "d", "e"]},
        {"id": "both", "op": "assert", "all": ["n3", "q1"], "expect": True, "holds": 5},
        {"id": "gated", "op": "read", "args": {"path": "README.md"}, "when": "n3"},
        {"id": "m", "op": "mutation_check", "args": {"path": "a", "line": 1, "replacement": "b", "tests": ["t"]}},
        {"id": "fin", "op": "finding", "when": "both", "file": "x", "line": "7", "level": "Major", "claim": "c", "evidence": ["q1", "m"],
         "category": "tests", "citations": [{"path": "x", "line": 1, "quote": "q"}], "unpinned": {}, "why": None}]}, COVERAGE, 80],
    # max_items variants and a duplicate sub-step id; a sub-step naming another for_each's sub-step.
    [{"steps": [{"id": "g", "op": "grep", "args": {"pattern": "x"}},
                {"id": "fa", "op": "for_each", "over": "g", "max_items": 0, "steps": [{"id": "s", "op": "read", "args": {"path": "p"}}]},
                {"id": "fb", "op": "for_each", "over": "g", "max_items": "x", "steps": [{"id": "s", "op": "read", "args": {"path": "p"}}]},
                {"id": "fc", "op": "for_each", "over": "g", "max_items": -4, "steps": [{"id": "t", "op": "read", "args": {"path": "p"}, "needs": ["s"]}]},
                {"id": "fd", "op": "for_each", "over": 3, "max_items": [1], "steps": [{"id": "u", "op": "read", "args": {"path": "p"}}]},
                {"id": "fe", "op": "for_each", "over": "g", "max_items": True, "steps": [{"id": "g", "op": "read", "args": {"path": "p"}}] * 9},
                {"id": "ff", "op": "for_each", "over": "g", "max_items": " 12 ", "steps": [{"id": "v", "op": "read", "args": {"path": "p"}, "needs": ["ff"]}]}]}, None, 80],
    # Ids: a trailing newline matches Python's `$`; non-objects; ids too long; an op that is not a string.
    [{"steps": [{"id": "ok\n", "op": "list", "args": {"dir": "."}}, "junk", {"id": "a" * 41, "op": "list", "args": {}},
                {"id": "_a-1", "op": ["grep"]}, {"op": "grep"}, {"id": 5, "op": "grep"}, {"id": "1a", "op": "grep"},
                {"id": "é", "op": "grep"}, {"id": "x", "op": None}]}, None, 80],
    # Coverage: bare, covered by naming, unknown declared ids, word boundaries.
    [{"steps": [{"id": "r", "op": "read", "args": {"path": "src/app.py"}}]}, COVERAGE, 80],
    [{"steps": [{"id": "refs", "op": "references", "args": {"symbol": "show"}},
                {"id": "g", "op": "grep", "args": {"pattern": "metrics_ingress_cidrs", "count_only": True}},
                {"id": "q", "op": "ask", "question": "Does the title hold?", "context": ["g"], "covers": ["C1"]}], "uncovered": ["T1: n/a"]}, COVERAGE, 80],
    [{"steps": [{"id": "refs", "op": "references", "args": {"symbol": "show"}}], "uncovered": ["Z9: nothing", "K1", " C1 : x", "T1:"]}, COVERAGE, 80],
    [{"steps": [{"id": "refs", "op": "references", "args": {"symbol": "shower"}},
                {"id": "q", "op": "ask", "question": "is xmetrics_ingress_cidrs set?", "context": ["refs"]},
                {"id": "a", "op": "assert", "step": "q", "predicate": "answer == yes", "expect": True, "holds": "show-off"}],
      "uncovered": "C1"}, COVERAGE, 80],
    [{"steps": [{"id": "q", "op": "ask", "question": "Is metrics_ingress_cidrs.x read by show?", "context": ["refs"]},
                {"id": "refs", "op": "references", "args": {"symbol": "y"}},
                F("f", {"step": "q"}, evidence=["refs"], claim="show_x and _show", why="éshow")],
      "uncovered": ["C1: why", "T1: why"]}, COVERAGE, 80],
    [{"steps": [{"id": "r", "op": "read", "args": {"path": "x"}}]}, [], 80],
    [{"steps": [{"id": "r", "op": "read", "args": {"path": "x"}}], "summary": 12}, None, 1],
    [{"steps": [{"id": "r", "op": "read", "args": {"path": "x"}}, {"id": "r2", "op": "read", "args": {"path": "x"}}]}, None, 1],
]


def validated(raw, coverage, max_planned):
    program, errors = c.validate(raw, coverage, max_planned)
    out = {"raw": raw, "coverage": coverage, "max_planned": max_planned, "errors": errors}
    if program is not None:
        out["steps"] = [norm(step) for step in program.steps]
        out["json"] = program.as_json()
    return out


validations = [validated(*case) for case in PROGRAMS]
# Every error message prefix appears more than 40 times: the list is cut at 40.
many = {"steps": [{"id": f"s{i}", "op": "grep", "args": {}} for i in range(45)]}
validations.append(validated(many, None, 80))

# --- Coverage and catalogue shapes ----------------------------------------------------------------------------


class Reference:
    def __init__(self, facts):
        self._facts = facts

    def facts(self):
        return self._facts


def brief(extracted=None, required=None, claim_list=None, facts=None, text="", tests=None):
    return SimpleNamespace(extracted=extracted or {}, required=required or {}, claim_list=claim_list or [],
                           reference=Reference(facts or []), text=text, tests=tests or [])


def brief_json(b):
    return {"extracted": b.extracted, "required": b.required, "claim_list": [list(item) for item in b.claim_list],
            "facts": b.reference.facts(), "text": b.text, "tests": b.tests}


def diff(path, added=(), removed=(), status=""):
    head = f"diff --git a/{path} b/{path}\n"
    if status == "deleted":
        head += "deleted file mode 100644\n"
    body = "".join(f"-{line}\n" for line in removed) + "".join(f"+{line}\n" for line in added)
    return head + f"--- a/{path}\n+++ b/{path}\n@@ -1,{len(removed)} +1,{len(added)} @@\n" + body


COVERAGE_CASES = [
    [brief(), ""],
    [brief(required={"siblings": ["show", "KINDS"], "claims": ["APP_MODE", "show"]},
           extracted={"fields": ["metrics_ingress_cidrs", "APP_MODE"] + [f"f{i}" for i in range(9)]},
           claim_list=[["src/app.py:5", "Upper-cases the kind."], ["title", "Add kind c"]] + [[f"s{i}", f"t{i}"] for i in range(8)]), None],
    [brief(extracted={"fields": ["a", "b", "c", "d", "e"], "env": ["APP_MODE"], "flags": ["--x"], "config": ["c1", "c2", "c3", "c4", "c5"],
                      "claims": [["README.md", 3, "The tool " + "x" * 100], ["a.py", 1, "short"], ["b", 2, "c"], ["d", 4, "e"]]},
           facts=[{"kind": "family", "id": "fam1"}, {"kind": "other", "id": "o"}], text="see: same name elsewhere: x.py"),
     diff("src/app.py", ['KINDS = ["a", "b", "c"]', "    'new-member',", "if len(x) > 3:", "    raise ValueError('bad')",
                         "    validate(x)", "    cursor.execute(q)", "    retry(3)"], ["old line"])
     + diff("tests/test_app.py", ["def test_x():", "    assert x"])
     + diff(".github/workflows/ci.yml", ["on: push"]) + diff("deploy/app.yaml", ["kind: Deployment"])
     + diff("docs/guide.md", ["# Guide"]) + diff("package-lock.json", ["{}"]) + diff("gone.py", [], ["x"], "deleted")],
    [brief(facts=[{"kind": "family", "id": "f1"}, {"kind": "family", "id": "f2"}, {"kind": "family", "id": "f3"}, {"kind": "family", "id": "f4"}]),
     diff("infra/main.tf", ["resource x {}"]) + diff("config.yml", ["a: 1"]) + diff(".gitlab-ci.yml", ["x"]) + diff("Jenkinsfile", ["x"])],
    [brief(extracted={"constants": ["A", "B"]}), diff("lib/a.ts", ["  'member',", "try { x() } catch (e) { throw e }", "const t = timeout(5)"])],
    [brief(), diff("lib/a.ts", ['  "member"', "x"], ["} finally {"]) + diff("Dockerfile", ["FROM x"])],
    [brief(), diff("conf/app.toml", ["retry = 3", "if x == 1: parse(y)"])],
    [brief(), diff("src/b.py", ["if x.startswith('a'):", "    os.remove(p)"])],
    [brief(), diff("src/c.py", ["check(x)", "requests.post(u)"])],
    [brief(), diff("src/c.py", ["check(x)"], ["subprocess.run(y)"])],
]
coverage_cases = []
for b, text in COVERAGE_CASES:
    files = ra.parse_diff(text) if text is not None else None
    coverage_cases.append({"brief": brief_json(b), "diff": text, "items": c.coverage_items(b, files)})

UNCOVERED = [
    [COVERAGE, [{"id": "a", "covers": ["S1"], "args": {"pattern": "metrics_ingress_cidrs"}}], []],
    [COVERAGE, [{"id": "a", "question": "show?"}, {"id": "b", "claim": "metrics_ingress_cidrs2", "why": 5}], ["C1 : later", "T1"]],
    [COVERAGE, [{"id": "a", "args": {"pattern": "aéshow"}, "predicate": "contains show_"}], ["K1:x"]],
    [[{"id": "S1", "kind": "symbol", "name": "", "text": "t"}, {"id": "K2", "kind": "key", "name": "a.b", "text": "t"}],
     [{"id": "a", "holds": "aXb a.b"}], []],
    [[{"id": "K2", "kind": "key", "name": "a.b", "text": "t"}], [{"id": "a", "holds": "aXb"}], []],
]
uncovered = [{"coverage": cov, "steps": steps, "declared": declared, "out": c.uncovered_items(cov, steps, declared)}
             for cov, steps, declared in UNCOVERED]

# --- Result reading -------------------------------------------------------------------------------------------

COUNTS = [
    ["grep", "grep show\\( -> 4 matches", ""], ["history", "history src/app.py -> 2 commits", ""],
    ["list", "list src (2 entries)", ""], ["read", "read a.py:1-3", "a\nb\nc\n"], ["read", "read a.py", ""],
    ["grep", "grep x (more not shown)", "a:1: x"], ["list", "list x -> 3 matches (5 entries)", ""],
    ["read", "read x -> 7 commits", "a\r\nb"],
]
counts = [[op, title, body, c._count_of(op, title, body)] for op, title, body in COUNTS]
ITEMS = [
    ["grep", "src/a.py:5: print(show(kind))\nsrc/b.py:12:x\nnot a hit\nc:d:3: y\n0 matches\nweird:007:  spaced"],
    ["references", "a.py:1:def x\n... cut\n"], ["definition", "x.py:9: class X"],
    ["list", "app.py\n  \n...more\ncli.py\n0 matches\n"],
    ["history", "abc123 fix\nno commits found\n"], ["blame_range", "no history available for these lines\nx"],
    ["pickaxe", "no commit added or removed it\n"], ["read", "a\nb"], ["run_tests", "x"],
]
items = [[op, body, c._items_of(op, body)] for op, body in ITEMS]
RENDERS = [{"path": "a.py", "line": 3}, {"path": "a.py", "line": 3, "text": "x"}, {"text": "é", "n": [1, 2.5, None, True]},
           None, "text", 5, 2.5, True, ["a", 1], {"path": None, "line": None}, {}, 1e20]
renders = [[value, c._render(value)] for value in RENDERS]
count_predicates = [[count, capped, op, n, list(c._count_predicate(count, capped, op, n))]
                    for count in (0, 3, 5) for capped in (False, True)
                    for op in ("==", "!=", ">=", "<=", ">", "<") for n in (0, 3, 5, 8)]
limits = [[n, list(c.limits_for(n))] for n in (0, 3, 100, 101, 400)]

# --- The interpreter's checks over recorded results -----------------------------------------------------------


class Frames:
    timings: list = []


def interpreter(program, results, defer=False):
    it = c.Interpreter(program, None, frames=Frames(), session=None, diff_lines={}, ask_model=None, ask_thinking=None,
                       cutoff=None, clock=lambda: 0.0, cap=lambda *a: "low", defer_unrunnable=defer)
    for step in program.steps:
        it.steps_by_id[step["id"]] = step
        for sub in step.get("steps") or []:
            it.steps_by_id[sub["id"]] = sub
    it.results = json.loads(json.dumps(results))
    return it


def ok(op, **kw):
    return {"op": op, "status": "ok", "text": "", **kw}


EVAL_PROGRAM = {"summary": "s", "steps": [
    {"id": "g", "op": "grep", "args": {"pattern": "show"}},
    {"id": "gc", "op": "grep", "args": {"pattern": "x", "count_only": True}},
    {"id": "ls", "op": "list", "args": {"dir": "src"}},
    {"id": "r", "op": "read", "args": {"path": "a.py"}},
    {"id": "rt", "op": "read", "args": {"path": "b.py"}},
    {"id": "secret", "op": "read", "args": {"path": "secret.txt"}},
    {"id": "gated", "op": "read", "args": {"path": "README.md"}},
    {"id": "nocount", "op": "run_tests", "args": {"paths": ["t"]}},
    {"id": "m", "op": "mutation_check", "args": {"path": "a", "line": 1, "replacement": "b", "tests": ["t"]}},
    {"id": "m2", "op": "mutation_check", "args": {"path": "a", "line": 1, "replacement": "b", "tests": ["t"]}},
    {"id": "t3", "op": "run_tests", "args": {"paths": ["t"]}},
    {"id": "q1", "op": "ask", "question": "q1?", "context": ["r"]},
    {"id": "q2", "op": "ask", "question": "q2?", "context": ["r"]},
    {"id": "q3", "op": "ask", "question": "q3?", "context": ["r"]},
    {"id": "qf", "op": "ask", "question": "qf?", "context": ["r"]},
    {"id": "n0", "op": "assert", "step": "g", "predicate": "count == 0", "holds": "never", "expect": False},
    {"id": "n3", "op": "assert", "step": "g", "predicate": "count >= 3", "holds": "show is called {{g.count}} times; {{g}} {{ls.text}} {{nope}} {{g.items}} {{g.title}}", "expect": True},
    {"id": "lt", "op": "assert", "step": "g", "predicate": "count < 2", "expect": False},
    {"id": "ne", "op": "assert", "step": "g", "predicate": "count != 3", "expect": True},
    {"id": "capeq", "op": "assert", "step": "gc", "predicate": "count == 2", "expect": True},
    {"id": "capge", "op": "assert", "step": "gc", "predicate": "count >= 20", "expect": True},
    {"id": "entries", "op": "assert", "step": "ls", "predicate": "count == 2", "holds": "src holds {{ls.count}} files: {{ls}}", "expect": True},
    {"id": "has_cli", "op": "assert", "step": "ls", "predicate": "contains cli.py", "expect": True},
    {"id": "no_db", "op": "assert", "step": "ls", "predicate": "not contains db.py", "expect": True},
    {"id": "spaced", "op": "assert", "step": "r", "predicate": "contains 'def   show(kind) :'", "expect": True, "holds": "found"},
    {"id": "cut", "op": "assert", "step": "rt", "predicate": "contains absent", "expect": False},
    {"id": "cut_not", "op": "assert", "step": "rt", "predicate": "not contains absent", "expect": True},
    {"id": "cut_found", "op": "assert", "step": "rt", "predicate": "contains present", "expect": True},
    {"id": "fill_needle", "op": "assert", "step": "r", "predicate": "contains {{ls.count}}", "expect": True},
    {"id": "onSecret", "op": "assert", "step": "secret", "predicate": "contains hunter2", "expect": True},
    {"id": "onGated", "op": "assert", "step": "gated", "predicate": "count >= 1", "expect": True},
    {"id": "noc", "op": "assert", "step": "r", "predicate": "count >= 1", "expect": True},
    {"id": "nocount_a", "op": "assert", "step": "q1", "predicate": "count >= 1", "expect": True},
    {"id": "mutant_survived", "op": "assert", "step": "m", "predicate": "status == passed", "expect": True, "holds": "the mutant survived"},
    {"id": "mutant_caught", "op": "assert", "step": "m2", "predicate": "status == passed", "expect": True},
    {"id": "cnr", "op": "assert", "step": "t3", "predicate": "status == failed", "expect": True},
    {"id": "cnr_is", "op": "assert", "step": "t3", "predicate": "status == could_not_run", "expect": True},
    {"id": "cnr_count", "op": "assert", "step": "t3", "predicate": "count == 0", "expect": True},
    {"id": "yes1", "op": "assert", "step": "q1", "predicate": "answer == no", "expect": True},
    {"id": "notyes2", "op": "assert", "step": "q2", "predicate": "answer != yes", "expect": True},
    {"id": "unclear2", "op": "assert", "step": "q2", "predicate": "answer == unclear", "expect": True},
    {"id": "q3yes", "op": "assert", "step": "q3", "predicate": "answer == yes", "expect": False},
    {"id": "both", "op": "assert", "all": ["n3", "yes1"], "holds": "all held", "expect": True},
    {"id": "either", "op": "assert", "any": ["n0", "yes1"], "expect": True},
    {"id": "neither", "op": "assert", "any": ["n0", "lt", "q2"], "expect": False},
    {"id": "allfalse", "op": "assert", "all": ["n0", "q2"], "expect": True},
    {"id": "allunk", "op": "assert", "all": ["n3", "q2"], "expect": True},
    {"id": "anyfalse", "op": "assert", "any": ["n0", "lt"], "expect": True},
    {"id": "viaask", "op": "assert", "any": ["q3yes"], "expect": True},
    {"id": "nested", "op": "assert", "all": ["either", "capeq"], "expect": True},
    {"id": "f_yes", "op": "finding", "when": {"step": "yes1"}, "file": "x", "line": 1, "level": "low", "claim": "c", "evidence": ["q1"]},
    {"id": "f_q3", "op": "finding", "when": {"step": "q2"}, "file": "x", "line": 1, "level": "low", "claim": "c", "evidence": ["q2"]},
    {"id": "f_q3n", "op": "finding", "when": {"step": "q2", "not": True}, "file": "x", "line": 1, "level": "low", "claim": "c", "evidence": ["q2"]},
    {"id": "f_false", "op": "finding", "when": {"step": "n0"}, "file": "x", "line": 1, "level": "low", "claim": "c", "evidence": ["g", "q1"]},
    {"id": "f_not", "op": "finding", "when": {"step": "n0", "not": True}, "file": "x", "line": 1, "level": "low", "claim": "c", "evidence": ["g", "q1"]},
    {"id": "f_contra", "op": "finding", "when": {"step": "capeq"}, "file": "x", "line": 1, "level": "low", "claim": "c", "evidence": ["gc"]},
    {"id": "f_contra_nested", "op": "finding", "when": {"step": "nested"}, "file": "x", "line": 1, "level": "low", "claim": "c", "evidence": ["gc"]},
    {"id": "f_unknown", "op": "finding", "when": {"step": "onSecret"}, "file": "x", "line": 1, "level": "low", "claim": "c", "evidence": ["g", "q1"]},
    {"id": "f_cnr", "op": "finding", "when": {"step": "cnr"}, "file": "x", "line": 1, "level": "low", "claim": "c", "evidence": ["t3"]},
    {"id": "f_caught", "op": "finding", "when": {"step": "mutant_caught"}, "file": "x", "line": 1, "level": "low", "claim": "c", "evidence": ["m2"]},
    {"id": "f_qf", "op": "finding", "when": {"step": "qf"}, "file": "x", "line": 1, "level": "low", "claim": "c", "evidence": ["qf"]},
    {"id": "f_missing", "op": "finding", "when": {"step": "late"}, "file": "x", "line": 1, "level": "low", "claim": "c", "evidence": ["g", "q1"]},
    {"id": "late", "op": "assert", "step": "g", "predicate": "count == 4", "expect": True},
]}
EVAL_RESULTS = {
    "g": ok("grep", title="grep show -> 4 matches", text="src/a.py:4: def show(kind):\nsrc/cli.py:5: print(show(kind))", count=4,
            items=[{"path": "src/a.py", "line": 4, "text": "def show(kind):"}, {"path": "src/cli.py", "line": 5, "text": "print(show(kind))"}],
            capped=False, truncated=False, summary="grep show -> 4 matches: src/a.py:4; src/cli.py:5"),
    "gc": ok("grep", title="grep x -> 20 matches (more not shown)", count=20, items=[], capped=True, truncated=False, summary="grep x"),
    "ls": ok("list", title="list src (2 entries)", text="app.py\ncli.py", count=2, items=["app.py", "cli.py"], capped=False, truncated=False,
             summary="list src (2 entries): app.py; cli.py"),
    "r": ok("read", title="read a.py", text="def show(kind):\n    return 2", count=None, items=[], capped=False, truncated=False, summary="read a.py"),
    "rt": ok("read", title="read b.py", text="present ... cut", count=1, items=[], capped=False, truncated=True, summary="read b.py"),
    "secret": {"op": "read", "status": "failed", "detail": "secret.txt is not a tracked file at the reviewed commit", "text": ""},
    "gated": {"op": "read", "status": "skipped", "detail": "its condition n0 does not hold", "text": ""},
    "m": ok("mutation_check", title="mutation_check run 1", outcome="passed", caught=False, run={"n": 1}, count=None),
    "m2": ok("mutation_check", title="mutation_check run 2", outcome="failed", caught=True, run={"n": 2}),
    "t3": ok("run_tests", title="run_tests", outcome="could_not_run", detail="the pytest runner was unavailable"),
    "q1": ok("ask", answer="no", quote='assert show("a")', quote_ok=True, why="truthiness only"),
    "q2": ok("ask", answer="unclear", quote="x", quote_ok=False, why="x"),
    "q3": ok("ask", answer="yes", quote="def show", quote_ok=True, why="w"),
    "qf": {"op": "ask", "status": "failed", "detail": "timeout", "text": ""},
}
EVAL_ORDER = ["n0", "n3", "lt", "ne", "capeq", "capge", "entries", "has_cli", "no_db", "spaced", "cut", "cut_not", "cut_found",
              "fill_needle", "onSecret", "onGated", "noc", "nocount_a", "mutant_survived", "mutant_caught", "cnr", "cnr_is",
              "cnr_count", "yes1", "notyes2", "unclear2", "q3yes", "both", "either", "neither", "allfalse", "allunk", "anyfalse",
              "viaask", "nested"]
FINDINGS = ["f_yes", "f_q3", "f_q3n", "f_false", "f_not", "f_contra", "f_contra_nested", "f_unknown", "f_cnr", "f_caught", "f_qf",
            "f_missing"]


def evaluate(defer):
    program, errors = c.validate(EVAL_PROGRAM)
    assert program is not None, errors
    it = interpreter(program, EVAL_RESULTS, defer)
    asserts = []
    for sid in EVAL_ORDER:
        it._assert(it.steps_by_id[sid])
        asserts.append({"id": sid, "result": it.results[sid], "record": it.records[-1]})
    ids = list(it.steps_by_id) + ["unknown_id"]
    queries = {sid: {"truth": it.truth(sid), "roots_in_ask": it.roots_in_ask(sid), "contradicted": it.contradicted(sid),
                     "test_decided": it.test_decided(sid), "test_could_not_run": it.test_could_not_run(sid)} for sid in ids}
    conditions = [[when, it._condition(when)] for when in
                  [None, {"step": "n3", "not": False}, {"step": "n3", "not": True}, {"step": "n0", "not": True},
                   {"step": "q2", "not": True}, {"step": "q2", "not": False}, {"step": "qf", "not": True},
                   {"step": "g", "not": False}, {"step": "nope", "not": True}]]
    emitted = []
    it._emit = lambda step, scope, began, resolution, **extra: emitted.append(step["id"])
    gates = []
    for sid in FINDINGS:
        before = len(it.records)
        counts = dict(it.not_emitted)
        out = it._finding(it.steps_by_id[sid], {})
        gates.append({"id": sid, "resolve": out[2] if out else None, "emitted": sid in emitted,
                      "record": it.records[before] if len(it.records) > before else None,
                      "not_emitted": {k: it.not_emitted[k] - counts[k] for k in counts}})
    return {"asserts": asserts, "queries": queries, "conditions": conditions, "gates": gates}


FILLS = [
    ["{{g.count}} and {{ g.title }} and {{g}} and {{g.text}} and {{ls.items}}", {}],
    ["{{item.path}}:{{item.line}} #{{index}} {{item}} {{item.nope}} {{item.path.x}}", {"item": {"path": "a.py", "line": 3, "text": "x"}, "index": 0}],
    ["{{item}} {{item.path}} {{index}}", {"item": "app.py", "index": 1}],
    ["{{q1.answer}} {{q1.quote}} {{q1.why}} {{q1.value}} {{q1.status}} {{m.status}} {{t3.status}} {{q1.other}} {{qf.text}}", {}],
    ["{{secret.text}} {{secret}} {{n0.value}} {{nope.count}} {{g[0].count}} {{ bad id }} {{}} {{x.y.z}}", {}],
    [{"path": "{{item.path}}", "n": ["{{index}}", 5, None, {"k": "{{g.count}}"}]}, {"item": {"path": "p", "line": 1}, "index": 2}],
    [7, {}], [None, {}],
]
fill_results = dict(EVAL_RESULTS)
fill_results["n0"] = ok("assert", value=False, summary="does not hold")
fill_it = interpreter(c.validate(EVAL_PROGRAM)[0], fill_results)
fills = [[text, scope, fill_it.fill(text, scope)] for text, scope in FILLS]

SUMMARIES = [
    ["run_tests", {"title": "run_tests t", "outcome": "passed"}], ["mutation_check", {"outcome": "failed"}],
    ["ask", {"answer": "yes", "quote": "q" * 200}], ["ask", {"answer": None, "quote": None}],
    ["assert", {"value": True, "detail": "count is 3"}], ["assert", {"value": False, "detail": "count is 3"}],
    ["assert", {"value": None, "detail": "x did not run"}], ["assert", {"value": None}],
    ["assert", {"value": True, "contradicted": True, "detail": "d"}], ["assert", {"value": False, "contradicted": True}],
    ["finding", {"emitted": True}], ["finding", {"gate": "gate false", "detail": "d"}], ["finding", {"detail": "d"}],
    ["finding", {}], ["grep", {"title": "grep x", "items": [{"path": "a", "line": i} for i in range(6)]}],
    ["list", {"title": "list x", "items": ["a", "b"]}], ["read", {"title": "read x", "items": []}], ["read", {}],
    ["for_each", {"title": "t", "items": [1, None, {"k": 1}]}],
]
summaries = [[op, result, fill_it._summary(op, result)] for op, result in SUMMARIES]

QUOTES = [
    [{"answer": "yes", "quote": "assert  show(\"a\")", "why": "w"}, 'def test_show():\n    assert show("a")\n'],
    [{"answer": " YES ", "quote": "this line is not in the material"}, "x\ny"],
    [{"answer": "no", "quote": "abc"}, "abc"],
    [{"answer": "maybe", "quote": "abcdef"}, "abcdef"],
    [{"answer": "unclear", "quote": ""}, ""],
    [{"answer": "yes", "quote": "prefix print(show(kind)) suffix"}, "    print(show(kind))\nshort\n"],
    [{"answer": "yes", "quote": "prefix short suffix"}, "short\n"],
    [{"answer": "yes", "quote": 5}, "x"], [{"quote": "abcd"}, "abcd"], [{"answer": True, "quote": "abcd"}, "abcd"],
    ["not a dict", "x"], [None, "x"],
]
quotes = [[reply, material, list(fill_it._check_quote(reply, material))] for reply, material in QUOTES]

# for_each expansion: items, max_items, the step limit, renamed references.
EXPAND_PROGRAM = {"steps": [
    {"id": "g", "op": "grep", "args": {"pattern": "x"}},
    {"id": "pre", "op": "assert", "step": "g", "predicate": "count >= 1", "expect": True},
    {"id": "fe", "op": "for_each", "over": "g", "max_items": 2, "steps": [
        {"id": "line", "op": "read", "args": {"path": "{{item.path}}", "start": "{{item.line}}"}, "needs": ["pre"]},
        {"id": "has", "op": "assert", "step": "line", "predicate": "contains show", "expect": True, "when": {"step": "pre", "not": True}},
        {"id": "q", "op": "ask", "question": "at {{item.path}}?", "context": ["line"], "material": ["m"]},
        {"id": "fq", "op": "finding", "when": {"step": "q", "not": True}, "file": "{{item.path}}", "line": "{{item.line}}", "level": "high",
         "claim": "c", "evidence": ["q", "line", "g"], "citations": [{"path": "a", "line": 1}]},
        {"id": "both", "op": "assert", "all": ["has", "pre"], "any": None, "expect": True}]},
    {"id": "fe2", "op": "for_each", "over": "g", "steps": [{"id": "z", "op": "list", "args": {"dir": "{{item}}"}}]},
]}
expand_program = c.validate(EXPAND_PROGRAM)[0]
assert expand_program is not None
EXPANDS = [
    ["fe", ok("grep", items=[{"path": "a.py", "line": 1}, {"path": "b.py", "line": 2}, {"path": "c.py", "line": 3}]), 120, 0],
    ["fe", ok("grep", items=[{"path": "a.py", "line": 1}, {"path": "b.py", "line": 2}]), 120, 0],
    ["fe", ok("grep", items=[{"path": "a.py", "line": 1}, {"path": "b.py", "line": 2}, {"path": "c.py", "line": 3}]), 16, 0],
    ["fe", ok("grep", items=[{"path": "a.py", "line": 1}]), 10, 0],
    ["fe", {"op": "grep", "status": "failed", "items": [{"path": "a.py", "line": 1}]}, 120, 0],
    ["fe", ok("grep", items=[]), 120, 0],
    ["fe2", ok("list", items=[f"d{i}" for i in range(10)]), 120, 0],
    ["fe2", ok("list", items=[f"d{i}" for i in range(10)]), 120, 105],
]
expands = []
for sid, source, max_steps, extra in EXPANDS:
    it = interpreter(expand_program, {"g": source})
    it.max_steps = max_steps
    for number in range(extra):
        it.steps_by_id[f"extra{number}"] = {"id": f"extra{number}", "op": "read"}
    instances = it._expand(it.steps_by_id[sid])
    expands.append({"step": sid, "source": source, "max_steps": max_steps, "known": len(it.steps_by_id),
                    "instances": [norm(item) for item in instances], "truncated": it.truncated, "expanded": it.expanded})

# --- Candidates (hybrid mode) ---------------------------------------------------------------------------------


class Repo:
    def __init__(self, files):
        self._files = files

    def files(self):
        return list(self._files)

    def lines(self, path):
        text = self._files.get(path)
        return None if text is None else text.splitlines()


SOURCES = {
    "src/app.py": 'KINDS = ["a", "b", "c"]\n\n\ndef show(kind):\n    # Upper-cases the kind.\n    return kind.upper()\n',
    "src/cli.py": "from app import show\n\n\ndef main(kind):\n    print(show(kind))\n",
    "tests/test_app.py": 'from app import KINDS, show\n\n\ndef test_show():\n    assert show("a")\n',
    "README.md": "# app\n",
    "big.py": "".join(f"line {i}\n" for i in range(1, 61)),
    "empty.py": "",
}
repo = Repo(SOURCES)
FIXTURE_DIFF = ("diff --git a/src/app.py b/src/app.py\n--- a/src/app.py\n+++ b/src/app.py\n@@ -1,5 +1,6 @@\n"
                '-KINDS = ["a", "b"]\n+KINDS = ["a", "b", "c"]\n \n \n def show(kind):\n+    # Upper-cases the kind.\n     return kind.upper()\n'
                "diff --git a/big.py b/big.py\n--- a/big.py\n+++ b/big.py\n@@ -30,2 +30,3 @@\n line 30\n+line 31\n line 32\n")
files = ra.parse_diff(FIXTURE_DIFF)
cand_brief = brief(extracted={"env": ["APP_MODE"], "flags": ["--fast"], "config": ["cfg.key"]}, tests=["tests/test_app.py", "tests/other.py", "t3"])
retrieval = {"keys": ["KINDS", "EMPTY", "OTHER"], "registries": {"KINDS": ["src/cli.py", "a", "b", "c"], "EMPTY": [], "OTHER": "x"},
             "sections": {"show": "refs of show", "KINDS": "refs of KINDS", "upper": "refs of upper", "kind": "refs of kind"}}
CANDS = {
    "tests_mut": {"file": "src/app.py", "line": 1, "category": "tests", "claim": "Nothing pins KINDS.", "why": "w", "level": "medium",
                  "unpinned": {"behaviour": "KINDS lists c (src/app.py:1)", "change": "drop c from KINDS", "closest_test": {"path": "tests/test_app.py", "line": 5},
                               "mutation": {"path": "src/app.py", "line": 1, "replacement": 'KINDS = ["a", "b"]'}}},
    "tests_ask": {"file": "src/app.py", "line": 6, "category": "tests", "claim": "Nothing pins the upper-casing.", "why": "w", "level": "medium",
                  "finder_level": "high",
                  "unpinned": {"behaviour": "show() upper-cases (src/app.py:6)", "change": "return the kind unchanged", "closest_test": {"path": "tests/test_app.py", "line": 5}}},
    "tests_untracked": {"file": "src/app.py", "line": 6, "category": "tests", "claim": "c", "why": "", "level": "low",
                        "unpinned": {"behaviour": "", "change": "", "closest_test": {"path": "tests/nope.py", "line": 99},
                                     "mutation": {"path": "src/app.py", "line": 6, "replacement": "x"}}},
    "tests_bare": {"file": "src/app.py", "line": 2, "category": "tests", "claim": "c", "why": "w", "unpinned": None},
    "tests_nosource": {"file": "nowhere.py", "line": 2, "category": "tests", "claim": "c", "why": "w",
                       "unpinned": {"closest_test": {"path": "nowhere_test.py", "line": 1}}},
    "registry": {"file": "src/app.py", "line": 1, "category": "correctness", "claim": "KINDS gained c but the cli registry was not updated.", "why": "w", "level": "medium"},
    "registry_empty": {"file": "src/app.py", "line": 1, "category": "correctness", "claim": "EMPTY and OTHER", "why": "w"},
    "env": {"file": "src/app.py", "line": 1, "category": "correctness", "claim": "APP_MODE is read here but set nowhere.", "why": "w", "level": "medium"},
    "flag": {"file": "src/app.py", "line": 1, "category": "correctness", "claim": "x", "why": "the cfg.key value"},
    "comment": {"file": "src/app.py", "line": 5, "category": "docs", "claim": "The comment says it upper-cases.", "why": "w", "level": "low"},
    "error": {"file": "src/cli.py", "line": 5, "category": "correctness", "claim": "The except can never fire.", "why": "w", "level": "high"},
    "input": {"file": "src/app.py", "line": 6, "category": "security", "claim": "The regex guard misses unicode.", "why": "w", "level": "medium"},
    "other": {"file": "src/cli.py", "line": 5, "category": "correctness", "claim": "main() prints the raw value.", "why": "w", "level": "low",
              "citations": [{"path": "src/app.py", "line": 4, "quote": "def show(kind):"}]},
    "other_cites": {"file": "big.py", "line": 31, "category": None, "claim": "show() and upper break kind", "why": "", "scenario": "s1",
                    "suggested_fix": "fix", "consequence": "k", "level": "medium",
                    "citations": [{"path": "big.py", "line": 40}, {"path": "src/app.py", "line": 2}, {"path": "big.py", "line": 50},
                                  {"path": "src/cli.py", "line": "4"}]},
    "nosource": {"file": "nowhere.py", "line": 3, "category": "correctness", "claim": "x", "why": "y", "citations": [{"path": "empty.py", "line": 1}]},
    "long": {"file": "big.py", "line": 3, "category": "correctness", "claim": "q" * 1500, "why": "w", "scenario": "sc"},
    "regression": {"file": "tests/test_app.py", "line": 4, "category": "correctness", "claim": "x", "host_confirmed": True, "test_run": 1},
}
candidates = []
for name, cand in CANDS.items():
    shape, details = c.candidate_shape(cand, cand_brief, retrieval)
    entry = {"name": name, "candidate": cand, "shape": shape, "details": details,
             "fields": c._finding_fields(cand), "material": c.ask_material(cand, files, retrieval),
             "material_bare": c.ask_material(cand, None, None)}
    if shape != "regression":
        variants = []
        for tests_available, runnable in ((True, None), (False, None), (True, False)):
            steps, how = c.template_steps("c1", cand, shape, details, repo=repo, brief=cand_brief, tests_available=tests_available,
                                          files=files, retrieval=retrieval,
                                          runnable=None if runnable is None else (lambda paths: False))
            program, errors = c.validate({"steps": steps}) if steps else (None, [])
            variants.append({"tests_available": tests_available, "runnable": runnable, "steps": steps, "how": how,
                             "errors": errors})
        entry["templates"] = variants
    candidates.append(entry)
# A shape forced on a candidate whose category would pick another.
forced = []
for shape, details in (("registry-member", {"key": "a.b", "registries": ["r1", "r2"]}), ("env-in-deploy", {"name": "X+Y"}),
                       ("error-path", {}), ("unpinned-behaviour", {})):
    steps, how = c.template_steps("k9", CANDS["error"], shape, details, repo=repo, brief=cand_brief, tests_available=True,
                                  files=None, retrieval=None)
    forced.append({"shape": shape, "details": details, "steps": steps, "how": how})

QUOTE_IN = [
    ["src/app.py", 4, "def show(kind):", 12], ["src/app.py", 4, "def", 12], ["src/app.py", 4, "  return   kind.upper() ", 12],
    ["src/app.py", 1, "return kind.upper()", 2], ["src/app.py", 1, "return kind.upper()", 6],
    ["src/app.py", 4, "prefix # Upper-cases the kind. suffix", 1], ["nowhere.py", 1, "abcd", 12], ["big.py", 50, "line 3", 12],
    ["big.py", 1, "line 60", 100], ["big.py", 70, "line 59", 12],
]
quote_in = [[path, line, quote, radius, c._quote_in(repo, path, line, quote, radius)] for path, line, quote, radius in QUOTE_IN]
READ_STEPS = [
    [{"id": "a", "op": "read", "args": {"path": "big.py", "start": 10, "end": 20}}],
    [{"id": "a", "op": "grep", "args": {"path": "big.py"}}, {"id": "b", "op": "read", "args": "x"},
     {"id": "c", "op": "read", "args": {"path": "big.py", "start": 40}}],
    [{"id": "a", "op": "read", "args": {"path": "big.py", "start": 0, "end": 0}}],
    [{"id": "a", "op": "read", "args": {"path": "big.py", "start": "5", "end": "5"}}],
]
quote_reads = [[steps, quote, c._quote_in_reads(repo, steps, quote)] for steps in READ_STEPS
               for quote in ("line 15", "line 21", "line 9", "line 1", "line 5", "line 60", "line 6")]


def session(records, runner=None):
    return SimpleNamespace(records=records, runner=runner)


AVAIL = [
    None,
    session([]),
    session([], SimpleNamespace(name="pytest", because="pytest.ini")),
    session([{"runner": "pytest", "cwd": None, "status": "passed"}, {"runner": "pytest", "cwd": ".", "status": "failed"},
             {"runner": "pytest", "cwd": "", "status": "passed"}, {"runner": "vitest", "cwd": "web", "status": "unavailable"},
             {"runner": "vitest", "cwd": "web", "status": "passed"}, {"runner": "go", "cwd": "svc", "status": "passed"},
             {"runner": "jest", "cwd": "ui", "status": "unavailable"}], SimpleNamespace(name="pytest", because="x")),
]
availability = []
for s in AVAIL:
    lines, unavailable = c.runner_availability(s)
    availability.append({"session": None if s is None else {"records": s.records, "runner": None if s.runner is None else vars(s.runner)},
                         "lines": lines, "unavailable": sorted(list(item) for item in unavailable)})

PLANNER = [
    ["diff", "brief", {"context": "", "intent": "", "guidance": "", "tests_block": "", "runners": [], "tests_allowed": False, "runs_left": 0}],
    ["d" * 40_001, "b", {"context": "ctx", "intent": "intent", "guidance": "", "tests_block": "tests", "runners": [], "tests_allowed": True,
                         "runs_left": 3, "coverage": COVERAGE, "limits": [120, 200], "retrieved": "refs"}],
    ["d" * 40_000, "b", {"context": "", "intent": "i", "guidance": "g", "tests_block": "", "runners": ["pytest in .: available", "x"],
                         "tests_allowed": True, "runs_left": 0, "coverage": []}],
]
planner = []
for diff_text, brief_text, options in PLANNER:
    kwargs = dict(options)
    if "limits" in kwargs:
        kwargs["limits"] = tuple(kwargs["limits"])
    views, cut = c.planner_views(diff_text, brief_text, **kwargs)
    planner.append({"diff": diff_text, "brief": brief_text, "options": options, "views": views, "cut": cut})

PUBLIC = [
    {"id": "g", "op": "grep", "status": "ok", "count": 4, "capped": False, "truncated": False, "title": "grep x",
     "items": list(range(25)), "text": "t" * 4100, "summary": "s", "answer": None},
    {"id": "m", "op": "mutation_check", "status": "ok", "outcome": "passed", "caught": False, "title": "mutation_check", "text": ""},
    {"id": "t", "op": "run_tests", "status": "failed", "detail": "no session"},
    {"id": "f", "op": "finding", "status": "ok", "emitted": True, "text": "finding emitted: {}"},
    {"id": "f2", "op": "finding", "status": "ok", "emitted": False, "gate": "gate false", "detail": "d"},
    {"id": "f3", "op": "finding", "status": "ok", "emitted": False},
    {"id": "a", "op": "assert", "status": "ok", "value": None, "contradicted": False, "detail": "x did not run", "items": []},
    {"op": "ask", "answer": "yes", "quote": "q", "why": "w", "value": False},
]
public = [[result, c._public_value(result)] for result in PUBLIC]

print(json.dumps({
    "predicates": predicates, "validations": validations, "coverage": coverage_cases, "uncovered": uncovered,
    "counts": counts, "items": items, "renders": renders, "count_predicates": count_predicates, "limits": limits,
    "eval_program": EVAL_PROGRAM, "eval_results": EVAL_RESULTS, "evaluation": evaluate(False), "evaluation_deferred": evaluate(True),
    "fill_results": fill_results, "fills": fills, "summaries": summaries, "quotes": quotes,
    "expand_program": EXPAND_PROGRAM, "expands": expands,
    "sources": SOURCES, "fixture_diff": FIXTURE_DIFF, "candidate_brief": brief_json(cand_brief), "retrieval": retrieval,
    "candidates": candidates, "forced": forced, "quote_in": quote_in, "quote_reads": quote_reads,
    "availability": availability, "planner": planner, "public": public,
}, indent=1, ensure_ascii=False) + "\n", end="")
